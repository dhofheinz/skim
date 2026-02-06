use anyhow::{Context, Result};
use chrono::Utc;
use clap::Parser;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use tokio::sync::mpsc;

mod app;
mod config;
mod content;
mod feed;
mod keybindings;
mod preferences;
mod storage;
mod theme;
mod ui;
mod util;

use app::{App, AppEvent};
use storage::{Database, DatabaseError, OpmlFeed};

/// Get the config directory path (~/.config/skim/)
fn get_config_dir() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME environment variable not set")?;
    let config_dir = PathBuf::from(home).join(".config").join("skim");
    Ok(config_dir)
}

/// S-5: Return the last 2 path components to avoid leaking full directory
/// structure in user-facing error messages.
fn truncate_path(path: &Path) -> String {
    let components: Vec<_> = path.components().rev().take(2).collect();
    let short: PathBuf = components.into_iter().rev().collect();
    short.display().to_string()
}

/// Atomically copy a file using write-to-temp-then-rename pattern.
/// This ensures the destination is never left in a partial state.
fn atomic_copy(src: &Path, dst: &Path) -> Result<()> {
    // SEC-009: Use randomized temp filename to prevent TOCTOU race conditions.
    // An attacker cannot predict the temp path, so cannot create a symlink there
    // between our non-existent check and file creation.
    use std::time::{SystemTime, UNIX_EPOCH};
    let random_suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let temp_path = dst.with_extension(format!("tmp.{:016x}", random_suffix));

    // Read source content
    let content = std::fs::read(src).with_context(|| {
        tracing::debug!(path = %src.display(), "Failed to read source file");
        format!(
            "Failed to read source file '{}': check file permissions",
            truncate_path(src)
        )
    })?;

    let mut temp_file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true) // Fails atomically if file exists (prevents symlink race)
        .open(&temp_path)
        .with_context(|| {
            tracing::debug!(path = %temp_path.display(), "Failed to create temporary file");
            format!(
                "Failed to create temporary file '{}': check directory permissions or disk space",
                truncate_path(&temp_path)
            )
        })?;

    temp_file.write_all(&content).with_context(|| {
        // B-5: Warn on temp file cleanup failure instead of silently ignoring
        if let Err(e) = std::fs::remove_file(&temp_path) {
            tracing::warn!(path = %temp_path.display(), error = %e, "Failed to clean up temp file");
        }
        tracing::debug!(path = %temp_path.display(), "Failed to write to temporary file");
        format!(
            "Failed to write to temporary file '{}': disk may be full",
            truncate_path(&temp_path)
        )
    })?;

    // Sync to disk to ensure data is persisted before rename
    temp_file.sync_all().with_context(|| {
        if let Err(e) = std::fs::remove_file(&temp_path) {
            tracing::warn!(path = %temp_path.display(), error = %e, "Failed to clean up temp file");
        }
        tracing::debug!(path = %temp_path.display(), "Failed to sync temporary file");
        format!(
            "Failed to sync temporary file '{}' to disk: disk may be full",
            truncate_path(&temp_path)
        )
    })?;

    // Drop the file handle before rename
    drop(temp_file);

    // Atomic rename (POSIX guarantees atomicity for rename on same filesystem)
    // On Windows, rename fails if destination exists, so remove it first
    #[cfg(windows)]
    if dst.exists() {
        std::fs::remove_file(dst).with_context(|| {
            if let Err(e) = std::fs::remove_file(&temp_path) {
                tracing::warn!(path = %temp_path.display(), error = %e, "Failed to clean up temp file");
            }
            format!(
                "Failed to remove existing '{}' before atomic replace",
                truncate_path(dst)
            )
        })?;
    }

    std::fs::rename(&temp_path, dst).with_context(|| {
        if let Err(e) = std::fs::remove_file(&temp_path) {
            tracing::warn!(path = %temp_path.display(), error = %e, "Failed to clean up temp file");
        }
        tracing::debug!(src = %temp_path.display(), dst = %dst.display(), "Failed to rename temp file");
        format!(
            "Failed to rename '{}' to '{}': check permissions",
            truncate_path(&temp_path),
            truncate_path(dst)
        )
    })?;

    Ok(())
}

/// Remove old OPML backups, keeping the most recent `keep` files.
fn rotate_opml_backups(config_dir: &Path, keep: usize) {
    let mut backups: Vec<_> = match std::fs::read_dir(config_dir) {
        Ok(entries) => entries
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .is_some_and(|n| n.starts_with("feeds.opml.backup."))
            })
            .collect(),
        Err(e) => {
            tracing::warn!(error = %e, "Failed to read config directory for backup rotation");
            return;
        }
    };

    if backups.len() <= keep {
        return;
    }

    // Sort by filename ascending (oldest first due to YYYYMMDD_HHMMSS format)
    backups.sort_by_key(|e| e.file_name());

    let to_remove = backups.len() - keep;
    for entry in backups.into_iter().take(to_remove) {
        let path = entry.path();
        if let Err(e) = std::fs::remove_file(&path) {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "Failed to remove old OPML backup"
            );
        } else {
            tracing::debug!(path = %path.display(), "Removed old OPML backup");
        }
    }
}

#[derive(Parser, Debug)]
#[command(name = "skim", about = "Terminal RSS reader with jina.ai integration")]
struct Args {
    /// Reset database (delete and recreate)
    #[arg(long)]
    reset_db: bool,

    /// Import OPML file (copies to config directory)
    #[arg(long, value_name = "FILE")]
    import: Option<PathBuf>,

    /// Export feeds to OPML file
    #[arg(long, value_name = "FILE")]
    export: Option<PathBuf>,

    /// Rebuild the search index (FTS5)
    #[arg(long)]
    rebuild_search: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing for debug logging
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let args = Args::parse();

    if args.import.is_some() && args.export.is_some() {
        eprintln!("Error: --import and --export cannot be used together");
        std::process::exit(1);
    }

    // Set up config directory
    let config_dir = get_config_dir()?;
    if !config_dir.exists() {
        std::fs::create_dir_all(&config_dir).context("Failed to create config directory")?;
        println!("Created config directory: {}", config_dir.display());
    }

    // SEC-007: Set directory permissions on Unix (user-only access)
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        match std::fs::metadata(&config_dir) {
            Ok(metadata) => {
                let mut perms = metadata.permissions();
                perms.set_mode(0o700);
                if let Err(e) = std::fs::set_permissions(&config_dir, perms) {
                    tracing::warn!(
                        path = %config_dir.display(),
                        error = %e,
                        "Failed to set config directory permissions to 0700"
                    );
                }
            }
            Err(e) => {
                tracing::warn!(
                    path = %config_dir.display(),
                    error = %e,
                    "Failed to read config directory metadata"
                );
            }
        }
    }

    let opml_path = config_dir.join("feeds.opml");
    let db_path = config_dir.join("rss.db");

    // Handle --import flag
    if let Some(import_file) = &args.import {
        // S-2: Open file first — binds FD to inode, closing TOCTOU window.
        // We intentionally do NOT use O_NOFOLLOW: users may legitimately symlink OPML files.
        let mut file = std::fs::File::open(import_file).with_context(|| {
            tracing::debug!(path = %import_file.display(), "Failed to open import file");
            format!("Failed to open import file: {}", truncate_path(import_file))
        })?;

        // Check metadata on the open FD (not the path) — TOCTOU-safe
        let metadata = file.metadata().with_context(|| {
            format!(
                "Failed to read file metadata: {}",
                truncate_path(import_file)
            )
        })?;
        if !metadata.is_file() {
            anyhow::bail!("Import path must be a regular file");
        }

        // Read from the FD (TOCTOU-safe: we read what we opened)
        let mut content = String::new();
        file.read_to_string(&mut content).with_context(|| {
            tracing::debug!(path = %import_file.display(), "Failed to read import file");
            format!("Failed to read import file: {}", truncate_path(import_file))
        })?;

        // Basic OPML validation: check for required elements
        if !content.contains("<opml") && !content.contains("<outline") {
            anyhow::bail!("File does not appear to be valid OPML");
        }

        // SEC-006: Atomic backup of existing OPML before overwriting
        // Create backup FIRST using atomic operation, only then proceed with import
        if opml_path.exists() {
            let backup_name = format!("feeds.opml.backup.{}", Utc::now().format("%Y%m%d_%H%M%S"));
            let backup_path = config_dir.join(&backup_name);

            // Atomic backup: if this fails, original is untouched
            atomic_copy(&opml_path, &backup_path).with_context(|| {
                tracing::debug!(path = %backup_path.display(), "Failed to create backup");
                format!(
                    "Failed to create backup at '{}'. Original file is unchanged.",
                    truncate_path(&backup_path)
                )
            })?;

            // Verify backup exists before proceeding
            if !backup_path.exists() {
                tracing::debug!(path = %backup_path.display(), "Backup verification failed");
                anyhow::bail!(
                    "Backup verification failed: '{}' was not created. Aborting import to protect existing data.",
                    truncate_path(&backup_path)
                );
            }
            println!("Backed up existing OPML to: {}", backup_path.display());
            rotate_opml_backups(&config_dir, 5);
        }

        // Atomic import: if this fails, the original file remains intact
        // (either unchanged if no backup was needed, or restorable from backup)
        atomic_copy(import_file, &opml_path).with_context(|| {
            tracing::debug!(path = %import_file.display(), "Failed to import OPML file");
            format!(
                "Failed to import OPML file '{}'. If a backup was created, your previous feeds are preserved there.",
                truncate_path(import_file)
            )
        })?;
        println!("Imported OPML to: {}", opml_path.display());
    }

    // Handle --reset-db flag
    if args.reset_db && db_path.exists() {
        std::fs::remove_file(&db_path).context("Failed to delete database")?;
        println!("Database reset.");
    }

    // Check OPML exists
    if !opml_path.exists() {
        eprintln!("Error: No feeds file found at {}", opml_path.display());
        eprintln!();
        eprintln!("To get started, import your OPML file:");
        eprintln!("  skim --import /path/to/your/feeds.opml");
        eprintln!();
        eprintln!("Or create {} manually.", opml_path.display());
        std::process::exit(1);
    }

    // Parse OPML
    let opml_path_str = opml_path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("Invalid UTF-8 in OPML path"))?;
    let opml_feeds = feed::parse(opml_path_str)
        .await
        .context("Failed to parse OPML file")?;

    if opml_feeds.is_empty() {
        eprintln!("Warning: No valid feeds found in OPML file");
        eprintln!("The file may be empty or contain only invalid URLs");
    } else {
        println!(
            "Loaded {} feeds from {}",
            opml_feeds.len(),
            opml_path.display()
        );
    }

    // Convert to storage OpmlFeed type
    let storage_feeds: Vec<OpmlFeed> = opml_feeds
        .into_iter()
        .map(|f| OpmlFeed {
            title: f.title,
            xml_url: f.xml_url,
            html_url: f.html_url,
        })
        .collect();

    // Open database
    let db_path_str = db_path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("Invalid UTF-8 in database path"))?;
    let db = match Database::open(db_path_str).await {
        Ok(db) => db,
        Err(DatabaseError::InstanceLocked) => {
            eprintln!(
                "Error: Another instance of skim appears to be running. Please close it and try again."
            );
            std::process::exit(1);
        }
        Err(e) => {
            return Err(anyhow::anyhow!("Failed to open database: {}", e));
        }
    };

    // B-3: Spawn FTS consistency check BEFORE sync_feeds to avoid false positives
    // from concurrent article inserts during the first refresh.
    if !args.rebuild_search {
        let db_check = db.clone();
        tokio::spawn(async move {
            match db_check.check_fts_consistency_detailed().await {
                Ok(report) if report.is_consistent => {
                    tracing::debug!("FTS5 index is consistent");
                }
                Ok(report) => {
                    tracing::warn!(
                        articles = report.articles_count,
                        fts = report.fts_count,
                        orphaned = report.orphaned_fts_entries,
                        missing = report.missing_fts_entries,
                        "FTS index inconsistent, run with --rebuild-search"
                    );
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Failed to check FTS5 consistency");
                }
            }
        });
    }

    // Sync feeds from OPML to database
    db.sync_feeds(&storage_feeds)
        .await
        .context("Failed to sync feeds")?;

    // Handle --rebuild-search flag (synchronous -- explicitly requested by user)
    if args.rebuild_search {
        tracing::info!("Rebuilding search index...");
        let count = db
            .rebuild_fts_index()
            .await
            .context("Failed to rebuild search index")?;
        tracing::info!(articles = count, "Search index rebuilt");
        println!("Search index rebuilt: {} articles indexed", count);
    }

    // Handle --export flag
    if let Some(export_path) = &args.export {
        let parent = export_path.parent().unwrap_or(Path::new("."));
        if !parent.exists() {
            anyhow::bail!("Parent directory does not exist: {}", parent.display());
        }
        let feeds = db.get_feeds_for_export().await?;
        if feeds.is_empty() {
            println!("No feeds to export.");
            return Ok(());
        }
        // Convert storage::OpmlFeed to feed::OpmlFeed (identical fields, separate types)
        let export_feeds: Vec<feed::OpmlFeed> = feeds
            .into_iter()
            .map(|f| feed::OpmlFeed {
                title: f.title,
                xml_url: f.xml_url,
                html_url: f.html_url,
            })
            .collect();
        feed::export_to_file(&export_feeds, export_path)?;
        println!(
            "Exported {} feeds to: {}",
            export_feeds.len(),
            export_path.display()
        );
        return Ok(());
    }

    // Create app state
    let mut app = App::new(db.clone()).context("Failed to create application")?;

    // Load initial data
    app.feeds = std::sync::Arc::new(
        db.get_feeds_with_unread_counts()
            .await
            .context("Failed to load feeds")?,
    );

    // PERF-005: Build feed title cache
    app.rebuild_feed_cache();

    // Load config and preferences
    let config_path = config_dir.join("config.toml");
    let config = config::Config::load(&config_path).unwrap_or_else(|e| {
        tracing::warn!(error = %e, "Failed to load config, using defaults");
        config::Config::default()
    });
    let prefs = preferences::PreferenceManager::load(&config, &db)
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e, "Failed to load preferences, using defaults");
            // Fallback: load from config alone (no DB)
            preferences::PreferenceManager::from_config(&config)
        });

    // Restore session if enabled
    if prefs.restore_session() {
        if let Some(snapshot_json) = db.get_preference("session.snapshot").await.unwrap_or(None) {
            match serde_json::from_str::<app::SessionSnapshot>(&snapshot_json) {
                Ok(snapshot) => {
                    app.restore(snapshot);
                    app.set_status("Session restored");
                    tracing::info!("Session restored from preferences");
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Corrupt session snapshot, ignoring");
                }
            }
        }
    }

    // Create event channel for background tasks
    // Sized for burst scenarios: 10 concurrent feed refreshes x progress/complete events
    // + concurrent content loads + search results + star toggles
    let (event_tx, event_rx) = mpsc::channel::<AppEvent>(256);

    // Run the TUI
    ui::run(&mut app, event_tx, event_rx).await?;

    println!("Goodbye!");
    Ok(())
}
