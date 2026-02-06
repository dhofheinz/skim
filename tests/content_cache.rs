//! Integration tests for the content cache lifecycle: TTL, eviction, offline fallback.
//!
//! Each test creates its own in-memory SQLite database for isolation.
//! These tests exercise the cache operations end-to-end, verifying that
//! content caching, retrieval, eviction, FTS5 indexing, and batch queries
//! compose correctly.

use skim::storage::{Database, OpmlFeed, ParsedArticle, SearchScope};

async fn test_db() -> Database {
    Database::open(":memory:").await.unwrap()
}

fn test_feed() -> OpmlFeed {
    OpmlFeed {
        title: "Cache Lifecycle Feed".to_string(),
        xml_url: "https://cache-lifecycle.example.com/rss".to_string(),
        html_url: None,
    }
}

fn test_article(guid: &str, title: &str) -> ParsedArticle {
    ParsedArticle {
        guid: guid.to_string(),
        title: title.to_string(),
        url: Some(format!("https://example.com/{guid}")),
        published: Some(1704067200),
        summary: Some(format!("Summary for {title}")),
    }
}

/// Helper: insert a feed and articles, returning the feed_id and article ids.
async fn seed_articles(db: &Database, count: usize) -> (i64, Vec<i64>) {
    db.sync_feeds(&[test_feed()]).await.unwrap();
    let feeds = db.get_feeds_with_unread_counts().await.unwrap();
    let feed_id = feeds[0].id;

    let articles: Vec<_> = (0..count)
        .map(|i| test_article(&format!("guid-{i}"), &format!("Article {i}")))
        .collect();
    db.upsert_articles(feed_id, &articles).await.unwrap();

    let db_articles = db.get_articles_for_feed(feed_id, None).await.unwrap();
    let ids: Vec<i64> = db_articles.iter().map(|a| a.id).collect();
    (feed_id, ids)
}

// ============================================================================
// Cache Content and Retrieval
// ============================================================================

#[tokio::test]
async fn test_cache_content_and_get() {
    let db = test_db().await;
    let (_feed_id, ids) = seed_articles(&db, 1).await;

    db.cache_content(ids[0], "# Hello World", None)
        .await
        .unwrap();

    let cached = db.get_cached_content(ids[0]).await.unwrap();
    assert!(cached.is_some());
    let cached = cached.unwrap();
    assert_eq!(cached.markdown, "# Hello World");
    assert_eq!(cached.size_bytes, "# Hello World".len() as i64);
}

#[tokio::test]
async fn test_cache_replace_updates_content() {
    let db = test_db().await;
    let (_feed_id, ids) = seed_articles(&db, 1).await;

    db.cache_content(ids[0], "original", None).await.unwrap();
    db.cache_content(ids[0], "updated", None).await.unwrap();

    let cached = db.get_cached_content(ids[0]).await.unwrap().unwrap();
    assert_eq!(cached.markdown, "updated");
    assert_eq!(cached.size_bytes, "updated".len() as i64);
}

#[tokio::test]
async fn test_uncached_article_returns_none() {
    let db = test_db().await;
    let (_feed_id, ids) = seed_articles(&db, 1).await;

    let cached = db.get_cached_content(ids[0]).await.unwrap();
    assert!(cached.is_none());
}

// ============================================================================
// Cache Statistics
// ============================================================================

#[tokio::test]
async fn test_cache_stats_empty() {
    let db = test_db().await;
    let _ids = seed_articles(&db, 1).await;

    let stats = db.cache_stats().await.unwrap();
    assert_eq!(stats.total_entries, 0);
    assert_eq!(stats.total_size_bytes, 0);
    assert!(stats.oldest_entry.is_none());
    assert!(stats.newest_entry.is_none());
}

#[tokio::test]
async fn test_cache_stats_accuracy() {
    let db = test_db().await;
    let (_feed_id, ids) = seed_articles(&db, 3).await;

    db.cache_content(ids[0], "aaa", None).await.unwrap();
    db.cache_content(ids[1], "bbbbb", None).await.unwrap();
    db.cache_content(ids[2], "c", None).await.unwrap();

    let stats = db.cache_stats().await.unwrap();
    assert_eq!(stats.total_entries, 3);
    assert_eq!(stats.total_size_bytes, 3 + 5 + 1);
    assert!(stats.oldest_entry.is_some());
    assert!(stats.newest_entry.is_some());
}

// ============================================================================
// Prefetch Candidates
// ============================================================================

#[tokio::test]
async fn test_prefetch_candidates_excludes_cached() {
    let db = test_db().await;
    let (_feed_id, ids) = seed_articles(&db, 3).await;

    // All 3 unread and uncached
    let candidates = db.prefetch_candidates(10).await.unwrap();
    assert_eq!(candidates.len(), 3);

    // Cache one
    db.cache_content(ids[0], "cached", None).await.unwrap();
    let candidates = db.prefetch_candidates(10).await.unwrap();
    assert_eq!(candidates.len(), 2);
    assert!(!candidates.contains(&ids[0]));
}

#[tokio::test]
async fn test_prefetch_candidates_excludes_read() {
    let db = test_db().await;
    let (_feed_id, ids) = seed_articles(&db, 3).await;

    db.mark_article_read(ids[1]).await.unwrap();
    let candidates = db.prefetch_candidates(10).await.unwrap();
    assert_eq!(candidates.len(), 2);
    assert!(!candidates.contains(&ids[1]));
}

#[tokio::test]
async fn test_prefetch_candidates_respects_limit() {
    let db = test_db().await;
    let _ids = seed_articles(&db, 5).await;

    let candidates = db.prefetch_candidates(2).await.unwrap();
    assert_eq!(candidates.len(), 2);
}

#[tokio::test]
async fn test_prefetch_candidates_for_feed() {
    let db = test_db().await;

    // Create two feeds
    let feed2 = OpmlFeed {
        title: "Second Feed".to_string(),
        xml_url: "https://second.example.com/rss".to_string(),
        html_url: None,
    };
    db.sync_feeds(&[test_feed(), feed2]).await.unwrap();
    let feeds = db.get_feeds_with_unread_counts().await.unwrap();

    // Insert articles in both feeds
    db.upsert_articles(feeds[0].id, &[test_article("a1", "A1")])
        .await
        .unwrap();
    db.upsert_articles(
        feeds[1].id,
        &[test_article("b1", "B1"), test_article("b2", "B2")],
    )
    .await
    .unwrap();

    // Feed-specific candidates
    let candidates_feed1 = db
        .prefetch_candidates_for_feed(feeds[0].id, 10)
        .await
        .unwrap();
    assert_eq!(candidates_feed1.len(), 1);

    let candidates_feed2 = db
        .prefetch_candidates_for_feed(feeds[1].id, 10)
        .await
        .unwrap();
    assert_eq!(candidates_feed2.len(), 2);
}

// ============================================================================
// Cached Article IDs (Batch Query)
// ============================================================================

#[tokio::test]
async fn test_cached_article_ids_empty_returns_empty() {
    let db = test_db().await;
    let (_feed_id, ids) = seed_articles(&db, 3).await;

    let cached = db.cached_article_ids(&ids).await.unwrap();
    assert!(cached.is_empty());
}

#[tokio::test]
async fn test_cached_article_ids_returns_subset() {
    let db = test_db().await;
    let (_feed_id, ids) = seed_articles(&db, 4).await;

    db.cache_content(ids[0], "content0", None).await.unwrap();
    db.cache_content(ids[2], "content2", None).await.unwrap();

    let cached = db.cached_article_ids(&ids).await.unwrap();
    assert_eq!(cached.len(), 2);
    assert!(cached.contains(&ids[0]));
    assert!(cached.contains(&ids[2]));
    assert!(!cached.contains(&ids[1]));
    assert!(!cached.contains(&ids[3]));
}

#[tokio::test]
async fn test_cached_article_ids_empty_input() {
    let db = test_db().await;
    let _ids = seed_articles(&db, 1).await;

    let cached = db.cached_article_ids(&[]).await.unwrap();
    assert!(cached.is_empty());
}

// ============================================================================
// Cascade Delete
// ============================================================================

#[tokio::test]
async fn test_cascade_delete_removes_cache() {
    let db = test_db().await;
    let (_feed_id, ids) = seed_articles(&db, 2).await;

    db.cache_content(ids[0], "cached content", None)
        .await
        .unwrap();
    db.cache_content(ids[1], "more cached", None).await.unwrap();

    let stats = db.cache_stats().await.unwrap();
    assert_eq!(stats.total_entries, 2);

    // Delete the feed — cascades to articles, which cascades to content_cache
    db.delete_feed(_feed_id).await.unwrap();

    let stats = db.cache_stats().await.unwrap();
    assert_eq!(stats.total_entries, 0);
}

// ============================================================================
// FTS5 Content Indexing + Search
// ============================================================================

#[tokio::test]
async fn test_content_searchable_after_indexing() {
    let db = test_db().await;
    let (_feed_id, ids) = seed_articles(&db, 1).await;

    // Index content for FTS5
    db.index_content(
        ids[0],
        "This article discusses quantum computing breakthroughs",
    )
    .await
    .unwrap();

    // Search with All scope should find it
    let results = db
        .search_articles("quantum", SearchScope::All)
        .await
        .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].id, ids[0]);
}

#[tokio::test]
async fn test_content_not_searchable_with_title_summary_scope() {
    let db = test_db().await;
    let (_feed_id, ids) = seed_articles(&db, 1).await;

    // Index content (not in title or summary)
    db.index_content(ids[0], "This discusses quantum computing")
        .await
        .unwrap();

    // Search with TitleAndSummary scope should NOT find it (term only in content)
    let results = db
        .search_articles("quantum", SearchScope::TitleAndSummary)
        .await
        .unwrap();
    assert!(
        results.is_empty(),
        "Content-only terms should not match TitleAndSummary scope"
    );
}

#[tokio::test]
async fn test_cache_and_index_workflow() {
    let db = test_db().await;
    let (_feed_id, ids) = seed_articles(&db, 1).await;

    let markdown = "# Deep Learning Guide\n\nNeural networks explained step by step.";

    // Simulate TASK-4 workflow: cache + set_article_content + index
    db.cache_content(ids[0], markdown, None).await.unwrap();
    db.set_article_content(ids[0], markdown).await.unwrap();
    db.index_content(ids[0], markdown).await.unwrap();

    // Verify cached
    let cached = db.get_cached_content(ids[0]).await.unwrap();
    assert!(cached.is_some());
    assert_eq!(cached.unwrap().markdown, markdown);

    // Verify searchable
    let results = db
        .search_articles("neural", SearchScope::All)
        .await
        .unwrap();
    assert_eq!(results.len(), 1);
}

// ============================================================================
// Eviction
// ============================================================================

#[tokio::test]
async fn test_evict_expired_no_valid_entries_affected() {
    let db = test_db().await;
    let (_feed_id, ids) = seed_articles(&db, 2).await;

    // Cache with default TTL (72h — valid)
    db.cache_content(ids[0], "content0", None).await.unwrap();
    db.cache_content(ids[1], "content1", None).await.unwrap();

    let evicted = db.evict_expired().await.unwrap();
    assert_eq!(evicted, 0, "No entries should be evicted with fresh cache");

    // Both should still be retrievable
    assert!(db.get_cached_content(ids[0]).await.unwrap().is_some());
    assert!(db.get_cached_content(ids[1]).await.unwrap().is_some());
}
