use std::fmt::Write;

use anyhow::Result;
use feed_rs::parser;
use sha2::{Digest, Sha256};

use crate::util::validate_url;

/// A parsed article extracted from an RSS or Atom feed entry.
///
/// This struct represents the normalized form of a feed entry, abstracting
/// over the differences between RSS and Atom formats.
#[derive(Debug, Clone)]
pub struct ParsedArticle {
    /// Unique identifier for the article. Either the original GUID/ID from
    /// the feed, or a generated SHA-256 hash if none was provided.
    pub guid: String,
    /// Article title. Falls back to "Untitled" if not present in the feed.
    pub title: String,
    /// URL to the full article content, if available.
    pub url: Option<String>,
    /// Publication timestamp as Unix epoch seconds.
    /// Derived from `pubDate` (RSS) or `published`/`updated` (Atom).
    pub published: Option<i64>,
    /// Article summary or description text, if available.
    /// Falls back to content body if no explicit summary is provided.
    pub summary: Option<String>,
}

/// Result of parsing a feed, including both successfully parsed articles
/// and a count of items that were skipped due to validation failures.
#[derive(Debug)]
pub struct ParseResult {
    /// Successfully parsed articles.
    pub articles: Vec<ParsedArticle>,
    /// Number of feed entries that were skipped due to invalid URLs or other issues.
    pub skipped: usize,
}

/// Parses RSS or Atom feed XML into a list of articles with best-effort recovery.
///
/// This function accepts raw XML bytes and automatically detects whether
/// the feed is RSS or Atom format using the `feed-rs` crate. It implements
/// partial recovery, continuing to process valid items even when some entries
/// are malformed or have invalid URLs.
///
/// # Arguments
///
/// * `bytes` - Raw XML content of the feed as a byte slice
///
/// # Returns
///
/// A [`ParseResult`] containing:
/// - `articles`: Successfully parsed [`ParsedArticle`] structs
/// - `skipped`: Count of entries that were skipped due to validation failures
///
/// Returns an empty articles vector if the feed contains no entries or all
/// entries were invalid.
///
/// # Errors
///
/// Returns an error if:
/// - The input is not valid XML
/// - The XML is not a recognized RSS or Atom format
/// - Required feed structure is missing or malformed
pub fn parse_feed(bytes: &[u8]) -> Result<ParseResult> {
    let feed = parser::parse(bytes)?;

    let total_entries = feed.entries.len();
    let mut skipped = 0usize;

    let articles: Vec<ParsedArticle> = feed
        .entries
        .into_iter()
        .filter_map(|entry| {
            // Validate article URL if present, skipping articles with dangerous URLs
            let validated_url =
                entry
                    .links
                    .first()
                    .and_then(|link| match validate_url(&link.href) {
                        Ok(validated) => Some(validated.to_string()),
                        Err(e) => {
                            tracing::debug!(
                                url = %link.href,
                                error = %e,
                                "Skipping article with invalid URL"
                            );
                            None
                        }
                    });

            // If the entry had a link but it was invalid, skip this article entirely
            let has_link = !entry.links.is_empty();
            if has_link && validated_url.is_none() {
                skipped += 1;
                return None;
            }

            let url_ref = validated_url.as_deref();
            let published = entry.published.or(entry.updated).map(|dt| dt.timestamp());
            let summary = entry
                .summary
                .map(|s| s.content)
                .or_else(|| entry.content.and_then(|c| c.body));
            let title = entry
                .title
                .map(|t| t.content)
                .unwrap_or_else(|| "Untitled".to_owned());

            let existing_id = if entry.id.is_empty() {
                None
            } else {
                Some(entry.id.as_str())
            };
            let guid = generate_guid(existing_id, url_ref, &title, published);

            Some(ParsedArticle {
                guid,
                title,
                url: validated_url,
                published,
                summary,
            })
        })
        .collect();

    // BUG-012: Use checked arithmetic for skipped count calculation
    // This handles edge cases where filter_map might skip entries for other reasons
    let calculated_skipped = total_entries.saturating_sub(articles.len());
    if calculated_skipped != skipped {
        tracing::debug!(
            tracked = skipped,
            calculated = calculated_skipped,
            "Skipped count mismatch, using calculated value"
        );
        skipped = calculated_skipped;
    }

    Ok(ParseResult { articles, skipped })
}

/// Generates a unique identifier for an article.
///
/// If the feed provides a non-empty GUID or ID, that value is used directly.
/// Otherwise, generates a deterministic SHA-256 hash from the article's URL,
/// title, and publication timestamp.
///
/// The hash-based fallback ensures that articles without explicit IDs can
/// still be uniquely identified and deduplicated across feed refreshes.
fn generate_guid(
    existing: Option<&str>,
    url: Option<&str>,
    title: &str,
    published: Option<i64>,
) -> String {
    if let Some(guid) = existing {
        let trimmed = guid.trim();
        if !trimmed.is_empty() {
            return trimmed.to_owned();
        }
    }

    // Stream directly to hasher to avoid intermediate string allocation
    let mut hasher = Sha256::new();
    hasher.update(url.unwrap_or("").as_bytes());
    hasher.update(b"|");
    hasher.update(title.as_bytes());
    hasher.update(b"|");
    if let Some(ts) = published {
        hasher.update(ts.to_string().as_bytes());
    }
    let hash = hasher.finalize();

    // Pre-allocate exact size for hex output (64 chars for SHA-256)
    let mut guid = String::with_capacity(64);
    for byte in hash.iter() {
        write!(&mut guid, "{:02x}", byte).unwrap();
    }
    guid
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use proptest::prelude::*;

    const RSS_MINIMAL: &str = r#"<?xml version="1.0"?>
<rss version="2.0">
  <channel>
    <title>Test Feed</title>
    <item>
      <guid>test-guid-1</guid>
      <title>Test Article</title>
      <link>https://example.com/article</link>
      <pubDate>Mon, 01 Jan 2024 12:00:00 GMT</pubDate>
      <description>Test summary</description>
    </item>
  </channel>
</rss>"#;

    const ATOM_MINIMAL: &str = r#"<?xml version="1.0"?>
<feed xmlns="http://www.w3.org/2005/Atom">
  <title>Test Feed</title>
  <entry>
    <id>test-id-1</id>
    <title>Test Article</title>
    <link href="https://example.com/article"/>
    <updated>2024-01-01T12:00:00Z</updated>
    <summary>Test summary</summary>
  </entry>
</feed>"#;

    // Edge case fixtures
    const RSS_NO_TITLE: &str = r#"<?xml version="1.0"?>
<rss version="2.0"><channel><item><guid>1</guid></item></channel></rss>"#;

    const RSS_NO_GUID: &str = r#"<?xml version="1.0"?>
<rss version="2.0"><channel><item>
  <title>No GUID Article</title>
  <link>https://example.com/no-guid</link>
</item></channel></rss>"#;

    #[test]
    fn test_parse_rss_basic() {
        let result = parse_feed(RSS_MINIMAL.as_bytes()).unwrap();
        assert_eq!(result.articles.len(), 1);
        assert_eq!(result.skipped, 0);
        assert_eq!(result.articles[0].guid, "test-guid-1");
        assert_eq!(result.articles[0].title, "Test Article");
        assert_eq!(
            result.articles[0].url,
            Some("https://example.com/article".to_owned())
        );
        assert!(result.articles[0].published.is_some());
    }

    #[test]
    fn test_parse_atom_basic() {
        let result = parse_feed(ATOM_MINIMAL.as_bytes()).unwrap();
        assert_eq!(result.articles.len(), 1);
        assert_eq!(result.skipped, 0);
        assert_eq!(result.articles[0].guid, "test-id-1");
        assert_eq!(result.articles[0].title, "Test Article");
    }

    #[test]
    fn test_parse_empty_feed() {
        let empty_rss = r#"<?xml version="1.0"?><rss version="2.0"><channel></channel></rss>"#;
        let result = parse_feed(empty_rss.as_bytes()).unwrap();
        assert!(result.articles.is_empty());
        assert_eq!(result.skipped, 0);
    }

    // Edge case tests

    #[test]
    fn test_parse_missing_title_defaults_untitled() {
        let result = parse_feed(RSS_NO_TITLE.as_bytes()).unwrap();
        assert_eq!(result.articles[0].title, "Untitled");
    }

    #[test]
    fn test_parse_missing_guid_generates_hash() {
        let result = parse_feed(RSS_NO_GUID.as_bytes()).unwrap();
        assert!(!result.articles[0].guid.is_empty());
        // feed-rs generates an MD5-style hex ID (31-32 chars) when no GUID is present,
        // or our code generates a SHA256 hash (63-64 hex chars) as fallback.
        // Either way, we get a non-empty, deterministic identifier.
        let len = result.articles[0].guid.len();
        assert!((31..=64).contains(&len), "unexpected GUID length: {}", len);
    }

    #[test]
    fn test_guid_deterministic() {
        let a1 = parse_feed(RSS_NO_GUID.as_bytes()).unwrap();
        let a2 = parse_feed(RSS_NO_GUID.as_bytes()).unwrap();
        assert_eq!(a1.articles[0].guid, a2.articles[0].guid);
    }

    #[test]
    fn test_parse_malformed_xml_error() {
        let result = parse_feed(b"<not valid xml");
        assert!(result.is_err());
    }

    // URL validation security tests

    #[test]
    fn test_localhost_url_rejected() {
        let rss = r#"<?xml version="1.0"?>
<rss version="2.0"><channel><item>
    <guid>malicious-1</guid>
    <title>Malicious Article</title>
    <link>http://localhost/admin</link>
</item></channel></rss>"#;
        let result = parse_feed(rss.as_bytes()).unwrap();
        assert!(
            result.articles.is_empty(),
            "Localhost URLs should be rejected"
        );
        assert_eq!(result.skipped, 1);
    }

    #[test]
    fn test_loopback_ip_rejected() {
        let rss = r#"<?xml version="1.0"?>
<rss version="2.0"><channel><item>
    <guid>malicious-2</guid>
    <title>Malicious Article</title>
    <link>http://127.0.0.1/admin</link>
</item></channel></rss>"#;
        let result = parse_feed(rss.as_bytes()).unwrap();
        assert!(
            result.articles.is_empty(),
            "Loopback IP URLs should be rejected"
        );
        assert_eq!(result.skipped, 1);
    }

    #[test]
    fn test_private_ip_10_rejected() {
        let rss = r#"<?xml version="1.0"?>
<rss version="2.0"><channel><item>
    <guid>malicious-3</guid>
    <title>Malicious Article</title>
    <link>http://10.0.0.1/internal</link>
</item></channel></rss>"#;
        let result = parse_feed(rss.as_bytes()).unwrap();
        assert!(
            result.articles.is_empty(),
            "10.x.x.x private IPs should be rejected"
        );
        assert_eq!(result.skipped, 1);
    }

    #[test]
    fn test_private_ip_192_168_rejected() {
        let rss = r#"<?xml version="1.0"?>
<rss version="2.0"><channel><item>
    <guid>malicious-4</guid>
    <title>Malicious Article</title>
    <link>http://192.168.1.1/router</link>
</item></channel></rss>"#;
        let result = parse_feed(rss.as_bytes()).unwrap();
        assert!(
            result.articles.is_empty(),
            "192.168.x.x private IPs should be rejected"
        );
        assert_eq!(result.skipped, 1);
    }

    #[test]
    fn test_private_ip_172_16_rejected() {
        let rss = r#"<?xml version="1.0"?>
<rss version="2.0"><channel><item>
    <guid>malicious-5</guid>
    <title>Malicious Article</title>
    <link>http://172.16.0.1/internal</link>
</item></channel></rss>"#;
        let result = parse_feed(rss.as_bytes()).unwrap();
        assert!(
            result.articles.is_empty(),
            "172.16.x.x private IPs should be rejected"
        );
        assert_eq!(result.skipped, 1);
    }

    #[test]
    fn test_file_scheme_rejected() {
        let rss = r#"<?xml version="1.0"?>
<rss version="2.0"><channel><item>
    <guid>malicious-6</guid>
    <title>Malicious Article</title>
    <link>file:///etc/passwd</link>
</item></channel></rss>"#;
        let result = parse_feed(rss.as_bytes()).unwrap();
        assert!(
            result.articles.is_empty(),
            "file:// scheme should be rejected"
        );
        assert_eq!(result.skipped, 1);
    }

    #[test]
    fn test_ftp_scheme_rejected() {
        let rss = r#"<?xml version="1.0"?>
<rss version="2.0"><channel><item>
    <guid>malicious-7</guid>
    <title>Malicious Article</title>
    <link>ftp://ftp.example.com/file</link>
</item></channel></rss>"#;
        let result = parse_feed(rss.as_bytes()).unwrap();
        assert!(
            result.articles.is_empty(),
            "ftp:// scheme should be rejected"
        );
        assert_eq!(result.skipped, 1);
    }

    #[test]
    fn test_valid_https_url_accepted() {
        let rss = r#"<?xml version="1.0"?>
<rss version="2.0"><channel><item>
    <guid>valid-1</guid>
    <title>Valid Article</title>
    <link>https://example.com/article</link>
</item></channel></rss>"#;
        let result = parse_feed(rss.as_bytes()).unwrap();
        assert_eq!(
            result.articles.len(),
            1,
            "Valid HTTPS URLs should be accepted"
        );
        assert_eq!(result.skipped, 0);
        assert_eq!(
            result.articles[0].url,
            Some("https://example.com/article".to_owned())
        );
    }

    #[test]
    fn test_valid_http_url_accepted() {
        let rss = r#"<?xml version="1.0"?>
<rss version="2.0"><channel><item>
    <guid>valid-2</guid>
    <title>Valid Article</title>
    <link>http://example.com/article</link>
</item></channel></rss>"#;
        let result = parse_feed(rss.as_bytes()).unwrap();
        assert_eq!(
            result.articles.len(),
            1,
            "Valid HTTP URLs should be accepted"
        );
        assert_eq!(result.skipped, 0);
    }

    #[test]
    fn test_article_without_url_included() {
        // Articles without URLs should still be included per requirements
        let rss = r#"<?xml version="1.0"?>
<rss version="2.0"><channel><item>
    <guid>no-url-1</guid>
    <title>Article Without URL</title>
    <description>This article has no link</description>
</item></channel></rss>"#;
        let result = parse_feed(rss.as_bytes()).unwrap();
        assert_eq!(
            result.articles.len(),
            1,
            "Articles without URLs should be included"
        );
        assert_eq!(result.skipped, 0);
        assert!(result.articles[0].url.is_none());
    }

    #[test]
    fn test_mixed_valid_and_invalid_urls() {
        let rss = r#"<?xml version="1.0"?>
<rss version="2.0"><channel>
    <item>
        <guid>valid-1</guid>
        <title>Valid Article</title>
        <link>https://example.com/good</link>
    </item>
    <item>
        <guid>malicious-1</guid>
        <title>Malicious Article</title>
        <link>http://localhost/bad</link>
    </item>
    <item>
        <guid>valid-2</guid>
        <title>Another Valid Article</title>
        <link>https://example.org/also-good</link>
    </item>
</channel></rss>"#;
        let result = parse_feed(rss.as_bytes()).unwrap();
        assert_eq!(
            result.articles.len(),
            2,
            "Only valid articles should be included"
        );
        assert_eq!(
            result.skipped, 1,
            "One article with localhost URL should be skipped"
        );
        assert_eq!(result.articles[0].guid, "valid-1");
        assert_eq!(result.articles[1].guid, "valid-2");
    }

    // Property-based tests

    proptest! {
        #[test]
        fn test_guid_never_empty(
            url in "https://example\\.com/[a-z0-9]{1,20}",
            title in "[a-zA-Z0-9 ]{0,50}"
        ) {
            let xml = format!(r#"<?xml version="1.0"?>
                <rss version="2.0"><channel><item>
                    <title>{}</title><link>{}</link>
                </item></channel></rss>"#, title, url);
            let result = parse_feed(xml.as_bytes()).unwrap();
            // GUIDs are always non-empty and hex-formatted
            prop_assert!(!result.articles[0].guid.is_empty());
            // feed-rs generates MD5-style hex ID (can be 28-32 chars depending on leading zeros)
            // or our SHA256 fallback (64 chars). The key property is that it's non-empty and
            // appears to be a hex string.
            let guid = &result.articles[0].guid;
            prop_assert!(guid.len() >= 24, "GUID too short: {} (len={})", guid, guid.len());
            prop_assert!(guid.chars().all(|c| c.is_ascii_hexdigit()), "GUID not hex: {}", guid);
        }
    }
}
