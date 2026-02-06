//! Configuration file parser for ~/.config/skim/config.toml.
//!
//! The config file is optional — a missing file yields `Config::default()`.
//! Unknown keys are silently ignored by serde (with `deny_unknown_fields` off),
//! though we log a warning when the file contains potential typos.
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;
use thiserror::Error;

// ============================================================================
// Error Types
// ============================================================================

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("Failed to read config file: {0}")]
    Io(#[from] std::io::Error),

    #[error("Invalid TOML in config file: {0}")]
    Parse(#[from] toml::de::Error),

    /// SEC-014: Config file exceeds maximum allowed size.
    #[error("Config file too large: {0}")]
    TooLarge(String),
}

// ============================================================================
// Configuration Structs
// ============================================================================

/// Top-level application configuration.
///
/// All fields use `#[serde(default)]` so any subset of keys can be specified.
/// Missing keys fall back to `Default::default()`.
///
/// SEC-015: Custom Debug impl masks `jina_api_key` to prevent secret leakage
/// in logs, error messages, and debug output.
#[derive(Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Theme variant name (e.g., "dark", "light", or a custom theme name).
    pub theme: String,

    /// Refresh interval in minutes. 0 = manual refresh only.
    pub refresh_interval_minutes: u64,

    /// Maximum number of articles to keep per feed (0 = unlimited).
    pub max_articles_per_feed: u64,

    /// Whether to mark articles as read when opened in reader.
    pub mark_read_on_open: bool,

    /// Whether to confirm before marking all articles as read.
    pub confirm_mark_all_read: bool,

    /// Custom keybinding overrides. Keys are action names, values are key strings.
    pub keybindings: HashMap<String, String>,

    /// Jina.ai API key (alternative to JINA_API_KEY env var).
    /// Env var takes precedence over config file.
    pub jina_api_key: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            theme: "dark".to_string(),
            refresh_interval_minutes: 0,
            max_articles_per_feed: 0,
            mark_read_on_open: true,
            confirm_mark_all_read: false,
            keybindings: HashMap::new(),
            jina_api_key: None,
        }
    }
}

/// SEC-015: Mask jina_api_key in Debug output to prevent secret leakage.
impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("theme", &self.theme)
            .field("refresh_interval_minutes", &self.refresh_interval_minutes)
            .field("max_articles_per_feed", &self.max_articles_per_feed)
            .field("mark_read_on_open", &self.mark_read_on_open)
            .field("confirm_mark_all_read", &self.confirm_mark_all_read)
            .field("keybindings", &self.keybindings)
            .field(
                "jina_api_key",
                &self.jina_api_key.as_ref().map(|_| "[REDACTED]"),
            )
            .finish()
    }
}

impl Config {
    /// SEC-014: Maximum config file size (1 MB).
    const MAX_FILE_SIZE: u64 = 1_048_576;

    /// Load configuration from a TOML file.
    ///
    /// - Missing file → `Ok(Config::default())`
    /// - Empty file → `Ok(Config::default())`
    /// - Invalid TOML → `Err(ConfigError::Parse)` with line number info
    /// - Unknown keys → silently accepted (serde default behavior), logged as warning
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        // SEC-014: Check file size before reading to prevent memory exhaustion
        // from a maliciously large or corrupted config file.
        match std::fs::metadata(path) {
            Ok(meta) if meta.len() > Self::MAX_FILE_SIZE => {
                return Err(ConfigError::TooLarge(format!(
                    "Config file is {} bytes (max {} bytes)",
                    meta.len(),
                    Self::MAX_FILE_SIZE
                )));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::debug!(path = %path.display(), "No config file found, using defaults");
                return Ok(Self::default());
            }
            Err(e) => return Err(ConfigError::Io(e)),
            Ok(_) => {} // Size is within limits, proceed
        }

        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Race condition: file deleted between metadata and read
                tracing::debug!(path = %path.display(), "Config file disappeared, using defaults");
                return Ok(Self::default());
            }
            Err(e) => return Err(ConfigError::Io(e)),
        };

        if content.trim().is_empty() {
            tracing::debug!(path = %path.display(), "Config file is empty, using defaults");
            return Ok(Self::default());
        }

        // Parse the TOML content first as a raw table to detect unknown keys
        if let Ok(raw) = content.parse::<toml::Table>() {
            let known_keys = [
                "theme",
                "refresh_interval_minutes",
                "max_articles_per_feed",
                "mark_read_on_open",
                "confirm_mark_all_read",
                "keybindings",
                "jina_api_key",
            ];
            for key in raw.keys() {
                if !known_keys.contains(&key.as_str()) {
                    tracing::warn!(key = %key, "Unknown key in config file, ignoring");
                }
            }
        }

        let config: Config = toml::from_str(&content)?;
        tracing::info!(path = %path.display(), theme = %config.theme, "Loaded configuration");
        Ok(config)
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = Config::default();
        assert_eq!(config.theme, "dark");
        assert_eq!(config.refresh_interval_minutes, 0);
        assert_eq!(config.max_articles_per_feed, 0);
        assert!(config.mark_read_on_open);
        assert!(!config.confirm_mark_all_read);
        assert!(config.keybindings.is_empty());
        assert!(config.jina_api_key.is_none());
    }

    #[test]
    fn test_missing_file_returns_default() {
        let path = Path::new("/tmp/skim_test_nonexistent_config.toml");
        let config = Config::load(path).unwrap();
        assert_eq!(config.theme, "dark");
    }

    #[test]
    fn test_empty_file_returns_default() {
        let dir = std::env::temp_dir().join("skim_config_test_empty");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, "").unwrap();

        let config = Config::load(&path).unwrap();
        assert_eq!(config.theme, "dark");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_partial_config_uses_defaults_for_missing() {
        let dir = std::env::temp_dir().join("skim_config_test_partial");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, "theme = \"light\"\n").unwrap();

        let config = Config::load(&path).unwrap();
        assert_eq!(config.theme, "light");
        assert_eq!(config.refresh_interval_minutes, 0); // default
        assert!(config.mark_read_on_open); // default

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_full_config() {
        let dir = std::env::temp_dir().join("skim_config_test_full");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");

        let content = r#"
theme = "solarized"
refresh_interval_minutes = 30
max_articles_per_feed = 500
mark_read_on_open = false
confirm_mark_all_read = true
jina_api_key = "test-key-123"

[keybindings]
quit = "Ctrl+q"
refresh = "F5"
"#;
        std::fs::write(&path, content).unwrap();

        let config = Config::load(&path).unwrap();
        assert_eq!(config.theme, "solarized");
        assert_eq!(config.refresh_interval_minutes, 30);
        assert_eq!(config.max_articles_per_feed, 500);
        assert!(!config.mark_read_on_open);
        assert!(config.confirm_mark_all_read);
        assert_eq!(config.jina_api_key.as_deref(), Some("test-key-123"));
        assert_eq!(
            config.keybindings.get("quit").map(String::as_str),
            Some("Ctrl+q")
        );
        assert_eq!(
            config.keybindings.get("refresh").map(String::as_str),
            Some("F5")
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_invalid_toml_returns_error() {
        let dir = std::env::temp_dir().join("skim_config_test_invalid");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, "this is not [valid toml").unwrap();

        let result = Config::load(&path);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, ConfigError::Parse(_)));
        // Verify error message contains useful info
        let msg = err.to_string();
        assert!(msg.contains("Invalid TOML"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_unknown_keys_accepted() {
        let dir = std::env::temp_dir().join("skim_config_test_unknown");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");

        let content = r#"
theme = "dark"
totally_fake_key = "should not fail"
another_unknown = 42
"#;
        std::fs::write(&path, content).unwrap();

        // Should succeed (unknown keys ignored)
        let config = Config::load(&path).unwrap();
        assert_eq!(config.theme, "dark");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_wrong_type_returns_error() {
        let dir = std::env::temp_dir().join("skim_config_test_wrongtype");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        // theme should be a string, not an integer
        std::fs::write(&path, "theme = 42\n").unwrap();

        let result = Config::load(&path);
        assert!(result.is_err());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_whitespace_only_file_returns_default() {
        let dir = std::env::temp_dir().join("skim_config_test_whitespace");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, "   \n  \n  ").unwrap();

        let config = Config::load(&path).unwrap();
        assert_eq!(config.theme, "dark");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_keybindings_empty_map() {
        let dir = std::env::temp_dir().join("skim_config_test_empty_kb");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");

        let content = "[keybindings]\n";
        std::fs::write(&path, content).unwrap();

        let config = Config::load(&path).unwrap();
        assert!(config.keybindings.is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }

    // SEC-014: File size limit
    #[test]
    fn test_too_large_file_rejected() {
        let dir = std::env::temp_dir().join("skim_config_test_too_large");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");

        // Write a file just over 1MB
        let content = "a".repeat(1_048_577);
        std::fs::write(&path, content).unwrap();

        let result = Config::load(&path);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, ConfigError::TooLarge(_)));
        assert!(err.to_string().contains("too large"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_file_at_size_limit_accepted() {
        let dir = std::env::temp_dir().join("skim_config_test_at_limit");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");

        // Write a valid TOML file exactly at 1MB (padded with whitespace)
        let mut content = "theme = \"dark\"\n".to_string();
        // Pad to exactly 1MB with TOML comments
        while content.len() < 1_048_576 - 20 {
            content.push_str("# padding comment\n");
        }
        content.truncate(1_048_576);
        std::fs::write(&path, &content).unwrap();

        let result = Config::load(&path);
        assert!(result.is_ok());

        std::fs::remove_dir_all(&dir).ok();
    }

    // SEC-015: Debug output masks API key
    #[test]
    fn test_debug_masks_api_key() {
        let mut config = Config::default();
        config.jina_api_key = Some("super-secret-key-12345".to_string());

        let debug_output = format!("{:?}", config);
        assert!(
            !debug_output.contains("super-secret-key-12345"),
            "Debug output should not contain the API key"
        );
        assert!(
            debug_output.contains("[REDACTED]"),
            "Debug output should show [REDACTED] for API key"
        );
    }

    #[test]
    fn test_debug_shows_none_when_no_api_key() {
        let config = Config::default();
        let debug_output = format!("{:?}", config);
        assert!(
            debug_output.contains("None"),
            "Debug output should show None when no API key is set"
        );
        assert!(
            !debug_output.contains("[REDACTED]"),
            "Debug output should not show [REDACTED] when no key"
        );
    }
}
