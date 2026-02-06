//! Utility functions for common operations.
//!
//! This module provides reusable utilities for:
//!
//! - **URL validation**: Security-focused validation to prevent SSRF attacks
//! - **Text processing**: Unicode-aware string width calculation and truncation
//!
//! # Examples
//!
//! ```
//! use skim::util::{validate_url, display_width, truncate_to_width};
//!
//! // Validate a feed URL
//! let url = validate_url("https://example.com/feed.xml").unwrap();
//!
//! // Calculate display width for proper terminal rendering
//! let width = display_width("Hello 世界"); // Returns 11 (5 + 2*2 + 2)
//!
//! // Truncate to fit terminal width
//! let truncated = truncate_to_width("Long article title", 15);
//! ```

mod text;
mod url_validator;

pub use text::{display_width, strip_control_chars, truncate_to_width};
pub use url_validator::{validate_url, validate_url_for_open};

/// Maximum allowed search query length — shared across UI validation and FTS5 validation layers
pub const MAX_SEARCH_QUERY_LENGTH: usize = 256;
