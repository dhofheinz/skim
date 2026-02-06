//! Reading stats panel rendering.

use crate::app::App;
use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
    Frame,
};

/// Render the reading stats panel as a centered overlay.
pub(super) fn render(f: &mut Frame, app: &App) {
    let area = f.area();

    // Centered panel: 60% width, 80% height (with reasonable minimums)
    let width = (area.width * 60 / 100)
        .max(40)
        .min(area.width.saturating_sub(4));
    let height = (area.height * 80 / 100)
        .max(12)
        .min(area.height.saturating_sub(2));
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let popup = Rect::new(x, y, width, height);

    if popup.width < 30 || popup.height < 8 {
        return;
    }

    f.render_widget(Clear, popup);

    match &app.stats_data {
        None => {
            let loading = Paragraph::new("Loading stats...")
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(app.style("panel_border_focused"))
                        .title(" Reading Stats "),
                )
                .alignment(Alignment::Center)
                .style(app.style("reader_body"));
            f.render_widget(loading, popup);
        }
        Some(data) => {
            // Check for empty history
            let is_empty = data.today.top_feeds.is_empty()
                && data.week.top_feeds.is_empty()
                && data.month.top_feeds.is_empty()
                && data.today.total_minutes == 0
                && data.week.total_minutes == 0
                && data.month.total_minutes == 0;

            if is_empty {
                let msg = Paragraph::new(
                    "No reading history yet.\n\nStart reading articles to see your stats here.",
                )
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(app.style("panel_border_focused"))
                        .title(" Reading Stats "),
                )
                .alignment(Alignment::Center)
                .style(app.style("reader_body"));
                f.render_widget(msg, popup);
                return;
            }

            // Split popup into sections
            let inner = Block::default()
                .borders(Borders::ALL)
                .border_style(app.style("panel_border_focused"))
                .title(" Reading Stats ");
            let inner_area = inner.inner(popup);
            f.render_widget(inner, popup);

            let sections = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(5), // Summary section
                    Constraint::Length(1), // Spacer
                    Constraint::Min(3),    // Top feeds
                    Constraint::Length(1), // Footer
                ])
                .split(inner_area);

            // Summary section
            let summary_lines = vec![
                Line::from(vec![
                    Span::styled("  Today:      ", app.style("feed_title")),
                    Span::raw(format_stats_line(&data.today)),
                ]),
                Line::from(vec![
                    Span::styled("  This week:  ", app.style("feed_title")),
                    Span::raw(format_stats_line(&data.week)),
                ]),
                Line::from(vec![
                    Span::styled("  This month: ", app.style("feed_title")),
                    Span::raw(format_stats_line(&data.month)),
                ]),
            ];
            let summary = Paragraph::new(summary_lines).style(app.style("reader_body"));
            f.render_widget(summary, sections[0]);

            // Top feeds section
            let top = &data.month.top_feeds;
            let mut feed_lines: Vec<Line> = vec![Line::from(Span::styled(
                "  Top feeds (30 days):",
                app.style("feed_title"),
            ))];
            for (i, (title, count)) in top.iter().take(5).enumerate() {
                feed_lines.push(Line::from(format!(
                    "    {}. {} ({} articles)",
                    i + 1,
                    title,
                    count
                )));
            }
            if top.is_empty() {
                feed_lines.push(Line::from("    (no data)"));
            }
            let feeds_para = Paragraph::new(feed_lines).style(app.style("reader_body"));
            f.render_widget(feeds_para, sections[2]);

            // Footer
            let footer = Paragraph::new("  Press Esc to close")
                .style(app.style("status_bar"))
                .alignment(Alignment::Left);
            f.render_widget(footer, sections[3]);
        }
    }
}

/// Format a single stats line: "X articles, Ym reading time"
fn format_stats_line(stats: &crate::storage::ReadingStats) -> String {
    let articles: u64 = stats.top_feeds.iter().map(|(_, c)| *c as u64).sum();
    if articles == 0 && stats.total_minutes == 0 {
        return "no activity".to_string();
    }

    let time_str = if stats.total_minutes >= 60 {
        format!(
            "{}h {}m",
            stats.total_minutes / 60,
            stats.total_minutes % 60
        )
    } else {
        format!("{}m", stats.total_minutes)
    };

    format!("{} articles, {} reading time", articles, time_str)
}
