//! Application event handling.
//!
//! This module processes background task completion events such as
//! refresh progress, content loading, and star toggle results.

use crate::app::{App, AppEvent, ContentState, Focus, View};
use crate::storage::Article;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use super::reader::render_markdown;

/// Maximum number of articles in What's New list to prevent memory exhaustion
pub(super) const MAX_WHATS_NEW: usize = 100;

/// Round-robin distribute articles across feeds for variety.
///
/// Takes a list of (feed_id, article) tuples and interleaves them so no single
/// feed dominates the list. Articles within each feed maintain their order
/// (by published date from the query).
fn round_robin_by_feed(
    articles: Vec<(i64, Article)>,
    feed_title_cache: &HashMap<i64, Arc<str>>,
) -> Vec<(Arc<str>, Article)> {
    // Group articles by feed, preserving order within each feed
    let mut by_feed: HashMap<i64, Vec<Article>> = HashMap::new();
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
                let title = feed_title_cache
                    .get(feed_id)
                    .map(Arc::clone)
                    .unwrap_or_else(|| Arc::from("Unknown"));
                result.push((title, article));
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
pub(super) async fn handle_app_event(app: &mut App, event: AppEvent) {
    match event {
        AppEvent::RefreshProgress(done, total) => {
            app.refresh_progress = Some((done, total));
        }
        AppEvent::RefreshComplete(results) => {
            handle_refresh_complete(app, results).await;
        }
        AppEvent::ContentLoaded {
            article_id,
            generation,
            result,
        } => {
            handle_content_loaded(app, article_id, generation, result);
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

    // Filter out results for deleted feeds to avoid wasted work.
    // If a feed was deleted during refresh, its results are orphaned.
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

        // BUG-011: Sync feed cache to remove stale entries from deleted feeds
        // This also updates all current feed entries
        app.sync_feed_cache();

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
            tracing::debug!(article_id, generation, "Content loaded successfully");
            app.content_loading_for = None;

            match result {
                Ok(content) => {
                    // PERF-004: Parse markdown once and cache rendered lines
                    let rendered_lines = render_markdown(&content);
                    app.content_state = ContentState::Loaded {
                        article_id,
                        content,
                        rendered_lines,
                    };
                }
                Err(e) => {
                    let fallback = reader_article.summary.clone();
                    app.content_state = ContentState::Failed {
                        article_id,
                        error: e.to_string(),
                        fallback,
                    };
                }
            }
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
    // PERF-015: Use Arc::make_mut for copy-on-write optimization
    // Only clones the Vec if there are other references, otherwise mutates in place
    let articles = Arc::make_mut(&mut app.articles);
    if let Some(article) = articles.iter_mut().find(|a| a.id == article_id) {
        article.starred = starred;
    }
    // Update in whats_new by ID
    if let Some((_, article)) = app.whats_new.iter_mut().find(|(_, a)| a.id == article_id) {
        article.starred = starred;
    }
    // Invalidate cache if it exists (defense in depth)
    if app.cached_articles.is_some() {
        tracing::debug!(
            article_id,
            "Invalidating article cache in StarToggled event"
        );
        app.cached_articles = None;
    }
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

    // Also rollback in whats_new if present
    for (_, article) in app.whats_new.iter_mut() {
        if article.id == article_id {
            article.starred = original_status;
            found = true;
        }
    }

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
