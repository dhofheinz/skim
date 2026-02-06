//! Terminal User Interface module.
//!
//! This module provides the TUI for the RSS reader, including:
//! - Main event loop (`run`)
//! - Input handling for browse, reader, and search modes
//! - Rendering for feeds, articles, and reader views
//! - Background task event processing
//!
//! # Module Structure
//!
//! - `loop_runner` - Main event loop and terminal management
//! - `input` - Keyboard input handling
//! - `events` - Background task event processing
//! - `render` - View rendering dispatch
//! - `helpers` - Shared utility functions
//! - `articles` - Article list widget
//! - `feeds` - Feed list widget
//! - `reader` - Article reader widget
//! - `status` - Status bar widget
//! - `whatsnew` - What's New panel widget

// Submodules for UI components
mod articles;
mod categories;
mod events;
mod feeds;
mod help;
mod helpers;
mod input;
mod loop_runner;
pub mod reader;
mod render;
mod status;
mod whatsnew;

// Re-export the public API
pub use loop_runner::{run, Action};
