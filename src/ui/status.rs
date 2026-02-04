use crate::app::{App, View};
use ratatui::{
    layout::Rect,
    style::{Color, Style},
    widgets::Paragraph,
    Frame,
};

/// Render the status bar
pub fn render(f: &mut Frame, app: &App, area: Rect) {
    let text = if let Some((done, total)) = app.refresh_progress {
        // Show refresh progress
        format!("Refreshing... {}/{} feeds", done, total)
    } else if let Some((msg, _)) = &app.status_message {
        // Show status message
        msg.clone()
    } else {
        // Show keybindings for current view
        match app.view {
            View::Browse => {
                if app.search_mode {
                    "Type to search | ESC cancel | ENTER confirm".to_string()
                } else {
                    "[r]efresh all [R]efresh one [/]search [s]tar [o]pen [Tab]switch [q]uit"
                        .to_string()
                }
            }
            View::Reader => "[b]ack [j/k]scroll [Ctrl+d/u]page [s]tar [o]pen [q]uit".to_string(),
        }
    };

    let style = Style::default().bg(Color::DarkGray).fg(Color::White);

    let paragraph = Paragraph::new(text).style(style);
    f.render_widget(paragraph, area);
}
