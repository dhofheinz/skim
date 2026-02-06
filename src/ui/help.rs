//! Help overlay — scrollable keybinding table.
//!
//! Renders a centered overlay showing all keybindings grouped by context.
//! Displays actual bindings including any user overrides from config.

use crate::app::App;
use crate::keybindings::Context;
use ratatui::{
    layout::{Constraint, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Row, Table},
    Frame,
};

/// Context display order and labels for the help screen.
const CONTEXT_ORDER: [(Context, &str); 5] = [
    (Context::Global, "General"),
    (Context::FeedList, "Feed List"),
    (Context::ArticleList, "Article List"),
    (Context::Reader, "Reader"),
    (Context::Search, "Search"),
];

/// Render the help overlay on top of the current view.
///
/// Draws a centered, bordered table of all keybindings grouped by context.
/// Supports vertical scrolling for long binding lists.
pub fn render(f: &mut Frame, app: &App) {
    let area = f.area();

    // Leave a margin around the overlay
    let overlay = centered_rect(80, 80, area);
    if overlay.width < 20 || overlay.height < 6 {
        return;
    }

    // Clear the background behind the overlay
    f.render_widget(Clear, overlay);

    let bindings = app.keybindings.all_bindings();

    // Build rows grouped by context
    let mut rows: Vec<Row> = Vec::new();

    for (ctx, label) in &CONTEXT_ORDER {
        let ctx_bindings: Vec<_> = bindings.iter().filter(|(c, _, _, _)| c == ctx).collect();

        if ctx_bindings.is_empty() {
            continue;
        }

        // Section header row
        rows.push(
            Row::new(vec![
                Line::from(Span::styled(
                    format!("-- {} --", label),
                    Style::default().add_modifier(Modifier::BOLD),
                )),
                Line::from(""),
            ])
            .style(app.style("reader_heading")),
        );

        for (_, key_str, _action, description) in ctx_bindings {
            rows.push(Row::new(vec![
                format!("  {}", key_str),
                description.to_string(),
            ]));
        }

        // Blank separator between groups
        rows.push(Row::new(vec!["".to_string(), String::new()]));
    }

    // Remove trailing blank row if present
    if !rows.is_empty() {
        rows.pop();
    }

    let total_rows = rows.len();

    // Apply scroll offset
    let visible_height = overlay.height.saturating_sub(3) as usize; // -2 border -1 header
    let max_scroll = total_rows.saturating_sub(visible_height);
    let scroll = app.help_scroll_offset.min(max_scroll);
    let visible_rows: Vec<Row> = rows.into_iter().skip(scroll).take(visible_height).collect();

    // Scroll indicator in title
    let title = if max_scroll > 0 {
        format!(
            " Help ({}/{}) ",
            scroll.saturating_add(1),
            max_scroll.saturating_add(1)
        )
    } else {
        " Help (? to close) ".to_string()
    };

    let widths = [Constraint::Length(16), Constraint::Min(20)];

    let table = Table::new(visible_rows, widths)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(app.style("panel_border_focused"))
                .title(title),
        )
        .header(
            Row::new(vec!["Key", "Action"])
                .style(
                    Style::default()
                        .add_modifier(Modifier::BOLD)
                        .add_modifier(Modifier::UNDERLINED),
                )
                .bottom_margin(1),
        )
        .style(app.style("reader_body"));

    f.render_widget(table, overlay);

    // Scroll hint at bottom if content overflows
    if max_scroll > 0 && scroll < max_scroll {
        let hint = Line::from(vec![Span::styled(
            " j/k to scroll, ? or Esc to close ",
            app.style("reader_metadata"),
        )]);
        let hint_area = Rect {
            x: overlay.x + 1,
            y: overlay.y + overlay.height.saturating_sub(1),
            width: overlay.width.saturating_sub(2),
            height: 1,
        };
        f.render_widget(Paragraph::new(hint), hint_area);
    }
}

/// Create a centered rectangle with the given percentage of the parent area.
fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let width = area.width * percent_x / 100;
    let height = area.height * percent_y / 100;
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    Rect::new(x, y, width, height)
}
