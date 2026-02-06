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
/// B-7: Zero-width characters (combining marks, ZWJ) have display width 0, so they
/// are always preserved without truncation. Strings consisting entirely of zero-width
/// characters will always pass the "fits within width" check and return borrowed.
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

/// SEC-001: Strip terminal control characters and ANSI escape sequences from text.
///
/// Removes characters that could manipulate terminal behavior when rendering
/// user-controlled text (feed titles, article content, etc.) from RSS feeds.
///
/// Strips:
/// - ASCII control chars: 0x00-0x08, 0x0B-0x0C, 0x0E-0x1F, 0x7F
/// - ANSI CSI sequences: `\x1b[` ... (terminal byte 0x40-0x7E)
/// - ANSI OSC sequences: `\x1b]` ... (until BEL 0x07 or ST `\x1b\\`)
/// - Bare ESC (0x1b) not followed by `[` or `]`
///
/// Preserves: tab (0x09), newline (0x0A), carriage return (0x0D).
///
/// Returns `Cow::Borrowed` when the input contains no control characters (common case).
///
/// P-9: The fast path (byte scan via `Iterator::any`) makes repeated calls on already-clean
/// content essentially free — a single pass over bytes with no allocation or string building.
pub fn strip_control_chars(s: &str) -> Cow<'_, str> {
    let bytes = s.as_bytes();
    let len = bytes.len();

    // Fast path: scan for any byte that needs stripping
    let needs_strip = bytes
        .iter()
        .any(|&b| b == 0x1b || b == 0x7f || (b < 0x20 && b != 0x09 && b != 0x0a && b != 0x0d));

    if !needs_strip {
        return Cow::Borrowed(s);
    }

    let mut out = String::with_capacity(len);
    let mut i = 0;

    while i < len {
        let b = bytes[i];

        if b == 0x1b {
            // ESC byte — check what follows
            if i + 1 < len && bytes[i + 1] == b'[' {
                // CSI sequence: skip \x1b[ then parameter/intermediate bytes until final byte
                i += 2;
                while i < len {
                    let c = bytes[i];
                    i += 1;
                    if (0x40..=0x7e).contains(&c) {
                        break; // final byte consumed
                    }
                }
            } else if i + 1 < len && bytes[i + 1] == b']' {
                // OSC sequence: skip \x1b] then everything until BEL or ST (\x1b\\)
                i += 2;
                while i < len {
                    if bytes[i] == 0x07 {
                        i += 1; // consume BEL
                        break;
                    }
                    if bytes[i] == 0x1b && i + 1 < len && bytes[i + 1] == b'\\' {
                        i += 2; // consume ST
                        break;
                    }
                    i += 1;
                }
            } else {
                // Bare ESC — skip it
                i += 1;
            }
        } else if b == 0x7f || (b < 0x20 && b != 0x09 && b != 0x0a && b != 0x0d) {
            // Control character (not tab/newline/CR) — skip
            i += 1;
        } else {
            // Safe byte — find the run of safe bytes to batch-copy
            let start = i;
            i += 1;
            while i < len {
                let nb = bytes[i];
                if nb == 0x1b || nb == 0x7f || (nb < 0x20 && nb != 0x09 && nb != 0x0a && nb != 0x0d)
                {
                    break;
                }
                i += 1;
            }
            // SAFETY: we only break on ASCII control bytes, which cannot appear
            // mid-codepoint in valid UTF-8, so s[start..i] is valid UTF-8.
            out.push_str(&s[start..i]);
        }
    }

    Cow::Owned(out)
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

    // ========================================================================
    // strip_control_chars tests
    // ========================================================================

    #[test]
    fn test_strip_clean_text_returns_borrowed() {
        let input = "Hello, world! This is clean text.";
        let result = strip_control_chars(input);
        assert!(matches!(result, Cow::Borrowed(_)));
        assert_eq!(result, input);
    }

    #[test]
    fn test_strip_preserves_tabs_newlines_cr() {
        let input = "line1\nline2\ttabbed\r\nwindows";
        let result = strip_control_chars(input);
        assert!(matches!(result, Cow::Borrowed(_)));
        assert_eq!(result, input);
    }

    #[test]
    fn test_strip_control_chars_removes_controls() {
        // NUL, BEL, BS, VT, FF, and other C0 controls
        let input = "he\x00ll\x07o\x08 w\x0bor\x0cld\x01!";
        let result = strip_control_chars(input);
        assert!(matches!(result, Cow::Owned(_)));
        assert_eq!(result, "hello world!");
    }

    #[test]
    fn test_strip_removes_del() {
        let input = "delete\x7fme";
        let result = strip_control_chars(input);
        assert_eq!(result, "deleteme");
    }

    #[test]
    fn test_strip_ansi_color_codes() {
        // CSI SGR: \x1b[31m (red) and \x1b[0m (reset)
        let input = "\x1b[31mRed text\x1b[0m";
        let result = strip_control_chars(input);
        assert_eq!(result, "Red text");
    }

    #[test]
    fn test_strip_ansi_cursor_movement() {
        // CSI cursor up: \x1b[2A
        let input = "before\x1b[2Aafter";
        let result = strip_control_chars(input);
        assert_eq!(result, "beforeafter");
    }

    #[test]
    fn test_strip_osc_with_bel() {
        // OSC set window title: \x1b]0;title\x07
        let input = "\x1b]0;malicious title\x07safe text";
        let result = strip_control_chars(input);
        assert_eq!(result, "safe text");
    }

    #[test]
    fn test_strip_osc_with_st() {
        // OSC with ST terminator: \x1b]0;title\x1b\\
        let input = "\x1b]0;malicious title\x1b\\safe text";
        let result = strip_control_chars(input);
        assert_eq!(result, "safe text");
    }

    #[test]
    fn test_strip_bare_esc() {
        let input = "before\x1bafter";
        let result = strip_control_chars(input);
        assert_eq!(result, "beforeafter");
    }

    #[test]
    fn test_strip_mixed_content() {
        // Mix of ANSI, OSC, control chars, and normal text
        let input = "\x1b[31mRed\x1b[0m \x00NUL \x1b]0;title\x07 \x08BS normal";
        let result = strip_control_chars(input);
        assert_eq!(result, "Red NUL  BS normal");
    }

    #[test]
    fn test_strip_empty_string() {
        let result = strip_control_chars("");
        assert!(matches!(result, Cow::Borrowed(_)));
        assert_eq!(result, "");
    }

    #[test]
    fn test_strip_unicode_preserved() {
        let input = "日本語 \x1b[31m赤い\x1b[0m テキスト";
        let result = strip_control_chars(input);
        assert_eq!(result, "日本語 赤い テキスト");
    }
}
