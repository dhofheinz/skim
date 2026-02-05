use crate::app::{App, Focus};
use crate::ui::articles::format_relative_time;
use crate::util::{display_width, truncate_to_width};
use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem},
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

    let items: Vec<ListItem> = app
        .whats_new
        .iter()
        .enumerate()
        .map(|(i, (feed_title, article))| {
            let time_str = format_relative_time(article.published);

            // Pre-allocate spans: feed name, title, time
            let mut spans = Vec::with_capacity(3);

            // Article title
            let title_style = if i == app.whats_new_selected {
                Style::default()
                    .bg(Color::DarkGray)
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().add_modifier(Modifier::BOLD)
            };

            // Calculate widths for right-alignment
            // Format feed prefix once, reuse for width calculation and span
            let available_width = area.width.saturating_sub(2) as usize;
            let feed_prefix = format!("[{}] ", feed_title);
            let feed_width = display_width(&feed_prefix);

            // Feed name in brackets (move ownership of pre-formatted string)
            spans.push(Span::styled(feed_prefix, Style::default().fg(Color::Cyan)));
            let time_width = display_width(&time_str);
            let min_padding = 2;

            // Max title width = available - feed prefix - time - padding
            let max_title_len = available_width
                .saturating_sub(feed_width)
                .saturating_sub(time_width)
                .saturating_sub(min_padding);

            // SAFE - character-aware truncation using unicode-width
            let title = truncate_to_width(&article.title, max_title_len);
            let title_width = display_width(&title);

            spans.push(Span::styled(title, title_style));

            // Right-align time
            if !time_str.is_empty() {
                let used_width = feed_width + title_width + time_width;
                let padding = available_width.saturating_sub(used_width);
                spans.push(Span::styled(
                    format!("{:>width$}", time_str, width = padding + time_width),
                    Style::default().fg(Color::DarkGray),
                ));
            }

            ListItem::new(Line::from(spans))
        })
        .collect();

    let border_style = if is_focused {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let title = format!(
        "✨ What's New ({}) - [Esc] dismiss, [Tab] switch, [Enter] open",
        app.whats_new.len()
    );

    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(border_style)
            .title(title),
    );

    f.render_widget(list, area);
}
