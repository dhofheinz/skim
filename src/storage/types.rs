use std::sync::Arc;
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
    pub(crate) fn from_sqlx(err: sqlx::Error) -> Self {
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
// Helper Types
// ============================================================================

/// Row type for feed query with unread count
pub(crate) type FeedRow = (
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
pub(crate) struct ArticleDbRow {
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
    pub(crate) fn into_article(self) -> Article {
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
pub(crate) struct ArticleRow {
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
    pub(crate) fn into_tuple(self) -> (i64, Article) {
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
