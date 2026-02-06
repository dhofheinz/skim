use crate::app::{App, Focus};
use crate::ui::articles::format_relative_time;
use crate::util::{display_width, truncate_to_width};
use ratatui::{
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState},
    Frame,
};

/// Render the What's New panel showing recently fetched articles
pub fn render(f: &mut Frame, app: &App, area: Rect) {
    // EDGE-001: Guard against zero-width/height areas
    // Layout may produce zero-sized rects during extreme terminal resizes
    if area.width < 3 || area.height < 3 {
        return;
    }

    let is_focused = app.focus == Focus::WhatsNew;

    // PERF-021: Hoist style lookups out of per-item loop
    let style_selected = app.style("whatsnew_selected");
    let style_title = app.style("whatsnew_title");
    let style_feed_prefix = app.style("article_feed_prefix");
    let style_date = app.style("article_date");

    let items: Vec<ListItem> = app
        .whats_new
        .iter()
        .enumerate()
        .map(|(i, entry)| {
            let time_str = format_relative_time(entry.published);

            // Pre-allocate spans: feed name, title, time
            let mut spans = Vec::with_capacity(3);

            // Article title
            let title_style = if i == app.whats_new_selected {
                style_selected
            } else {
                style_title
            };

            // Calculate widths for right-alignment
            // Format feed prefix once, reuse for width calculation and span
            let available_width = area.width.saturating_sub(2) as usize;
            let feed_prefix = format!("[{}] ", entry.feed_title);
            let feed_width = display_width(&feed_prefix);

            // Feed name in brackets (move ownership of pre-formatted string)
            spans.push(Span::styled(feed_prefix, style_feed_prefix));
            let time_width = display_width(&time_str);
            let min_padding = 2;

            // Max title width = available - feed prefix - time - padding
            let max_title_len = available_width
                .saturating_sub(feed_width)
                .saturating_sub(time_width)
                .saturating_sub(min_padding);

            // SAFE - character-aware truncation using unicode-width
            let title = truncate_to_width(&entry.title, max_title_len);
            let title_width = display_width(&title);

            spans.push(Span::styled(title, title_style));

            // Right-align time
            if !time_str.is_empty() {
                let used_width = feed_width + title_width + time_width;
                let padding = available_width.saturating_sub(used_width);
                spans.push(Span::styled(
                    format!("{:>width$}", time_str, width = padding + time_width),
                    style_date,
                ));
            }

            ListItem::new(Line::from(spans))
        })
        .collect();

    let border_style = if is_focused {
        app.style("whatsnew_border_focused")
    } else {
        app.style("whatsnew_border_unfocused")
    };

    let title = format!(
        "✨ What's New ({}) - [Esc] dismiss, [Tab] switch, [Enter] open",
        app.whats_new.len()
    );

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(border_style)
                .title(title),
        )
        .highlight_style(Style::default()); // Selection styling handled per-item above

    // Use ListState to enable auto-scrolling to keep selection visible
    let mut state = ListState::default().with_selected(Some(app.whats_new_selected));
    f.render_stateful_widget(list, area, &mut state);
}
