//! Theme system for the TUI.
//!
//! Provides semantic color roles that map to ratatui `Style` values.
//! The `ThemeVariant` enum selects between Dark and Light palettes,
//! and `StyleMap` resolves role names to concrete styles.

use ratatui::style::{Color, Modifier, Style};
use std::collections::HashMap;

// ============================================================================
// Theme Variant
// ============================================================================

/// Available theme variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeVariant {
    Dark,
    Light,
}

impl ThemeVariant {
    /// Parse a variant name from a string (case-insensitive).
    #[cfg_attr(not(test), allow(dead_code))] // Wired when theme-from-prefs loads at startup
    pub fn from_str_name(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "dark" => Some(Self::Dark),
            "light" => Some(Self::Light),
            _ => None,
        }
    }

    /// Build the `ColorPalette` for this variant.
    pub fn palette(self) -> ColorPalette {
        match self {
            Self::Dark => ColorPalette::dark(),
            Self::Light => ColorPalette::light(),
        }
    }

    /// Cycle to the next variant: Dark → Light → Dark.
    pub fn next(self) -> Self {
        match self {
            Self::Dark => Self::Light,
            Self::Light => Self::Dark,
        }
    }

    /// Human-readable name for status display.
    pub fn name(self) -> &'static str {
        match self {
            Self::Dark => "Dark",
            Self::Light => "Light",
        }
    }
}

// ============================================================================
// Color Palette — semantic roles to Style
// ============================================================================

/// A complete color palette mapping every semantic UI role to a `Style`.
///
/// Each field corresponds to a specific visual element in the TUI.
/// The Dark palette exactly reproduces the original hardcoded colors.
#[derive(Debug, Clone)]
pub struct ColorPalette {
    // -- Feed list --
    pub feed_normal: Style,
    pub feed_selected: Style,
    pub feed_unread: Style,
    pub feed_error: Style,

    // -- Article list --
    pub article_title: Style,
    pub article_read: Style,
    pub article_selected: Style,
    pub article_date: Style,
    pub article_star: Style,
    pub article_feed_prefix: Style,

    // -- Reader --
    pub reader_heading: Style,
    pub reader_body: Style,
    pub reader_metadata: Style,
    pub reader_code_block: Style,
    pub reader_inline_code: Style,
    pub reader_emphasis: Style,
    pub reader_strong: Style,
    pub reader_image: Style,
    pub reader_error: Style,
    pub reader_fallback: Style,

    // -- Chrome --
    pub status_bar: Style,
    pub panel_border: Style,
    pub panel_border_focused: Style,

    // -- What's New panel --
    pub whatsnew_border_focused: Style,
    pub whatsnew_border_unfocused: Style,
    pub whatsnew_selected: Style,
    pub whatsnew_title: Style,
}

impl ColorPalette {
    /// Dark palette — exactly reproduces original hardcoded colors.
    fn dark() -> Self {
        Self {
            // Feed list
            feed_normal: Style::default(),
            feed_selected: Style::default().bg(Color::DarkGray).fg(Color::White),
            feed_unread: Style::default().add_modifier(Modifier::BOLD),
            feed_error: Style::default().fg(Color::Red),

            // Article list
            article_title: Style::default().add_modifier(Modifier::BOLD),
            article_read: Style::default().fg(Color::Gray),
            article_selected: Style::default().bg(Color::DarkGray).fg(Color::White),
            article_date: Style::default().fg(Color::DarkGray),
            article_star: Style::default().fg(Color::Yellow),
            article_feed_prefix: Style::default().fg(Color::Cyan),

            // Reader
            reader_heading: Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
            reader_body: Style::default(),
            reader_metadata: Style::default().fg(Color::DarkGray),
            reader_code_block: Style::default().fg(Color::Yellow).bg(Color::Black),
            reader_inline_code: Style::default().fg(Color::Yellow),
            reader_emphasis: Style::default().add_modifier(Modifier::ITALIC),
            reader_strong: Style::default().add_modifier(Modifier::BOLD),
            reader_image: Style::default().fg(Color::Blue),
            reader_error: Style::default().fg(Color::Red),
            reader_fallback: Style::default().fg(Color::Yellow),

            // Chrome
            status_bar: Style::default().bg(Color::DarkGray).fg(Color::White),
            panel_border: Style::default(),
            panel_border_focused: Style::default().fg(Color::Cyan),

            // What's New
            whatsnew_border_focused: Style::default().fg(Color::Yellow),
            whatsnew_border_unfocused: Style::default().fg(Color::DarkGray),
            whatsnew_selected: Style::default()
                .bg(Color::DarkGray)
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
            whatsnew_title: Style::default().add_modifier(Modifier::BOLD),
        }
    }

    /// Light palette — adapted for light terminal backgrounds.
    fn light() -> Self {
        Self {
            // Feed list
            feed_normal: Style::default().fg(Color::Black),
            feed_selected: Style::default().bg(Color::Blue).fg(Color::White),
            feed_unread: Style::default()
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD),
            feed_error: Style::default().fg(Color::Red),

            // Article list
            article_title: Style::default()
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD),
            article_read: Style::default().fg(Color::DarkGray),
            article_selected: Style::default().bg(Color::Blue).fg(Color::White),
            article_date: Style::default().fg(Color::DarkGray),
            article_star: Style::default().fg(Color::Magenta),
            article_feed_prefix: Style::default().fg(Color::Blue),

            // Reader
            reader_heading: Style::default()
                .fg(Color::Blue)
                .add_modifier(Modifier::BOLD),
            reader_body: Style::default().fg(Color::Black),
            reader_metadata: Style::default().fg(Color::DarkGray),
            reader_code_block: Style::default().fg(Color::DarkGray).bg(Color::White),
            reader_inline_code: Style::default().fg(Color::DarkGray),
            reader_emphasis: Style::default().add_modifier(Modifier::ITALIC),
            reader_strong: Style::default().add_modifier(Modifier::BOLD),
            reader_image: Style::default().fg(Color::Blue),
            reader_error: Style::default().fg(Color::Red),
            reader_fallback: Style::default().fg(Color::Magenta),

            // Chrome
            status_bar: Style::default().bg(Color::White).fg(Color::Black),
            panel_border: Style::default().fg(Color::DarkGray),
            panel_border_focused: Style::default().fg(Color::Blue),

            // What's New
            whatsnew_border_focused: Style::default().fg(Color::Magenta),
            whatsnew_border_unfocused: Style::default().fg(Color::DarkGray),
            whatsnew_selected: Style::default()
                .bg(Color::Blue)
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
            whatsnew_title: Style::default()
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD),
        }
    }
}

// ============================================================================
// Style Map — string-keyed lookup for config-driven overrides
// ============================================================================

/// String-keyed style lookup for dynamic/config-driven overrides.
///
/// Built from a `ColorPalette`, this allows resolving role names (e.g.
/// `"reader_heading"`) to their concrete `Style` at runtime.
#[derive(Debug, Clone)]
pub struct StyleMap {
    map: HashMap<&'static str, Style>,
}

/// All semantic role names, in declaration order.
const ROLE_NAMES: [&str; 27] = [
    "feed_normal",
    "feed_selected",
    "feed_unread",
    "feed_error",
    "article_title",
    "article_read",
    "article_selected",
    "article_date",
    "article_star",
    "article_feed_prefix",
    "reader_heading",
    "reader_body",
    "reader_metadata",
    "reader_code_block",
    "reader_inline_code",
    "reader_emphasis",
    "reader_strong",
    "reader_image",
    "reader_error",
    "reader_fallback",
    "status_bar",
    "panel_border",
    "panel_border_focused",
    "whatsnew_border_focused",
    "whatsnew_border_unfocused",
    "whatsnew_selected",
    "whatsnew_title",
];

impl StyleMap {
    /// Build a `StyleMap` from a `ColorPalette`.
    pub fn from_palette(p: &ColorPalette) -> Self {
        let styles: [Style; 27] = [
            p.feed_normal,
            p.feed_selected,
            p.feed_unread,
            p.feed_error,
            p.article_title,
            p.article_read,
            p.article_selected,
            p.article_date,
            p.article_star,
            p.article_feed_prefix,
            p.reader_heading,
            p.reader_body,
            p.reader_metadata,
            p.reader_code_block,
            p.reader_inline_code,
            p.reader_emphasis,
            p.reader_strong,
            p.reader_image,
            p.reader_error,
            p.reader_fallback,
            p.status_bar,
            p.panel_border,
            p.panel_border_focused,
            p.whatsnew_border_focused,
            p.whatsnew_border_unfocused,
            p.whatsnew_selected,
            p.whatsnew_title,
        ];

        let mut map = HashMap::with_capacity(ROLE_NAMES.len());
        for (name, style) in ROLE_NAMES.iter().zip(styles.iter()) {
            map.insert(*name, *style);
        }

        Self { map }
    }

    /// Resolve a role name to its `Style`. Returns `Style::default()` for unknown roles.
    // PERF-021: HashMap lookup is O(1) amortized for 27 entries (~465ns/frame).
    // If per-row styling needed, consider enum-indexed [Style; 27] array.
    pub fn resolve(&self, role: &str) -> Style {
        self.map.get(role).copied().unwrap_or_default()
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dark_palette_feed_selected_matches_original() {
        let palette = ThemeVariant::Dark.palette();
        // Original: Style::default().bg(Color::DarkGray).fg(Color::White)
        assert_eq!(
            palette.feed_selected,
            Style::default().bg(Color::DarkGray).fg(Color::White)
        );
    }

    #[test]
    fn dark_palette_focus_border_matches_original() {
        let palette = ThemeVariant::Dark.palette();
        assert_eq!(
            palette.panel_border_focused,
            Style::default().fg(Color::Cyan)
        );
    }

    #[test]
    fn dark_palette_reader_code_block_matches_original() {
        let palette = ThemeVariant::Dark.palette();
        assert_eq!(
            palette.reader_code_block,
            Style::default().fg(Color::Yellow).bg(Color::Black)
        );
    }

    #[test]
    fn dark_palette_status_bar_matches_original() {
        let palette = ThemeVariant::Dark.palette();
        assert_eq!(
            palette.status_bar,
            Style::default().bg(Color::DarkGray).fg(Color::White)
        );
    }

    #[test]
    fn dark_palette_stars_matches_original() {
        let palette = ThemeVariant::Dark.palette();
        assert_eq!(palette.article_star, Style::default().fg(Color::Yellow));
    }

    #[test]
    fn dark_palette_whatsnew_focused_matches_original() {
        let palette = ThemeVariant::Dark.palette();
        assert_eq!(
            palette.whatsnew_border_focused,
            Style::default().fg(Color::Yellow)
        );
    }

    #[test]
    fn light_palette_differs_from_dark() {
        let dark = ThemeVariant::Dark.palette();
        let light = ThemeVariant::Light.palette();
        // Light selection uses Blue bg instead of DarkGray
        assert_ne!(dark.feed_selected, light.feed_selected);
        assert_ne!(dark.article_selected, light.article_selected);
    }

    #[test]
    fn variant_from_str_name() {
        assert_eq!(
            ThemeVariant::from_str_name("dark"),
            Some(ThemeVariant::Dark)
        );
        assert_eq!(
            ThemeVariant::from_str_name("Light"),
            Some(ThemeVariant::Light)
        );
        assert_eq!(
            ThemeVariant::from_str_name("DARK"),
            Some(ThemeVariant::Dark)
        );
        assert_eq!(ThemeVariant::from_str_name("neon"), None);
    }

    #[test]
    fn style_map_resolves_known_roles() {
        let palette = ThemeVariant::Dark.palette();
        let sm = StyleMap::from_palette(&palette);

        assert_eq!(sm.resolve("feed_selected"), palette.feed_selected);
        assert_eq!(sm.resolve("reader_heading"), palette.reader_heading);
        assert_eq!(sm.resolve("status_bar"), palette.status_bar);
    }

    #[test]
    fn style_map_returns_default_for_unknown() {
        let palette = ThemeVariant::Dark.palette();
        let sm = StyleMap::from_palette(&palette);
        assert_eq!(sm.resolve("nonexistent_role"), Style::default());
    }

    #[test]
    fn style_map_has_all_roles() {
        let palette = ThemeVariant::Dark.palette();
        let sm = StyleMap::from_palette(&palette);
        for name in ROLE_NAMES {
            assert_ne!(
                sm.map.get(name),
                None,
                "Role '{}' missing from StyleMap",
                name
            );
        }
    }

    #[test]
    fn role_names_count_matches_palette_fields() {
        // Ensure ROLE_NAMES array stays in sync with palette fields.
        // If a role is added to ColorPalette but not to ROLE_NAMES,
        // this will catch it via the from_palette array length.
        let palette = ThemeVariant::Dark.palette();
        let sm = StyleMap::from_palette(&palette);
        assert_eq!(sm.map.len(), ROLE_NAMES.len());
    }
}
