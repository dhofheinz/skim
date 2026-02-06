//! Input handling for the TUI.
//!
//! This module processes keyboard input and dispatches to the appropriate
//! handler based on current view and mode.

use crate::app::{App, AppEvent, CachedArticleState, ContentState, FetchResult, Focus, View};
use crate::feed::{refresh_all, refresh_one};
use crate::keybindings::{Action as KbAction, Context as KbContext};
use anyhow::Result;
use crossterm::event::{KeyCode, KeyModifiers};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

use super::helpers::{
    catch_task_panic, exit_search_mode, exit_starred_mode, restore_articles_from_search,
    try_spawn_content_load, ERR_ARTICLE_NO_URL,
};
use super::Action;
use crate::util::{validate_url_for_open, MAX_SEARCH_QUERY_LENGTH};

/// Map the current focus panel to a keybinding context for context-specific lookups.
fn focus_to_context(focus: Focus) -> KbContext {
    match focus {
        Focus::Feeds => KbContext::FeedList,
        Focus::Articles => KbContext::ArticleList,
        Focus::WhatsNew => KbContext::WhatsNew,
    }
}

/// Main input dispatch function.
///
/// Routes input to the appropriate handler based on current mode and view.
pub(super) async fn handle_input(
    app: &mut App,
    code: KeyCode,
    modifiers: KeyModifiers,
    event_tx: &mpsc::Sender<AppEvent>,
) -> Result<Action> {
    // Handle help overlay input first (captures all keys when visible)
    if app.show_help {
        return Ok(handle_help_input(app, code));
    }

    // Handle search mode input separately
    if app.search_mode {
        return handle_search_input(app, code).await;
    }

    match app.view {
        View::Browse => handle_browse_input(app, code, modifiers, event_tx).await,
        View::Reader => handle_reader_input(app, code, modifiers, event_tx),
    }
}

/// Handle input while the help overlay is visible.
///
/// Captures all keys: j/k/Up/Down scroll, Esc/q/? dismiss.
fn handle_help_input(app: &mut App, code: KeyCode) -> Action {
    match code {
        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('?') => {
            app.show_help = false;
            app.help_scroll_offset = 0;
        }
        KeyCode::Char('j') | KeyCode::Down => {
            app.help_scroll_offset = app.help_scroll_offset.saturating_add(1);
        }
        KeyCode::Char('k') | KeyCode::Up => {
            app.help_scroll_offset = app.help_scroll_offset.saturating_sub(1);
        }
        _ => {}
    }
    Action::Continue
}

/// Handle input in browse view (feeds + articles panels).
async fn handle_browse_input(
    app: &mut App,
    code: KeyCode,
    modifiers: KeyModifiers,
    event_tx: &mpsc::Sender<AppEvent>,
) -> Result<Action> {
    let context = focus_to_context(app.focus);
    let action = app.keybindings.action_for_key(code, modifiers, context);

    match action {
        Some(KbAction::Quit) => return Ok(Action::Quit),
        Some(KbAction::Back) => {
            // Priority: Dismiss What's New panel first, then exit starred mode
            if app.show_whats_new {
                app.dismiss_whats_new();
            } else {
                exit_starred_mode(app, "ESC").await?;
            }
        }
        Some(KbAction::NavDown) => app.nav_down(),
        Some(KbAction::NavUp) => app.nav_up(),
        Some(KbAction::CycleFocus) => {
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
        Some(KbAction::Select) => {
            handle_enter_key(app, event_tx).await?;
        }
        Some(KbAction::RefreshAll) => {
            handle_refresh_all(app, event_tx).await;
        }
        Some(KbAction::RefreshOne) => {
            handle_refresh_one(app, event_tx).await;
        }
        Some(KbAction::ToggleStar) => {
            handle_star_toggle_browse(app, event_tx).await;
        }
        Some(KbAction::OpenInBrowser) => {
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
        Some(KbAction::OpenFeedSite) => {
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
        Some(KbAction::EnterSearch) => {
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
        Some(KbAction::ToggleStarredMode) => {
            handle_starred_mode_toggle(app).await?;
        }
        Some(KbAction::MarkFeedRead) => {
            handle_mark_feed_read(app, event_tx).await;
        }
        Some(KbAction::MarkAllRead) => {
            handle_mark_all_read(app, event_tx).await;
        }
        Some(KbAction::ExportOpml) => {
            handle_export_opml(app, event_tx);
        }
        Some(KbAction::CycleTheme) => {
            let name = app.cycle_theme();
            app.set_status(format!("Theme: {}", name));
        }
        Some(KbAction::ShowHelp) => {
            app.show_help = true;
            app.help_scroll_offset = 0;
        }
        _ => {}
    }
    Ok(Action::Continue)
}

/// Handle Enter key press in browse view.
async fn handle_enter_key(app: &mut App, event_tx: &mpsc::Sender<AppEvent>) -> Result<()> {
    if app.focus == Focus::WhatsNew {
        // PERF-023: Fetch full Article from DB for reader entry
        let entry = app.whats_new.get(app.whats_new_selected).or_else(|| {
            // Stale selection index — fallback to first entry
            app.whats_new.first()
        });

        if let Some(entry) = entry {
            let article_id = entry.article_id;
            match app.db.get_article_by_id(article_id).await? {
                Some(article) => {
                    app.view = View::Reader;
                    app.scroll_offset = 0;
                    app.content_state = ContentState::Loading { article_id };
                    app.reader_article = Some(article.clone());
                    app.db.mark_article_read(article_id).await?;

                    try_spawn_content_load(app, &article, event_tx);
                }
                None => {
                    app.set_status("Article no longer exists");
                }
            }
        } else {
            app.set_status("No new articles");
        }
    } else if app.focus == Focus::Feeds {
        // Load articles for selected feed
        if let Some(feed) = app.selected_feed() {
            let feed_id = feed.id;
            // PERF-008: Invalidate cache when feed selection changes
            app.cached_articles = None;
            // BUG-015: Update search_feed_id when switching feeds during search mode
            if app.search_mode {
                app.search_feed_id = Some(feed_id);
            }
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
                let mut refresh_handle = tokio::spawn(async move {
                    refresh_all(db, client, feeds, progress_tx_for_task, Some(rate_limit_tx))
                        .await
                });

                // Forward progress updates with timeout safety net
                // RES-001: Receiver loop terminates when sender drops OR timeout
                // B-2: Also monitor refresh_handle to detect panics immediately
                let mut handle_result = None;
                loop {
                    tokio::select! {
                        Some((done, total)) = progress_rx.recv() => {
                            if let Err(e) = tx.send(AppEvent::RefreshProgress(done, total)).await {
                                tracing::warn!(error = %e, event = "RefreshProgress", "Channel send failed (receiver dropped)");
                            }
                        }
                        result = &mut refresh_handle => {
                            // B-2: Refresh task completed (success or panic) — break immediately
                            if let Err(ref e) = result {
                                tracing::warn!(error = %e, "Refresh task panicked");
                            }
                            handle_result = Some(result);
                            break;
                        }
                        _ = tokio::time::sleep(Duration::from_secs(30)) => {
                            tracing::warn!("Progress receiver timed out after 30s");
                            break;
                        }
                        else => break, // Channel closed (sender dropped)
                    }
                }

                // Get results and send completion
                // B-2: Use cached result if handle completed in select, otherwise await
                let join_result = match handle_result {
                    Some(r) => r,
                    None => refresh_handle.await,
                };
                if let Ok(results) = join_result {
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

        // SAFETY: All Arc::make_mut calls on app.articles must happen on the event loop
        // thread (never in spawned tasks). Background tasks may hold Arc clones for reading
        // but must never mutate. This ensures sequential access without locks.
        // PERF-015: Use Arc::make_mut for copy-on-write optimization
        // Only clones the Vec if there are other references, otherwise mutates in place
        let articles = Arc::make_mut(&mut app.articles);
        if let Some(article) = articles.iter_mut().find(|a| a.id == article_id) {
            article.starred = !current_starred;
        }
        // PERF-023: WhatsNewEntry is display-only; no starred field to update
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

/// Handle mark all articles read for current feed (a key).
async fn handle_mark_feed_read(app: &mut App, event_tx: &mpsc::Sender<AppEvent>) {
    let Some(feed) = app.selected_feed() else {
        return;
    };
    let feed_id = feed.id;

    // Optimistic UI: mark all visible articles as read
    let articles = Arc::make_mut(&mut app.articles);
    for article in articles.iter_mut() {
        if article.feed_id == feed_id {
            article.read = true;
        }
    }
    // PERF-023: WhatsNewEntry is display-only; no read field to update
    app.cached_articles = None;
    app.needs_redraw = true;

    // Spawn background task to persist
    let db = app.db.clone();
    let tx = event_tx.clone();
    tokio::spawn(async move {
        let tx_panic = tx.clone();
        match catch_task_panic(async {
            match db.mark_all_read_for_feed(feed_id).await {
                Ok(count) => {
                    if let Err(e) = tx
                        .send(AppEvent::BulkMarkReadComplete {
                            feed_id: Some(feed_id),
                            count,
                        })
                        .await
                    {
                        tracing::warn!(error = %e, event = "BulkMarkReadComplete", "Channel send failed (receiver dropped)");
                    }
                }
                Err(e) => {
                    tracing::error!(error = %e, feed_id, "Failed to mark feed read");
                    if let Err(e) = tx
                        .send(AppEvent::BulkMarkReadFailed {
                            feed_id: Some(feed_id),
                            error: e.to_string(),
                        })
                        .await
                    {
                        tracing::warn!(error = %e, event = "BulkMarkReadFailed", "Channel send failed (receiver dropped)");
                    }
                }
            }
        })
        .await
        {
            Ok(()) => {}
            Err(panic_msg) => {
                tracing::error!(task = "mark_feed_read", feed_id, error = %panic_msg, "Background task panicked");
                let _ = tx_panic
                    .send(AppEvent::TaskPanicked {
                        task: "mark_feed_read",
                        error: panic_msg,
                    })
                    .await;
            }
        }
    });
}

/// Handle mark all articles read across all feeds (A key).
async fn handle_mark_all_read(app: &mut App, event_tx: &mpsc::Sender<AppEvent>) {
    // Optimistic UI: mark all visible articles as read
    let articles = Arc::make_mut(&mut app.articles);
    for article in articles.iter_mut() {
        article.read = true;
    }
    // PERF-023: WhatsNewEntry is display-only; no read field to update
    app.cached_articles = None;
    app.needs_redraw = true;

    // Spawn background task to persist
    let db = app.db.clone();
    let tx = event_tx.clone();
    tokio::spawn(async move {
        let tx_panic = tx.clone();
        match catch_task_panic(async {
            match db.mark_all_read().await {
                Ok(count) => {
                    if let Err(e) = tx
                        .send(AppEvent::BulkMarkReadComplete {
                            feed_id: None,
                            count,
                        })
                        .await
                    {
                        tracing::warn!(error = %e, event = "BulkMarkReadComplete", "Channel send failed (receiver dropped)");
                    }
                }
                Err(e) => {
                    tracing::error!(error = %e, "Failed to mark all read");
                    if let Err(e) = tx
                        .send(AppEvent::BulkMarkReadFailed {
                            feed_id: None,
                            error: e.to_string(),
                        })
                        .await
                    {
                        tracing::warn!(error = %e, event = "BulkMarkReadFailed", "Channel send failed (receiver dropped)");
                    }
                }
            }
        })
        .await
        {
            Ok(()) => {}
            Err(panic_msg) => {
                tracing::error!(task = "mark_all_read", error = %panic_msg, "Background task panicked");
                let _ = tx_panic
                    .send(AppEvent::TaskPanicked {
                        task: "mark_all_read",
                        error: panic_msg,
                    })
                    .await;
            }
        }
    });
}

/// Handle OPML export (e key in feed panel).
fn handle_export_opml(app: &mut App, event_tx: &mpsc::Sender<AppEvent>) {
    app.set_status("Exporting feeds...");

    let db = app.db.clone();
    let tx = event_tx.clone();
    tokio::spawn(async move {
        let tx_panic = tx.clone();
        match catch_task_panic(async {
            // Get feeds from DB
            let storage_feeds = match db.get_feeds_for_export().await {
                Ok(feeds) => feeds,
                Err(e) => {
                    let _ = tx
                        .send(AppEvent::ExportFailed {
                            error: e.to_string(),
                        })
                        .await;
                    return;
                }
            };

            let count = storage_feeds.len();

            // Convert storage::OpmlFeed to feed::OpmlFeed
            let opml_feeds: Vec<crate::feed::OpmlFeed> = storage_feeds
                .into_iter()
                .map(|f| crate::feed::OpmlFeed {
                    title: f.title,
                    xml_url: f.xml_url,
                    html_url: f.html_url,
                })
                .collect();

            // Determine export path
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
            let config_dir = std::path::PathBuf::from(&home).join(".config/skim");
            let export_path = config_dir.join("feeds-export.opml");

            // S-8: Verify config directory exists and is within expected location
            if let Ok(canonical_config) = config_dir.canonicalize() {
                if !export_path.starts_with(&canonical_config) {
                    let _ = tx
                        .send(AppEvent::ExportFailed {
                            error: "Export path is outside config directory".to_string(),
                        })
                        .await;
                    return;
                }
            }

            match crate::feed::export_to_file(&opml_feeds, &export_path) {
                Ok(()) => {
                    let path_str = export_path.display().to_string();
                    if let Err(e) = tx
                        .send(AppEvent::ExportComplete {
                            count,
                            path: path_str,
                        })
                        .await
                    {
                        tracing::warn!(error = %e, event = "ExportComplete", "Channel send failed (receiver dropped)");
                    }
                }
                Err(e) => {
                    if let Err(e) = tx
                        .send(AppEvent::ExportFailed {
                            error: e.to_string(),
                        })
                        .await
                    {
                        tracing::warn!(error = %e, event = "ExportFailed", "Channel send failed (receiver dropped)");
                    }
                }
            }
        })
        .await
        {
            Ok(()) => {}
            Err(panic_msg) => {
                tracing::error!(task = "export_opml", error = %panic_msg, "Background task panicked");
                let _ = tx_panic
                    .send(AppEvent::TaskPanicked {
                        task: "export_opml",
                        error: panic_msg,
                    })
                    .await;
            }
        }
    });
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
///
/// Uses keybinding registry for action dispatch with Reader context.
pub(super) fn handle_reader_input(
    app: &mut App,
    code: KeyCode,
    modifiers: KeyModifiers,
    event_tx: &mpsc::Sender<AppEvent>,
) -> Result<Action> {
    let action = app
        .keybindings
        .action_for_key(code, modifiers, KbContext::Reader);

    match action {
        Some(KbAction::Quit) => return Ok(Action::Quit),
        Some(KbAction::ExitReader) => {
            app.exit_reader();
        }
        Some(KbAction::ScrollDown) => {
            app.scroll_down(1);
            app.clamp_reader_scroll();
        }
        Some(KbAction::ScrollUp) => app.scroll_up(1),
        Some(KbAction::PageDown) => {
            app.scroll_down(20);
            app.clamp_reader_scroll();
        }
        Some(KbAction::PageUp) => app.scroll_up(20),
        Some(KbAction::ToggleStar) => {
            handle_star_toggle_reader(app, event_tx);
        }
        Some(KbAction::OpenInBrowser) => {
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
            if app.search_input.len() >= MAX_SEARCH_QUERY_LENGTH {
                app.set_status(format!(
                    "Search query at max length ({} chars)",
                    MAX_SEARCH_QUERY_LENGTH
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
