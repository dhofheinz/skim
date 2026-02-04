use anyhow::Result;
use sqlx::{sqlite::SqlitePoolOptions, FromRow, SqlitePool};

// ============================================================================
// Helper Types
// ============================================================================

/// Row type for feed query with unread count
type FeedRow = (
    i64,
    String,
    String,
    Option<String>,
    Option<i64>,
    Option<String>,
    i64,
);

/// Represents a feed imported from OPML
#[derive(Debug, Clone)]
pub struct OpmlFeed {
    pub title: String,
    pub xml_url: String,
    pub html_url: Option<String>,
}

/// Represents a parsed article from a feed
#[derive(Debug, Clone)]
pub struct ParsedArticle {
    pub guid: String,
    pub title: String,
    pub url: Option<String>,
    pub published: Option<i64>,
    pub summary: Option<String>,
}

// ============================================================================
// Data Structures
// ============================================================================

#[derive(Debug, Clone, FromRow)]
#[allow(dead_code)]
pub struct Feed {
    pub id: i64,
    pub title: String,
    pub url: String,
    pub html_url: Option<String>,
    pub last_fetched: Option<i64>,
    pub error: Option<String>,
    #[sqlx(skip)]
    pub unread_count: i64,
}

#[derive(Debug, Clone, FromRow)]
#[allow(dead_code)]
pub struct Article {
    pub id: i64,
    pub feed_id: i64,
    pub guid: String,
    pub title: String,
    pub url: Option<String>,
    pub published: Option<i64>,
    pub summary: Option<String>,
    pub content: Option<String>,
    pub read: bool,
    pub starred: bool,
    pub fetched_at: i64,
}

// ============================================================================
// Database
// ============================================================================

#[derive(Clone)]
pub struct Database {
    pool: SqlitePool,
}

impl Database {
    /// Open a database connection and run migrations
    pub async fn open(path: &str) -> Result<Self> {
        let url = format!("sqlite:{}?mode=rwc", path);
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect(&url)
            .await?;
        let db = Self { pool };
        db.migrate().await?;
        Ok(db)
    }

    /// Run database migrations
    async fn migrate(&self) -> Result<()> {
        // Enable foreign keys
        sqlx::query("PRAGMA foreign_keys = ON")
            .execute(&self.pool)
            .await?;

        // Create feeds table
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS feeds (
                id INTEGER PRIMARY KEY,
                title TEXT NOT NULL,
                url TEXT UNIQUE NOT NULL,
                html_url TEXT,
                last_fetched INTEGER,
                error TEXT
            )
        "#,
        )
        .execute(&self.pool)
        .await?;

        // Create articles table
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS articles (
                id INTEGER PRIMARY KEY,
                feed_id INTEGER NOT NULL REFERENCES feeds(id) ON DELETE CASCADE,
                guid TEXT NOT NULL,
                title TEXT NOT NULL,
                url TEXT,
                published INTEGER,
                summary TEXT,
                content TEXT,
                read INTEGER NOT NULL DEFAULT 0,
                starred INTEGER NOT NULL DEFAULT 0,
                fetched_at INTEGER NOT NULL,
                UNIQUE(feed_id, guid)
            )
        "#,
        )
        .execute(&self.pool)
        .await?;

        // Create indexes
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_articles_feed ON articles(feed_id)")
            .execute(&self.pool)
            .await?;
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_articles_published ON articles(published DESC)",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_articles_read ON articles(read)")
            .execute(&self.pool)
            .await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_articles_starred ON articles(starred)")
            .execute(&self.pool)
            .await?;

        Ok(())
    }

    // ========================================================================
    // Feed Operations
    // ========================================================================

    /// Sync feeds from OPML import (INSERT OR REPLACE)
    pub async fn sync_feeds(&self, feeds: &[OpmlFeed]) -> Result<()> {
        for feed in feeds {
            sqlx::query(
                r#"
                INSERT INTO feeds (title, url, html_url)
                VALUES (?, ?, ?)
                ON CONFLICT(url) DO UPDATE SET
                    title = excluded.title,
                    html_url = excluded.html_url
            "#,
            )
            .bind(&feed.title)
            .bind(&feed.xml_url)
            .bind(&feed.html_url)
            .execute(&self.pool)
            .await?;
        }
        Ok(())
    }

    /// Get all feeds with their unread article counts
    pub async fn get_feeds_with_unread_counts(&self) -> Result<Vec<Feed>> {
        let rows: Vec<FeedRow> = sqlx::query_as(
            r#"
                SELECT
                    f.id, f.title, f.url, f.html_url, f.last_fetched, f.error,
                    COUNT(CASE WHEN a.read = 0 THEN 1 END) as unread_count
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
                |(id, title, url, html_url, last_fetched, error, unread_count)| Feed {
                    id,
                    title,
                    url,
                    html_url,
                    last_fetched,
                    error,
                    unread_count,
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

    /// Update the last_fetched timestamp for a feed
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
    // Article Operations
    // ========================================================================

    /// Upsert articles for a feed, returns the number of new articles inserted
    pub async fn upsert_articles(&self, feed_id: i64, articles: &[ParsedArticle]) -> Result<usize> {
        let now = chrono::Utc::now().timestamp();
        let mut inserted = 0;

        for article in articles {
            let result = sqlx::query(
                r#"
                INSERT INTO articles (feed_id, guid, title, url, published, summary, fetched_at)
                VALUES (?, ?, ?, ?, ?, ?, ?)
                ON CONFLICT(feed_id, guid) DO UPDATE SET
                    title = excluded.title,
                    url = excluded.url,
                    published = excluded.published,
                    summary = excluded.summary
            "#,
            )
            .bind(feed_id)
            .bind(&article.guid)
            .bind(&article.title)
            .bind(&article.url)
            .bind(article.published)
            .bind(&article.summary)
            .bind(now)
            .execute(&self.pool)
            .await?;

            if result.rows_affected() > 0 {
                inserted += 1;
            }
        }

        Ok(inserted)
    }

    /// Get all articles for a specific feed
    pub async fn get_articles_for_feed(&self, feed_id: i64) -> Result<Vec<Article>> {
        let articles = sqlx::query_as::<_, Article>(
            r#"
            SELECT id, feed_id, guid, title, url, published, summary, content,
                   read, starred, fetched_at
            FROM articles
            WHERE feed_id = ?
            ORDER BY published DESC, fetched_at DESC
        "#,
        )
        .bind(feed_id)
        .fetch_all(&self.pool)
        .await?;

        Ok(articles)
    }

    /// Get all starred articles
    #[allow(dead_code)]
    pub async fn get_starred_articles(&self) -> Result<Vec<Article>> {
        let articles = sqlx::query_as::<_, Article>(
            r#"
            SELECT id, feed_id, guid, title, url, published, summary, content,
                   read, starred, fetched_at
            FROM articles
            WHERE starred = 1
            ORDER BY published DESC, fetched_at DESC
        "#,
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(articles)
    }

    /// Search articles by title or summary
    pub async fn search_articles(&self, query: &str) -> Result<Vec<Article>> {
        let search_pattern = format!("%{}%", query);
        let articles = sqlx::query_as::<_, Article>(
            r#"
            SELECT id, feed_id, guid, title, url, published, summary, content,
                   read, starred, fetched_at
            FROM articles
            WHERE title LIKE ? OR summary LIKE ?
            ORDER BY published DESC, fetched_at DESC
        "#,
        )
        .bind(&search_pattern)
        .bind(&search_pattern)
        .fetch_all(&self.pool)
        .await?;

        Ok(articles)
    }

    /// Mark an article as read
    pub async fn mark_read(&self, article_id: i64) -> Result<()> {
        sqlx::query("UPDATE articles SET read = 1 WHERE id = ?")
            .bind(article_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Toggle the starred status of an article
    pub async fn toggle_star(&self, article_id: i64) -> Result<()> {
        sqlx::query("UPDATE articles SET starred = NOT starred WHERE id = ?")
            .bind(article_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Get the full content of an article
    #[allow(dead_code)]
    pub async fn get_article_content(&self, article_id: i64) -> Result<Option<String>> {
        let row: Option<(Option<String>,)> =
            sqlx::query_as("SELECT content FROM articles WHERE id = ?")
                .bind(article_id)
                .fetch_optional(&self.pool)
                .await?;

        Ok(row.and_then(|(content,)| content))
    }

    /// Set the full content of an article
    #[allow(dead_code)]
    pub async fn set_article_content(&self, article_id: i64, content: &str) -> Result<()> {
        sqlx::query("UPDATE articles SET content = ? WHERE id = ?")
            .bind(content)
            .bind(article_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}
