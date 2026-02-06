# Skim

A terminal RSS reader that fetches clean article content via jina.ai for distraction-free reading.

![Rust](https://img.shields.io/badge/rust-2021-orange)
![License](https://img.shields.io/badge/license-MIT-blue)

## Features

- **Fast TUI** - Keyboard-driven interface built with ratatui
- **Clean content** - Article extraction via jina.ai Reader API
- **Subscribe by URL** - Discover and add feeds from any URL or HTML page
- **Feed management** - Delete, rename, and organize feeds via context menu
- **Categories** - Collapsible tree sidebar for grouping feeds into folders
- **Offline-first** - Sync once, read without network
- **Concurrent refresh** - Fetches 10 feeds simultaneously
- **Markdown rendering** - Styled headings, code blocks, emphasis
- **Search** - Global search across all feeds
- **OPML import/export** - Round-trip with category nesting preserved
- **Persistent state** - Read/starred status saved in SQLite

## Installation

```bash
cargo install --path .
```

Or build from source:

```bash
cargo build --release
./target/release/skim
```

## Getting Started

Import an OPML file or subscribe to individual feeds:

```bash
skim --import /path/to/feeds.opml
```

Then just run:

```bash
skim
```

Press `+` to subscribe to a feed by URL, or `m` on any feed for rename, move, delete, and more.

## Keybindings

### Browse View

| Key | Action |
|-----|--------|
| `j` / `↓` | Navigate down |
| `k` / `↑` | Navigate up |
| `Enter` | Select feed / Open article |
| `Tab` | Cycle focus (Categories → Feeds → Articles) |
| `r` | Refresh all feeds |
| `R` | Refresh selected feed |
| `s` | Toggle star |
| `o` | Open in browser |
| `/` | Search |
| `+` | Subscribe to feed by URL |
| `d` | Delete selected feed |
| `m` | Feed context menu (rename, move, delete, refresh, open) |
| `c` | Toggle category sidebar |
| `e` | Export feeds to OPML |
| `?` | Show help overlay |
| `Esc` | Dismiss What's New panel |
| `q` | Quit |

### Categories

| Key | Action |
|-----|--------|
| `j` / `↓` | Navigate categories |
| `k` / `↑` | Navigate categories |
| `Enter` | Filter feeds by selected category |
| `h` / `←` | Collapse category |
| `l` / `→` | Expand category |

### Reader View

| Key | Action |
|-----|--------|
| `j` / `↓` | Scroll down |
| `k` / `↑` | Scroll up |
| `Ctrl+d` | Page down |
| `Ctrl+u` | Page up |
| `o` | Open in browser |
| `b` / `Esc` | Back to browse |
| `q` | Quit |

### Search Mode

| Key | Action |
|-----|--------|
| Type | Filter articles by title/summary |
| `Enter` | Confirm search |
| `Esc` | Cancel search |

## Configuration

Config directory: `~/.config/skim/`

| File | Purpose |
|------|---------|
| `feeds.opml` | Your feed subscriptions |
| `rss.db` | SQLite database (articles, categories, read state) |
| `config.toml` | Theme, keybindings, and preferences |

### Environment Variables

| Variable | Purpose |
|----------|---------|
| `JINA_API_KEY` | Optional API key for jina.ai (higher rate limits) |
| `RUST_LOG` | Debug logging level (e.g., `debug`, `info`) |

## CLI Options

```
skim [OPTIONS]

Options:
  --import <FILE>    Import OPML file
  --reset-db         Delete and recreate database
  -h, --help         Print help
```

## Architecture

```
src/
├── main.rs          # Entry point, CLI, startup
├── app.rs           # Central state, navigation
├── feed/            # OPML parsing, feed fetching
├── storage/         # SQLite operations
├── content/         # jina.ai content extraction
└── ui/              # TUI widgets and event loop
```

**Stack**: tokio (async), ratatui (TUI), sqlx (SQLite), reqwest (HTTP), feed-rs (parsing)

## License

MIT
