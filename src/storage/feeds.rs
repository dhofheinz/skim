use anyhow::Result;
use sqlx::QueryBuilder;
use std::sync::Arc;

use super::schema::Database;
use super::types::{DatabaseError, Feed, FeedRow, OpmlFeed, ParsedArticle};

impl Database {
    // ========================================================================
    // Feed Operations
    // ========================================================================

    /// Sync feeds from OPML import (INSERT OR REPLACE)
    /// PERF-001: Batch INSERT in chunks of 100 for 10-50x performance on large OPML imports
    pub async fn sync_feeds(&self, feeds: &[OpmlFeed]) -> Result<()> {
        if feeds.is_empty() {
            return Ok(());
        }

        const BATCH_SIZE: usize = 100;
        let mut tx = self.pool.begin().await?;

        for chunk in feeds.chunks(BATCH_SIZE) {
            let mut builder: QueryBuilder<sqlx::Sqlite> =
                QueryBuilder::new("INSERT INTO feeds (title, url, html_url) ");

            builder.push_values(chunk, |mut b, feed| {
                b.push_bind(&feed.title)
                    .push_bind(&feed.xml_url)
                    .push_bind(&feed.html_url);
            });

            builder.push(
                " ON CONFLICT(url) DO UPDATE SET title = excluded.title, html_url = excluded.html_url",
            );

            builder.build().execute(&mut *tx).await?;
        }

        tx.commit().await?;
        Ok(())
    }

    /// Get all feeds with their unread article counts
    pub async fn get_feeds_with_unread_counts(&self) -> Result<Vec<Feed>> {
        let rows: Vec<FeedRow> = sqlx::query_as(
            r#"
                SELECT
                    f.id, f.title, f.url, f.html_url, f.last_fetched, f.error,
                    COUNT(CASE WHEN a.read = 0 THEN 1 END) as unread_count,
                    f.consecutive_failures
                FROM feeds f
                LEFT JOIN articles a ON f.id = a.feed_id
                GROUP BY f.id
                ORDER BY f.title
            "#,
        )
        .fetch_all(&self.pool)
        .await?;

        let feeds = rows
            .into_iter()
            .map(
                |(
                    id,
                    title,
                    url,
                    html_url,
                    last_fetched,
                    error,
                    unread_count,
                    consecutive_failures,
                )| Feed {
                    id,
                    title: Arc::from(title),
                    url,
                    html_url,
                    last_fetched,
                    error,
                    unread_count,
                    consecutive_failures,
                },
            )
            .collect();

        Ok(feeds)
    }

    /// Set or clear the error status for a feed
    pub async fn set_feed_error(&self, feed_id: i64, error: Option<&str>) -> Result<()> {
        sqlx::query("UPDATE feeds SET error = ? WHERE id = ?")
            .bind(error)
            .bind(feed_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Batch update feed error statuses in a single UPDATE statement.
    ///
    /// PERF-002: Uses a single bulk UPDATE with CASE expression instead of N
    /// individual UPDATE calls. For 100 feeds, this reduces database round-trips
    /// from ~100 to 1.
    ///
    /// # Arguments
    ///
    /// * `updates` - Slice of (feed_id, error_message) tuples. `None` clears the error.
    pub async fn batch_set_feed_errors(&self, updates: &[(i64, Option<String>)]) -> Result<()> {
        if updates.is_empty() {
            return Ok(());
        }

        // Build: UPDATE feeds SET error = CASE id
        //            WHEN 1 THEN 'error1'
        //            WHEN 2 THEN NULL
        //        END
        //        WHERE id IN (1, 2)
        let mut builder: QueryBuilder<sqlx::Sqlite> =
            QueryBuilder::new("UPDATE feeds SET error = CASE id ");

        for (feed_id, error) in updates {
            builder.push("WHEN ");
            builder.push_bind(*feed_id);
            builder.push(" THEN ");
            builder.push_bind(error.as_deref());
            builder.push(" ");
        }

        builder.push("END WHERE id IN (");
        let mut separated = builder.separated(", ");
        for (feed_id, _) in updates {
            separated.push_bind(*feed_id);
        }
        separated.push_unseparated(")");

        let mut tx = self.pool.begin().await?;
        builder.build().execute(&mut *tx).await?;
        tx.commit().await?;

        Ok(())
    }

    /// Update the last_fetched timestamp for a feed
    #[allow(dead_code)] // Kept for potential use outside of complete_feed_refresh
    pub async fn update_feed_fetched(&self, feed_id: i64) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        sqlx::query("UPDATE feeds SET last_fetched = ?, error = NULL WHERE id = ?")
            .bind(now)
            .bind(feed_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // ========================================================================
    // Circuit Breaker Operations
    // ========================================================================

    /// Threshold for consecutive failures before a feed is skipped
    pub const CIRCUIT_BREAKER_THRESHOLD: i64 = 5;

    /// Increment consecutive failure count for a feed.
    ///
    /// Called when a feed fetch fails. Returns the new failure count.
    /// When the count reaches [`CIRCUIT_BREAKER_THRESHOLD`], the feed will be
    /// skipped during bulk refresh operations until manually retried.
    pub async fn increment_feed_failures(&self, feed_id: i64) -> Result<i64, DatabaseError> {
        let result: (i64,) = sqlx::query_as(
            "UPDATE feeds SET consecutive_failures = consecutive_failures + 1
             WHERE id = ? RETURNING consecutive_failures",
        )
        .bind(feed_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(result.0)
    }

    /// Reset consecutive failure count for a feed.
    ///
    /// Called on successful fetch to clear the circuit breaker state.
    /// Note: Currently reset is done atomically within `complete_feed_refresh`,
    /// but this method is kept for potential future manual reset functionality.
    #[allow(dead_code)]
    pub async fn reset_feed_failures(&self, feed_id: i64) -> Result<(), DatabaseError> {
        sqlx::query("UPDATE feeds SET consecutive_failures = 0 WHERE id = ?")
            .bind(feed_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Get feeds that haven't exceeded the failure threshold.
    ///
    /// Returns feeds with `consecutive_failures < CIRCUIT_BREAKER_THRESHOLD`.
    /// Kept for potential admin UI listing of healthy feeds.
    #[allow(dead_code)]
    pub async fn get_active_feeds(&self) -> Result<Vec<Feed>, DatabaseError> {
        let rows: Vec<FeedRow> = sqlx::query_as(
            r#"
                SELECT
                    f.id, f.title, f.url, f.html_url, f.last_fetched, f.error,
                    COUNT(CASE WHEN a.read = 0 THEN 1 END) as unread_count,
                    f.consecutive_failures
                FROM feeds f
                LEFT JOIN articles a ON f.id = a.feed_id
                WHERE f.consecutive_failures < ?
                GROUP BY f.id
                ORDER BY f.title
            "#,
        )
        .bind(Self::CIRCUIT_BREAKER_THRESHOLD)
        .fetch_all(&self.pool)
        .await?;

        let feeds = rows
            .into_iter()
            .map(
                |(
                    id,
                    title,
                    url,
                    html_url,
                    last_fetched,
                    error,
                    unread_count,
                    consecutive_failures,
                )| Feed {
                    id,
                    title: Arc::from(title),
                    url,
                    html_url,
                    last_fetched,
                    error,
                    unread_count,
                    consecutive_failures,
                },
            )
            .collect();

        Ok(feeds)
    }

    /// Complete a feed refresh atomically: clear error, upsert articles, update timestamp.
    ///
    /// All operations are wrapped in a single transaction for data integrity.
    /// If any step fails, the entire transaction is rolled back, preventing
    /// inconsistent state where articles are stored but timestamp is stale.
    ///
    /// # Arguments
    ///
    /// * `feed_id` - The database ID of the feed being refreshed
    /// * `articles` - Parsed articles to upsert
    ///
    /// # Returns
    ///
    /// The number of newly inserted articles (not updated).
    ///
    /// # PERF-012
    ///
    /// Uses two-phase insert (INSERT OR IGNORE + UPDATE) with `changes()` instead of
    /// before/after COUNT queries. This eliminates 2 table scans per feed refresh.
    pub async fn complete_feed_refresh(
        &self,
        feed_id: i64,
        articles: &[ParsedArticle],
    ) -> Result<usize, DatabaseError> {
        let now = chrono::Utc::now().timestamp();
        let mut tx = self.pool.begin().await?;

        // Clear any previous error and reset circuit breaker
        sqlx::query("UPDATE feeds SET error = NULL, consecutive_failures = 0 WHERE id = ?")
            .bind(feed_id)
            .execute(&mut *tx)
            .await?;

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

        // Update fetched timestamp
        sqlx::query("UPDATE feeds SET last_fetched = ? WHERE id = ?")
            .bind(now)
            .bind(feed_id)
            .execute(&mut *tx)
            .await?;

        // FTS5 triggers (articles_fts_insert, articles_fts_update, articles_fts_delete) execute
        // within the same transaction as article inserts/updates/deletes.
        // SQLite guarantees atomicity: if commit succeeds, both articles and FTS are consistent.
        tx.commit().await?;

        // Verify FTS consistency after commit (defensive check against trigger failures)
        // This catches edge cases like disk full during trigger execution where SQLite
        // might succeed on main table but fail silently on FTS virtual table.
        let article_count: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM articles WHERE feed_id = ?")
                .bind(feed_id)
                .fetch_one(&self.pool)
                .await?;

        let fts_count: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM articles_fts WHERE rowid IN (SELECT id FROM articles WHERE feed_id = ?)",
        )
        .bind(feed_id)
        .fetch_one(&self.pool)
        .await?;

        if article_count.0 != fts_count.0 {
            tracing::warn!(
                feed_id = feed_id,
                articles = article_count.0,
                fts_entries = fts_count.0,
                "FTS index may be inconsistent after refresh, consider --rebuild-search"
            );
        }

        Ok(total_inserted)
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
    async fn test_sync_feeds_insert() {
        let db = test_db().await;
        db.sync_feeds(&[test_feed(1)]).await.unwrap();

        let feeds = db.get_feeds_with_unread_counts().await.unwrap();
        assert_eq!(feeds.len(), 1);
        assert_eq!(&*feeds[0].title, "Test Feed 1");
        assert_eq!(feeds[0].unread_count, 0);
    }

    #[tokio::test]
    async fn test_sync_feeds_upsert_updates_title() {
        let db = test_db().await;
        db.sync_feeds(&[test_feed(1)]).await.unwrap();

        let updated = OpmlFeed {
            title: "Updated Title".to_string(),
            xml_url: "https://feed1.example.com/rss".to_string(),
            html_url: Some("https://feed1.example.com".to_string()),
        };
        db.sync_feeds(&[updated]).await.unwrap();

        let feeds = db.get_feeds_with_unread_counts().await.unwrap();
        assert_eq!(feeds.len(), 1);
        assert_eq!(&*feeds[0].title, "Updated Title");
    }

    #[tokio::test]
    async fn test_sync_feeds_empty() {
        let db = test_db().await;
        db.sync_feeds(&[]).await.unwrap();
        let feeds = db.get_feeds_with_unread_counts().await.unwrap();
        assert!(feeds.is_empty());
    }

    #[tokio::test]
    async fn test_sync_feeds_batch_chunking() {
        let db = test_db().await;

        let feeds: Vec<OpmlFeed> = (0..250).map(test_feed).collect();
        db.sync_feeds(&feeds).await.unwrap();

        let result = db.get_feeds_with_unread_counts().await.unwrap();
        assert_eq!(result.len(), 250);

        assert!(result.iter().any(|f| &*f.title == "Test Feed 0"));
        assert!(result.iter().any(|f| &*f.title == "Test Feed 249"));
    }

    #[tokio::test]
    async fn test_sync_feeds_batch_upsert() {
        let db = test_db().await;

        let feeds: Vec<OpmlFeed> = (0..150).map(test_feed).collect();
        db.sync_feeds(&feeds).await.unwrap();

        let mut updated_feeds: Vec<OpmlFeed> = (100..200)
            .map(|i| OpmlFeed {
                title: format!("Updated Feed {}", i),
                xml_url: format!("https://feed{}.example.com/rss", i),
                html_url: Some(format!("https://feed{}.example.com", i)),
            })
            .collect();
        updated_feeds.extend((0..50).map(test_feed));

        db.sync_feeds(&updated_feeds).await.unwrap();

        let result = db.get_feeds_with_unread_counts().await.unwrap();
        assert_eq!(result.len(), 200);

        let feed_120 = result
            .iter()
            .find(|f| f.url == "https://feed120.example.com/rss");
        assert!(feed_120.is_some());
        assert_eq!(&*feed_120.unwrap().title, "Updated Feed 120");
        assert_eq!(
            feed_120.unwrap().html_url,
            Some("https://feed120.example.com".to_string())
        );
    }

    #[tokio::test]
    async fn test_get_feeds_unread_counts() {
        let db = test_db().await;
        db.sync_feeds(&[test_feed(1)]).await.unwrap();
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

        let feeds = db.get_feeds_with_unread_counts().await.unwrap();
        assert_eq!(feeds[0].unread_count, 3);

        let articles = db.get_articles_for_feed(feeds[0].id, None).await.unwrap();
        db.mark_article_read(articles[0].id).await.unwrap();

        let feeds = db.get_feeds_with_unread_counts().await.unwrap();
        assert_eq!(feeds[0].unread_count, 2);
    }

    #[tokio::test]
    async fn test_batch_set_feed_errors() {
        let db = test_db().await;

        db.sync_feeds(&[test_feed(1), test_feed(2), test_feed(3)])
            .await
            .unwrap();

        let feeds = db.get_feeds_with_unread_counts().await.unwrap();
        assert_eq!(feeds.len(), 3);

        let updates = vec![
            (feeds[0].id, Some("Network error".to_string())),
            (feeds[1].id, None),
            (feeds[2].id, Some("Parse error".to_string())),
        ];

        db.batch_set_feed_errors(&updates).await.unwrap();

        let feeds = db.get_feeds_with_unread_counts().await.unwrap();
        assert_eq!(feeds[0].error, Some("Network error".to_string()));
        assert_eq!(feeds[1].error, None);
        assert_eq!(feeds[2].error, Some("Parse error".to_string()));
    }

    #[tokio::test]
    async fn test_batch_set_feed_errors_empty() {
        let db = test_db().await;
        db.batch_set_feed_errors(&[]).await.unwrap();
    }

    #[tokio::test]
    async fn test_complete_feed_refresh_atomic() {
        let db = test_db().await;
        db.sync_feeds(&[test_feed(1)]).await.unwrap();
        let feeds = db.get_feeds_with_unread_counts().await.unwrap();
        let feed_id = feeds[0].id;

        db.set_feed_error(feed_id, Some("Previous error"))
            .await
            .unwrap();

        let articles = vec![
            test_article("guid-1", "Article 1"),
            test_article("guid-2", "Article 2"),
        ];
        let count = db.complete_feed_refresh(feed_id, &articles).await.unwrap();
        assert_eq!(count, 2);

        let feeds = db.get_feeds_with_unread_counts().await.unwrap();
        assert!(feeds[0].error.is_none(), "Error should be cleared");
        assert!(
            feeds[0].last_fetched.is_some(),
            "last_fetched should be set"
        );
        assert_eq!(feeds[0].unread_count, 2);

        let stored = db.get_articles_for_feed(feed_id, None).await.unwrap();
        assert_eq!(stored.len(), 2);
    }

    #[tokio::test]
    async fn test_complete_feed_refresh_empty_articles() {
        let db = test_db().await;
        db.sync_feeds(&[test_feed(1)]).await.unwrap();
        let feeds = db.get_feeds_with_unread_counts().await.unwrap();
        let feed_id = feeds[0].id;

        let count = db.complete_feed_refresh(feed_id, &[]).await.unwrap();
        assert_eq!(count, 0);

        let feeds = db.get_feeds_with_unread_counts().await.unwrap();
        assert!(
            feeds[0].last_fetched.is_some(),
            "last_fetched should be set even with no articles"
        );
    }

    #[tokio::test]
    async fn test_complete_feed_refresh_upsert() {
        let db = test_db().await;
        db.sync_feeds(&[test_feed(1)]).await.unwrap();
        let feeds = db.get_feeds_with_unread_counts().await.unwrap();
        let feed_id = feeds[0].id;

        let count = db
            .complete_feed_refresh(feed_id, &[test_article("existing", "Original")])
            .await
            .unwrap();
        assert_eq!(count, 1);

        let articles = db.get_articles_for_feed(feed_id, None).await.unwrap();
        db.mark_article_read(articles[0].id).await.unwrap();

        let articles = vec![
            test_article("existing", "Updated Title"),
            test_article("new-1", "New Article"),
        ];
        let count = db.complete_feed_refresh(feed_id, &articles).await.unwrap();
        assert_eq!(count, 1);

        let stored = db.get_articles_for_feed(feed_id, None).await.unwrap();
        let existing = stored.iter().find(|a| a.guid == "existing").unwrap();
        assert!(existing.read, "Read status should be preserved");
        assert_eq!(&*existing.title, "Updated Title");
    }

    // ========================================================================
    // Circuit Breaker Tests
    // ========================================================================

    #[tokio::test]
    async fn test_increment_feed_failures() {
        let db = test_db().await;
        db.sync_feeds(&[test_feed(1)]).await.unwrap();
        let feeds = db.get_feeds_with_unread_counts().await.unwrap();
        let feed_id = feeds[0].id;

        assert_eq!(feeds[0].consecutive_failures, 0);

        let count = db.increment_feed_failures(feed_id).await.unwrap();
        assert_eq!(count, 1);

        let count = db.increment_feed_failures(feed_id).await.unwrap();
        assert_eq!(count, 2);

        let feeds = db.get_feeds_with_unread_counts().await.unwrap();
        assert_eq!(feeds[0].consecutive_failures, 2);
    }

    #[tokio::test]
    async fn test_reset_feed_failures() {
        let db = test_db().await;
        db.sync_feeds(&[test_feed(1)]).await.unwrap();
        let feeds = db.get_feeds_with_unread_counts().await.unwrap();
        let feed_id = feeds[0].id;

        for _ in 0..3 {
            db.increment_feed_failures(feed_id).await.unwrap();
        }

        db.reset_feed_failures(feed_id).await.unwrap();

        let feeds = db.get_feeds_with_unread_counts().await.unwrap();
        assert_eq!(feeds[0].consecutive_failures, 0);
    }

    #[tokio::test]
    async fn test_get_active_feeds_filters_high_failures() {
        let db = test_db().await;
        db.sync_feeds(&[test_feed(1), test_feed(2), test_feed(3)])
            .await
            .unwrap();
        let feeds = db.get_feeds_with_unread_counts().await.unwrap();

        for _ in 0..Database::CIRCUIT_BREAKER_THRESHOLD {
            db.increment_feed_failures(feeds[1].id).await.unwrap();
        }

        for _ in 0..(Database::CIRCUIT_BREAKER_THRESHOLD - 1) {
            db.increment_feed_failures(feeds[2].id).await.unwrap();
        }

        let active = db.get_active_feeds().await.unwrap();
        assert_eq!(active.len(), 2, "Should exclude feed with 5 failures");

        assert!(
            !active.iter().any(|f| f.id == feeds[1].id),
            "Feed with threshold failures should be excluded"
        );
    }

    #[tokio::test]
    async fn test_complete_feed_refresh_resets_failures() {
        let db = test_db().await;
        db.sync_feeds(&[test_feed(1)]).await.unwrap();
        let feeds = db.get_feeds_with_unread_counts().await.unwrap();
        let feed_id = feeds[0].id;

        for _ in 0..4 {
            db.increment_feed_failures(feed_id).await.unwrap();
        }

        db.complete_feed_refresh(feed_id, &[test_article("1", "Test")])
            .await
            .unwrap();

        let feeds = db.get_feeds_with_unread_counts().await.unwrap();
        assert_eq!(
            feeds[0].consecutive_failures, 0,
            "Successful refresh should reset failure count"
        );
    }

    #[tokio::test]
    async fn test_circuit_breaker_threshold_constant() {
        assert_eq!(
            Database::CIRCUIT_BREAKER_THRESHOLD,
            5,
            "Circuit breaker threshold should be 5"
        );
    }
}
