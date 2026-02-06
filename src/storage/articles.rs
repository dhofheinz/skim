use anyhow::Result;
use sqlx::QueryBuilder;

use super::schema::Database;
use super::types::{Article, ArticleDbRow, ArticleRow, ParsedArticle};

// ============================================================================
// Query Limit Constants
// ============================================================================

/// Maximum number of articles to return from any single query (OOM protection)
const MAX_ARTICLES: i64 = 2000;

/// Maximum limit for batch article queries like get_recent_articles_for_feeds
const MAX_BATCH_LIMIT: usize = 10000;

impl Database {
    // ========================================================================
    // Article Operations
    // ========================================================================

    /// Upsert articles for a feed, returns the number of new articles inserted
    ///
    /// PERF-003: Uses INSERT ... ON CONFLICT DO UPDATE (UPSERT) for efficient handling
    /// of both new and existing articles in a single pass.
    /// Batch size of 50 keeps us well under SQLite's 999 parameter limit (7 columns * 50 = 350).
    ///
    /// Preserves user state (read, starred, content, fetched_at) for existing articles
    /// while updating metadata (title, url, published, summary) from the feed.
    ///
    /// # PERF-012
    ///
    /// Uses two-phase insert (INSERT OR IGNORE + UPDATE) with `changes()` instead of
    /// before/after COUNT queries. This eliminates 2 table scans per upsert operation.
    #[allow(dead_code)] // Kept for potential use outside of complete_feed_refresh
    pub async fn upsert_articles(&self, feed_id: i64, articles: &[ParsedArticle]) -> Result<usize> {
        if articles.is_empty() {
            return Ok(0);
        }

        let now = chrono::Utc::now().timestamp();
        let mut tx = self.pool.begin().await?;

        // PERF-012: Two-phase insert to accurately count new articles using changes()
        // Phase 1: INSERT OR IGNORE to insert only new articles, track count via changes()
        // Phase 2: UPDATE to refresh metadata for all articles (new and existing)
        const BATCH_SIZE: usize = 50;
        let mut total_inserted: usize = 0;

        for chunk in articles.chunks(BATCH_SIZE) {
            // Phase 1: Insert new articles only (INSERT OR IGNORE)
            let mut insert_builder: QueryBuilder<sqlx::Sqlite> = QueryBuilder::new(
                "INSERT OR IGNORE INTO articles (feed_id, guid, title, url, published, summary, fetched_at) ",
            );

            insert_builder.push_values(chunk, |mut b, article| {
                b.push_bind(feed_id)
                    .push_bind(&article.guid)
                    .push_bind(&article.title)
                    .push_bind(&article.url)
                    .push_bind(article.published)
                    .push_bind(&article.summary)
                    .push_bind(now);
            });

            insert_builder.build().execute(&mut *tx).await?;

            // PERF-012: Use changes() to count inserted rows (no table scan)
            let changes: (i64,) = sqlx::query_as("SELECT changes()")
                .fetch_one(&mut *tx)
                .await?;
            total_inserted += changes.0 as usize;

            // Phase 2: Update metadata for existing articles (preserves user state)
            // - fetched_at is NOT updated to preserve "first seen" timestamp for What's New ordering
            let mut update_builder: QueryBuilder<sqlx::Sqlite> = QueryBuilder::new(
                "UPDATE articles SET \
                 title = CASE guid ",
            );

            // Build CASE expressions for each field
            for article in chunk {
                update_builder.push("WHEN ");
                update_builder.push_bind(&article.guid);
                update_builder.push(" THEN ");
                update_builder.push_bind(&article.title);
                update_builder.push(" ");
            }
            update_builder.push("ELSE title END, url = CASE guid ");
            for article in chunk {
                update_builder.push("WHEN ");
                update_builder.push_bind(&article.guid);
                update_builder.push(" THEN ");
                update_builder.push_bind(&article.url);
                update_builder.push(" ");
            }
            update_builder.push("ELSE url END, published = CASE guid ");
            for article in chunk {
                update_builder.push("WHEN ");
                update_builder.push_bind(&article.guid);
                update_builder.push(" THEN ");
                update_builder.push_bind(article.published);
                update_builder.push(" ");
            }
            update_builder.push("ELSE published END, summary = CASE guid ");
            for article in chunk {
                update_builder.push("WHEN ");
                update_builder.push_bind(&article.guid);
                update_builder.push(" THEN ");
                update_builder.push_bind(&article.summary);
                update_builder.push(" ");
            }
            update_builder.push("ELSE summary END WHERE feed_id = ");
            update_builder.push_bind(feed_id);
            update_builder.push(" AND guid IN (");

            let mut separated = update_builder.separated(", ");
            for article in chunk {
                separated.push_bind(&article.guid);
            }
            separated.push_unseparated(")");

            update_builder.build().execute(&mut *tx).await?;
        }

        tx.commit().await?;
        Ok(total_inserted)
    }

    // ========================================================================
    // Article Queries
    // ========================================================================

    /// Get articles for a specific feed with optional pagination limit
    /// EDGE-003: Add optional limit for pagination (default 500)
    /// PERF-003: Hard cap at MAX_ARTICLES (2000) to prevent OOM
    pub async fn get_articles_for_feed(
        &self,
        feed_id: i64,
        limit: Option<i64>,
    ) -> Result<Vec<Article>> {
        let limit = limit.unwrap_or(500).min(MAX_ARTICLES);
        tracing::debug!(
            limit = limit,
            feed_id = feed_id,
            "get_articles_for_feed with limit cap"
        );

        let rows = sqlx::query_as::<_, ArticleDbRow>(
            r#"
            SELECT id, feed_id, guid, title, url, published, summary, content,
                   read, starred, fetched_at
            FROM articles
            WHERE feed_id = ?
            ORDER BY published DESC, fetched_at DESC
            LIMIT ?
        "#,
        )
        .bind(feed_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.into_iter().map(ArticleDbRow::into_article).collect())
    }

    /// Get a single article by its ID.
    ///
    /// Used by What's New panel navigation when the user selects an entry
    /// and needs the full Article for the reader view.
    pub async fn get_article_by_id(&self, article_id: i64) -> Result<Option<Article>> {
        let row = sqlx::query_as::<_, ArticleDbRow>(
            r#"
            SELECT id, feed_id, guid, title, url, published, summary, content,
                   read, starred, fetched_at
            FROM articles
            WHERE id = ?
        "#,
        )
        .bind(article_id)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(ArticleDbRow::into_article))
    }

    /// Returns all starred articles across all feeds.
    ///
    /// # Keybind
    ///
    /// `S` (Shift+S) to view all starred articles in Browse view.
    ///
    /// # Returns
    ///
    /// A vector of all articles where `starred = true`, ordered by publication
    /// date (most recent first). Limited to MAX_ARTICLES (2000) to prevent OOM.
    ///
    /// # PERF-003
    ///
    /// Hard cap at MAX_ARTICLES to prevent unbounded memory allocation.
    pub async fn get_starred_articles(&self) -> Result<Vec<Article>> {
        tracing::debug!(limit = MAX_ARTICLES, "get_starred_articles with limit cap");
        let rows = sqlx::query_as::<_, ArticleDbRow>(
            r#"
            SELECT id, feed_id, guid, title, url, published, summary, content,
                   read, starred, fetched_at
            FROM articles
            WHERE starred = 1
            ORDER BY published DESC, fetched_at DESC
            LIMIT ?
        "#,
        )
        .bind(MAX_ARTICLES)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.into_iter().map(ArticleDbRow::into_article).collect())
    }

    // ========================================================================
    // Article Mutations
    // ========================================================================

    /// Mark article as read (idempotent), returns whether it was changed
    ///
    /// Uses `WHERE read = 0` to make the operation idempotent and efficient.
    /// The article is only updated if it was previously unread, avoiding
    /// unnecessary writes and race conditions.
    pub async fn mark_article_read(&self, article_id: i64) -> Result<bool> {
        let result = sqlx::query("UPDATE articles SET read = 1 WHERE id = ? AND read = 0")
            .bind(article_id)
            .execute(&self.pool)
            .await?;

        Ok(result.rows_affected() > 0)
    }

    /// Atomically toggle starred status, returning the new value
    ///
    /// Uses SQLite's RETURNING clause to perform the toggle and get the
    /// new value in a single atomic operation, preventing TOCTOU races.
    pub async fn toggle_article_starred(&self, article_id: i64) -> Result<bool> {
        let result: (bool,) = sqlx::query_as(
            r#"UPDATE articles SET starred = NOT starred WHERE id = ? RETURNING starred"#,
        )
        .bind(article_id)
        .fetch_one(&self.pool)
        .await?;

        Ok(result.0)
    }

    /// Mark all articles as read for a specific feed, returns count of articles marked
    ///
    /// Uses `WHERE read = 0` to make the operation idempotent. Only unread articles
    /// are updated, avoiding unnecessary writes when called repeatedly.
    #[allow(dead_code)] // Kept for future UI integration
    pub async fn mark_all_read_for_feed(&self, feed_id: i64) -> Result<u64> {
        let result = sqlx::query("UPDATE articles SET read = 1 WHERE feed_id = ? AND read = 0")
            .bind(feed_id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected())
    }

    /// Mark all articles as read across all feeds, returns count of articles marked
    ///
    /// Uses `WHERE read = 0` to make the operation idempotent. Only unread articles
    /// are updated, avoiding unnecessary writes when called repeatedly.
    #[allow(dead_code)] // Kept for future UI integration
    pub async fn mark_all_read(&self) -> Result<u64> {
        let result = sqlx::query("UPDATE articles SET read = 1 WHERE read = 0")
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected())
    }

    // ========================================================================
    // Content Caching
    // ========================================================================

    /// Retrieves the cached full content of an article.
    ///
    /// Used by the content loader to check for cached jina.ai Reader API responses
    /// before making network requests. Returns `None` if no content has been cached yet.
    ///
    /// # Arguments
    ///
    /// * `article_id` - The database ID of the article
    ///
    /// # Returns
    ///
    /// The cached content if available, or `None` if not yet fetched.
    pub async fn get_article_content(&self, article_id: i64) -> Result<Option<String>> {
        let row: Option<(Option<String>,)> =
            sqlx::query_as("SELECT content FROM articles WHERE id = ?")
                .bind(article_id)
                .fetch_optional(&self.pool)
                .await?;

        Ok(row.and_then(|(content,)| content))
    }

    /// Stores the full content of an article for caching.
    ///
    /// Called after successful jina.ai Reader API fetch to persist content
    /// for offline access and faster subsequent loads.
    ///
    /// # Arguments
    ///
    /// * `article_id` - The database ID of the article
    /// * `content` - The full article content (markdown from jina.ai)
    pub async fn set_article_content(&self, article_id: i64, content: &str) -> Result<()> {
        sqlx::query("UPDATE articles SET content = ? WHERE id = ?")
            .bind(content)
            .bind(article_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // ========================================================================
    // Batch Article Operations
    // ========================================================================

    /// PERF-001 + BUG-005: Get recent unread articles from multiple feeds in one query
    /// BUG-005: Safe limit cap at MAX_BATCH_LIMIT (10000) to prevent overflow
    pub async fn get_recent_articles_for_feeds(
        &self,
        feed_ids: &[i64],
        limit: usize,
    ) -> Result<Vec<(i64, Article)>> {
        if feed_ids.is_empty() {
            return Ok(Vec::new());
        }

        // BUG-005: Cap limit to prevent integer overflow on cast and excessive memory use
        // Use try_into with fallback for safe conversion
        let safe_limit: i64 = limit.min(MAX_BATCH_LIMIT).try_into().unwrap_or(i64::MAX);
        tracing::debug!(
            requested_limit = limit,
            safe_limit = safe_limit,
            "get_recent_articles_for_feeds with limit cap"
        );

        let mut builder: QueryBuilder<sqlx::Sqlite> = QueryBuilder::new(
            r#"SELECT feed_id, id, guid, title, url, published, summary, content, read, starred, fetched_at
               FROM articles WHERE read = 0 AND feed_id IN ("#,
        );

        let mut separated = builder.separated(", ");
        for id in feed_ids {
            separated.push_bind(*id);
        }
        separated.push_unseparated(") ORDER BY published DESC LIMIT ");
        builder.push_bind(safe_limit);

        let rows: Vec<ArticleRow> = builder.build_query_as().fetch_all(&self.pool).await?;
        Ok(rows.into_iter().map(ArticleRow::into_tuple).collect())
    }
}

#[cfg(test)]
mod tests {
    use crate::storage::{Database, OpmlFeed, ParsedArticle};

    async fn test_db() -> Database {
        Database::open(":memory:").await.unwrap()
    }

    fn test_feed(id: i64) -> OpmlFeed {
        OpmlFeed {
            title: format!("Test Feed {}", id),
            xml_url: format!("https://feed{}.example.com/rss", id),
            html_url: None,
        }
    }

    fn test_article(guid: &str, title: &str) -> ParsedArticle {
        ParsedArticle {
            guid: guid.to_string(),
            title: title.to_string(),
            url: Some(format!("https://example.com/{}", guid)),
            published: Some(1704067200),
            summary: Some("Test summary".to_string()),
        }
    }

    #[tokio::test]
    async fn test_upsert_articles_insert() {
        let db = test_db().await;
        db.sync_feeds(&[test_feed(1)]).await.unwrap();
        let feeds = db.get_feeds_with_unread_counts().await.unwrap();

        let count = db
            .upsert_articles(
                feeds[0].id,
                &[
                    test_article("guid-1", "Article 1"),
                    test_article("guid-2", "Article 2"),
                ],
            )
            .await
            .unwrap();

        assert_eq!(count, 2);
    }

    #[tokio::test]
    async fn test_upsert_articles_update_returns_zero() {
        let db = test_db().await;
        db.sync_feeds(&[test_feed(1)]).await.unwrap();
        let feeds = db.get_feeds_with_unread_counts().await.unwrap();

        db.upsert_articles(feeds[0].id, &[test_article("guid-1", "Original")])
            .await
            .unwrap();

        let count = db
            .upsert_articles(feeds[0].id, &[test_article("guid-1", "Updated")])
            .await
            .unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn test_upsert_articles_updates_metadata() {
        let db = test_db().await;
        db.sync_feeds(&[test_feed(1)]).await.unwrap();
        let feeds = db.get_feeds_with_unread_counts().await.unwrap();

        db.upsert_articles(feeds[0].id, &[test_article("guid-1", "Original Title")])
            .await
            .unwrap();

        let articles = db.get_articles_for_feed(feeds[0].id, None).await.unwrap();
        db.mark_article_read(articles[0].id).await.unwrap();
        db.toggle_article_starred(articles[0].id).await.unwrap();

        let updated = ParsedArticle {
            guid: "guid-1".to_string(),
            title: "Updated Title".to_string(),
            url: Some("https://example.com/updated".to_string()),
            published: Some(1704153600),
            summary: Some("Updated summary".to_string()),
        };
        db.upsert_articles(feeds[0].id, &[updated]).await.unwrap();

        let articles = db.get_articles_for_feed(feeds[0].id, None).await.unwrap();
        assert_eq!(articles.len(), 1);
        assert_eq!(&*articles[0].title, "Updated Title");
        assert_eq!(
            articles[0].url.as_deref(),
            Some("https://example.com/updated")
        );
        assert_eq!(articles[0].summary.as_deref(), Some("Updated summary"));
        assert!(articles[0].read, "read status should be preserved");
        assert!(articles[0].starred, "starred status should be preserved");
    }

    #[tokio::test]
    async fn test_upsert_articles_mixed_batch() {
        let db = test_db().await;
        db.sync_feeds(&[test_feed(1)]).await.unwrap();
        let feeds = db.get_feeds_with_unread_counts().await.unwrap();

        db.upsert_articles(feeds[0].id, &[test_article("existing", "Existing")])
            .await
            .unwrap();

        let count = db
            .upsert_articles(
                feeds[0].id,
                &[
                    test_article("existing", "Updated"),
                    test_article("new-1", "New 1"),
                    test_article("new-2", "New 2"),
                ],
            )
            .await
            .unwrap();

        assert_eq!(count, 2);
    }

    #[tokio::test]
    async fn test_upsert_articles_empty_batch() {
        let db = test_db().await;
        db.sync_feeds(&[test_feed(1)]).await.unwrap();
        let feeds = db.get_feeds_with_unread_counts().await.unwrap();

        let count = db.upsert_articles(feeds[0].id, &[]).await.unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn test_mark_article_read() {
        let db = test_db().await;
        db.sync_feeds(&[test_feed(1)]).await.unwrap();
        let feeds = db.get_feeds_with_unread_counts().await.unwrap();
        db.upsert_articles(feeds[0].id, &[test_article("1", "Test")])
            .await
            .unwrap();

        let articles = db.get_articles_for_feed(feeds[0].id, None).await.unwrap();
        assert!(!articles[0].read);

        let changed = db.mark_article_read(articles[0].id).await.unwrap();
        assert!(changed);

        let articles = db.get_articles_for_feed(feeds[0].id, None).await.unwrap();
        assert!(articles[0].read);

        let changed = db.mark_article_read(articles[0].id).await.unwrap();
        assert!(!changed);
    }

    #[tokio::test]
    async fn test_toggle_article_starred() {
        let db = test_db().await;
        db.sync_feeds(&[test_feed(1)]).await.unwrap();
        let feeds = db.get_feeds_with_unread_counts().await.unwrap();
        db.upsert_articles(feeds[0].id, &[test_article("1", "Test")])
            .await
            .unwrap();

        let articles = db.get_articles_for_feed(feeds[0].id, None).await.unwrap();
        assert!(!articles[0].starred);

        let new_status = db.toggle_article_starred(articles[0].id).await.unwrap();
        assert!(new_status);

        let articles = db.get_articles_for_feed(feeds[0].id, None).await.unwrap();
        assert!(articles[0].starred);
    }

    #[tokio::test]
    async fn test_toggle_article_starred_twice() {
        let db = test_db().await;
        db.sync_feeds(&[test_feed(1)]).await.unwrap();
        let feeds = db.get_feeds_with_unread_counts().await.unwrap();
        db.upsert_articles(feeds[0].id, &[test_article("1", "Test")])
            .await
            .unwrap();

        let articles = db.get_articles_for_feed(feeds[0].id, None).await.unwrap();
        let initial = articles[0].starred;

        let first = db.toggle_article_starred(articles[0].id).await.unwrap();
        assert_eq!(first, !initial);

        let second = db.toggle_article_starred(articles[0].id).await.unwrap();
        assert_eq!(second, initial);

        let articles = db.get_articles_for_feed(feeds[0].id, None).await.unwrap();
        assert_eq!(articles[0].starred, initial);
    }

    #[tokio::test]
    async fn test_get_articles_pagination() {
        let db = test_db().await;
        db.sync_feeds(&[test_feed(1)]).await.unwrap();
        let feeds = db.get_feeds_with_unread_counts().await.unwrap();

        let articles: Vec<_> = (0..10)
            .map(|i| test_article(&format!("guid-{}", i), &format!("Article {}", i)))
            .collect();
        db.upsert_articles(feeds[0].id, &articles).await.unwrap();

        let limited = db
            .get_articles_for_feed(feeds[0].id, Some(5))
            .await
            .unwrap();
        assert_eq!(limited.len(), 5);
    }

    #[tokio::test]
    async fn test_mark_all_read_for_feed() {
        let db = test_db().await;
        db.sync_feeds(&[test_feed(1), test_feed(2)]).await.unwrap();
        let feeds = db.get_feeds_with_unread_counts().await.unwrap();

        db.upsert_articles(
            feeds[0].id,
            &[
                test_article("1", "Article 1"),
                test_article("2", "Article 2"),
                test_article("3", "Article 3"),
            ],
        )
        .await
        .unwrap();

        db.upsert_articles(feeds[1].id, &[test_article("4", "Article 4")])
            .await
            .unwrap();

        let articles = db.get_articles_for_feed(feeds[0].id, None).await.unwrap();
        db.mark_article_read(articles[0].id).await.unwrap();

        let count = db.mark_all_read_for_feed(feeds[0].id).await.unwrap();
        assert_eq!(count, 2);

        let articles = db.get_articles_for_feed(feeds[0].id, None).await.unwrap();
        assert!(articles.iter().all(|a| a.read));

        let articles = db.get_articles_for_feed(feeds[1].id, None).await.unwrap();
        assert!(articles.iter().all(|a| !a.read));
    }

    #[tokio::test]
    async fn test_mark_all_read_for_feed_idempotent() {
        let db = test_db().await;
        db.sync_feeds(&[test_feed(1)]).await.unwrap();
        let feeds = db.get_feeds_with_unread_counts().await.unwrap();

        db.upsert_articles(
            feeds[0].id,
            &[
                test_article("1", "Article 1"),
                test_article("2", "Article 2"),
            ],
        )
        .await
        .unwrap();

        let count = db.mark_all_read_for_feed(feeds[0].id).await.unwrap();
        assert_eq!(count, 2);

        let count = db.mark_all_read_for_feed(feeds[0].id).await.unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn test_mark_all_read() {
        let db = test_db().await;
        db.sync_feeds(&[test_feed(1), test_feed(2)]).await.unwrap();
        let feeds = db.get_feeds_with_unread_counts().await.unwrap();

        db.upsert_articles(
            feeds[0].id,
            &[
                test_article("1", "Article 1"),
                test_article("2", "Article 2"),
            ],
        )
        .await
        .unwrap();

        db.upsert_articles(
            feeds[1].id,
            &[
                test_article("3", "Article 3"),
                test_article("4", "Article 4"),
            ],
        )
        .await
        .unwrap();

        let count = db.mark_all_read().await.unwrap();
        assert_eq!(count, 4);

        let articles = db.get_articles_for_feed(feeds[0].id, None).await.unwrap();
        assert!(articles.iter().all(|a| a.read));

        let articles = db.get_articles_for_feed(feeds[1].id, None).await.unwrap();
        assert!(articles.iter().all(|a| a.read));
    }

    #[tokio::test]
    async fn test_mark_all_read_idempotent() {
        let db = test_db().await;
        db.sync_feeds(&[test_feed(1)]).await.unwrap();
        let feeds = db.get_feeds_with_unread_counts().await.unwrap();

        db.upsert_articles(feeds[0].id, &[test_article("1", "Article 1")])
            .await
            .unwrap();

        let count = db.mark_all_read().await.unwrap();
        assert_eq!(count, 1);

        let count = db.mark_all_read().await.unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn test_mark_all_read_for_feed_empty() {
        let db = test_db().await;
        db.sync_feeds(&[test_feed(1)]).await.unwrap();
        let feeds = db.get_feeds_with_unread_counts().await.unwrap();

        // Feed has zero articles — should be a no-op
        let count = db.mark_all_read_for_feed(feeds[0].id).await.unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn test_unread_counts_after_bulk_mark_read() {
        let db = test_db().await;
        db.sync_feeds(&[test_feed(1), test_feed(2)]).await.unwrap();
        let feeds = db.get_feeds_with_unread_counts().await.unwrap();

        db.upsert_articles(
            feeds[0].id,
            &[
                test_article("1", "Article 1"),
                test_article("2", "Article 2"),
                test_article("3", "Article 3"),
            ],
        )
        .await
        .unwrap();

        db.upsert_articles(
            feeds[1].id,
            &[
                test_article("4", "Article 4"),
                test_article("5", "Article 5"),
            ],
        )
        .await
        .unwrap();

        // Verify initial unread counts
        let feeds = db.get_feeds_with_unread_counts().await.unwrap();
        let feed1 = feeds.iter().find(|f| &*f.title == "Test Feed 1").unwrap();
        let feed2 = feeds.iter().find(|f| &*f.title == "Test Feed 2").unwrap();
        assert_eq!(feed1.unread_count, 3);
        assert_eq!(feed2.unread_count, 2);

        // Mark feed 1 as read
        db.mark_all_read_for_feed(feed1.id).await.unwrap();

        // Verify counts: feed 1 should be 0, feed 2 unchanged
        let feeds = db.get_feeds_with_unread_counts().await.unwrap();
        let feed1 = feeds.iter().find(|f| &*f.title == "Test Feed 1").unwrap();
        let feed2 = feeds.iter().find(|f| &*f.title == "Test Feed 2").unwrap();
        assert_eq!(feed1.unread_count, 0);
        assert_eq!(feed2.unread_count, 2);

        // Mark all as read
        db.mark_all_read().await.unwrap();

        // Verify all counts are 0
        let feeds = db.get_feeds_with_unread_counts().await.unwrap();
        assert!(feeds.iter().all(|f| f.unread_count == 0));
    }
}
