# Repository Guidelines

## Project Overview
Skim (`skim`) is a Rust 2021 terminal RSS reader (TUI). It fetches feeds concurrently, stores state in SQLite, and can extract “clean” article content via the jina.ai Reader API.

## Project Structure & Module Organization
- `src/main.rs`: CLI parsing and app startup
- `src/app.rs`: central state, navigation, view/focus management
- `src/feed/`: OPML import, feed fetching, RSS/Atom parsing
- `src/storage/`: SQLite persistence (via `sqlx`)
- `src/content/`: jina.ai content fetching/extraction
- `src/ui/`: ratatui widgets + event loop
- `docs/`: specs/notes (currently ignored by `.gitignore`)

## Build, Test, and Development Commands
- `cargo build`: dev build
- `cargo run -- [args]`: run locally (example: `RUST_LOG=debug cargo run`)
- `cargo build --release`: optimized build (`./target/release/skim`)
- `cargo install --path .`: install binary to your Cargo bin dir
- `cargo fmt`: format (CI enforces `cargo fmt --check`)
- `cargo clippy -- -D warnings`: lint (CI treats warnings as errors)
- `cargo test`: run unit/property tests

## Coding Style & Naming Conventions
- Formatting: `rustfmt` (don’t hand-format).
- Indentation: 4 spaces by default; YAML/TOML use 2 spaces (see `.editorconfig`).
- Naming: modules/functions `snake_case`, types/traits `CamelCase`, constants `SCREAMING_SNAKE_CASE`.
- Prefer `tracing::info!(key = %value, ...)` over string formatting; use `thiserror` for typed errors; avoid unnecessary `clone()`/`to_string()`.

## Testing Guidelines
- Tests are inline (`#[cfg(test)] mod tests`) next to the code under test.
- Keep tests offline: mock HTTP with `wiremock`; use `proptest` for parsers/validators.
- Helpful: `RUST_LOG=debug cargo test -- --nocapture` when debugging failures.

## Configuration & Data Safety
- Runtime data lives in `~/.config/skim/` (`feeds.opml`, `rss.db`).
- Env vars: `JINA_API_KEY` (optional), `RUST_LOG`.
- Don’t commit local data: `*.db` and `.env` are ignored for a reason.

## Commit & Pull Request Guidelines
- Commits: conventional commits `type(scope): summary` (e.g., `fix(storage): handle empty query`).
  - Common types: `feat`, `fix`, `refactor`, `test`, `docs`, `chore`.
- PRs: include a clear description + test steps; attach a screenshot/recording for UI changes; ensure `cargo fmt`, `cargo clippy`, and `cargo test` pass.

