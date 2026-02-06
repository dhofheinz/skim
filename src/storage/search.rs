use std::time::Duration;

use anyhow::Result;

use super::schema::Database;
use super::types::{Article, ArticleDbRow, DatabaseError, FtsConsistencyReport, SearchScope};

// ============================================================================
// FTS5 Query Validation
// ============================================================================

use crate::util::MAX_SEARCH_QUERY_LENGTH;
const MAX_WILDCARDS: usize = 3;
const MAX_OR_OPERATORS: usize = 5;
const MAX_PARENTHESES: usize = 5;
const MAX_AND_OPERATORS: usize = 10;
const MAX_TERM_LENGTH: usize = 64;
const MAX_NESTING_DEPTH: usize = 3;

// ============================================================================
// Query Limit Constants
// ============================================================================

/// Maximum number of articles to return from any single query (OOM protection)
const MAX_ARTICLES: i64 = 2000;

/// SEC-012: Maximum time to wait for FTS5 query execution before falling back to LIKE
const SEARCH_TIMEOUT: Duration = Duration::from_secs(5);

/// SEC-013: Maximum time to wait for FTS consistency check before failing
const FTS_CONSISTENCY_TIMEOUT: Duration = Duration::from_secs(10);

/// Maximum time to wait for FTS5 rebuild before failing
const REBUILD_TIMEOUT: Duration = Duration::from_secs(30);

/// Validate FTS5 query complexity to prevent DoS via expensive wildcard expansions.
///
/// PERF-015: Single-pass byte-level validation replaces 6+ passes with 0 allocations.
/// Uses `eq_ignore_ascii_case` on byte windows for case-insensitive operator detection.
///
/// Limits:
/// - Maximum query length: 256 characters
/// - Maximum wildcards (*): 3
/// - Maximum OR operators: 5
/// - Maximum parentheses: 5 (BUG-008)
/// - Maximum AND operators: 10 (BUG-008)
/// - Maximum nesting depth: 3 (SEC-012)
fn validate_fts_query(query: &str) -> Result<()> {
    if query.len() > MAX_SEARCH_QUERY_LENGTH {
        anyhow::bail!(
            "Search query exceeds maximum length of {} characters",
            MAX_SEARCH_QUERY_LENGTH
        );
    }

    let mut wildcards = 0usize;
    let mut or_count = 0usize;
    let mut and_count = 0usize;
    let mut open_parens = 0usize;
    let mut close_parens = 0usize;
    let mut depth = 0usize;
    let mut max_depth = 0usize;
    let mut current_term_len = 0usize;
    let mut max_term_len = 0usize;

    let bytes = query.as_bytes();
    let len = bytes.len();
    let mut i = 0;

    while i < len {
        match bytes[i] {
            b'*' => {
                wildcards += 1;
                max_term_len = max_term_len.max(current_term_len);
                current_term_len = 0;
            }
            b'(' => {
                open_parens += 1;
                depth += 1;
                max_depth = max_depth.max(depth);
                max_term_len = max_term_len.max(current_term_len);
                current_term_len = 0;
            }
            b')' => {
                close_parens += 1;
                depth = depth.saturating_sub(1);
                max_term_len = max_term_len.max(current_term_len);
                current_term_len = 0;
            }
            b'"' => { /* decorator, skip */ }
            // SEC-012: Reject FTS5 column filter syntax to prevent scope bypass
            // e.g., `{content} : secret` would bypass TitleAndSummary scope
            b'{' | b'}' => {
                anyhow::bail!("Search query contains invalid characters");
            }
            b' ' | b'\t' => {
                max_term_len = max_term_len.max(current_term_len);
                current_term_len = 0;
                // Check for " OR " (4 bytes) case-insensitively
                if i + 4 <= len && bytes[i..i + 4].eq_ignore_ascii_case(b" OR ") {
                    or_count += 1;
                    i += 3;
                } else if i + 5 <= len && bytes[i..i + 5].eq_ignore_ascii_case(b" AND ") {
                    and_count += 1;
                    i += 4;
                }
            }
            _ => {
                current_term_len += 1;
            }
        }
        i += 1;
    }
    max_term_len = max_term_len.max(current_term_len);

    if wildcards > MAX_WILDCARDS {
        anyhow::bail!(
            "Search query contains too many wildcards (max {})",
            MAX_WILDCARDS
        );
    }
    if or_count > MAX_OR_OPERATORS {
        anyhow::bail!(
            "Search query contains too many OR operators (max {})",
            MAX_OR_OPERATORS
        );
    }
    if open_parens > MAX_PARENTHESES {
        anyhow::bail!(
            "Search query contains too many parentheses (max {})",
            MAX_PARENTHESES
        );
    }
    if open_parens != close_parens {
        anyhow::bail!("Search query has unbalanced parentheses");
    }
    if max_depth > MAX_NESTING_DEPTH {
        anyhow::bail!(
            "Search query nesting too deep (max {} levels)",
            MAX_NESTING_DEPTH
        );
    }
    if and_count > MAX_AND_OPERATORS {
        anyhow::bail!(
            "Search query contains too many AND operators (max {})",
            MAX_AND_OPERATORS
        );
    }
    if max_term_len > MAX_TERM_LENGTH {
        anyhow::bail!("Search term too long (max {} characters)", MAX_TERM_LENGTH);
    }

    Ok(())
}

impl Database {
    // ========================================================================
    // Search Operations
    // ========================================================================

    /// Search articles using FTS5 with configurable scope.
    ///
    /// `SearchScope::TitleAndSummary` restricts to title+summary columns (original behavior).
    /// `SearchScope::All` searches title, summary, AND content for full-text search.
    ///
    /// Uses FTS5 for fast search with LIKE fallback for syntax errors or timeout.
    /// PERF-003: Hard cap at MAX_ARTICLES (2000) to prevent OOM.
    /// SEC-012: FTS5 query wrapped with 5s timeout to prevent CPU-bound DoS.
    pub async fn search_articles(&self, query: &str, scope: SearchScope) -> Result<Vec<Article>> {
        // Early return for empty/whitespace-only queries
        let query = query.trim();
        if query.is_empty() {
            return Ok(Vec::new());
        }

        // Validate query complexity to prevent DoS via expensive wildcard expansions
        validate_fts_query(query)?;

        tracing::debug!(limit = MAX_ARTICLES, query = %query, scope = ?scope, "search_articles with limit cap");

        // Build FTS5 MATCH expression: column filter for TitleAndSummary, bare query for All
        let fts_query = match scope {
            SearchScope::TitleAndSummary => format!("{{title summary}} : {}", query),
            SearchScope::All => query.to_string(),
        };

        // PERF-002: Try FTS5 MATCH first for fast search
        // SEC-012: Wrap with timeout to prevent CPU-bound queries from blocking
        let fts_result = tokio::time::timeout(
            SEARCH_TIMEOUT,
            sqlx::query_as::<_, ArticleDbRow>(
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
            .bind(&fts_query)
            .bind(MAX_ARTICLES)
            .fetch_all(&self.pool),
        )
        .await;

        // Fall back to LIKE for queries that fail FTS5 syntax or time out
        match fts_result {
            Ok(Ok(rows)) => Ok(rows.into_iter().map(ArticleDbRow::into_article).collect()),
            Ok(Err(e)) => {
                tracing::warn!(error = %e, query = %query, "FTS5 search failed, falling back to LIKE");
                self.like_fallback(query).await
            }
            Err(_elapsed) => {
                tracing::warn!(query = %query, "FTS5 search timed out after 5s, falling back to LIKE");
                self.like_fallback(query).await
            }
        }
    }

    /// Update the FTS5 content column for a specific article.
    ///
    /// Sets `articles.content` which triggers the FTS5 UPDATE trigger to sync
    /// the content into the search index.
    #[allow(dead_code)] // Consumed by TASK-4, TASK-6
    pub async fn index_content(&self, article_id: i64, content: &str) -> Result<(), DatabaseError> {
        sqlx::query("UPDATE articles SET content = ? WHERE id = ?")
            .bind(content)
            .bind(article_id)
            .execute(&self.pool)
            .await
            .map_err(DatabaseError::from_sqlx)?;
        Ok(())
    }

    /// LIKE-based search fallback when FTS5 fails or times out.
    async fn like_fallback(&self, query: &str) -> Result<Vec<Article>> {
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
    /// SEC-013: Wrapped with 10s timeout to prevent blocking on large/corrupted databases.
    ///
    /// # Returns
    ///
    /// A detailed [`FtsConsistencyReport`] with counts and consistency status.
    pub async fn check_fts_consistency_detailed(
        &self,
    ) -> Result<FtsConsistencyReport, DatabaseError> {
        // SEC-013: Wrap entire consistency check with timeout
        tokio::time::timeout(
            FTS_CONSISTENCY_TIMEOUT,
            async {
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
        )
        .await
        .map_err(|_| DatabaseError::Other(sqlx::Error::PoolTimedOut))?
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
        // FTS5 rebuild with timeout to prevent unbounded execution on large DBs
        tokio::time::timeout(REBUILD_TIMEOUT, async {
            sqlx::query("INSERT INTO articles_fts(articles_fts) VALUES('rebuild')")
                .execute(&self.pool)
                .await
        })
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "FTS5 rebuild timed out after {}s",
                REBUILD_TIMEOUT.as_secs()
            )
        })?
        .map_err(|e| anyhow::anyhow!("FTS5 rebuild failed: {}", e))?;

        // Return count of indexed articles
        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM articles")
            .fetch_one(&self.pool)
            .await?;

        Ok(count.0 as usize)
    }
}

#[cfg(test)]
mod tests {
    use crate::storage::{Database, OpmlFeed, ParsedArticle, SearchScope};

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

        let results = db
            .search_articles("Rust", SearchScope::TitleAndSummary)
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(&*results[0].title, "Rust Programming Guide");
    }

    #[tokio::test]
    async fn test_search_empty_query() {
        let db = test_db().await;
        let results = db
            .search_articles("", SearchScope::TitleAndSummary)
            .await
            .unwrap();
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

        let results = db
            .search_articles("nonexistent", SearchScope::TitleAndSummary)
            .await
            .unwrap();
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

        let results = db
            .search_articles("Rust", SearchScope::TitleAndSummary)
            .await
            .unwrap();
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

        assert_eq!(
            db.search_articles("Important", SearchScope::TitleAndSummary)
                .await
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            db.search_articles("Another", SearchScope::TitleAndSummary)
                .await
                .unwrap()
                .len(),
            1
        );

        let count = db.rebuild_fts_index().await.unwrap();
        assert_eq!(count, 2);

        assert_eq!(
            db.search_articles("Important", SearchScope::TitleAndSummary)
                .await
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            db.search_articles("Another", SearchScope::TitleAndSummary)
                .await
                .unwrap()
                .len(),
            1
        );
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
        // Build a query at MAX_SEARCH_QUERY_LENGTH using short terms to avoid SEC-011 term limit
        let term = "abcd ";
        let repeats = super::MAX_SEARCH_QUERY_LENGTH / term.len();
        let mut query_at_limit = term.repeat(repeats);
        // Pad remaining chars with 'a' to hit exact limit
        while query_at_limit.len() < super::MAX_SEARCH_QUERY_LENGTH {
            query_at_limit.push('a');
        }
        assert_eq!(query_at_limit.len(), super::MAX_SEARCH_QUERY_LENGTH);
        assert!(super::validate_fts_query(&query_at_limit).is_ok());

        let query_over_limit = format!("{} a", query_at_limit);
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
        let long_query = "a".repeat(super::MAX_SEARCH_QUERY_LENGTH + 1);
        let result = db
            .search_articles(&long_query, SearchScope::TitleAndSummary)
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("maximum length"));
    }

    #[tokio::test]
    async fn test_search_articles_rejects_too_many_wildcards() {
        let db = test_db().await;
        let result = db
            .search_articles("a* b* c* d*", SearchScope::TitleAndSummary)
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("wildcards"));
    }

    #[tokio::test]
    async fn test_search_articles_rejects_too_many_or() {
        let db = test_db().await;
        let result = db
            .search_articles(
                "a OR b OR c OR d OR e OR f OR g",
                SearchScope::TitleAndSummary,
            )
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("OR operators"));
    }

    #[tokio::test]
    async fn test_search_articles_rejects_too_many_parentheses() {
        let db = test_db().await;
        let result = db
            .search_articles(
                "(a) AND (b) AND (c) AND (d) AND (e) AND (f)",
                SearchScope::TitleAndSummary,
            )
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("parentheses"));
    }

    #[tokio::test]
    async fn test_search_articles_rejects_too_many_and() {
        let db = test_db().await;
        let result = db
            .search_articles(
                "a AND b AND c AND d AND e AND f AND g AND h AND i AND j AND k AND l",
                SearchScope::TitleAndSummary,
            )
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("AND operators"));
    }

    #[test]
    fn test_validate_fts_query_normal_term_length() {
        let query = "rust programming language";
        assert!(super::validate_fts_query(query).is_ok());

        let query_at_limit = "a".repeat(super::MAX_TERM_LENGTH);
        assert!(super::validate_fts_query(&query_at_limit).is_ok());
    }

    #[test]
    fn test_validate_fts_query_term_too_long() {
        let long_term = "a".repeat(super::MAX_TERM_LENGTH + 1);
        let result = super::validate_fts_query(&long_term);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("term too long"));
    }

    #[test]
    fn test_validate_fts_query_term_length_strips_decorators() {
        // A term wrapped in quotes/wildcards/parens should validate the inner content length
        let inner = "a".repeat(super::MAX_TERM_LENGTH + 1);
        let decorated = format!("\"{}\"*", inner);
        let result = super::validate_fts_query(&decorated);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("term too long"));

        // Inner content at exactly the limit should pass
        let inner_ok = "b".repeat(super::MAX_TERM_LENGTH);
        let decorated_ok = format!("(\"{}\"*)", inner_ok);
        assert!(super::validate_fts_query(&decorated_ok).is_ok());
    }

    #[test]
    fn test_validate_fts_query_nesting_depth_ok() {
        // Depth 3 -- exactly at limit
        assert!(super::validate_fts_query("(a AND (b OR (c)))").is_ok());
    }

    #[test]
    fn test_validate_fts_query_nesting_depth_exceeded() {
        // Depth 4 -- over limit
        let result = super::validate_fts_query("(((( a ))))");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("nesting"));
    }

    #[test]
    fn test_validate_fts_query_flat_parens_ok() {
        // 5 parens but only depth 1
        assert!(super::validate_fts_query("(a) AND (b) AND (c) AND (d) AND (e)").is_ok());
    }

    #[test]
    fn test_validate_fts_query_rejects_column_filter_syntax() {
        // SEC-012: Reject { and } to prevent FTS5 column filter injection
        let result = super::validate_fts_query("{content} : secret");
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("invalid characters"));

        let result = super::validate_fts_query("foo OR {content} : bar");
        assert!(result.is_err());

        // Closing brace alone also rejected
        let result = super::validate_fts_query("test}");
        assert!(result.is_err());
    }

    // ========================================================================
    // TASK-3: Full-content FTS5 tests
    // ========================================================================

    #[tokio::test]
    async fn test_fts5_content_column_indexed() {
        let db = test_db().await;
        db.sync_feeds(&[test_feed(1)]).await.unwrap();
        let feeds = db.get_feeds_with_unread_counts().await.unwrap();

        db.upsert_articles(feeds[0].id, &[test_article("1", "Generic Title")])
            .await
            .unwrap();

        // Store unique content via index_content
        let articles = db.get_articles_for_feed(feeds[0].id, None).await.unwrap();
        db.index_content(articles[0].id, "UniqueContentMarker xyz789")
            .await
            .unwrap();

        // SearchScope::All should find it via content
        let results = db
            .search_articles("UniqueContentMarker", SearchScope::All)
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(&*results[0].title, "Generic Title");

        // SearchScope::TitleAndSummary should NOT find it (only in content)
        let results = db
            .search_articles("UniqueContentMarker", SearchScope::TitleAndSummary)
            .await
            .unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn test_index_content() {
        let db = test_db().await;
        db.sync_feeds(&[test_feed(1)]).await.unwrap();
        let feeds = db.get_feeds_with_unread_counts().await.unwrap();

        db.upsert_articles(feeds[0].id, &[test_article("1", "Article")])
            .await
            .unwrap();

        let articles = db.get_articles_for_feed(feeds[0].id, None).await.unwrap();
        let article_id = articles[0].id;

        // Index content
        db.index_content(article_id, "Full markdown body here")
            .await
            .unwrap();

        // Verify content is stored and searchable
        let results = db
            .search_articles("markdown", SearchScope::All)
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, article_id);
    }

    #[tokio::test]
    async fn test_search_title_and_summary_scope() {
        let db = test_db().await;
        db.sync_feeds(&[test_feed(1)]).await.unwrap();
        let feeds = db.get_feeds_with_unread_counts().await.unwrap();

        db.upsert_articles(feeds[0].id, &[test_article("1", "Rust Programming Guide")])
            .await
            .unwrap();

        // Title match works with TitleAndSummary scope
        let results = db
            .search_articles("Rust", SearchScope::TitleAndSummary)
            .await
            .unwrap();
        assert_eq!(results.len(), 1);

        // Summary match works with TitleAndSummary scope ("Test summary" from test_article)
        let results = db
            .search_articles("summary", SearchScope::TitleAndSummary)
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
    }

    #[tokio::test]
    async fn test_search_all_scope() {
        let db = test_db().await;
        db.sync_feeds(&[test_feed(1)]).await.unwrap();
        let feeds = db.get_feeds_with_unread_counts().await.unwrap();

        db.upsert_articles(feeds[0].id, &[test_article("1", "Plain Title")])
            .await
            .unwrap();

        let articles = db.get_articles_for_feed(feeds[0].id, None).await.unwrap();
        db.index_content(articles[0].id, "Deep content about quantum computing")
            .await
            .unwrap();

        // All scope finds content
        let results = db
            .search_articles("quantum", SearchScope::All)
            .await
            .unwrap();
        assert_eq!(results.len(), 1);

        // All scope also finds title
        let results = db.search_articles("Plain", SearchScope::All).await.unwrap();
        assert_eq!(results.len(), 1);
    }

    #[tokio::test]
    async fn test_search_scope_default_unchanged() {
        // Verify SearchScope::default() is TitleAndSummary
        assert_eq!(SearchScope::default(), SearchScope::TitleAndSummary);
    }
}
