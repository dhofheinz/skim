# Skim

A terminal RSS reader that fetches clean article content via jina.ai for distraction-free reading.

![Rust](https://img.shields.io/badge/rust-2021-orange)
![License](https://img.shields.io/badge/license-MIT-blue)

## Features

- **Fast TUI** - Keyboard-driven interface built with ratatui
- **Clean content** - Article extraction via jina.ai Reader API
- **Offline-first** - Sync once, read without network
- **Concurrent refresh** - Fetches 10 feeds simultaneously
- **Markdown rendering** - Styled headings, code blocks, emphasis
- **Search** - Global search across all feeds
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

Import your OPML file:

```bash
skim --import /path/to/feeds.opml
```

Then just run:

```bash
skim
```

## Keybindings

### Browse View

| Key | Action |
|-----|--------|
| `j` / `↓` | Navigate down |
| `k` / `↑` | Navigate up |
| `Enter` | Select feed / Open article |
| `Tab` | Cycle focus (Feeds → Articles → What's New) |
| `r` | Refresh all feeds |
| `R` | Refresh selected feed |
| `s` | Toggle star |
| `o` | Open in browser |
| `/` | Search |
| `Esc` | Dismiss What's New panel |
| `q` | Quit |

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
| `rss.db` | SQLite database (articles, read state) |

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
