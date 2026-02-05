use std::borrow::Cow;

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Calculates the display width of a string in terminal columns.
///
/// Handles Unicode correctly, accounting for:
/// - CJK characters (typically 2 columns wide)
/// - Emoji (typically 2 columns wide)
/// - Zero-width characters (combining marks, etc.)
/// - Standard ASCII (1 column each)
///
/// # Arguments
///
/// * `s` - The string to measure
///
/// # Returns
///
/// The number of terminal columns the string would occupy when displayed.
///
/// # Examples
///
/// ```
/// use skim::util::display_width;
///
/// assert_eq!(display_width("Hello"), 5);      // ASCII: 5 columns
/// assert_eq!(display_width("你好"), 4);        // CJK: 2 chars * 2 columns
/// assert_eq!(display_width("Hi 🎉"), 5);      // "Hi " (3) + emoji (2)
/// ```
pub fn display_width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

/// Ellipsis string used for truncation
const ELLIPSIS: &str = "...";
/// Display width of the ellipsis (3 columns for ASCII "...")
const ELLIPSIS_WIDTH: usize = 3;

/// Truncates a string to fit within a maximum display width.
///
/// If truncation is necessary, appends "..." to indicate text was cut off.
/// Uses Unicode-aware width calculation to handle CJK characters and emoji
/// correctly, ensuring the result fits within the specified column width.
///
/// This is a single-pass implementation optimized for render-heavy code paths
/// in the TUI.
///
/// # Arguments
///
/// * `s` - The string to truncate
/// * `max_width` - Maximum display width in terminal columns
///
/// # Returns
///
/// A `Cow<str>` that fits within `max_width` columns:
/// - If `max_width == 0`, returns `Cow::Borrowed("")`
/// - If `max_width <= ELLIPSIS_WIDTH` (3), returns `Cow::Owned` with as many characters as fit without ellipsis
/// - If the string fits, returns `Cow::Borrowed(s)` (no allocation!)
/// - If truncation is needed, returns `Cow::Owned` with truncated text and "..." appended
///
/// # Edge Case Behavior
///
/// For very narrow widths (0-3 columns), we return characters that fit without
/// ellipsis, since there's not enough room for "char + ellipsis":
/// - width 0: "" (empty)
/// - width 1: first character if it fits (1-column width), else ""
/// - width 2: up to 2 columns of characters
/// - width 3: up to 3 columns of characters (could be "..." but we don't truncate to just ellipsis)
///
/// # Examples
///
/// ```
/// use skim::util::truncate_to_width;
///
/// // String fits within width
/// assert_eq!(truncate_to_width("Short", 10), "Short");
///
/// // String needs truncation
/// assert_eq!(truncate_to_width("Hello World", 8), "Hello...");
///
/// // CJK text (2 columns per character)
/// assert_eq!(truncate_to_width("你好世界", 7), "你好...");
///
/// // Edge cases: very narrow widths
/// assert_eq!(truncate_to_width("Test", 0), "");
/// assert_eq!(truncate_to_width("Test", 1), "T");
/// assert_eq!(truncate_to_width("Test", 2), "Te");
/// assert_eq!(truncate_to_width("Test", 3), "Tes");
/// ```
pub fn truncate_to_width(s: &str, max_width: usize) -> Cow<'_, str> {
    // Edge case: zero width returns empty string (borrowed static)
    if max_width == 0 {
        return Cow::Borrowed("");
    }

    // Edge case: width too narrow to fit char + ellipsis
    // Return as many characters as fit without ellipsis
    if max_width <= ELLIPSIS_WIDTH {
        let mut byte_end = 0;
        let mut current_width = 0;
        for (idx, c) in s.char_indices() {
            let char_width = UnicodeWidthChar::width(c).unwrap_or(0);
            if current_width + char_width > max_width {
                break;
            }
            current_width += char_width;
            byte_end = idx + c.len_utf8();
        }
        // Check if we're returning the whole string
        if byte_end == s.len() {
            return Cow::Borrowed(s);
        }
        return Cow::Owned(s[..byte_end].to_string());
    }
    let target_width = max_width.saturating_sub(ELLIPSIS_WIDTH);

    let mut current_width = 0;
    let mut cut_point = None; // Byte index where we'd cut if truncation needed
    let mut exceeded_max = false;

    for (idx, c) in s.char_indices() {
        let char_width = UnicodeWidthChar::width(c).unwrap_or(0);

        // Record potential cut point when we first exceed target_width
        // (leaving room for ellipsis)
        if cut_point.is_none() && current_width + char_width > target_width {
            cut_point = Some(idx);
        }

        // Check if string exceeds max_width (needs truncation)
        if current_width + char_width > max_width {
            exceeded_max = true;
            break;
        }

        current_width += char_width;
    }

    if exceeded_max {
        // Use cut_point if set, otherwise cut at current position
        let cut = cut_point.unwrap_or(s.len());
        Cow::Owned(format!("{}{}", &s[..cut], ELLIPSIS))
    } else {
        Cow::Borrowed(s) // No allocation needed - string fits!
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ascii_truncation() {
        // "Hello World" = 11 cols, max 8 -> need truncation
        // target_width = 8 - 3 = 5, "Hello" = 5 cols -> "Hello..."
        assert_eq!(truncate_to_width("Hello World", 8), "Hello...");
        // "Short" = 5 cols fits in max 10
        assert_eq!(truncate_to_width("Short", 10), "Short");
    }

    #[test]
    fn test_cjk_truncation() {
        // CJK characters are typically 2 columns wide
        // "你好世界" = 8 cols, max 7 -> need truncation
        // target_width = 7 - 3 = 4, "你好" = 4 cols -> "你好..."
        assert_eq!(truncate_to_width("你好世界", 7), "你好...");
        // "你好" = 4 cols fits in max 10
        assert_eq!(truncate_to_width("你好", 10), "你好");
        // Edge case: max 5 -> target 2, only "你" fits -> "你..."
        assert_eq!(truncate_to_width("你好世界", 5), "你...");
    }

    #[test]
    fn test_emoji_truncation() {
        // "Hello 🎉 World" = 6 + 2 + 1 + 5 = 14 cols (emoji is 2 wide)
        // max 12 -> target 9, "Hello 🎉 " = 9 cols -> "Hello 🎉 ..."
        assert_eq!(truncate_to_width("Hello 🎉 World", 12), "Hello 🎉 ...");
        // max 11 -> target 8, "Hello 🎉" = 8 cols -> "Hello 🎉..."
        assert_eq!(truncate_to_width("Hello 🎉 World", 11), "Hello 🎉...");
    }

    #[test]
    fn test_min_width() {
        // At minimum width 4: "Test" = 4 cols fits, no truncation
        assert_eq!(truncate_to_width("Test", 4), "Test");
        // "Testing" = 7 cols, max 4 -> target 1, "T" = 1 -> "T..."
        assert_eq!(truncate_to_width("Testing", 4), "T...");
    }

    #[test]
    fn test_edge_case_widths() {
        // Width 0: always empty
        assert_eq!(truncate_to_width("Test", 0), "");
        assert_eq!(truncate_to_width("", 0), "");

        // Width 1: one character if it fits
        assert_eq!(truncate_to_width("Test", 1), "T");
        assert_eq!(truncate_to_width("X", 1), "X");
        // CJK char is 2 columns, doesn't fit in width 1
        assert_eq!(truncate_to_width("你好", 1), "");

        // Width 2: up to 2 columns
        assert_eq!(truncate_to_width("Test", 2), "Te");
        assert_eq!(truncate_to_width("AB", 2), "AB");
        // CJK char fits exactly
        assert_eq!(truncate_to_width("你好", 2), "你");

        // Width 3: up to 3 columns (exactly ellipsis width, but we return chars without ellipsis)
        assert_eq!(truncate_to_width("Test", 3), "Tes");
        assert_eq!(truncate_to_width("Hi", 3), "Hi");
        // CJK: "你" is 2 cols, fits with 1 col to spare (but "好" doesn't fit)
        assert_eq!(truncate_to_width("你好", 3), "你");
    }

    #[test]
    fn test_exact_fit() {
        // "12345" = 5 cols fits exactly in max 5
        assert_eq!(truncate_to_width("12345", 5), "12345");
    }

    #[test]
    fn test_no_panic_on_utf8_boundaries() {
        // Ensure we never panic on multi-byte characters
        let cjk = "日本語テスト";
        let result = truncate_to_width(cjk, 6);
        // Should not panic, and result should be valid UTF-8
        assert!(result.is_ascii() || !result.is_empty() || result.chars().count() > 0);

        // Mixed content
        let mixed = "Hello世界";
        let result = truncate_to_width(mixed, 8);
        assert!(!result.is_empty());
    }
}
