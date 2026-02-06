use crate::app::{App, View};
use crate::storage::SearchScope;
use ratatui::{layout::Rect, widgets::Paragraph, Frame};
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
    } else if let Some((completed, total)) = app.prefetch_progress {
        Cow::Owned(format!("Prefetching: {}/{}...", completed, total))
    } else if let Some((msg, _)) = &app.status_message {
        // P-8: Borrow from Cow — zero-copy for both static and owned messages
        Cow::Borrowed(msg.as_ref())
    } else if app.search_mode
        && app.search_debounce.is_some()
        && app.search_scope == SearchScope::All
    {
        // TASK-9: Show content search indicator during debounce with All scope
        Cow::Borrowed("Searching all content...")
    } else {
        // Static keybinding hints - zero allocation
        match app.view {
            View::Browse => {
                if app.search_mode {
                    // TASK-6/9: Show scope indicator and Ctrl+S hint
                    match app.search_scope {
                        SearchScope::TitleAndSummary => Cow::Borrowed(
                            "[title+summary] Type to search | Ctrl+S: toggle scope | ESC cancel | ENTER confirm",
                        ),
                        SearchScope::All => Cow::Borrowed(
                            "[all] Type to search | Ctrl+S: toggle scope | ESC cancel | ENTER confirm",
                        ),
                    }
                } else {
                    Cow::Borrowed(
                        "[r]efresh all [R]efresh one [/]search [s]tar [o]pen [Tab]switch [q]uit",
                    )
                }
            }
            View::Reader => Cow::Borrowed("[b]ack [j/k]scroll [Ctrl+d/u]page [s]tar [o]pen [q]uit"),
            View::Stats => Cow::Borrowed("[Esc]close [q]uit"),
        }
    };

    let paragraph = Paragraph::new(text).style(app.style("status_bar"));
    f.render_widget(paragraph, area);
}
