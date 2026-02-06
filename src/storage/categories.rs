use anyhow::{bail, Result};

use super::schema::Database;
use super::types::FeedCategory;
use crate::util::strip_control_chars;

#[allow(dead_code)] // Methods consumed by downstream tasks (TASK-5, TASK-8, TASK-9)
impl Database {
    // ========================================================================
    // Category Operations
    // ========================================================================

    /// Maximum nesting depth for categories.
    /// Root = depth 0, child = depth 1, grandchild = depth 2.
    /// A category at depth 3 is rejected.
    const MAX_CATEGORY_DEPTH: i64 = 3;

    /// SEC-014: Sanitize and validate a category name.
    ///
    /// Strips control characters (ANSI escape injection prevention), trims
    /// whitespace, and rejects empty/whitespace-only names.
    fn sanitize_category_name(name: &str) -> Result<String> {
        let sanitized = strip_control_chars(name);
        let trimmed = sanitized.trim();
        if trimmed.is_empty() {
            bail!("Category name cannot be empty or whitespace-only");
        }
        Ok(trimmed.to_owned())
    }

    /// Create a new feed category, returning its ID.
    ///
    /// If `parent_id` is `Some`, the parent chain depth is validated.
    /// Creating a category that would exceed [`MAX_CATEGORY_DEPTH`] levels is rejected.
    ///
    /// SEC-014: The name is sanitized (control chars stripped, whitespace trimmed)
    /// before insertion.
    pub async fn create_category(&self, name: &str, parent_id: Option<i64>) -> Result<i64> {
        let clean_name = Self::sanitize_category_name(name)?;

        if let Some(pid) = parent_id {
            let depth = self.ancestor_depth(pid).await?;
            // depth is the level of the parent (0 = root).
            // The new child would be at depth+1.
            if depth + 1 >= Self::MAX_CATEGORY_DEPTH {
                bail!(
                    "Cannot create category: maximum nesting depth ({}) would be exceeded",
                    Self::MAX_CATEGORY_DEPTH
                );
            }
        }

        let row: (i64,) = sqlx::query_as(
            "INSERT INTO feed_categories (name, parent_id) VALUES (?, ?) RETURNING id",
        )
        .bind(&clean_name)
        .bind(parent_id)
        .fetch_one(&self.pool)
        .await?;

        Ok(row.0)
    }

    /// Rename an existing category.
    ///
    /// SEC-014: The name is sanitized (control chars stripped, whitespace trimmed)
    /// before update.
    pub async fn rename_category(&self, id: i64, new_name: &str) -> Result<()> {
        let clean_name = Self::sanitize_category_name(new_name)?;

        sqlx::query("UPDATE feed_categories SET name = ? WHERE id = ?")
            .bind(&clean_name)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Delete a category. Feeds in this category are moved to uncategorized (NULL).
    /// Child categories have their parent_id set to NULL by the ON DELETE SET NULL FK.
    pub async fn delete_category(&self, id: i64) -> Result<()> {
        let mut tx = self.pool.begin().await?;

        // Move feeds in this category to uncategorized
        sqlx::query("UPDATE feeds SET category_id = NULL WHERE category_id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await?;

        // Delete the category (ON DELETE SET NULL handles child categories)
        sqlx::query("DELETE FROM feed_categories WHERE id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await?;

        tx.commit().await?;
        Ok(())
    }

    /// Move a feed into a category, or to uncategorized if `category_id` is `None`.
    pub async fn move_feed_to_category(
        &self,
        feed_id: i64,
        category_id: Option<i64>,
    ) -> Result<()> {
        sqlx::query("UPDATE feeds SET category_id = ? WHERE id = ?")
            .bind(category_id)
            .bind(feed_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Get all categories as a flat list ordered by sort_order.
    /// The UI layer builds the tree structure from parent_id relationships.
    pub async fn get_categories_tree(&self) -> Result<Vec<FeedCategory>> {
        let rows: Vec<(i64, String, Option<i64>, i64)> = sqlx::query_as(
            "SELECT id, name, parent_id, sort_order FROM feed_categories ORDER BY sort_order, name",
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|(id, name, parent_id, sort_order)| FeedCategory {
                id,
                name,
                parent_id,
                sort_order,
            })
            .collect())
    }

    /// Compute the depth of a category by walking its ancestor chain.
    /// Root categories have depth 0.
    ///
    /// Defense-in-depth: LIMIT 50 on the recursive CTE prevents unbounded
    /// recursion if a cycle exists in corrupted data.
    async fn ancestor_depth(&self, category_id: i64) -> Result<i64> {
        // Use a recursive CTE to count ancestors
        let row: (i64,) = sqlx::query_as(
            r#"
            WITH RECURSIVE ancestors(id, parent_id, depth) AS (
                SELECT id, parent_id, 0 FROM feed_categories WHERE id = ?
                UNION ALL
                SELECT fc.id, fc.parent_id, a.depth + 1
                FROM feed_categories fc
                JOIN ancestors a ON fc.id = a.parent_id
                LIMIT 50
            )
            SELECT MAX(depth) FROM ancestors
            "#,
        )
        .bind(category_id)
        .fetch_one(&self.pool)
        .await?;

        Ok(row.0)
    }
}

#[cfg(test)]
mod tests {
    use crate::storage::{Database, OpmlFeed};

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

    #[tokio::test]
    async fn test_create_category() {
        let db = test_db().await;

        let id = db.create_category("Tech", None).await.unwrap();
        assert!(id > 0);

        let categories = db.get_categories_tree().await.unwrap();
        assert_eq!(categories.len(), 1);
        assert_eq!(categories[0].name, "Tech");
        assert_eq!(categories[0].parent_id, None);
    }

    #[tokio::test]
    async fn test_nested_categories() {
        let db = test_db().await;

        let root = db.create_category("Root", None).await.unwrap();
        let child = db.create_category("Child", Some(root)).await.unwrap();
        let grandchild = db.create_category("Grandchild", Some(child)).await.unwrap();

        let categories = db.get_categories_tree().await.unwrap();
        assert_eq!(categories.len(), 3);

        let gc = categories.iter().find(|c| c.id == grandchild).unwrap();
        assert_eq!(gc.parent_id, Some(child));
    }

    #[tokio::test]
    async fn test_move_feed_to_category() {
        let db = test_db().await;

        db.sync_feeds(&[test_feed(1)]).await.unwrap();
        let feeds = db.get_feeds_with_unread_counts().await.unwrap();
        let feed_id = feeds[0].id;

        let cat_id = db.create_category("News", None).await.unwrap();

        db.move_feed_to_category(feed_id, Some(cat_id))
            .await
            .unwrap();

        // Verify by checking the raw category_id value
        let row: (Option<i64>,) = sqlx::query_as("SELECT category_id FROM feeds WHERE id = ?")
            .bind(feed_id)
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(row.0, Some(cat_id));

        // Move back to uncategorized
        db.move_feed_to_category(feed_id, None).await.unwrap();

        let row: (Option<i64>,) = sqlx::query_as("SELECT category_id FROM feeds WHERE id = ?")
            .bind(feed_id)
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(row.0, None);
    }

    #[tokio::test]
    async fn test_delete_category_orphans_feeds() {
        let db = test_db().await;

        db.sync_feeds(&[test_feed(1), test_feed(2)]).await.unwrap();
        let feeds = db.get_feeds_with_unread_counts().await.unwrap();

        let cat_id = db.create_category("Disposable", None).await.unwrap();

        // Move both feeds into the category
        db.move_feed_to_category(feeds[0].id, Some(cat_id))
            .await
            .unwrap();
        db.move_feed_to_category(feeds[1].id, Some(cat_id))
            .await
            .unwrap();

        // Delete the category
        db.delete_category(cat_id).await.unwrap();

        // Feeds should still exist but be uncategorized
        let remaining = db.get_feeds_with_unread_counts().await.unwrap();
        assert_eq!(remaining.len(), 2);

        for feed in &remaining {
            let row: (Option<i64>,) = sqlx::query_as("SELECT category_id FROM feeds WHERE id = ?")
                .bind(feed.id)
                .fetch_one(&db.pool)
                .await
                .unwrap();
            assert_eq!(
                row.0, None,
                "Feed should be uncategorized after category deletion"
            );
        }

        // Category should be gone
        let categories = db.get_categories_tree().await.unwrap();
        assert!(categories.is_empty());
    }

    #[tokio::test]
    async fn test_max_nesting_depth() {
        let db = test_db().await;

        // Create chain: root (depth 0) -> child (depth 1) -> grandchild (depth 2)
        let root = db.create_category("Level 0", None).await.unwrap();
        let child = db.create_category("Level 1", Some(root)).await.unwrap();
        let grandchild = db.create_category("Level 2", Some(child)).await.unwrap();

        // Attempting to create at depth 3 should fail
        let result = db.create_category("Level 3", Some(grandchild)).await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("maximum nesting depth"));
    }

    #[tokio::test]
    async fn test_rename_category() {
        let db = test_db().await;

        let id = db.create_category("Old Name", None).await.unwrap();
        db.rename_category(id, "New Name").await.unwrap();

        let categories = db.get_categories_tree().await.unwrap();
        assert_eq!(categories[0].name, "New Name");
    }

    #[tokio::test]
    async fn test_delete_category_orphans_children() {
        let db = test_db().await;

        let parent = db.create_category("Parent", None).await.unwrap();
        let child = db.create_category("Child", Some(parent)).await.unwrap();

        db.delete_category(parent).await.unwrap();

        // Child should still exist but with parent_id = NULL (ON DELETE SET NULL)
        let categories = db.get_categories_tree().await.unwrap();
        assert_eq!(categories.len(), 1);
        assert_eq!(categories[0].id, child);
        assert_eq!(categories[0].parent_id, None);
    }

    #[tokio::test]
    async fn test_get_categories_tree_ordering() {
        let db = test_db().await;

        // Insert categories — default sort_order is 0, so they sort by name
        db.create_category("Zebra", None).await.unwrap();
        db.create_category("Alpha", None).await.unwrap();
        db.create_category("Middle", None).await.unwrap();

        let categories = db.get_categories_tree().await.unwrap();
        assert_eq!(categories.len(), 3);
        assert_eq!(categories[0].name, "Alpha");
        assert_eq!(categories[1].name, "Middle");
        assert_eq!(categories[2].name, "Zebra");
    }

    // ====================================================================
    // SEC-014: Category name sanitization tests
    // ====================================================================

    #[tokio::test]
    async fn test_create_category_strips_control_chars() {
        let db = test_db().await;

        // ANSI escape in name should be stripped
        let id = db
            .create_category("\x1b[31mEvil\x1b[0m", None)
            .await
            .unwrap();
        let categories = db.get_categories_tree().await.unwrap();
        let cat = categories.iter().find(|c| c.id == id).unwrap();
        assert_eq!(cat.name, "Evil");
    }

    #[tokio::test]
    async fn test_create_category_trims_whitespace() {
        let db = test_db().await;

        let id = db.create_category("  Padded  ", None).await.unwrap();
        let categories = db.get_categories_tree().await.unwrap();
        let cat = categories.iter().find(|c| c.id == id).unwrap();
        assert_eq!(cat.name, "Padded");
    }

    #[tokio::test]
    async fn test_create_category_rejects_empty_name() {
        let db = test_db().await;

        let result = db.create_category("", None).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("empty"));
    }

    #[tokio::test]
    async fn test_create_category_rejects_whitespace_only() {
        let db = test_db().await;

        let result = db.create_category("   ", None).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("empty"));
    }

    #[tokio::test]
    async fn test_create_category_rejects_control_chars_only() {
        let db = test_db().await;

        // Name that is only ANSI escapes — after stripping, it's empty
        let result = db.create_category("\x1b[31m\x1b[0m", None).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("empty"));
    }

    #[tokio::test]
    async fn test_rename_category_strips_control_chars() {
        let db = test_db().await;

        let id = db.create_category("Original", None).await.unwrap();
        db.rename_category(id, "\x1b[31mRenamed\x1b[0m")
            .await
            .unwrap();

        let categories = db.get_categories_tree().await.unwrap();
        assert_eq!(categories[0].name, "Renamed");
    }

    #[tokio::test]
    async fn test_rename_category_rejects_empty_name() {
        let db = test_db().await;

        let id = db.create_category("Valid", None).await.unwrap();
        let result = db.rename_category(id, "").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("empty"));

        // Original name should be preserved
        let categories = db.get_categories_tree().await.unwrap();
        assert_eq!(categories[0].name, "Valid");
    }

    #[tokio::test]
    async fn test_rename_category_trims_whitespace() {
        let db = test_db().await;

        let id = db.create_category("Before", None).await.unwrap();
        db.rename_category(id, "  After  ").await.unwrap();

        let categories = db.get_categories_tree().await.unwrap();
        assert_eq!(categories[0].name, "After");
    }
}
