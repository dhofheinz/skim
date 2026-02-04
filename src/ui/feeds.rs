use crate::app::{App, Focus};
use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem},
    Frame,
};

/// Render the feed list panel
pub fn render(f: &mut Frame, app: &App, area: Rect) {
    let is_focused = app.focus == Focus::Feeds;

    let items: Vec<ListItem> = if app.feeds.is_empty() {
        vec![ListItem::new("No feeds loaded")]
    } else {
        app.feeds
            .iter()
            .enumerate()
            .map(|(i, feed)| {
                let unread_text = if feed.unread_count > 0 {
                    format!(" ({})", feed.unread_count)
                } else {
                    String::new()
                };

                let content = format!("{}{}", feed.title, unread_text);

                let style = if i == app.selected_feed {
                    Style::default().bg(Color::DarkGray).fg(Color::White)
                } else if feed.unread_count > 0 {
                    Style::default().add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };

                // Show error indicator if feed has error
                let line = if feed.error.is_some() {
                    Line::from(vec![
                        Span::styled("⚠ ", Style::default().fg(Color::Red)),
                        Span::styled(content, style),
                    ])
                } else {
                    Line::from(Span::styled(content, style))
                };

                ListItem::new(line)
            })
            .collect()
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
