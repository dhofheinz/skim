use super::articles::format_relative_time;
use crate::app::{App, Focus};
use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem},
    Frame,
};

/// Render the feed list panel
///
/// # Performance Note (PERF-006)
///
/// This function uses `format!()` for constructing display strings on each render:
/// - Unread count: `format!(" ({})", feed.unread_count)`
/// - Time suffix: `format!(" · {}", time_str)`
/// - Block title: `format!("Feeds ({})", app.feeds.len())`
///
/// This is acceptable overhead because:
/// 1. TUI renders are capped at ~60fps, and these are small string allocations
/// 2. ratatui already buffers all output, so the allocations are short-lived
/// 3. The optimization (caching in App state or using write! to a buffer) adds
///    complexity for marginal benefit in a feed list of typically <100 items
/// 4. Profile before optimizing - these allocations are unlikely to be the bottleneck
///
/// If profiling reveals this is a hotspot, consider caching formatted strings
/// in the Feed struct when unread_count or last_fetched changes.
pub fn render(f: &mut Frame, app: &App, area: Rect) {
    // EDGE-001: Guard against zero-width/height areas
    // Layout may produce zero-sized rects during extreme terminal resizes
    if area.width < 3 || area.height < 3 {
        return;
    }

    let is_focused = app.focus == Focus::Feeds;

    let items: Vec<ListItem> = if app.feeds.is_empty() {
        vec![ListItem::new("No feeds loaded")]
    } else {
        let mut items = Vec::with_capacity(app.feeds.len());
        for (i, feed) in app.feeds.iter().enumerate() {
            // Build feed line using Span composition to avoid format! allocations
            let time_str = format_relative_time(feed.last_fetched);

            let style = if i == app.selected_feed {
                Style::default().bg(Color::DarkGray).fg(Color::White)
            } else if feed.unread_count > 0 {
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };

            // Pre-allocate spans: error indicator (optional) + title + count (optional) + time (optional)
            let mut spans = Vec::with_capacity(4);

            // Error indicator if present
            if feed.error.is_some() {
                spans.push(Span::styled("⚠ ", Style::default().fg(Color::Red)));
            }

            // Title span (borrow from feed.title, no allocation)
            // PERF-009: Deref Arc<str> to &str
            spans.push(Span::styled(&*feed.title, style));

            // Unread count span (small allocation only when count > 0)
            if feed.unread_count > 0 {
                spans.push(Span::styled(format!(" ({})", feed.unread_count), style));
            }

            // Time suffix span (small allocation only when time_str is non-empty)
            if !time_str.is_empty() {
                spans.push(Span::styled(format!(" · {}", time_str), style));
            }

            items.push(ListItem::new(Line::from(spans)));
        }
        items
    };

    let border_style = if is_focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default()
    };

    let title = format!("Feeds ({})", app.feeds.len());
    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(border_style)
            .title(title),
    );

    f.render_widget(list, area);
}
