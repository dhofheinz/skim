use crate::app::{App, ContentState};
use crate::ui::articles::format_relative_time;
use pulldown_cmark::{Event, Parser, Tag, TagEnd};
use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Paragraph, Wrap},
    Frame,
};

/// Render the article reader view
pub fn render(f: &mut Frame, app: &App, area: Rect) {
    let Some(article) = app.reader_article.as_ref() else {
        let paragraph = Paragraph::new("No article selected")
            .block(Block::default().borders(Borders::ALL).title("Reader"));
        f.render_widget(paragraph, area);
        return;
    };

    // Build header - try to find feed name from feeds list or what's new
    let feed_name = app
        .feeds
        .iter()
        .find(|f| f.id == article.feed_id)
        .map(|f| f.title.as_str())
        .or_else(|| {
            app.whats_new
                .iter()
                .find(|(_, a)| a.id == article.id)
                .map(|(name, _)| name.as_str())
        })
        .unwrap_or("Unknown");
    let time_str = format_relative_time(article.published);

    let header = vec![
        Line::from(Span::styled(
            &article.title,
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            format!("{} • {}", feed_name, time_str),
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(""), // Blank line
    ];

    // Build content based on state
    let content_lines: Vec<Line> = match &app.content_state {
        ContentState::Idle => {
            vec![Line::from("Press Enter to load content...")]
        }
        ContentState::Loading { .. } => {
            vec![Line::from("Loading content...")]
        }
        ContentState::Loaded { content, .. } => render_markdown(content),
        ContentState::Failed {
            error, fallback, ..
        } => {
            let mut lines = vec![
                Line::from(Span::styled(
                    format!("Failed to load content: {}", error),
                    Style::default().fg(Color::Red),
                )),
                Line::from(""),
            ];
            if let Some(summary) = fallback {
                lines.push(Line::from(Span::styled(
                    "Showing summary:",
                    Style::default().fg(Color::Yellow),
                )));
                lines.push(Line::from(""));
                lines.extend(summary.lines().map(|l| Line::from(l.to_string())));
            }
            lines
        }
    };

    // Combine header and content
    let mut all_lines = header;
    all_lines.extend(content_lines);

    let text = Text::from(all_lines);

    let paragraph = Paragraph::new(text)
        .block(Block::default().borders(Borders::ALL).title("Article"))
        .wrap(Wrap { trim: false })
        .scroll((app.scroll_offset as u16, 0));

    f.render_widget(paragraph, area);
}

/// Convert markdown to styled ratatui Lines
fn render_markdown(md: &str) -> Vec<Line<'static>> {
    let parser = Parser::new(md);
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut current_spans: Vec<Span<'static>> = Vec::new();
    let mut in_code_block = false;
    let mut in_heading = false;
    let mut in_emphasis = false;
    let mut in_strong = false;

    for event in parser {
        match event {
            Event::Start(Tag::Heading { .. }) => {
                in_heading = true;
            }
            Event::End(TagEnd::Heading(_)) => {
                if !current_spans.is_empty() {
                    lines.push(Line::from(std::mem::take(&mut current_spans)));
                }
                in_heading = false;
            }
            Event::Start(Tag::Paragraph) => {}
            Event::End(TagEnd::Paragraph) => {
                if !current_spans.is_empty() {
                    lines.push(Line::from(std::mem::take(&mut current_spans)));
                }
                lines.push(Line::from("")); // Blank line after paragraph
            }
            Event::Start(Tag::CodeBlock(_)) => {
                in_code_block = true;
            }
            Event::End(TagEnd::CodeBlock) => {
                in_code_block = false;
                lines.push(Line::from(""));
            }
            Event::Start(Tag::Emphasis) => {
                in_emphasis = true;
            }
            Event::End(TagEnd::Emphasis) => {
                in_emphasis = false;
            }
            Event::Start(Tag::Strong) => {
                in_strong = true;
            }
            Event::End(TagEnd::Strong) => {
                in_strong = false;
            }
            Event::Start(Tag::Link { .. }) => {
                // Links will show URL after text
            }
            Event::End(TagEnd::Link) => {}
            Event::Start(Tag::Image { dest_url, .. }) => {
                current_spans.push(Span::styled(
                    format!("[Image: {}]", dest_url),
                    Style::default().fg(Color::Blue),
                ));
            }
            Event::Text(text) => {
                let style = if in_code_block {
                    Style::default().fg(Color::Yellow).bg(Color::Black)
                } else if in_heading {
                    Style::default()
                        .add_modifier(Modifier::BOLD)
                        .fg(Color::Cyan)
                } else if in_strong {
                    Style::default().add_modifier(Modifier::BOLD)
                } else if in_emphasis {
                    Style::default().add_modifier(Modifier::ITALIC)
                } else {
                    Style::default()
                };
                current_spans.push(Span::styled(text.to_string(), style));
            }
            Event::Code(code) => {
                current_spans.push(Span::styled(
                    format!("`{}`", code),
                    Style::default().fg(Color::Yellow),
                ));
            }
            Event::SoftBreak => {
                current_spans.push(Span::raw(" "));
            }
            Event::HardBreak => {
                if !current_spans.is_empty() {
                    lines.push(Line::from(std::mem::take(&mut current_spans)));
                }
            }
            _ => {}
        }
    }

    // Flush remaining spans
    if !current_spans.is_empty() {
        lines.push(Line::from(current_spans));
    }

    lines
}
