//! Feed management module for RSS/Atom feed parsing and fetching.
//!
//! This module provides the core functionality for working with RSS and Atom feeds:
//!
//! - **Parsing**: Convert RSS/Atom XML into structured article data
//! - **Fetching**: Concurrent HTTP retrieval with retry logic and rate limiting
//! - **OPML Import**: Parse OPML subscription lists for bulk feed import
//!
//! # Architecture
//!
//! The module is organized into three submodules:
//!
//! - [`parser`] - Low-level feed parsing using the `feed-rs` crate
//! - [`fetcher`] - HTTP fetching with progress reporting and database integration
//! - [`opml`] - OPML file parsing for subscription import/export
//!
//! # Example
//!
//! ```ignore
//! use crate::feed::{parse, refresh_all, refresh_one};
//!
//! // Import feeds from OPML file
//! let feeds = parse("/path/to/subscriptions.opml")?;
//!
//! // Refresh all feeds concurrently
//! let results = refresh_all(db, client, feeds, progress_tx).await;
//! ```

mod discovery;
mod fetcher;
mod opml;
mod parser;

#[allow(unused_imports)] // Re-exported for downstream consumers (TASK-10 integration tests)
pub use discovery::DiscoveryError;
pub use discovery::{discover_feed, DiscoveredFeed};
pub use fetcher::{refresh_all, refresh_one};
pub use opml::{export_to_file, export_to_file_with_categories, parse, OpmlFeed};
