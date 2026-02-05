use crate::app::AppEvent;
use crate::feed::parser::{parse_feed, ParseResult};
use crate::storage::{Database, Feed, ParsedArticle};
use anyhow::Result;
use futures::stream::{self, StreamExt};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use tokio::sync::mpsc;

const MAX_RETRIES: u32 = 3;
const MAX_FEED_SIZE: usize = 10 * 1024 * 1024; // 10MB

/// Errors that can occur during feed fetching operations.
///
/// These errors cover the full lifecycle of a fetch: network issues,
/// HTTP errors, parsing failures, and database problems.
#[derive(Debug, Error)]
pub enum FetchError {
    /// Network-level error (DNS, connection, TLS, etc.)
    #[error("Request failed: {0}")]
    Network(#[from] reqwest::Error),
    /// HTTP response with non-2xx status code
    #[error("HTTP error: status {0}")]
    HttpStatus(u16),
    /// Request exceeded the 30-second timeout
    #[error("Request timed out")]
    Timeout,
    /// Feed XML could not be parsed as RSS or Atom
    #[error("Parse error: {0}")]
    Parse(String),
    /// Database operation failed during article storage
    #[error("Database error: {0}")]
    Database(String),
    /// Server returned 429 Too Many Requests after max retries
    #[error("Rate limited after {0} retries")]
    RateLimited(u32),
    /// Response body exceeded the 10MB size limit
    #[error("Response too large")]
    ResponseTooLarge,
    /// Response was incomplete (received fewer bytes than Content-Length)
    #[error("Incomplete response: expected {expected} bytes, received {received}")]
    IncompleteResponse { expected: u64, received: usize },
}

/// Result of a single feed fetch operation.
///
/// Contains the feed ID for correlation and either the count of new
/// articles inserted or the error that occurred.
pub struct FetchResult {
    /// Database ID of the feed that was fetched
    pub feed_id: i64,
    /// Number of new articles inserted, or the error that occurred
    pub result: Result<usize, FetchError>,
}

/// Refreshes all feeds concurrently with progress reporting.
///
/// Fetches multiple feeds in parallel using a bounded concurrency pool,
/// parsing each feed's XML and upserting new articles to the database.
///
/// # Arguments
///
/// * `db` - Database handle for storing articles and updating feed status
/// * `client` - HTTP client for fetching feeds (allows custom configuration)
/// * `feeds` - List of feeds to refresh (Arc for O(1) cloning from App state)
/// * `progress_tx` - Channel for progress updates as `(completed, total)` tuples
/// * `event_tx` - Optional channel for UI events (rate limiting notifications)
///
/// # Returns
///
/// A `Vec` of [`FetchResult`] containing the outcome for each feed.
/// Results are returned in completion order, not input order.
///
/// # Behavior
///
/// - Skips feeds with 5+ consecutive failures (circuit breaker)
/// - Fetches up to 10 feeds simultaneously
/// - Each request has a 30-second timeout
/// - Rate limiting (HTTP 429) triggers exponential backoff with up to 3 retries
/// - Response bodies are limited to 10MB to prevent memory exhaustion
/// - PERF-002: Feed error statuses are batch-updated in a single transaction after all fetches complete
pub async fn refresh_all(
    db: Database,
    client: reqwest::Client,
    feeds: Arc<Vec<Feed>>,
    progress_tx: mpsc::Sender<(usize, usize)>,
    event_tx: Option<mpsc::Sender<AppEvent>>,
) -> Vec<FetchResult> {
    if feeds.is_empty() {
        return Vec::new();
    }

    // Filter out feeds that have tripped the circuit breaker
    let active_feeds: Vec<_> = feeds
        .iter()
        .filter(|f| f.consecutive_failures < Database::CIRCUIT_BREAKER_THRESHOLD)
        .cloned()
        .collect();

    let skipped = feeds.len() - active_feeds.len();
    if skipped > 0 {
        tracing::info!(
            skipped = skipped,
            threshold = Database::CIRCUIT_BREAKER_THRESHOLD,
            "Skipping feeds due to consecutive failures (use Shift+R to force refresh)"
        );
    }

    if active_feeds.is_empty() {
        // All feeds are circuit-broken, send progress complete immediately
        let _ = progress_tx.send((0, 0)).await;
        return Vec::new();
    }

    let total = active_feeds.len();
    let completed = Arc::new(AtomicUsize::new(0));

    // Clone individual feeds lazily as the stream iterates (not the entire Vec upfront)
    // Feed clone is cheap: Arc<str> for title just increments refcount
    let results: Vec<FetchResult> = stream::iter(active_feeds.into_iter())
        .map(|feed| {
            let db = db.clone();
            let client = client.clone();
            let progress_tx = progress_tx.clone();
            let completed = completed.clone();
            let event_tx = event_tx.clone();

            async move {
                let feed_id = feed.id;
                let result = fetch_one(&db, &client, &feed, event_tx.as_ref()).await;

                // Update progress (status updates are batched after all fetches complete)
                let done = completed.fetch_add(1, Ordering::Relaxed).saturating_add(1);
                if let Err(e) = progress_tx.send((done, total)).await {
                    tracing::warn!(error = %e, done = done, total = total, "Progress channel send failed (receiver dropped)");
                }

                // Track failure count for circuit breaker (success resets in complete_feed_refresh)
                if result.is_err() {
                    match db.increment_feed_failures(feed_id).await {
                        Ok(failures) => {
                            if failures >= Database::CIRCUIT_BREAKER_THRESHOLD {
                                // PERF-009: Use %feed.title directly - Arc<str> implements Display
                                tracing::info!(
                                    feed_id = feed_id,
                                    title = %feed.title,
                                    failures = failures,
                                    "Feed circuit breaker tripped - will be skipped until manual retry"
                                );
                            }
                        }
                        Err(db_err) => {
                            tracing::warn!(
                                feed_id = feed_id,
                                error = %db_err,
                                "Failed to increment feed failure count"
                            );
                        }
                    }
                }

                FetchResult {
                    feed_id,
                    result,
                }
            }
        })
        .buffer_unordered(10) // Max 10 concurrent fetches
        .collect()
        .await;

    // PERF-002: Batch update all feed error statuses in a single transaction
    // This reduces N+1 database round-trips to a single transaction
    let updates: Vec<(i64, Option<String>)> = results
        .iter()
        .map(|r| {
            let error = match &r.result {
                Ok(_) => None,
                Err(e) => Some(e.to_string()),
            };
            (r.feed_id, error)
        })
        .collect();

    if let Err(e) = db.batch_set_feed_errors(&updates).await {
        tracing::warn!(error = %e, "Failed to batch update feed error statuses");
    }

    results
}

/// Refreshes a single feed and stores new articles.
///
/// Fetches the feed's XML content, parses it, and upserts any new articles
/// to the database. Updates the feed's error status based on the result.
///
/// # Arguments
///
/// * `db` - Database handle for storing articles and updating feed status
/// * `client` - HTTP client for fetching the feed
/// * `feed` - The feed to refresh
/// * `event_tx` - Optional channel for UI events (rate limiting notifications)
///
/// # Returns
///
/// A [`FetchResult`] containing either the count of new articles inserted
/// or the error that occurred during fetching/parsing.
///
/// # Errors
///
/// The returned `FetchResult.result` may contain:
/// - [`FetchError::Network`] - Connection or TLS errors
/// - [`FetchError::Timeout`] - Request exceeded 30 seconds
/// - [`FetchError::HttpStatus`] - Non-2xx HTTP response
/// - [`FetchError::RateLimited`] - 429 response after max retries
/// - [`FetchError::ResponseTooLarge`] - Response exceeded 10MB
/// - [`FetchError::Parse`] - Invalid RSS/Atom XML
/// - [`FetchError::Database`] - Failed to store articles
///
/// # Circuit Breaker
///
/// This function bypasses the circuit breaker filter used by [`refresh_all`].
/// Used for manual `R` (Shift+R) refresh of a single feed, it always attempts
/// the fetch regardless of failure count. On success, the circuit breaker
/// counter is reset via `complete_feed_refresh`.
pub async fn refresh_one(
    db: &Database,
    client: &reqwest::Client,
    feed: &Feed,
    event_tx: Option<&mpsc::Sender<AppEvent>>,
) -> FetchResult {
    let result = fetch_one(db, client, feed, event_tx).await;

    // Update feed error status based on result
    // Note: Success resets circuit breaker via complete_feed_refresh transaction
    record_fetch_result(db, feed.id, &feed.title, &result).await;

    FetchResult {
        feed_id: feed.id,
        result,
    }
}

/// Record the result of a feed fetch operation to the database.
///
/// On success, clears any previous error and resets failure count.
/// On failure, stores the error message and increments failure count.
/// Uses fire-and-forget semantics - database errors are intentionally ignored.
///
/// # Arguments
///
/// * `db` - Database handle
/// * `feed_id` - The feed ID to update
/// * `feed_title` - The feed title (for logging)
/// * `result` - The fetch result
async fn record_fetch_result<T>(
    db: &Database,
    feed_id: i64,
    feed_title: &str,
    result: &Result<T, FetchError>,
) {
    match result {
        Ok(_) => {
            // Success resets failure count (handled in complete_feed_refresh transaction)
            let _ = db.set_feed_error(feed_id, None).await;
        }
        Err(e) => {
            let _ = db.set_feed_error(feed_id, Some(&e.to_string())).await;
            // Increment failure count for circuit breaker
            match db.increment_feed_failures(feed_id).await {
                Ok(failures) => {
                    if failures >= Database::CIRCUIT_BREAKER_THRESHOLD {
                        tracing::info!(
                            feed_id = feed_id,
                            title = %feed_title,
                            failures = failures,
                            "Feed circuit breaker tripped - will be skipped until manual retry"
                        );
                    }
                }
                Err(db_err) => {
                    tracing::warn!(
                        feed_id = feed_id,
                        error = %db_err,
                        "Failed to increment feed failure count"
                    );
                }
            }
        }
    }
}

async fn fetch_one(
    db: &Database,
    client: &reqwest::Client,
    feed: &Feed,
    event_tx: Option<&mpsc::Sender<AppEvent>>,
) -> Result<usize, FetchError> {
    let mut retry_count = 0;

    let bytes = loop {
        // Fetch with 30 second timeout
        let response = tokio::time::timeout(Duration::from_secs(30), client.get(&feed.url).send())
            .await
            .map_err(|_| FetchError::Timeout)?
            .map_err(FetchError::Network)?;

        // EDGE-004: Handle rate limiting with exponential backoff
        if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            if retry_count >= MAX_RETRIES {
                return Err(FetchError::RateLimited(MAX_RETRIES));
            }

            let delay_secs = 2u64.pow(retry_count); // 2s, 4s, 8s
            tracing::warn!(
                feed = %feed.url,
                retry = retry_count,
                delay_secs = delay_secs,
                "Rate limited, backing off"
            );

            // EDGE-002: Send rate limit notification to UI
            // PERF-016: Use Arc::clone for zero-copy feed title
            if let Some(tx) = event_tx {
                let _ = tx
                    .send(AppEvent::FeedRateLimited {
                        feed_title: Arc::clone(&feed.title),
                        delay_secs,
                    })
                    .await;
            }

            tokio::time::sleep(Duration::from_secs(delay_secs)).await;
            retry_count += 1;
            continue;
        }

        // Handle server errors (5xx) with exponential backoff
        if response.status().is_server_error() {
            if retry_count >= MAX_RETRIES {
                return Err(FetchError::HttpStatus(response.status().as_u16()));
            }

            let delay_secs = 2u64.pow(retry_count); // 2s, 4s, 8s
            tracing::warn!(
                feed = %feed.url,
                status = %response.status(),
                retry = retry_count,
                delay_secs = delay_secs,
                "Server error, retrying after delay"
            );

            tokio::time::sleep(Duration::from_secs(delay_secs)).await;
            retry_count += 1;
            continue;
        }

        // EDGE-002: Validate HTTP status before processing (4xx errors fail immediately)
        if !response.status().is_success() {
            return Err(FetchError::HttpStatus(response.status().as_u16()));
        }

        // Read response body with size limit and completeness check
        match read_limited_bytes(response, MAX_FEED_SIZE).await {
            Ok(bytes) => break bytes,
            Err(FetchError::IncompleteResponse { expected, received }) => {
                // EDGE-005: Handle incomplete downloads with retry and exponential backoff
                if retry_count >= MAX_RETRIES {
                    return Err(FetchError::IncompleteResponse { expected, received });
                }

                let delay_secs = 2u64.pow(retry_count); // 2s, 4s, 8s
                tracing::debug!(
                    feed = %feed.url,
                    expected = expected,
                    received = received,
                    attempt = retry_count + 1,
                    delay_secs = delay_secs,
                    "Retrying incomplete download"
                );

                tokio::time::sleep(Duration::from_secs(delay_secs)).await;
                retry_count += 1;
                continue;
            }
            Err(e) => return Err(e),
        }
    };

    // Parse feed with best-effort recovery for malformed items
    let ParseResult { articles, skipped } =
        parse_feed(&bytes).map_err(|e| FetchError::Parse(e.to_string()))?;

    // Log warning if any articles were filtered due to invalid URLs
    if skipped > 0 {
        tracing::warn!(
            feed = %feed.url,
            filtered = skipped,
            "Articles with invalid URLs skipped"
        );
    }

    // Convert to ParsedArticle format expected by db
    let parsed: Vec<ParsedArticle> = articles
        .into_iter()
        .map(|a| ParsedArticle {
            guid: a.guid,
            title: a.title,
            url: a.url,
            published: a.published,
            summary: a.summary,
        })
        .collect();

    // Complete feed refresh atomically: clear error, upsert articles, update timestamp
    // All operations wrapped in single transaction for data integrity
    let count = db
        .complete_feed_refresh(feed.id, &parsed)
        .await
        .map_err(|e| FetchError::Database(e.to_string()))?;

    Ok(count)
}

async fn read_limited_bytes(
    response: reqwest::Response,
    limit: usize,
) -> Result<Vec<u8>, FetchError> {
    // Capture Content-Length for completeness check
    let expected_length = response.content_length();

    // Fast path: check Content-Length header
    if let Some(len) = expected_length {
        if len as usize > limit {
            return Err(FetchError::ResponseTooLarge);
        }
    }

    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(FetchError::Network)?;
        if bytes.len().saturating_add(chunk.len()) > limit {
            return Err(FetchError::ResponseTooLarge);
        }
        bytes.extend_from_slice(&chunk);
    }

    // EDGE-005: Check for incomplete response (received fewer bytes than Content-Length)
    // This can happen when network interruptions occur during chunk reading.
    // Callers can retry this operation with exponential backoff.
    if let Some(expected) = expected_length {
        if (bytes.len() as u64) < expected {
            return Err(FetchError::IncompleteResponse {
                expected,
                received: bytes.len(),
            });
        }
    }

    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{Database, OpmlFeed};
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const VALID_RSS: &str = r#"<?xml version="1.0"?>
<rss version="2.0"><channel>
    <item><guid>1</guid><title>Test</title></item>
</channel></rss>"#;

    async fn setup_db_with_feed(url: &str) -> (Database, Feed) {
        let db = Database::open(":memory:").await.unwrap();
        db.sync_feeds(&[OpmlFeed {
            title: "Test".into(),
            xml_url: url.into(),
            html_url: None,
        }])
        .await
        .unwrap();
        let feeds = db.get_feeds_with_unread_counts().await.unwrap();
        (db, feeds.into_iter().next().unwrap())
    }

    #[tokio::test]
    async fn test_refresh_success() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(VALID_RSS)
                    .insert_header("Content-Type", "application/xml"),
            )
            .mount(&mock_server)
            .await;

        let (db, feed) = setup_db_with_feed(&format!("{}/feed", mock_server.uri())).await;
        let client = reqwest::Client::new();

        let result = refresh_one(&db, &client, &feed, None).await;
        assert!(result.result.is_ok());
        assert_eq!(result.result.unwrap(), 1); // One article inserted
    }

    #[tokio::test]
    async fn test_refresh_404_error() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&mock_server)
            .await;

        let (db, feed) = setup_db_with_feed(&format!("{}/feed", mock_server.uri())).await;
        let client = reqwest::Client::new();

        let result = refresh_one(&db, &client, &feed, None).await;
        assert!(result.result.is_err());
        match result.result.unwrap_err() {
            FetchError::HttpStatus(404) => {}
            e => panic!("Expected HttpStatus(404), got {:?}", e),
        }
    }

    #[tokio::test]
    async fn test_refresh_500_error_retries_then_fails() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(500))
            .expect(4) // Initial request + 3 retries
            .mount(&mock_server)
            .await;

        let (db, feed) = setup_db_with_feed(&format!("{}/feed", mock_server.uri())).await;
        let client = reqwest::Client::new();

        let result = refresh_one(&db, &client, &feed, None).await;
        assert!(result.result.is_err());
        match result.result.unwrap_err() {
            FetchError::HttpStatus(500) => {}
            e => panic!("Expected HttpStatus(500), got {:?}", e),
        }
    }

    #[tokio::test]
    async fn test_refresh_503_retry_then_success() {
        use wiremock::matchers::any;

        let mock_server = MockServer::start().await;

        // First two requests return 503, third succeeds
        Mock::given(any())
            .respond_with(ResponseTemplate::new(503))
            .up_to_n_times(2)
            .mount(&mock_server)
            .await;

        Mock::given(any())
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(VALID_RSS)
                    .insert_header("Content-Type", "application/xml"),
            )
            .mount(&mock_server)
            .await;

        let (db, feed) = setup_db_with_feed(&format!("{}/feed", mock_server.uri())).await;
        let client = reqwest::Client::new();

        let result = refresh_one(&db, &client, &feed, None).await;
        assert!(result.result.is_ok());
        assert_eq!(result.result.unwrap(), 1); // One article inserted after retry
    }

    #[tokio::test]
    async fn test_malformed_feed_parse_error() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string("<not valid xml"))
            .mount(&mock_server)
            .await;

        let (db, feed) = setup_db_with_feed(&format!("{}/feed", mock_server.uri())).await;
        let client = reqwest::Client::new();

        let result = refresh_one(&db, &client, &feed, None).await;
        assert!(result.result.is_err());
        match result.result.unwrap_err() {
            FetchError::Parse(_) => {}
            e => panic!("Expected Parse error, got {:?}", e),
        }
    }

    #[tokio::test]
    async fn test_empty_feed_success() {
        let empty_rss = r#"<?xml version="1.0"?>
<rss version="2.0"><channel></channel></rss>"#;

        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string(empty_rss))
            .mount(&mock_server)
            .await;

        let (db, feed) = setup_db_with_feed(&format!("{}/feed", mock_server.uri())).await;
        let client = reqwest::Client::new();

        let result = refresh_one(&db, &client, &feed, None).await;
        assert!(result.result.is_ok());
        assert_eq!(result.result.unwrap(), 0); // No articles inserted
    }
}
