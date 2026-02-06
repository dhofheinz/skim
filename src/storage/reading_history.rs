use std::borrow::Cow;

use anyhow::Result;

use super::schema::Database;
use super::types::{ReadingHistoryEntry, ReadingStats};
use crate::util::strip_control_chars;

impl Database {
    // ========================================================================
    // Reading History Operations
    // ========================================================================

    /// Record that the user opened an article for reading.
    ///
    /// Inserts a new reading history row with `opened_at = datetime('now')`.
    /// Returns the new row's ID for later use with `record_close`.
    pub async fn record_open(&self, article_id: i64, feed_id: i64) -> Result<i64> {
        let row: (i64,) = sqlx::query_as(
            r#"
            INSERT INTO reading_history (article_id, feed_id, opened_at)
            VALUES (?, ?, datetime('now'))
            RETURNING id
        "#,
        )
        .bind(article_id)
        .bind(feed_id)
        .fetch_one(&self.pool)
        .await?;

        Ok(row.0)
    }

    /// Record that the user closed the reader, completing a reading session.
    ///
    /// Updates `closed_at = datetime('now')` and stores the elapsed duration.
    /// `duration_seconds` remains NULL if this is never called (app crash, force quit).
    pub async fn record_close(&self, history_id: i64, duration_seconds: i64) -> Result<()> {
        // SEC: Clamp to non-negative to prevent unsigned wraparound in stats aggregation
        let duration_seconds = duration_seconds.max(0);
        sqlx::query(
            r#"
            UPDATE reading_history
            SET closed_at = datetime('now'), duration_seconds = ?
            WHERE id = ?
        "#,
        )
        .bind(duration_seconds)
        .bind(history_id)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Compute aggregated reading statistics over the last N days.
    ///
    /// Returns articles per day, total reading time, and top feeds by read count.
    /// Used by TASK-8 (reading stats panel).
    #[allow(dead_code)] // Consumed by downstream TASK-8
    pub async fn get_reading_stats(&self, days: u32) -> Result<ReadingStats> {
        let rows: Vec<(i64, String, i64, i64)> = sqlx::query_as(
            r#"
            SELECT rh.feed_id, f.title, COUNT(*) as cnt,
                   COALESCE(SUM(rh.duration_seconds), 0) as total_secs
            FROM reading_history rh
            JOIN feeds f ON rh.feed_id = f.id
            WHERE rh.opened_at > datetime('now', '-' || ? || ' days')
            GROUP BY rh.feed_id
            ORDER BY cnt DESC
            LIMIT 10
        "#,
        )
        .bind(days)
        .fetch_all(&self.pool)
        .await?;

        let total_articles: u64 = rows.iter().map(|r| r.2 as u64).sum();
        let total_seconds: u64 = rows.iter().map(|r| r.3 as u64).sum();

        let articles_per_day = if days == 0 {
            0.0
        } else {
            total_articles as f64 / days as f64
        };

        // SEC-001: Sanitize feed titles — these bypass the normal ArticleDbRow::into_article() path
        let top_feeds = rows
            .into_iter()
            .map(|(_feed_id, title, cnt, _secs)| {
                let sanitized = match strip_control_chars(&title) {
                    Cow::Borrowed(_) => title,
                    Cow::Owned(s) => s,
                };
                (sanitized, cnt as u32)
            })
            .collect();

        Ok(ReadingStats {
            articles_per_day,
            total_minutes: total_seconds / 60,
            top_feeds,
        })
    }

    /// Get recent reading history entries with article and feed titles.
    ///
    /// Returns the most recent `limit` entries ordered by opened_at descending.
    /// Used by TASK-8 (reading stats panel).
    #[allow(dead_code)] // Consumed by downstream TASK-8
    pub async fn get_reading_history(&self, limit: u32) -> Result<Vec<ReadingHistoryEntry>> {
        let rows: Vec<(i64, i64, String, String, String, Option<i64>)> = sqlx::query_as(
            r#"
            SELECT rh.id, rh.article_id, a.title as article_title,
                   f.title as feed_title, rh.opened_at, rh.duration_seconds
            FROM reading_history rh
            JOIN articles a ON rh.article_id = a.id
            JOIN feeds f ON rh.feed_id = f.id
            ORDER BY rh.opened_at DESC
            LIMIT ?
        "#,
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        // SEC-001: Sanitize titles — these bypass the normal ArticleDbRow::into_article() path
        Ok(rows
            .into_iter()
            .map(
                |(id, article_id, article_title, feed_title, opened_at, duration_seconds)| {
                    ReadingHistoryEntry {
                        id,
                        article_id,
                        article_title: strip_control_chars(&article_title).into_owned(),
                        feed_title: strip_control_chars(&feed_title).into_owned(),
                        opened_at,
                        duration_seconds,
                    }
                },
            )
            .collect())
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
    async fn test_record_open_close() {
        let db = test_db().await;
        db.sync_feeds(&[test_feed(1)]).await.unwrap();
        let feeds = db.get_feeds_with_unread_counts().await.unwrap();
        db.upsert_articles(feeds[0].id, &[test_article("1", "Article 1")])
            .await
            .unwrap();
        let articles = db.get_articles_for_feed(feeds[0].id, None).await.unwrap();

        let history_id = db.record_open(articles[0].id, feeds[0].id).await.unwrap();
        assert!(history_id > 0);

        db.record_close(history_id, 120).await.unwrap();

        let history = db.get_reading_history(10).await.unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].id, history_id);
        assert_eq!(history[0].article_id, articles[0].id);
        assert_eq!(history[0].duration_seconds, Some(120));
    }

    #[tokio::test]
    async fn test_null_duration_on_unclosed() {
        let db = test_db().await;
        db.sync_feeds(&[test_feed(1)]).await.unwrap();
        let feeds = db.get_feeds_with_unread_counts().await.unwrap();
        db.upsert_articles(feeds[0].id, &[test_article("1", "Article 1")])
            .await
            .unwrap();
        let articles = db.get_articles_for_feed(feeds[0].id, None).await.unwrap();

        let _history_id = db.record_open(articles[0].id, feeds[0].id).await.unwrap();

        // Never call record_close — simulates crash/force quit
        let history = db.get_reading_history(10).await.unwrap();
        assert_eq!(history.len(), 1);
        assert!(history[0].duration_seconds.is_none());
    }

    #[tokio::test]
    async fn test_reading_stats_calculation() {
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
        db.upsert_articles(feeds[1].id, &[test_article("3", "Article 3")])
            .await
            .unwrap();

        let articles_f1 = db.get_articles_for_feed(feeds[0].id, None).await.unwrap();
        let articles_f2 = db.get_articles_for_feed(feeds[1].id, None).await.unwrap();

        // Record reading sessions with durations
        let h1 = db
            .record_open(articles_f1[0].id, feeds[0].id)
            .await
            .unwrap();
        db.record_close(h1, 300).await.unwrap(); // 5 minutes

        let h2 = db
            .record_open(articles_f1[1].id, feeds[0].id)
            .await
            .unwrap();
        db.record_close(h2, 180).await.unwrap(); // 3 minutes

        let h3 = db
            .record_open(articles_f2[0].id, feeds[1].id)
            .await
            .unwrap();
        db.record_close(h3, 120).await.unwrap(); // 2 minutes

        let stats = db.get_reading_stats(7).await.unwrap();

        // 3 articles over 7 days
        let expected_apd = 3.0 / 7.0;
        assert!((stats.articles_per_day - expected_apd).abs() < 0.001);

        // Total: 300 + 180 + 120 = 600 seconds = 10 minutes
        assert_eq!(stats.total_minutes, 10);

        // Top feeds: Feed 1 (2 reads), Feed 2 (1 read)
        assert_eq!(stats.top_feeds.len(), 2);
        assert_eq!(stats.top_feeds[0].1, 2);
        assert_eq!(stats.top_feeds[1].1, 1);
    }

    #[tokio::test]
    async fn test_top_feeds_ranking() {
        let db = test_db().await;
        db.sync_feeds(&[test_feed(1), test_feed(2), test_feed(3)])
            .await
            .unwrap();
        let feeds = db.get_feeds_with_unread_counts().await.unwrap();

        // Insert articles for each feed
        for (i, feed) in feeds.iter().enumerate() {
            let articles: Vec<_> = (0..3)
                .map(|j| {
                    test_article(
                        &format!("f{}-a{}", i, j),
                        &format!("Feed {} Article {}", i + 1, j),
                    )
                })
                .collect();
            db.upsert_articles(feed.id, &articles).await.unwrap();
        }

        let articles_f1 = db.get_articles_for_feed(feeds[0].id, None).await.unwrap();
        let articles_f2 = db.get_articles_for_feed(feeds[1].id, None).await.unwrap();
        let articles_f3 = db.get_articles_for_feed(feeds[2].id, None).await.unwrap();

        // Feed 3: 3 reads (most)
        for a in &articles_f3 {
            let h = db.record_open(a.id, feeds[2].id).await.unwrap();
            db.record_close(h, 60).await.unwrap();
        }
        // Feed 1: 2 reads
        for a in &articles_f1[..2] {
            let h = db.record_open(a.id, feeds[0].id).await.unwrap();
            db.record_close(h, 60).await.unwrap();
        }
        // Feed 2: 1 read
        let h = db
            .record_open(articles_f2[0].id, feeds[1].id)
            .await
            .unwrap();
        db.record_close(h, 60).await.unwrap();

        let stats = db.get_reading_stats(30).await.unwrap();

        // Feed 3 first (3), Feed 1 second (2), Feed 2 third (1)
        assert_eq!(stats.top_feeds.len(), 3);
        assert_eq!(stats.top_feeds[0].0, "Test Feed 3");
        assert_eq!(stats.top_feeds[0].1, 3);
        assert_eq!(stats.top_feeds[1].0, "Test Feed 1");
        assert_eq!(stats.top_feeds[1].1, 2);
        assert_eq!(stats.top_feeds[2].0, "Test Feed 2");
        assert_eq!(stats.top_feeds[2].1, 1);
    }

    #[tokio::test]
    async fn test_get_reading_history() {
        let db = test_db().await;
        db.sync_feeds(&[test_feed(1)]).await.unwrap();
        let feeds = db.get_feeds_with_unread_counts().await.unwrap();

        db.upsert_articles(
            feeds[0].id,
            &[
                test_article("1", "Article Alpha"),
                test_article("2", "Article Beta"),
                test_article("3", "Article Gamma"),
            ],
        )
        .await
        .unwrap();
        let articles = db.get_articles_for_feed(feeds[0].id, None).await.unwrap();

        // Record 3 reading sessions
        for a in &articles {
            let h = db.record_open(a.id, feeds[0].id).await.unwrap();
            db.record_close(h, 90).await.unwrap();
        }

        // Limit to 2
        let history = db.get_reading_history(2).await.unwrap();
        assert_eq!(history.len(), 2);

        // All should have feed title and duration
        for entry in &history {
            assert_eq!(entry.feed_title, "Test Feed 1");
            assert_eq!(entry.duration_seconds, Some(90));
            assert!(!entry.article_title.is_empty());
            assert!(!entry.opened_at.is_empty());
        }
    }

    #[tokio::test]
    async fn test_reading_stats_zero_days() {
        let db = test_db().await;
        let stats = db.get_reading_stats(0).await.unwrap();
        assert_eq!(stats.articles_per_day, 0.0);
        assert_eq!(stats.total_minutes, 0);
        assert!(stats.top_feeds.is_empty());
    }

    #[tokio::test]
    async fn test_cascade_delete_on_article_removal() {
        let db = test_db().await;
        db.sync_feeds(&[test_feed(1)]).await.unwrap();
        let feeds = db.get_feeds_with_unread_counts().await.unwrap();
        db.upsert_articles(feeds[0].id, &[test_article("1", "To Delete")])
            .await
            .unwrap();
        let articles = db.get_articles_for_feed(feeds[0].id, None).await.unwrap();

        let _h = db.record_open(articles[0].id, feeds[0].id).await.unwrap();

        // Delete the article directly
        sqlx::query("DELETE FROM articles WHERE id = ?")
            .bind(articles[0].id)
            .execute(&db.pool)
            .await
            .unwrap();

        // History should be cascade-deleted
        let history = db.get_reading_history(10).await.unwrap();
        assert!(history.is_empty());
    }
}
