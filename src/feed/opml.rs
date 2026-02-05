use anyhow::{Context, Result};
use quick_xml::events::Event;
use quick_xml::Reader;
use thiserror::Error;

use crate::util::validate_url;

/// SEC-003: Maximum allowed nesting depth for OPML outline elements.
/// Prevents stack overflow attacks from maliciously crafted deeply nested OPMLs.
const MAX_OPML_DEPTH: usize = 50;

/// Errors that can occur during OPML parsing.
#[derive(Debug, Error)]
pub enum OpmlError {
    /// SEC-003: OPML nesting depth exceeds safety limit.
    #[error("OPML nesting depth exceeds maximum of {0} levels")]
    MaxDepthExceeded(usize),

    /// XML parsing failed.
    #[error("XML parse error: {0}")]
    XmlParse(String),

    /// File I/O error.
    #[error("Failed to read OPML file: {0}")]
    Io(#[from] std::io::Error),
}

/// A feed subscription extracted from an OPML file.
///
/// Represents a single `<outline>` element with an `xmlUrl` attribute,
/// typically used for RSS/Atom feed subscriptions.
#[derive(Debug, Clone)]
pub struct OpmlFeed {
    /// Display title for the feed. Sourced from `title` attribute,
    /// falling back to `text` attribute, then to the XML URL itself.
    pub title: String,
    /// URL of the RSS/Atom feed XML. Validated to be HTTP(S) and not
    /// pointing to localhost or private IP ranges.
    pub xml_url: String,
    /// URL of the feed's website, if provided via `htmlUrl` attribute.
    pub html_url: Option<String>,
}

/// Parses an OPML file from disk and extracts feed subscriptions.
///
/// Reads the file at the given path and parses it as OPML format,
/// extracting all outline elements that have an `xmlUrl` attribute.
///
/// # Arguments
///
/// * `path` - Filesystem path to the OPML file
///
/// # Returns
///
/// A `Vec` of [`OpmlFeed`] structs representing the feed subscriptions.
/// Feeds with invalid URLs (localhost, private IPs, non-HTTP schemes)
/// are silently skipped with a warning log.
///
/// # Errors
///
/// Returns an error if:
/// - The file cannot be read
/// - The content is not valid XML
/// - XML parsing fails
///
/// # Security
///
/// - XXE (XML External Entity) attacks are mitigated by `quick-xml`'s default
///   configuration which disables entity expansion
/// - URLs are validated to prevent SSRF attacks against localhost and private networks
pub async fn parse(path: &str) -> Result<Vec<OpmlFeed>> {
    let content = tokio::fs::read_to_string(path)
        .await
        .with_context(|| format!("Failed to read OPML file: {}", path))?;
    parse_opml_content(&content)
}

/// Parses OPML content string and extracts feed subscriptions.
///
/// Internal implementation shared by `parse()`. Handles both nested and
/// flat OPML structures, extracting feeds from any `<outline>` element
/// with an `xmlUrl` attribute regardless of nesting depth.
///
/// Category/folder outlines (those without `xmlUrl`) are traversed but
/// not returned in the result.
fn parse_opml_content(content: &str) -> Result<Vec<OpmlFeed>> {
    // SEC-002: quick-xml has XXE protection enabled by default (entity expansion disabled).
    // Custom entity definitions in DOCTYPE are not expanded, preventing XXE attacks.
    let mut reader = Reader::from_str(content);
    reader.config_mut().trim_text(true);

    let mut feeds = Vec::new();
    let mut buf = Vec::new();
    // SEC-003: Track nesting depth to prevent stack overflow from malicious OPMLs
    let mut depth: usize = 0;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) if e.name().as_ref() == b"outline" => {
                depth += 1;
                // SEC-003: Reject excessively nested OPMLs
                if depth > MAX_OPML_DEPTH {
                    return Err(OpmlError::MaxDepthExceeded(MAX_OPML_DEPTH).into());
                }

                if let Some(feed) = parse_outline_attributes(&e, &reader)? {
                    feeds.push(feed);
                }
            }
            Ok(Event::Empty(e)) if e.name().as_ref() == b"outline" => {
                // Self-closing outline doesn't affect depth
                if let Some(feed) = parse_outline_attributes(&e, &reader)? {
                    feeds.push(feed);
                }
            }
            Ok(Event::End(e)) if e.name().as_ref() == b"outline" => {
                depth = depth.saturating_sub(1);
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(OpmlError::XmlParse(e.to_string()).into()),
            _ => {}
        }
        buf.clear();
    }

    Ok(feeds)
}

/// Extracts feed attributes from an outline element.
///
/// Returns `Some(OpmlFeed)` if the outline has a valid `xmlUrl` attribute,
/// `None` for category/folder outlines without feed URLs.
fn parse_outline_attributes(
    e: &quick_xml::events::BytesStart<'_>,
    reader: &Reader<&[u8]>,
) -> Result<Option<OpmlFeed>> {
    let mut xml_url = None;
    let mut html_url = None;
    let mut title = None;

    for attr_result in e.attributes() {
        let attr = match attr_result {
            Ok(attr) => attr,
            Err(e) => {
                tracing::warn!(error = %e, "Skipping malformed OPML attribute");
                continue;
            }
        };
        let decoder = reader.decoder();
        match attr.key.as_ref() {
            b"xmlUrl" => xml_url = Some(attr.decode_and_unescape_value(decoder)?.to_string()),
            b"htmlUrl" => {
                let url_str = attr.decode_and_unescape_value(decoder)?;
                // SEC: Validate HTML URL before accepting (same as xmlUrl)
                match validate_url(&url_str) {
                    Ok(_) => html_url = Some(url_str.to_string()),
                    Err(e) => {
                        tracing::warn!(url = %url_str, error = %e, "Ignoring invalid htmlUrl in OPML");
                    }
                }
            }
            b"title" => title = Some(attr.decode_and_unescape_value(decoder)?.to_string()),
            b"text" => {
                if title.is_none() {
                    title = Some(attr.decode_and_unescape_value(decoder)?.to_string())
                }
            }
            _ => {}
        }
    }

    if let Some(url) = xml_url {
        // SEC-002: Validate URL before accepting
        match validate_url(&url) {
            Ok(_) => Ok(Some(OpmlFeed {
                title: title.unwrap_or_else(|| url.clone()),
                xml_url: url,
                html_url,
            })),
            Err(e) => {
                tracing::warn!(url = %url, error = %e, "Skipping invalid feed URL");
                Ok(None)
            }
        }
    } else {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_opml_content() {
        let content = r#"<?xml version="1.0" encoding="UTF-8"?>
<opml version="2.0">
  <head><title>Test Feeds</title></head>
  <body>
    <outline text="Blogs" title="Blogs">
      <outline type="rss" text="Example Blog" title="Example Blog" xmlUrl="https://example.com/feed.xml" htmlUrl="https://example.com"/>
      <outline type="rss" text="No HTML" title="No HTML" xmlUrl="https://nohtml.com/rss"/>
    </outline>
  </body>
</opml>"#;

        let feeds =
            parse_opml_content(content).expect("Failed to parse OPML content with nested outlines");
        assert_eq!(feeds.len(), 2);

        assert_eq!(feeds[0].title, "Example Blog");
        assert_eq!(feeds[0].xml_url, "https://example.com/feed.xml");
        assert_eq!(feeds[0].html_url, Some("https://example.com".to_string()));

        assert_eq!(feeds[1].title, "No HTML");
        assert_eq!(feeds[1].xml_url, "https://nohtml.com/rss");
        assert_eq!(feeds[1].html_url, None);
    }

    #[test]
    fn test_fallback_to_text() {
        let content = r#"<?xml version="1.0"?>
<opml version="2.0">
  <body>
    <outline type="rss" text="Text Only" xmlUrl="https://textonly.com/feed"/>
  </body>
</opml>"#;

        let feeds =
            parse_opml_content(content).expect("Failed to parse OPML with text-only attribute");
        assert_eq!(feeds.len(), 1);
        assert_eq!(feeds[0].title, "Text Only");
    }

    #[test]
    fn test_fallback_to_url() {
        let content = r#"<?xml version="1.0"?>
<opml version="2.0">
  <body>
    <outline type="rss" xmlUrl="https://notitle.com/feed"/>
  </body>
</opml>"#;

        let feeds =
            parse_opml_content(content).expect("Failed to parse OPML with URL-only outline");
        assert_eq!(feeds.len(), 1);
        assert_eq!(feeds[0].title, "https://notitle.com/feed");
    }

    #[test]
    fn test_skip_private_ip_feeds() {
        let content = r#"<?xml version="1.0"?>
    <opml version="2.0"><body>
        <outline xmlUrl="https://valid.com/feed"/>
        <outline xmlUrl="http://192.168.1.1/feed"/>
        <outline xmlUrl="http://10.0.0.1/feed"/>
    </body></opml>"#;

        let feeds = parse_opml_content(content).unwrap();
        assert_eq!(feeds.len(), 1); // Only valid.com included
        assert_eq!(feeds[0].xml_url, "https://valid.com/feed");
    }

    #[test]
    fn test_skip_localhost_feeds() {
        let content = r#"<?xml version="1.0"?>
    <opml version="2.0"><body>
        <outline xmlUrl="https://valid.com/feed"/>
        <outline xmlUrl="http://localhost/feed"/>
        <outline xmlUrl="http://127.0.0.1/feed"/>
    </body></opml>"#;

        let feeds = parse_opml_content(content).unwrap();
        assert_eq!(feeds.len(), 1);
        assert_eq!(feeds[0].xml_url, "https://valid.com/feed");
    }

    #[test]
    fn test_skip_invalid_scheme_feeds() {
        let content = r#"<?xml version="1.0"?>
    <opml version="2.0"><body>
        <outline xmlUrl="https://valid.com/feed"/>
        <outline xmlUrl="file:///etc/passwd"/>
        <outline xmlUrl="ftp://internal.server/feed"/>
    </body></opml>"#;

        let feeds = parse_opml_content(content).unwrap();
        assert_eq!(feeds.len(), 1);
        assert_eq!(feeds[0].xml_url, "https://valid.com/feed");
    }

    #[test]
    fn test_empty_opml() {
        let content = r#"<?xml version="1.0"?>
    <opml version="2.0"><body></body></opml>"#;

        let feeds = parse_opml_content(content).unwrap();
        assert!(feeds.is_empty());
    }

    #[test]
    fn test_malformed_xml_error() {
        let content = "<not valid xml";
        let result = parse_opml_content(content);
        assert!(result.is_err());
    }

    #[test]
    fn test_xxe_protection() {
        // SEC-002: This XXE payload should NOT expand to file contents.
        // quick-xml does not expand custom entity definitions by default.
        let malicious_opml = r#"<?xml version="1.0"?>
<!DOCTYPE opml [<!ENTITY xxe SYSTEM "file:///etc/passwd">]>
<opml version="2.0">
    <body>
        <outline text="&xxe;" xmlUrl="https://example.com/feed.xml"/>
    </body>
</opml>"#;

        let result = parse_opml_content(malicious_opml);
        // Should either error OR have literal text (not file contents)
        match result {
            Ok(feeds) => {
                // If it parses, the text should NOT contain actual /etc/passwd content
                for feed in &feeds {
                    assert!(
                        !feed.title.contains("root:"),
                        "XXE expansion detected! Feed title contains passwd content"
                    );
                    assert!(
                        !feed.title.contains("/bin/"),
                        "XXE expansion detected! Feed title contains passwd content"
                    );
                }
            }
            Err(_) => {
                // Rejection of XXE payload is also acceptable behavior
            }
        }
    }

    #[test]
    fn test_deeply_nested_opml_rejected() {
        // SEC-003: Create OPML with 100+ nested outlines (exceeds MAX_OPML_DEPTH of 50)
        let mut opml = String::from(r#"<?xml version="1.0"?><opml version="2.0"><body>"#);
        for _ in 0..100 {
            opml.push_str(r#"<outline text="level">"#);
        }
        for _ in 0..100 {
            opml.push_str("</outline>");
        }
        opml.push_str("</body></opml>");

        let result = parse_opml_content(&opml);
        assert!(result.is_err(), "Deeply nested OPML should be rejected");

        let err = result.unwrap_err();
        let err_msg = err.to_string();
        assert!(
            err_msg.contains("depth") && err_msg.contains("50"),
            "Error should mention depth limit: {}",
            err_msg
        );
    }

    #[test]
    fn test_nesting_at_depth_limit_allowed() {
        // Create OPML with exactly MAX_OPML_DEPTH (50) nested outlines - should be allowed
        let mut opml = String::from(r#"<?xml version="1.0"?><opml version="2.0"><body>"#);
        for _ in 0..50 {
            opml.push_str(r#"<outline text="level">"#);
        }
        // Add a feed at the deepest level
        opml.push_str(r#"<outline text="Deep Feed" xmlUrl="https://deep.example.com/feed"/>"#);
        for _ in 0..50 {
            opml.push_str("</outline>");
        }
        opml.push_str("</body></opml>");

        let result = parse_opml_content(&opml);
        assert!(
            result.is_ok(),
            "OPML at exactly max depth should be allowed: {:?}",
            result.err()
        );
        let feeds = result.unwrap();
        assert_eq!(feeds.len(), 1);
        assert_eq!(feeds[0].title, "Deep Feed");
    }
}
