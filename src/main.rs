use anyhow::{Context, Result};
use clap::Parser;
use std::path::PathBuf;
use tokio::sync::mpsc;

mod app;
mod content;
mod feed;
mod storage;
mod ui;

use app::{App, AppEvent};
use storage::{Database, OpmlFeed};

/// Get the config directory path (~/.config/rss-reader/)
fn get_config_dir() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME environment variable not set")?;
    let config_dir = PathBuf::from(home).join(".config").join("rss-reader");
    Ok(config_dir)
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

    let opml_path = config_dir.join("feeds.opml");
    let db_path = config_dir.join("rss.db");

    // Handle --import flag
    if let Some(import_file) = &args.import {
        if !import_file.exists() {
            eprintln!("Error: Import file not found: {}", import_file.display());
            std::process::exit(1);
        }
        std::fs::copy(import_file, &opml_path).context("Failed to copy OPML file")?;
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
    let opml_feeds =
        feed::parse(opml_path.to_str().unwrap()).context("Failed to parse OPML file")?;
    println!(
        "Loaded {} feeds from {}",
        opml_feeds.len(),
        opml_path.display()
    );

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
    let db = Database::open(db_path.to_str().unwrap())
        .await
        .context("Failed to open database")?;

    // Sync feeds from OPML to database
    db.sync_feeds(&storage_feeds)
        .await
        .context("Failed to sync feeds")?;

    // Create app state
    let mut app = App::new(db.clone());

    // Load initial data
    app.feeds = db
        .get_feeds_with_unread_counts()
        .await
        .context("Failed to load feeds")?;

    // Create event channel for background tasks
    let (event_tx, event_rx) = mpsc::channel::<AppEvent>(32);

    // Run the TUI
    ui::run(&mut app, event_tx, event_rx).await?;

    println!("Goodbye!");
    Ok(())
}
