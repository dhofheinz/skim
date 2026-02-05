use anyhow::Result;

use super::schema::Database;
use super::types::{Article, ArticleDbRow, DatabaseError, FtsConsistencyReport};

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

impl Database {
    // ========================================================================
    // Search Operations
    // ========================================================================

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

        let count = db.rebuild_fts_index().await.unwrap();
        assert_eq!(count, 3);

        let results = db.search_articles("Rust").await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(&*results[0].title, "Rust Programming");
    }

    #[tokio::test]
    async fn test_fts_rebuild_empty_table() {
        let db = test_db().await;

        let count = db.rebuild_fts_index().await.unwrap();
        assert_eq!(count, 0);

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

        assert_eq!(db.search_articles("Important").await.unwrap().len(), 1);
        assert_eq!(db.search_articles("Another").await.unwrap().len(), 1);

        let count = db.rebuild_fts_index().await.unwrap();
        assert_eq!(count, 2);

        assert_eq!(db.search_articles("Important").await.unwrap().len(), 1);
        assert_eq!(db.search_articles("Another").await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn test_fts_consistency_after_operations() {
        let db = test_db().await;
        db.sync_feeds(&[test_feed(1)]).await.unwrap();
        let feeds = db.get_feeds_with_unread_counts().await.unwrap();

        let report = db.check_fts_consistency_detailed().await.unwrap();
        assert!(report.is_consistent);
        assert_eq!(report.articles_count, 0);
        assert_eq!(report.fts_count, 0);

        db.upsert_articles(
            feeds[0].id,
            &[
                test_article("g1", "Article One"),
                test_article("g2", "Article Two"),
            ],
        )
        .await
        .unwrap();

        let report = db.check_fts_consistency_detailed().await.unwrap();
        assert!(report.is_consistent);
        assert_eq!(report.articles_count, 2);
        assert_eq!(report.fts_count, 2);

        db.rebuild_fts_index().await.unwrap();
        let report = db.check_fts_consistency_detailed().await.unwrap();
        assert!(report.is_consistent);
        assert_eq!(report.articles_count, 2);
        assert_eq!(report.fts_count, 2);
    }

    #[test]
    fn test_validate_fts_query_length_limit() {
        let query_at_limit = "a".repeat(super::MAX_QUERY_LENGTH);
        assert!(super::validate_fts_query(&query_at_limit).is_ok());

        let query_over_limit = "a".repeat(super::MAX_QUERY_LENGTH + 1);
        let result = super::validate_fts_query(&query_over_limit);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("maximum length"));
    }

    #[test]
    fn test_validate_fts_query_wildcard_limit() {
        let query_ok = "foo* bar* baz*";
        assert!(super::validate_fts_query(query_ok).is_ok());

        let query_too_many = "foo* bar* baz* qux*";
        let result = super::validate_fts_query(query_too_many);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("wildcards"));
    }

    #[test]
    fn test_validate_fts_query_or_limit() {
        let query_ok = "a OR b OR c OR d OR e OR f";
        assert!(super::validate_fts_query(query_ok).is_ok());

        let query_too_many = "a OR b OR c OR d OR e OR f OR g";
        let result = super::validate_fts_query(query_too_many);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("OR operators"));
    }

    #[test]
    fn test_validate_fts_query_or_case_insensitive() {
        let query_lowercase = "a or b or c or d or e or f";
        assert!(super::validate_fts_query(query_lowercase).is_ok());

        let query_lowercase_too_many = "a or b or c or d or e or f or g";
        let result = super::validate_fts_query(query_lowercase_too_many);
        assert!(result.is_err());

        let query_mixed = "a Or b oR c OR d OR e OR f OR g";
        let result = super::validate_fts_query(query_mixed);
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_fts_query_parentheses_limit() {
        let query_ok = "(a) AND (b) AND (c) AND (d) AND (e)";
        assert!(super::validate_fts_query(query_ok).is_ok());

        let query_too_many = "(a) AND (b) AND (c) AND (d) AND (e) AND (f)";
        let result = super::validate_fts_query(query_too_many);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("parentheses"));
    }

    #[test]
    fn test_validate_fts_query_unbalanced_parentheses() {
        let query_missing_close = "(a AND b";
        let result = super::validate_fts_query(query_missing_close);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("unbalanced"));

        let query_missing_open = "a AND b)";
        let result = super::validate_fts_query(query_missing_open);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("unbalanced"));

        let query_balanced = "(a AND b)";
        assert!(super::validate_fts_query(query_balanced).is_ok());
    }

    #[test]
    fn test_validate_fts_query_and_limit() {
        let query_ok = "a AND b AND c AND d AND e AND f AND g AND h AND i AND j AND k";
        assert!(super::validate_fts_query(query_ok).is_ok());

        let query_too_many = "a AND b AND c AND d AND e AND f AND g AND h AND i AND j AND k AND l";
        let result = super::validate_fts_query(query_too_many);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("AND operators"));
    }

    #[test]
    fn test_validate_fts_query_and_case_insensitive() {
        let query_lowercase = "a and b and c and d and e and f and g and h and i and j and k";
        assert!(super::validate_fts_query(query_lowercase).is_ok());

        let query_lowercase_too_many =
            "a and b and c and d and e and f and g and h and i and j and k and l";
        let result = super::validate_fts_query(query_lowercase_too_many);
        assert!(result.is_err());

        let query_mixed = "a And b aNd c AND d AND e AND f AND g AND h AND i AND j AND k AND l";
        let result = super::validate_fts_query(query_mixed);
        assert!(result.is_err());
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
}
