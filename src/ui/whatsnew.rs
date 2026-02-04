use crate::app::{App, Focus};
use crate::ui::articles::format_relative_time;
use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem},
    Frame,
};

/// Render the What's New panel showing recently fetched articles
pub fn render(f: &mut Frame, app: &App, area: Rect) {
    let is_focused = app.focus == Focus::WhatsNew;

    let items: Vec<ListItem> = app
        .whats_new
        .iter()
        .enumerate()
        .map(|(i, (feed_title, article))| {
            let time_str = format_relative_time(article.published);

            let mut spans = Vec::new();

            // Feed name in brackets
            spans.push(Span::styled(
                format!("[{}] ", feed_title),
                Style::default().fg(Color::Cyan),
            ));

            // Article title
            let title_style = if i == app.whats_new_selected {
                Style::default()
                    .bg(Color::DarkGray)
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().add_modifier(Modifier::BOLD)
            };

            // Truncate title if needed
            let max_title_len = area.width.saturating_sub(30) as usize;
            let title = if article.title.len() > max_title_len {
                format!("{}...", &article.title[..max_title_len.saturating_sub(3)])
            } else {
                article.title.clone()
            };

            spans.push(Span::styled(title, title_style));

            // Time
            if !time_str.is_empty() {
                spans.push(Span::styled(
                    format!("  {}", time_str),
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
