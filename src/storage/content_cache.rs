use anyhow::Result;
use sqlx::QueryBuilder;

use super::schema::Database;
use super::types::{CacheStats, CachedContent};

/// Default TTL for cached content (72 hours)
const DEFAULT_TTL_HOURS: i64 = 72;

impl Database {
    // ========================================================================
    // Content Cache Operations
    // ========================================================================

    /// Cache article content with a TTL.
    ///
    /// Inserts or replaces the cached markdown for the given article.
    /// `size_bytes` is computed from the markdown byte length.
    /// `expires_at` is computed as `now + ttl_hours`.
    ///
    /// # Arguments
    ///
    /// * `article_id` - The database ID of the article
    /// * `markdown` - The full article content (markdown from jina.ai)
    /// * `ttl_hours` - Hours until this cache entry expires (use `None` for default 72h)
    pub async fn cache_content(
        &self,
        article_id: i64,
        markdown: &str,
        ttl_hours: Option<i64>,
    ) -> Result<()> {
        let ttl = ttl_hours.unwrap_or(DEFAULT_TTL_HOURS).max(1);
        let size_bytes = markdown.len() as i64;
        let ttl_modifier = format!("+{ttl} hours");

        sqlx::query(
            r#"
            INSERT OR REPLACE INTO content_cache
                (article_id, markdown, fetched_at, expires_at, size_bytes)
            VALUES (?, ?, datetime('now'), datetime('now', ?), ?)
        "#,
        )
        .bind(article_id)
        .bind(markdown)
        .bind(&ttl_modifier)
        .bind(size_bytes)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Retrieve cached content if it has not expired.
    ///
    /// Returns `None` if no cache entry exists or if the entry has expired.
    pub async fn get_cached_content(&self, article_id: i64) -> Result<Option<CachedContent>> {
        let row: Option<(i64, String, String, String, i64)> = sqlx::query_as(
            r#"
            SELECT article_id, markdown, fetched_at, expires_at, size_bytes
            FROM content_cache
            WHERE article_id = ? AND expires_at > datetime('now')
        "#,
        )
        .bind(article_id)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(
            |(article_id, markdown, fetched_at, expires_at, size_bytes)| CachedContent {
                article_id,
                markdown,
                fetched_at,
                expires_at,
                size_bytes,
            },
        ))
    }

    /// Delete all expired cache entries.
    ///
    /// Returns the number of entries evicted.
    pub async fn evict_expired(&self) -> Result<u64> {
        let result = sqlx::query("DELETE FROM content_cache WHERE expires_at < datetime('now')")
            .execute(&self.pool)
            .await?;

        Ok(result.rows_affected())
    }

    /// Compute aggregate cache statistics.
    ///
    /// Returns total entry count, total size in bytes, and oldest/newest
    /// `fetched_at` timestamps.
    #[allow(dead_code)] // Consumed by TASK-8 (stats panel) and TASK-10 (cache tests)
    pub async fn cache_stats(&self) -> Result<CacheStats> {
        let row: (i64, Option<i64>, Option<String>, Option<String>) = sqlx::query_as(
            r#"
            SELECT COUNT(*), SUM(size_bytes), MIN(fetched_at), MAX(fetched_at)
            FROM content_cache
        "#,
        )
        .fetch_one(&self.pool)
        .await?;

        Ok(CacheStats {
            total_entries: row.0,
            total_size_bytes: row.1.unwrap_or(0),
            oldest_entry: row.2,
            newest_entry: row.3,
        })
    }

    /// Find unread articles that have no cache entries, ordered by published DESC.
    ///
    /// Used by the prefetch scheduler to prioritise which articles to fetch next.
    #[allow(dead_code)] // Consumed by TASK-7 (bulk prefetch) and TASK-10 (cache tests)
    pub async fn prefetch_candidates(&self, limit: i64) -> Result<Vec<i64>> {
        let rows: Vec<(i64,)> = sqlx::query_as(
            r#"
            SELECT a.id
            FROM articles a
            LEFT JOIN content_cache cc ON a.id = cc.article_id
            WHERE a.read = 0 AND cc.article_id IS NULL
            ORDER BY a.published DESC
            LIMIT ?
        "#,
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.into_iter().map(|(id,)| id).collect())
    }

    /// Find unread articles for a specific feed that have no cache entries.
    ///
    /// Used by the `P` keybind prefetch action to cache all unread articles
    /// in the selected feed for offline reading.
    pub async fn prefetch_candidates_for_feed(&self, feed_id: i64, limit: i64) -> Result<Vec<i64>> {
        let rows: Vec<(i64,)> = sqlx::query_as(
            r#"
            SELECT a.id
            FROM articles a
            LEFT JOIN content_cache cc ON a.id = cc.article_id
            WHERE a.feed_id = ? AND a.read = 0 AND cc.article_id IS NULL
            ORDER BY a.published DESC
            LIMIT ?
        "#,
        )
        .bind(feed_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.into_iter().map(|(id,)| id).collect())
    }

    /// Batch-check which of the given article IDs have valid (non-expired) cache entries.
    ///
    /// Returns the subset of `ids` that have cached content.
    /// PERF-001: Chunks at 500 IDs per query to avoid SQLite bind-parameter limits.
    pub async fn cached_article_ids(&self, ids: &[i64]) -> Result<Vec<i64>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }

        const CHUNK_SIZE: usize = 500;
        let mut result = Vec::new();

        for chunk in ids.chunks(CHUNK_SIZE) {
            let mut builder: QueryBuilder<sqlx::Sqlite> = QueryBuilder::new(
                "SELECT article_id FROM content_cache WHERE expires_at > datetime('now') AND article_id IN (",
            );

            let mut separated = builder.separated(", ");
            for id in chunk {
                separated.push_bind(*id);
            }
            separated.push_unseparated(")");

            let rows: Vec<(i64,)> = builder.build_query_as().fetch_all(&self.pool).await?;
            result.extend(rows.into_iter().map(|(id,)| id));
        }

        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use crate::storage::{Database, OpmlFeed, ParsedArticle};

    async fn test_db() -> Database {
        Database::open(":memory:").await.unwrap()
    }

    fn test_feed() -> OpmlFeed {
        OpmlFeed {
            title: "Cache Test Feed".to_string(),
            xml_url: "https://cache-test.example.com/rss".to_string(),
            html_url: None,
        }
    }

    fn test_article(guid: &str, title: &str) -> ParsedArticle {
        ParsedArticle {
            guid: guid.to_string(),
            title: title.to_string(),
            url: Some(format!("https://example.com/{guid}")),
            published: Some(1704067200),
            summary: Some("Test summary".to_string()),
        }
    }

    /// Helper: insert a feed and articles, returning the feed_id and article ids.
    async fn setup_articles(db: &Database, count: usize) -> (i64, Vec<i64>) {
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

    #[tokio::test]
    async fn test_cache_content() {
        let db = test_db().await;
        let (_feed_id, ids) = setup_articles(&db, 1).await;

        db.cache_content(ids[0], "# Hello World", None)
            .await
            .unwrap();

        let cached = db.get_cached_content(ids[0]).await.unwrap();
        assert!(cached.is_some());
        let cached = cached.unwrap();
        assert_eq!(cached.article_id, ids[0]);
        assert_eq!(cached.markdown, "# Hello World");
        assert_eq!(cached.size_bytes, "# Hello World".len() as i64);
    }

    #[tokio::test]
    async fn test_cache_content_replace() {
        let db = test_db().await;
        let (_feed_id, ids) = setup_articles(&db, 1).await;

        db.cache_content(ids[0], "old content", None).await.unwrap();
        db.cache_content(ids[0], "new content", None).await.unwrap();

        let cached = db.get_cached_content(ids[0]).await.unwrap().unwrap();
        assert_eq!(cached.markdown, "new content");
        assert_eq!(cached.size_bytes, "new content".len() as i64);
    }

    #[tokio::test]
    async fn test_get_expired_returns_none() {
        let db = test_db().await;
        let (_feed_id, ids) = setup_articles(&db, 1).await;

        // Insert with TTL of 0 hours (immediately expired)
        // We need to manually insert with an already-past expires_at
        sqlx::query(
            r#"
            INSERT INTO content_cache (article_id, markdown, fetched_at, expires_at, size_bytes)
            VALUES (?, 'expired content', datetime('now', '-1 hour'), datetime('now', '-1 second'), 15)
        "#,
        )
        .bind(ids[0])
        .execute(&db.pool)
        .await
        .unwrap();

        let cached = db.get_cached_content(ids[0]).await.unwrap();
        assert!(cached.is_none(), "Expired cache entry should return None");
    }

    #[tokio::test]
    async fn test_evict_expired() {
        let db = test_db().await;
        let (_feed_id, ids) = setup_articles(&db, 2).await;

        // Cache one article with default TTL (valid)
        db.cache_content(ids[0], "valid content", None)
            .await
            .unwrap();

        // Insert one expired entry directly
        sqlx::query(
            r#"
            INSERT INTO content_cache (article_id, markdown, fetched_at, expires_at, size_bytes)
            VALUES (?, 'expired', datetime('now', '-2 hours'), datetime('now', '-1 second'), 7)
        "#,
        )
        .bind(ids[1])
        .execute(&db.pool)
        .await
        .unwrap();

        let evicted = db.evict_expired().await.unwrap();
        assert_eq!(evicted, 1);

        // Valid entry should still exist
        assert!(db.get_cached_content(ids[0]).await.unwrap().is_some());
        // Expired entry should be gone (even from raw query)
        let row: Option<(i64,)> =
            sqlx::query_as("SELECT article_id FROM content_cache WHERE article_id = ?")
                .bind(ids[1])
                .fetch_optional(&db.pool)
                .await
                .unwrap();
        assert!(row.is_none());
    }

    #[tokio::test]
    async fn test_cache_stats() {
        let db = test_db().await;
        let (_feed_id, ids) = setup_articles(&db, 3).await;

        // Empty cache
        let stats = db.cache_stats().await.unwrap();
        assert_eq!(stats.total_entries, 0);
        assert_eq!(stats.total_size_bytes, 0);
        assert!(stats.oldest_entry.is_none());
        assert!(stats.newest_entry.is_none());

        // Add some entries
        db.cache_content(ids[0], "short", None).await.unwrap();
        db.cache_content(ids[1], "medium content here", None)
            .await
            .unwrap();
        db.cache_content(ids[2], "a", None).await.unwrap();

        let stats = db.cache_stats().await.unwrap();
        assert_eq!(stats.total_entries, 3);
        assert_eq!(
            stats.total_size_bytes,
            "short".len() as i64 + "medium content here".len() as i64 + "a".len() as i64
        );
        assert!(stats.oldest_entry.is_some());
        assert!(stats.newest_entry.is_some());
    }

    #[tokio::test]
    async fn test_prefetch_candidates() {
        let db = test_db().await;
        let (_feed_id, ids) = setup_articles(&db, 3).await;

        // All 3 articles are unread and uncached — all should be candidates
        let candidates = db.prefetch_candidates(10).await.unwrap();
        assert_eq!(candidates.len(), 3);

        // Cache one article — it should no longer be a candidate
        db.cache_content(ids[0], "cached", None).await.unwrap();
        let candidates = db.prefetch_candidates(10).await.unwrap();
        assert_eq!(candidates.len(), 2);
        assert!(!candidates.contains(&ids[0]));

        // Mark one article as read — it should no longer be a candidate
        db.mark_article_read(ids[1]).await.unwrap();
        let candidates = db.prefetch_candidates(10).await.unwrap();
        assert_eq!(candidates.len(), 1);
        assert!(!candidates.contains(&ids[1]));

        // Limit parameter should work
        let candidates = db.prefetch_candidates(0).await.unwrap();
        assert!(candidates.is_empty());
    }

    #[tokio::test]
    async fn test_cached_article_ids() {
        let db = test_db().await;
        let (_feed_id, ids) = setup_articles(&db, 3).await;

        // No cached articles yet
        let cached = db.cached_article_ids(&ids).await.unwrap();
        assert!(cached.is_empty());

        // Cache two articles
        db.cache_content(ids[0], "content 0", None).await.unwrap();
        db.cache_content(ids[2], "content 2", None).await.unwrap();

        let cached = db.cached_article_ids(&ids).await.unwrap();
        assert_eq!(cached.len(), 2);
        assert!(cached.contains(&ids[0]));
        assert!(cached.contains(&ids[2]));
        assert!(!cached.contains(&ids[1]));

        // Empty input
        let cached = db.cached_article_ids(&[]).await.unwrap();
        assert!(cached.is_empty());
    }

    #[tokio::test]
    async fn test_cascade_delete() {
        let db = test_db().await;
        let (_feed_id, ids) = setup_articles(&db, 1).await;

        db.cache_content(ids[0], "cached", None).await.unwrap();
        assert!(db.get_cached_content(ids[0]).await.unwrap().is_some());

        // Delete the feed (cascades to articles, which cascades to content_cache)
        sqlx::query("DELETE FROM feeds")
            .execute(&db.pool)
            .await
            .unwrap();

        let stats = db.cache_stats().await.unwrap();
        assert_eq!(stats.total_entries, 0);
    }
}
