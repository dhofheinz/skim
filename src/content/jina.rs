use crate::util::validate_url;
use futures::StreamExt;
use secrecy::{ExposeSecret, SecretString};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use thiserror::Error;

static JINA_API_KEY: OnceLock<Option<SecretString>> = OnceLock::new();

fn get_jina_api_key() -> Option<&'static SecretString> {
    JINA_API_KEY
        .get_or_init(|| std::env::var("JINA_API_KEY").ok().map(SecretString::from))
        .as_ref()
}

const MAX_CONTENT_SIZE: usize = 5 * 1024 * 1024; // 5MB

static LAST_REQUEST_MS: AtomicU64 = AtomicU64::new(0);
const MIN_REQUEST_INTERVAL_MS: u64 = 100; // 10 requests/sec max
const MAX_COLLISIONS: u64 = 20; // Safety valve for rate limiter collision loop

// BUG-012: Use monotonic clock for rate limiting to prevent issues with
// system time jumps (NTP corrections, VM resume, daylight saving, etc.)
static START_INSTANT: OnceLock<Instant> = OnceLock::new();

/// Returns monotonic milliseconds since process start.
/// Unlike SystemTime, this never goes backward regardless of clock adjustments.
fn monotonic_ms() -> u64 {
    let start = START_INSTANT.get_or_init(Instant::now);
    start.elapsed().as_millis() as u64
}

/// CSS selectors targeting main article content across common blog platforms.
/// Order matters: more specific selectors first, generic fallbacks last.
const TARGET_SELECTORS: &str =
    "article, .entry-content, .post-content, .article-content, .post-body, main .content, main";

/// Minimum content length (in bytes) to consider a fetch successful.
/// If X-Target-Selector returns less than this, we retry without it.
/// Set to 200 to account for metadata lines (Title, URL Source, etc.)
const MIN_CONTENT_LEN: usize = 200;

#[derive(Debug, Error)]
pub enum ContentError {
    #[error("Request timed out after 20s")]
    Timeout,
    #[error("Network error: {0}")]
    Network(#[from] reqwest::Error),
    #[error("HTTP error: status {0}")]
    HttpStatus(u16),
    #[error("Response too large (exceeds {0} bytes)")]
    ResponseTooLarge(usize),
    #[error("Invalid UTF-8 in response")]
    InvalidUtf8,
    #[error("Invalid URL")]
    InvalidUrl,
    #[error("Insecure base URL: HTTPS required (except localhost for testing)")]
    InsecureBaseUrl,
}

impl ContentError {
    /// Returns true if this error is transient and the request should be retried.
    fn is_retryable(&self) -> bool {
        match self {
            ContentError::Timeout | ContentError::Network(_) => true,
            ContentError::HttpStatus(status) => *status >= 500,
            ContentError::ResponseTooLarge(_)
            | ContentError::InvalidUtf8
            | ContentError::InvalidUrl
            | ContentError::InsecureBaseUrl => false,
        }
    }
}

pub async fn fetch_content(
    client: &reqwest::Client,
    url: &str,
    base_url: Option<&str>,
) -> Result<String, ContentError> {
    // Rate limiting: 10 requests/sec max
    // Use compare_exchange to atomically claim a time slot, avoiding TOCTOU race
    // BUG-012: Use monotonic clock to avoid issues with system time changes
    // BUG-013: Add timeout budget to prevent excessive waiting in high-concurrency scenarios
    let mut collision_count: u64 = 0;
    let rate_limit_start = Instant::now();
    const RATE_LIMIT_TIMEOUT: Duration = Duration::from_secs(5);
    loop {
        // BUG-013: Check total time budget first to prevent excessive wait in pathological cases
        if rate_limit_start.elapsed() > RATE_LIMIT_TIMEOUT {
            tracing::debug!(
                elapsed_ms = rate_limit_start.elapsed().as_millis(),
                collisions = collision_count,
                "Rate limiter timeout budget exceeded, proceeding"
            );
            break;
        }

        let now = monotonic_ms();
        let last = LAST_REQUEST_MS.load(Ordering::Acquire);
        let next_allowed = last.saturating_add(MIN_REQUEST_INTERVAL_MS);

        if now >= next_allowed {
            // Try to claim this slot atomically
            match LAST_REQUEST_MS.compare_exchange(last, now, Ordering::Release, Ordering::Relaxed)
            {
                Ok(_) => break, // Successfully claimed the slot
                Err(_) => {
                    // Another thread won, apply backoff to avoid busy-waiting
                    collision_count += 1;

                    // Safety valve: if too many collisions, proceed anyway
                    if collision_count >= MAX_COLLISIONS {
                        tracing::warn!(
                            collisions = collision_count,
                            "Rate limiter max collisions reached, proceeding without slot"
                        );
                        break;
                    }

                    if collision_count > 1 {
                        // Exponential backoff: 100us, 200us, 400us, 800us... (capped at 6.4ms)
                        let backoff_us = 100 * (1u64 << collision_count.min(6));
                        tokio::time::sleep(Duration::from_micros(backoff_us)).await;
                    }
                    continue;
                }
            }
        } else {
            // Need to wait before we can claim a slot
            // BUG-011 fix: Ensure minimum 1ms sleep to prevent spin-wait on time drift
            let wait_ms = next_allowed.saturating_sub(now).max(1);
            tokio::time::sleep(Duration::from_millis(wait_ms)).await;
        }
    }

    // SEC-001: Validate URL before use to prevent SSRF attacks
    let parsed_url = validate_url(url).map_err(|_| ContentError::InvalidUrl)?;

    let base = base_url.unwrap_or("https://r.jina.ai");

    // SEC-002: Enforce HTTPS for base URL to prevent API key exposure
    // Allow HTTP only for localhost/127.0.0.1 (testing purposes)
    if !base.starts_with("https://") {
        let is_localhost =
            base.starts_with("http://127.0.0.1") || base.starts_with("http://localhost");
        if !is_localhost {
            tracing::error!(base_url = %base, "Rejecting non-HTTPS base URL (HTTPS required except for localhost)");
            return Err(ContentError::InsecureBaseUrl);
        }
        tracing::warn!(base_url = %base, "Using non-HTTPS Jina base URL (localhost only)");
    }

    if base_url.is_some() {
        tracing::info!(base_url = %base, "Using custom Jina API base URL");
    }

    let jina_url = format!("{}/{}", base, parsed_url.as_str());

    // First attempt: with X-Target-Selector for cleaner extraction
    let content = fetch_with_retry(client, &jina_url, true).await?;

    // If selector returned too little content, retry without it
    // (some sites don't use standard article/main containers)
    if content.len() < MIN_CONTENT_LEN {
        tracing::debug!(
            content_len = content.len(),
            "Target selector returned minimal content, retrying without selector"
        );
        let content = fetch_with_retry(client, &jina_url, false).await?;
        return Ok(strip_boilerplate(&content));
    }

    Ok(strip_boilerplate(&content))
}

/// Fetch content from jina.ai with retry logic for transient failures.
/// Uses exponential backoff: 1s, 2s, 4s (max 3 retries).
async fn fetch_with_retry(
    client: &reqwest::Client,
    jina_url: &str,
    use_selector: bool,
) -> Result<String, ContentError> {
    const MAX_RETRIES: u32 = 3;
    let mut retry_count = 0;

    loop {
        match fetch_with_selector(client, jina_url, use_selector).await {
            Ok(content) => return Ok(content),
            Err(e) if e.is_retryable() && retry_count < MAX_RETRIES => {
                let delay = 1u64 << retry_count; // 1s, 2s, 4s
                tracing::debug!(
                    error = %e,
                    retry = retry_count + 1,
                    delay_secs = delay,
                    "Retrying jina fetch after transient error"
                );
                tokio::time::sleep(Duration::from_secs(delay)).await;
                retry_count += 1;
            }
            Err(e) => return Err(e),
        }
    }
}

/// Fetch content from jina.ai, optionally using X-Target-Selector.
async fn fetch_with_selector(
    client: &reqwest::Client,
    jina_url: &str,
    use_selector: bool,
) -> Result<String, ContentError> {
    let mut request = client.get(jina_url);

    if use_selector {
        request = request.header("X-Target-Selector", TARGET_SELECTORS);
    }

    // SEC-002: Only send API key to official Jina domain to prevent credential leakage
    // Custom base URLs (used for testing) don't receive the API key
    let is_official_jina =
        jina_url.starts_with("https://r.jina.ai/") || jina_url.starts_with("https://api.jina.ai/");
    if let Some(key) = get_jina_api_key() {
        if is_official_jina {
            tracing::trace!("Jina API authentication configured");
            request = request.header("Authorization", format!("Bearer {}", key.expose_secret()));
        } else {
            tracing::debug!("Skipping API key for non-official Jina URL (custom base_url in use)");
        }
    }

    let response = tokio::time::timeout(Duration::from_secs(20), request.send())
        .await
        .map_err(|_| ContentError::Timeout)?
        .map_err(ContentError::Network)?;

    if !response.status().is_success() {
        return Err(ContentError::HttpStatus(response.status().as_u16()));
    }

    read_limited_text(response, MAX_CONTENT_SIZE).await
}

/// Strip common boilerplate patterns that jina.ai doesn't filter.
///
/// Patterns targeted:
/// - "Skip to content" navigation links
/// - Comment section scaffolding (Loading Comments, form fields)
/// - WordPress "Powered by" footers
/// - Consecutive archive link lists (Month Year patterns)
fn strip_boilerplate(content: &str) -> String {
    let mut lines: Vec<&str> = content.lines().collect();

    // Pass 1: Remove individual cruft lines
    lines.retain(|line| {
        let trimmed = line.trim();

        // Skip to content links
        if trimmed.starts_with("[Skip to content]") {
            return false;
        }

        // Comment scaffolding
        if trimmed == "Loading Comments..."
            || trimmed == "Write a Comment..."
            || trimmed.starts_with("Email (Required)")
            || trimmed == "%d"
        {
            return false;
        }

        // WordPress footer
        if trimmed.contains("Proudly powered by WordPress") {
            return false;
        }

        // Standalone "Menu" text (navigation remnant)
        if trimmed == "Menu" {
            return false;
        }

        true
    });

    // Pass 2: Remove consecutive archive link runs (3+ in a row)
    // Pattern: "*   [Month Year](url)"
    let mut result = Vec::with_capacity(lines.len());
    let mut archive_run_start: Option<usize> = None;
    let mut archive_run_len = 0;

    for line in lines.iter() {
        if is_archive_link(line) {
            if archive_run_start.is_none() {
                archive_run_start = Some(result.len());
            }
            archive_run_len += 1;
            result.push(*line);
        } else {
            // End of potential archive run
            if archive_run_len >= 3 {
                // Remove the archive run
                if let Some(start) = archive_run_start {
                    result.truncate(start);
                }
            }
            archive_run_start = None;
            archive_run_len = 0;
            result.push(*line);
        }
    }

    // Handle trailing archive run
    if archive_run_len >= 3 {
        if let Some(start) = archive_run_start {
            result.truncate(start);
        }
    }

    result.join("\n")
}

/// Static patterns for month detection in archive links
const MONTH_PATTERNS: &[&str] = &[
    "[January",
    "[February",
    "[March",
    "[April",
    "[May",
    "[June",
    "[July",
    "[August",
    "[September",
    "[October",
    "[November",
    "[December",
];

/// Check if a line matches the archive link pattern: "*   [Month Year](url)"
fn is_archive_link(line: &str) -> bool {
    let trimmed = line.trim();
    if !trimmed.starts_with('*') {
        return false;
    }

    for pattern in MONTH_PATTERNS {
        if let Some(idx) = trimmed.find(pattern) {
            let after = &trimmed[idx + pattern.len()..];
            // BUG-003 fix: Use safe slicing to prevent panic on UTF-8 boundary or short strings
            if after
                .get(1..5)
                .is_some_and(|year_part| year_part.chars().all(|c| c.is_ascii_digit()))
            {
                return true;
            }
        }
    }
    false
}

async fn read_limited_text(
    response: reqwest::Response,
    limit: usize,
) -> Result<String, ContentError> {
    // Fast path: check Content-Length header
    if let Some(len) = response.content_length() {
        if len as usize > limit {
            return Err(ContentError::ResponseTooLarge(limit));
        }
    }

    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(ContentError::Network)?;
        // SEC-003: Use saturating_add to prevent integer overflow in size check
        if bytes.len().saturating_add(chunk.len()) > limit {
            return Err(ContentError::ResponseTooLarge(limit));
        }
        bytes.extend_from_slice(&chunk);
    }

    String::from_utf8(bytes).map_err(|_| ContentError::InvalidUtf8)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn test_fetch_content_success() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_regex(".*"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string("# Article Content\n\nHello world"),
            )
            .mount(&mock_server)
            .await;

        let client = reqwest::Client::new();
        let result = fetch_content(
            &client,
            "https://example.com/article",
            Some(&mock_server.uri()),
        )
        .await;

        assert!(result.is_ok());
        assert!(result.unwrap().contains("Article Content"));
    }

    #[tokio::test]
    async fn test_invalid_url_rejected() {
        let client = reqwest::Client::new();
        let result = fetch_content(&client, "not-a-valid-url", None).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_localhost_rejected() {
        let client = reqwest::Client::new();
        let result = fetch_content(&client, "http://localhost/article", None).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_private_ip_rejected() {
        let client = reqwest::Client::new();

        let result = fetch_content(&client, "http://192.168.1.1/article", None).await;
        assert!(result.is_err());

        let result = fetch_content(&client, "http://10.0.0.1/article", None).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_http_404() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&mock_server)
            .await;

        let client = reqwest::Client::new();
        let result = fetch_content(
            &client,
            "https://example.com/article",
            Some(&mock_server.uri()),
        )
        .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_http_500() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&mock_server)
            .await;

        let client = reqwest::Client::new();
        let result = fetch_content(
            &client,
            "https://example.com/article",
            Some(&mock_server.uri()),
        )
        .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_empty_response() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string(""))
            .mount(&mock_server)
            .await;

        let client = reqwest::Client::new();
        let result = fetch_content(
            &client,
            "https://example.com/article",
            Some(&mock_server.uri()),
        )
        .await;

        // Should not panic, result may be Ok or Err depending on implementation
        let _ = result;
    }

    #[tokio::test]
    async fn test_http_base_url_rejected() {
        let client = reqwest::Client::new();
        // Non-localhost HTTP base URL should be rejected
        let result = fetch_content(
            &client,
            "https://example.com/article",
            Some("http://evil.com"),
        )
        .await;

        assert!(matches!(result, Err(ContentError::InsecureBaseUrl)));
    }

    #[tokio::test]
    async fn test_localhost_base_url_allowed() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_regex(".*"))
            .respond_with(ResponseTemplate::new(200).set_body_string("content"))
            .mount(&mock_server)
            .await;

        let client = reqwest::Client::new();
        // Localhost HTTP should be allowed for testing
        let result = fetch_content(
            &client,
            "https://example.com/article",
            Some(&mock_server.uri()),
        )
        .await;

        // MockServer uses 127.0.0.1, which should be allowed
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_https_base_url_allowed() {
        let client = reqwest::Client::new();
        // HTTPS base URL should be allowed (will fail at network level, but not URL validation)
        let result = fetch_content(
            &client,
            "https://example.com/article",
            Some("https://custom-jina.example.com"),
        )
        .await;

        // Should fail with network error, not InsecureBaseUrl
        assert!(!matches!(result, Err(ContentError::InsecureBaseUrl)));
    }

    #[test]
    fn test_strip_skip_to_content() {
        let input =
            "[Skip to content](https://example.com/#content)\n\n# Article Title\n\nContent here.";
        let result = strip_boilerplate(input);
        assert!(!result.contains("Skip to content"));
        assert!(result.contains("Article Title"));
        assert!(result.contains("Content here"));
    }

    #[test]
    fn test_strip_comment_scaffolding() {
        let input = "# Article\n\nContent\n\nLoading Comments...\n\nWrite a Comment...\n\nEmail (Required) Name Website";
        let result = strip_boilerplate(input);
        assert!(!result.contains("Loading Comments"));
        assert!(!result.contains("Write a Comment"));
        assert!(!result.contains("Email (Required)"));
        assert!(result.contains("Article"));
        assert!(result.contains("Content"));
    }

    #[test]
    fn test_strip_wordpress_footer() {
        let input = "Article content\n\nProudly powered by WordPress\n\nMore stuff";
        let result = strip_boilerplate(input);
        assert!(!result.contains("Proudly powered by WordPress"));
        assert!(result.contains("Article content"));
    }

    #[test]
    fn test_strip_menu_remnant() {
        let input = "Menu\n\n# Article Title\n\nContent";
        let result = strip_boilerplate(input);
        // Should only strip standalone "Menu", not "Menu" within other text
        assert!(!result.starts_with("Menu\n"));
        assert!(result.contains("Article Title"));
    }

    #[test]
    fn test_strip_archive_links() {
        let input = "Article content\n\n*   [January 2024](https://example.com/2024/01/)\n*   [February 2024](https://example.com/2024/02/)\n*   [March 2024](https://example.com/2024/03/)\n*   [April 2024](https://example.com/2024/04/)";
        let result = strip_boilerplate(input);
        // 4 consecutive archive links should be stripped
        assert!(!result.contains("January 2024"));
        assert!(!result.contains("February 2024"));
        assert!(result.contains("Article content"));
    }

    #[test]
    fn test_preserve_short_archive_list() {
        let input = "Related posts:\n\n*   [January 2024](https://example.com/2024/01/)\n*   [February 2024](https://example.com/2024/02/)\n\nMore content";
        let result = strip_boilerplate(input);
        // Only 2 archive links - should be preserved
        assert!(result.contains("January 2024"));
        assert!(result.contains("February 2024"));
    }

    #[test]
    fn test_preserve_legitimate_content() {
        let input = "# My Article\n\nThis is about **January 2024** events.\n\n*   First point\n*   Second point\n\nConclusion.";
        let result = strip_boilerplate(input);
        // Should preserve all content - month mention isn't a link list
        assert_eq!(input, result);
    }

    #[test]
    fn test_is_archive_link() {
        assert!(is_archive_link("*   [January 2024](https://example.com/)"));
        assert!(is_archive_link("*   [December 2023](https://example.com/)"));
        assert!(is_archive_link("  *   [March 2025](https://example.com/)"));

        // Not archive links
        assert!(!is_archive_link("*   [Some Article](https://example.com/)"));
        assert!(!is_archive_link("January 2024")); // No bullet
        assert!(!is_archive_link("*   January 2024")); // No link brackets
    }
}
