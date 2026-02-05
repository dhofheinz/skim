//! Main event loop for the TUI.
//!
//! This module contains the core event loop that multiplexes terminal input,
//! background task events, and periodic ticks.

use crate::app::{App, AppEvent, ContentState, View};
use anyhow::Result;
use crossterm::{
    event::Event,
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use futures::StreamExt;
use ratatui::{backend::CrosstermBackend, Terminal};
use std::io::{self, Stdout};
use std::time::Duration;
use tokio::sync::mpsc;

#[cfg(unix)]
use tokio::signal::unix::{signal, SignalKind};

use super::events::handle_app_event;
use super::input::handle_input;
use super::render::render;

/// Maximum allowed search query length (UI layer validation)
const MAX_SEARCH_LENGTH: usize = 256;

/// Result of handling a key press event.
///
/// Returned by input handlers to signal whether the application should
/// continue running or terminate gracefully.
pub enum Action {
    /// Continue the event loop and process more events.
    Continue,
    /// Exit the application and restore the terminal.
    Quit,
}

/// Runs the TUI application event loop.
///
/// Uses `tokio::select!` to multiplex three event sources:
/// - **Terminal input**: Key presses from crossterm's async event stream
/// - **Background tasks**: Feed refresh, content loading via `AppEvent` channel
/// - **Periodic tick**: 250ms timer for status expiry and debounced search
///
/// # Panic Safety
///
/// Installs a panic hook that restores terminal state before unwinding,
/// ensuring the terminal is not left in raw mode on panic.
///
/// # Arguments
///
/// * `app` - Mutable application state
/// * `event_tx` - Sender for spawning background tasks
/// * `event_rx` - Receiver for background task completion events
///
/// # Returns
///
/// Returns `Ok(())` on graceful exit (user quit), or an error if terminal
/// setup fails.
pub async fn run(
    app: &mut App,
    event_tx: mpsc::Sender<AppEvent>,
    mut event_rx: mpsc::Receiver<AppEvent>,
) -> Result<()> {
    // Install panic hook BEFORE setting up terminal
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        original_hook(panic_info);
    }));

    let mut terminal = setup_terminal()?;
    let mut event_stream = crossterm::event::EventStream::new();

    // PERF-012: Use interval instead of sleep for consistent periodic ticks
    let mut tick_interval = tokio::time::interval(Duration::from_millis(250));

    // Signal handlers for graceful shutdown (Unix only)
    // On non-Unix platforms, these become pending futures that never complete
    #[cfg(unix)]
    let mut sigterm = signal(SignalKind::terminate())?;
    #[cfg(unix)]
    let mut sigint = signal(SignalKind::interrupt())?;

    loop {
        // PERF-010: Only render when state has changed
        if app.needs_redraw {
            terminal.draw(|f| render(f, app))?;
            app.needs_redraw = false;
        }

        // Clear expired status messages and trigger redraw if cleared
        if app.clear_expired_status() {
            app.needs_redraw = true;
        }

        // PERF-013: Drain all pending app events before handling more input.
        // This ensures background task results (ContentLoaded, RefreshComplete)
        // are processed promptly even during rapid user input, preventing event
        // starvation where typing while content is loading delays content display.
        while let Ok(event) = event_rx.try_recv() {
            app.needs_redraw = true;
            handle_app_event(app, event).await;
        }

        // Platform-specific signal futures
        #[cfg(unix)]
        let sigterm_fut = sigterm.recv();
        #[cfg(not(unix))]
        let sigterm_fut = std::future::pending::<Option<()>>();

        #[cfg(unix)]
        let sigint_fut = sigint.recv();
        #[cfg(not(unix))]
        let sigint_fut = std::future::pending::<Option<()>>();

        tokio::select! {
            biased;  // Process in order listed for predictable behavior

            // Signal handlers for graceful shutdown (highest priority)
            _ = sigterm_fut => {
                tracing::info!("Received SIGTERM, shutting down gracefully");
                break;
            }

            _ = sigint_fut => {
                tracing::info!("Received SIGINT, shutting down gracefully");
                break;
            }

            // Terminal input events
            maybe_event = event_stream.next() => {
                if let Some(Ok(Event::Key(key))) = maybe_event {
                    // BUG-006: Track last input time for idle detection
                    app.last_input_time = tokio::time::Instant::now();
                    app.needs_redraw = true;
                    match handle_input(app, key.code, key.modifiers, &event_tx).await {
                        Ok(Action::Quit) => break,
                        Ok(Action::Continue) => {}
                        Err(e) => app.set_status(format!("Error: {}", e)),
                    }
                }
            }

            // Background task events (blocking recv for when queue was empty)
            Some(event) = event_rx.recv() => {
                app.needs_redraw = true;
                handle_app_event(app, event).await;
            }

            // Periodic tick for status expiry and debounced search
            _ = tick_interval.tick() => {
                handle_tick(app, &event_tx);
            }
        }
    }

    restore_terminal(terminal)?;
    Ok(())
}

/// Number of frames in the loading spinner animation.
const SPINNER_FRAMES: usize = 10;

/// Handle periodic tick for debounced search execution.
///
/// PERF-015: Search is now spawned as an async background task to prevent
/// UI blocking on large article sets. Results are sent via AppEvent::SearchCompleted.
fn handle_tick(app: &mut App, event_tx: &mpsc::Sender<AppEvent>) {
    // Animate spinner when content is loading in reader view
    if app.view == View::Reader && matches!(app.content_state, ContentState::Loading { .. }) {
        app.spinner_frame = (app.spinner_frame + 1) % SPINNER_FRAMES;
        app.needs_redraw = true;
    }

    // PERF-006: Check for debounced search
    // Only execute debounced search if still in search mode
    if app.search_mode {
        if let Some(last_keystroke) = app.search_debounce {
            if last_keystroke.elapsed() >= Duration::from_millis(300) {
                app.needs_redraw = true;
                if let Some(query) = app.pending_search.take() {
                    if query.is_empty() {
                        // If query empty, spawn task to restore feed articles
                        if let Some(feed) = app.selected_feed() {
                            spawn_empty_query_restore(app, feed.id, event_tx);
                        }
                    } else if query.len() > MAX_SEARCH_LENGTH {
                        app.set_status(format!(
                            "Search query too long (max {} chars)",
                            MAX_SEARCH_LENGTH
                        ));
                    } else {
                        // PERF-015: Spawn search as background task
                        spawn_search(app, query, event_tx);
                    }
                }
                app.search_debounce = None;
            }
        }
    }
}

/// Spawn a background search task.
///
/// PERF-015: Search is async to prevent UI blocking. The task sends results
/// via `AppEvent::SearchCompleted` with a generation counter to handle rapid typing.
fn spawn_search(app: &mut App, query: String, event_tx: &mpsc::Sender<AppEvent>) {
    // Abort any previous search task
    if let Some(handle) = app.search_handle.take() {
        handle.abort();
        tracing::debug!("Aborted previous search task");
    }

    // Increment generation counter for this new search
    app.search_generation = app.search_generation.wrapping_add(1);
    let generation = app.search_generation;

    // Show "Searching..." status
    app.set_status("Searching...");

    // Clone what we need for the spawned task
    let db = app.db.clone();
    let tx = event_tx.clone();
    let query_for_task = query.clone();

    tracing::debug!(query = %query, generation, "Spawning async search task");

    app.search_handle = Some(tokio::spawn(async move {
        let results = db.search_articles(&query_for_task).await;
        let event = AppEvent::SearchCompleted {
            query: query_for_task,
            generation,
            results: results.map_err(|e| e.to_string()),
        };

        if let Err(e) = tx.send(event).await {
            tracing::warn!(error = %e, "Failed to send search results (receiver dropped)");
        }
    }));
}

/// Spawn a background task to restore feed articles when search query is cleared.
///
/// PERF-015: Like search, empty query restoration is also async to prevent UI blocking.
fn spawn_empty_query_restore(app: &mut App, feed_id: i64, event_tx: &mpsc::Sender<AppEvent>) {
    // Abort any previous search task
    if let Some(handle) = app.search_handle.take() {
        handle.abort();
        tracing::debug!("Aborted previous search task for empty query restore");
    }

    // Increment generation counter
    app.search_generation = app.search_generation.wrapping_add(1);
    let generation = app.search_generation;

    // Clone what we need for the spawned task
    let db = app.db.clone();
    let tx = event_tx.clone();

    tracing::debug!(feed_id, generation, "Spawning async feed restore task");

    app.search_handle = Some(tokio::spawn(async move {
        let results = db.get_articles_for_feed(feed_id, None).await;
        let event = AppEvent::SearchCompleted {
            query: String::new(), // Empty query indicates restore
            generation,
            results: results.map_err(|e| e.to_string()),
        };

        if let Err(e) = tx.send(event).await {
            tracing::warn!(error = %e, "Failed to send restore results (receiver dropped)");
        }
    }));
}

/// Set up the terminal for TUI rendering.
fn setup_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let terminal = Terminal::new(backend)?;
    Ok(terminal)
}

/// Restore terminal to normal state.
fn restore_terminal(mut terminal: Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}
