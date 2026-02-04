# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build & Run

```bash
cargo build              # Dev build
cargo run                # Run with dev profile
cargo install --path .   # Install to ~/.cargo/bin/ for global access
```

Debug logging: `RUST_LOG=debug cargo run`

## CLI Usage

```bash
skim --import /path/to/feeds.opml  # Import OPML file
skim --reset-db                    # Reset database
```

Config stored at `~/.config/rss-reader/` (feeds.opml, rss.db).

## Architecture

**Event Loop Pattern**: The TUI uses `tokio::select!` to multiplex terminal input, background task events, and periodic ticks. Background tasks (feed refresh, content loading) communicate via `mpsc::channel<AppEvent>`.

**Key Data Flow**:
- `main.rs` → parses OPML, opens DB, creates `App` state, runs UI event loop
- `App` (app.rs) → central state: view mode, focus, selections, content state
- `ui/mod.rs` → event loop, input handling, spawns background tasks
- Background tasks send `AppEvent` variants back to main loop

**Modules**:
- `feed/` - OPML parsing (quick-xml), RSS/Atom parsing (feed-rs), concurrent fetching
- `storage/` - SQLite via sqlx (async), feeds/articles tables
- `content/` - jina.ai Reader API (`r.jina.ai/{url}`) for clean article extraction
- `ui/` - ratatui widgets: feeds, articles, reader, whatsnew, status

**View States**: `View::Browse` (two-column feeds/articles) and `View::Reader` (full-screen article). `Focus` tracks which panel is active in Browse view.

**Content Loading**: `ContentState` enum tracks Idle→Loading→Loaded/Failed. Article content fetched on-demand when entering reader, with summary fallback on failure.

## Commit Guidelines

Use conventional commits: `type(scope): description`
- Types: `feat`, `fix`, `refactor`, `test`, `docs`, `chore`
- Pre-commit hooks enforce fmt and clippy - commits will fail if checks don't pass

## Dependencies

Always check crates.io for current stable versions before adding dependencies.
Version pinning: Use major version only (e.g., `"1"` not `"1.0.123"`) unless pinning a specific minor is required.

## Code Style

- Static > Dynamic (generics over trait objects)
- Borrow > Clone (lifetimes are free, `.to_string()` is not)
- Lazy > Eager (return `impl Iterator` not `Vec`, avoid `.collect()` mid-chain)
- Structured logging: `tracing::info!(user_id = %id, ...)` not `info!("user {}", id)`
- thiserror for typed errors