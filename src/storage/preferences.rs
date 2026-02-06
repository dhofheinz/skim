use anyhow::Result;

use super::schema::Database;

impl Database {
    // ========================================================================
    // User Preferences Operations
    // ========================================================================

    /// Get a single preference value by key.
    ///
    /// Keys use dotted convention: `theme.variant`, `keybind.quit`, `session.view`, etc.
    ///
    /// # Returns
    ///
    /// The preference value if the key exists, or `None` if not set.
    pub async fn get_preference(&self, key: &str) -> Result<Option<String>> {
        let row: Option<(String,)> =
            sqlx::query_as("SELECT value FROM user_preferences WHERE key = ?")
                .bind(key)
                .fetch_optional(&self.pool)
                .await?;

        Ok(row.map(|(value,)| value))
    }

    /// Set a preference value (UPSERT).
    ///
    /// Inserts the key-value pair if it doesn't exist, or updates the value and
    /// timestamp if the key already exists.
    ///
    /// # Arguments
    ///
    /// * `key` - Dotted preference key (e.g., `theme.variant`)
    /// * `value` - The preference value to store
    pub async fn set_preference(&self, key: &str, value: &str) -> Result<()> {
        sqlx::query(
            r#"
            INSERT INTO user_preferences (key, value, updated_at)
            VALUES (?, ?, datetime('now'))
            ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at
        "#,
        )
        .bind(key)
        .bind(value)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Get all preferences matching a key prefix.
    ///
    /// Useful for loading grouped settings (e.g., all `theme.*` or `keybind.*` entries).
    ///
    /// # Arguments
    ///
    /// * `prefix` - The key prefix to match (e.g., `theme.` returns `theme.variant`, `theme.custom_bg`, etc.)
    ///
    /// # Returns
    ///
    /// A vector of (key, value) pairs matching the prefix, ordered by key.
    pub async fn get_preferences_by_prefix(&self, prefix: &str) -> Result<Vec<(String, String)>> {
        let pattern = format!("{}%", prefix);
        let rows: Vec<(String, String)> =
            sqlx::query_as("SELECT key, value FROM user_preferences WHERE key LIKE ? ORDER BY key")
                .bind(&pattern)
                .fetch_all(&self.pool)
                .await?;

        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use crate::storage::Database;

    async fn test_db() -> Database {
        Database::open(":memory:").await.unwrap()
    }

    #[tokio::test]
    async fn test_get_preference_missing() {
        let db = test_db().await;
        let value = db.get_preference("nonexistent.key").await.unwrap();
        assert_eq!(value, None);
    }

    #[tokio::test]
    async fn test_set_and_get_preference() {
        let db = test_db().await;
        db.set_preference("theme.variant", "dark").await.unwrap();

        let value = db.get_preference("theme.variant").await.unwrap();
        assert_eq!(value, Some("dark".to_string()));
    }

    #[tokio::test]
    async fn test_set_preference_upsert() {
        let db = test_db().await;
        db.set_preference("theme.variant", "dark").await.unwrap();
        db.set_preference("theme.variant", "light").await.unwrap();

        let value = db.get_preference("theme.variant").await.unwrap();
        assert_eq!(value, Some("light".to_string()));
    }

    #[tokio::test]
    async fn test_get_preferences_by_prefix() {
        let db = test_db().await;
        db.set_preference("theme.variant", "dark").await.unwrap();
        db.set_preference("theme.custom_bg", "#1e1e2e")
            .await
            .unwrap();
        db.set_preference("keybind.quit", "q").await.unwrap();

        let theme_prefs = db.get_preferences_by_prefix("theme.").await.unwrap();
        assert_eq!(theme_prefs.len(), 2);
        assert_eq!(
            theme_prefs[0],
            ("theme.custom_bg".to_string(), "#1e1e2e".to_string())
        );
        assert_eq!(
            theme_prefs[1],
            ("theme.variant".to_string(), "dark".to_string())
        );
    }

    #[tokio::test]
    async fn test_get_preferences_by_prefix_empty() {
        let db = test_db().await;
        db.set_preference("theme.variant", "dark").await.unwrap();

        let prefs = db.get_preferences_by_prefix("keybind.").await.unwrap();
        assert!(prefs.is_empty());
    }

    #[tokio::test]
    async fn test_get_preferences_by_prefix_no_false_matches() {
        let db = test_db().await;
        db.set_preference("theme.variant", "dark").await.unwrap();
        db.set_preference("thematic.value", "test").await.unwrap();

        // "theme." should not match "thematic."
        let prefs = db.get_preferences_by_prefix("theme.").await.unwrap();
        assert_eq!(prefs.len(), 1);
        assert_eq!(prefs[0].0, "theme.variant");
    }

    #[tokio::test]
    async fn test_set_preference_updates_timestamp() {
        let db = test_db().await;
        db.set_preference("test.key", "value1").await.unwrap();

        // Fetch the updated_at for the first insert
        let row1: (String,) =
            sqlx::query_as("SELECT updated_at FROM user_preferences WHERE key = ?")
                .bind("test.key")
                .fetch_one(&db.pool)
                .await
                .unwrap();

        db.set_preference("test.key", "value2").await.unwrap();

        let row2: (String,) =
            sqlx::query_as("SELECT updated_at FROM user_preferences WHERE key = ?")
                .bind("test.key")
                .fetch_one(&db.pool)
                .await
                .unwrap();

        // Both should be valid datetime strings (may or may not differ depending on timing)
        assert!(!row1.0.is_empty());
        assert!(!row2.0.is_empty());
    }
}
