//! Render functions for the TUI.
//!
//! This module handles all rendering logic, dispatching to the appropriate
//! view based on application state.

use crate::app::{
    App, ConfirmAction, ContextMenuSubState, SubscribeState, View, CONTEXT_MENU_ITEMS,
};
use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    widgets::{Block, Borders, Clear, Paragraph},
    Frame,
};

use super::{articles, categories, feeds, help, reader, stats, status, whatsnew};

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
        View::Stats => stats::render(f, app),
    }

    // Render help overlay on top of any view when active
    if app.show_help {
        help::render(f, app);
    }

    // Render confirmation dialog on top of any view when active
    if let Some(ref confirm) = app.pending_confirm {
        render_confirm_overlay(f, app, confirm);
    }

    // Render subscribe dialog on top of any view when active
    if let Some(ref state) = app.subscribe_state {
        render_subscribe_overlay(f, app, state);
    }

    // Render context menu on top of any view when active
    if app.context_menu.is_some() {
        render_context_menu_overlay(f, app);
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
        render_main_panels(f, app, chunks[1]);
        status::render(f, app, chunks[2]);
    } else {
        // Normal two-row layout
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(0), Constraint::Length(1)])
            .split(f.area());

        render_main_panels(f, app, chunks[0]);
        status::render(f, app, chunks[1]);
    }
}

/// Render the main panels (categories + feeds + articles).
///
/// When `show_categories` is true, the left side splits into categories (20%) and feeds (80%).
/// Otherwise, feeds take the full left panel width.
fn render_main_panels(f: &mut Frame, app: &App, area: Rect) {
    if app.show_categories {
        // Three-column layout: categories | feeds | articles
        let main_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(15),
                Constraint::Percentage(20),
                Constraint::Percentage(65),
            ])
            .split(area);

        categories::render(f, app, main_chunks[0]);
        feeds::render(f, app, main_chunks[1]);
        articles::render(f, app, main_chunks[2]);
    } else {
        // Two-column layout: feeds | articles
        let main_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(30), Constraint::Percentage(70)])
            .split(area);

        feeds::render(f, app, main_chunks[0]);
        articles::render(f, app, main_chunks[1]);
    }
}

/// Render a confirmation dialog overlay centered on screen.
fn render_confirm_overlay(f: &mut Frame, app: &App, confirm: &ConfirmAction) {
    let area = f.area();

    let text = match confirm {
        ConfirmAction::DeleteFeed { title, .. } => {
            format!(
                "Delete \"{}\"?\n\nAll articles will be removed.\n\n(y) Confirm  (n/Esc) Cancel",
                title
            )
        }
    };

    // Size: at most 50 chars wide, 7 lines tall, centered
    let width = 50u16.min(area.width.saturating_sub(4));
    let height = 7u16.min(area.height.saturating_sub(4));
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let overlay = Rect::new(x, y, width, height);

    if overlay.width < 10 || overlay.height < 5 {
        return;
    }

    f.render_widget(Clear, overlay);

    let paragraph = Paragraph::new(text)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(app.style("panel_border_focused"))
                .title(" Confirm "),
        )
        .alignment(Alignment::Center)
        .style(app.style("reader_body"));

    f.render_widget(paragraph, overlay);
}

/// Render the subscribe dialog overlay centered on screen.
fn render_subscribe_overlay(f: &mut Frame, app: &App, state: &SubscribeState) {
    let area = f.area();

    let (title, text) = match state {
        SubscribeState::InputUrl { input } => (
            " Subscribe to Feed ",
            format!(
                "Enter feed URL:\n\n> {}_\n\n(Enter) Discover  (Esc) Cancel",
                input
            ),
        ),
        SubscribeState::Discovering { url } => (
            " Discovering Feed ",
            format!("Fetching {}...\n\nPlease wait.\n\n(Esc) Cancel", url),
        ),
        SubscribeState::Preview { feed } => {
            let desc = feed.description.as_deref().unwrap_or("No description");
            (
                " Feed Preview ",
                format!(
                    "Title: {}\nURL:   {}\n{}\n\n(y/Enter) Subscribe  (n/Esc) Cancel",
                    feed.title, feed.feed_url, desc,
                ),
            )
        }
    };

    let width = 60u16.min(area.width.saturating_sub(4));
    let height = 10u16.min(area.height.saturating_sub(4));
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let overlay = Rect::new(x, y, width, height);

    if overlay.width < 20 || overlay.height < 6 {
        return;
    }

    f.render_widget(Clear, overlay);

    let paragraph = Paragraph::new(text)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(app.style("panel_border_focused"))
                .title(title),
        )
        .style(app.style("reader_body"));

    f.render_widget(paragraph, overlay);
}

/// Render the context menu overlay centered on screen.
///
/// Shows the main menu items, rename input, or category picker depending
/// on the current sub-state.
fn render_context_menu_overlay(f: &mut Frame, app: &App) {
    let area = f.area();

    let menu = match &app.context_menu {
        Some(m) => m,
        None => return,
    };

    let (title, text) = match &menu.sub_state {
        ContextMenuSubState::MainMenu => {
            let items: String = CONTEXT_MENU_ITEMS
                .iter()
                .enumerate()
                .map(|(i, item)| {
                    if i == menu.selected_item {
                        format!("> {}", item)
                    } else {
                        format!("  {}", item)
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
            (
                format!(" {} ", menu.feed_title),
                format!("{}\n\n(Enter) Select  (Esc) Cancel", items),
            )
        }
        ContextMenuSubState::Renaming { input } => (
            " Rename Feed ".to_string(),
            format!("New name:\n\n> {}_\n\n(Enter) Save  (Esc) Back", input),
        ),
        ContextMenuSubState::CategoryPicker { selected } => {
            let mut items = vec![if *selected == 0 {
                "> Uncategorized".to_string()
            } else {
                "  Uncategorized".to_string()
            }];
            for (i, cat) in app.categories.iter().enumerate() {
                let idx = i + 1;
                if *selected == idx {
                    items.push(format!("> {}", cat.name));
                } else {
                    items.push(format!("  {}", cat.name));
                }
            }
            (
                " Move to Category ".to_string(),
                format!("{}\n\n(Enter) Move  (Esc) Back", items.join("\n")),
            )
        }
    };

    let content_lines = text.lines().count() as u16 + 2; // +2 for borders
    let width = 45u16.min(area.width.saturating_sub(4));
    let height = content_lines.min(area.height.saturating_sub(4));
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let overlay = Rect::new(x, y, width, height);

    if overlay.width < 20 || overlay.height < 5 {
        return;
    }

    f.render_widget(Clear, overlay);

    let paragraph = Paragraph::new(text)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(app.style("panel_border_focused"))
                .title(title),
        )
        .style(app.style("reader_body"));

    f.render_widget(paragraph, overlay);
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
