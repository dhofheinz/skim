use std::net::IpAddr;
use thiserror::Error;
use url::Url;

/// Errors that can occur during URL validation.
///
/// These errors cover both parsing failures and security policy violations
/// designed to prevent SSRF (Server-Side Request Forgery) attacks.
#[derive(Error, Debug)]
pub enum UrlValidationError {
    /// The URL string could not be parsed.
    #[error("Invalid URL: {0}")]
    InvalidUrl(#[from] url::ParseError),
    /// The URL uses a scheme other than http or https.
    #[error("Unsupported scheme: {0} (only http/https allowed)")]
    UnsupportedScheme(String),
    /// The URL points to a private/internal IP address.
    #[error("Private IP address not allowed: {0}")]
    PrivateIp(String),
    /// The URL points to localhost.
    #[error("Localhost not allowed")]
    Localhost,
}

/// Validates a URL string for use as a feed source.
///
/// Performs security-focused validation to prevent SSRF attacks by rejecting:
/// - Non-HTTP(S) schemes (e.g., `file://`, `ftp://`)
/// - Localhost addresses (`localhost`, `127.0.0.1`, `::1`)
/// - Private IP ranges (RFC 1918, link-local, unique local IPv6)
///
/// # Arguments
///
/// * `url_str` - The URL string to validate
///
/// # Returns
///
/// The parsed and validated [`Url`] on success.
///
/// # Errors
///
/// Returns [`UrlValidationError`] if:
/// - The URL cannot be parsed ([`UrlValidationError::InvalidUrl`])
/// - The scheme is not `http` or `https` ([`UrlValidationError::UnsupportedScheme`])
/// - The host is localhost ([`UrlValidationError::Localhost`])
/// - The host is a private IP address ([`UrlValidationError::PrivateIp`])
///
/// # Examples
///
/// ```
/// use skim::util::validate_url;
///
/// // Valid public URL
/// let url = validate_url("https://example.com/feed.xml").unwrap();
/// assert_eq!(url.host_str(), Some("example.com"));
///
/// // Rejects localhost
/// assert!(validate_url("http://localhost/feed").is_err());
///
/// // Rejects private IPs
/// assert!(validate_url("http://192.168.1.1/feed").is_err());
///
/// // Rejects non-HTTP schemes
/// assert!(validate_url("file:///etc/passwd").is_err());
/// ```
pub fn validate_url(url_str: &str) -> Result<Url, UrlValidationError> {
    let url = Url::parse(url_str)?;

    match url.scheme() {
        "http" | "https" => {}
        scheme => return Err(UrlValidationError::UnsupportedScheme(scheme.to_owned())),
    }

    if let Some(host) = url.host_str() {
        if host == "localhost" {
            return Err(UrlValidationError::Localhost);
        }

        // Strip brackets from IPv6 addresses for parsing
        let host_for_parse = host
            .strip_prefix('[')
            .and_then(|h| h.strip_suffix(']'))
            .unwrap_or(host);

        if let Ok(ip) = host_for_parse.parse::<IpAddr>() {
            if ip.is_loopback() {
                return Err(UrlValidationError::Localhost);
            }
            if is_private_ip(&ip) {
                return Err(UrlValidationError::PrivateIp(ip.to_string()));
            }
        }
    }

    Ok(url)
}

/// Validates a URL before passing to `open::that()` to prevent command injection.
///
/// Performs SSRF checks via [`validate_url`] first, then applies shell-safety checks
/// against control characters, encoded newlines, and dangerous shell metacharacters.
///
/// # Security
///
/// This function guards against:
/// - All SSRF vectors (via `validate_url`: private IPs, localhost, non-HTTP schemes)
/// - Control characters (ASCII 0-31, DEL) that could manipulate shell behavior
/// - Unicode line/paragraph separators (U+2028, U+2029) that could act as newlines
/// - Dangerous shell metacharacters (backtick, $, ;, |, <, >, backslash, etc.) that could enable command injection
/// - Percent-encoded CR/LF (`%0A`, `%0D`) to prevent newline injection
///
/// Note: Valid URL characters like `&`, `?`, `=`, `#` are allowed since they're common in query strings.
///
/// # Returns
///
/// - `Ok(())` if the URL is safe to open
/// - `Err(&'static str)` with a user-friendly error message otherwise
pub fn validate_url_for_open(url_str: &str) -> Result<(), &'static str> {
    // Check for control characters (ASCII 0-31 and DEL 127)
    if url_str.bytes().any(|b| b < 32 || b == 127) {
        return Err("URL contains invalid control characters");
    }

    // Block Unicode line/paragraph separators that could act as newlines in shell contexts
    if url_str.chars().any(|c| c == '\u{2028}' || c == '\u{2029}') {
        return Err("URL contains Unicode line separator characters");
    }

    // SEC-003: Block percent-encoded CR/LF (%0A, %0D) to prevent command injection.
    // On Linux, open::that() uses xdg-open which shells through /bin/sh.
    // If encoded newlines are decoded before shell execution, they could inject commands.
    if url_str.contains("%0A")
        || url_str.contains("%0a")
        || url_str.contains("%0D")
        || url_str.contains("%0d")
    {
        return Err("URL contains encoded control characters");
    }

    // Ensure URL uses http or https scheme (checked before validate_url to preserve
    // the specific error message for non-http schemes)
    if !url_str.starts_with("http://") && !url_str.starts_with("https://") {
        return Err("URL must use http or https scheme");
    }

    // Reject particularly dangerous shell metacharacters
    // Note: & is valid in query strings, so we only block the most dangerous ones
    const DANGEROUS_CHARS: &[char] = &['`', '$', ';', '|', '<', '>', '(', ')', '{', '}', '\\'];
    if url_str.chars().any(|c| DANGEROUS_CHARS.contains(&c)) {
        return Err("URL contains potentially unsafe characters");
    }

    // SSRF checks via validate_url(): localhost, private IPs, plus URL format validation.
    // Scheme is already verified above, so remaining errors are parse failures or SSRF blocks.
    validate_url(url_str).map_err(|e| match e {
        UrlValidationError::InvalidUrl(_) => "Invalid URL format",
        UrlValidationError::Localhost | UrlValidationError::PrivateIp(_) => {
            "URL points to a restricted address"
        }
        UrlValidationError::UnsupportedScheme(_) => {
            // Should not reach here since scheme is checked above
            "URL must use http or https scheme"
        }
    })?;

    Ok(())
}

fn is_private_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(ipv4) => {
            ipv4.is_private() || ipv4.is_loopback() || ipv4.is_link_local() || ipv4.is_unspecified()
        }
        IpAddr::V6(ipv6) => {
            if ipv6.is_loopback() || ipv6.is_unspecified() {
                return true;
            }
            // SEC-003: Check IPv4-mapped IPv6 addresses (e.g. ::ffff:127.0.0.1)
            // to prevent SSRF bypass via IPv4-mapped notation
            if let Some(mapped_v4) = ipv6.to_ipv4_mapped() {
                return is_private_ip(&IpAddr::V4(mapped_v4));
            }
            let segments = ipv6.segments();
            // Unique Local (fc00::/7)
            let is_unique_local = (segments[0] & 0xfe00) == 0xfc00;
            // Link-Local (fe80::/10)
            let is_link_local = (segments[0] & 0xffc0) == 0xfe80;
            is_unique_local || is_link_local
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_valid_urls() {
        assert!(validate_url("https://example.com/feed.xml").is_ok());
        assert!(validate_url("http://news.example.org").is_ok());
    }

    #[test]
    fn test_invalid_schemes() {
        assert!(validate_url("file:///etc/passwd").is_err());
        assert!(validate_url("ftp://example.com").is_err());
    }

    #[test]
    fn test_localhost_rejected() {
        assert!(validate_url("http://localhost/feed").is_err());
        assert!(validate_url("http://127.0.0.1/feed").is_err());
    }

    #[test]
    fn test_private_ips_rejected() {
        assert!(validate_url("http://192.168.1.1/feed").is_err());
        assert!(validate_url("http://10.0.0.1/feed").is_err());
        assert!(validate_url("http://172.16.0.1/feed").is_err());
    }

    #[test]
    fn test_ipv6_loopback_rejected() {
        let result = validate_url("http://[::1]/feed");
        assert!(result.is_err());
    }

    #[test]
    fn test_link_local_ipv4_rejected() {
        let result = validate_url("http://169.254.1.1/feed");
        assert!(result.is_err());
    }

    #[test]
    fn test_link_local_ipv6_rejected() {
        let result = validate_url("http://[fe80::1]/feed");
        assert!(result.is_err());
    }

    #[test]
    fn test_zero_address_rejected() {
        let result = validate_url("http://0.0.0.0/feed");
        assert!(result.is_err());
    }

    #[test]
    fn test_url_with_port_on_private_ip() {
        let result = validate_url("http://192.168.1.1:8080/feed");
        assert!(result.is_err());

        let result = validate_url("http://10.0.0.1:3000/feed");
        assert!(result.is_err());
    }

    #[test]
    fn test_valid_public_url_accepted() {
        let result = validate_url("https://example.com/feed.xml");
        assert!(result.is_ok());
    }

    #[test]
    fn test_valid_url_with_port_accepted() {
        let result = validate_url("https://example.com:443/feed.xml");
        assert!(result.is_ok());
    }

    #[test]
    fn test_ipv4_mapped_ipv6_loopback_rejected() {
        let result = validate_url("http://[::ffff:127.0.0.1]/feed");
        assert!(result.is_err());
    }

    #[test]
    fn test_ipv4_mapped_ipv6_private_rejected() {
        assert!(validate_url("http://[::ffff:192.168.1.1]/feed").is_err());
        assert!(validate_url("http://[::ffff:10.0.0.1]/feed").is_err());
    }

    // --- validate_url_for_open tests ---

    // Valid URLs

    #[test]
    fn test_validate_url_for_open_valid_https() {
        assert!(validate_url_for_open("https://example.com/article").is_ok());
    }

    #[test]
    fn test_validate_url_for_open_valid_http() {
        assert!(validate_url_for_open("http://example.com/page").is_ok());
    }

    #[test]
    fn test_validate_url_for_open_valid_with_query_params() {
        assert!(validate_url_for_open("https://example.com/search?q=rust&page=1").is_ok());
    }

    #[test]
    fn test_validate_url_for_open_valid_with_fragment() {
        assert!(validate_url_for_open("https://example.com/article#section-2").is_ok());
    }

    #[test]
    fn test_validate_url_for_open_valid_with_path() {
        assert!(validate_url_for_open("https://example.com/a/b/c/d.html").is_ok());
    }

    // Control character rejection

    #[test]
    fn test_validate_url_for_open_rejects_null_byte() {
        let result = validate_url_for_open("https://example.com/\x00bad");
        assert_eq!(result, Err("URL contains invalid control characters"));
    }

    #[test]
    fn test_validate_url_for_open_rejects_raw_newline() {
        let result = validate_url_for_open("https://example.com/\nbad");
        assert_eq!(result, Err("URL contains invalid control characters"));
    }

    #[test]
    fn test_validate_url_for_open_rejects_raw_carriage_return() {
        let result = validate_url_for_open("https://example.com/\rbad");
        assert_eq!(result, Err("URL contains invalid control characters"));
    }

    #[test]
    fn test_validate_url_for_open_rejects_del() {
        let result = validate_url_for_open("https://example.com/\x7Fbad");
        assert_eq!(result, Err("URL contains invalid control characters"));
    }

    // Encoded newline rejection (SEC-003)

    #[test]
    fn test_validate_url_for_open_rejects_encoded_lf_uppercase() {
        let result = validate_url_for_open("https://example.com/%0Aevil");
        assert_eq!(result, Err("URL contains encoded control characters"));
    }

    #[test]
    fn test_validate_url_for_open_rejects_encoded_lf_lowercase() {
        let result = validate_url_for_open("https://example.com/%0aevil");
        assert_eq!(result, Err("URL contains encoded control characters"));
    }

    #[test]
    fn test_validate_url_for_open_rejects_encoded_cr_uppercase() {
        let result = validate_url_for_open("https://example.com/%0Devil");
        assert_eq!(result, Err("URL contains encoded control characters"));
    }

    #[test]
    fn test_validate_url_for_open_rejects_encoded_cr_lowercase() {
        let result = validate_url_for_open("https://example.com/%0devil");
        assert_eq!(result, Err("URL contains encoded control characters"));
    }

    #[test]
    fn test_validate_url_for_open_rejects_encoded_crlf() {
        let result = validate_url_for_open("https://example.com/%0D%0Aevil");
        assert_eq!(result, Err("URL contains encoded control characters"));
    }

    // Scheme rejection

    #[test]
    fn test_validate_url_for_open_rejects_javascript_scheme() {
        let result = validate_url_for_open("javascript:alert(1)");
        assert_eq!(result, Err("URL must use http or https scheme"));
    }

    #[test]
    fn test_validate_url_for_open_rejects_file_scheme() {
        let result = validate_url_for_open("file:///etc/passwd");
        assert_eq!(result, Err("URL must use http or https scheme"));
    }

    #[test]
    fn test_validate_url_for_open_rejects_data_scheme() {
        let result = validate_url_for_open("data:text/html,<h1>Hi</h1>");
        assert_eq!(result, Err("URL must use http or https scheme"));
    }

    // Dangerous character rejection

    #[test]
    fn test_validate_url_for_open_rejects_backtick() {
        let result = validate_url_for_open("https://example.com/`whoami`");
        assert_eq!(result, Err("URL contains potentially unsafe characters"));
    }

    #[test]
    fn test_validate_url_for_open_rejects_dollar() {
        let result = validate_url_for_open("https://example.com/$HOME");
        assert_eq!(result, Err("URL contains potentially unsafe characters"));
    }

    #[test]
    fn test_validate_url_for_open_rejects_semicolon() {
        let result = validate_url_for_open("https://example.com/;rm -rf /");
        assert_eq!(result, Err("URL contains potentially unsafe characters"));
    }

    #[test]
    fn test_validate_url_for_open_rejects_pipe() {
        let result = validate_url_for_open("https://example.com/|cat /etc/passwd");
        assert_eq!(result, Err("URL contains potentially unsafe characters"));
    }

    // Invalid URL format

    #[test]
    fn test_validate_url_for_open_rejects_malformed() {
        let result = validate_url_for_open("https://");
        assert_eq!(result, Err("Invalid URL format"));
    }

    // Unicode separator rejection

    #[test]
    fn test_validate_url_for_open_rejects_unicode_line_separator() {
        let url = "https://example.com/\u{2028}evil".to_string();
        let result = validate_url_for_open(&url);
        assert_eq!(
            result,
            Err("URL contains Unicode line separator characters")
        );
    }

    #[test]
    fn test_validate_url_for_open_rejects_unicode_paragraph_separator() {
        let url = "https://example.com/\u{2029}evil".to_string();
        let result = validate_url_for_open(&url);
        assert_eq!(
            result,
            Err("URL contains Unicode line separator characters")
        );
    }

    // Backslash rejection

    #[test]
    fn test_validate_url_for_open_rejects_backslash() {
        let result = validate_url_for_open("https://example.com/\\evil");
        assert_eq!(result, Err("URL contains potentially unsafe characters"));
    }
}
