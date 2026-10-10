use crate::{
    config,
    state::{
        Album, Artist, ArtistFocusState, BrowsePageUIState, Context, ContextPageUIState,
        DataReadGuard, Id, LibraryFocusState, MouseArea, MouseTarget, MutableWindowState,
        PageState, PageType, PlaybackMetadata, PlaylistCreateCurrentField, PlaylistFolderItem,
        PlaylistPopupAction, PopupState, SearchFocusState, SharedState, Track, UIStateGuard,
    },
};
use anyhow::{Context as AnyhowContext, Result};
use ratatui::{
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{
        Block, BorderType, Borders, Cell, Gauge, LineGauge, List, ListItem, ListState, Paragraph,
        Row, Table, TableState, Wrap,
    },
    Frame,
};

#[cfg(feature = "image")]
use crate::state::ImageRenderInfo;

type Terminal = ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>;

#[cfg(feature = "image")]
pub mod cover_image;
mod page;
mod playback;
mod popup;
pub mod single_line_input;
#[cfg(feature = "streaming")]
pub mod streaming;
pub mod utils;

/// Run the application UI
pub fn run(state: &SharedState, mut terminal: Terminal) -> Result<()> {
    let ui_refresh_duration = std::time::Duration::from_millis(
        config::get_config().app_config.app_refresh_duration_in_ms,
    );
    let mut last_terminal_size = None;

    loop {
        {
            let mut ui = state.ui.lock();
            if !ui.is_running {
                clean_up(terminal).context("clean up UI resources")?;
                std::process::exit(0);
            }

            let terminal_size = terminal.size()?;
            if Some(terminal_size) != last_terminal_size {
                last_terminal_size = Some(terminal_size);
                #[cfg(feature = "image")]
                {
                    // redraw the cover image when the terminal's size changes
                    ui.last_cover_image_render_info = ImageRenderInfo::default();
                }
            }

            if let Err(err) = terminal.draw(|frame| {
                // set the background and foreground colors for the application
                let rect = frame.area();
                let block = Block::default().style(ui.theme.app());
                frame.render_widget(block, rect);

                render_application(frame, state, &mut ui, rect);
            }) {
                tracing::error!("Failed to render the application: {err:#}");
            }
        }

        std::thread::sleep(ui_refresh_duration);
    }
}

pub fn init_terminal() -> Result<Terminal> {
    let mut stdout = std::io::stdout();
    crossterm::terminal::enable_raw_mode()?;
    crossterm::execute!(
        stdout,
        crossterm::terminal::EnterAlternateScreen,
        crossterm::event::EnableMouseCapture
    )?;
    let backend = ratatui::backend::CrosstermBackend::new(stdout);
    let mut terminal = ratatui::Terminal::new(backend)?;
    terminal.clear()?;
    Ok(terminal)
}

#[cfg(feature = "image")]
pub fn init_image_picker(state: &SharedState) -> Result<()> {
    let mut ui = state.ui.lock();
    crossterm::terminal::enable_raw_mode()?;
    ui.picker = match ratatui_image::picker::Picker::from_query_stdio() {
        Ok(p) => p,
        Err(err) => {
            tracing::warn!("Failed to initialize query_stdio picker, error: {err:#}");
            ratatui_image::picker::Picker::halfblocks()
        }
    };
    crossterm::terminal::disable_raw_mode()?;

    // ratatui_image might detect the wrong protocol for iTerm2, so override it to the native iTerm2 protocol if detected
    // https://github.com/ratatui/ratatui-image/issues/158
    if is_iterm2() && ui.picker.protocol_type() != ratatui_image::picker::ProtocolType::Iterm2 {
        ui.picker
            .set_protocol_type(ratatui_image::picker::ProtocolType::Iterm2);
        tracing::info!("Detected iTerm2; overriding image protocol to native iTerm2");
    }
    tracing::info!("Image protocol: {:?}", ui.picker.protocol_type());
    Ok(())
}

/// Whether the application is running inside iTerm2.
///
/// iTerm2 sets `TERM_PROGRAM=iTerm.app` locally and forwards `LC_TERMINAL=iTerm2`
/// over SSH, so checking both covers the common cases.
#[cfg(feature = "image")]
fn is_iterm2() -> bool {
    std::env::var("TERM_PROGRAM").is_ok_and(|v| v == "iTerm.app")
        || std::env::var("LC_TERMINAL").is_ok_and(|v| v.eq_ignore_ascii_case("iTerm2"))
}

/// Clean up UI resources before quitting the application
fn clean_up(mut terminal: Terminal) -> Result<()> {
    crossterm::terminal::disable_raw_mode()?;
    crossterm::execute!(
        terminal.backend_mut(),
        crossterm::terminal::LeaveAlternateScreen,
        crossterm::event::DisableMouseCapture,
    )?;
    terminal.show_cursor()?;
    Ok(())
}

/// Render the application
fn render_application(frame: &mut Frame, state: &SharedState, ui: &mut UIStateGuard, rect: Rect) {
    // rendering order: playback window -> shortcut help popup -> other popups -> main layout

    ui.mouse_areas.clear();

    // render playback window before other popups and windows to ensure nothing is rendered on top
    // of the playback window, which is to avoid "duplicated images" issue
    // See: https://github.com/aome510/spotify-player/issues/498
    let full_rect = rect;
    let rect = playback::render_playback_window(frame, state, ui, rect);
    // registered first so that the interactive regions inside the window take precedence
    if let Some(playback_rect) = carved_rect(full_rect, rect) {
        ui.mouse_areas.insert(
            0,
            MouseArea {
                rect: playback_rect,
                target: MouseTarget::PlaybackWindow,
            },
        );
    }

    let rect = popup::render_shortcut_help_popup(frame, ui, rect);

    let n_areas = ui.mouse_areas.len();
    let full_rect = rect;
    let (rect, is_active) = popup::render_popup(frame, state, ui, rect);
    if ui.has_focused_popup() {
        if let Some(popup_rect) = carved_rect(full_rect, rect) {
            ui.mouse_areas.insert(
                n_areas,
                MouseArea {
                    rect: popup_rect,
                    target: MouseTarget::PopupWindow,
                },
            );
        }
    }

    render_main_layout(is_active, frame, state, ui, rect);

    render_mouse_hover(frame, ui);
}

/// The part of `outer` taken by a window rendered at its top or bottom, leaving `remaining`
fn carved_rect(outer: Rect, remaining: Rect) -> Option<Rect> {
    let rect = if remaining.y > outer.y {
        Rect::new(outer.x, outer.y, outer.width, remaining.y - outer.y)
    } else {
        Rect::new(
            outer.x,
            remaining.bottom(),
            outer.width,
            outer.bottom().saturating_sub(remaining.bottom()),
        )
    };
    (rect.height > 0).then_some(rect)
}

/// Highlight the item or button under the mouse pointer
fn render_mouse_hover(frame: &mut Frame, ui: &UIStateGuard) {
    let Some((column, row)) = ui.mouse_position else {
        return;
    };
    let Some(area) = ui.mouse_area_at(column, row) else {
        return;
    };
    let rect = match area.target {
        MouseTarget::ResumePause | MouseTarget::ConfirmAction(_) => area.rect,
        _ if area.item_at(row).is_some() => Rect::new(area.rect.x, row, area.rect.width, 1),
        _ => return,
    };

    let style = ui.theme.hover();
    let app_bg = ui.theme.app().bg.unwrap_or(Color::Reset);
    let buffer = frame.buffer_mut();
    for position in rect.positions() {
        let Some(cell) = buffer.cell_mut(position) else {
            continue;
        };
        // keep the selection's highlight
        let has_own_bg = cell.bg != app_bg && cell.bg != Color::Reset;
        if cell.modifier.contains(Modifier::REVERSED) || has_own_bg {
            continue;
        }
        cell.set_style(style);
    }
}

/// Render the application's main layout
fn render_main_layout(
    is_active: bool,
    frame: &mut Frame,
    state: &SharedState,
    ui: &mut UIStateGuard,
    rect: Rect,
) {
    let page_type = ui.current_page().page_type();
    match page_type {
        PageType::Library => page::render_library_page(is_active, frame, state, ui, rect),
        PageType::Search => page::render_search_page(is_active, frame, state, ui, rect),
        PageType::Context => page::render_context_page(is_active, frame, state, ui, rect),
        PageType::Browse => page::render_browse_page(is_active, frame, state, ui, rect),
        PageType::Lyrics => page::render_lyrics_page(is_active, frame, state, ui, rect),
        PageType::Queue => page::render_queue_page(frame, state, ui, rect),
        PageType::CommandHelp => page::render_commands_help_page(frame, ui, rect),
        PageType::Logs => page::render_logs_page(frame, state, ui, rect),
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Orientation {
    Vertical,
    #[default]
    Horizontal,
}

impl Orientation {
    /// Construct screen orientation based on the terminal's size
    pub fn from_size(columns: u16, rows: u16) -> Self {
        let ratio = f64::from(columns) / f64::from(rows);

        // a larger ratio has to be used since terminal cells aren't square
        if ratio > 2.3 {
            Self::Horizontal
        } else {
            Self::Vertical
        }
    }

    pub fn layout<I>(self, constraints: I) -> Layout
    where
        I: IntoIterator,
        I::Item: Into<Constraint>,
    {
        match self {
            Self::Vertical => Layout::vertical(constraints),
            Self::Horizontal => Layout::horizontal(constraints),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{LibraryFocusState, UIState};
    use ratatui::{backend::TestBackend, Terminal};

    #[test]
    fn carved_rect_finds_windows_at_the_top_or_bottom() {
        let outer = Rect::new(0, 0, 80, 40);
        assert_eq!(
            carved_rect(outer, Rect::new(0, 8, 80, 32)),
            Some(Rect::new(0, 0, 80, 8))
        );
        assert_eq!(
            carved_rect(outer, Rect::new(0, 0, 80, 30)),
            Some(Rect::new(0, 30, 80, 10))
        );
        assert_eq!(carved_rect(outer, outer), None);
    }

    fn render_hover(mouse_position: Option<(u16, u16)>) -> ratatui::buffer::Buffer {
        let ui = parking_lot::Mutex::new(UIState::default());
        let mut ui = ui.lock();
        ui.mouse_areas.push(MouseArea {
            rect: Rect::new(1, 1, 10, 3),
            target: MouseTarget::LibraryWindow {
                focus: LibraryFocusState::Playlists,
                first_item: 0,
                item_count: 2,
            },
        });
        ui.mouse_position = mouse_position;

        let mut terminal = Terminal::new(TestBackend::new(12, 5)).unwrap();
        terminal
            .draw(|frame| {
                // the selected row
                frame.buffer_mut().set_style(
                    Rect::new(1, 2, 10, 1),
                    Style::new().add_modifier(Modifier::REVERSED),
                );
                render_mouse_hover(frame, &ui);
            })
            .unwrap();
        terminal.backend().buffer().clone()
    }

    #[test]
    fn hover_highlights_the_item_row_under_the_pointer() {
        let hover = UIState::default().theme.hover();
        let buffer = render_hover(Some((4, 1)));

        assert!((1..11).all(|x| buffer[(x, 1)].bg == hover.bg.unwrap()));
        assert_eq!(buffer[(0, 1)].bg, Color::Reset);
        assert_eq!(buffer[(11, 1)].bg, Color::Reset);
    }

    #[test]
    fn hover_keeps_the_selection_highlight() {
        let buffer = render_hover(Some((4, 2)));

        assert!((1..11).all(|x| buffer[(x, 2)].bg == Color::Reset));
    }

    #[test]
    fn hover_ignores_rows_without_items_and_a_missing_pointer() {
        for position in [Some((4, 3)), None] {
            let buffer = render_hover(position);
            assert!(buffer.content.iter().all(|cell| cell.bg == Color::Reset));
        }
    }
}
