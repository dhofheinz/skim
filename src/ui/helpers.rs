//! Helper functions for UI operations.
//!
//! This module contains utility functions shared across the UI layer,
//! including mode transitions, content loading, and URL validation.

use crate::app::{App, AppEvent, ContentState};
use crate::content::fetch_content;
use crate::storage::{Article, Database};
use anyhow::Result;
use futures::FutureExt;
use std::collections::HashSet;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

/// Error message for articles without URLs
pub(super) const ERR_ARTICLE_NO_URL: &str = "Article has no URL";

/// Wraps a future to catch panics and convert them to errors.
///
/// This function enables graceful handling of panics in spawned background tasks.
/// Instead of the task silently disappearing (caught by Tokio's runtime but not
/// handled), panics are converted to `Err(String)` containing the panic message.
///
/// # Returns
///
/// - `Ok(result)` if the future completes normally
/// - `Err(panic_message)` if the future panics
///
/// # Example
///
/// ```ignore
/// tokio::spawn(async move {
///     match catch_task_panic(async { do_work().await }).await {
///         Ok(result) => handle_result(result),
///         Err(panic_msg) => {
///             tracing::error!(error = %panic_msg, "Task panicked");
///             let _ = tx.send(AppEvent::TaskPanicked { task: "work", error: panic_msg }).await;
///         }
///     }
/// });
/// ```
pub(super) async fn catch_task_panic<F, T>(future: F) -> Result<T, String>
where
    F: std::future::Future<Output = T>,
{
    AssertUnwindSafe(future)
        .catch_unwind()
        .await
        .map_err(|panic| {
            if let Some(s) = panic.downcast_ref::<&'static str>() {
                s.to_string()
            } else if let Some(s) = panic.downcast_ref::<String>() {
                s.clone()
            } else if let Some(e) = panic.downcast_ref::<Box<dyn std::error::Error + Send>>() {
                e.to_string()
            } else {
                format!("Unknown panic: {:?}", (*panic).type_id())
            }
        })
}

/// Exit starred mode and restore previous feed articles.
///
/// Attempts to restore articles from cache if valid; otherwise reloads from database.
/// Calls `clamp_selections()` to ensure selection indices remain valid.
///
/// # Arguments
///
/// * `app` - Mutable application state
/// * `log_context` - Context string for logging (e.g., "ESC", "S toggle")
///
/// # Returns
///
/// - `Ok(true)` if starred mode was exited
/// - `Ok(false)` if not in starred mode (no-op)
/// - `Err` if database query fails
pub(super) async fn exit_starred_mode(app: &mut App, log_context: &str) -> Result<bool> {
    if !app.starred_mode {
        return Ok(false);
    }

    app.starred_mode = false;
    // PERF-014: Clear prefix cache when exiting starred mode
    app.feed_prefix_cache.clear();
    tracing::info!("Exiting starred mode via {}", log_context);

    // PERF-008: Try to restore from cache
    let current_feed_id = app.selected_feed().map(|f| f.id);
    if let Some(cached) = app.cached_articles.take() {
        if cached.feed_id == current_feed_id {
            // Cache is valid, restore without DB query
            tracing::debug!("Restoring articles from cache ({})", log_context);
            app.articles = cached.articles;
            app.selected_article = if app.articles.is_empty() {
                0 // Will be handled by empty-list rendering
            } else {
                let max_idx = app.articles.len().saturating_sub(1);
                cached.selected.min(max_idx)
            };
            app.clamp_selections();
            debug_assert!(app.articles.is_empty() || app.selected_article < app.articles.len());
        } else {
            // Cache stale (feed changed), fall back to DB query
            tracing::debug!("Cache stale, querying DB ({})", log_context);
            if let Some(feed_id) = current_feed_id {
                match app.db.get_articles_for_feed(feed_id, None).await {
                    Ok(articles) => {
                        app.articles = Arc::new(articles);
                        app.selected_article = 0;
                        app.clamp_selections();
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "Failed to reload articles");
                        return Err(e);
                    }
                }
            } else {
                app.articles = Arc::new(Vec::new());
                app.selected_article = 0;
                app.clamp_selections();
            }
        }
    } else {
        // No cache, fall back to DB query
        if let Some(feed_id) = current_feed_id {
            match app.db.get_articles_for_feed(feed_id, None).await {
                Ok(articles) => {
                    app.articles = Arc::new(articles);
                    app.selected_article = 0;
                    app.clamp_selections();
                }
                Err(e) => {
                    tracing::error!(error = %e, "Failed to reload articles");
                    return Err(e);
                }
            }
        } else {
            app.articles = Arc::new(Vec::new());
            app.selected_article = 0;
            app.clamp_selections();
        }
    }

    Ok(true)
}

/// Restore feed articles after exiting search mode.
///
/// Uses cached articles if available and valid (matching feed ID and search feed ID),
/// otherwise reloads from DB. Preserves selection when possible.
///
/// # Arguments
///
/// * `app` - Mutable application state
/// * `use_search_feed_id` - If true, uses `app.search_feed_id` for DB fallback and cache validation (ESC case).
///   If false, uses current selected feed ID (Enter case).
///
/// # Cache Validation
///
/// Cache is considered valid when:
/// - `cached_articles` exists
/// - `cached.feed_id` matches current selected feed ID
/// - For ESC case: `cached.feed_id` also matches `search_feed_id`
pub(super) async fn restore_articles_from_search(
    app: &mut App,
    use_search_feed_id: bool,
) -> Result<()> {
    let current_feed_id = app.selected_feed().map(|f| f.id);

    if let Some(cached) = app.cached_articles.take() {
        // For ESC: validate cache matches both current feed and search_feed_id
        // For Enter: only validate cache matches current feed
        let cache_valid = if use_search_feed_id {
            cached.feed_id == current_feed_id && cached.feed_id == app.search_feed_id
        } else {
            cached.feed_id == current_feed_id
        };

        if cache_valid {
            // Cache is valid, restore without DB query
            tracing::debug!(feed_id = ?cached.feed_id, "Restoring articles from search cache");
            app.articles = cached.articles;
            app.selected_article = cached.selected.min(app.articles.len().saturating_sub(1));
            // Handle empty list case
            if app.articles.is_empty() {
                app.selected_article = 0;
            }
            app.clamp_selections();
            debug_assert!(app.articles.is_empty() || app.selected_article < app.articles.len());
            return Ok(());
        }
        tracing::debug!("Search cache stale (feed changed), reloading from DB");
    }

    // No valid cache, reload from DB
    // Use search_feed_id for ESC case, current feed for Enter case
    let feed_id_for_query = if use_search_feed_id {
        app.search_feed_id.take()
    } else {
        current_feed_id
    };

    if let Some(feed_id) = feed_id_for_query {
        let articles = app.db.get_articles_for_feed(feed_id, None).await?;
        app.articles = Arc::new(articles);
        app.selected_article = 0;
        tracing::debug!(feed_id, "Reloaded articles from DB after search");
    } else {
        app.articles = Arc::new(Vec::new());
        app.selected_article = 0;
    }

    app.clamp_selections();
    Ok(())
}

/// Exit search mode completely, restoring previous article state.
///
/// Clears all search-related state and restores articles from cache or DB.
///
/// # Arguments
///
/// * `app` - Mutable application state
pub(super) async fn exit_search_mode(app: &mut App) -> Result<()> {
    app.search_mode = false;
    app.search_input.clear();
    app.pending_search = None;
    app.search_debounce = None;

    restore_articles_from_search(app, true).await?;
    app.search_feed_id = None;
    Ok(())
}

/// Attempt to spawn content load for an article.
///
/// Handles all pre-spawn checks and state updates:
/// - Guards against duplicate loading (content_loading_for already set)
/// - Clears old loading state when switching articles
/// - Handles no-URL case by setting Failed state with summary fallback
/// - Aborts previous load handle before spawning new one
/// - Increments generation counter for race condition handling
///
/// # Arguments
///
/// * `app` - Mutable application state
/// * `article` - The article to load content for
/// * `event_tx` - Channel to send completion events
///
/// # Returns
///
/// Returns `true` if a content load was spawned, `false` if skipped (already loading
/// or article has no URL).
/// TTL for negative content cache entries (5 minutes).
const FAILED_CONTENT_TTL: Duration = Duration::from_secs(300);

pub(super) fn try_spawn_content_load(
    app: &mut App,
    article: &Article,
    event_tx: &mpsc::Sender<AppEvent>,
) -> bool {
    let article_id = article.id;

    // PERF-022: Check negative cache for recent failures
    if let Some(failed_at) = app.failed_content_cache.get(&article_id) {
        if failed_at.elapsed() < FAILED_CONTENT_TTL {
            let remaining = FAILED_CONTENT_TTL - failed_at.elapsed();
            let mins = remaining.as_secs() / 60;
            let secs = remaining.as_secs() % 60;
            app.content_state = ContentState::Failed {
                article_id,
                error: format!("Content unavailable (retry in {}m {}s)", mins, secs),
                fallback: article.summary.clone(),
            };
            tracing::debug!(article_id, mins, secs, "Negative cache hit, skipping fetch");
            return false;
        }
        // TTL expired, remove from cache and allow retry
        app.failed_content_cache.remove(&article_id);
    }

    // Check if already loading content for a different article
    if app.content_loading_for.is_some() && app.content_loading_for != Some(article_id) {
        tracing::debug!(
            old = ?app.content_loading_for,
            new = article_id,
            "Clearing old content load, starting new"
        );
        app.content_loading_for = None;
    }

    // Guard: skip if actively loading for this article
    if app.content_loading_for == Some(article_id) {
        if matches!(app.content_state, ContentState::Loading { .. }) {
            tracing::debug!(article_id, "Content load already in progress, skipping");
            return false;
        }
        // BUG-016: Stale content_loading_for from previous reader session — clear and proceed
        tracing::debug!(article_id, "Clearing stale content_loading_for flag");
        app.content_loading_for = None;
    }

    // Handle no-URL case with fallback to summary
    let Some(url) = article.url.clone() else {
        app.content_state = ContentState::Failed {
            article_id,
            error: ERR_ARTICLE_NO_URL.to_string(),
            fallback: article.summary.clone(),
        };
        return false;
    };

    // Abort any previous content load task
    if let Some(handle) = app.content_load_handle.take() {
        handle.abort();
        tracing::debug!("Aborted previous content load task");
    }

    // Set loading state and increment generation
    tracing::debug!(article_id, "Starting content load");
    app.content_loading_for = Some(article_id);
    app.content_load_generation += 1;
    let generation = app.content_load_generation;

    // Spawn the content load task
    app.content_load_handle = Some(spawn_content_load(
        article_id,
        generation,
        url,
        app.http_client.clone(),
        app.db.clone(),
        event_tx.clone(),
    ));

    true
}

/// Spawn a background task to load article content with cache-aware flow.
///
/// Checks content_cache table first (TTL-aware), then articles.content column,
/// then fetches from jina.ai on miss. Caches fetched content and indexes for FTS5.
/// Sends `AppEvent::ContentLoaded` on completion (success or failure).
///
/// # Arguments
///
/// * `article_id` - The article to load content for
/// * `generation` - The generation counter at spawn time (for race condition handling)
/// * `url` - The article URL to fetch
/// * `client` - HTTP client for fetching
/// * `db` - Database for caching
/// * `tx` - Channel to send completion event
///
/// # Returns
///
/// Returns the JoinHandle for the spawned task, allowing the caller to abort
/// the task if a new content load is started or the reader view is exited.
///
/// PERF-010: Takes Arc<str> for cheap reference counting instead of String clone.
pub(super) fn spawn_content_load(
    article_id: i64,
    generation: u64,
    url: Arc<str>,
    client: reqwest::Client,
    db: Database,
    tx: mpsc::Sender<AppEvent>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let tx_panic = tx.clone();
        match catch_task_panic(async {
            // 1. Check content_cache table (TTL-aware)
            if let Ok(Some(cached)) = db.get_cached_content(article_id).await {
                tracing::debug!(article_id, generation, "content_cache hit");
                if let Err(e) = tx
                    .send(AppEvent::ContentLoaded {
                        article_id,
                        generation,
                        result: Ok(cached.markdown),
                        cached: true,
                    })
                    .await
                {
                    tracing::warn!(error = %e, event = "ContentLoaded", "Channel send failed (receiver dropped)");
                }
                return;
            }

            // 2. Check articles.content column (legacy fallback)
            if let Ok(Some(content)) = db.get_article_content(article_id).await {
                tracing::debug!(article_id, generation, "articles.content hit, migrating to cache");
                // Migrate legacy content into content_cache for future TTL-aware lookups
                if let Err(e) = db.cache_content(article_id, &content, None).await {
                    tracing::warn!(article_id, error = %e, "Failed to migrate content to cache");
                }
                if let Err(e) = tx
                    .send(AppEvent::ContentLoaded {
                        article_id,
                        generation,
                        result: Ok(content),
                        cached: true,
                    })
                    .await
                {
                    tracing::warn!(error = %e, event = "ContentLoaded", "Channel send failed (receiver dropped)");
                }
                return;
            }

            // 3. Cache miss — fetch from jina.ai
            let result = fetch_content(&client, &url, None).await;

            // On success: cache content, update articles.content, index for FTS5
            if let Ok(ref markdown) = result {
                if let Err(e) = db.cache_content(article_id, markdown, None).await {
                    tracing::warn!(article_id, error = %e, "Failed to cache content");
                    let _ = tx
                        .send(AppEvent::ContentCacheFailed {
                            article_id,
                            error: e.to_string(),
                        })
                        .await;
                }
                // Index content for FTS5 full-text search (TASK-3)
                // Also writes articles.content via UPDATE trigger
                if let Err(e) = db.index_content(article_id, markdown).await {
                    tracing::warn!(article_id, error = %e, "Failed to index content for FTS5");
                }
            }

            if let Err(e) = tx
                .send(AppEvent::ContentLoaded {
                    article_id,
                    generation,
                    result,
                    cached: false,
                })
                .await
            {
                tracing::warn!(error = %e, event = "ContentLoaded", "Channel send failed (receiver dropped)");
            }
        })
        .await
        {
            Ok(()) => {}
            Err(panic_msg) => {
                tracing::error!(task = "content_load", article_id, error = %panic_msg, "Background task panicked");
                let _ = tx_panic
                    .send(AppEvent::TaskPanicked {
                        task: "content_load",
                        error: panic_msg,
                    })
                    .await;
            }
        }
    })
}

/// Spawn a background task to load cached article IDs for the current article list.
///
/// Sends `AppEvent::CachedIdsLoaded` with the set of article IDs that have valid
/// content_cache entries. Used to populate `app.cached_article_set` for UI indicators.
pub(super) fn spawn_cached_ids_load(
    article_ids: Vec<i64>,
    db: Database,
    tx: mpsc::Sender<AppEvent>,
) {
    tokio::spawn(async move {
        match db.cached_article_ids(&article_ids).await {
            Ok(ids) => {
                let set: HashSet<i64> = ids.into_iter().collect();
                let _ = tx.send(AppEvent::CachedIdsLoaded(set)).await;
            }
            Err(e) => {
                tracing::warn!(error = %e, "Failed to load cached article IDs");
            }
        }
    });
}

/// Spawn a background prefetch task for a feed's unread articles.
///
/// Fetches up to `limit` unread articles without cache entries, caching each one.
/// Sends `PrefetchProgress` and `PrefetchComplete` events.
pub(super) fn spawn_prefetch(
    feed_id: i64,
    db: Database,
    client: reqwest::Client,
    tx: mpsc::Sender<AppEvent>,
) {
    const PREFETCH_LIMIT: i64 = 50;

    tokio::spawn(async move {
        let tx_panic = tx.clone();
        match catch_task_panic(async {
            let candidates = match db
                .prefetch_candidates_for_feed(feed_id, PREFETCH_LIMIT)
                .await
            {
                Ok(c) => c,
                Err(e) => {
                    tracing::error!(error = %e, "Failed to get prefetch candidates");
                    let _ = tx
                        .send(AppEvent::PrefetchComplete {
                            succeeded: 0,
                            failed: 0,
                        })
                        .await;
                    return;
                }
            };

            let total = candidates.len();
            if total == 0 {
                let _ = tx
                    .send(AppEvent::PrefetchComplete {
                        succeeded: 0,
                        failed: 0,
                    })
                    .await;
                return;
            }

            let mut succeeded = 0;
            let mut failed = 0;

            for (i, article_id) in candidates.iter().enumerate() {
                let _ = tx
                    .send(AppEvent::PrefetchProgress {
                        completed: i,
                        total,
                    })
                    .await;

                // Look up article to get URL
                let article = match db.get_article_by_id(*article_id).await {
                    Ok(Some(a)) => a,
                    _ => {
                        failed += 1;
                        continue;
                    }
                };

                let url = match article.url.as_deref() {
                    Some(u) => u,
                    None => {
                        failed += 1;
                        continue;
                    }
                };

                match fetch_content(&client, url, None).await {
                    Ok(markdown) => {
                        // Cache content (non-fatal on error)
                        let _ = db.cache_content(*article_id, &markdown, None).await;
                        // index_content writes articles.content AND triggers FTS5 update
                        let _ = db.index_content(*article_id, &markdown).await;
                        succeeded += 1;
                    }
                    Err(e) => {
                        tracing::warn!(article_id, error = %e, "Prefetch failed for article");
                        failed += 1;
                    }
                }
            }

            let _ = tx
                .send(AppEvent::PrefetchComplete { succeeded, failed })
                .await;
        })
        .await
        {
            Ok(()) => {}
            Err(panic_msg) => {
                tracing::error!(task = "prefetch", error = %panic_msg, "Prefetch task panicked");
                let _ = tx_panic
                    .send(AppEvent::TaskPanicked {
                        task: "prefetch",
                        error: panic_msg,
                    })
                    .await;
            }
        }
    });
}
