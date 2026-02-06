//! Render functions for the TUI.
//!
//! This module handles all rendering logic, dispatching to the appropriate
//! view based on application state.

use crate::app::{App, View};
use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout},
    widgets::Paragraph,
    Frame,
};

use super::{articles, feeds, help, reader, status, whatsnew};

/// Minimum terminal dimensions required for normal operation.
pub(super) const MIN_WIDTH: u16 = 60;
pub(super) const MIN_HEIGHT: u16 = 10;

/// Main render dispatch function.
///
/// Routes to the appropriate view renderer based on current application state.
/// Handles terminal size validation before rendering.
pub(super) fn render(f: &mut Frame, app: &mut App) {
    let area = f.area();

    // EDGE-001: Guard against zero-width/height to prevent panics
    // At truly minimal dimensions, we can't render anything meaningful
    if area.width < 1 || area.height < 1 {
        return;
    }

    // EDGE-001: Minimum terminal size check for usable UI
    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        // For very small terminals (less than 3 lines), just show minimal message
        let msg = if area.height < 3 || area.width < 20 {
            Paragraph::new("Too small")
        } else {
            Paragraph::new(format!(
                "Terminal too small\n\nMinimum: {}x{}\nCurrent: {}x{}",
                MIN_WIDTH, MIN_HEIGHT, area.width, area.height
            ))
            .alignment(Alignment::Center)
        };
        f.render_widget(msg, area);
        return;
    }

    match app.view {
        View::Browse => render_browse(f, app),
        View::Reader => render_reader(f, app),
    }

    // Render help overlay on top of any view when active
    if app.show_help {
        help::render(f, app);
    }
}

/// Render the browse view (feeds + articles panels).
fn render_browse(f: &mut Frame, app: &App) {
    // Layout depends on whether What's New panel is visible
    if app.show_whats_new && !app.whats_new.is_empty() {
        // Three rows: What's New (dynamic height), main panels, status bar
        // P-10/I-3: Clamp before cast to prevent u16 overflow; .min(8)+2 caps at 10
        let whats_new_height = app.whats_new.len().min(8) as u16 + 2;

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(whats_new_height),
                Constraint::Min(0),
                Constraint::Length(1),
            ])
            .split(f.area());

        whatsnew::render(f, app, chunks[0]);

        let main_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(30), Constraint::Percentage(70)])
            .split(chunks[1]);

        feeds::render(f, app, main_chunks[0]);
        articles::render(f, app, main_chunks[1]);
        status::render(f, app, chunks[2]);
    } else {
        // Normal two-row layout
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(0), Constraint::Length(1)])
            .split(f.area());

        let main_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(30), Constraint::Percentage(70)])
            .split(chunks[0]);

        feeds::render(f, app, main_chunks[0]);
        articles::render(f, app, main_chunks[1]);
        status::render(f, app, chunks[1]);
    }
}

/// Render the reader view (article content + status bar).
fn render_reader(f: &mut Frame, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(f.area());

    reader::render(f, app, chunks[0]);
    status::render(f, app, chunks[1]);
}
