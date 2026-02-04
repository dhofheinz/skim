use crate::content::ContentError;
use crate::storage::{Article, Database, Feed};
use std::time::Instant;

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
    },
    Failed {
        article_id: i64,
        error: String,
        fallback: Option<String>,
    },
}

/// Result from fetching a single feed
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct FetchResult {
    pub feed_id: i64,
    pub new_articles: usize,
    pub error: Option<String>,
}

/// Events from background tasks
pub enum AppEvent {
    RefreshProgress(usize, usize),
    RefreshComplete(Vec<FetchResult>),
    ContentLoaded(i64, Result<String, ContentError>),
}

// ============================================================================
// Application State
// ============================================================================

/// Central application state
pub struct App {
    pub db: Database,
    pub http_client: reqwest::Client,

    // Data
    pub feeds: Vec<Feed>,
    pub articles: Vec<Article>,

    // UI State
    pub view: View,
    pub focus: Focus,
    pub selected_feed: usize,
    pub selected_article: usize,
    pub scroll_offset: usize,

    // Search
    pub search_mode: bool,
    pub search_input: String,

    // Content loading
    pub content_state: ContentState,
    pub reader_article: Option<Article>, // The article currently being read

    // Refresh progress
    pub refresh_progress: Option<(usize, usize)>,

    // Status message with expiry
    pub status_message: Option<(String, Instant)>,

    // What's New panel - shows recent articles after refresh
    pub whats_new: Vec<(String, Article)>, // (feed_title, article)
    pub whats_new_selected: usize,
    pub show_whats_new: bool,
}

impl App {
    pub fn new(db: Database) -> Self {
        Self {
            db,
            http_client: reqwest::Client::new(),
            feeds: Vec::new(),
            articles: Vec::new(),
            view: View::Browse,
            focus: Focus::Feeds,
            selected_feed: 0,
            selected_article: 0,
            scroll_offset: 0,
            search_mode: false,
            search_input: String::new(),
            content_state: ContentState::Idle,
            reader_article: None,
            refresh_progress: None,
            status_message: None,
            whats_new: Vec::new(),
            whats_new_selected: 0,
            show_whats_new: false,
        }
    }

    /// Get currently selected feed (bounds-checked)
    pub fn selected_feed(&self) -> Option<&Feed> {
        self.feeds.get(self.selected_feed)
    }

    /// Get currently selected article (bounds-checked)
    pub fn selected_article(&self) -> Option<&Article> {
        self.articles.get(self.selected_article)
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
                    self.whats_new_selected =
                        (self.whats_new_selected + 1).min(self.whats_new.len() - 1);
                }
            }
            Focus::Feeds => {
                if !self.feeds.is_empty() {
                    self.selected_feed = (self.selected_feed + 1).min(self.feeds.len() - 1);
                }
            }
            Focus::Articles => {
                if !self.articles.is_empty() {
                    self.selected_article =
                        (self.selected_article + 1).min(self.articles.len() - 1);
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

    /// Set status message (will auto-expire after 3 seconds)
    pub fn set_status(&mut self, msg: impl Into<String>) {
        self.status_message = Some((msg.into(), Instant::now()));
    }

    /// Clear status message if expired (older than 3 seconds)
    pub fn clear_expired_status(&mut self) {
        if let Some((_, time)) = &self.status_message {
            if time.elapsed().as_secs() >= 3 {
                self.status_message = None;
            }
        }
    }

    /// Enter reader view for currently selected article
    /// Returns the article for content fetching
    pub fn enter_reader(&mut self) -> Option<&Article> {
        if let Some(article) = self.articles.get(self.selected_article).cloned() {
            self.view = View::Reader;
            self.scroll_offset = 0;
            self.content_state = ContentState::Loading {
                article_id: article.id,
            };
            self.reader_article = Some(article);
            self.reader_article.as_ref()
        } else {
            None
        }
    }

    /// Exit reader view back to browse
    pub fn exit_reader(&mut self) {
        self.view = View::Browse;
        self.content_state = ContentState::Idle;
        self.scroll_offset = 0;
        self.reader_article = None;
    }
}
