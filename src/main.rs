use anyhow::{Context, Result};
use chrono::Utc;
use clap::Parser;
use std::io::Write;
use std::path::{Path, PathBuf};
use tokio::sync::mpsc;

mod app;
mod content;
mod feed;
mod storage;
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
        format!(
            "Failed to read source file '{}': check file permissions",
            src.display()
        )
    })?;

    let mut temp_file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true) // Fails atomically if file exists (prevents symlink race)
        .open(&temp_path)
        .with_context(|| {
            format!(
                "Failed to create temporary file '{}': check directory permissions or disk space",
                temp_path.display()
            )
        })?;

    temp_file.write_all(&content).with_context(|| {
        // Clean up temp file on write failure
        let _ = std::fs::remove_file(&temp_path);
        format!(
            "Failed to write to temporary file '{}': disk may be full",
            temp_path.display()
        )
    })?;

    // Sync to disk to ensure data is persisted before rename
    temp_file.sync_all().with_context(|| {
        let _ = std::fs::remove_file(&temp_path);
        format!(
            "Failed to sync temporary file '{}' to disk: disk may be full",
            temp_path.display()
        )
    })?;

    // Drop the file handle before rename
    drop(temp_file);

    // Atomic rename (POSIX guarantees atomicity for rename on same filesystem)
    // On Windows, rename fails if destination exists, so remove it first
    #[cfg(windows)]
    if dst.exists() {
        std::fs::remove_file(dst).with_context(|| {
            let _ = std::fs::remove_file(&temp_path);
            format!(
                "Failed to remove existing '{}' before atomic replace",
                dst.display()
            )
        })?;
    }

    std::fs::rename(&temp_path, dst).with_context(|| {
        let _ = std::fs::remove_file(&temp_path);
        format!(
            "Failed to rename '{}' to '{}': check permissions",
            temp_path.display(),
            dst.display()
        )
    })?;

    Ok(())
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
        // SEC-008: Canonicalize to resolve symlinks and prevent path traversal
        let canonical_import = import_file
            .canonicalize()
            .with_context(|| format!("Failed to resolve import file: {}", import_file.display()))?;

        // Verify it's a regular file (not a directory, device, etc.)
        let metadata = std::fs::metadata(&canonical_import)?;
        if !metadata.is_file() {
            anyhow::bail!("Import path must be a regular file");
        }

        // Basic OPML validation: check for required elements
        let content = std::fs::read_to_string(&canonical_import).with_context(|| {
            format!("Failed to read import file: {}", canonical_import.display())
        })?;
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
                format!(
                    "Failed to create backup at '{}'. Original file is unchanged.",
                    backup_path.display()
                )
            })?;

            // Verify backup exists before proceeding
            if !backup_path.exists() {
                anyhow::bail!(
                    "Backup verification failed: '{}' was not created. Aborting import to protect existing data.",
                    backup_path.display()
                );
            }
            println!("Backed up existing OPML to: {}", backup_path.display());
        }

        // Atomic import: if this fails, the original file remains intact
        // (either unchanged if no backup was needed, or restorable from backup)
        atomic_copy(&canonical_import, &opml_path).with_context(|| {
            format!(
                "Failed to import OPML file '{}'. If a backup was created, your previous feeds are preserved there.",
                canonical_import.display()
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

    // Sync feeds from OPML to database
    db.sync_feeds(&storage_feeds)
        .await
        .context("Failed to sync feeds")?;

    // Handle --rebuild-search flag
    if args.rebuild_search {
        tracing::info!("Rebuilding search index...");
        let count = db
            .rebuild_fts_index()
            .await
            .context("Failed to rebuild search index")?;
        tracing::info!(articles = count, "Search index rebuilt");
        println!("Search index rebuilt: {} articles indexed", count);
    } else {
        // Check FTS consistency on startup with detailed report
        match db.check_fts_consistency_detailed().await {
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
                eprintln!(
                    "Warning: Search index is out of sync (missing: {}, orphaned: {}). Run with --rebuild-search to fix.",
                    report.missing_fts_entries,
                    report.orphaned_fts_entries
                );
            }
            Err(e) => {
                tracing::warn!(error = %e, "Failed to check FTS5 consistency");
            }
        }
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

    // Create event channel for background tasks
    let (event_tx, event_rx) = mpsc::channel::<AppEvent>(32);

    // Run the TUI
    ui::run(&mut app, event_tx, event_rx).await?;

    println!("Goodbye!");
    Ok(())
}
