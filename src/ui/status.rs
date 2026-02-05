use crate::app::{App, View};
use ratatui::{
    layout::Rect,
    style::{Color, Style},
    widgets::Paragraph,
    Frame,
};
use std::borrow::Cow;

/// Render the status bar
pub fn render(f: &mut Frame, app: &App, area: Rect) {
    // EDGE-001: Guard against zero-width/height areas
    // Status bar needs at least 1 char width to be meaningful
    if area.width < 1 || area.height < 1 {
        return;
    }

    // Use Cow to avoid allocations for static strings and borrowed status messages
    let text: Cow<'_, str> = if let Some((done, total)) = app.refresh_progress {
        // Dynamic content requires allocation
        Cow::Owned(format!("Refreshing... {}/{} feeds", done, total))
    } else if let Some((msg, _)) = &app.status_message {
        // Borrow existing status message instead of cloning
        Cow::Borrowed(msg.as_str())
    } else {
        // Static keybinding hints - zero allocation
        match app.view {
            View::Browse => {
                if app.search_mode {
                    Cow::Borrowed("Type to search | ESC cancel | ENTER confirm")
                } else {
                    Cow::Borrowed(
                        "[r]efresh all [R]efresh one [/]search [s]tar [o]pen [Tab]switch [q]uit",
                    )
                }
            }
            View::Reader => Cow::Borrowed("[b]ack [j/k]scroll [Ctrl+d/u]page [s]tar [o]pen [q]uit"),
        }
    };

    let style = Style::default().bg(Color::DarkGray).fg(Color::White);

    let paragraph = Paragraph::new(text).style(style);
    f.render_widget(paragraph, area);
}
