use std::sync::Arc;
use thiserror::Error;

use crate::util::strip_control_chars;

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
        // Primary: check structured error codes (reliable across SQLite versions)
        if let sqlx::Error::Database(ref db_err) = err {
            if let Some(code) = db_err.code() {
                match code.as_ref() {
                    "5" | "6" => return DatabaseError::InstanceLocked, // BUSY | LOCKED
                    _ => {}
                }
            }
        }

        // Fallback: string matching for edge cases (connection errors, CANTOPEN)
        let error_string = err.to_string().to_lowercase();
        if error_string.contains("database is locked")
            || error_string.contains("database table is locked")
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
    /// SEC-001: Sanitizes all external string fields at the DB boundary.
    /// P-9: strip_control_chars uses a byte-scan fast path — for already-clean content
    /// it returns Cow::Borrowed with no allocation, making repeated calls negligible.
    pub(crate) fn into_article(self) -> Article {
        Article {
            id: self.id,
            feed_id: self.feed_id,
            guid: self.guid,
            title: Arc::from(strip_control_chars(&self.title)),
            url: self.url.map(|u| Arc::from(strip_control_chars(&u))),
            published: self.published,
            summary: self.summary.map(|s| Arc::from(strip_control_chars(&s))),
            content: self.content.map(|c| Arc::from(strip_control_chars(&c))),
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
    /// SEC-001: Sanitizes all external string fields at the DB boundary
    pub(crate) fn into_tuple(self) -> (i64, Article) {
        (
            self.feed_id,
            Article {
                id: self.id,
                feed_id: self.feed_id,
                guid: self.guid,
                title: Arc::from(strip_control_chars(&self.title)),
                url: self.url.map(|u| Arc::from(strip_control_chars(&u))),
                published: self.published,
                summary: self.summary.map(|s| Arc::from(strip_control_chars(&s))),
                content: self.content.map(|c| Arc::from(strip_control_chars(&c))),
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

#[cfg(test)]
mod tests {
    use super::DatabaseError;
    use std::borrow::Cow;
    use std::fmt;

    /// Mock SQLite database error for testing error code matching
    #[derive(Debug)]
    struct MockDbError {
        message: String,
        code: Option<String>,
    }

    impl fmt::Display for MockDbError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "{}", self.message)
        }
    }

    impl std::error::Error for MockDbError {}

    impl sqlx::error::DatabaseError for MockDbError {
        fn message(&self) -> &str {
            &self.message
        }

        fn code(&self) -> Option<Cow<'_, str>> {
            self.code.as_deref().map(Cow::Borrowed)
        }

        fn kind(&self) -> sqlx::error::ErrorKind {
            sqlx::error::ErrorKind::Other
        }

        fn as_error(&self) -> &(dyn std::error::Error + Send + Sync + 'static) {
            self
        }

        fn as_error_mut(&mut self) -> &mut (dyn std::error::Error + Send + Sync + 'static) {
            self
        }

        fn into_error(self: Box<Self>) -> Box<dyn std::error::Error + Send + Sync + 'static> {
            self
        }
    }

    fn db_error(message: &str, code: Option<&str>) -> sqlx::Error {
        sqlx::Error::Database(Box::new(MockDbError {
            message: message.to_string(),
            code: code.map(String::from),
        }))
    }

    #[test]
    fn test_error_code_5_returns_instance_locked() {
        let err = db_error("database is locked", Some("5"));
        assert!(matches!(
            DatabaseError::from_sqlx(err),
            DatabaseError::InstanceLocked
        ));
    }

    #[test]
    fn test_error_code_6_returns_instance_locked() {
        let err = db_error("database table is locked", Some("6"));
        assert!(matches!(
            DatabaseError::from_sqlx(err),
            DatabaseError::InstanceLocked
        ));
    }

    #[test]
    fn test_unknown_code_with_lock_message_falls_through_to_string_match() {
        let err = db_error("database is locked", Some("99"));
        assert!(matches!(
            DatabaseError::from_sqlx(err),
            DatabaseError::InstanceLocked
        ));
    }

    #[test]
    fn test_no_code_with_lock_message_falls_through_to_string_match() {
        let err = db_error("database is locked", None);
        assert!(matches!(
            DatabaseError::from_sqlx(err),
            DatabaseError::InstanceLocked
        ));
    }

    #[test]
    fn test_cantopen_message_returns_instance_locked() {
        let err = db_error("unable to open database file", Some("14"));
        assert!(matches!(
            DatabaseError::from_sqlx(err),
            DatabaseError::InstanceLocked
        ));
    }

    #[test]
    fn test_unrelated_error_returns_other() {
        let err = db_error("syntax error near SELECT", Some("1"));
        assert!(matches!(
            DatabaseError::from_sqlx(err),
            DatabaseError::Other(_)
        ));
    }

    #[test]
    fn test_non_database_error_variant_returns_other() {
        let err = sqlx::Error::PoolTimedOut;
        assert!(matches!(
            DatabaseError::from_sqlx(err),
            DatabaseError::Other(_)
        ));
    }
}
