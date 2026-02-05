use anyhow::Result;
use sqlx::{
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
    QueryBuilder, SqlitePool,
};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;

// ============================================================================
// Error Types
// ============================================================================

/// Database-specific errors with user-friendly messages
#[derive(Debug, Error)]
pub enum DatabaseError {
    /// Another instance of the application has locked the database
    #[error("Another instance of skim appears to be running. Please close it and try again.")]
    InstanceLocked,

    /// Migration failed
    #[error("Database migration failed: {0}")]
    Migration(String),

    /// Generic database error
    #[error("Database error: {0}")]
    Other(#[from] sqlx::Error),
}

impl DatabaseError {
    /// Check if a sqlx error indicates database locking
    fn from_sqlx(err: sqlx::Error) -> Self {
        let error_string = err.to_string().to_lowercase();

        // Check for SQLite lock-related error messages
        // SQLITE_BUSY (5): database is locked
        // SQLITE_LOCKED (6): database table is locked
        // SQLITE_CANTOPEN (14): unable to open database file
        if error_string.contains("database is locked")
            || error_string.contains("database table is locked")
            || error_string.contains("sqlite_busy")
            || error_string.contains("sqlite_locked")
            || error_string.contains("unable to open database file")
        {
            return DatabaseError::InstanceLocked;
        }

        DatabaseError::Other(err)
    }
}

// ============================================================================
// FTS5 Consistency Report
// ============================================================================

/// Detailed FTS consistency report
///
/// Provides comprehensive information about the state of the FTS5 index
/// relative to the articles table, including detection of orphaned entries
/// (in FTS but not in articles) and missing entries (in articles but not in FTS).
#[derive(Debug)]
pub struct FtsConsistencyReport {
    /// Number of rows in the articles table
    pub articles_count: i64,
    /// Number of rows in the articles_fts table
    pub fts_count: i64,
    /// Number of FTS entries with no corresponding article (orphaned)
    pub orphaned_fts_entries: i64,
    /// Number of articles with no corresponding FTS entry (missing)
    pub missing_fts_entries: i64,
    /// True if the index is fully consistent (no orphans, no missing, counts match)
    pub is_consistent: bool,
}

// ============================================================================
// FTS5 Query Validation
// ============================================================================

const MAX_QUERY_LENGTH: usize = 256;
const MAX_WILDCARDS: usize = 3;
const MAX_OR_OPERATORS: usize = 5;
const MAX_PARENTHESES: usize = 5;
const MAX_AND_OPERATORS: usize = 10;

// ============================================================================
// Query Limit Constants
// ============================================================================

/// Maximum number of articles to return from any single query (OOM protection)
const MAX_ARTICLES: i64 = 2000;

/// Maximum limit for batch article queries like get_recent_articles_for_feeds
const MAX_BATCH_LIMIT: usize = 10000;

/// Validate FTS5 query complexity to prevent DoS via expensive wildcard expansions.
///
/// Limits:
/// - Maximum query length: 256 characters
/// - Maximum wildcards (*): 3
/// - Maximum OR operators: 5
/// - Maximum parentheses: 5 (BUG-008)
/// - Maximum AND operators: 10 (BUG-008)
fn validate_fts_query(query: &str) -> Result<()> {
    if query.len() > MAX_QUERY_LENGTH {
        anyhow::bail!(
            "Search query exceeds maximum length of {} characters",
            MAX_QUERY_LENGTH
        );
    }

    let wildcard_count = query.matches('*').count();
    if wildcard_count > MAX_WILDCARDS {
        anyhow::bail!(
            "Search query contains too many wildcards (max {})",
            MAX_WILDCARDS
        );
    }

    // Case-insensitive OR count
    let or_count = query.to_uppercase().matches(" OR ").count();
    if or_count > MAX_OR_OPERATORS {
        anyhow::bail!(
            "Search query contains too many OR operators (max {})",
            MAX_OR_OPERATORS
        );
    }

    // BUG-008: Add parenthesis limit to prevent exponential parsing time
    let open_paren_count = query.chars().filter(|&c| c == '(').count();
    let close_paren_count = query.chars().filter(|&c| c == ')').count();
    if open_paren_count > MAX_PARENTHESES {
        anyhow::bail!(
            "Search query contains too many parentheses (max {})",
            MAX_PARENTHESES
        );
    }

    // BUG-014: Check for balanced parentheses to catch malformed queries early
    if open_paren_count != close_paren_count {
        anyhow::bail!("Search query has unbalanced parentheses");
    }

    // BUG-008: Add AND operator limit
    let and_count = query.to_uppercase().matches(" AND ").count();
    if and_count > MAX_AND_OPERATORS {
        anyhow::bail!(
            "Search query contains too many AND operators (max {})",
            MAX_AND_OPERATORS
        );
    }

    Ok(())
}

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
    i64, // consecutive_failures
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

/// Internal row type for Article queries (used by sqlx FromRow)
/// Converts to Article via into_article() with Arc wrapping
#[derive(Debug, sqlx::FromRow)]
struct ArticleDbRow {
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

impl ArticleDbRow {
    fn into_article(self) -> Article {
        Article {
            id: self.id,
            feed_id: self.feed_id,
            guid: self.guid,
            title: Arc::from(self.title),
            url: self.url.map(Arc::from),
            published: self.published,
            summary: self.summary.map(Arc::from),
            content: self.content.map(Arc::from),
            read: self.read,
            starred: self.starred,
            fetched_at: self.fetched_at,
        }
    }
}

/// Article with feed_id for batch queries (used in get_recent_articles_for_feeds)
#[derive(Debug, sqlx::FromRow)]
struct ArticleRow {
    pub feed_id: i64,
    pub id: i64,
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

impl ArticleRow {
    fn into_tuple(self) -> (i64, Article) {
        (
            self.feed_id,
            Article {
                id: self.id,
                feed_id: self.feed_id,
                guid: self.guid,
                title: Arc::from(self.title),
                url: self.url.map(Arc::from),
                published: self.published,
                summary: self.summary.map(Arc::from),
                content: self.content.map(Arc::from),
                read: self.read,
                starred: self.starred,
                fetched_at: self.fetched_at,
            },
        )
    }
}

// ============================================================================
// Data Structures
// ============================================================================

/// Feed data from database
///
/// All fields are actively used in application logic.
/// Note: `title` uses `Arc<str>` for cheap cloning in feed_title_cache (PERF-009).
#[derive(Debug, Clone)]
pub struct Feed {
    pub id: i64,
    pub title: Arc<str>,
    pub url: String,
    pub html_url: Option<String>,
    pub last_fetched: Option<i64>,
    pub error: Option<String>,
    pub unread_count: i64,
    /// Number of consecutive fetch failures (circuit breaker)
    pub consecutive_failures: i64,
}

/// Article data from database
///
/// Note: Fields `guid`, `content`, and `fetched_at` are populated from DB but not
/// read by application logic:
/// - `guid`: Used only for DB deduplication (UNIQUE constraint)
/// - `content`: Cached content accessed via `get_article_content()` by ID
/// - `fetched_at`: Used only in SQL ORDER BY clauses
///
/// PERF-010: String fields (title, url, summary, content) use `Arc<str>` for cheap
/// cloning in event handlers and reader view.
///
/// Annotation retained: sqlx FromRow requires all columns to deserialize.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct Article {
    pub id: i64,
    pub feed_id: i64,
    pub guid: String, // Keep as String (used for hashing/comparison)
    pub title: Arc<str>,
    pub url: Option<Arc<str>>,
    pub published: Option<i64>,
    pub summary: Option<Arc<str>>,
    pub content: Option<Arc<str>>,
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
    ///
    /// # Errors
    ///
    /// Returns `DatabaseError::InstanceLocked` if another instance of skim
    /// has the database locked (SQLITE_BUSY, SQLITE_LOCKED, SQLITE_CANTOPEN).
    /// Returns `DatabaseError::Other` for other database errors.
    pub async fn open(path: &str) -> Result<Self, DatabaseError> {
        let url = format!("sqlite:{}?mode=rwc", path);
        // Configure SQLite connection options with busy_timeout pragma.
        // busy_timeout=5000: SQLite waits up to 5 seconds for locks to release before returning SQLITE_BUSY.
        // This handles transient lock contention (e.g., concurrent refresh operations) automatically.
        // Using pragma() ensures all connections in the pool inherit this setting.
        let options = SqliteConnectOptions::from_str(&url)
            .map_err(DatabaseError::from_sqlx)?
            .pragma("busy_timeout", "5000");
        // Pool sized for: 10 concurrent feed fetches + content loads + UI queries + headroom
        let pool = SqlitePoolOptions::new()
            .max_connections(20)
            .acquire_timeout(Duration::from_secs(10))
            .connect_with(options)
            .await
            .map_err(DatabaseError::from_sqlx)?;
        let db = Self { pool };
        db.migrate().await.map_err(|e| {
            // Migration errors could also be lock-related
            // Check if the error message indicates a lock condition
            let error_string = e.to_string().to_lowercase();
            if error_string.contains("database is locked")
                || error_string.contains("database table is locked")
                || error_string.contains("sqlite_busy")
                || error_string.contains("sqlite_locked")
            {
                DatabaseError::InstanceLocked
            } else {
                DatabaseError::Migration(e.to_string())
            }
        })?;
        Ok(db)
    }

    /// Run database migrations atomically within a transaction.
    ///
    /// All schema changes (tables, indexes, triggers) are wrapped in a single
    /// transaction to ensure atomicity. If any migration step fails (e.g., disk
    /// full, power loss), the entire migration is rolled back, leaving the
    /// database in its previous consistent state.
    ///
    /// SQLite supports DDL statements within transactions, making this safe.
    /// All migrations use `IF NOT EXISTS` for idempotency, so re-running on
    /// an existing database is a no-op.
    async fn migrate(&self) -> Result<()> {
        // Enable foreign keys (must be outside transaction, per-connection setting)
        sqlx::query("PRAGMA foreign_keys = ON")
            .execute(&self.pool)
            .await?;

        // Set busy timeout to 5 seconds: SQLite waits for locks to release before returning SQLITE_BUSY.
        // This handles transient lock contention (e.g., concurrent refresh operations) automatically.
        sqlx::query("PRAGMA busy_timeout = 5000")
            .execute(&self.pool)
            .await?;

        // Begin transaction for all schema migrations
        let mut tx = self.pool.begin().await?;

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
        .execute(&mut *tx)
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
        .execute(&mut *tx)
        .await?;

        // Create indexes
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_articles_feed ON articles(feed_id)")
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_articles_published ON articles(published DESC)",
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_articles_read ON articles(read)")
            .execute(&mut *tx)
            .await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_articles_starred ON articles(starred)")
            .execute(&mut *tx)
            .await?;

        // Composite index for efficient unread count aggregation in get_feeds_with_unread_counts()
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_articles_feed_read ON articles(feed_id, read)")
            .execute(&mut *tx)
            .await?;

        // PERF-017: Composite index for get_articles_for_feed() which filters by feed_id and sorts by published DESC
        // This replaces both idx_articles_feed and idx_articles_published for this common query pattern
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_articles_feed_published ON articles(feed_id, published DESC)",
        )
        .execute(&mut *tx)
        .await?;

        // Composite index for starred articles query: filters by starred=1, orders by published DESC
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_articles_starred_published ON articles(starred, published DESC)",
        )
        .execute(&mut *tx)
        .await?;

        // Covering index for recent unread articles query (What's New panel)
        // Partial index on unread articles only, ordered by fetched_at DESC
        // Covers: WHERE read = 0 AND feed_id IN (...) ORDER BY fetched_at DESC
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_articles_unread_recent ON articles(feed_id, fetched_at DESC) WHERE read = 0",
        )
        .execute(&mut *tx)
        .await?;

        // PERF-002: Add FTS5 virtual table
        sqlx::query(
            r#"
            CREATE VIRTUAL TABLE IF NOT EXISTS articles_fts
            USING fts5(title, summary, content=articles, content_rowid=id)
        "#,
        )
        .execute(&mut *tx)
        .await?;

        // Populate FTS5 with existing data (safe to run multiple times)
        // BUG-007: Handle errors explicitly - only treat "no such table" as expected
        // Use proper SQLx error type matching instead of string containment
        let fts_count: (i64,) = match sqlx::query_as("SELECT COUNT(*) FROM articles_fts")
            .fetch_one(&mut *tx)
            .await
        {
            Ok(row) => row,
            Err(sqlx::Error::Database(db_err)) => {
                // SQLite error code 1 (SQLITE_ERROR) is used for "no such table"
                // We still need string check as fallback since error codes vary
                if db_err.message().contains("no such table") {
                    tracing::debug!("FTS table does not exist yet, treating as empty");
                    (0,)
                } else {
                    tracing::warn!(error = %db_err, "FTS table query failed");
                    return Err(sqlx::Error::Database(db_err).into());
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "FTS table query failed (non-database error)");
                return Err(e.into());
            }
        };

        if fts_count.0 == 0 {
            // Only populate if empty (first run or fresh db)
            let article_count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM articles")
                .fetch_one(&mut *tx)
                .await
                .unwrap_or((0,));

            if article_count.0 > 0 {
                sqlx::query(
                    r#"
                    INSERT INTO articles_fts(rowid, title, summary)
                    SELECT id, title, summary FROM articles
                "#,
                )
                .execute(&mut *tx)
                .await?;
            }
        }

        // Sync triggers
        sqlx::query(
            r#"
            CREATE TRIGGER IF NOT EXISTS articles_fts_insert AFTER INSERT ON articles BEGIN
                INSERT INTO articles_fts(rowid, title, summary)
                VALUES (new.id, new.title, new.summary);
            END
        "#,
        )
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            r#"
            CREATE TRIGGER IF NOT EXISTS articles_fts_delete AFTER DELETE ON articles BEGIN
                INSERT INTO articles_fts(articles_fts, rowid, title, summary)
                VALUES ('delete', old.id, old.title, old.summary);
            END
        "#,
        )
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            r#"
            CREATE TRIGGER IF NOT EXISTS articles_fts_update AFTER UPDATE ON articles BEGIN
                INSERT INTO articles_fts(articles_fts, rowid, title, summary)
                VALUES ('delete', old.id, old.title, old.summary);
                INSERT INTO articles_fts(rowid, title, summary)
                VALUES (new.id, new.title, new.summary);
            END
        "#,
        )
        .execute(&mut *tx)
        .await?;

        // Add consecutive_failures column for circuit breaker (ignore error if exists)
        sqlx::query("ALTER TABLE feeds ADD COLUMN consecutive_failures INTEGER NOT NULL DEFAULT 0")
            .execute(&mut *tx)
            .await
            .ok(); // Ignore error if column already exists

        // Commit all migrations atomically
        tx.commit().await?;

        Ok(())
    }

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

    /// Search articles by title or summary
    /// Uses FTS5 for fast search with LIKE fallback for syntax errors
    /// PERF-003: Hard cap at MAX_ARTICLES (2000) to prevent OOM
    pub async fn search_articles(&self, query: &str) -> Result<Vec<Article>> {
        // Early return for empty/whitespace-only queries
        let query = query.trim();
        if query.is_empty() {
            return Ok(Vec::new());
        }

        // Validate query complexity to prevent DoS via expensive wildcard expansions
        validate_fts_query(query)?;

        tracing::debug!(limit = MAX_ARTICLES, query = %query, "search_articles with limit cap");

        // PERF-002: Try FTS5 MATCH first for fast search
        let fts_result = sqlx::query_as::<_, ArticleDbRow>(
            r#"
            SELECT a.id, a.feed_id, a.guid, a.title, a.url, a.published,
                   a.summary, a.content, a.read, a.starred, a.fetched_at
            FROM articles a
            INNER JOIN articles_fts ON a.id = articles_fts.rowid
            WHERE articles_fts MATCH ?
            ORDER BY a.published DESC
            LIMIT ?
        "#,
        )
        .bind(query)
        .bind(MAX_ARTICLES)
        .fetch_all(&self.pool)
        .await;

        // Fall back to LIKE for queries that fail FTS5 syntax
        match fts_result {
            Ok(rows) => Ok(rows.into_iter().map(ArticleDbRow::into_article).collect()),
            Err(e) => {
                tracing::warn!(error = %e, query = %query, "FTS5 search failed, falling back to LIKE");
                let like_pattern = format!("%{}%", query);
                let rows = sqlx::query_as::<_, ArticleDbRow>(
                    r#"
                    SELECT id, feed_id, guid, title, url, published,
                           summary, NULL as content, read, starred, fetched_at
                    FROM articles
                    WHERE title LIKE ?1 OR summary LIKE ?1
                    ORDER BY published DESC
                    LIMIT ?2
                "#,
                )
                .bind(&like_pattern)
                .bind(MAX_ARTICLES)
                .fetch_all(&self.pool)
                .await?;

                Ok(rows.into_iter().map(ArticleDbRow::into_article).collect())
            }
        }
    }

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
    // FTS5 Maintenance Operations
    // ========================================================================

    /// Check FTS consistency with detailed report.
    ///
    /// Performs comprehensive consistency checks between `articles` and `articles_fts`:
    /// - Counts rows in both tables
    /// - Detects orphaned FTS entries (in FTS but not in articles)
    /// - Detects missing FTS entries (in articles but not in FTS)
    ///
    /// # Returns
    ///
    /// A detailed [`FtsConsistencyReport`] with counts and consistency status.
    pub async fn check_fts_consistency_detailed(
        &self,
    ) -> Result<FtsConsistencyReport, DatabaseError> {
        let articles_count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM articles")
            .fetch_one(&self.pool)
            .await?;

        let fts_count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM articles_fts")
            .fetch_one(&self.pool)
            .await?;

        // PERF-014: Use LEFT JOIN instead of NOT IN for O(n) instead of O(n*m)
        // Orphaned: in FTS but not in articles
        let orphaned: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM articles_fts LEFT JOIN articles ON articles_fts.rowid = articles.id WHERE articles.id IS NULL",
        )
        .fetch_one(&self.pool)
        .await?;

        // Missing: in articles but not in FTS
        let missing: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM articles LEFT JOIN articles_fts ON articles.id = articles_fts.rowid WHERE articles_fts.rowid IS NULL",
        )
        .fetch_one(&self.pool)
        .await?;

        let is_consistent = orphaned.0 == 0 && missing.0 == 0 && articles_count.0 == fts_count.0;

        tracing::debug!(
            articles = articles_count.0,
            fts = fts_count.0,
            orphaned = orphaned.0,
            missing = missing.0,
            is_consistent = is_consistent,
            "FTS5 detailed consistency check"
        );

        Ok(FtsConsistencyReport {
            articles_count: articles_count.0,
            fts_count: fts_count.0,
            orphaned_fts_entries: orphaned.0,
            missing_fts_entries: missing.0,
            is_consistent,
        })
    }

    /// Check if FTS5 index is consistent with articles table.
    ///
    /// Simple consistency check that returns a boolean. For detailed diagnostics,
    /// use [`check_fts_consistency_detailed`] instead.
    ///
    /// # Returns
    ///
    /// `true` if consistent, `false` if inconsistent.
    #[allow(dead_code)] // Public API used in tests
    pub async fn check_fts_consistency(&self) -> Result<bool, DatabaseError> {
        let report = self.check_fts_consistency_detailed().await?;
        Ok(report.is_consistent)
    }

    /// Rebuild FTS5 index from articles table.
    ///
    /// Clears the FTS5 table and repopulates it from the articles table.
    /// Use this when `check_fts_consistency()` returns `false`, or after
    /// database restore/migration issues.
    ///
    /// # Returns
    ///
    /// The number of articles indexed.
    pub async fn rebuild_fts_index(&self) -> Result<usize> {
        // FTS5 rebuild command: clears and repopulates the index from content table
        // This is the proper way to rebuild an external content FTS5 table
        sqlx::query("INSERT INTO articles_fts(articles_fts) VALUES('rebuild')")
            .execute(&self.pool)
            .await?;

        // Return count of indexed articles
        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM articles")
            .fetch_one(&self.pool)
            .await?;

        Ok(count.0 as usize)
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
        separated.push_unseparated(") ORDER BY fetched_at DESC LIMIT ");
        builder.push_bind(safe_limit);

        let rows: Vec<ArticleRow> = builder.build_query_as().fetch_all(&self.pool).await?;
        Ok(rows.into_iter().map(ArticleRow::into_tuple).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_db() -> Database {
        // Creates in-memory DB with full schema (auto-migrates)
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

        // Insert original article
        db.upsert_articles(feeds[0].id, &[test_article("guid-1", "Original Title")])
            .await
            .unwrap();

        // Mark as read and starred to verify user state is preserved
        let articles = db.get_articles_for_feed(feeds[0].id, None).await.unwrap();
        db.mark_article_read(articles[0].id).await.unwrap();
        db.toggle_article_starred(articles[0].id).await.unwrap();

        // Re-fetch with updated metadata
        let updated = ParsedArticle {
            guid: "guid-1".to_string(),
            title: "Updated Title".to_string(),
            url: Some("https://example.com/updated".to_string()),
            published: Some(1704153600), // Different timestamp
            summary: Some("Updated summary".to_string()),
        };
        db.upsert_articles(feeds[0].id, &[updated]).await.unwrap();

        // Verify metadata updated but user state preserved
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
    async fn test_mark_article_read() {
        let db = test_db().await;
        db.sync_feeds(&[test_feed(1)]).await.unwrap();
        let feeds = db.get_feeds_with_unread_counts().await.unwrap();
        db.upsert_articles(feeds[0].id, &[test_article("1", "Test")])
            .await
            .unwrap();

        let articles = db.get_articles_for_feed(feeds[0].id, None).await.unwrap();
        assert!(!articles[0].read);

        // First call should return true (changed)
        let changed = db.mark_article_read(articles[0].id).await.unwrap();
        assert!(changed);

        let articles = db.get_articles_for_feed(feeds[0].id, None).await.unwrap();
        assert!(articles[0].read);

        // Second call should return false (idempotent, already read)
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

        // Toggle returns new value (should be true now)
        let new_status = db.toggle_article_starred(articles[0].id).await.unwrap();
        assert!(new_status);

        let articles = db.get_articles_for_feed(feeds[0].id, None).await.unwrap();
        assert!(articles[0].starred);
    }

    #[tokio::test]
    async fn test_search_by_title() {
        let db = test_db().await;
        db.sync_feeds(&[test_feed(1)]).await.unwrap();
        let feeds = db.get_feeds_with_unread_counts().await.unwrap();

        db.upsert_articles(
            feeds[0].id,
            &[
                test_article("1", "Rust Programming Guide"),
                test_article("2", "Python Tutorial"),
            ],
        )
        .await
        .unwrap();

        let results = db.search_articles("Rust").await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(&*results[0].title, "Rust Programming Guide");
    }

    #[tokio::test]
    async fn test_search_empty_query() {
        let db = test_db().await;
        let results = db.search_articles("").await.unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn test_search_no_results() {
        let db = test_db().await;
        db.sync_feeds(&[test_feed(1)]).await.unwrap();
        let feeds = db.get_feeds_with_unread_counts().await.unwrap();
        db.upsert_articles(feeds[0].id, &[test_article("1", "Test Article")])
            .await
            .unwrap();

        let results = db.search_articles("nonexistent").await.unwrap();
        assert!(results.is_empty());
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
    async fn test_get_articles_pagination() {
        let db = test_db().await;
        db.sync_feeds(&[test_feed(1)]).await.unwrap();
        let feeds = db.get_feeds_with_unread_counts().await.unwrap();

        // Insert 10 articles
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
    async fn test_toggle_article_starred_twice() {
        let db = test_db().await;
        db.sync_feeds(&[test_feed(1)]).await.unwrap();
        let feeds = db.get_feeds_with_unread_counts().await.unwrap();
        db.upsert_articles(feeds[0].id, &[test_article("1", "Test")])
            .await
            .unwrap();

        let articles = db.get_articles_for_feed(feeds[0].id, None).await.unwrap();
        let initial = articles[0].starred;

        // First toggle: false -> true
        let first = db.toggle_article_starred(articles[0].id).await.unwrap();
        assert_eq!(first, !initial);

        // Second toggle: true -> false (back to initial)
        let second = db.toggle_article_starred(articles[0].id).await.unwrap();
        assert_eq!(second, initial);

        let articles = db.get_articles_for_feed(feeds[0].id, None).await.unwrap();
        assert_eq!(articles[0].starred, initial); // Back to original
    }

    #[tokio::test]
    async fn test_get_feeds_unread_counts() {
        let db = test_db().await;
        db.sync_feeds(&[test_feed(1)]).await.unwrap();
        let feeds = db.get_feeds_with_unread_counts().await.unwrap();

        // Add 3 articles
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

        // Mark one as read
        let articles = db.get_articles_for_feed(feeds[0].id, None).await.unwrap();
        db.mark_article_read(articles[0].id).await.unwrap();

        let feeds = db.get_feeds_with_unread_counts().await.unwrap();
        assert_eq!(feeds[0].unread_count, 2);
    }

    #[tokio::test]
    async fn test_sync_feeds_empty() {
        let db = test_db().await;
        // Empty slice should not error
        db.sync_feeds(&[]).await.unwrap();
        let feeds = db.get_feeds_with_unread_counts().await.unwrap();
        assert!(feeds.is_empty());
    }

    #[tokio::test]
    async fn test_sync_feeds_batch_chunking() {
        let db = test_db().await;

        // Insert 250 feeds (tests chunking at BATCH_SIZE=100)
        let feeds: Vec<OpmlFeed> = (0..250).map(test_feed).collect();
        db.sync_feeds(&feeds).await.unwrap();

        let result = db.get_feeds_with_unread_counts().await.unwrap();
        assert_eq!(result.len(), 250);

        // Verify first and last feeds
        assert!(result.iter().any(|f| &*f.title == "Test Feed 0"));
        assert!(result.iter().any(|f| &*f.title == "Test Feed 249"));
    }

    #[tokio::test]
    async fn test_sync_feeds_batch_upsert() {
        let db = test_db().await;

        // Initial insert of 150 feeds
        let feeds: Vec<OpmlFeed> = (0..150).map(test_feed).collect();
        db.sync_feeds(&feeds).await.unwrap();

        // Update with mix of existing and new feeds
        let mut updated_feeds: Vec<OpmlFeed> = (100..200)
            .map(|i| OpmlFeed {
                title: format!("Updated Feed {}", i),
                xml_url: format!("https://feed{}.example.com/rss", i),
                html_url: Some(format!("https://feed{}.example.com", i)),
            })
            .collect();
        // Keep some with same title to verify ON CONFLICT works
        updated_feeds.extend((0..50).map(test_feed));

        db.sync_feeds(&updated_feeds).await.unwrap();

        let result = db.get_feeds_with_unread_counts().await.unwrap();
        // Should have 200 total: 0-99 original, 100-149 updated, 150-199 new
        assert_eq!(result.len(), 200);

        // Check that feeds 100-149 were updated
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
    async fn test_fts_consistency_check_consistent() {
        let db = test_db().await;
        db.sync_feeds(&[test_feed(1)]).await.unwrap();
        let feeds = db.get_feeds_with_unread_counts().await.unwrap();

        db.upsert_articles(
            feeds[0].id,
            &[
                test_article("1", "Article One"),
                test_article("2", "Article Two"),
            ],
        )
        .await
        .unwrap();

        // FTS should be consistent after normal inserts via triggers
        let consistent = db.check_fts_consistency().await.unwrap();
        assert!(consistent);
    }

    #[tokio::test]
    async fn test_fts_rebuild_index() {
        let db = test_db().await;
        db.sync_feeds(&[test_feed(1)]).await.unwrap();
        let feeds = db.get_feeds_with_unread_counts().await.unwrap();

        db.upsert_articles(
            feeds[0].id,
            &[
                test_article("1", "Rust Programming"),
                test_article("2", "Python Tutorial"),
                test_article("3", "Go Handbook"),
            ],
        )
        .await
        .unwrap();

        // Rebuild should return correct count
        let count = db.rebuild_fts_index().await.unwrap();
        assert_eq!(count, 3);

        // Search should still work after rebuild
        let results = db.search_articles("Rust").await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(&*results[0].title, "Rust Programming");
    }

    #[tokio::test]
    async fn test_fts_rebuild_empty_table() {
        let db = test_db().await;

        // Rebuild on empty database should return 0
        let count = db.rebuild_fts_index().await.unwrap();
        assert_eq!(count, 0);

        // Consistency check should pass
        let consistent = db.check_fts_consistency().await.unwrap();
        assert!(consistent);
    }

    #[tokio::test]
    async fn test_fts_consistency_detailed_consistent() {
        let db = test_db().await;
        db.sync_feeds(&[test_feed(1)]).await.unwrap();
        let feeds = db.get_feeds_with_unread_counts().await.unwrap();

        db.upsert_articles(
            feeds[0].id,
            &[
                test_article("1", "Article One"),
                test_article("2", "Article Two"),
            ],
        )
        .await
        .unwrap();

        // Detailed report should show consistent state
        let report = db.check_fts_consistency_detailed().await.unwrap();
        assert!(report.is_consistent);
        assert_eq!(report.articles_count, 2);
        assert_eq!(report.fts_count, 2);
        assert_eq!(report.orphaned_fts_entries, 0);
        assert_eq!(report.missing_fts_entries, 0);
    }

    #[tokio::test]
    async fn test_fts_consistency_detailed_empty() {
        let db = test_db().await;

        // Empty database should be consistent
        let report = db.check_fts_consistency_detailed().await.unwrap();
        assert!(report.is_consistent);
        assert_eq!(report.articles_count, 0);
        assert_eq!(report.fts_count, 0);
        assert_eq!(report.orphaned_fts_entries, 0);
        assert_eq!(report.missing_fts_entries, 0);
    }

    // NOTE on FTS5 external content consistency checking:
    //
    // With FTS5 external content mode (content=articles), SELECT queries without
    // MATCH are redirected to the content table. This means check_fts_consistency_detailed()
    // compares article count with what's accessible via FTS.
    //
    // For full FTS health, use rebuild_fts_index() periodically or when search
    // results seem incomplete.

    #[tokio::test]
    async fn test_fts_rebuild_maintains_searchability() {
        // Test that rebuild_fts_index maintains full searchability
        let db = test_db().await;
        db.sync_feeds(&[test_feed(1)]).await.unwrap();
        let feeds = db.get_feeds_with_unread_counts().await.unwrap();

        db.upsert_articles(
            feeds[0].id,
            &[
                test_article("guid1", "Important Document"),
                test_article("guid2", "Another Article"),
            ],
        )
        .await
        .unwrap();

        // Verify searchable
        assert_eq!(db.search_articles("Important").await.unwrap().len(), 1);
        assert_eq!(db.search_articles("Another").await.unwrap().len(), 1);

        // Rebuild should maintain searchability
        let count = db.rebuild_fts_index().await.unwrap();
        assert_eq!(count, 2);

        // Still searchable after rebuild
        assert_eq!(db.search_articles("Important").await.unwrap().len(), 1);
        assert_eq!(db.search_articles("Another").await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn test_fts_consistency_after_operations() {
        // Test that consistency check reports correctly after various operations
        let db = test_db().await;
        db.sync_feeds(&[test_feed(1)]).await.unwrap();
        let feeds = db.get_feeds_with_unread_counts().await.unwrap();

        // Initially empty and consistent
        let report = db.check_fts_consistency_detailed().await.unwrap();
        assert!(report.is_consistent);
        assert_eq!(report.articles_count, 0);
        assert_eq!(report.fts_count, 0);

        // Add articles
        db.upsert_articles(
            feeds[0].id,
            &[
                test_article("g1", "Article One"),
                test_article("g2", "Article Two"),
            ],
        )
        .await
        .unwrap();

        // Still consistent after inserts (triggers maintain FTS)
        let report = db.check_fts_consistency_detailed().await.unwrap();
        assert!(report.is_consistent);
        assert_eq!(report.articles_count, 2);
        assert_eq!(report.fts_count, 2);

        // Rebuild and verify still consistent
        db.rebuild_fts_index().await.unwrap();
        let report = db.check_fts_consistency_detailed().await.unwrap();
        assert!(report.is_consistent);
        assert_eq!(report.articles_count, 2);
        assert_eq!(report.fts_count, 2);
    }

    #[test]
    fn test_validate_fts_query_length_limit() {
        // Query at limit should pass
        let query_at_limit = "a".repeat(super::MAX_QUERY_LENGTH);
        assert!(super::validate_fts_query(&query_at_limit).is_ok());

        // Query over limit should fail
        let query_over_limit = "a".repeat(super::MAX_QUERY_LENGTH + 1);
        let result = super::validate_fts_query(&query_over_limit);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("maximum length"));
    }

    #[test]
    fn test_validate_fts_query_wildcard_limit() {
        // 3 wildcards should pass
        let query_ok = "foo* bar* baz*";
        assert!(super::validate_fts_query(query_ok).is_ok());

        // 4 wildcards should fail
        let query_too_many = "foo* bar* baz* qux*";
        let result = super::validate_fts_query(query_too_many);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("wildcards"));
    }

    #[test]
    fn test_validate_fts_query_or_limit() {
        // 5 OR operators should pass
        let query_ok = "a OR b OR c OR d OR e OR f";
        assert!(super::validate_fts_query(query_ok).is_ok());

        // 6 OR operators should fail
        let query_too_many = "a OR b OR c OR d OR e OR f OR g";
        let result = super::validate_fts_query(query_too_many);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("OR operators"));
    }

    #[test]
    fn test_validate_fts_query_or_case_insensitive() {
        // Case-insensitive: lowercase "or" counts as OR operator
        let query_lowercase = "a or b or c or d or e or f";
        assert!(super::validate_fts_query(query_lowercase).is_ok()); // 5 ORs = ok

        let query_lowercase_too_many = "a or b or c or d or e or f or g";
        let result = super::validate_fts_query(query_lowercase_too_many);
        assert!(result.is_err()); // 6 ORs = error

        // Mixed case also counts
        let query_mixed = "a Or b oR c OR d OR e OR f OR g";
        let result = super::validate_fts_query(query_mixed);
        assert!(result.is_err()); // 6 ORs = error
    }

    #[test]
    fn test_validate_fts_query_parentheses_limit() {
        // 5 parentheses should pass
        let query_ok = "(a) AND (b) AND (c) AND (d) AND (e)";
        assert!(super::validate_fts_query(query_ok).is_ok());

        // 6 parentheses should fail
        let query_too_many = "(a) AND (b) AND (c) AND (d) AND (e) AND (f)";
        let result = super::validate_fts_query(query_too_many);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("parentheses"));
    }

    #[test]
    fn test_validate_fts_query_unbalanced_parentheses() {
        // BUG-014: Unbalanced parentheses should fail
        let query_missing_close = "(a AND b";
        let result = super::validate_fts_query(query_missing_close);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("unbalanced"));

        let query_missing_open = "a AND b)";
        let result = super::validate_fts_query(query_missing_open);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("unbalanced"));

        // Balanced parentheses should pass
        let query_balanced = "(a AND b)";
        assert!(super::validate_fts_query(query_balanced).is_ok());
    }

    #[test]
    fn test_validate_fts_query_and_limit() {
        // 10 AND operators should pass
        let query_ok = "a AND b AND c AND d AND e AND f AND g AND h AND i AND j AND k";
        assert!(super::validate_fts_query(query_ok).is_ok());

        // 11 AND operators should fail
        let query_too_many = "a AND b AND c AND d AND e AND f AND g AND h AND i AND j AND k AND l";
        let result = super::validate_fts_query(query_too_many);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("AND operators"));
    }

    #[test]
    fn test_validate_fts_query_and_case_insensitive() {
        // Case-insensitive: lowercase "and" counts as AND operator
        let query_lowercase = "a and b and c and d and e and f and g and h and i and j and k";
        assert!(super::validate_fts_query(query_lowercase).is_ok()); // 10 ANDs = ok

        let query_lowercase_too_many =
            "a and b and c and d and e and f and g and h and i and j and k and l";
        let result = super::validate_fts_query(query_lowercase_too_many);
        assert!(result.is_err()); // 11 ANDs = error

        // Mixed case also counts
        let query_mixed = "a And b aNd c AND d AND e AND f AND g AND h AND i AND j AND k AND l";
        let result = super::validate_fts_query(query_mixed);
        assert!(result.is_err()); // 11 ANDs = error
    }

    #[tokio::test]
    async fn test_search_articles_rejects_long_query() {
        let db = test_db().await;
        let long_query = "a".repeat(super::MAX_QUERY_LENGTH + 1);
        let result = db.search_articles(&long_query).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("maximum length"));
    }

    #[tokio::test]
    async fn test_search_articles_rejects_too_many_wildcards() {
        let db = test_db().await;
        let result = db.search_articles("a* b* c* d*").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("wildcards"));
    }

    #[tokio::test]
    async fn test_search_articles_rejects_too_many_or() {
        let db = test_db().await;
        let result = db.search_articles("a OR b OR c OR d OR e OR f OR g").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("OR operators"));
    }

    #[tokio::test]
    async fn test_search_articles_rejects_too_many_parentheses() {
        let db = test_db().await;
        let result = db
            .search_articles("(a) AND (b) AND (c) AND (d) AND (e) AND (f)")
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("parentheses"));
    }

    #[tokio::test]
    async fn test_search_articles_rejects_too_many_and() {
        let db = test_db().await;
        let result = db
            .search_articles("a AND b AND c AND d AND e AND f AND g AND h AND i AND j AND k AND l")
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("AND operators"));
    }

    #[tokio::test]
    async fn test_batch_set_feed_errors() {
        let db = test_db().await;

        // Create 3 feeds
        db.sync_feeds(&[test_feed(1), test_feed(2), test_feed(3)])
            .await
            .unwrap();

        let feeds = db.get_feeds_with_unread_counts().await.unwrap();
        assert_eq!(feeds.len(), 3);

        // Batch update: set error for feed 1, clear for feed 2, set different error for feed 3
        let updates = vec![
            (feeds[0].id, Some("Network error".to_string())),
            (feeds[1].id, None),
            (feeds[2].id, Some("Parse error".to_string())),
        ];

        db.batch_set_feed_errors(&updates).await.unwrap();

        // Verify updates
        let feeds = db.get_feeds_with_unread_counts().await.unwrap();
        assert_eq!(feeds[0].error, Some("Network error".to_string()));
        assert_eq!(feeds[1].error, None);
        assert_eq!(feeds[2].error, Some("Parse error".to_string()));
    }

    #[tokio::test]
    async fn test_batch_set_feed_errors_empty() {
        let db = test_db().await;

        // Empty batch should not error
        db.batch_set_feed_errors(&[]).await.unwrap();
    }

    #[tokio::test]
    async fn test_complete_feed_refresh_atomic() {
        let db = test_db().await;
        db.sync_feeds(&[test_feed(1)]).await.unwrap();
        let feeds = db.get_feeds_with_unread_counts().await.unwrap();
        let feed_id = feeds[0].id;

        // Set an initial error on the feed
        db.set_feed_error(feed_id, Some("Previous error"))
            .await
            .unwrap();

        // Complete a feed refresh with some articles
        let articles = vec![
            test_article("guid-1", "Article 1"),
            test_article("guid-2", "Article 2"),
        ];
        let count = db.complete_feed_refresh(feed_id, &articles).await.unwrap();
        assert_eq!(count, 2);

        // Verify error was cleared, articles inserted, and timestamp updated
        let feeds = db.get_feeds_with_unread_counts().await.unwrap();
        assert!(feeds[0].error.is_none(), "Error should be cleared");
        assert!(
            feeds[0].last_fetched.is_some(),
            "last_fetched should be set"
        );
        assert_eq!(feeds[0].unread_count, 2);

        // Verify articles exist
        let stored = db.get_articles_for_feed(feed_id, None).await.unwrap();
        assert_eq!(stored.len(), 2);
    }

    #[tokio::test]
    async fn test_complete_feed_refresh_empty_articles() {
        let db = test_db().await;
        db.sync_feeds(&[test_feed(1)]).await.unwrap();
        let feeds = db.get_feeds_with_unread_counts().await.unwrap();
        let feed_id = feeds[0].id;

        // Complete with empty articles list
        let count = db.complete_feed_refresh(feed_id, &[]).await.unwrap();
        assert_eq!(count, 0);

        // Verify timestamp was still updated
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

        // Insert initial article
        let count = db
            .complete_feed_refresh(feed_id, &[test_article("existing", "Original")])
            .await
            .unwrap();
        assert_eq!(count, 1);

        // Mark as read
        let articles = db.get_articles_for_feed(feed_id, None).await.unwrap();
        db.mark_article_read(articles[0].id).await.unwrap();

        // Refresh with mix of existing and new
        let articles = vec![
            test_article("existing", "Updated Title"),
            test_article("new-1", "New Article"),
        ];
        let count = db.complete_feed_refresh(feed_id, &articles).await.unwrap();
        assert_eq!(count, 1); // Only 1 new article

        // Verify user state preserved
        let stored = db.get_articles_for_feed(feed_id, None).await.unwrap();
        let existing = stored.iter().find(|a| a.guid == "existing").unwrap();
        assert!(existing.read, "Read status should be preserved");
        assert_eq!(&*existing.title, "Updated Title"); // Title updated
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

        // Initial count should be 0
        assert_eq!(feeds[0].consecutive_failures, 0);

        // Increment failures
        let count = db.increment_feed_failures(feed_id).await.unwrap();
        assert_eq!(count, 1);

        let count = db.increment_feed_failures(feed_id).await.unwrap();
        assert_eq!(count, 2);

        // Verify in database
        let feeds = db.get_feeds_with_unread_counts().await.unwrap();
        assert_eq!(feeds[0].consecutive_failures, 2);
    }

    #[tokio::test]
    async fn test_reset_feed_failures() {
        let db = test_db().await;
        db.sync_feeds(&[test_feed(1)]).await.unwrap();
        let feeds = db.get_feeds_with_unread_counts().await.unwrap();
        let feed_id = feeds[0].id;

        // Increment to 3
        for _ in 0..3 {
            db.increment_feed_failures(feed_id).await.unwrap();
        }

        // Reset
        db.reset_feed_failures(feed_id).await.unwrap();

        // Verify reset
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

        // Set feed 2 to exactly threshold (should be filtered)
        for _ in 0..Database::CIRCUIT_BREAKER_THRESHOLD {
            db.increment_feed_failures(feeds[1].id).await.unwrap();
        }

        // Set feed 3 to one below threshold (should be included)
        for _ in 0..(Database::CIRCUIT_BREAKER_THRESHOLD - 1) {
            db.increment_feed_failures(feeds[2].id).await.unwrap();
        }

        // Get active feeds
        let active = db.get_active_feeds().await.unwrap();
        assert_eq!(active.len(), 2, "Should exclude feed with 5 failures");

        // Verify feed 2 is not in active list
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

        // Increment failures to 4 (one below threshold)
        for _ in 0..4 {
            db.increment_feed_failures(feed_id).await.unwrap();
        }

        // Complete a successful refresh
        db.complete_feed_refresh(feed_id, &[test_article("1", "Test")])
            .await
            .unwrap();

        // Verify failures reset
        let feeds = db.get_feeds_with_unread_counts().await.unwrap();
        assert_eq!(
            feeds[0].consecutive_failures, 0,
            "Successful refresh should reset failure count"
        );
    }

    #[tokio::test]
    async fn test_circuit_breaker_threshold_constant() {
        // Verify the threshold is as documented
        assert_eq!(
            Database::CIRCUIT_BREAKER_THRESHOLD,
            5,
            "Circuit breaker threshold should be 5"
        );
    }
}
