//! Preference manager that merges config.toml defaults with DB overrides.
//!
//! Config values serve as defaults; DB values (user_preferences table) override them.
//! Writes always go to the DB, never to the config file.
use std::collections::HashMap;

use anyhow::Result;

use crate::config::Config;
use crate::storage::Database;

// ============================================================================
// PreferenceManager
// ============================================================================

/// Merged preference store: config.toml defaults + DB overrides.
///
/// On load, config values are flattened into a `HashMap<String, String>`, then
/// all DB preferences are layered on top. Reads are in-memory O(1). Writes
/// persist to the DB and update the in-memory map atomically.
pub struct PreferenceManager {
    prefs: HashMap<String, String>,
}

impl PreferenceManager {
    /// Load preferences by merging config defaults with DB overrides.
    ///
    /// 1. Flatten `Config` fields into dotted key-value pairs
    /// 2. Query all rows from `user_preferences` table
    /// 3. DB values overwrite config values for matching keys
    pub async fn load(config: &Config, db: &Database) -> Result<Self> {
        let mut prefs = Self::flatten_config(config);

        // Layer DB preferences on top (DB wins over config)
        let db_prefs = db.get_preferences_by_prefix("").await?;
        for (key, value) in db_prefs {
            prefs.insert(key, value);
        }

        Ok(Self { prefs })
    }

    /// Create from config only (no DB). Fallback for when DB load fails.
    pub fn from_config(config: &Config) -> Self {
        Self {
            prefs: Self::flatten_config(config),
        }
    }

    /// Get a preference value by key.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.prefs.get(key).map(String::as_str)
    }

    /// Set a preference: writes to DB and updates in-memory map.
    #[cfg_attr(not(test), allow(dead_code))] // Wired when session save + theme persistence lands
    pub async fn set(&mut self, db: &Database, key: &str, value: &str) -> Result<()> {
        db.set_preference(key, value).await?;
        self.prefs.insert(key.to_string(), value.to_string());
        Ok(())
    }

    // ========================================================================
    // Type-safe Accessors
    // ========================================================================

    /// Current theme variant name (e.g., "dark", "light").
    #[cfg_attr(not(test), allow(dead_code))] // Wired when theme-from-prefs loads at startup
    pub fn theme_variant(&self) -> &str {
        self.get("theme").unwrap_or("dark")
    }

    /// Refresh interval in minutes. 0 = manual only.
    #[cfg_attr(not(test), allow(dead_code))] // Wired when auto-refresh uses config interval
    pub fn refresh_interval(&self) -> u64 {
        self.get("refresh_interval_minutes")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    }

    /// Whether to restore the previous session on startup.
    pub fn restore_session(&self) -> bool {
        self.get("restore_session")
            .and_then(|v| v.parse().ok())
            .unwrap_or(false)
    }

    // ========================================================================
    // Internal Helpers
    // ========================================================================

    /// Flatten Config struct into dotted key-value pairs.
    fn flatten_config(config: &Config) -> HashMap<String, String> {
        let mut map = HashMap::new();

        map.insert("theme".to_string(), config.theme.clone());
        map.insert(
            "refresh_interval_minutes".to_string(),
            config.refresh_interval_minutes.to_string(),
        );
        map.insert(
            "max_articles_per_feed".to_string(),
            config.max_articles_per_feed.to_string(),
        );
        map.insert(
            "mark_read_on_open".to_string(),
            config.mark_read_on_open.to_string(),
        );
        map.insert(
            "confirm_mark_all_read".to_string(),
            config.confirm_mark_all_read.to_string(),
        );

        // Flatten keybindings into keybind.{action} keys
        for (action, key_str) in &config.keybindings {
            map.insert(format!("keybind.{}", action), key_str.clone());
        }

        // SEC-015: jina_api_key is intentionally NOT flattened into preferences.
        // Credentials must not enter the preference store (in-memory HashMap or
        // unencrypted user_preferences DB table). The Jina client reads the key
        // directly from the JINA_API_KEY env var or Config.jina_api_key field.

        map
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::storage::Database;

    async fn test_db() -> Database {
        Database::open(":memory:").await.unwrap()
    }

    #[tokio::test]
    async fn test_load_defaults_from_config() {
        let db = test_db().await;
        let config = Config::default();
        let pm = PreferenceManager::load(&config, &db).await.unwrap();

        assert_eq!(pm.theme_variant(), "dark");
        assert_eq!(pm.refresh_interval(), 0);
        assert!(!pm.restore_session());
    }

    #[tokio::test]
    async fn test_db_overrides_config() {
        let db = test_db().await;
        let config = Config::default();

        // Set a DB override
        db.set_preference("theme", "light").await.unwrap();

        let pm = PreferenceManager::load(&config, &db).await.unwrap();
        assert_eq!(pm.theme_variant(), "light");
    }

    #[tokio::test]
    async fn test_set_persists_and_updates_memory() {
        let db = test_db().await;
        let config = Config::default();
        let mut pm = PreferenceManager::load(&config, &db).await.unwrap();

        assert_eq!(pm.theme_variant(), "dark");

        pm.set(&db, "theme", "solarized").await.unwrap();
        assert_eq!(pm.theme_variant(), "solarized");

        // Verify it persisted to DB
        let stored = db.get_preference("theme").await.unwrap();
        assert_eq!(stored, Some("solarized".to_string()));
    }

    #[tokio::test]
    async fn test_get_returns_none_for_unknown() {
        let db = test_db().await;
        let config = Config::default();
        let pm = PreferenceManager::load(&config, &db).await.unwrap();

        assert_eq!(pm.get("nonexistent.key"), None);
    }

    #[tokio::test]
    async fn test_config_keybindings_flattened() {
        let db = test_db().await;
        let mut config = Config::default();
        config
            .keybindings
            .insert("quit".to_string(), "Ctrl+q".to_string());
        config
            .keybindings
            .insert("refresh".to_string(), "F5".to_string());

        let pm = PreferenceManager::load(&config, &db).await.unwrap();

        assert_eq!(pm.get("keybind.quit"), Some("Ctrl+q"));
        assert_eq!(pm.get("keybind.refresh"), Some("F5"));
    }

    #[tokio::test]
    async fn test_refresh_interval_parse() {
        let db = test_db().await;
        let mut config = Config::default();
        config.refresh_interval_minutes = 30;

        let pm = PreferenceManager::load(&config, &db).await.unwrap();
        assert_eq!(pm.refresh_interval(), 30);
    }

    #[tokio::test]
    async fn test_restore_session_default_false() {
        let db = test_db().await;
        let config = Config::default();
        let pm = PreferenceManager::load(&config, &db).await.unwrap();
        assert!(!pm.restore_session());
    }

    #[tokio::test]
    async fn test_restore_session_from_db() {
        let db = test_db().await;
        let config = Config::default();

        db.set_preference("restore_session", "true").await.unwrap();

        let pm = PreferenceManager::load(&config, &db).await.unwrap();
        assert!(pm.restore_session());
    }

    // ========================================================================
    // Config Lifecycle Integration Tests (TASK-15)
    //
    // These test the full config → preferences → session round-trip.
    // Located here because binary crates cannot be imported in tests/ dir.
    // ========================================================================

    #[tokio::test]
    async fn test_config_to_db_round_trip() {
        let db = test_db().await;

        // Start with custom config
        let mut config = Config::default();
        config.theme = "solarized".to_string();
        config.refresh_interval_minutes = 15;

        // Load preferences (config defaults)
        let mut pm = PreferenceManager::load(&config, &db).await.unwrap();
        assert_eq!(pm.theme_variant(), "solarized");
        assert_eq!(pm.refresh_interval(), 15);

        // User overrides theme via DB
        pm.set(&db, "theme", "gruvbox").await.unwrap();
        assert_eq!(pm.theme_variant(), "gruvbox");

        // Reload from scratch — DB should win
        let pm2 = PreferenceManager::load(&config, &db).await.unwrap();
        assert_eq!(pm2.theme_variant(), "gruvbox");
        // Config-only value should still be present
        assert_eq!(pm2.refresh_interval(), 15);
    }

    #[tokio::test]
    async fn test_session_snapshot_round_trip() {
        use crate::app::SessionSnapshot;

        let db = test_db().await;

        // Create a snapshot
        let snapshot = SessionSnapshot {
            focus: "articles".to_string(),
            selected_feed: 3,
            selected_article: 7,
            scroll_offset: 42,
        };

        // Serialize and store
        let json = serde_json::to_string(&snapshot).unwrap();
        db.set_preference("session.snapshot", &json).await.unwrap();

        // Retrieve and deserialize
        let stored = db
            .get_preference("session.snapshot")
            .await
            .unwrap()
            .unwrap();
        let restored: SessionSnapshot = serde_json::from_str(&stored).unwrap();

        assert_eq!(restored.focus, "articles");
        assert_eq!(restored.selected_feed, 3);
        assert_eq!(restored.selected_article, 7);
        assert_eq!(restored.scroll_offset, 42);
    }

    #[tokio::test]
    async fn test_corrupt_snapshot_ignored() {
        use crate::app::SessionSnapshot;

        let db = test_db().await;

        // Store corrupt JSON
        db.set_preference("session.snapshot", "not valid json {{")
            .await
            .unwrap();

        // Deserialize should fail gracefully
        let stored = db
            .get_preference("session.snapshot")
            .await
            .unwrap()
            .unwrap();
        let result = serde_json::from_str::<SessionSnapshot>(&stored);
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_preferences_survive_reload() {
        let db = test_db().await;
        let config = Config::default();

        // First session: set some preferences
        let mut pm = PreferenceManager::load(&config, &db).await.unwrap();
        pm.set(&db, "theme", "light").await.unwrap();
        pm.set(&db, "restore_session", "true").await.unwrap();
        pm.set(&db, "keybind.quit", "Ctrl+w").await.unwrap();
        drop(pm);

        // Second session: preferences should persist
        let pm2 = PreferenceManager::load(&config, &db).await.unwrap();
        assert_eq!(pm2.theme_variant(), "light");
        assert!(pm2.restore_session());
        assert_eq!(pm2.get("keybind.quit"), Some("Ctrl+w"));
    }

    #[tokio::test]
    async fn test_from_config_fallback() {
        let mut config = Config::default();
        config.theme = "monokai".to_string();
        config.refresh_interval_minutes = 45;

        let pm = PreferenceManager::from_config(&config);
        assert_eq!(pm.theme_variant(), "monokai");
        assert_eq!(pm.refresh_interval(), 45);
        assert!(!pm.restore_session());
    }

    #[tokio::test]
    async fn test_config_file_load_and_merge() {
        let db = test_db().await;

        // Simulate config file with custom values
        let dir = std::env::temp_dir().join("skim_lifecycle_test");
        std::fs::create_dir_all(&dir).unwrap();
        let config_path = dir.join("config.toml");
        std::fs::write(
            &config_path,
            r#"
theme = "nord"
refresh_interval_minutes = 60
mark_read_on_open = false
"#,
        )
        .unwrap();

        let config = Config::load(&config_path).unwrap();
        assert_eq!(config.theme, "nord");
        assert_eq!(config.refresh_interval_minutes, 60);
        assert!(!config.mark_read_on_open);

        // Merge with DB
        let pm = PreferenceManager::load(&config, &db).await.unwrap();
        assert_eq!(pm.theme_variant(), "nord");
        assert_eq!(pm.refresh_interval(), 60);
        assert_eq!(pm.get("mark_read_on_open"), Some("false"));

        std::fs::remove_dir_all(&dir).ok();
    }
}
