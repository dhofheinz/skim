//! Helper functions for UI operations.
//!
//! This module contains utility functions shared across the UI layer,
//! including mode transitions, content loading, and URL validation.

use crate::app::{App, AppEvent, ContentState};
use crate::content::fetch_content;
use crate::storage::{Article, Database};
use anyhow::Result;
use futures::FutureExt;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
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

/// Validates a URL before passing to open::that() to prevent command injection.
///
/// # Security
///
/// This function guards against:
/// - Non-HTTP(S) schemes (e.g., `file://`, `javascript:`, custom schemes)
/// - Control characters (ASCII 0-31, DEL) that could manipulate shell behavior
/// - Dangerous shell metacharacters (backtick, $, ;, |, <, >, etc.) that could enable command injection
/// - Malformed URLs that could be interpreted as shell commands
///
/// Note: Valid URL characters like `&`, `?`, `=`, `#` are allowed since they're common in query strings.
///
/// # Returns
///
/// - `Ok(())` if the URL is safe to open
/// - `Err(&'static str)` with a user-friendly error message otherwise
pub(super) fn validate_url_for_open(url_str: &str) -> Result<(), &'static str> {
    use url::Url;

    // Check for control characters (ASCII 0-31 and DEL 127)
    if url_str.bytes().any(|b| b < 32 || b == 127) {
        return Err("URL contains invalid control characters");
    }

    // Ensure URL uses http or https scheme
    if !url_str.starts_with("http://") && !url_str.starts_with("https://") {
        return Err("URL must use http or https scheme");
    }

    // Reject particularly dangerous shell metacharacters
    // Note: & is valid in query strings, so we only block the most dangerous ones
    const DANGEROUS_CHARS: &[char] = &['`', '$', ';', '|', '<', '>', '(', ')', '{', '}'];
    if url_str.chars().any(|c| DANGEROUS_CHARS.contains(&c)) {
        return Err("URL contains potentially unsafe characters");
    }

    // Parse with url crate for additional format validation
    Url::parse(url_str).map_err(|_| "Invalid URL format")?;

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
pub(super) fn try_spawn_content_load(
    app: &mut App,
    article: &Article,
    event_tx: &mpsc::Sender<AppEvent>,
) -> bool {
    let article_id = article.id;

    // Check if already loading content for a different article
    if app.content_loading_for.is_some() && app.content_loading_for != Some(article_id) {
        tracing::debug!(
            old = ?app.content_loading_for,
            new = article_id,
            "Clearing old content load, starting new"
        );
        app.content_loading_for = None;
    }

    // Guard: skip if already loading for this article
    if app.content_loading_for.is_some() {
        tracing::debug!(article_id, "Content load already in progress, skipping");
        return false;
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

/// Spawn a background task to load article content with caching.
///
/// Checks the database cache first, fetching from jina.ai on cache miss.
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
            // Check cache first
            if let Ok(Some(cached)) = db.get_article_content(article_id).await {
                tracing::debug!(article_id, generation, "content cache hit");
                if let Err(e) = tx
                    .send(AppEvent::ContentLoaded {
                        article_id,
                        generation,
                        result: Ok(cached),
                    })
                    .await
                {
                    tracing::warn!(error = %e, event = "ContentLoaded", "Channel send failed (receiver dropped)");
                }
                return;
            }

            // Cache miss - fetch from jina.ai
            let result = fetch_content(&client, &url, None).await;

            // Store in cache on success
            if let Ok(ref content) = result {
                if let Err(e) = db.set_article_content(article_id, content).await {
                    tracing::warn!(article_id, error = %e, "failed to cache content");
                    let _ = tx
                        .send(AppEvent::ContentCacheFailed {
                            article_id,
                            error: e.to_string(),
                        })
                        .await;
                }
            }

            if let Err(e) = tx
                .send(AppEvent::ContentLoaded {
                    article_id,
                    generation,
                    result,
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
