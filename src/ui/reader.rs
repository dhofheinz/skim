use crate::app::{App, ContentState};
use crate::theme::StyleMap;
use crate::ui::articles::format_relative_time;
use pulldown_cmark::{Event, Parser, Tag, TagEnd};
use ratatui::{
    layout::Rect,
    text::{Line, Span, Text},
    widgets::{Block, Borders, Paragraph, Wrap},
    Frame,
};
use std::borrow::Cow;

/// Render the article reader view
pub fn render(f: &mut Frame, app: &mut App, area: Rect) {
    // EDGE-001: Guard against zero-width/height areas
    // Layout may produce zero-sized rects during extreme terminal resizes
    if area.width < 3 || area.height < 3 {
        return;
    }

    // Update viewport dimensions for scroll clamping (minus 2 for borders)
    app.reader_visible_lines = area.height.saturating_sub(2) as usize;
    app.reader_viewport_width = area.width.saturating_sub(2) as usize;

    // BUG-012: Clamp scroll BEFORE rendering to prevent visual glitches on resize.
    // Previously, clamping happened after render in render.rs, which could cause
    // one frame to render with invalid scroll offset during terminal resize.
    app.clamp_reader_scroll();

    let Some(article) = app.reader_article.as_ref() else {
        let paragraph = Paragraph::new("No article selected")
            .block(Block::default().borders(Borders::ALL).title("Reader"));
        f.render_widget(paragraph, area);
        return;
    };

    // Build header - O(1) lookup via feed title cache (PERF-005)
    // PERF-009: Deref Arc<str> to &str
    let feed_name = app
        .feed_title_cache
        .get(&article.feed_id)
        .map(|s| &**s)
        .unwrap_or("Unknown Feed");
    let time_str = format_relative_time(article.published);

    let header = vec![
        Line::from(Span::styled(&*article.title, app.style("reader_heading"))),
        Line::from(Span::styled(
            format!("{} • {}", feed_name, time_str),
            app.style("reader_metadata"),
        )),
        Line::from(""), // Blank line
    ];

    // Build content based on state (PERF-003)
    // Note: ratatui's Text requires ownership of lines. We use Cow to reference cached
    // lines when Loaded, avoiding intermediate Vec allocation. The from_iter + cloned()
    // still clones elements due to Text's API, but avoids Vec::clone() overhead.
    let content_lines: Cow<'_, [Line<'static>]> = match &app.content_state {
        ContentState::Idle => Cow::Owned(vec![Line::from("Press Enter to load content...")]),
        ContentState::Loading { .. } => {
            const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
            let frame = app.spinner_frame % SPINNER.len();
            Cow::Owned(vec![
                Line::from(""),
                Line::from(""),
                Line::from(format!("{} Loading content...", SPINNER[frame])),
            ])
        }
        ContentState::Loaded { rendered_lines, .. } => Cow::Borrowed(rendered_lines),
        ContentState::Failed {
            error, fallback, ..
        } => {
            let mut lines = vec![
                Line::from(Span::styled(
                    format!("Failed to load content: {}", error),
                    app.style("reader_error"),
                )),
                Line::from(""),
            ];
            if let Some(summary) = fallback {
                lines.push(Line::from(Span::styled(
                    "Showing summary:",
                    app.style("reader_fallback"),
                )));
                lines.push(Line::from(""));
                lines.extend(summary.lines().map(|l| Line::from(l.to_string())));
            }
            Cow::Owned(lines)
        }
    };

    // Build text by chaining iterators - avoids intermediate Vec allocation
    let text = Text::from_iter(header.into_iter().chain(content_lines.iter().cloned()));

    // scroll_offset is guaranteed <= u16::MAX by clamp_scroll() (see app::MAX_SCROLL)
    let paragraph = Paragraph::new(text)
        .block(Block::default().borders(Borders::ALL).title("Article"))
        .wrap(Wrap { trim: false })
        .scroll((app.scroll_offset as u16, 0));

    f.render_widget(paragraph, area);
}

/// Convert markdown to styled ratatui Lines.
/// Returns owned Lines for caching (PERF-004).
///
/// Accepts a `StyleMap` to resolve semantic roles for markdown elements.
pub fn render_markdown(md: &str, styles: &StyleMap) -> Vec<Line<'static>> {
    let parser = Parser::new(md);
    // Estimate: markdown lines roughly map to output lines
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(md.lines().count());
    // Most lines have few styled spans (text, emphasis, code, etc.)
    let mut current_spans: Vec<Span<'static>> = Vec::with_capacity(4);
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
                    styles.resolve("reader_image"),
                ));
            }
            Event::Text(text) => {
                let style = if in_code_block {
                    styles.resolve("reader_code_block")
                } else if in_heading {
                    styles.resolve("reader_heading")
                } else if in_strong {
                    styles.resolve("reader_strong")
                } else if in_emphasis {
                    styles.resolve("reader_emphasis")
                } else {
                    styles.resolve("reader_body")
                };
                // PERF-011: CowStr::into_string() is O(1) for Boxed variant (no allocation),
                // vs .to_string() which always allocates
                current_spans.push(Span::styled(text.into_string(), style));
            }
            Event::Code(code) => {
                current_spans.push(Span::styled(
                    format!("`{}`", code),
                    styles.resolve("reader_inline_code"),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::ThemeVariant;

    fn test_styles() -> StyleMap {
        StyleMap::from_palette(&ThemeVariant::Dark.palette())
    }

    #[test]
    fn test_render_plain_text() {
        let lines = render_markdown("Hello world", &test_styles());
        assert!(!lines.is_empty());
    }

    #[test]
    fn test_render_heading() {
        let lines = render_markdown("# Heading 1\n\n## Heading 2", &test_styles());
        assert!(!lines.is_empty());
    }

    #[test]
    fn test_render_bold() {
        let lines = render_markdown("This is **bold** text", &test_styles());
        assert!(!lines.is_empty());
    }

    #[test]
    fn test_render_italic() {
        let lines = render_markdown("This is *italic* text", &test_styles());
        assert!(!lines.is_empty());
    }

    #[test]
    fn test_render_code_block() {
        let lines = render_markdown("```\ncode block\n```", &test_styles());
        assert!(!lines.is_empty());
    }

    #[test]
    fn test_render_link() {
        let lines = render_markdown("[link text](https://example.com)", &test_styles());
        assert!(!lines.is_empty());
    }

    #[test]
    fn test_render_empty() {
        let lines = render_markdown("", &test_styles());
        // Should not panic - empty input is valid
        assert!(lines.is_empty());
    }

    #[test]
    fn test_render_unicode() {
        let lines = render_markdown("Hello 世界 🌍", &test_styles());
        assert!(!lines.is_empty());
    }
}
