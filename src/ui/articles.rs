use crate::app::{App, Focus};
use crate::util::{display_width, truncate_to_width};
use chrono::{DateTime, Utc};
use ratatui::{
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState},
    Frame,
};

/// Format timestamp as relative time
pub fn format_relative_time(timestamp: Option<i64>) -> String {
    format_relative_time_with_now(timestamp, Utc::now().timestamp())
}

/// Format timestamp as relative time using a provided "now" timestamp.
/// This function enables deterministic testing by allowing injection of the current time.
pub fn format_relative_time_with_now(timestamp: Option<i64>, now: i64) -> String {
    let Some(ts) = timestamp else {
        return String::new();
    };

    let diff = now - ts;

    // Future dates (malformed feeds)
    if diff < 0 {
        return "now".to_owned();
    }

    // Less than 1 hour
    if diff < 3600 {
        return format!("{}m", diff / 60);
    }

    // Less than 24 hours
    if diff < 86400 {
        return format!("{}h", diff / 3600);
    }

    // Less than 7 days
    if diff < 604800 {
        return format!("{}d", diff / 86400);
    }

    // Older than 7 days - show date
    DateTime::from_timestamp(ts, 0)
        .map(|dt| dt.format("%b %d").to_string())
        .unwrap_or_else(|| "Unknown date".to_string())
}

/// Render the article list panel
pub fn render(f: &mut Frame, app: &App, area: Rect) {
    // EDGE-001: Guard against zero-width/height areas
    // Layout may produce zero-sized rects during extreme terminal resizes
    if area.width < 3 || area.height < 3 {
        return;
    }

    let is_focused = app.focus == Focus::Articles;

    // PERF-021: Hoist style lookups out of per-item loop
    let style_star = app.style("article_star");
    let style_feed_prefix = app.style("article_feed_prefix");
    let style_selected = app.style("article_selected");
    let style_title = app.style("article_title");
    let style_read = app.style("article_read");
    let style_date = app.style("article_date");

    let items: Vec<ListItem> = if app.articles.is_empty() {
        // EDGE-006: Contextual empty message
        if app.search_mode && !app.search_input.is_empty() {
            vec![ListItem::new(format!(
                "No results for '{}'",
                app.search_input
            ))]
        } else if app.search_mode {
            vec![ListItem::new("Type to search...")]
        } else {
            vec![ListItem::new("No articles")]
        }
    } else {
        let mut items = Vec::with_capacity(app.articles.len());
        for (i, article) in app.articles.iter().enumerate() {
            let time_str = format_relative_time(article.published);

            // PERF-014: In starred mode, use cached feed prefix to avoid per-render allocations
            // The prefix cache is populated when entering starred mode
            let feed_prefix = if app.starred_mode {
                app.feed_prefix_cache.get(&article.feed_id)
            } else {
                None
            };

            // Build line with star, feed name (starred mode), title, and time
            // Pre-allocate spans: at most 4 (star, feed, title, time)
            let mut spans = Vec::with_capacity(4);

            // Star indicator
            if article.starred {
                spans.push(Span::styled("★ ", style_star));
            }

            // Feed name prefix in starred mode
            let feed_prefix_width = if let Some(prefix) = feed_prefix {
                spans.push(Span::styled(prefix.as_str(), style_feed_prefix));
                display_width(prefix)
            } else {
                0
            };

            // Title style based on read status and selection
            let title_style = if i == app.selected_article {
                style_selected
            } else if !article.read {
                style_title
            } else {
                style_read
            };

            // Calculate widths for right-alignment
            // Available width = panel width - borders (2)
            let available_width = area.width.saturating_sub(2) as usize;
            let star_width = if article.starred { 2 } else { 0 };
            let time_width = display_width(&time_str);
            // Minimum padding between title and time
            let min_padding = 2;

            // Max title width = available - star - feed_prefix - time - padding
            let max_title_len = available_width
                .saturating_sub(star_width)
                .saturating_sub(feed_prefix_width)
                .saturating_sub(time_width)
                .saturating_sub(min_padding);

            // SAFE - character-aware truncation using unicode-width
            let title = truncate_to_width(&article.title, max_title_len);
            let title_width = display_width(&title);

            spans.push(Span::styled(title, title_style));

            // Right-align time: calculate padding to push to right edge
            if !time_str.is_empty() {
                let used_width = star_width + feed_prefix_width + title_width + time_width;
                let padding = available_width.saturating_sub(used_width);
                spans.push(Span::styled(
                    format!("{:>width$}", time_str, width = padding + time_width),
                    style_date,
                ));
            }

            items.push(ListItem::new(Line::from(spans)));
        }
        items
    };

    let border_style = if is_focused {
        app.style("panel_border_focused")
    } else {
        app.style("panel_border")
    };

    let title = if app.search_mode {
        format!("Search: {}_", app.search_input)
    } else if app.starred_mode {
        "★ Starred Articles".to_owned()
    } else if let Some(feed) = app.selected_feed() {
        format!("Articles - {}", feed.title)
    } else {
        "Articles".to_owned()
    };

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(border_style)
                .title(title),
        )
        .highlight_style(Style::default()); // Selection styling handled per-item above

    // Use ListState to enable auto-scrolling to keep selection visible
    let mut state = ListState::default().with_selected(Some(app.selected_article));
    f.render_stateful_widget(list, area, &mut state);
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    // Base timestamp: 2024-01-15 12:00:00 UTC
    const NOW: i64 = 1705320000;

    #[test]
    fn test_format_relative_time_none() {
        let result = format_relative_time_with_now(None, NOW);
        assert_eq!(result, "");
    }

    #[test]
    fn test_format_relative_time_future() {
        let future = NOW + 3600;
        let result = format_relative_time_with_now(Some(future), NOW);
        assert_eq!(result, "now");
    }

    #[test]
    fn test_format_relative_time_minutes() {
        let result = format_relative_time_with_now(Some(NOW - 300), NOW);
        assert_eq!(result, "5m");

        let result = format_relative_time_with_now(Some(NOW - 3540), NOW);
        assert_eq!(result, "59m");
    }

    #[test]
    fn test_format_relative_time_hours() {
        let result = format_relative_time_with_now(Some(NOW - 3600), NOW);
        assert_eq!(result, "1h");

        let result = format_relative_time_with_now(Some(NOW - 82800), NOW);
        assert_eq!(result, "23h");
    }

    #[test]
    fn test_format_relative_time_days() {
        let result = format_relative_time_with_now(Some(NOW - 86400), NOW);
        assert_eq!(result, "1d");

        let result = format_relative_time_with_now(Some(NOW - 518400), NOW);
        assert_eq!(result, "6d");
    }

    #[test]
    fn test_format_relative_time_older_shows_date() {
        let result = format_relative_time_with_now(Some(NOW - 691200), NOW);
        // Should show date format like "Jan 07" not "8d"
        assert!(!result.ends_with('d'));
        assert!(result.contains("Jan"));
    }
}
