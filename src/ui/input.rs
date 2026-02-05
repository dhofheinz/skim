//! Input handling for the TUI.
//!
//! This module processes keyboard input and dispatches to the appropriate
//! handler based on current view and mode.

use crate::app::{App, AppEvent, CachedArticleState, ContentState, FetchResult, Focus, View};
use crate::feed::{refresh_all, refresh_one};
use anyhow::Result;
use crossterm::event::{KeyCode, KeyModifiers};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

use super::helpers::{
    catch_task_panic, exit_search_mode, exit_starred_mode, restore_articles_from_search,
    try_spawn_content_load, validate_url_for_open, ERR_ARTICLE_NO_URL,
};
use super::Action;

/// Maximum allowed search query length (UI layer validation)
const MAX_SEARCH_LENGTH: usize = 256;

/// Main input dispatch function.
///
/// Routes input to the appropriate handler based on current mode and view.
pub(super) async fn handle_input(
    app: &mut App,
    code: KeyCode,
    modifiers: KeyModifiers,
    event_tx: &mpsc::Sender<AppEvent>,
) -> Result<Action> {
    // Handle search mode input separately
    if app.search_mode {
        return handle_search_input(app, code).await;
    }

    match app.view {
        View::Browse => handle_browse_input(app, code, modifiers, event_tx).await,
        View::Reader => handle_reader_input(app, code, modifiers, event_tx),
    }
}

/// Handle input in browse view (feeds + articles panels).
async fn handle_browse_input(
    app: &mut App,
    code: KeyCode,
    _modifiers: KeyModifiers,
    event_tx: &mpsc::Sender<AppEvent>,
) -> Result<Action> {
    match code {
        KeyCode::Char('q') => return Ok(Action::Quit),
        KeyCode::Esc => {
            // Priority: Dismiss What's New panel first, then exit starred mode
            if app.show_whats_new {
                app.dismiss_whats_new();
            } else {
                exit_starred_mode(app, "ESC").await?;
            }
        }
        KeyCode::Char('j') | KeyCode::Down => app.nav_down(),
        KeyCode::Char('k') | KeyCode::Up => app.nav_up(),
        KeyCode::Tab => {
            app.focus = if app.show_whats_new {
                match app.focus {
                    Focus::WhatsNew => Focus::Feeds,
                    Focus::Feeds => Focus::Articles,
                    Focus::Articles => Focus::WhatsNew,
                }
            } else {
                match app.focus {
                    Focus::Feeds => Focus::Articles,
                    Focus::Articles => Focus::Feeds,
                    Focus::WhatsNew => Focus::Feeds, // Shouldn't happen, but handle gracefully
                }
            };
        }
        KeyCode::Enter => {
            handle_enter_key(app, event_tx).await?;
        }
        KeyCode::Char('r') => {
            handle_refresh_all(app, event_tx).await;
        }
        KeyCode::Char('R') => {
            handle_refresh_one(app, event_tx).await;
        }
        KeyCode::Char('s') => {
            handle_star_toggle_browse(app, event_tx).await;
        }
        KeyCode::Char('o') => {
            if let Some(article) = app.selected_article() {
                if let Some(url) = &article.url {
                    // SEC: Validate URL before open::that() to prevent command injection
                    if let Err(e) = validate_url_for_open(url) {
                        app.set_status(e);
                    } else if let Err(e) = open::that(&**url) {
                        app.set_status(format!("Failed to open browser: {}", e));
                    }
                } else {
                    app.set_status(ERR_ARTICLE_NO_URL);
                }
            }
        }
        KeyCode::Char('O') => {
            // Open feed website (html_url from OPML)
            if let Some(feed) = app.selected_feed() {
                if let Some(url) = &feed.html_url {
                    // SEC: Validate URL before open::that() to prevent command injection
                    if let Err(e) = validate_url_for_open(url) {
                        app.set_status(e);
                    } else if let Err(e) = open::that(url) {
                        app.set_status(format!("Failed to open browser: {}", e));
                    } else {
                        app.set_status(format!("Opening {} website...", feed.title));
                    }
                } else {
                    app.set_status("No website URL for this feed");
                }
            }
        }
        KeyCode::Char('/') => {
            // PERF-008: Cache current state before entering search mode
            // Arc::clone is O(1) - just increments reference count
            app.cached_articles = Some(CachedArticleState {
                feed_id: app.selected_feed().map(|f| f.id),
                articles: Arc::clone(&app.articles),
                selected: app.selected_article,
            });
            app.search_mode = true;
            app.search_input.clear();
            app.search_feed_id = app.selected_feed().map(|f| f.id);
        }
        KeyCode::Char('S') => {
            handle_starred_mode_toggle(app).await?;
        }
        _ => {}
    }
    Ok(Action::Continue)
}

/// Handle Enter key press in browse view.
async fn handle_enter_key(app: &mut App, event_tx: &mpsc::Sender<AppEvent>) -> Result<()> {
    if app.focus == Focus::WhatsNew {
        // Open article from What's New panel - use get() directly for safety
        match app.whats_new.get(app.whats_new_selected).cloned() {
            Some((_, article)) => {
                let article_id = article.id;

                // Set up reader view
                app.view = View::Reader;
                app.scroll_offset = 0;
                app.content_state = ContentState::Loading { article_id };
                app.reader_article = Some(article.clone());
                app.db.mark_article_read(article_id).await?;

                // Spawn content loading task
                try_spawn_content_load(app, &article, event_tx);
            }
            None => {
                // Either empty list or stale selection index - reset and handle
                app.whats_new_selected = 0;
                if app.whats_new.is_empty() {
                    app.set_status("No new articles");
                } else if let Some((_, article)) = app.whats_new.first().cloned() {
                    // Retry with index 0 after reset
                    let article_id = article.id;

                    app.view = View::Reader;
                    app.scroll_offset = 0;
                    app.content_state = ContentState::Loading { article_id };
                    app.reader_article = Some(article.clone());
                    app.db.mark_article_read(article_id).await?;

                    try_spawn_content_load(app, &article, event_tx);
                }
            }
        }
    } else if app.focus == Focus::Feeds {
        // Load articles for selected feed
        if let Some(feed) = app.selected_feed() {
            let feed_id = feed.id;
            // PERF-008: Invalidate cache when feed selection changes
            app.cached_articles = None;
            app.articles = Arc::new(app.db.get_articles_for_feed(feed_id, None).await?);
            app.selected_article = 0;
            app.clamp_selections();
            app.focus = Focus::Articles;
        }
    } else if app.focus == Focus::Articles {
        // Enter reader view and spawn content loading task
        if let Some(article) = app.enter_reader() {
            app.db.mark_article_read(article.id).await?;

            // Spawn content loading task
            try_spawn_content_load(app, &article, event_tx);
        }
    }
    Ok(())
}

/// Handle refresh all feeds (r key).
async fn handle_refresh_all(app: &mut App, event_tx: &mpsc::Sender<AppEvent>) {
    // Prevent multiple concurrent refreshes
    if app.refresh_progress.is_some() {
        app.set_status("Refresh already in progress");
    } else if app.feeds.is_empty() {
        app.set_status("No feeds to refresh");
    } else {
        app.set_status("Refreshing feeds...");
        app.refresh_progress = Some((0, app.feeds.len()));

        // Clone what we need for the background task
        // PERF-011: Arc::clone is O(1) - just increments reference count
        let db = app.db.clone();
        let client = app.http_client.clone();
        let feeds = Arc::clone(&app.feeds);
        let tx = event_tx.clone();

        // Create progress channel with proper lifecycle management
        // RES-001: Clone sender for task, drop original immediately
        // This ensures receiver terminates when task completes (success or panic)
        let (progress_tx, mut progress_rx) = mpsc::channel::<(usize, usize)>(32);
        let progress_tx_for_task = progress_tx.clone();
        drop(progress_tx); // Drop original - only task has sender now

        // Spawn refresh task
        tokio::spawn(async move {
            let tx_panic = tx.clone();
            match catch_task_panic(async {
                // Clone tx for rate limit events
                let rate_limit_tx = tx.clone();

                // Spawn the refresh in a separate task
                let refresh_handle = tokio::spawn(async move {
                    refresh_all(db, client, feeds, progress_tx_for_task, Some(rate_limit_tx))
                        .await
                });

                // Forward progress updates with timeout safety net
                // RES-001: Receiver loop terminates when sender drops OR timeout
                loop {
                    tokio::select! {
                        Some((done, total)) = progress_rx.recv() => {
                            if let Err(e) = tx.send(AppEvent::RefreshProgress(done, total)).await {
                                tracing::warn!(error = %e, event = "RefreshProgress", "Channel send failed (receiver dropped)");
                            }
                        }
                        _ = tokio::time::sleep(Duration::from_secs(120)) => {
                            tracing::warn!("Progress receiver timed out after 120s");
                            break;
                        }
                        else => break, // Channel closed (sender dropped)
                    }
                }

                // Get results and send completion
                if let Ok(results) = refresh_handle.await {
                    let fetch_results: Vec<FetchResult> = results
                        .into_iter()
                        .map(|r| {
                            let (new_articles, error) = match r.result {
                                Ok(count) => (count, None),
                                Err(e) => (0, Some(e.to_string())),
                            };
                            FetchResult {
                                feed_id: r.feed_id,
                                new_articles,
                                error,
                            }
                        })
                        .collect();
                    if let Err(e) = tx.send(AppEvent::RefreshComplete(fetch_results)).await {
                        tracing::warn!(error = %e, event = "RefreshComplete", "Channel send failed (receiver dropped)");
                    }
                }
            })
            .await
            {
                Ok(()) => {}
                Err(panic_msg) => {
                    tracing::error!(task = "refresh", error = %panic_msg, "Background task panicked");
                    let _ = tx_panic
                        .send(AppEvent::TaskPanicked {
                            task: "refresh",
                            error: panic_msg,
                        })
                        .await;
                }
            }
        });
    }
}

/// Handle refresh single feed (Shift+R).
async fn handle_refresh_one(app: &mut App, event_tx: &mpsc::Sender<AppEvent>) {
    if app.refresh_progress.is_some() {
        app.set_status("Refresh already in progress");
    } else if let Some(feed) = app.selected_feed().cloned() {
        app.set_status(format!("Refreshing {}...", feed.title));
        app.refresh_progress = Some((0, 1));

        let db = app.db.clone();
        let client = app.http_client.clone();
        let tx = event_tx.clone();

        tokio::spawn(async move {
            let tx_panic = tx.clone();
            match catch_task_panic(async {
                let result = refresh_one(&db, &client, &feed, Some(&tx)).await;

                // Send progress complete
                if let Err(e) = tx.send(AppEvent::RefreshProgress(1, 1)).await {
                    tracing::warn!(error = %e, event = "RefreshProgress", "Channel send failed (receiver dropped)");
                }

                // Convert to FetchResult for completion event
                let (new_articles, error): (usize, Option<String>) = match result.result {
                    Ok(count) => (count, None),
                    Err(e) => (0, Some(e.to_string())),
                };
                let fetch_result = FetchResult {
                    feed_id: result.feed_id,
                    new_articles,
                    error,
                };
                if let Err(e) = tx.send(AppEvent::RefreshComplete(vec![fetch_result])).await {
                    tracing::warn!(error = %e, event = "RefreshComplete", "Channel send failed (receiver dropped)");
                }
            })
            .await
            {
                Ok(()) => {}
                Err(panic_msg) => {
                    tracing::error!(task = "refresh_one", error = %panic_msg, "Background task panicked");
                    let _ = tx_panic
                        .send(AppEvent::TaskPanicked {
                            task: "refresh_one",
                            error: panic_msg,
                        })
                        .await;
                }
            }
        });
    } else {
        app.set_status("No feed selected");
    }
}

/// Handle star toggle in browse view.
async fn handle_star_toggle_browse(app: &mut App, event_tx: &mpsc::Sender<AppEvent>) {
    if let Some(article) = app.selected_article() {
        let article_id = article.id;
        let current_starred = article.starred;

        // PERF-015: Use Arc::make_mut for copy-on-write optimization
        // Only clones the Vec if there are other references, otherwise mutates in place
        let articles = Arc::make_mut(&mut app.articles);
        if let Some(article) = articles.iter_mut().find(|a| a.id == article_id) {
            article.starred = !current_starred;
        }
        // Also update in whats_new if present
        if let Some((_, article)) = app.whats_new.iter_mut().find(|(_, a)| a.id == article_id) {
            article.starred = !current_starred;
        }
        app.needs_redraw = true;

        // Invalidate cache if it exists - prevents stale data on search/starred mode exit
        if app.cached_articles.is_some() {
            tracing::debug!(article_id, "Invalidating article cache due to star toggle");
            app.cached_articles = None;
        }

        // Spawn background task to persist change
        let db = app.db.clone();
        let tx = event_tx.clone();
        tokio::spawn(async move {
            let tx_panic = tx.clone();
            match catch_task_panic(async {
                match db.toggle_article_starred(article_id).await {
                    Ok(new_status) => {
                        if let Err(e) = tx
                            .send(AppEvent::StarToggled {
                                article_id,
                                starred: new_status,
                            })
                            .await
                        {
                            tracing::warn!(error = %e, event = "StarToggled", "Channel send failed (receiver dropped)");
                        }
                    }
                    Err(e) => {
                        tracing::error!(error = %e, article_id, "Failed to toggle star");
                        if let Err(e) = tx
                            .send(AppEvent::StarToggleFailed { article_id, original_status: current_starred })
                            .await
                        {
                            tracing::warn!(error = %e, event = "StarToggleFailed", "Channel send failed (receiver dropped)");
                        }
                    }
                }
            })
            .await
            {
                Ok(()) => {}
                Err(panic_msg) => {
                    tracing::error!(task = "star_toggle", article_id, error = %panic_msg, "Background task panicked");
                    let _ = tx_panic
                        .send(AppEvent::TaskPanicked {
                            task: "star_toggle",
                            error: panic_msg,
                        })
                        .await;
                }
            }
        });
    }
}

/// Handle starred mode toggle (S key).
async fn handle_starred_mode_toggle(app: &mut App) -> Result<()> {
    if app.starred_mode {
        exit_starred_mode(app, "S toggle").await?;
    } else {
        // Enter starred mode - cache current state first
        // PERF-008: Cache current state before entering starred mode
        // Arc::clone is O(1) - just increments reference count
        app.cached_articles = Some(CachedArticleState {
            feed_id: app.selected_feed().map(|f| f.id),
            articles: Arc::clone(&app.articles),
            selected: app.selected_article,
        });

        match app.db.get_starred_articles().await {
            Ok(starred) => {
                tracing::info!(starred_count = starred.len(), "Entering starred mode");
                app.starred_mode = true;

                // PERF-014: Build prefix cache for starred mode display
                // Pre-compute formatted feed prefixes to avoid N allocations per render frame
                app.feed_prefix_cache.clear();
                for article in &starred {
                    if !app.feed_prefix_cache.contains_key(&article.feed_id) {
                        if let Some(feed_title) = app.feed_title_cache.get(&article.feed_id) {
                            let prefix =
                                format!("[{}] ", crate::util::truncate_to_width(feed_title, 15));
                            app.feed_prefix_cache.insert(article.feed_id, prefix);
                        }
                    }
                }

                app.articles = Arc::new(starred);
                app.selected_article = 0;
                app.clamp_selections();
                app.focus = Focus::Articles;
            }
            Err(e) => {
                // Failed to enter starred mode, clear cache
                app.cached_articles = None;
                app.set_status(format!("Failed to load starred: {}", e));
            }
        }
    }
    Ok(())
}

/// Handle input in reader view.
pub(super) fn handle_reader_input(
    app: &mut App,
    code: KeyCode,
    modifiers: KeyModifiers,
    event_tx: &mpsc::Sender<AppEvent>,
) -> Result<Action> {
    match code {
        KeyCode::Char('q') => return Ok(Action::Quit),
        KeyCode::Char('b') | KeyCode::Esc => {
            app.exit_reader();
        }
        KeyCode::Char('j') | KeyCode::Down => {
            app.scroll_down(1);
            app.clamp_reader_scroll();
        }
        KeyCode::Char('k') | KeyCode::Up => app.scroll_up(1),
        KeyCode::Char('d') if modifiers.contains(KeyModifiers::CONTROL) => {
            app.scroll_down(20);
            app.clamp_reader_scroll();
        }
        KeyCode::Char('u') if modifiers.contains(KeyModifiers::CONTROL) => app.scroll_up(20),
        KeyCode::Char('s') => {
            handle_star_toggle_reader(app, event_tx);
        }
        KeyCode::Char('o') => {
            if let Some(article) = app.reader_article.as_ref() {
                if let Some(url) = &article.url {
                    // SEC: Validate URL before open::that() to prevent command injection
                    if let Err(e) = validate_url_for_open(url) {
                        app.set_status(e);
                    } else if let Err(e) = open::that(&**url) {
                        app.set_status(format!("Failed to open browser: {}", e));
                    }
                }
            }
        }
        _ => {}
    }
    Ok(Action::Continue)
}

/// Handle star toggle in reader view.
fn handle_star_toggle_reader(app: &mut App, event_tx: &mpsc::Sender<AppEvent>) {
    if let Some(article) = app.reader_article.as_mut() {
        let article_id = article.id;
        let original_starred = article.starred;

        // Optimistic UI update in reader
        article.starred = !original_starred;
        app.needs_redraw = true;

        let db = app.db.clone();
        let tx = event_tx.clone();

        // Invalidate cache if it exists - prevents stale data on search/starred mode exit
        if app.cached_articles.is_some() {
            tracing::debug!(
                article_id,
                "Invalidating article cache due to star toggle in reader"
            );
            app.cached_articles = None;
        }

        tokio::spawn(async move {
            let tx_panic = tx.clone();
            match catch_task_panic(async {
                match db.toggle_article_starred(article_id).await {
                    Ok(new_status) => {
                        // Send authoritative new status from DB
                        if let Err(e) = tx
                            .send(AppEvent::StarToggled {
                                article_id,
                                starred: new_status,
                            })
                            .await
                        {
                            tracing::warn!(error = %e, event = "StarToggled", "Channel send failed (receiver dropped)");
                        }
                    }
                    Err(e) => {
                        tracing::error!(error = %e, article_id, "Failed to toggle star");
                        if let Err(e) = tx
                            .send(AppEvent::StarToggleFailed { article_id, original_status: original_starred })
                            .await
                        {
                            tracing::warn!(error = %e, event = "StarToggleFailed", "Channel send failed (receiver dropped)");
                        }
                    }
                }
            })
            .await
            {
                Ok(()) => {}
                Err(panic_msg) => {
                    tracing::error!(task = "star_toggle_reader", article_id, error = %panic_msg, "Background task panicked");
                    let _ = tx_panic
                        .send(AppEvent::TaskPanicked {
                            task: "star_toggle_reader",
                            error: panic_msg,
                        })
                        .await;
                }
            }
        });
    }
}

/// Handle input in search mode.
async fn handle_search_input(app: &mut App, code: KeyCode) -> Result<Action> {
    match code {
        KeyCode::Esc => {
            exit_search_mode(app).await?;
        }
        KeyCode::Enter => {
            // Cancel any pending debounce - explicit search takes priority
            // Must clear BEFORE search execution to prevent race with tick handler
            app.search_debounce = None;
            app.search_mode = false;
            // PERF-006: Execute pending search immediately on Enter
            // Use pending_search if available, otherwise use current search_input
            let query = app
                .pending_search
                .take()
                .unwrap_or_else(|| app.search_input.clone());
            if !query.is_empty() {
                // Committing to search results - clear cache
                app.cached_articles = None;
                app.articles = Arc::new(app.db.search_articles(&query).await?);
                app.selected_article = 0;
                app.clamp_selections();
            } else {
                // Empty query - restore original feed articles from cache or DB
                restore_articles_from_search(app, false).await?;
            }
            app.search_feed_id = None; // Clear after use
        }
        KeyCode::Backspace => {
            app.search_input.pop();
            // PERF-006: Set debounce instead of immediate search
            app.search_debounce = Some(tokio::time::Instant::now());
            app.pending_search = Some(app.search_input.clone());
        }
        KeyCode::Char(c) => {
            // Prevent input beyond max search length
            if app.search_input.len() >= MAX_SEARCH_LENGTH {
                app.set_status(format!(
                    "Search query at max length ({} chars)",
                    MAX_SEARCH_LENGTH
                ));
                return Ok(Action::Continue);
            }
            app.search_input.push(c);
            // PERF-006: Set debounce instead of immediate search
            app.search_debounce = Some(tokio::time::Instant::now());
            app.pending_search = Some(app.search_input.clone());
        }
        _ => {}
    }
    Ok(Action::Continue)
}
