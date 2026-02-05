use crate::content::ContentError;
use crate::storage::{Article, Database, Feed};
use anyhow::Result;
use ratatui::text::Line;
use reqwest::redirect::Policy;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::time::Instant;

// ============================================================================
// Cached Article State (PERF-008)
// ============================================================================

/// Cached article state for restoring after search/starred mode.
///
/// When entering search or starred mode, the current articles list and selection
/// are cached. On exit, if the feed selection hasn't changed, the cached state
/// is restored without a DB query.
///
/// Uses `Arc<Vec<Article>>` for O(1) cache creation - just increments reference count
/// instead of cloning the entire article list.
#[derive(Clone)]
pub struct CachedArticleState {
    /// Feed ID at time of caching (for staleness check)
    pub feed_id: Option<i64>,
    /// Cached articles list (Arc for zero-copy caching)
    pub articles: Arc<Vec<Article>>,
    /// Selected article index at time of caching
    pub selected: usize,
}

// ============================================================================
// HTTP Client Configuration
// ============================================================================

/// Create a custom redirect policy with loop detection and limited hops.
///
/// - Limits redirects to 3 hops maximum
/// - Detects redirect loops (same URL appearing twice in chain)
/// - Logs redirect chain for debugging
fn create_redirect_policy() -> Policy {
    Policy::custom(|attempt| {
        // Limit to 3 redirects
        if attempt.previous().len() >= 3 {
            return attempt.error("Too many redirects (max 3)");
        }

        // Detect loops
        let url = attempt.url();
        for prev in attempt.previous() {
            if prev.as_str() == url.as_str() {
                return attempt.error("Redirect loop detected");
            }
        }

        // Log redirect chain
        tracing::debug!(
            from = %attempt.previous().last().map(|u| u.as_str()).unwrap_or("initial"),
            to = %url,
            hop = attempt.previous().len() + 1,
            "Following redirect"
        );

        attempt.follow()
    })
}

// ============================================================================
// View and Focus Enums
// ============================================================================

/// Current view mode
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Browse, // Side-by-side feeds/articles
    Reader, // Full-screen article reader
}

/// Which panel has focus in Browse view
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    WhatsNew,
    Feeds,
    Articles,
}

// ============================================================================
// Content and Event Types
// ============================================================================

/// Content loading state for article reader
///
/// Note: `article_id` fields in Loading/Loaded/Failed variants are stored but
/// not read by rendering logic (ui/reader.rs uses `..` to ignore them).
/// Retained for: Debug trait output, future validation, tracing.
/// The `content` field in Loaded is also stored but only `rendered_lines` is read.
/// Annotation retained because: variant fields are part of enum definition.
///
/// PERF-010: `fallback` uses `Arc<str>` for cheap cloning from Article.summary.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum ContentState {
    Idle,
    Loading {
        article_id: i64,
    },
    Loaded {
        article_id: i64,
        content: String,
        rendered_lines: Vec<Line<'static>>, // PERF-004: Cached render
    },
    Failed {
        article_id: i64,
        error: String,
        fallback: Option<Arc<str>>,
    },
}

/// Result from fetching a single feed
#[derive(Debug, Clone)]
pub struct FetchResult {
    pub feed_id: i64,
    pub new_articles: usize,
    pub error: Option<String>,
}

/// Events from background tasks
pub enum AppEvent {
    RefreshProgress(usize, usize),
    RefreshComplete(Vec<FetchResult>),
    /// Content loaded for an article.
    ///
    /// Fields:
    /// - `article_id`: The article this content belongs to
    /// - `generation`: The generation counter when this load was spawned
    /// - `result`: The content or error from fetching
    ContentLoaded {
        article_id: i64,
        generation: u64,
        result: Result<String, ContentError>,
    },
    FeedRateLimited {
        feed_title: Arc<str>, // PERF-016: Zero-copy from Feed.title
        delay_secs: u64,
    },
    StarToggled {
        article_id: i64,
        starred: bool,
    },
    StarToggleFailed {
        article_id: i64,
        original_status: bool,
    },
    ContentCacheFailed {
        article_id: i64,
        error: String,
    },
    /// A background task panicked.
    ///
    /// Fields:
    /// - `task`: Name of the task that panicked (e.g., "refresh", "content_load")
    /// - `error`: The panic message extracted from the panic payload
    TaskPanicked {
        task: &'static str,
        error: String,
    },
    /// Search completed with results.
    ///
    /// PERF-015: Search is now async to prevent UI blocking on large article sets.
    ///
    /// Fields:
    /// - `query`: The search query that was executed
    /// - `generation`: Generation counter when search was spawned (for stale result detection)
    /// - `results`: The matching articles, or error message
    SearchCompleted {
        query: String,
        generation: u64,
        results: Result<Vec<Article>, String>,
    },
}

// ============================================================================
// Application State
// ============================================================================

/// Central application state
pub struct App {
    pub db: Database,
    pub http_client: reqwest::Client,

    // Data
    /// Feed list wrapped in Arc for O(1) cloning during refresh (PERF-011).
    /// Mutations require creating a new Vec and wrapping in new Arc.
    pub feeds: Arc<Vec<Feed>>,
    /// Article list wrapped in Arc for O(1) caching (PERF-008).
    /// Mutations require creating a new Vec and wrapping in new Arc.
    pub articles: Arc<Vec<Article>>,

    // UI State
    pub view: View,
    pub focus: Focus,
    pub selected_feed: usize,
    pub selected_article: usize,
    pub scroll_offset: usize,

    /// BUG-006: Tracks last user input time for idle detection.
    /// Used to avoid stealing focus to What's New panel during active navigation.
    pub last_input_time: Instant,

    // Search
    pub search_mode: bool,
    pub search_input: String,
    /// Feed ID when search was initiated (for restoring on ESC)
    pub search_feed_id: Option<i64>,

    /// PERF-006: Debounce timer for search
    pub search_debounce: Option<Instant>,
    /// PERF-006: Pending search query
    pub pending_search: Option<String>,

    // Content loading
    pub content_state: ContentState,
    pub reader_article: Option<Article>, // The article currently being read

    // Refresh progress
    pub refresh_progress: Option<(usize, usize)>,

    // Status message with expiry
    pub status_message: Option<(String, Instant)>,

    // What's New panel - shows recent articles after refresh
    // PERF-009: Uses Arc<str> for feed titles to avoid cloning
    pub whats_new: Vec<(Arc<str>, Article)>,
    pub whats_new_selected: usize,
    pub show_whats_new: bool,

    // PERF-005: Cached feed ID -> title mapping
    // PERF-009: Uses Arc<str> for cheap cloning
    pub feed_title_cache: HashMap<i64, Arc<str>>,

    // Starred articles view mode
    pub starred_mode: bool,

    /// PERF-014: Pre-formatted feed prefixes for starred mode display.
    /// Key: feed_id, Value: formatted "[FeedTitle] " string.
    /// Populated when entering starred mode to avoid N allocations per render frame.
    pub feed_prefix_cache: HashMap<i64, String>,

    /// PERF-008: Cached articles for restoring after search/starred mode
    pub cached_articles: Option<CachedArticleState>,

    /// PERF-010: Dirty flag to skip unnecessary frame renders
    pub needs_redraw: bool,

    /// Track article ID currently being fetched to prevent duplicate requests
    pub content_loading_for: Option<i64>,

    /// Generation counter for content loading to handle race conditions.
    ///
    /// Incremented each time a new content load is spawned. The spawned task
    /// includes this generation in its response. When handling ContentLoaded,
    /// we reject responses where generation doesn't match, preventing stale
    /// content from overwriting newer requests (e.g., user navigates A->B->A quickly).
    pub content_load_generation: u64,

    /// Handle to the current content load task for cancellation.
    ///
    /// When a new content load is started, any previous load is aborted via this handle.
    /// Also aborted when exiting reader view to prevent orphaned tasks.
    pub content_load_handle: Option<tokio::task::JoinHandle<()>>,

    /// PERF-015: Generation counter for search to handle rapid typing.
    ///
    /// Incremented each time a new search is spawned. When handling SearchCompleted,
    /// we reject responses where generation doesn't match, preventing stale results
    /// from overwriting newer searches (e.g., user types "rust" then "python" quickly).
    pub search_generation: u64,

    /// Handle to the current search task for cancellation.
    ///
    /// When a new search is spawned, any previous search is aborted via this handle.
    /// Also aborted when exiting search mode to prevent orphaned tasks.
    pub search_handle: Option<tokio::task::JoinHandle<()>>,

    /// Last known reader viewport size (visible lines).
    ///
    /// Updated during reader rendering to enable scroll clamping in input handlers.
    /// Does not include border lines (2 lines for top/bottom borders).
    pub reader_visible_lines: usize,
}

impl App {
    pub fn new(db: Database) -> Result<Self> {
        // PERF-019: Configure HTTP client with connection pooling and keepalive
        let http_client = reqwest::Client::builder()
            .redirect(create_redirect_policy())
            .pool_max_idle_per_host(5) // Limit idle connections per host
            .pool_idle_timeout(std::time::Duration::from_secs(90)) // Close idle connections after 90s
            .tcp_keepalive(std::time::Duration::from_secs(60)) // TCP keepalive probes
            .timeout(std::time::Duration::from_secs(30)) // Default request timeout
            .build()?;

        Ok(Self {
            db,
            http_client,
            feeds: Arc::new(Vec::new()),
            articles: Arc::new(Vec::new()),
            view: View::Browse,
            focus: Focus::Feeds,
            selected_feed: 0,
            selected_article: 0,
            scroll_offset: 0,
            last_input_time: Instant::now(),
            search_mode: false,
            search_input: String::new(),
            search_feed_id: None,
            search_debounce: None,
            pending_search: None,
            content_state: ContentState::Idle,
            reader_article: None,
            refresh_progress: None,
            status_message: None,
            whats_new: Vec::new(),
            whats_new_selected: 0,
            show_whats_new: false,
            feed_title_cache: HashMap::new(),
            starred_mode: false,
            feed_prefix_cache: HashMap::new(),
            cached_articles: None,
            needs_redraw: true,
            content_loading_for: None,
            content_load_generation: 0,
            content_load_handle: None,
            search_generation: 0,
            search_handle: None,
            reader_visible_lines: 0,
        })
    }

    /// PERF-005: Rebuild feed title cache from current feeds list
    /// PERF-009: Uses Arc::clone for cheap reference counting instead of String clone
    /// Call this after any operation that modifies `self.feeds`
    pub fn rebuild_feed_cache(&mut self) {
        self.feed_title_cache = self
            .feeds
            .iter()
            .map(|f| (f.id, Arc::clone(&f.title)))
            .collect();
    }

    /// PERF-004: Incremental feed cache update for specific feed IDs.
    ///
    /// Updates only the specified feed IDs in the cache instead of rebuilding
    /// the entire HashMap. O(k) where k = feed_ids.len() vs O(n) for full rebuild.
    ///
    /// Use this when you know exactly which feeds changed (e.g., after refreshing
    /// a subset of feeds). Falls back to full rebuild if feed_ids is large
    /// relative to total feeds.
    #[allow(dead_code)] // Public API used in tests
    pub fn update_feed_cache_for(&mut self, feed_ids: &[i64]) {
        // If updating more than half the feeds, full rebuild is more efficient
        // due to HashMap resize amortization
        if feed_ids.len() > self.feeds.len() / 2 {
            self.rebuild_feed_cache();
            return;
        }

        // Build a temporary lookup for the feeds we need to update
        // PERF-012: Convert feed_ids to HashSet for O(1) lookup instead of O(n) Vec.contains
        use std::collections::HashSet;
        let feed_ids_set: HashSet<i64> = feed_ids.iter().copied().collect();
        let feed_map: HashMap<i64, &Feed> = self
            .feeds
            .iter()
            .filter(|f| feed_ids_set.contains(&f.id))
            .map(|f| (f.id, f))
            .collect();

        // Update only the specified entries
        for &feed_id in feed_ids {
            if let Some(feed) = feed_map.get(&feed_id) {
                self.feed_title_cache
                    .insert(feed_id, Arc::clone(&feed.title));
            } else {
                // Feed was removed, clean up stale cache entry
                self.feed_title_cache.remove(&feed_id);
            }
        }
    }

    /// Synchronize feed title cache with current feeds list, removing stale entries.
    ///
    /// This method ensures cache consistency by:
    /// 1. Removing entries for feeds no longer in the feeds list
    /// 2. Updating all current feed entries
    ///
    /// Use this after any operation that may have deleted feeds (e.g., refresh
    /// that detects removed feeds, OPML reimport). Unlike `rebuild_feed_cache()`,
    /// this method logs when stale entries are removed for debugging.
    ///
    /// BUG-011: Fixes issue where starred mode shows wrong feed titles after
    /// feed deletion because cache retained stale entries.
    pub fn sync_feed_cache(&mut self) {
        use std::collections::HashSet;

        let current_feed_ids: HashSet<i64> = self.feeds.iter().map(|f| f.id).collect();

        // Find and remove stale cache entries
        let stale_ids: Vec<i64> = self
            .feed_title_cache
            .keys()
            .filter(|id| !current_feed_ids.contains(id))
            .copied()
            .collect();

        for id in &stale_ids {
            self.feed_title_cache.remove(id);
            tracing::debug!(feed_id = id, "Removed stale feed from title cache");
        }

        if !stale_ids.is_empty() {
            tracing::info!(
                count = stale_ids.len(),
                "Removed stale feed title cache entries"
            );
        }

        // Update entries for current feeds
        for feed in self.feeds.iter() {
            self.feed_title_cache
                .insert(feed.id, Arc::clone(&feed.title));
        }
    }

    /// BUG-004: Clamp all selection indices to valid ranges.
    ///
    /// Call this after any operation that may invalidate selection indices,
    /// such as background refresh completing, article deletion, or feed removal.
    /// Ensures indices never point past the end of their respective lists.
    pub fn clamp_selections(&mut self) {
        self.selected_feed = if self.feeds.is_empty() {
            0
        } else {
            self.selected_feed.min(self.feeds.len().saturating_sub(1))
        };
        self.selected_article = if self.articles.is_empty() {
            0
        } else {
            self.selected_article
                .min(self.articles.len().saturating_sub(1))
        };
        self.whats_new_selected = if self.whats_new.is_empty() {
            0
        } else {
            self.whats_new_selected
                .min(self.whats_new.len().saturating_sub(1))
        };

        // Debug assertions to catch missed clamp_selections calls during development
        debug_assert!(
            self.feeds.is_empty() || self.selected_feed < self.feeds.len(),
            "selected_feed {} out of bounds for feeds len {}",
            self.selected_feed,
            self.feeds.len()
        );
        debug_assert!(
            self.articles.is_empty() || self.selected_article < self.articles.len(),
            "selected_article {} out of bounds for articles len {}",
            self.selected_article,
            self.articles.len()
        );
        debug_assert!(
            self.whats_new.is_empty() || self.whats_new_selected < self.whats_new.len(),
            "whats_new_selected {} out of bounds for whats_new len {}",
            self.whats_new_selected,
            self.whats_new.len()
        );
    }

    /// Get currently selected feed (bounds-checked)
    pub fn selected_feed(&self) -> Option<&Feed> {
        self.feeds.get(self.selected_feed)
    }

    /// Get currently selected article (bounds-checked)
    pub fn selected_article(&self) -> Option<&Article> {
        self.articles.get(self.selected_article)
    }

    /// Get currently selected What's New item (bounds-checked with auto-clamp)
    ///
    /// If `whats_new_selected` is out of bounds (e.g., list shrank during refresh),
    /// this method clamps it to a valid range before returning.
    ///
    /// Note: Available as a safe access pattern for What's New items.
    #[allow(dead_code)]
    pub fn selected_whats_new(&mut self) -> Option<&(Arc<str>, Article)> {
        if self.whats_new.is_empty() {
            self.whats_new_selected = 0;
            return None;
        }
        // Clamp selection to valid range
        if self.whats_new_selected >= self.whats_new.len() {
            self.whats_new_selected = self.whats_new.len().saturating_sub(1);
        }
        self.whats_new.get(self.whats_new_selected)
    }

    /// Navigate up in current list
    pub fn nav_up(&mut self) {
        match self.focus {
            Focus::WhatsNew => {
                self.whats_new_selected = self.whats_new_selected.saturating_sub(1);
            }
            Focus::Feeds => {
                self.selected_feed = self.selected_feed.saturating_sub(1);
            }
            Focus::Articles => {
                self.selected_article = self.selected_article.saturating_sub(1);
            }
        }
    }

    /// Navigate down in current list
    pub fn nav_down(&mut self) {
        match self.focus {
            Focus::WhatsNew => {
                if !self.whats_new.is_empty() {
                    let max_index = self.whats_new.len().saturating_sub(1);
                    self.whats_new_selected =
                        self.whats_new_selected.saturating_add(1).min(max_index);
                }
            }
            Focus::Feeds => {
                if !self.feeds.is_empty() {
                    let max_index = self.feeds.len().saturating_sub(1);
                    self.selected_feed = self.selected_feed.saturating_add(1).min(max_index);
                }
            }
            Focus::Articles => {
                if !self.articles.is_empty() {
                    let max_index = self.articles.len().saturating_sub(1);
                    self.selected_article = self.selected_article.saturating_add(1).min(max_index);
                }
            }
        }
    }

    /// Dismiss the What's New panel
    pub fn dismiss_whats_new(&mut self) {
        self.show_whats_new = false;
        self.whats_new.clear();
        self.whats_new_selected = 0;
        if self.focus == Focus::WhatsNew {
            self.focus = Focus::Feeds;
        }
    }

    /// Scroll up in reader view
    pub fn scroll_up(&mut self, lines: usize) {
        self.scroll_offset = self.scroll_offset.saturating_sub(lines);
    }

    /// Scroll down in reader view
    pub fn scroll_down(&mut self, lines: usize) {
        self.scroll_offset = self.scroll_offset.saturating_add(lines);
    }

    /// Clamp scroll offset to valid range based on content and viewport size.
    ///
    /// This prevents scrolling past the end of content. Call this after scrolling
    /// or when content/viewport size changes (e.g., terminal resize, content load).
    ///
    /// # Arguments
    /// * `content_lines` - Total number of lines in the content
    /// * `visible_lines` - Number of lines visible in the viewport (excluding borders)
    pub fn clamp_scroll(&mut self, content_lines: usize, visible_lines: usize) {
        let max_scroll = content_lines.saturating_sub(visible_lines);
        self.scroll_offset = self.scroll_offset.min(max_scroll);
    }

    /// Get the number of content lines in the reader view.
    ///
    /// Returns the line count from rendered content if loaded, or a small
    /// placeholder count for loading/error states. Includes the 3-line header.
    pub fn reader_content_lines(&self) -> usize {
        const HEADER_LINES: usize = 3; // Title, feed/time, blank line
        let content_lines = match &self.content_state {
            ContentState::Idle => 1,
            ContentState::Loading { .. } => 1,
            ContentState::Loaded { rendered_lines, .. } => rendered_lines.len(),
            ContentState::Failed { fallback, .. } => {
                // Error line + blank + optional summary
                2 + fallback.as_ref().map_or(0, |s| s.lines().count() + 2)
            }
        };
        HEADER_LINES + content_lines
    }

    /// Clamp scroll offset to content bounds using stored viewport size.
    ///
    /// Convenience method that uses `reader_visible_lines` from last render.
    /// Call this after scroll operations in the reader view.
    pub fn clamp_reader_scroll(&mut self) {
        let content_lines = self.reader_content_lines();
        self.clamp_scroll(content_lines, self.reader_visible_lines);
    }

    /// Set status message (will auto-expire after 3 seconds)
    pub fn set_status(&mut self, msg: impl Into<String>) {
        self.status_message = Some((msg.into(), Instant::now()));
    }

    /// Clear status message if expired (older than 3 seconds)
    /// Returns true if a message was actually cleared
    pub fn clear_expired_status(&mut self) -> bool {
        if let Some((_, time)) = &self.status_message {
            if time.elapsed().as_secs() >= 3 {
                self.status_message = None;
                return true;
            }
        }
        false
    }

    /// Enter reader view for currently selected article.
    /// Returns a reference to the stored article for content fetching.
    ///
    /// PERF-005: Clone is required here because `reader_article` must own the
    /// Article data for display during async content fetch. The reader view
    /// needs stable article data even if `self.articles` changes during the
    /// async operation (e.g., background refresh, search mode change).
    /// Alternatives considered:
    /// - Index-based access: Would require re-validation on every render and
    ///   could show wrong article if list changes.
    /// - Arc<Article>: Would require changing Article storage throughout the
    ///   codebase; clone cost is acceptable for single-article reader entry.
    pub fn enter_reader(&mut self) -> Option<Article> {
        let article = self.articles.get(self.selected_article)?.clone();
        let article_id = article.id;

        self.view = View::Reader;
        self.scroll_offset = 0;
        self.content_state = ContentState::Loading { article_id };
        self.reader_article = Some(article.clone());
        Some(article)
    }

    /// Exit reader view back to browse
    pub fn exit_reader(&mut self) {
        // Abort any in-flight content load to prevent orphaned tasks
        if let Some(handle) = self.content_load_handle.take() {
            handle.abort();
            tracing::debug!("Aborted content load task on reader exit");
        }

        self.view = View::Browse;
        self.content_state = ContentState::Idle;
        self.content_loading_for = None; // BUG-001: Clear loading flag on view exit
        self.scroll_offset = 0;
        self.reader_article = None;
    }
}

// ============================================================================
// Resource Cleanup
// ============================================================================

/// RES-002: Abort all in-flight async tasks on App drop.
///
/// Ensures proper cleanup when the application exits, preventing orphaned
/// tokio tasks from continuing to run after the main event loop terminates.
impl Drop for App {
    fn drop(&mut self) {
        if let Some(handle) = self.content_load_handle.take() {
            handle.abort();
            tracing::debug!("Aborted content load task on App drop");
        }
        if let Some(handle) = self.search_handle.take() {
            handle.abort();
            tracing::debug!("Aborted search task on App drop");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::Database;
    use tokio::time::{self, Duration};

    async fn test_app() -> App {
        let db = Database::open(":memory:").await.unwrap();
        App::new(db).unwrap()
    }

    // Navigation tests
    #[tokio::test]
    async fn test_nav_empty_list() {
        let app = test_app().await;
        assert!(app.selected_feed().is_none());
        assert!(app.selected_article().is_none());
    }

    #[tokio::test]
    async fn test_scroll_up_at_zero() {
        let mut app = test_app().await;
        app.scroll_offset = 0;
        app.scroll_up(1);
        assert_eq!(app.scroll_offset, 0); // Should saturate at 0
    }

    #[tokio::test]
    async fn test_scroll_down_increment() {
        let mut app = test_app().await;
        app.scroll_offset = 0;
        app.scroll_down(1);
        assert!(app.scroll_offset > 0);
    }

    // Status message expiry with time control
    #[tokio::test]
    async fn test_status_expires_after_3_seconds() {
        // Create app before pausing time to avoid DB connection timeout
        let mut app = test_app().await;
        time::pause();
        app.set_status("Test message");

        assert!(app.status_message.is_some());

        time::advance(Duration::from_secs(2)).await;
        app.clear_expired_status();
        assert!(app.status_message.is_some()); // Still present at 2s

        time::advance(Duration::from_secs(2)).await;
        app.clear_expired_status();
        assert!(app.status_message.is_none()); // Expired after 3s
    }

    #[tokio::test]
    async fn test_status_not_expired_before_3_seconds() {
        // Create app before pausing time to avoid DB connection timeout
        let mut app = test_app().await;
        time::pause();
        app.set_status("Test");

        time::advance(Duration::from_millis(2999)).await;
        app.clear_expired_status();
        assert!(app.status_message.is_some());
    }

    // View transitions
    #[tokio::test]
    async fn test_exit_reader_resets_scroll() {
        let mut app = test_app().await;
        app.view = View::Reader;
        app.scroll_offset = 100;

        app.exit_reader();

        assert!(matches!(app.view, View::Browse));
        assert_eq!(app.scroll_offset, 0);
    }

    #[tokio::test]
    async fn test_dismiss_whats_new() {
        let mut app = test_app().await;
        app.show_whats_new = true;
        app.dismiss_whats_new();
        assert!(!app.show_whats_new);
    }

    // Helper to create a test Feed with minimal required fields
    fn test_feed(id: i64, title: &str) -> Feed {
        Feed {
            id,
            title: Arc::from(title),
            url: format!("http://feed{}.com", id),
            html_url: None,
            last_fetched: None,
            error: None,
            unread_count: 0,
            consecutive_failures: 0,
        }
    }

    // BUG-004: clamp_selections tests
    #[tokio::test]
    async fn test_clamp_selections_empty_lists() {
        let mut app = test_app().await;
        app.selected_feed = 10;
        app.selected_article = 20;
        app.whats_new_selected = 30;

        app.clamp_selections();

        assert_eq!(app.selected_feed, 0);
        assert_eq!(app.selected_article, 0);
        assert_eq!(app.whats_new_selected, 0);
    }

    #[tokio::test]
    async fn test_clamp_selections_valid_indices() {
        let mut app = test_app().await;
        app.feeds = Arc::new(vec![test_feed(1, "Feed A"), test_feed(2, "Feed B")]);
        app.selected_feed = 1;

        app.clamp_selections();

        assert_eq!(app.selected_feed, 1); // Valid index unchanged
    }

    #[tokio::test]
    async fn test_clamp_selections_out_of_bounds() {
        let mut app = test_app().await;
        app.feeds = Arc::new(vec![test_feed(1, "Feed A")]);
        app.selected_feed = 5; // Out of bounds

        app.clamp_selections();

        assert_eq!(app.selected_feed, 0); // Clamped to max valid index
    }

    // PERF-004: update_feed_cache_for tests
    #[tokio::test]
    async fn test_update_feed_cache_incremental() {
        let mut app = test_app().await;
        app.feeds = Arc::new(vec![
            test_feed(1, "Feed A"),
            test_feed(2, "Feed B"),
            test_feed(3, "Feed C"),
        ]);
        app.rebuild_feed_cache();

        // Simulate updating feed 2's title
        app.feeds = Arc::new(vec![
            test_feed(1, "Feed A"),
            test_feed(2, "Updated Feed B"),
            test_feed(3, "Feed C"),
        ]);

        // Only update cache for feed 2
        app.update_feed_cache_for(&[2]);

        assert_eq!(
            app.feed_title_cache.get(&1).map(|s| s.as_ref()),
            Some("Feed A")
        );
        assert_eq!(
            app.feed_title_cache.get(&2).map(|s| s.as_ref()),
            Some("Updated Feed B")
        );
        assert_eq!(
            app.feed_title_cache.get(&3).map(|s| s.as_ref()),
            Some("Feed C")
        );
    }

    #[tokio::test]
    async fn test_update_feed_cache_removes_deleted() {
        let mut app = test_app().await;
        app.feeds = Arc::new(vec![test_feed(1, "Feed A"), test_feed(2, "Feed B")]);
        app.rebuild_feed_cache();
        assert!(app.feed_title_cache.contains_key(&2));

        // Remove feed 2 from list
        app.feeds = Arc::new(vec![test_feed(1, "Feed A")]);

        // Update cache for removed feed
        app.update_feed_cache_for(&[2]);

        assert!(!app.feed_title_cache.contains_key(&2)); // Should be removed
        assert!(app.feed_title_cache.contains_key(&1)); // Others unchanged
    }

    // BUG-011: sync_feed_cache tests
    #[tokio::test]
    async fn test_sync_feed_cache_removes_stale_entries() {
        let mut app = test_app().await;

        // Start with 3 feeds in cache
        app.feeds = Arc::new(vec![
            test_feed(1, "Feed A"),
            test_feed(2, "Feed B"),
            test_feed(3, "Feed C"),
        ]);
        app.rebuild_feed_cache();
        assert_eq!(app.feed_title_cache.len(), 3);

        // Simulate feeds 2 and 3 being deleted (only feed 1 remains)
        app.feeds = Arc::new(vec![test_feed(1, "Feed A")]);

        // sync_feed_cache should remove stale entries
        app.sync_feed_cache();

        assert_eq!(app.feed_title_cache.len(), 1);
        assert!(app.feed_title_cache.contains_key(&1));
        assert!(!app.feed_title_cache.contains_key(&2));
        assert!(!app.feed_title_cache.contains_key(&3));
    }

    #[tokio::test]
    async fn test_sync_feed_cache_updates_existing_titles() {
        let mut app = test_app().await;

        // Start with feed
        app.feeds = Arc::new(vec![test_feed(1, "Old Title")]);
        app.rebuild_feed_cache();
        assert_eq!(
            app.feed_title_cache.get(&1).map(|s| s.as_ref()),
            Some("Old Title")
        );

        // Update title
        app.feeds = Arc::new(vec![test_feed(1, "New Title")]);
        app.sync_feed_cache();

        assert_eq!(
            app.feed_title_cache.get(&1).map(|s| s.as_ref()),
            Some("New Title")
        );
    }

    #[tokio::test]
    async fn test_sync_feed_cache_handles_empty_feeds() {
        let mut app = test_app().await;

        // Start with feeds
        app.feeds = Arc::new(vec![test_feed(1, "Feed A"), test_feed(2, "Feed B")]);
        app.rebuild_feed_cache();

        // All feeds removed
        app.feeds = Arc::new(vec![]);
        app.sync_feed_cache();

        assert!(app.feed_title_cache.is_empty());
    }

    // Scroll clamping tests
    #[tokio::test]
    async fn test_clamp_scroll_within_bounds() {
        let mut app = test_app().await;
        app.scroll_offset = 5;
        app.clamp_scroll(100, 20); // 100 content lines, 20 visible

        // max_scroll = 100 - 20 = 80, offset 5 is within bounds
        assert_eq!(app.scroll_offset, 5);
    }

    #[tokio::test]
    async fn test_clamp_scroll_exceeds_max() {
        let mut app = test_app().await;
        app.scroll_offset = 100;
        app.clamp_scroll(50, 20); // 50 content lines, 20 visible

        // max_scroll = 50 - 20 = 30, offset 100 should be clamped to 30
        assert_eq!(app.scroll_offset, 30);
    }

    #[tokio::test]
    async fn test_clamp_scroll_content_smaller_than_viewport() {
        let mut app = test_app().await;
        app.scroll_offset = 10;
        app.clamp_scroll(15, 20); // 15 content lines, 20 visible

        // max_scroll = 15.saturating_sub(20) = 0
        assert_eq!(app.scroll_offset, 0);
    }

    #[tokio::test]
    async fn test_clamp_reader_scroll_with_loaded_content() {
        use ratatui::text::Line;

        let mut app = test_app().await;
        app.scroll_offset = 100;
        app.reader_visible_lines = 20;

        // Set up loaded content with 30 lines
        let rendered_lines: Vec<Line<'static>> = (0..30).map(|_| Line::from("test")).collect();
        app.content_state = ContentState::Loaded {
            article_id: 1,
            content: "test".to_string(),
            rendered_lines,
        };

        app.clamp_reader_scroll();

        // content = 3 (header) + 30 = 33 lines
        // max_scroll = 33 - 20 = 13
        assert_eq!(app.scroll_offset, 13);
    }

    #[tokio::test]
    async fn test_reader_content_lines_idle() {
        let app = test_app().await;
        // Idle: 3 header + 1 content = 4
        assert_eq!(app.reader_content_lines(), 4);
    }

    #[tokio::test]
    async fn test_reader_content_lines_loaded() {
        use ratatui::text::Line;

        let mut app = test_app().await;
        let rendered_lines: Vec<Line<'static>> = (0..50).map(|_| Line::from("test")).collect();
        app.content_state = ContentState::Loaded {
            article_id: 1,
            content: "test".to_string(),
            rendered_lines,
        };

        // Loaded: 3 header + 50 content = 53
        assert_eq!(app.reader_content_lines(), 53);
    }
}
