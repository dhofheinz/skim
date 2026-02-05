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

fn is_private_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(ipv4) => {
            ipv4.is_private() || ipv4.is_loopback() || ipv4.is_link_local() || ipv4.is_unspecified()
        }
        IpAddr::V6(ipv6) => {
            if ipv6.is_loopback() || ipv6.is_unspecified() {
                return true;
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
}
