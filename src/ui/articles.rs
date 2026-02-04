use crate::app::{App, Focus};
use chrono::{DateTime, Utc};
use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem},
    Frame,
};

/// Format timestamp as relative time
pub fn format_relative_time(timestamp: Option<i64>) -> String {
    let Some(ts) = timestamp else {
        return String::new();
    };

    let now = Utc::now().timestamp();
    let diff = now - ts;

    // Future dates (malformed feeds)
    if diff < 0 {
        return "now".to_string();
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
        .unwrap_or_default()
}

/// Render the article list panel
pub fn render(f: &mut Frame, app: &App, area: Rect) {
    let is_focused = app.focus == Focus::Articles;

    let items: Vec<ListItem> = if app.articles.is_empty() {
        vec![ListItem::new("No articles")]
    } else {
        app.articles
            .iter()
            .enumerate()
            .map(|(i, article)| {
                let time_str = format_relative_time(article.published);

                // Build line with star, title, and time
                let mut spans = Vec::new();

                // Star indicator
                if article.starred {
                    spans.push(Span::styled("★ ", Style::default().fg(Color::Yellow)));
                }

                // Title style based on read status and selection
                let title_style = if i == app.selected_article {
                    Style::default().bg(Color::DarkGray).fg(Color::White)
                } else if !article.read {
                    Style::default().add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::Gray)
                };

                // Truncate title if needed (leave room for time)
                let max_title_len = area.width.saturating_sub(12) as usize;
                let title = if article.title.len() > max_title_len {
                    format!("{}...", &article.title[..max_title_len.saturating_sub(3)])
                } else {
                    article.title.clone()
                };

                spans.push(Span::styled(title, title_style));

                // Time (right-aligned would be nice but simpler to just add space)
                if !time_str.is_empty() {
                    spans.push(Span::styled(
                        format!("  {}", time_str),
                        Style::default().fg(Color::DarkGray),
                    ));
                }

                ListItem::new(Line::from(spans))
            })
            .collect()
    };

    let border_style = if is_focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default()
    };

    let title = if app.search_mode {
        format!("Search: {}_", app.search_input)
    } else if let Some(feed) = app.selected_feed() {
        format!("Articles - {}", feed.title)
    } else {
        "Articles".to_string()
    };

    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(border_style)
            .title(title),
    );

    f.render_widget(list, area);
}
