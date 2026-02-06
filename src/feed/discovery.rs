use crate::util::{strip_control_chars, validate_url};
use futures::StreamExt;
use std::time::Duration;
use thiserror::Error;

const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_DISCOVERY_SIZE: usize = 5 * 1024 * 1024; // 5MB

/// A feed discovered from a URL, containing metadata extracted from the feed XML.
#[derive(Debug, Clone)]
pub struct DiscoveredFeed {
    /// Feed title (e.g., "Hacker News")
    pub title: String,
    /// URL of the RSS/Atom feed itself
    pub feed_url: String,
    /// URL of the associated website, if available
    pub site_url: Option<String>,
    /// Feed description, if available
    pub description: Option<String>,
}

/// Errors that can occur during feed discovery.
#[derive(Debug, Error)]
pub enum DiscoveryError {
    /// The provided URL failed validation (SSRF, bad scheme, etc.)
    #[error("invalid URL: {0}")]
    InvalidUrl(String),
    /// The URL does not point to an RSS/Atom feed and no feed link was found in HTML
    #[error("not a feed: no RSS/Atom content found")]
    NotAFeed,
    /// HTTP request failed
    #[error("network error: {0}")]
    Network(#[from] reqwest::Error),
    /// Request exceeded the 10-second timeout
    #[error("request timed out")]
    Timeout,
    /// Response body exceeded the 5MB size limit
    #[error("response too large")]
    TooLarge,
}

/// Discovers an RSS/Atom feed from a URL.
///
/// Accepts either a direct feed URL or an HTML page URL. For HTML pages,
/// scans for `<link rel="alternate">` tags pointing to RSS/Atom feeds,
/// then fetches and parses the discovered feed URL.
///
/// # Arguments
///
/// * `client` - HTTP client (caller controls configuration)
/// * `url` - URL to discover a feed from
///
/// # Errors
///
/// Returns [`DiscoveryError`] on validation failure, network error, timeout,
/// oversized response, or if no feed could be found at the URL.
pub async fn discover_feed(
    client: &reqwest::Client,
    url: &str,
) -> Result<DiscoveredFeed, DiscoveryError> {
    // Step 1: Validate URL (SSRF prevention)
    let validated = validate_url(url).map_err(|e| DiscoveryError::InvalidUrl(e.to_string()))?;
    let url_str = validated.to_string();

    fetch_and_discover(client, &url_str).await
}

/// Core discovery logic: fetch a pre-validated URL and detect/parse its feed content.
async fn fetch_and_discover(
    client: &reqwest::Client,
    url_str: &str,
) -> Result<DiscoveredFeed, DiscoveryError> {
    // Fetch with timeout and size limit
    let response = tokio::time::timeout(DISCOVERY_TIMEOUT, client.get(url_str).send())
        .await
        .map_err(|_| DiscoveryError::Timeout)?
        .map_err(DiscoveryError::Network)?;

    if !response.status().is_success() {
        return Err(DiscoveryError::Network(
            response.error_for_status().unwrap_err(),
        ));
    }

    // Check Content-Type to decide parsing strategy
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_lowercase();

    let is_xml = content_type.contains("application/rss+xml")
        || content_type.contains("application/atom+xml")
        || content_type.contains("application/xml")
        || content_type.contains("text/xml");

    let is_html = content_type.contains("text/html") || content_type.contains("application/xhtml");

    // Read body with size limit
    let bytes = read_discovery_bytes(response).await?;

    // Parse based on content type
    if is_xml {
        return parse_feed_bytes(&bytes, url_str);
    }

    if is_html {
        return discover_from_html(client, &bytes, url_str).await;
    }

    // Ambiguous or missing Content-Type: try feed first, fallback to HTML scan
    if let Ok(feed) = parse_feed_bytes(&bytes, url_str) {
        return Ok(feed);
    }

    discover_from_html(client, &bytes, url_str).await
}

/// Reads response body with a 5MB size limit using stream-based reading.
async fn read_discovery_bytes(response: reqwest::Response) -> Result<Vec<u8>, DiscoveryError> {
    // Fast path: check Content-Length header
    if let Some(len) = response.content_length() {
        if len as usize > MAX_DISCOVERY_SIZE {
            return Err(DiscoveryError::TooLarge);
        }
    }

    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(DiscoveryError::Network)?;
        if bytes.len().saturating_add(chunk.len()) > MAX_DISCOVERY_SIZE {
            return Err(DiscoveryError::TooLarge);
        }
        bytes.extend_from_slice(&chunk);
    }

    Ok(bytes)
}

/// Parses feed bytes and extracts metadata into a `DiscoveredFeed`.
fn parse_feed_bytes(bytes: &[u8], feed_url: &str) -> Result<DiscoveredFeed, DiscoveryError> {
    let feed = feed_rs::parser::parse(bytes).map_err(|_| DiscoveryError::NotAFeed)?;

    // SEC-016: Sanitize feed metadata to strip control characters (same pattern as SEC-001
    // in ArticleDbRow::into_article) — attacker-controlled feed XML could embed terminal
    // escape sequences that persist into the TUI
    let title = strip_control_chars(
        &feed
            .title
            .map(|t| t.content)
            .unwrap_or_else(|| "Untitled Feed".to_owned()),
    )
    .into_owned();

    let description = feed
        .description
        .map(|d| strip_control_chars(&d.content).into_owned());

    // Extract site URL from feed links (not the feed URL itself)
    let site_url = feed
        .links
        .iter()
        .find(|link| {
            // Prefer links that aren't the feed itself
            link.href != feed_url
        })
        .or_else(|| feed.links.first())
        .map(|link| strip_control_chars(&link.href).into_owned())
        .filter(|href| href != feed_url);

    Ok(DiscoveredFeed {
        title,
        feed_url: feed_url.to_owned(),
        site_url,
        description,
    })
}

/// Scans HTML content for `<link rel="alternate">` tags pointing to RSS/Atom feeds,
/// then fetches and parses the first discovered feed.
async fn discover_from_html(
    client: &reqwest::Client,
    html_bytes: &[u8],
    base_url: &str,
) -> Result<DiscoveredFeed, DiscoveryError> {
    let html = String::from_utf8_lossy(html_bytes);

    let feed_href = find_feed_link_in_html(&html, base_url).ok_or(DiscoveryError::NotAFeed)?;

    // SEC: Validate discovered feed URL before fetching (SSRF prevention)
    validate_url(&feed_href).map_err(|e| DiscoveryError::InvalidUrl(e.to_string()))?;

    // Fetch the discovered feed URL
    let response = tokio::time::timeout(DISCOVERY_TIMEOUT, client.get(&feed_href).send())
        .await
        .map_err(|_| DiscoveryError::Timeout)?
        .map_err(DiscoveryError::Network)?;

    if !response.status().is_success() {
        return Err(DiscoveryError::Network(
            response.error_for_status().unwrap_err(),
        ));
    }

    let bytes = read_discovery_bytes(response).await?;
    let mut discovered = parse_feed_bytes(&bytes, &feed_href)?;

    // Set site_url to the original HTML page if not already set
    if discovered.site_url.is_none() {
        discovered.site_url = Some(base_url.to_owned());
    }

    Ok(discovered)
}

/// Scans HTML for `<link>` tags with `rel="alternate"` and RSS/Atom type attributes.
///
/// Uses simple string scanning (no HTML parser dependency). Handles attribute
/// ordering variations and resolves relative URLs against the base URL.
///
/// Returns the first matching feed URL, or `None` if no feed link is found.
fn find_feed_link_in_html(html: &str, base_url: &str) -> Option<String> {
    let html_lower = html.to_lowercase();
    let mut search_from = 0;

    while let Some(link_start) = html_lower[search_from..].find("<link") {
        let abs_start = search_from + link_start;
        let remaining = &html_lower[abs_start..];

        // Find the end of this <link> tag
        let tag_end = match remaining.find('>') {
            Some(pos) => pos,
            None => break,
        };

        let tag = &remaining[..=tag_end];

        // Must have rel="alternate"
        if contains_attr(tag, "rel", "alternate") && is_feed_type(tag) {
            // Extract href from the original (non-lowered) HTML to preserve URL case
            let original_tag = &html[abs_start..abs_start + tag_end + 1];
            if let Some(href) = extract_attr_value(original_tag, "href") {
                let resolved = resolve_url(href, base_url);
                return Some(resolved);
            }
        }

        search_from = abs_start + tag_end + 1;
    }

    None
}

/// Checks if a lowercased tag contains an attribute with the given value.
fn contains_attr(tag: &str, attr_name: &str, attr_value: &str) -> bool {
    // Match: attr_name="attr_value" or attr_name='attr_value'
    let pattern_double = format!("{attr_name}=\"{attr_value}\"");
    let pattern_single = format!("{attr_name}='{attr_value}'");
    tag.contains(&pattern_double) || tag.contains(&pattern_single)
}

/// Checks if a lowercased `<link>` tag has an RSS or Atom feed type.
fn is_feed_type(tag: &str) -> bool {
    tag.contains("application/rss+xml") || tag.contains("application/atom+xml")
}

/// Extracts the value of an attribute from a tag string (case-preserving).
fn extract_attr_value<'a>(tag: &'a str, attr_name: &str) -> Option<&'a str> {
    let tag_lower = tag.to_lowercase();
    let attr_prefix = format!("{attr_name}=");

    let attr_start = tag_lower.find(&attr_prefix)?;
    let value_start = attr_start + attr_prefix.len();

    if value_start >= tag.len() {
        return None;
    }

    let rest = &tag[value_start..];
    let quote = rest.as_bytes().first()?;

    if *quote != b'"' && *quote != b'\'' {
        return None;
    }

    let quote_char = *quote as char;
    let inner = &rest[1..];
    let end = inner.find(quote_char)?;

    Some(&inner[..end])
}

/// Resolves a potentially relative URL against a base URL.
fn resolve_url(href: &str, base_url: &str) -> String {
    // Already absolute
    if href.starts_with("http://") || href.starts_with("https://") {
        return href.to_owned();
    }

    // SEC-014: Protocol-relative — use URL parser to normalize and prevent credential injection
    if href.starts_with("//") {
        let with_scheme = format!("https:{}", href);
        if let Ok(parsed) = url::Url::parse(&with_scheme) {
            return parsed.to_string();
        }
    }

    // Relative URL: resolve against base
    if let Ok(base) = url::Url::parse(base_url) {
        if let Ok(resolved) = base.join(href) {
            return resolved.to_string();
        }
    }

    // Fallback: return as-is
    href.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Unit tests for parsing functions (no network) ---

    const RSS_WITH_METADATA: &str = r#"<?xml version="1.0"?>
<rss version="2.0">
  <channel>
    <title>Example Blog</title>
    <link>https://example.com</link>
    <description>An example blog about things</description>
    <item>
      <guid>1</guid>
      <title>First Post</title>
      <link>https://example.com/post/1</link>
    </item>
  </channel>
</rss>"#;

    const ATOM_WITH_METADATA: &str = r#"<?xml version="1.0"?>
<feed xmlns="http://www.w3.org/2005/Atom">
  <title>Example Blog</title>
  <link href="https://example.com" rel="alternate"/>
  <link href="https://example.com/feed.xml" rel="self"/>
  <subtitle>An example blog about things</subtitle>
  <entry>
    <id>1</id>
    <title>First Post</title>
    <link href="https://example.com/post/1"/>
    <updated>2024-01-01T00:00:00Z</updated>
  </entry>
</feed>"#;

    #[test]
    fn test_parse_rss_metadata() {
        let feed =
            parse_feed_bytes(RSS_WITH_METADATA.as_bytes(), "https://example.com/feed.xml").unwrap();
        assert_eq!(feed.title, "Example Blog");
        assert_eq!(feed.feed_url, "https://example.com/feed.xml");
        // feed-rs normalizes "https://example.com" to "https://example.com/"
        assert!(feed.site_url.is_some());
        assert!(feed
            .site_url
            .as_deref()
            .unwrap()
            .starts_with("https://example.com"));
        assert_eq!(
            feed.description.as_deref(),
            Some("An example blog about things")
        );
    }

    #[test]
    fn test_parse_atom_metadata() {
        let feed = parse_feed_bytes(
            ATOM_WITH_METADATA.as_bytes(),
            "https://example.com/feed.xml",
        )
        .unwrap();
        assert_eq!(feed.title, "Example Blog");
        assert_eq!(feed.feed_url, "https://example.com/feed.xml");
        // site_url should be the alternate link, not the self link
        assert_eq!(feed.site_url.as_deref(), Some("https://example.com/"));
    }

    #[test]
    fn test_parse_invalid_xml_returns_not_a_feed() {
        let result = parse_feed_bytes(b"<html><body>Hello</body></html>", "https://example.com");
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), DiscoveryError::NotAFeed));
    }

    // SEC-016: Control character sanitization in parsed feed metadata
    #[test]
    fn test_parse_feed_strips_control_chars_from_title() {
        let rss = "<?xml version=\"1.0\"?>\n<rss version=\"2.0\"><channel>\
            <title>Evil\x1b[31m Feed</title>\
            <item><guid>1</guid><title>Post</title></item>\
            </channel></rss>";
        let feed = parse_feed_bytes(rss.as_bytes(), "https://example.com/feed").unwrap();
        assert!(!feed.title.contains('\x1b'));
        assert!(feed.title.contains("Evil"));
        assert!(feed.title.contains("Feed"));
    }

    #[test]
    fn test_parse_feed_strips_control_chars_from_description() {
        let rss = "<?xml version=\"1.0\"?>\n<rss version=\"2.0\"><channel>\
            <title>Feed</title>\
            <description>About\x07 things</description>\
            <item><guid>1</guid><title>Post</title></item>\
            </channel></rss>";
        let feed = parse_feed_bytes(rss.as_bytes(), "https://example.com/feed").unwrap();
        let desc = feed.description.unwrap();
        assert!(!desc.contains('\x07'));
        assert!(desc.contains("About"));
    }

    #[test]
    fn test_parse_feed_without_title_defaults() {
        let rss = r#"<?xml version="1.0"?>
<rss version="2.0"><channel>
  <item><guid>1</guid><title>Post</title></item>
</channel></rss>"#;
        let feed = parse_feed_bytes(rss.as_bytes(), "https://example.com/feed").unwrap();
        assert_eq!(feed.title, "Untitled Feed");
    }

    // --- HTML link discovery tests ---

    #[test]
    fn test_find_rss_link_in_html() {
        let html = r#"<html><head>
            <link rel="alternate" type="application/rss+xml" href="/feed.xml" title="RSS">
        </head><body></body></html>"#;
        let result = find_feed_link_in_html(html, "https://example.com");
        assert_eq!(result, Some("https://example.com/feed.xml".to_owned()));
    }

    #[test]
    fn test_find_atom_link_in_html() {
        let html = r#"<html><head>
            <link rel="alternate" type="application/atom+xml" href="https://example.com/atom.xml">
        </head><body></body></html>"#;
        let result = find_feed_link_in_html(html, "https://example.com");
        assert_eq!(result, Some("https://example.com/atom.xml".to_owned()));
    }

    #[test]
    fn test_find_feed_link_reversed_attrs() {
        let html = r#"<html><head>
            <link href="/feed.xml" type="application/rss+xml" rel="alternate">
        </head><body></body></html>"#;
        let result = find_feed_link_in_html(html, "https://example.com");
        assert_eq!(result, Some("https://example.com/feed.xml".to_owned()));
    }

    #[test]
    fn test_find_feed_link_single_quotes() {
        let html = r#"<html><head>
            <link rel='alternate' type='application/rss+xml' href='/rss'>
        </head><body></body></html>"#;
        let result = find_feed_link_in_html(html, "https://example.com");
        assert_eq!(result, Some("https://example.com/rss".to_owned()));
    }

    #[test]
    fn test_no_feed_link_in_html() {
        let html = r#"<html><head>
            <link rel="stylesheet" href="/style.css">
        </head><body></body></html>"#;
        let result = find_feed_link_in_html(html, "https://example.com");
        assert_eq!(result, None);
    }

    #[test]
    fn test_find_feed_link_protocol_relative() {
        let html = r#"<html><head>
            <link rel="alternate" type="application/rss+xml" href="//cdn.example.com/feed.xml">
        </head><body></body></html>"#;
        let result = find_feed_link_in_html(html, "https://example.com");
        assert_eq!(result, Some("https://cdn.example.com/feed.xml".to_owned()));
    }

    #[test]
    fn test_find_feed_link_absolute_url() {
        let html = r#"<html><head>
            <link rel="alternate" type="application/rss+xml" href="https://feeds.example.com/rss">
        </head><body></body></html>"#;
        let result = find_feed_link_in_html(html, "https://example.com");
        assert_eq!(result, Some("https://feeds.example.com/rss".to_owned()));
    }

    // --- URL resolution tests ---

    #[test]
    fn test_resolve_absolute_url() {
        assert_eq!(
            resolve_url("https://other.com/feed", "https://example.com"),
            "https://other.com/feed"
        );
    }

    #[test]
    fn test_resolve_relative_url() {
        assert_eq!(
            resolve_url("/feed.xml", "https://example.com/page"),
            "https://example.com/feed.xml"
        );
    }

    #[test]
    fn test_resolve_protocol_relative() {
        assert_eq!(
            resolve_url("//cdn.example.com/feed", "https://example.com"),
            "https://cdn.example.com/feed"
        );
    }

    // SEC-014: Protocol-relative URL parser normalization
    #[test]
    fn test_resolve_protocol_relative_uses_parser() {
        // Resolved URL must be a valid, parseable URL (no raw string formatting)
        let resolved = resolve_url("//user:pass@evil.com/feed", "https://example.com");
        let parsed = url::Url::parse(&resolved).unwrap();
        // URL parser produces a well-formed URL — downstream validate_url() (SEC-015)
        // will reject the userinfo before it is fetched
        assert_eq!(parsed.scheme(), "https");
        assert_eq!(parsed.host_str(), Some("evil.com"));
    }

    #[test]
    fn test_resolve_protocol_relative_normalizes_path() {
        // URL parser normalizes the path, preventing path traversal
        let resolved = resolve_url("//evil.com/../../../etc/passwd", "https://example.com");
        let parsed = url::Url::parse(&resolved).unwrap();
        assert_eq!(parsed.host_str(), Some("evil.com"));
        // Path is normalized by the URL parser
        assert!(!parsed.path().contains(".."));
    }

    #[test]
    fn test_resolve_relative_path() {
        assert_eq!(
            resolve_url("feed.xml", "https://example.com/blog/"),
            "https://example.com/blog/feed.xml"
        );
    }

    // --- Validation tests ---

    #[tokio::test]
    async fn test_discover_invalid_url() {
        let client = reqwest::Client::new();
        let result = discover_feed(&client, "not a url").await;
        assert!(matches!(result, Err(DiscoveryError::InvalidUrl(_))));
    }

    #[tokio::test]
    async fn test_discover_localhost_rejected() {
        let client = reqwest::Client::new();
        let result = discover_feed(&client, "http://localhost/feed").await;
        assert!(matches!(result, Err(DiscoveryError::InvalidUrl(_))));
    }

    #[tokio::test]
    async fn test_discover_private_ip_rejected() {
        let client = reqwest::Client::new();
        let result = discover_feed(&client, "http://192.168.1.1/feed").await;
        assert!(matches!(result, Err(DiscoveryError::InvalidUrl(_))));
    }

    // --- Integration tests with wiremock ---
    // These use fetch_and_discover (internal) to bypass SSRF check on localhost mock server.

    #[tokio::test]
    async fn test_discover_direct_rss() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(RSS_WITH_METADATA)
                    .insert_header("Content-Type", "application/rss+xml"),
            )
            .mount(&mock_server)
            .await;

        let client = reqwest::Client::new();
        let url = format!("{}/feed.xml", mock_server.uri());
        let feed = fetch_and_discover(&client, &url).await.unwrap();

        assert_eq!(feed.title, "Example Blog");
        assert_eq!(feed.feed_url, url);
        assert!(feed.site_url.is_some());
        assert_eq!(
            feed.description.as_deref(),
            Some("An example blog about things")
        );
    }

    #[test]
    fn test_discover_from_html_finds_and_parses_feed() {
        // Tests the HTML discovery pipeline: find link in HTML, then parse feed bytes.
        // Network round-trip tested via test_discover_direct_rss; SSRF validation
        // in discover_from_html prevents wiremock localhost from working end-to-end.
        let html = r#"<html><head>
            <link rel="alternate" type="application/rss+xml" href="https://example.com/feed.xml">
        </head><body><h1>My Blog</h1></body></html>"#;

        // Step 1: HTML scanning finds the feed URL
        let feed_url =
            find_feed_link_in_html(html, "https://example.com").expect("should find feed link");
        assert_eq!(feed_url, "https://example.com/feed.xml");

        // Step 2: Parsing the feed bytes extracts metadata
        let feed = parse_feed_bytes(RSS_WITH_METADATA.as_bytes(), &feed_url).unwrap();
        assert_eq!(feed.title, "Example Blog");
        assert_eq!(feed.feed_url, "https://example.com/feed.xml");
        assert!(feed.site_url.is_some());
        assert_eq!(
            feed.description.as_deref(),
            Some("An example blog about things")
        );
    }

    #[tokio::test]
    async fn test_discover_not_a_feed() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string("<html><body>Just a page</body></html>")
                    .insert_header("Content-Type", "text/html"),
            )
            .mount(&mock_server)
            .await;

        let client = reqwest::Client::new();
        let result = fetch_and_discover(&client, &format!("{}/page", mock_server.uri())).await;

        assert!(matches!(result, Err(DiscoveryError::NotAFeed)));
    }

    #[tokio::test]
    async fn test_discover_ambiguous_content_type_tries_feed_first() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock_server = MockServer::start().await;

        // Server returns RSS but with no Content-Type
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(RSS_WITH_METADATA), // No Content-Type header
            )
            .mount(&mock_server)
            .await;

        let client = reqwest::Client::new();
        let url = format!("{}/feed", mock_server.uri());
        let feed = fetch_and_discover(&client, &url).await.unwrap();

        assert_eq!(feed.title, "Example Blog");
    }
}
