//! Keybinding registry — maps actions to key events with config overrides.
//!
//! Replaces hardcoded key match arms with a data-driven registry that supports
//! user customization via config.toml.
use crossterm::event::{KeyCode, KeyModifiers};
use std::collections::HashMap;

// ============================================================================
// Action Enum
// ============================================================================

/// All user-facing actions that can be triggered by keybindings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    Quit,
    NavDown,
    NavUp,
    CycleFocus,
    Back,
    Select,
    RefreshAll,
    RefreshOne,
    ToggleStar,
    ToggleStarredMode,
    EnterSearch,
    ExitSearch,
    CommitSearch,
    MarkFeedRead,
    MarkAllRead,
    OpenInBrowser,
    OpenFeedSite,
    ExportOpml,
    ScrollDown,
    ScrollUp,
    PageDown,
    PageUp,
    ExitReader,
    CycleTheme,
    ShowHelp,
    DeleteFeed,
    Subscribe,
    ToggleCategories,
    CollapseCategory,
    ExpandCategory,
    ContextMenu,
    Prefetch,
    ViewStats,
}

impl Action {
    /// Human-readable description for the help screen.
    pub fn describe(self) -> &'static str {
        match self {
            Self::Quit => "Quit application",
            Self::NavDown => "Navigate down",
            Self::NavUp => "Navigate up",
            Self::CycleFocus => "Cycle panel focus",
            Self::Back => "Go back / dismiss",
            Self::Select => "Select / open",
            Self::RefreshAll => "Refresh all feeds",
            Self::RefreshOne => "Refresh current feed",
            Self::ToggleStar => "Toggle star on article",
            Self::ToggleStarredMode => "Toggle starred articles view",
            Self::EnterSearch => "Enter search mode",
            Self::ExitSearch => "Exit search mode",
            Self::CommitSearch => "Execute search",
            Self::MarkFeedRead => "Mark feed as read",
            Self::MarkAllRead => "Mark all as read",
            Self::OpenInBrowser => "Open in browser",
            Self::OpenFeedSite => "Open feed website",
            Self::ExportOpml => "Export feeds to OPML",
            Self::ScrollDown => "Scroll down one line",
            Self::ScrollUp => "Scroll up one line",
            Self::PageDown => "Page down",
            Self::PageUp => "Page up",
            Self::ExitReader => "Exit reader view",
            Self::CycleTheme => "Cycle theme",
            Self::ShowHelp => "Show help",
            Self::DeleteFeed => "Delete feed",
            Self::Subscribe => "Subscribe to feed",
            Self::ToggleCategories => "Toggle category panel",
            Self::CollapseCategory => "Collapse category",
            Self::ExpandCategory => "Expand category",
            Self::ContextMenu => "Feed context menu",
            Self::Prefetch => "Prefetch articles for offline",
            Self::ViewStats => "View reading stats",
        }
    }
}

// ============================================================================
// Context Enum
// ============================================================================

/// Dispatch context — determines which bindings are active.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Context {
    Global,
    FeedList,
    ArticleList,
    Reader,
    Search,
    WhatsNew,
    Categories,
}

// ============================================================================
// Key Specification
// ============================================================================

/// A key event: code + modifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KeySpec {
    pub code: KeyCode,
    pub modifiers: KeyModifiers,
}

impl KeySpec {
    pub const fn new(code: KeyCode, modifiers: KeyModifiers) -> Self {
        Self { code, modifiers }
    }

    pub const fn plain(code: KeyCode) -> Self {
        Self::new(code, KeyModifiers::NONE)
    }

    pub const fn ctrl(c: char) -> Self {
        Self::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }
}

/// Parse a key string from config into a KeySpec.
///
/// Supported formats:
/// - Single char: "q", "j", "/"
/// - Named keys: "Enter", "Esc", "Tab", "Up", "Down", "Backspace"
/// - Modifier combos: "Ctrl+d", "Ctrl+u"
/// - Function keys: "F1" through "F12"
#[cfg_attr(not(test), allow(dead_code))] // Called from apply_overrides; wired in main on config integration
fn parse_key_string(s: &str) -> Option<KeySpec> {
    let s = s.trim();

    // Handle Ctrl+ prefix
    if let Some(rest) = s.strip_prefix("Ctrl+") {
        let rest = rest.trim();
        if rest.len() == 1 {
            let c = rest.chars().next()?;
            return Some(KeySpec::ctrl(c));
        }
        return None;
    }

    // Named keys (case-insensitive)
    match s.to_lowercase().as_str() {
        "enter" | "return" => return Some(KeySpec::plain(KeyCode::Enter)),
        "esc" | "escape" => return Some(KeySpec::plain(KeyCode::Esc)),
        "tab" => return Some(KeySpec::plain(KeyCode::Tab)),
        "up" => return Some(KeySpec::plain(KeyCode::Up)),
        "down" => return Some(KeySpec::plain(KeyCode::Down)),
        "left" => return Some(KeySpec::plain(KeyCode::Left)),
        "right" => return Some(KeySpec::plain(KeyCode::Right)),
        "backspace" => return Some(KeySpec::plain(KeyCode::Backspace)),
        "space" => return Some(KeySpec::plain(KeyCode::Char(' '))),
        _ => {}
    }

    // Function keys
    if s.starts_with('F') || s.starts_with('f') {
        if let Ok(n) = s[1..].parse::<u8>() {
            if (1..=12).contains(&n) {
                return Some(KeySpec::plain(KeyCode::F(n)));
            }
        }
    }

    // Single character
    if s.len() == 1 {
        let c = s.chars().next()?;
        return Some(KeySpec::plain(KeyCode::Char(c)));
    }

    None
}

/// Format a KeySpec as a human-readable string for the help screen.
fn format_key(key: &KeySpec) -> String {
    let modifier = if key.modifiers.contains(KeyModifiers::CONTROL) {
        "Ctrl+"
    } else {
        ""
    };

    let key_name = match key.code {
        KeyCode::Char(c) => c.to_string(),
        KeyCode::Enter => "Enter".to_string(),
        KeyCode::Esc => "Esc".to_string(),
        KeyCode::Tab => "Tab".to_string(),
        KeyCode::Up => "Up".to_string(),
        KeyCode::Down => "Down".to_string(),
        KeyCode::Left => "Left".to_string(),
        KeyCode::Right => "Right".to_string(),
        KeyCode::Backspace => "Backspace".to_string(),
        KeyCode::F(n) => format!("F{}", n),
        _ => "?".to_string(),
    };

    format!("{}{}", modifier, key_name)
}

// ============================================================================
// Keybinding Registry
// ============================================================================

/// Registry of keybindings, supporting default bindings and config overrides.
///
/// Lookup is O(1) via HashMap. The registry supports context-aware dispatch:
/// the same key can map to different actions in different contexts.
pub struct KeybindingRegistry {
    /// Primary lookup: (Context, KeySpec) -> Action
    lookup: HashMap<(Context, KeySpec), Action>,
    /// All bindings for help screen enumeration
    bindings: Vec<(Context, KeySpec, Action)>,
}

impl KeybindingRegistry {
    /// Create a registry with default bindings matching the current hardcoded keys.
    pub fn new() -> Self {
        let mut registry = Self {
            lookup: HashMap::new(),
            bindings: Vec::new(),
        };
        registry.register_defaults();
        registry
    }

    /// Register a single binding.
    fn bind(&mut self, context: Context, key: KeySpec, action: Action) {
        self.lookup.insert((context, key), action);
        self.bindings.push((context, key, action));
    }

    /// Register all default bindings matching current hardcoded behavior.
    fn register_defaults(&mut self) {
        // === Global (Browse view) ===
        // Quit
        self.bind(
            Context::Global,
            KeySpec::plain(KeyCode::Char('q')),
            Action::Quit,
        );

        // Navigation
        self.bind(
            Context::Global,
            KeySpec::plain(KeyCode::Char('j')),
            Action::NavDown,
        );
        self.bind(
            Context::Global,
            KeySpec::plain(KeyCode::Down),
            Action::NavDown,
        );
        self.bind(
            Context::Global,
            KeySpec::plain(KeyCode::Char('k')),
            Action::NavUp,
        );
        self.bind(Context::Global, KeySpec::plain(KeyCode::Up), Action::NavUp);

        // Focus
        self.bind(
            Context::Global,
            KeySpec::plain(KeyCode::Tab),
            Action::CycleFocus,
        );

        // Back / dismiss
        self.bind(Context::Global, KeySpec::plain(KeyCode::Esc), Action::Back);

        // Select / open
        self.bind(
            Context::Global,
            KeySpec::plain(KeyCode::Enter),
            Action::Select,
        );

        // Feed operations
        self.bind(
            Context::Global,
            KeySpec::plain(KeyCode::Char('r')),
            Action::RefreshAll,
        );
        self.bind(
            Context::Global,
            KeySpec::plain(KeyCode::Char('R')),
            Action::RefreshOne,
        );

        // Star
        self.bind(
            Context::Global,
            KeySpec::plain(KeyCode::Char('s')),
            Action::ToggleStar,
        );
        self.bind(
            Context::Global,
            KeySpec::plain(KeyCode::Char('S')),
            Action::ToggleStarredMode,
        );
        self.bind(
            Context::Global,
            KeySpec::plain(KeyCode::Char('I')),
            Action::ViewStats,
        );

        // Search
        self.bind(
            Context::Global,
            KeySpec::plain(KeyCode::Char('/')),
            Action::EnterSearch,
        );

        // Mark read
        self.bind(
            Context::Global,
            KeySpec::plain(KeyCode::Char('a')),
            Action::MarkFeedRead,
        );
        self.bind(
            Context::Global,
            KeySpec::plain(KeyCode::Char('A')),
            Action::MarkAllRead,
        );

        // Open
        self.bind(
            Context::Global,
            KeySpec::plain(KeyCode::Char('o')),
            Action::OpenInBrowser,
        );
        self.bind(
            Context::Global,
            KeySpec::plain(KeyCode::Char('O')),
            Action::OpenFeedSite,
        );

        // Export (feed list context)
        self.bind(
            Context::FeedList,
            KeySpec::plain(KeyCode::Char('e')),
            Action::ExportOpml,
        );

        // Delete feed (feed list context)
        self.bind(
            Context::FeedList,
            KeySpec::plain(KeyCode::Char('d')),
            Action::DeleteFeed,
        );

        // Subscribe to feed (feed list context)
        self.bind(
            Context::FeedList,
            KeySpec::plain(KeyCode::Char('+')),
            Action::Subscribe,
        );

        // Feed context menu (feed list context)
        self.bind(
            Context::FeedList,
            KeySpec::plain(KeyCode::Char('m')),
            Action::ContextMenu,
        );

        // Prefetch articles for offline reading (feed list context)
        self.bind(
            Context::FeedList,
            KeySpec::plain(KeyCode::Char('P')),
            Action::Prefetch,
        );

        // Theme + Help (new actions)
        self.bind(
            Context::Global,
            KeySpec::plain(KeyCode::Char('T')),
            Action::CycleTheme,
        );
        self.bind(
            Context::Global,
            KeySpec::plain(KeyCode::Char('?')),
            Action::ShowHelp,
        );

        // Categories panel toggle
        self.bind(
            Context::Global,
            KeySpec::plain(KeyCode::Char('c')),
            Action::ToggleCategories,
        );

        // Category-specific keys
        self.bind(
            Context::Categories,
            KeySpec::plain(KeyCode::Left),
            Action::CollapseCategory,
        );
        self.bind(
            Context::Categories,
            KeySpec::plain(KeyCode::Char('h')),
            Action::CollapseCategory,
        );
        self.bind(
            Context::Categories,
            KeySpec::plain(KeyCode::Right),
            Action::ExpandCategory,
        );
        self.bind(
            Context::Categories,
            KeySpec::plain(KeyCode::Char('l')),
            Action::ExpandCategory,
        );

        // === Reader view ===
        // Quit (also works in reader)
        self.bind(
            Context::Reader,
            KeySpec::plain(KeyCode::Char('q')),
            Action::Quit,
        );

        // Exit reader
        self.bind(
            Context::Reader,
            KeySpec::plain(KeyCode::Char('b')),
            Action::ExitReader,
        );
        self.bind(
            Context::Reader,
            KeySpec::plain(KeyCode::Esc),
            Action::ExitReader,
        );

        // Scroll
        self.bind(
            Context::Reader,
            KeySpec::plain(KeyCode::Char('j')),
            Action::ScrollDown,
        );
        self.bind(
            Context::Reader,
            KeySpec::plain(KeyCode::Down),
            Action::ScrollDown,
        );
        self.bind(
            Context::Reader,
            KeySpec::plain(KeyCode::Char('k')),
            Action::ScrollUp,
        );
        self.bind(
            Context::Reader,
            KeySpec::plain(KeyCode::Up),
            Action::ScrollUp,
        );

        // Page scroll
        self.bind(Context::Reader, KeySpec::ctrl('d'), Action::PageDown);
        self.bind(Context::Reader, KeySpec::ctrl('u'), Action::PageUp);

        // Star in reader
        self.bind(
            Context::Reader,
            KeySpec::plain(KeyCode::Char('s')),
            Action::ToggleStar,
        );

        // Open in browser from reader
        self.bind(
            Context::Reader,
            KeySpec::plain(KeyCode::Char('o')),
            Action::OpenInBrowser,
        );

        // === Search mode ===
        self.bind(
            Context::Search,
            KeySpec::plain(KeyCode::Esc),
            Action::ExitSearch,
        );
        self.bind(
            Context::Search,
            KeySpec::plain(KeyCode::Enter),
            Action::CommitSearch,
        );
    }

    /// Apply user overrides from config keybindings map.
    ///
    /// Keys in the map are action names (e.g., "quit", "nav_down").
    /// Values are key strings (e.g., "q", "Ctrl+d", "F5").
    ///
    /// Returns a list of warnings for unrecognized action names or unparseable keys.
    #[cfg_attr(not(test), allow(dead_code))] // Wired in main on config integration
    pub fn apply_overrides(&mut self, overrides: &HashMap<String, String>) -> Vec<String> {
        let mut warnings = Vec::new();

        for (action_name, key_str) in overrides {
            let action = match parse_action_name(action_name) {
                Some(a) => a,
                None => {
                    warnings.push(format!("Unknown action '{}', ignoring", action_name));
                    continue;
                }
            };

            let key = match parse_key_string(key_str) {
                Some(k) => k,
                None => {
                    warnings.push(format!(
                        "Cannot parse key '{}' for action '{}', ignoring",
                        key_str, action_name
                    ));
                    continue;
                }
            };

            // Remove old bindings for this action (in all contexts where it's bound)
            let contexts_for_action: Vec<Context> = self
                .bindings
                .iter()
                .filter(|(_, _, a)| *a == action)
                .map(|(c, _, _)| *c)
                .collect();

            // Remove old entries from lookup
            self.lookup.retain(|_, a| *a != action);
            self.bindings.retain(|(_, _, a)| *a != action);

            // Re-bind in the same contexts with the new key
            for ctx in contexts_for_action {
                self.bind(ctx, key, action);
            }

            tracing::info!(
                action = %action_name,
                key = %key_str,
                "Applied keybinding override"
            );
        }

        warnings
    }

    /// Look up the action for a given key in a given context.
    ///
    /// Tries the specific context first, then falls back to Global.
    pub fn action_for_key(
        &self,
        code: KeyCode,
        modifiers: KeyModifiers,
        context: Context,
    ) -> Option<Action> {
        let key = KeySpec::new(code, modifiers);

        // Try specific context first
        if let Some(&action) = self.lookup.get(&(context, key)) {
            return Some(action);
        }

        // Fall back to Global (unless we're already looking at Global)
        if context != Context::Global {
            if let Some(&action) = self.lookup.get(&(Context::Global, key)) {
                return Some(action);
            }
        }

        None
    }

    /// Get all bindings for the help screen.
    ///
    /// Returns (context, key_display_string, action, description) tuples.
    pub fn all_bindings(&self) -> Vec<(Context, String, Action, &'static str)> {
        self.bindings
            .iter()
            .map(|(ctx, key, action)| (*ctx, format_key(key), *action, action.describe()))
            .collect()
    }
}

impl Default for KeybindingRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Parse an action name string (from config) into an Action enum.
#[cfg_attr(not(test), allow(dead_code))] // Called from apply_overrides; wired in main on config integration
fn parse_action_name(name: &str) -> Option<Action> {
    match name.to_lowercase().as_str() {
        "quit" => Some(Action::Quit),
        "nav_down" | "navdown" | "down" => Some(Action::NavDown),
        "nav_up" | "navup" | "up" => Some(Action::NavUp),
        "cycle_focus" | "cyclefocus" | "tab" => Some(Action::CycleFocus),
        "back" => Some(Action::Back),
        "select" | "enter" => Some(Action::Select),
        "refresh_all" | "refreshall" | "refresh" => Some(Action::RefreshAll),
        "refresh_one" | "refreshone" => Some(Action::RefreshOne),
        "toggle_star" | "togglestar" | "star" => Some(Action::ToggleStar),
        "toggle_starred_mode" | "togglestarredmode" | "starred" => Some(Action::ToggleStarredMode),
        "enter_search" | "entersearch" | "search" => Some(Action::EnterSearch),
        "exit_search" | "exitsearch" => Some(Action::ExitSearch),
        "commit_search" | "commitsearch" => Some(Action::CommitSearch),
        "mark_feed_read" | "markfeedread" => Some(Action::MarkFeedRead),
        "mark_all_read" | "markallread" => Some(Action::MarkAllRead),
        "open_in_browser" | "openinbrowser" | "open" => Some(Action::OpenInBrowser),
        "open_feed_site" | "openfeedsite" => Some(Action::OpenFeedSite),
        "export_opml" | "exportopml" | "export" => Some(Action::ExportOpml),
        "scroll_down" | "scrolldown" => Some(Action::ScrollDown),
        "scroll_up" | "scrollup" => Some(Action::ScrollUp),
        "page_down" | "pagedown" => Some(Action::PageDown),
        "page_up" | "pageup" => Some(Action::PageUp),
        "exit_reader" | "exitreader" => Some(Action::ExitReader),
        "cycle_theme" | "cycletheme" | "theme" => Some(Action::CycleTheme),
        "show_help" | "showhelp" | "help" => Some(Action::ShowHelp),
        "delete_feed" | "deletefeed" | "delete" => Some(Action::DeleteFeed),
        "subscribe" | "add_feed" | "addfeed" => Some(Action::Subscribe),
        "toggle_categories" | "togglecategories" | "categories" => Some(Action::ToggleCategories),
        "collapse_category" | "collapsecategory" => Some(Action::CollapseCategory),
        "expand_category" | "expandcategory" => Some(Action::ExpandCategory),
        "context_menu" | "contextmenu" | "menu" => Some(Action::ContextMenu),
        "view_stats" | "viewstats" | "stats" => Some(Action::ViewStats),
        _ => None,
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_registry_has_quit() {
        let reg = KeybindingRegistry::new();
        let action = reg.action_for_key(KeyCode::Char('q'), KeyModifiers::NONE, Context::Global);
        assert_eq!(action, Some(Action::Quit));
    }

    #[test]
    fn test_default_nav_keys() {
        let reg = KeybindingRegistry::new();
        assert_eq!(
            reg.action_for_key(KeyCode::Char('j'), KeyModifiers::NONE, Context::Global),
            Some(Action::NavDown)
        );
        assert_eq!(
            reg.action_for_key(KeyCode::Down, KeyModifiers::NONE, Context::Global),
            Some(Action::NavDown)
        );
        assert_eq!(
            reg.action_for_key(KeyCode::Char('k'), KeyModifiers::NONE, Context::Global),
            Some(Action::NavUp)
        );
        assert_eq!(
            reg.action_for_key(KeyCode::Up, KeyModifiers::NONE, Context::Global),
            Some(Action::NavUp)
        );
    }

    #[test]
    fn test_reader_context_overrides_global() {
        let reg = KeybindingRegistry::new();
        // In reader, 'j' = ScrollDown (not NavDown)
        assert_eq!(
            reg.action_for_key(KeyCode::Char('j'), KeyModifiers::NONE, Context::Reader),
            Some(Action::ScrollDown)
        );
        // In reader, Esc = ExitReader (not Back)
        assert_eq!(
            reg.action_for_key(KeyCode::Esc, KeyModifiers::NONE, Context::Reader),
            Some(Action::ExitReader)
        );
    }

    #[test]
    fn test_reader_falls_back_to_global_for_quit() {
        let reg = KeybindingRegistry::new();
        // Reader has its own 'q' binding
        assert_eq!(
            reg.action_for_key(KeyCode::Char('q'), KeyModifiers::NONE, Context::Reader),
            Some(Action::Quit)
        );
    }

    #[test]
    fn test_ctrl_modifiers() {
        let reg = KeybindingRegistry::new();
        assert_eq!(
            reg.action_for_key(KeyCode::Char('d'), KeyModifiers::CONTROL, Context::Reader),
            Some(Action::PageDown)
        );
        assert_eq!(
            reg.action_for_key(KeyCode::Char('u'), KeyModifiers::CONTROL, Context::Reader),
            Some(Action::PageUp)
        );
    }

    #[test]
    fn test_search_context() {
        let reg = KeybindingRegistry::new();
        assert_eq!(
            reg.action_for_key(KeyCode::Esc, KeyModifiers::NONE, Context::Search),
            Some(Action::ExitSearch)
        );
        assert_eq!(
            reg.action_for_key(KeyCode::Enter, KeyModifiers::NONE, Context::Search),
            Some(Action::CommitSearch)
        );
    }

    #[test]
    fn test_unknown_key_returns_none() {
        let reg = KeybindingRegistry::new();
        assert_eq!(
            reg.action_for_key(KeyCode::F(12), KeyModifiers::NONE, Context::Global),
            None
        );
    }

    #[test]
    fn test_export_only_in_feed_list() {
        let reg = KeybindingRegistry::new();
        // 'e' in FeedList = ExportOpml
        assert_eq!(
            reg.action_for_key(KeyCode::Char('e'), KeyModifiers::NONE, Context::FeedList),
            Some(Action::ExportOpml)
        );
        // 'e' in ArticleList = None (not bound globally or in article context)
        assert_eq!(
            reg.action_for_key(KeyCode::Char('e'), KeyModifiers::NONE, Context::ArticleList),
            None
        );
    }

    #[test]
    fn test_apply_overrides_valid() {
        let mut reg = KeybindingRegistry::new();
        let mut overrides = HashMap::new();
        overrides.insert("quit".to_string(), "Ctrl+q".to_string());

        let warnings = reg.apply_overrides(&overrides);
        assert!(warnings.is_empty());

        // Old binding should be gone
        assert_eq!(
            reg.action_for_key(KeyCode::Char('q'), KeyModifiers::NONE, Context::Global),
            None
        );
        // New binding should work
        assert_eq!(
            reg.action_for_key(KeyCode::Char('q'), KeyModifiers::CONTROL, Context::Global),
            Some(Action::Quit)
        );
    }

    #[test]
    fn test_apply_overrides_unknown_action() {
        let mut reg = KeybindingRegistry::new();
        let mut overrides = HashMap::new();
        overrides.insert("nonexistent_action".to_string(), "q".to_string());

        let warnings = reg.apply_overrides(&overrides);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("Unknown action"));
    }

    #[test]
    fn test_apply_overrides_bad_key() {
        let mut reg = KeybindingRegistry::new();
        let mut overrides = HashMap::new();
        overrides.insert("quit".to_string(), "Ctrl+Alt+Shift+Q".to_string());

        let warnings = reg.apply_overrides(&overrides);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("Cannot parse key"));
    }

    #[test]
    fn test_parse_key_string_named_keys() {
        assert_eq!(
            parse_key_string("Enter"),
            Some(KeySpec::plain(KeyCode::Enter))
        );
        assert_eq!(parse_key_string("esc"), Some(KeySpec::plain(KeyCode::Esc)));
        assert_eq!(parse_key_string("Tab"), Some(KeySpec::plain(KeyCode::Tab)));
        assert_eq!(
            parse_key_string("space"),
            Some(KeySpec::plain(KeyCode::Char(' ')))
        );
    }

    #[test]
    fn test_parse_key_string_function_keys() {
        assert_eq!(parse_key_string("F1"), Some(KeySpec::plain(KeyCode::F(1))));
        assert_eq!(
            parse_key_string("F12"),
            Some(KeySpec::plain(KeyCode::F(12)))
        );
        assert_eq!(parse_key_string("F0"), None);
        assert_eq!(parse_key_string("F13"), None);
    }

    #[test]
    fn test_parse_key_string_ctrl() {
        assert_eq!(parse_key_string("Ctrl+d"), Some(KeySpec::ctrl('d')));
        assert_eq!(parse_key_string("Ctrl+u"), Some(KeySpec::ctrl('u')));
    }

    #[test]
    fn test_parse_key_string_single_char() {
        assert_eq!(
            parse_key_string("q"),
            Some(KeySpec::plain(KeyCode::Char('q')))
        );
        assert_eq!(
            parse_key_string("/"),
            Some(KeySpec::plain(KeyCode::Char('/')))
        );
    }

    #[test]
    fn test_all_bindings_non_empty() {
        let reg = KeybindingRegistry::new();
        let bindings = reg.all_bindings();
        assert!(!bindings.is_empty());
        // Should have at least the core bindings
        assert!(bindings.len() >= 20);
    }

    #[test]
    fn test_action_describe() {
        assert_eq!(Action::Quit.describe(), "Quit application");
        assert_eq!(Action::NavDown.describe(), "Navigate down");
        assert_eq!(Action::ScrollDown.describe(), "Scroll down one line");
    }

    #[test]
    fn test_format_key_display() {
        assert_eq!(format_key(&KeySpec::plain(KeyCode::Char('q'))), "q");
        assert_eq!(format_key(&KeySpec::ctrl('d')), "Ctrl+d");
        assert_eq!(format_key(&KeySpec::plain(KeyCode::Enter)), "Enter");
        assert_eq!(format_key(&KeySpec::plain(KeyCode::F(5))), "F5");
    }

    #[test]
    fn test_override_preserves_contexts() {
        let mut reg = KeybindingRegistry::new();
        // ToggleStar is bound in Global and Reader contexts
        assert_eq!(
            reg.action_for_key(KeyCode::Char('s'), KeyModifiers::NONE, Context::Global),
            Some(Action::ToggleStar)
        );
        assert_eq!(
            reg.action_for_key(KeyCode::Char('s'), KeyModifiers::NONE, Context::Reader),
            Some(Action::ToggleStar)
        );

        // Override star to 'x'
        let mut overrides = HashMap::new();
        overrides.insert("star".to_string(), "x".to_string());
        let warnings = reg.apply_overrides(&overrides);
        assert!(warnings.is_empty());

        // New key should work in both contexts
        assert_eq!(
            reg.action_for_key(KeyCode::Char('x'), KeyModifiers::NONE, Context::Global),
            Some(Action::ToggleStar)
        );
        assert_eq!(
            reg.action_for_key(KeyCode::Char('x'), KeyModifiers::NONE, Context::Reader),
            Some(Action::ToggleStar)
        );

        // Old key should be gone
        assert_eq!(
            reg.action_for_key(KeyCode::Char('s'), KeyModifiers::NONE, Context::Global),
            None
        );
    }
}
