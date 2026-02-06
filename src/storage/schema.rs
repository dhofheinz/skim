use anyhow::Result;
use sqlx::{
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
    SqlitePool,
};
use std::str::FromStr;
use std::time::Duration;

use super::types::DatabaseError;

// ============================================================================
// Database
// ============================================================================

#[derive(Clone)]
pub struct Database {
    pub(crate) pool: SqlitePool,
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

        // SEC-010: Set database file permissions BEFORE pool creation
        // Ensures no window where the file exists with default umask permissions
        #[cfg(unix)]
        if path != ":memory:" {
            use std::os::unix::fs::PermissionsExt;
            let db_path = std::path::Path::new(path);
            if db_path.exists() {
                let perms = std::fs::Permissions::from_mode(0o600);
                if let Err(e) = std::fs::set_permissions(path, perms) {
                    tracing::warn!(path = %path, error = %e, "SEC-010: Failed to set database file permissions");
                }
            } else if let Some(parent) = db_path.parent() {
                if parent.exists() {
                    // SEC-010/S-6: Pre-create DB file with mode(0o600) atomically
                    // Using OpenOptionsExt::mode() sets permissions at creation time,
                    // eliminating the TOCTOU window between create and chmod.
                    use std::os::unix::fs::OpenOptionsExt;
                    let _file = std::fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .mode(0o600)
                        .open(db_path)
                        .ok(); // If creation fails, SQLite will report the error at connect_with.
                }
            }
        }

        // Configure SQLite connection options with busy_timeout pragma.
        // busy_timeout=5000: SQLite waits up to 5 seconds for locks to release before returning SQLITE_BUSY.
        // This handles transient lock contention (e.g., concurrent refresh operations) automatically.
        // Using pragma() ensures all connections in the pool inherit this setting.
        let options = SqliteConnectOptions::from_str(&url)
            .map_err(DatabaseError::from_sqlx)?
            .pragma("busy_timeout", "5000");
        // SQLite is single-writer; 5 connections covers 3-5 peak concurrent readers
        // (feed fetches + content loads + UI queries).
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
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

        // Create user preferences table (key-value store for user settings)
        // Keys use dotted convention: theme.variant, keybind.quit, session.view, etc.
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS user_preferences (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL,
                updated_at TEXT NOT NULL DEFAULT (datetime('now'))
            )
        "#,
        )
        .execute(&mut *tx)
        .await?;

        // Commit all migrations atomically
        tx.commit().await?;

        Ok(())
    }
}
