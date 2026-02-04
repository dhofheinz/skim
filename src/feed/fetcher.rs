use crate::feed::parser::{parse_feed, ParsedArticle as ParserArticle};
use crate::storage::{Database, Feed, ParsedArticle};
use anyhow::Result;
use futures::stream::{self, StreamExt};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use tokio::sync::mpsc;

#[derive(Debug, Error)]
pub enum FetchError {
    #[error("Request failed: {0}")]
    Network(#[from] reqwest::Error),
    #[error("Request timed out")]
    Timeout,
    #[error("Parse error: {0}")]
    Parse(String),
}

pub struct FetchResult {
    pub feed_id: i64,
    pub result: Result<usize, FetchError>,
}

/// Refresh all feeds concurrently with progress reporting.
///
/// - Fetches up to 10 feeds simultaneously
/// - 30 second timeout per feed
/// - Reports progress via channel: (completed_count, total_count)
/// - Stores results in database (upserts articles, updates feed status)
pub async fn refresh_all(
    db: Database,
    client: reqwest::Client,
    feeds: Vec<Feed>,
    progress_tx: mpsc::Sender<(usize, usize)>,
) -> Vec<FetchResult> {
    let total = feeds.len();
    let completed = Arc::new(AtomicUsize::new(0));

    let results: Vec<FetchResult> = stream::iter(feeds.into_iter())
        .map(|feed| {
            let db = db.clone();
            let client = client.clone();
            let progress_tx = progress_tx.clone();
            let completed = completed.clone();

            async move {
                let result = fetch_one(&db, &client, &feed).await;

                // Update feed error status based on result
                match &result {
                    Ok(_) => {
                        let _ = db.set_feed_error(feed.id, None).await;
                    }
                    Err(e) => {
                        let _ = db.set_feed_error(feed.id, Some(&e.to_string())).await;
                    }
                }

                // Update progress
                let done = completed.fetch_add(1, Ordering::SeqCst) + 1;
                let _ = progress_tx.send((done, total)).await;

                FetchResult {
                    feed_id: feed.id,
                    result,
                }
            }
        })
        .buffer_unordered(10) // Max 10 concurrent fetches
        .collect()
        .await;

    results
}

/// Refresh a single feed.
///
/// Returns the number of new articles fetched, or an error.
pub async fn refresh_one(db: &Database, client: &reqwest::Client, feed: &Feed) -> FetchResult {
    let result = fetch_one(db, client, feed).await;

    // Update feed error status based on result
    match &result {
        Ok(_) => {
            let _ = db.set_feed_error(feed.id, None).await;
        }
        Err(e) => {
            let _ = db.set_feed_error(feed.id, Some(&e.to_string())).await;
        }
    }

    FetchResult {
        feed_id: feed.id,
        result,
    }
}

async fn fetch_one(
    db: &Database,
    client: &reqwest::Client,
    feed: &Feed,
) -> Result<usize, FetchError> {
    // Fetch with 30 second timeout
    let response = tokio::time::timeout(Duration::from_secs(30), client.get(&feed.url).send())
        .await
        .map_err(|_| FetchError::Timeout)?
        .map_err(FetchError::Network)?;

    let bytes = response.bytes().await.map_err(FetchError::Network)?;

    // Parse feed
    let articles: Vec<ParserArticle> =
        parse_feed(&bytes).map_err(|e| FetchError::Parse(e.to_string()))?;

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

    // Upsert to database
    let count = db
        .upsert_articles(feed.id, &parsed)
        .await
        .map_err(|e| FetchError::Parse(e.to_string()))?;

    // Update feed status
    db.update_feed_fetched(feed.id)
        .await
        .map_err(|e| FetchError::Parse(e.to_string()))?;

    Ok(count)
}
