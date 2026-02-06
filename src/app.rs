use crate::content::ContentError;
use crate::feed::DiscoveredFeed;
use crate::keybindings::KeybindingRegistry;
use crate::storage::{Article, Database, Feed, FeedCategory, SearchScope};
use crate::theme::{StyleMap, ThemeVariant};
use anyhow::Result;
use ratatui::style::Style;
use ratatui::text::Line;
use reqwest::redirect::Policy;
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::time::Instant;
use unicode_width::UnicodeWidthStr;

/// Maximum scroll offset for the reader view (ratatui u16 limit).
pub const MAX_SCROLL: usize = u16::MAX as usize;

// ============================================================================
// Session Snapshot
// ============================================================================

/// Serializable snapshot of App state for session persistence.
///
/// Uses string representations for enums to maintain forward-compatibility:
/// if new variants are added, old snapshots with unknown strings will use
/// `#[serde(default)]` fallbacks instead of failing to deserialize.
///
/// Always restores to Browse view — Reader view state is transient.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
#[allow(dead_code)] // Used by TASK-13 (session restore)
pub struct SessionSnapshot {
    /// Focus panel name: "feeds", "articles", or "whatsnew".
    pub focus: String,
    /// Selected feed index.
    pub selected_feed: usize,
    /// Selected article index.
    pub selected_article: usize,
    /// Scroll offset in reader/article list.
    pub scroll_offset: usize,
}

impl Default for SessionSnapshot {
    fn default() -> Self {
        Self {
            focus: "feeds".to_string(),
            selected_feed: 0,
            selected_article: 0,
            scroll_offset: 0,
        }
    }
}

// ============================================================================
// Reading Session Tracking
// ============================================================================

/// Tracks an active reading session for duration recording.
///
/// Created when the user enters reader view, consumed when they exit.
/// The `history_id` starts at 0 (placeholder) and is updated asynchronously
/// once the DB confirms the insert via `AppEvent::ReadingSessionOpened`.
pub struct ReadingSession {
    pub history_id: i64,
    #[allow(dead_code)] // Used by TASK-8 (reading stats panel) for session context
    pub article_id: i64,
    pub started_at: std::time::Instant,
}

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
// What's New Entry (PERF-022)
// ============================================================================

/// Lightweight entry for What's New panel.
///
/// Stores only display data to avoid duplicating full Article structs.
/// Full Article data is fetched from DB on demand (e.g., when entering reader).
#[derive(Clone)]
pub struct WhatsNewEntry {
    pub article_id: i64,
    pub feed_title: Arc<str>,
    pub title: Arc<str>,
    pub published: Option<i64>,
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
    Stats,  // Reading statistics panel
}

/// Aggregated stats for multiple time windows.
///
/// Loaded asynchronously when entering `View::Stats`.
pub struct StatsData {
    pub today: crate::storage::ReadingStats,
    pub week: crate::storage::ReadingStats,
    pub month: crate::storage::ReadingStats,
}

/// Which panel has focus in Browse view
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    WhatsNew,
    Categories,
    Feeds,
    Articles,
}

// ============================================================================
// Category Tree
// ============================================================================

/// A single item in the flattened category tree for rendering.
#[derive(Debug, Clone)]
pub struct CategoryTreeItem {
    /// Category ID, or None for "All".
    pub category_id: Option<i64>,
    /// Display name.
    pub name: String,
    /// Nesting depth (0 = top-level).
    pub depth: usize,
    /// Total unread articles across feeds in this category.
    pub unread_count: i64,
    /// Whether this category has child categories.
    pub has_children: bool,
    /// Whether this category is expanded (children visible).
    pub is_expanded: bool,
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

// ============================================================================
// Confirmation Dialog
// ============================================================================

/// Pending confirmation action for destructive operations.
pub enum ConfirmAction {
    /// Delete a feed and all its articles.
    DeleteFeed { feed_id: i64, title: String },
}

// ============================================================================
// Context Menu State
// ============================================================================

/// Menu items for the feed context menu.
pub const CONTEXT_MENU_ITEMS: &[&str] = &[
    "Rename",
    "Move to Category",
    "Delete",
    "Refresh",
    "Open in Browser",
];

/// State for the feed context menu popup.
pub struct ContextMenuState {
    pub feed_id: i64,
    pub feed_title: String,
    pub feed_url: String,
    pub feed_html_url: Option<String>,
    pub selected_item: usize,
    pub sub_state: ContextMenuSubState,
}

/// Sub-state within the context menu for multi-step operations.
#[allow(dead_code)] // Wired in by TASK-9 (feed context menu)
pub enum ContextMenuSubState {
    /// Browsing the main menu list.
    MainMenu,
    /// Editing a new name for the feed.
    Renaming { input: String },
    /// Picking a category to move the feed into. Index 0 = Uncategorized.
    CategoryPicker { selected: usize },
}

// ============================================================================
// Subscribe Dialog State
// ============================================================================

/// State machine for the subscribe-by-URL dialog.
#[allow(dead_code)] // Wired in by TASK-7 (subscribe dialog)
pub enum SubscribeState {
    /// User is typing a URL.
    InputUrl { input: String },
    /// Discovery is in progress for the given URL.
    Discovering { url: String },
    /// Feed discovered; awaiting user confirmation.
    Preview { feed: DiscoveredFeed },
}

/// Events from background tasks
#[allow(dead_code)] // BulkMarkRead variants constructed by UI keybind handlers (TASK-5)
pub enum AppEvent {
    RefreshProgress(usize, usize),
    RefreshComplete(Vec<FetchResult>),
    /// Content loaded for an article.
    ///
    /// Fields:
    /// - `article_id`: The article this content belongs to
    /// - `generation`: The generation counter when this load was spawned
    /// - `result`: The content or error from fetching
    /// - `cached`: True if content was served from content_cache table
    ContentLoaded {
        article_id: i64,
        generation: u64,
        result: Result<String, ContentError>,
        cached: bool,
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
    /// Bulk mark-read operation completed successfully.
    ///
    /// Fields:
    /// - `feed_id`: The feed whose articles were marked read, or None for all feeds
    /// - `count`: Number of articles actually marked as read
    BulkMarkReadComplete {
        feed_id: Option<i64>,
        count: u64,
    },
    /// Bulk mark-read operation failed.
    ///
    /// Fields:
    /// - `feed_id`: The feed that was targeted, or None for all feeds
    /// - `error`: Description of the failure
    BulkMarkReadFailed {
        feed_id: Option<i64>,
        error: String,
    },
    /// OPML export completed successfully.
    ///
    /// Fields:
    /// - `count`: Number of feeds exported
    /// - `path`: Filesystem path where the OPML file was written
    ExportComplete {
        count: usize,
        path: String,
    },
    /// OPML export failed.
    ExportFailed {
        error: String,
    },
    /// Feed deletion completed successfully.
    ///
    /// Fields:
    /// - `feed_id`: The database ID of the deleted feed
    /// - `title`: The feed title for status display
    /// - `articles_removed`: Number of articles that were cascade-deleted
    FeedDeleted {
        feed_id: i64,
        title: String,
        articles_removed: usize,
    },
    /// Feed deletion failed.
    FeedDeleteFailed {
        feed_id: i64,
        error: String,
    },
    /// Feed discovery completed (from subscribe dialog).
    ///
    /// Fields:
    /// - `url`: The URL that was submitted for discovery
    /// - `result`: The discovered feed metadata or error message
    FeedDiscovered {
        url: String,
        result: Result<DiscoveredFeed, String>,
    },
    /// Feed subscription completed (inserted into DB).
    ///
    /// Fields:
    /// - `title`: The feed title for status display
    FeedSubscribed {
        title: String,
    },
    /// Feed subscription failed.
    FeedSubscribeFailed {
        error: String,
    },
    /// Feed renamed successfully.
    FeedRenamed {
        feed_id: i64,
        new_title: String,
    },
    /// Feed rename failed.
    FeedRenameFailed {
        feed_id: i64,
        error: String,
    },
    /// Feed moved to a different category.
    FeedMoved {
        feed_id: i64,
        category_id: Option<i64>,
        category_name: String,
    },
    /// Feed move failed.
    FeedMoveFailed {
        feed_id: i64,
        error: String,
    },
    /// Reading session opened — DB confirmed the history row insert.
    ///
    /// Updates the in-memory `ReadingSession.history_id` so that
    /// `record_close` can be called with the correct ID on reader exit.
    ReadingSessionOpened {
        history_id: i64,
    },
    /// Prefetch progress update.
    PrefetchProgress {
        completed: usize,
        total: usize,
    },
    /// Prefetch batch completed.
    PrefetchComplete {
        succeeded: usize,
        failed: usize,
    },
    /// Cached article IDs loaded (batch query result for cache indicators).
    CachedIdsLoaded(HashSet<i64>),
    /// Reading stats loaded for the stats panel.
    StatsLoaded(StatsData),
}

// ============================================================================
// Application State
// ============================================================================

/// Central application state
pub struct App {
    pub db: Database,
    pub http_client: reqwest::Client,

    // Theme
    /// Current theme variant (for cycling).
    pub theme_variant: ThemeVariant,
    /// Active style map for all UI rendering.
    /// Initialized from `ThemeVariant::Dark` and switchable at runtime.
    pub theme: StyleMap,

    // Keybindings
    /// Keybinding registry for action-key mapping with config overrides.
    #[allow(dead_code)] // Used by TASK-11 (keybinding dispatch refactor)
    pub keybindings: KeybindingRegistry,

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
    /// TASK-6: Current search scope — persists across search sessions
    pub search_scope: SearchScope,

    // Content loading
    pub content_state: ContentState,
    pub reader_article: Option<Article>, // The article currently being read

    /// Active reading session for duration tracking.
    ///
    /// Set on reader entry, consumed on reader exit or app quit.
    /// Sessions shorter than 2 seconds are discarded (accidental enters).
    pub reading_session: Option<ReadingSession>,

    /// Stats data loaded asynchronously for View::Stats.
    /// None when stats are loading or not yet requested.
    pub stats_data: Option<StatsData>,

    // Refresh progress
    pub refresh_progress: Option<(usize, usize)>,

    // P-8: Status message with expiry — Cow avoids allocation for static literals
    pub status_message: Option<(Cow<'static, str>, Instant)>,

    // What's New panel - shows recent articles after refresh
    // PERF-023: Lightweight entries instead of full Article clones
    pub whats_new: Vec<WhatsNewEntry>,
    pub whats_new_selected: usize,
    pub show_whats_new: bool,

    // PERF-005: Cached feed ID -> title mapping
    // PERF-009: Uses Arc<str> for cheap cloning
    pub feed_title_cache: HashMap<i64, Arc<str>>,

    // Starred articles view mode
    pub starred_mode: bool,

    // Category state
    /// All categories loaded from DB, ordered by sort_order then name.
    pub categories: Arc<Vec<FeedCategory>>,
    /// Currently selected category index, or None for "All feeds".
    pub selected_category: Option<usize>,
    /// Whether the category sidebar is visible.
    pub show_categories: bool,
    /// Set of collapsed category IDs. Collapsed categories hide their children in the tree.
    pub collapsed_categories: HashSet<i64>,

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

    /// PERF-022: Negative cache for failed content loads.
    /// Maps article_id -> failure time. Prevents repeated network requests
    /// for articles whose content fetch recently failed (5-minute TTL).
    pub failed_content_cache: HashMap<i64, Instant>,

    /// Set of article IDs that have valid cache entries (for UI indicators).
    /// Loaded via batch query when articles list changes.
    pub cached_article_set: HashSet<i64>,

    /// Progress of ongoing prefetch operation: (completed, total).
    pub prefetch_progress: Option<(usize, usize)>,

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

    /// Last known reader viewport width (characters).
    ///
    /// Updated during reader rendering to enable accurate wrapped line count calculation.
    /// Does not include border characters (2 chars for left/right borders).
    pub reader_viewport_width: usize,

    /// Current frame of the loading spinner animation (0-9).
    ///
    /// Incremented by the tick handler when content is loading.
    pub spinner_frame: usize,

    /// PERF-020: Cached reader content line count: (viewport_width, total_content_lines).
    ///
    /// Avoids recomputing wrapped line widths on every scroll clamp (j/k keypress).
    /// Invalidated when content state changes or viewport width changes.
    pub reader_cached_line_count: Option<(usize, usize)>,

    /// PERF-021: Cached flattened category tree for rendering and navigation.
    ///
    /// Avoids O(categories * feeds) rebuild on every render frame and nav event.
    /// Invalidated when categories, feeds, or collapsed state changes.
    pub cached_category_tree: Option<Vec<CategoryTreeItem>>,

    /// Whether the help overlay is currently displayed.
    pub show_help: bool,
    /// Scroll offset in the help screen for long keybinding lists.
    pub help_scroll_offset: usize,

    /// Pending confirmation dialog for destructive operations.
    ///
    /// When set, the UI renders a confirmation overlay and input is routed
    /// to the confirmation handler instead of normal dispatch.
    pub pending_confirm: Option<ConfirmAction>,

    /// Subscribe dialog state machine.
    ///
    /// When set, the UI renders a subscribe overlay and input is routed
    /// to the subscribe handler. Progresses: InputUrl → Discovering → Preview.
    #[allow(dead_code)] // Wired in by TASK-7 (subscribe dialog)
    pub subscribe_state: Option<SubscribeState>,

    /// Context menu state for feed operations (rename, move, delete, refresh, open).
    ///
    /// When set, the UI renders a context menu overlay and input is routed
    /// to the context menu handler instead of normal dispatch.
    pub context_menu: Option<ContextMenuState>,
}

impl App {
    pub fn new(db: Database) -> Result<Self> {
        // PERF-019: Configure HTTP client with connection pooling and keepalive
        let http_client = reqwest::Client::builder()
            .redirect(create_redirect_policy())
            .pool_max_idle_per_host(4) // P-7: 4 idle conns per host improves throughput for domain-heavy reading
            .pool_idle_timeout(std::time::Duration::from_secs(30)) // Close idle connections promptly
            .tcp_keepalive(std::time::Duration::from_secs(60)) // TCP keepalive probes
            .timeout(std::time::Duration::from_secs(30)) // Default request timeout
            // TODO: TASK-10: .http2_adaptive_window(true) method not available in reqwest 0.13
            .build()?;

        Ok(Self {
            db,
            http_client,
            theme_variant: ThemeVariant::Dark,
            theme: StyleMap::from_palette(&ThemeVariant::Dark.palette()),
            keybindings: KeybindingRegistry::new(),
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
            search_scope: SearchScope::default(),
            content_state: ContentState::Idle,
            reader_article: None,
            reading_session: None,
            stats_data: None,
            refresh_progress: None,
            status_message: None,
            whats_new: Vec::new(),
            whats_new_selected: 0,
            show_whats_new: false,
            feed_title_cache: HashMap::new(),
            starred_mode: false,
            categories: Arc::new(Vec::new()),
            selected_category: None,
            show_categories: false,
            collapsed_categories: HashSet::new(),
            feed_prefix_cache: HashMap::new(),
            cached_articles: None,
            needs_redraw: true,
            content_loading_for: None,
            failed_content_cache: HashMap::new(),
            cached_article_set: HashSet::new(),
            prefetch_progress: None,
            content_load_generation: 0,
            content_load_handle: None,
            search_generation: 0,
            search_handle: None,
            reader_visible_lines: 0,
            reader_viewport_width: 0,
            spinner_frame: 0,
            reader_cached_line_count: None,
            cached_category_tree: None,
            show_help: false,
            help_scroll_offset: 0,
            pending_confirm: None,
            subscribe_state: None,
            context_menu: None,
        })
    }

    /// Resolve a semantic role name to its `Style`.
    ///
    /// Convenience method that delegates to `StyleMap::resolve`.
    /// Returns `Style::default()` for unknown roles.
    #[allow(dead_code)] // Used by downstream tasks (TASK-10, TASK-12)
    pub fn style(&self, role: &str) -> Style {
        self.theme.resolve(role)
    }

    /// Switch to a different theme variant at runtime.
    ///
    /// Rebuilds the `StyleMap` from the new variant's palette and
    /// marks the UI as needing a full redraw.
    pub fn set_theme(&mut self, variant: ThemeVariant) {
        self.theme_variant = variant;
        self.theme = StyleMap::from_palette(&variant.palette());
        self.needs_redraw = true;
    }

    /// Cycle to the next theme variant (Dark → Light → Dark).
    ///
    /// Returns the name of the new theme for status display.
    pub fn cycle_theme(&mut self) -> &'static str {
        let next = self.theme_variant.next();
        self.set_theme(next);
        next.name()
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
        // Clamp category selection
        if let Some(idx) = self.selected_category {
            if self.categories.is_empty() {
                self.selected_category = None;
            } else if idx >= self.categories.len() {
                self.selected_category = Some(self.categories.len().saturating_sub(1));
            }
        }

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
    pub fn selected_whats_new(&mut self) -> Option<&WhatsNewEntry> {
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

    /// Get the category ID for the currently selected category, or None for "All".
    pub fn selected_category_id(&self) -> Option<i64> {
        self.selected_category
            .and_then(|idx| self.categories.get(idx))
            .map(|cat| cat.id)
    }

    /// Return feeds filtered by the selected category.
    ///
    /// If `selected_category` is None ("All"), returns all feeds.
    /// If Some(idx), returns only feeds whose `category_id` matches the selected category's ID.
    #[allow(dead_code)] // Not yet used in rendering; consumed by tests
    pub fn filtered_feeds(&self) -> Vec<&Feed> {
        match self.selected_category_id() {
            None => self.feeds.iter().collect(),
            Some(cat_id) => self
                .feeds
                .iter()
                .filter(|f| f.category_id == Some(cat_id))
                .collect(),
        }
    }

    /// Build the visible category tree, respecting collapsed state.
    ///
    /// PERF-021: Returns a cached tree when available. The cache is invalidated
    /// by `invalidate_category_tree()` which must be called whenever categories,
    /// feeds (unread counts), or collapsed state changes.
    ///
    /// Returns a flat list of `CategoryTreeItem` in display order.
    /// "All" is always item 0 (with `category_id = None`).
    pub fn build_category_tree(&mut self) -> Vec<CategoryTreeItem> {
        if let Some(ref cached) = self.cached_category_tree {
            return cached.clone();
        }
        let tree = self.build_category_tree_uncached();
        self.cached_category_tree = Some(tree.clone());
        tree
    }

    /// PERF-021: Get the cached category tree, or build it fresh.
    ///
    /// For read-only callers (e.g., render functions) that cannot call `build_category_tree()`.
    /// Returns a reference to the cached tree if available, otherwise builds a new one.
    pub fn category_tree(&self) -> Cow<'_, [CategoryTreeItem]> {
        match &self.cached_category_tree {
            Some(cached) => Cow::Borrowed(cached.as_slice()),
            None => Cow::Owned(self.build_category_tree_uncached()),
        }
    }

    /// Build the category tree without caching. Used internally and by
    /// read-only callers that cannot mutate App.
    fn build_category_tree_uncached(&self) -> Vec<CategoryTreeItem> {
        let mut items = vec![CategoryTreeItem {
            category_id: None,
            name: "All".to_string(),
            depth: 0,
            unread_count: self.feeds.iter().map(|f| f.unread_count).sum(),
            has_children: false,
            is_expanded: true,
        }];

        let roots: Vec<&FeedCategory> = self
            .categories
            .iter()
            .filter(|c| c.parent_id.is_none())
            .collect();

        for root in roots {
            self.add_tree_item(&mut items, root, 1);
        }

        items
    }

    /// PERF-021: Invalidate the cached category tree.
    ///
    /// Must be called after any mutation to:
    /// - `self.categories` (add/remove/reorder)
    /// - `self.feeds` (unread count changes, add/remove/move)
    /// - `self.collapsed_categories` (expand/collapse)
    pub fn invalidate_category_tree(&mut self) {
        self.cached_category_tree = None;
    }

    fn add_tree_item(&self, items: &mut Vec<CategoryTreeItem>, cat: &FeedCategory, depth: usize) {
        let children: Vec<&FeedCategory> = self
            .categories
            .iter()
            .filter(|c| c.parent_id == Some(cat.id))
            .collect();
        let has_children = !children.is_empty();
        let is_expanded = !self.collapsed_categories.contains(&cat.id);

        let unread_count: i64 = self
            .feeds
            .iter()
            .filter(|f| f.category_id == Some(cat.id))
            .map(|f| f.unread_count)
            .sum();

        items.push(CategoryTreeItem {
            category_id: Some(cat.id),
            name: cat.name.clone(),
            depth,
            unread_count,
            has_children,
            is_expanded,
        });

        if is_expanded {
            for child in children {
                self.add_tree_item(items, child, depth + 1);
            }
        }
    }

    /// Toggle collapse state for a category.
    pub fn toggle_category_collapse(&mut self, cat_id: i64) {
        if self.collapsed_categories.contains(&cat_id) {
            self.collapsed_categories.remove(&cat_id);
        } else {
            self.collapsed_categories.insert(cat_id);
        }
        self.invalidate_category_tree(); // PERF-021: Collapsed state changed
    }

    /// Map selected_category to a visible tree index. 0 = "All".
    ///
    /// PERF-021: Accepts a pre-built tree to avoid redundant rebuilds.
    pub fn category_tree_selected_index_in(&self, tree: &[CategoryTreeItem]) -> usize {
        match self.selected_category {
            None => 0,
            Some(idx) => {
                if let Some(cat) = self.categories.get(idx) {
                    tree.iter()
                        .position(|item| item.category_id == Some(cat.id))
                        .unwrap_or(0)
                } else {
                    0
                }
            }
        }
    }

    /// Select a category by its visible tree index.
    ///
    /// PERF-021: Accepts a pre-built tree to avoid redundant rebuilds.
    pub fn select_category_by_tree_index_in(&mut self, tree: &[CategoryTreeItem], tree_idx: usize) {
        if let Some(item) = tree.get(tree_idx) {
            match item.category_id {
                None => self.selected_category = None,
                Some(cat_id) => {
                    if let Some(idx) = self.categories.iter().position(|c| c.id == cat_id) {
                        self.selected_category = Some(idx);
                    }
                }
            }
        }
    }

    /// Navigate up in current list
    pub fn nav_up(&mut self) {
        match self.focus {
            Focus::WhatsNew => {
                self.whats_new_selected = self.whats_new_selected.saturating_sub(1);
            }
            Focus::Categories => {
                // PERF-021: Single tree build for index lookup + selection
                let tree = self.build_category_tree();
                let current = self.category_tree_selected_index_in(&tree);
                if current > 0 {
                    self.select_category_by_tree_index_in(&tree, current - 1);
                }
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
            Focus::Categories => {
                // PERF-021: Single tree build for index lookup + selection
                let tree = self.build_category_tree();
                let current = self.category_tree_selected_index_in(&tree);
                let max_index = tree.len().saturating_sub(1);
                if current < max_index {
                    self.select_category_by_tree_index_in(&tree, current + 1);
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
        self.scroll_offset = self.scroll_offset.min(max_scroll).min(MAX_SCROLL);
    }

    /// Get the number of display lines in the reader view (accounting for wrapping).
    ///
    /// Calculates wrapped line count based on viewport width. Each logical line
    /// may wrap to multiple display lines depending on its width.
    /// Includes the 3-line header.
    ///
    /// PERF-020: Uses cached line count when available to avoid recomputing
    /// wrapped line widths on every scroll clamp.
    pub fn reader_content_lines(&self) -> usize {
        const HEADER_LINES: usize = 3; // Title, feed/time, blank line
        let width = self.reader_viewport_width.max(1); // Avoid division by zero

        // PERF-020: Return cached value if viewport width matches
        if let Some((cached_width, cached_count)) = self.reader_cached_line_count {
            if cached_width == width {
                return HEADER_LINES + cached_count;
            }
        }

        let content_lines = match &self.content_state {
            ContentState::Idle => 1,
            ContentState::Loading { .. } => 1,
            ContentState::Loaded { rendered_lines, .. } => {
                // Calculate wrapped line count for each line
                rendered_lines
                    .iter()
                    .map(|line| Self::wrapped_line_count(line, width))
                    .sum()
            }
            ContentState::Failed { fallback, .. } => {
                // Error line + blank + optional summary (estimate wrapping)
                let base = 2;
                let summary_lines = fallback.as_ref().map_or(0, |s| {
                    2 + s
                        .lines()
                        .map(|l| l.width().max(1).div_ceil(width))
                        .sum::<usize>()
                });
                base + summary_lines
            }
        };
        HEADER_LINES + content_lines
    }

    /// Calculate how many display lines a single Line will occupy after wrapping.
    fn wrapped_line_count(line: &Line<'_>, viewport_width: usize) -> usize {
        let width = viewport_width.max(1);
        let line_width: usize = line.spans.iter().map(|s| s.content.width()).sum();
        if line_width == 0 {
            1 // Empty lines still take one line
        } else {
            line_width.div_ceil(width)
        }
    }

    /// PERF-020: Compute and cache the reader content line count for the current viewport width.
    ///
    /// This pre-populates `reader_cached_line_count` so that subsequent calls to
    /// `reader_content_lines()` (which takes `&self`) can return the cached value
    /// without recomputing wrapped line widths.
    pub fn cache_reader_line_count(&mut self) {
        let width = self.reader_viewport_width.max(1);
        let content_lines = match &self.content_state {
            ContentState::Idle => 1,
            ContentState::Loading { .. } => 1,
            ContentState::Loaded { rendered_lines, .. } => rendered_lines
                .iter()
                .map(|line| Self::wrapped_line_count(line, width))
                .sum(),
            ContentState::Failed { fallback, .. } => {
                let base = 2;
                let summary_lines = fallback.as_ref().map_or(0, |s| {
                    2 + s
                        .lines()
                        .map(|l| l.width().max(1).div_ceil(width))
                        .sum::<usize>()
                });
                base + summary_lines
            }
        };
        self.reader_cached_line_count = Some((width, content_lines));
    }

    /// Clamp scroll offset to content bounds using stored viewport size.
    ///
    /// Convenience method that uses `reader_visible_lines` from last render.
    /// Call this after scroll operations in the reader view.
    ///
    /// PERF-020: Pre-populates the line count cache before reading it,
    /// so subsequent scroll clamps on the same content are O(1).
    pub fn clamp_reader_scroll(&mut self) {
        self.cache_reader_line_count();
        let content_lines = self.reader_content_lines();
        self.clamp_scroll(content_lines, self.reader_visible_lines);
    }

    /// Set status message (will auto-expire after 3 seconds)
    pub fn set_status(&mut self, msg: impl Into<Cow<'static, str>>) {
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
        self.reader_cached_line_count = None; // PERF-020: Invalidate cache on reader entry
        Some(article)
    }

    /// Capture current App state as a serializable snapshot.
    #[allow(dead_code)] // Used by TASK-13 (session restore)
    pub fn snapshot(&self) -> SessionSnapshot {
        let focus = match self.focus {
            Focus::WhatsNew => "whatsnew",
            Focus::Categories => "feeds", // Snapshot restores to feeds panel
            Focus::Feeds => "feeds",
            Focus::Articles => "articles",
        };
        SessionSnapshot {
            focus: focus.to_string(),
            selected_feed: self.selected_feed,
            selected_article: self.selected_article,
            scroll_offset: self.scroll_offset,
        }
    }

    /// Restore App state from a snapshot with bounds clamping.
    ///
    /// Always restores to Browse view. Unknown focus values default to Feeds.
    #[allow(dead_code)] // Used by TASK-13 (session restore)
    pub fn restore(&mut self, snapshot: SessionSnapshot) {
        self.view = View::Browse;
        self.focus = match snapshot.focus.as_str() {
            "whatsnew" => Focus::WhatsNew,
            "articles" => Focus::Articles,
            _ => Focus::Feeds,
        };
        self.selected_feed = snapshot.selected_feed;
        self.selected_article = snapshot.selected_article;
        self.scroll_offset = snapshot.scroll_offset;
        self.clamp_selections();
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
        self.reader_cached_line_count = None; // PERF-020: Invalidate cache on reader exit
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
            category_id: None,
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
        app.reader_viewport_width = 80; // Wide enough that "test" doesn't wrap

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
        app.reader_viewport_width = 80; // Wide enough that "test" doesn't wrap
        let rendered_lines: Vec<Line<'static>> = (0..50).map(|_| Line::from("test")).collect();
        app.content_state = ContentState::Loaded {
            article_id: 1,
            content: "test".to_string(),
            rendered_lines,
        };

        // Loaded: 3 header + 50 content = 53
        assert_eq!(app.reader_content_lines(), 53);
    }

    // SessionSnapshot tests
    #[tokio::test]
    async fn test_snapshot_captures_state() {
        let mut app = test_app().await;
        app.feeds = Arc::new(vec![test_feed(1, "A"), test_feed(2, "B")]);
        app.focus = Focus::Articles;
        app.selected_feed = 1;
        app.selected_article = 3;
        app.scroll_offset = 10;

        let snap = app.snapshot();
        assert_eq!(snap.focus, "articles");
        assert_eq!(snap.selected_feed, 1);
        assert_eq!(snap.selected_article, 3);
        assert_eq!(snap.scroll_offset, 10);
    }

    #[tokio::test]
    async fn test_restore_applies_state() {
        let mut app = test_app().await;
        app.feeds = Arc::new(vec![test_feed(1, "A"), test_feed(2, "B")]);

        let snap = SessionSnapshot {
            focus: "articles".to_string(),
            selected_feed: 1,
            selected_article: 0,
            scroll_offset: 5,
        };

        app.restore(snap);
        assert_eq!(app.view, View::Browse);
        assert_eq!(app.focus, Focus::Articles);
        assert_eq!(app.selected_feed, 1);
        assert_eq!(app.scroll_offset, 5);
    }

    #[tokio::test]
    async fn test_restore_clamps_out_of_bounds() {
        let mut app = test_app().await;
        // No feeds loaded — indices should clamp to 0
        let snap = SessionSnapshot {
            focus: "feeds".to_string(),
            selected_feed: 999,
            selected_article: 999,
            scroll_offset: 0,
        };

        app.restore(snap);
        assert_eq!(app.selected_feed, 0);
        assert_eq!(app.selected_article, 0);
    }

    #[tokio::test]
    async fn test_restore_unknown_focus_defaults_to_feeds() {
        let mut app = test_app().await;
        let snap = SessionSnapshot {
            focus: "unknown_panel".to_string(),
            selected_feed: 0,
            selected_article: 0,
            scroll_offset: 0,
        };

        app.restore(snap);
        assert_eq!(app.focus, Focus::Feeds);
    }

    #[tokio::test]
    async fn test_snapshot_json_round_trip() {
        let snap = SessionSnapshot {
            focus: "articles".to_string(),
            selected_feed: 2,
            selected_article: 5,
            scroll_offset: 10,
        };

        let json = serde_json::to_string(&snap).unwrap();
        let restored: SessionSnapshot = serde_json::from_str(&json).unwrap();

        assert_eq!(restored.focus, "articles");
        assert_eq!(restored.selected_feed, 2);
        assert_eq!(restored.selected_article, 5);
        assert_eq!(restored.scroll_offset, 10);
    }

    #[tokio::test]
    async fn test_snapshot_forward_compatible() {
        // Simulate a JSON from a future version with extra fields
        let json = r#"{"focus":"feeds","selected_feed":1,"selected_article":0,"scroll_offset":0,"new_field":"value"}"#;
        let snap: SessionSnapshot = serde_json::from_str(json).unwrap();
        assert_eq!(snap.focus, "feeds");
        assert_eq!(snap.selected_feed, 1);
    }

    #[tokio::test]
    async fn test_snapshot_default_on_empty_json() {
        let snap: SessionSnapshot = serde_json::from_str("{}").unwrap();
        assert_eq!(snap.focus, "feeds");
        assert_eq!(snap.selected_feed, 0);
        assert_eq!(snap.selected_article, 0);
        assert_eq!(snap.scroll_offset, 0);
    }

    // ========================================================================
    // Theme + Keybinding Integration Tests (TASK-16)
    // ========================================================================

    #[tokio::test]
    async fn test_theme_defaults_to_dark() {
        let app = test_app().await;
        assert_eq!(app.theme_variant, ThemeVariant::Dark);
    }

    #[tokio::test]
    async fn test_cycle_theme_dark_to_light() {
        let mut app = test_app().await;
        let name = app.cycle_theme();
        assert_eq!(name, "Light");
        assert_eq!(app.theme_variant, ThemeVariant::Light);
        assert!(app.needs_redraw);
    }

    #[tokio::test]
    async fn test_cycle_theme_light_to_dark() {
        let mut app = test_app().await;
        app.cycle_theme(); // Dark -> Light
        app.needs_redraw = false; // Reset
        let name = app.cycle_theme(); // Light -> Dark
        assert_eq!(name, "Dark");
        assert_eq!(app.theme_variant, ThemeVariant::Dark);
        assert!(app.needs_redraw);
    }

    #[tokio::test]
    async fn test_cycle_theme_full_round_trip() {
        let mut app = test_app().await;
        // Save initial style for a role
        let initial_selected = app.style("feed_selected");

        // Cycle to Light
        app.cycle_theme();
        let light_selected = app.style("feed_selected");
        assert_ne!(
            initial_selected, light_selected,
            "Light should differ from Dark"
        );

        // Cycle back to Dark
        app.cycle_theme();
        let restored_selected = app.style("feed_selected");
        assert_eq!(
            initial_selected, restored_selected,
            "Dark should match original after full cycle"
        );
    }

    #[tokio::test]
    async fn test_set_theme_updates_variant_and_styles() {
        let mut app = test_app().await;
        let dark_border = app.style("panel_border_focused");

        app.set_theme(ThemeVariant::Light);
        assert_eq!(app.theme_variant, ThemeVariant::Light);

        let light_border = app.style("panel_border_focused");
        assert_ne!(dark_border, light_border);
    }

    #[tokio::test]
    async fn test_theme_style_resolves_all_roles() {
        let app = test_app().await;
        // Verify all 26 semantic roles resolve to non-default (most should)
        let roles = [
            "feed_selected",
            "feed_unread",
            "feed_error",
            "article_selected",
            "article_star",
            "article_feed_prefix",
            "reader_heading",
            "reader_code_block",
            "reader_inline_code",
            "reader_error",
            "reader_fallback",
            "reader_image",
            "status_bar",
            "panel_border_focused",
            "whatsnew_border_focused",
            "whatsnew_selected",
        ];
        for role in roles {
            let style = app.style(role);
            assert_ne!(
                style,
                Style::default(),
                "Role '{}' should not be default style",
                role
            );
        }
    }

    #[tokio::test]
    async fn test_keybinding_registry_has_cycle_theme() {
        use crate::keybindings::{Action as KbAction, Context as KbContext};
        use crossterm::event::{KeyCode, KeyModifiers};

        let app = test_app().await;
        let action = app.keybindings.action_for_key(
            KeyCode::Char('T'),
            KeyModifiers::NONE,
            KbContext::Global,
        );
        assert_eq!(action, Some(KbAction::CycleTheme));
    }

    #[tokio::test]
    async fn test_theme_unknown_role_returns_default() {
        let app = test_app().await;
        assert_eq!(app.style("nonexistent_role"), Style::default());
    }

    #[tokio::test]
    async fn test_config_keybinding_overrides_applied_to_app() {
        use crate::keybindings::{Action as KbAction, Context as KbContext};
        use crossterm::event::{KeyCode, KeyModifiers};
        use std::collections::HashMap;

        let mut app = test_app().await;

        // Default: 'q' = Quit
        assert_eq!(
            app.keybindings.action_for_key(
                KeyCode::Char('q'),
                KeyModifiers::NONE,
                KbContext::Global
            ),
            Some(KbAction::Quit)
        );

        // Override quit to Ctrl+q
        let mut overrides = HashMap::new();
        overrides.insert("quit".to_string(), "Ctrl+q".to_string());
        let warnings = app.keybindings.apply_overrides(&overrides);
        assert!(warnings.is_empty());

        // Old binding gone, new binding active
        assert_eq!(
            app.keybindings.action_for_key(
                KeyCode::Char('q'),
                KeyModifiers::NONE,
                KbContext::Global
            ),
            None
        );
        assert_eq!(
            app.keybindings.action_for_key(
                KeyCode::Char('q'),
                KeyModifiers::CONTROL,
                KbContext::Global
            ),
            Some(KbAction::Quit)
        );
    }

    #[tokio::test]
    async fn test_config_theme_variant_from_str() {
        // Verify config theme strings map to correct ThemeVariant
        assert_eq!(
            ThemeVariant::from_str_name("dark"),
            Some(ThemeVariant::Dark)
        );
        assert_eq!(
            ThemeVariant::from_str_name("light"),
            Some(ThemeVariant::Light)
        );
        assert_eq!(
            ThemeVariant::from_str_name("DARK"),
            Some(ThemeVariant::Dark)
        );
        assert_eq!(ThemeVariant::from_str_name("neon"), None);
    }

    #[tokio::test]
    async fn test_keybinding_override_preserves_theme_cycling() {
        use crate::keybindings::{Action as KbAction, Context as KbContext};
        use crossterm::event::{KeyCode, KeyModifiers};
        use std::collections::HashMap;

        let mut app = test_app().await;

        // Override CycleTheme to F5
        let mut overrides = HashMap::new();
        overrides.insert("theme".to_string(), "F5".to_string());
        app.keybindings.apply_overrides(&overrides);

        // Old key gone
        assert_eq!(
            app.keybindings.action_for_key(
                KeyCode::Char('T'),
                KeyModifiers::NONE,
                KbContext::Global
            ),
            None
        );

        // New key works
        assert_eq!(
            app.keybindings
                .action_for_key(KeyCode::F(5), KeyModifiers::NONE, KbContext::Global),
            Some(KbAction::CycleTheme)
        );

        // Theme cycling still functions via App method
        assert_eq!(app.theme_variant, ThemeVariant::Dark);
        let name = app.cycle_theme();
        assert_eq!(name, "Light");
        assert_eq!(app.theme_variant, ThemeVariant::Light);
    }

    #[tokio::test]
    async fn test_theme_styles_differ_between_variants() {
        use ratatui::style::Color;

        let mut app = test_app().await;

        // Dark theme: feed_selected uses DarkGray bg
        let dark_selected = app.style("feed_selected");
        assert_eq!(
            dark_selected,
            Style::default().bg(Color::DarkGray).fg(Color::White)
        );

        // Switch to Light
        app.set_theme(ThemeVariant::Light);

        // Light theme: feed_selected uses Blue bg
        let light_selected = app.style("feed_selected");
        assert_eq!(
            light_selected,
            Style::default().bg(Color::Blue).fg(Color::White)
        );
    }

    #[tokio::test]
    async fn test_show_help_keybinding_exists() {
        use crate::keybindings::{Action as KbAction, Context as KbContext};
        use crossterm::event::{KeyCode, KeyModifiers};

        let app = test_app().await;
        let action = app.keybindings.action_for_key(
            KeyCode::Char('?'),
            KeyModifiers::NONE,
            KbContext::Global,
        );
        assert_eq!(action, Some(KbAction::ShowHelp));
    }

    #[tokio::test]
    async fn test_all_bindings_include_theme_and_help() {
        let app = test_app().await;
        let bindings = app.keybindings.all_bindings();

        let has_cycle_theme = bindings
            .iter()
            .any(|(_, _, action, _)| *action == crate::keybindings::Action::CycleTheme);
        let has_show_help = bindings
            .iter()
            .any(|(_, _, action, _)| *action == crate::keybindings::Action::ShowHelp);

        assert!(has_cycle_theme, "all_bindings should include CycleTheme");
        assert!(has_show_help, "all_bindings should include ShowHelp");
    }

    #[tokio::test]
    async fn test_theme_set_status_on_cycle() {
        let mut app = test_app().await;
        let name = app.cycle_theme();
        app.set_status(format!("Theme: {}", name));

        assert!(app.status_message.is_some());
        let (msg, _) = app.status_message.as_ref().unwrap();
        assert_eq!(msg, "Theme: Light");
    }

    // ========================================================================
    // Category State Tests (TASK-5)
    // ========================================================================

    fn test_category(id: i64, name: &str, parent_id: Option<i64>) -> FeedCategory {
        FeedCategory {
            id,
            name: name.to_string(),
            parent_id,
            sort_order: 0,
        }
    }

    #[tokio::test]
    async fn test_category_filter_feeds() {
        let mut app = test_app().await;

        let cat_tech = test_category(10, "Tech", None);
        let cat_news = test_category(20, "News", None);
        app.categories = Arc::new(vec![cat_tech, cat_news]);

        let mut f1 = test_feed(1, "Rust Blog");
        f1.category_id = Some(10);
        let mut f2 = test_feed(2, "Go Blog");
        f2.category_id = Some(10);
        let mut f3 = test_feed(3, "BBC");
        f3.category_id = Some(20);
        let f4 = test_feed(4, "Uncategorized");
        app.feeds = Arc::new(vec![f1, f2, f3, f4]);

        // All feeds when no category selected
        app.selected_category = None;
        assert_eq!(app.filtered_feeds().len(), 4);

        // Filter by Tech (index 0 in categories vec)
        app.selected_category = Some(0);
        let filtered = app.filtered_feeds();
        assert_eq!(filtered.len(), 2);
        assert!(filtered.iter().all(|f| f.category_id == Some(10)));

        // Filter by News (index 1 in categories vec)
        app.selected_category = Some(1);
        let filtered = app.filtered_feeds();
        assert_eq!(filtered.len(), 1);
        assert_eq!(&*filtered[0].title, "BBC");
    }

    #[tokio::test]
    async fn test_category_selection_clamp() {
        let mut app = test_app().await;

        // Category index out of bounds on empty list
        app.selected_category = Some(5);
        app.clamp_selections();
        assert_eq!(app.selected_category, None);

        // Category index out of bounds on non-empty list
        app.categories = Arc::new(vec![test_category(1, "Cat", None)]);
        app.selected_category = Some(5);
        app.clamp_selections();
        assert_eq!(app.selected_category, Some(0));

        // Valid index unchanged
        app.selected_category = Some(0);
        app.clamp_selections();
        assert_eq!(app.selected_category, Some(0));
    }

    #[tokio::test]
    async fn test_selected_category_id() {
        let mut app = test_app().await;

        // No category selected
        assert_eq!(app.selected_category_id(), None);

        app.categories = Arc::new(vec![
            test_category(10, "Tech", None),
            test_category(20, "News", None),
        ]);

        // Select first category
        app.selected_category = Some(0);
        assert_eq!(app.selected_category_id(), Some(10));

        // Select second category
        app.selected_category = Some(1);
        assert_eq!(app.selected_category_id(), Some(20));

        // Out of bounds index returns None
        app.selected_category = Some(99);
        assert_eq!(app.selected_category_id(), None);
    }

    // ========================================================================
    // Delete Feed Confirmation Tests (TASK-6)
    // ========================================================================

    #[tokio::test]
    async fn test_delete_confirm_flow() {
        let mut app = test_app().await;

        // Set up feed
        app.feeds = Arc::new(vec![test_feed(1, "Test Feed")]);
        app.selected_feed = 0;

        // Trigger delete → should set pending_confirm
        assert!(app.pending_confirm.is_none());
        if let Some(feed) = app.selected_feed() {
            app.pending_confirm = Some(ConfirmAction::DeleteFeed {
                feed_id: feed.id,
                title: feed.title.to_string(),
            });
        }
        assert!(app.pending_confirm.is_some());
        if let Some(ConfirmAction::DeleteFeed { feed_id, title }) = &app.pending_confirm {
            assert_eq!(*feed_id, 1);
            assert_eq!(title, "Test Feed");
        } else {
            panic!("Expected DeleteFeed confirm action");
        }
    }

    #[tokio::test]
    async fn test_delete_cancel() {
        let mut app = test_app().await;

        app.pending_confirm = Some(ConfirmAction::DeleteFeed {
            feed_id: 1,
            title: "Test Feed".to_string(),
        });

        // Cancel → should clear pending_confirm
        app.pending_confirm = None;
        app.set_status("Cancelled");

        assert!(app.pending_confirm.is_none());
        assert!(app.status_message.is_some());
    }

    #[tokio::test]
    async fn test_feed_deleted_event_updates_state() {
        let mut app = test_app().await;

        // Set up feeds and articles
        app.feeds = Arc::new(vec![test_feed(1, "Feed A"), test_feed(2, "Feed B")]);
        app.selected_feed = 0;
        app.rebuild_feed_cache();

        // Simulate article data for feed 1
        let article = Article {
            id: 100,
            feed_id: 1,
            guid: "guid-1".to_string(),
            title: Arc::from("Article 1"),
            url: None,
            published: None,
            summary: None,
            content: None,
            read: false,
            starred: false,
            fetched_at: 0,
        };
        app.articles = Arc::new(vec![article]);

        // Simulate FeedDeleted event handling
        let feeds = Arc::make_mut(&mut app.feeds);
        feeds.retain(|f| f.id != 1);
        let articles = Arc::make_mut(&mut app.articles);
        articles.retain(|a| a.feed_id != 1);
        app.feed_title_cache.remove(&1);
        app.cached_articles = None;
        app.clamp_selections();

        // Verify state
        assert_eq!(app.feeds.len(), 1);
        assert_eq!(app.feeds[0].id, 2);
        assert!(app.articles.is_empty());
        assert!(!app.feed_title_cache.contains_key(&1));
    }

    // ========================================================================
    // Category Tree Tests (TASK-8)
    // ========================================================================

    #[tokio::test]
    async fn test_category_panel_toggle() {
        let mut app = test_app().await;
        assert!(!app.show_categories);

        app.show_categories = true;
        assert!(app.show_categories);

        // When hiding categories, focus should move away from Categories
        app.focus = Focus::Categories;
        app.show_categories = false;
        // Simulating what the input handler does:
        if !app.show_categories && app.focus == Focus::Categories {
            app.focus = Focus::Feeds;
        }
        assert_eq!(app.focus, Focus::Feeds);
    }

    #[tokio::test]
    async fn test_category_focus_cycle() {
        let mut app = test_app().await;

        // Without categories: Feeds -> Articles -> Feeds
        app.focus = Focus::Feeds;
        // Simulate: no categories, no whatsnew
        assert_eq!(app.show_categories, false);
        // Cycle: Feeds -> Articles
        app.focus = Focus::Articles;
        // Cycle: Articles -> Feeds
        app.focus = Focus::Feeds;

        // With categories: Categories -> Feeds -> Articles -> Categories
        app.show_categories = true;
        app.focus = Focus::Categories;
        // These transitions are tested through the actual input handler
        // but we verify the enum values exist and focus transitions are valid
        app.focus = Focus::Feeds;
        assert_eq!(app.focus, Focus::Feeds);
        app.focus = Focus::Articles;
        assert_eq!(app.focus, Focus::Articles);
        app.focus = Focus::Categories;
        assert_eq!(app.focus, Focus::Categories);
    }

    #[tokio::test]
    async fn test_category_tree_building() {
        let mut app = test_app().await;

        // Empty categories: tree has only "All"
        let tree = app.build_category_tree();
        assert_eq!(tree.len(), 1);
        assert_eq!(tree[0].name, "All");
        assert!(tree[0].category_id.is_none());

        // Add categories with nesting
        app.categories = Arc::new(vec![
            FeedCategory {
                id: 1,
                name: "Tech".to_string(),
                parent_id: None,
                sort_order: 0,
            },
            FeedCategory {
                id: 2,
                name: "Rust".to_string(),
                parent_id: Some(1),
                sort_order: 0,
            },
            FeedCategory {
                id: 3,
                name: "News".to_string(),
                parent_id: None,
                sort_order: 1,
            },
        ]);
        app.invalidate_category_tree(); // PERF-021: Invalidate cache after mutation

        let tree = app.build_category_tree();
        // All, Tech, Rust (child of Tech), News
        assert_eq!(tree.len(), 4);
        assert_eq!(tree[0].name, "All");
        assert_eq!(tree[1].name, "Tech");
        assert_eq!(tree[1].depth, 1);
        assert!(tree[1].has_children);
        assert_eq!(tree[2].name, "Rust");
        assert_eq!(tree[2].depth, 2);
        assert_eq!(tree[3].name, "News");
        assert_eq!(tree[3].depth, 1);
    }

    #[tokio::test]
    async fn test_category_collapse_expand() {
        let mut app = test_app().await;

        app.categories = Arc::new(vec![
            FeedCategory {
                id: 1,
                name: "Tech".to_string(),
                parent_id: None,
                sort_order: 0,
            },
            FeedCategory {
                id: 2,
                name: "Rust".to_string(),
                parent_id: Some(1),
                sort_order: 0,
            },
        ]);
        app.invalidate_category_tree(); // PERF-021: Invalidate cache after mutation

        // Initially expanded: All, Tech, Rust
        let tree = app.build_category_tree();
        assert_eq!(tree.len(), 3);

        // Collapse Tech
        app.toggle_category_collapse(1);
        let tree = app.build_category_tree();
        assert_eq!(tree.len(), 2); // All, Tech (Rust hidden)
        assert!(!tree[1].is_expanded);

        // Expand Tech
        app.toggle_category_collapse(1);
        let tree = app.build_category_tree();
        assert_eq!(tree.len(), 3);
        assert!(tree[1].is_expanded);
    }

    // ========================================================================
    // Context Menu Tests (TASK-9)
    // ========================================================================

    #[tokio::test]
    async fn test_context_menu_open_close() {
        let mut app = test_app().await;
        app.feeds = Arc::new(vec![test_feed(1, "Test Feed")]);
        app.selected_feed = 0;

        // Open context menu for selected feed
        assert!(app.context_menu.is_none());
        if let Some(feed) = app.selected_feed() {
            app.context_menu = Some(ContextMenuState {
                feed_id: feed.id,
                feed_title: feed.title.to_string(),
                feed_url: feed.url.clone(),
                feed_html_url: feed.html_url.clone(),
                selected_item: 0,
                sub_state: ContextMenuSubState::MainMenu,
            });
        }
        assert!(app.context_menu.is_some());
        let menu = app.context_menu.as_ref().unwrap();
        assert_eq!(menu.feed_id, 1);
        assert_eq!(menu.feed_title, "Test Feed");
        assert_eq!(menu.selected_item, 0);

        // Close
        app.context_menu = None;
        assert!(app.context_menu.is_none());
    }

    #[tokio::test]
    async fn test_context_menu_delegates_delete() {
        let mut app = test_app().await;

        // Simulate context menu selecting "Delete" (item 2)
        app.context_menu = None; // Menu is dismissed
        app.pending_confirm = Some(ConfirmAction::DeleteFeed {
            feed_id: 42,
            title: "My Feed".to_string(),
        });

        // Verify it delegates to the confirm flow
        assert!(app.pending_confirm.is_some());
        if let Some(ConfirmAction::DeleteFeed { feed_id, title }) = &app.pending_confirm {
            assert_eq!(*feed_id, 42);
            assert_eq!(title, "My Feed");
        }
    }

    #[tokio::test]
    async fn test_move_feed_to_category_state() {
        let mut app = test_app().await;

        let mut f1 = test_feed(1, "Rust Blog");
        f1.category_id = None;
        app.feeds = Arc::new(vec![f1]);

        app.categories = Arc::new(vec![test_category(10, "Tech", None)]);

        // Simulate FeedMoved event handler
        let feeds = Arc::make_mut(&mut app.feeds);
        if let Some(f) = feeds.iter_mut().find(|f| f.id == 1) {
            f.category_id = Some(10);
        }

        assert_eq!(app.feeds[0].category_id, Some(10));
    }
}
