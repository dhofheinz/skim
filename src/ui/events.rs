//! Application event handling.
//!
//! This module processes background task completion events such as
//! refresh progress, content loading, and star toggle results.

#[allow(unused_imports)] // SubscribeState used by TASK-7 subscribe dialog event handling
use crate::app::{App, AppEvent, ContentState, Focus, SubscribeState, View, WhatsNewEntry};
use crate::storage::Article;
use crate::util::strip_control_chars;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

use super::reader::render_markdown;

/// Maximum number of articles in What's New list to prevent memory exhaustion
pub(super) const MAX_WHATS_NEW: usize = 100;

/// Round-robin distribute articles across feeds for variety.
///
/// Takes a list of (feed_id, article) tuples and interleaves them so no single
/// feed dominates the list. Articles within each feed maintain their order
/// (by published date from the query).
///
/// PERF-023: Returns lightweight `WhatsNewEntry` instead of full `Article` clones.
fn round_robin_by_feed(
    articles: Vec<(i64, Article)>,
    feed_title_cache: &HashMap<i64, Arc<str>>,
) -> Vec<WhatsNewEntry> {
    // Group articles by feed, preserving order within each feed
    // PERF-024: Pre-allocate assuming ~5 articles per feed to avoid resizes
    let mut by_feed: HashMap<i64, Vec<Article>> = HashMap::with_capacity(articles.len() / 5);
    for (feed_id, article) in articles {
        by_feed.entry(feed_id).or_default().push(article);
    }

    // Convert to vec of (feed_id, articles) for round-robin iteration
    let mut feeds: Vec<(i64, std::collections::VecDeque<Article>)> = by_feed
        .into_iter()
        .map(|(id, arts)| (id, arts.into_iter().collect()))
        .collect();

    // Round-robin pick from each feed
    let mut result = Vec::with_capacity(MAX_WHATS_NEW);
    while result.len() < MAX_WHATS_NEW && !feeds.is_empty() {
        // Take one article from each feed that still has articles
        feeds.retain_mut(|(feed_id, articles)| {
            if result.len() >= MAX_WHATS_NEW {
                return false; // Stop early if we hit the limit
            }
            if let Some(article) = articles.pop_front() {
                let feed_title = feed_title_cache
                    .get(feed_id)
                    .map(Arc::clone)
                    .unwrap_or_else(|| Arc::from("Unknown"));
                result.push(WhatsNewEntry {
                    article_id: article.id,
                    feed_title,
                    title: Arc::clone(&article.title),
                    published: article.published,
                });
            }
            !articles.is_empty() // Retain only feeds with remaining articles
        });
    }

    result
}

/// Handle application events from background tasks.
///
/// Processes events like refresh completion, content loading results,
/// and star toggle outcomes. Updates application state accordingly.
pub(super) async fn handle_app_event(
    app: &mut App,
    event: AppEvent,
    event_tx: &mpsc::Sender<AppEvent>,
) {
    match event {
        AppEvent::RefreshProgress(done, total) => {
            app.refresh_progress = Some((done, total));
        }
        AppEvent::RefreshComplete(results) => {
            handle_refresh_complete(app, results).await;
            // Reload cached article IDs for cache indicators
            let article_ids: Vec<i64> = app.articles.iter().map(|a| a.id).collect();
            if !article_ids.is_empty() {
                super::helpers::spawn_cached_ids_load(
                    article_ids,
                    app.db.clone(),
                    event_tx.clone(),
                );
            }
        }
        AppEvent::ContentLoaded {
            article_id,
            generation,
            result,
            cached,
        } => {
            handle_content_loaded(app, article_id, generation, result, cached);
        }
        AppEvent::FeedRateLimited {
            feed_title,
            delay_secs,
        } => {
            app.set_status(format!(
                "Rate limited: {} (retrying in {}s)",
                feed_title, delay_secs
            ));
        }
        AppEvent::StarToggled {
            article_id,
            starred,
        } => {
            handle_star_toggled(app, article_id, starred);
        }
        AppEvent::StarToggleFailed {
            article_id,
            original_status,
        } => {
            handle_star_toggle_failed(app, article_id, original_status);
        }
        AppEvent::ContentCacheFailed { article_id, error } => {
            // Only show notification if still viewing this article
            if app.reader_article.as_ref().map(|a| a.id) == Some(article_id) {
                app.set_status("Note: Content won't be cached (disk issue)");
            }
            tracing::debug!(article_id, error, "Content cache failed, user notified");
        }
        AppEvent::TaskPanicked { task, error } => {
            tracing::error!(task, error, "Background task panicked");
            app.set_status(format!("Internal error in {} task", task));
        }
        AppEvent::SearchCompleted {
            query,
            generation,
            results,
        } => {
            handle_search_completed(app, query, generation, results);
        }
        AppEvent::BulkMarkReadComplete { feed_id, count } => {
            handle_bulk_mark_read_complete(app, feed_id, count);
        }
        AppEvent::BulkMarkReadFailed { feed_id, error } => {
            handle_bulk_mark_read_failed(app, feed_id, error);
        }
        AppEvent::ExportComplete { count, path } => {
            tracing::info!(count, path = %path, "OPML export complete");
            app.set_status(format!("Exported {} feeds to {}", count, path));
            app.needs_redraw = true;
        }
        AppEvent::ExportFailed { error } => {
            tracing::error!(error = %error, "OPML export failed");
            app.set_status(format!("Export failed: {}", error));
            app.needs_redraw = true;
        }
        AppEvent::FeedDeleted {
            feed_id,
            title,
            articles_removed,
        } => {
            handle_feed_deleted(app, feed_id, &title, articles_removed);
        }
        AppEvent::FeedDeleteFailed { feed_id, error } => {
            tracing::error!(feed_id, error = %error, "Feed deletion failed");
            app.set_status(format!("Delete failed: {}", error));
            app.needs_redraw = true;
        }
        AppEvent::FeedDiscovered { url, result } => {
            handle_feed_discovered(app, url, result);
        }
        AppEvent::FeedSubscribed { title } => {
            handle_feed_subscribed(app, title).await;
        }
        AppEvent::FeedSubscribeFailed { error } => {
            tracing::error!(error = %error, "Feed subscription failed");
            app.set_status(format!("Subscribe failed: {}", error));
            app.needs_redraw = true;
        }
        AppEvent::FeedRenamed { feed_id, new_title } => {
            tracing::info!(feed_id, new_title = %new_title, "Feed renamed");
            let feeds = Arc::make_mut(&mut app.feeds);
            if let Some(feed) = feeds.iter_mut().find(|f| f.id == feed_id) {
                feed.title = Arc::from(new_title.as_str());
            }
            app.sync_feed_cache();
            app.invalidate_category_tree(); // PERF-021: Feed title changed
            app.set_status(format!("Renamed to '{}'", new_title));
            app.needs_redraw = true;
        }
        AppEvent::FeedRenameFailed { feed_id, error } => {
            tracing::error!(feed_id, error = %error, "Feed rename failed");
            app.set_status(format!("Rename failed: {}", error));
            app.needs_redraw = true;
        }
        AppEvent::FeedMoved {
            feed_id,
            category_id,
            category_name,
        } => {
            tracing::info!(feed_id, category_id = ?category_id, "Feed moved to category");
            let feeds = Arc::make_mut(&mut app.feeds);
            if let Some(feed) = feeds.iter_mut().find(|f| f.id == feed_id) {
                feed.category_id = category_id;
            }
            app.invalidate_category_tree(); // PERF-021: Feed category membership changed
            app.set_status(format!("Moved to '{}'", category_name));
            app.needs_redraw = true;
        }
        AppEvent::FeedMoveFailed { feed_id, error } => {
            tracing::error!(feed_id, error = %error, "Feed move failed");
            app.set_status(format!("Move failed: {}", error));
            app.needs_redraw = true;
        }
        AppEvent::ReadingSessionOpened { history_id } => {
            if let Some(session) = &mut app.reading_session {
                session.history_id = history_id;
                tracing::debug!(history_id, "Reading session confirmed by DB");
            }
        }
        AppEvent::StatsLoaded(data) => {
            if app.view == View::Stats {
                app.stats_data = Some(data);
                app.needs_redraw = true;
            }
        }
        AppEvent::PrefetchProgress { completed, total } => {
            app.prefetch_progress = Some((completed, total));
            app.set_status(format!("Prefetching: {}/{}...", completed + 1, total));
            app.needs_redraw = true;
        }
        AppEvent::PrefetchComplete { succeeded, failed } => {
            app.prefetch_progress = None;
            if failed > 0 {
                app.set_status(format!(
                    "Prefetch done: {} cached, {} failed",
                    succeeded, failed
                ));
            } else if succeeded > 0 {
                app.set_status(format!("Prefetch done: {} articles cached", succeeded));
            } else {
                app.set_status("All articles already cached");
            }
            // Reload cached IDs for the current article list
            let article_ids: Vec<i64> = app.articles.iter().map(|a| a.id).collect();
            if !article_ids.is_empty() {
                super::helpers::spawn_cached_ids_load(
                    article_ids,
                    app.db.clone(),
                    event_tx.clone(),
                );
            }
            app.needs_redraw = true;
        }
        AppEvent::CachedIdsLoaded(ids) => {
            app.cached_article_set = ids;
            app.needs_redraw = true;
        }
    }
}

/// Handle search completed event.
fn handle_search_completed(
    app: &mut App,
    query: String,
    generation: u64,
    results: Result<Vec<Article>, String>,
) {
    // Check generation to prevent stale search results
    if generation != app.search_generation {
        tracing::debug!(
            expected = app.search_generation,
            got = generation,
            query = %query,
            "Ignoring stale search result (generation mismatch)"
        );
        return;
    }

    match results {
        Ok(articles) => {
            let count = articles.len();
            app.articles = Arc::new(articles);
            app.selected_article = 0;
            app.clamp_selections();
            tracing::debug!(query = %query, count, "Search completed");
        }
        Err(e) => {
            tracing::warn!(query = %query, error = %e, "Search failed");
            app.set_status(format!("Search failed: {}", e));
        }
    }
}

/// Handle refresh completion event.
async fn handle_refresh_complete(app: &mut App, results: Vec<crate::app::FetchResult>) {
    app.refresh_progress = None;

    // BUG-025: Filter out results for feeds deleted during refresh.
    // This IS reachable: feeds are Arc-cloned before refresh starts, but the user
    // can delete a feed from the UI while the refresh task is in-flight. When that
    // happens, the refresh returns results for a feed_id no longer in app.feeds.
    // Without this filter, we'd attempt to display orphaned articles.
    use std::collections::HashSet;
    let current_feed_ids: HashSet<i64> = app.feeds.iter().map(|f| f.id).collect();
    let results: Vec<_> = results
        .into_iter()
        .filter(|r| {
            if current_feed_ids.contains(&r.feed_id) {
                true
            } else {
                tracing::debug!(feed_id = r.feed_id, "Skipping result for deleted feed");
                false
            }
        })
        .collect();

    // PERF-018: Single-pass iteration to collect all metrics at once
    // Previously iterated 4 times: total, failed, total_new, network_errors
    // Network error patterns (lowercase for case-insensitive matching)
    const NETWORK_PATTERNS: &[&str] = &[
        "request failed",
        "timed out",
        "connection",
        "dns",
        "network",
    ];

    struct RefreshStats {
        total: usize,
        failed: usize,
        total_new: usize,
        network_errors: usize,
    }

    let stats = results.iter().fold(
        RefreshStats {
            total: 0,
            failed: 0,
            total_new: 0,
            network_errors: 0,
        },
        |mut acc, r| {
            acc.total += 1;
            acc.total_new += r.new_articles;
            if let Some(ref error) = r.error {
                acc.failed += 1;
                // PERF-012: Only lowercase once per error string
                let e_lower = error.to_lowercase();
                if NETWORK_PATTERNS.iter().any(|pat| e_lower.contains(pat)) {
                    acc.network_errors += 1;
                }
            }
            acc
        },
    );

    let RefreshStats {
        total,
        failed,
        total_new,
        network_errors,
    } = stats;

    // If >80% of feeds fail with network errors, likely offline
    let is_offline = total > 0 && failed > 0 && (network_errors as f64 / total as f64) > 0.8;

    if is_offline {
        app.set_status("Offline - Network unavailable. Check your connection.");
    } else if failed > 0 {
        app.set_status(format!(
            "Refresh complete. {} new articles, {} feeds failed.",
            total_new, failed
        ));
    } else {
        app.set_status(format!("Refresh complete. {} new articles.", total_new));
    }

    // Reload feeds with updated counts
    if let Ok(feeds) = app.db.get_feeds_with_unread_counts().await {
        app.feeds = Arc::new(feeds);
        // BUG-004: Clamp immediately after feed list mutation to prevent out-of-bounds access
        app.clamp_selections();

        // BUG-011: Sync feed cache to remove stale entries from deleted feeds
        // This also updates all current feed entries
        app.sync_feed_cache();

        app.invalidate_category_tree(); // PERF-021: Feeds replaced with new unread counts

        // DATA-001: Reload current feed's articles to ensure in-memory matches DB
        // This fixes data divergence if user modified articles during refresh
        if let Some(feed) = app.feeds.get(app.selected_feed) {
            let feed_id = feed.id;
            let current_selected = app.selected_article;

            match app.db.get_articles_for_feed(feed_id, None).await {
                Ok(articles) => {
                    app.articles = Arc::new(articles);
                    // Preserve selection if still valid
                    app.selected_article = if app.articles.is_empty() {
                        0
                    } else {
                        current_selected.min(app.articles.len().saturating_sub(1))
                    };
                    tracing::debug!(
                        feed_id,
                        count = app.articles.len(),
                        "Reloaded articles after refresh"
                    );
                }
                Err(e) => {
                    tracing::warn!(error = %e, feed_id, "Failed to reload articles after refresh");
                }
            }
        }

        // Ensure all selections are valid
        app.clamp_selections();

        // PERF-001 + PERF-007: Use batched query and HashMap for feed lookup
        if total_new > 0 {
            let feed_ids: Vec<i64> = results
                .iter()
                .filter(|r| r.new_articles > 0)
                .map(|r| r.feed_id)
                .collect();

            if !feed_ids.is_empty() {
                // PERF-001: Single batched query instead of N queries
                // Fetch larger pool for round-robin distribution across feeds
                let pool_size = (feed_ids.len() * 10).max(100);
                if let Ok(recent) = app
                    .db
                    .get_recent_articles_for_feeds(&feed_ids, pool_size)
                    .await
                {
                    // Round-robin across feeds for variety in What's New
                    let new_articles = round_robin_by_feed(recent, &app.feed_title_cache);

                    if !new_articles.is_empty() {
                        // MEM-001: Bound whats_new list to prevent memory exhaustion
                        let total_new = new_articles.len();
                        app.whats_new = if total_new > MAX_WHATS_NEW {
                            tracing::info!(
                                total = total_new,
                                shown = MAX_WHATS_NEW,
                                "Truncated What's New list"
                            );
                            new_articles.into_iter().take(MAX_WHATS_NEW).collect()
                        } else {
                            new_articles
                        };
                        app.whats_new_selected = 0;
                        app.show_whats_new = true;

                        // BUG-006: Only steal focus if user has been idle for 2+ seconds
                        // This prevents interrupting active article selection
                        let idle_duration = Duration::from_secs(2);
                        let user_is_idle = app.last_input_time.elapsed() > idle_duration;

                        if app.view != View::Reader && user_is_idle {
                            app.focus = Focus::WhatsNew;
                            tracing::debug!(
                                idle_secs = app.last_input_time.elapsed().as_secs_f32(),
                                "Stealing focus to What's New (user idle)"
                            );
                        } else {
                            tracing::debug!(
                                view = ?app.view,
                                idle_secs = app.last_input_time.elapsed().as_secs_f32(),
                                "Not stealing focus to What's New (user active or in Reader)"
                            );
                        }
                    }
                }
            }
        }
    }
}

/// Handle content loaded event.
fn handle_content_loaded(
    app: &mut App,
    article_id: i64,
    generation: u64,
    result: Result<String, crate::content::ContentError>,
    cached: bool,
) {
    // BUG-010: Check generation first to prevent stale content race conditions.
    // If user rapidly navigates A->B->A, we may receive content from the first A
    // request after switching to B. Generation counter ensures we only accept
    // content from the most recent request.
    if generation != app.content_load_generation {
        tracing::debug!(
            expected = app.content_load_generation,
            got = generation,
            article_id,
            "Ignoring stale content load (generation mismatch)"
        );
        return;
    }

    // Only update if still viewing the same article
    if let Some(ref reader_article) = app.reader_article {
        if reader_article.id == article_id {
            // This is the content we wanted - apply it and clear tracking
            let source = if cached { "cached" } else { "fetched" };
            tracing::debug!(article_id, generation, source, "Content loaded");
            app.content_loading_for = None;

            match result {
                Ok(content) => {
                    // SEC-001: Sanitize content from jina.ai before rendering
                    let content = strip_control_chars(&content).into_owned();
                    // PERF-004: Parse markdown once and cache rendered lines
                    let rendered_lines = render_markdown(&content, &app.theme);
                    app.content_state = ContentState::Loaded {
                        article_id,
                        content,
                        rendered_lines,
                    };
                    // PERF-022: Clear negative cache on success
                    app.failed_content_cache.remove(&article_id);
                    // Update cache indicator
                    app.cached_article_set.insert(article_id);
                }
                Err(e) => {
                    let fallback = reader_article.summary.clone();
                    app.content_state = ContentState::Failed {
                        article_id,
                        error: e.to_string(),
                        fallback,
                    };
                    // PERF-022: Insert into negative cache on failure
                    app.failed_content_cache
                        .insert(article_id, tokio::time::Instant::now());
                }
            }
            // PERF-020: Invalidate cached line count when content state changes
            app.reader_cached_line_count = None;
        } else {
            // BUG-001 fix: Clear loading flag for stale content
            // The generation counter handles race conditions, so we can safely
            // clear the flag here. This ensures new content loads can start.
            tracing::debug!(
                expected = ?app.reader_article.as_ref().map(|a| a.id),
                received = article_id,
                "Stale content arrived, clearing loading flag"
            );
            app.content_loading_for = None;
        }
    } else {
        // Not in reader view - discard content silently
        // Must still clear loading flag to prevent blocking future loads
        tracing::debug!(article_id, "Discarding content - not in reader view");
        app.content_loading_for = None;
    }
}

/// Handle star toggle success event.
fn handle_star_toggled(app: &mut App, article_id: i64, starred: bool) {
    // Only update reader_article if still viewing this specific article in Reader view
    if app.view == View::Reader {
        if let Some(ref mut article) = app.reader_article {
            if article.id == article_id {
                article.starred = starred;
            }
        }
    }
    // SAFETY: All Arc::make_mut calls on app.articles must happen on the event loop
    // thread (never in spawned tasks). Background tasks may hold Arc clones for reading
    // but must never mutate. This ensures sequential access without locks.
    // PERF-015: Use Arc::make_mut for copy-on-write optimization
    // Only clones the Vec if there are other references, otherwise mutates in place
    let articles = Arc::make_mut(&mut app.articles);
    if let Some(article) = articles.iter_mut().find(|a| a.id == article_id) {
        article.starred = starred;
    }
    // PERF-023: WhatsNewEntry is display-only; no starred/read fields to update
    // Invalidate cache if it exists (defense in depth)
    if app.cached_articles.is_some() {
        tracing::debug!(
            article_id,
            "Invalidating article cache in StarToggled event"
        );
        app.cached_articles = None;
    }
    app.needs_redraw = true;
}

/// Handle star toggle failure event with rollback.
fn handle_star_toggle_failed(app: &mut App, article_id: i64, original_status: bool) {
    tracing::warn!(
        article_id,
        original_status,
        "Star toggle failed, rolling back"
    );

    let mut found = false;

    // PERF-015: Use Arc::make_mut for copy-on-write optimization
    // Only clones the Vec if there are other references, otherwise mutates in place
    let articles = Arc::make_mut(&mut app.articles);
    if let Some(article) = articles.iter_mut().find(|a| a.id == article_id) {
        article.starred = original_status;
        found = true;
    }

    // PERF-023: WhatsNewEntry is display-only; no starred field to rollback

    // Also rollback in reader_article if viewing
    if let Some(ref mut reader_article) = app.reader_article {
        if reader_article.id == article_id {
            reader_article.starred = original_status;
            found = true;
        }
    }

    // Invalidate cache if it exists (defense in depth)
    if app.cached_articles.is_some() {
        tracing::debug!(
            article_id,
            "Invalidating article cache in StarToggleFailed event"
        );
        app.cached_articles = None;
    }

    // User feedback
    if found {
        app.set_status("Failed to save star status - reverted");
    } else {
        app.set_status("Failed to save star status");
    }

    app.needs_redraw = true;
}

/// Handle bulk mark-read completion event.
///
/// Updates feed unread counts without reloading articles from DB.
/// The optimistic UI update on articles happens in the input handler (TASK-5).
fn handle_bulk_mark_read_complete(app: &mut App, feed_id: Option<i64>, count: u64) {
    match feed_id {
        Some(fid) => {
            // Decrement the specific feed's unread_count
            let feeds = Arc::make_mut(&mut app.feeds);
            if let Some(feed) = feeds.iter_mut().find(|f| f.id == fid) {
                feed.unread_count = feed.unread_count.saturating_sub(count as i64);
            }
            tracing::info!(feed_id = fid, count, "Marked articles read for feed");
        }
        None => {
            // All feeds: set every feed's unread_count to 0
            let feeds = Arc::make_mut(&mut app.feeds);
            for feed in feeds.iter_mut() {
                feed.unread_count = 0;
            }
            tracing::info!(count, "Marked all articles read across all feeds");
        }
    }
    app.clamp_selections();
    app.invalidate_category_tree(); // PERF-021: Unread counts changed
    app.needs_redraw = true;
    app.set_status(format!("Marked {} articles read", count));
}

/// Handle bulk mark-read failure event.
fn handle_bulk_mark_read_failed(app: &mut App, feed_id: Option<i64>, error: String) {
    tracing::error!(feed_id = ?feed_id, error = %error, "Bulk mark-read failed");
    app.set_status(format!("Failed to mark articles read: {}", error));
    app.needs_redraw = true;
}

/// Handle feed deletion completion event.
///
/// Removes the feed from in-memory state, cleans up articles belonging to
/// the deleted feed, invalidates caches, and clamps selections.
fn handle_feed_deleted(app: &mut App, feed_id: i64, title: &str, articles_removed: usize) {
    tracing::info!(feed_id, title = %title, articles_removed, "Feed deleted");

    // Remove feed from in-memory list
    let feeds = Arc::make_mut(&mut app.feeds);
    feeds.retain(|f| f.id != feed_id);

    // Remove articles belonging to the deleted feed
    let articles = Arc::make_mut(&mut app.articles);
    articles.retain(|a| a.feed_id != feed_id);

    // Remove from feed title cache
    app.feed_title_cache.remove(&feed_id);

    // Invalidate article cache (may contain stale references)
    app.cached_articles = None;

    // Clear pending confirm if it was for this feed (edge case: confirm still pending)
    if let Some(crate::app::ConfirmAction::DeleteFeed {
        feed_id: pending_id,
        ..
    }) = &app.pending_confirm
    {
        if *pending_id == feed_id {
            app.pending_confirm = None;
        }
    }

    // BUG-004: Clamp immediately after list mutation to prevent out-of-bounds access
    app.clamp_selections();
    app.invalidate_category_tree(); // PERF-021: Feed removed

    app.set_status(format!(
        "Deleted '{}' ({} articles removed)",
        title, articles_removed
    ));
    app.needs_redraw = true;
}

/// Handle feed discovery result from subscribe dialog.
fn handle_feed_discovered(
    app: &mut App,
    url: String,
    result: Result<crate::feed::DiscoveredFeed, String>,
) {
    // Only process if still in Discovering state for this URL
    let is_discovering = matches!(
        &app.subscribe_state,
        Some(SubscribeState::Discovering { url: u }) if *u == url
    );
    if !is_discovering {
        tracing::debug!(url = %url, "Ignoring discovery result (dialog cancelled or state changed)");
        return;
    }

    match result {
        Ok(feed) => {
            // Check if already subscribed
            if app.feeds.iter().any(|f| f.url == feed.feed_url) {
                app.subscribe_state = None;
                app.set_status(format!("Already subscribed to {}", feed.title));
            } else {
                app.subscribe_state = Some(SubscribeState::Preview { feed });
            }
        }
        Err(e) => {
            // Return to URL input so user can fix and retry
            app.subscribe_state = Some(SubscribeState::InputUrl { input: url });
            app.set_status(format!("Discovery failed: {}", e));
        }
    }
    app.needs_redraw = true;
}

/// Handle feed subscription completion.
///
/// Reloads feeds from DB and triggers a single-feed refresh to populate articles.
async fn handle_feed_subscribed(app: &mut App, title: String) {
    tracing::info!(title = %title, "Feed subscribed");
    app.set_status(format!("Subscribed to {}", title));

    // Reload feeds from DB to include the new feed
    if let Ok(feeds) = app.db.get_feeds_with_unread_counts().await {
        app.feeds = Arc::new(feeds);
        app.rebuild_feed_cache();
        app.clamp_selections();
        app.invalidate_category_tree(); // PERF-021: New feed added
    }

    app.needs_redraw = true;
}
