//! URL safety: scheme, SSRF and blocklist checks. Callers must check before every outbound fetch
//! and again on every redirect hop.

use reqwest::Url;
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// Stable code for a URL rejected by the SSRF/scheme/query policy.
pub const URL_BLOCKED_CODE: &str = "url_blocked";
/// Stable code for a URL rejected by the operator's website blocklist.
pub const WEBSITE_BLOCKED_CODE: &str = "website_blocked";

/// Cloud metadata hostnames; blocked by name so DNS never runs for them.
const BLOCKED_HOSTNAMES: &[&str] = &["metadata.google.internal", "metadata.goog"];

/// IPv4 cloud metadata / credential endpoints that are not covered by a broader range.
const METADATA_IPV4: &[Ipv4Addr] = &[
    Ipv4Addr::new(169, 254, 169, 254),
    Ipv4Addr::new(169, 254, 170, 2),
    Ipv4Addr::new(169, 254, 169, 253),
    Ipv4Addr::new(100, 100, 100, 200),
];

/// AWS IPv6 metadata endpoint (fd00:ec2::254).
const METADATA_IPV6: Ipv6Addr = Ipv6Addr::new(0xfd00, 0x0ec2, 0, 0, 0, 0, 0, 0x0254);

/// Query parameter names that unambiguously carry a credential. Deliberately narrow: bare
/// words that double as page facets (code, key, auth, session, sig) stay allowed.
const SENSITIVE_QUERY_PARAM_NAMES: &[&str] = &[
    "access_token",
    "api_key",
    "apikey",
    "auth_token",
    "authorization",
    "awsaccesskeyid",
    "client_secret",
    "credential",
    "credentials",
    "jwt",
    "password",
    "passwd",
    "secret",
    "session_id",
    "signature",
    "token",
    "x_amz_security_token",
    "x_amz_signature",
    "x-amz-security-token",
    "x-amz-signature",
];

/// Why a URL was rejected. code() is the stable wire code for every variant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UrlSafetyError {
    /// The URL could not be parsed.
    InvalidUrl(String),
    /// The scheme is not http or https.
    UnsupportedScheme(String),
    /// The host is a literal blocked name (localhost, *.localhost, metadata).
    BlockedHost(String),
    /// The URL has no host.
    MissingHost(String),
    /// The host resolved to a blocked address.
    BlockedAddress {
        host: String,
        address: String,
        reason: String,
    },
    /// DNS resolution failed; the check fails closed.
    DnsFailure(String),
}

impl UrlSafetyError {
    /// Stable code surfaced to callers.
    pub fn code(&self) -> &'static str {
        URL_BLOCKED_CODE
    }
}

impl fmt::Display for UrlSafetyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UrlSafetyError::InvalidUrl(detail) => {
                write!(f, "{URL_BLOCKED_CODE}: invalid URL: {detail}")
            }
            UrlSafetyError::UnsupportedScheme(scheme) => {
                write!(f, "{URL_BLOCKED_CODE}: unsupported scheme '{scheme}'")
            }
            UrlSafetyError::BlockedHost(host) => {
                write!(f, "{URL_BLOCKED_CODE}: blocked host '{host}'")
            }
            UrlSafetyError::MissingHost(url) => {
                write!(f, "{URL_BLOCKED_CODE}: missing host in '{url}'")
            }
            UrlSafetyError::BlockedAddress {
                host,
                address,
                reason,
            } => write!(
                f,
                "{URL_BLOCKED_CODE}: {host} resolves to {reason} ({address})"
            ),
            UrlSafetyError::DnsFailure(host) => {
                write!(f, "{URL_BLOCKED_CODE}: DNS resolution failed for '{host}'")
            }
        }
    }
}

impl std::error::Error for UrlSafetyError {}

/// Reject non-http(s) schemes, localhost and metadata hostnames, and any host that is or resolves
/// to a private, internal or metadata address. DNS failure fails closed.
pub async fn is_safe_url(url: &str) -> Result<(), UrlSafetyError> {
    let parsed = Url::parse(url).map_err(|err| UrlSafetyError::InvalidUrl(err.to_string()))?;
    let scheme = parsed.scheme().to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return Err(UrlSafetyError::UnsupportedScheme(scheme));
    }
    let host = parsed
        .host_str()
        .map(normalize_host)
        .filter(|host| !host.is_empty())
        .ok_or_else(|| UrlSafetyError::MissingHost(url.to_string()))?;

    if host == "localhost"
        || host.ends_with(".localhost")
        || BLOCKED_HOSTNAMES.contains(&host.as_str())
    {
        return Err(UrlSafetyError::BlockedHost(host));
    }

    if let Ok(ip) = host.parse::<IpAddr>() {
        return check_address(&host, ip);
    }

    let port = parsed.port_or_known_default().unwrap_or(443);
    let Ok(addresses) = tokio::net::lookup_host((host.as_str(), port)).await else {
        return Err(UrlSafetyError::DnsFailure(host));
    };
    let mut resolved = false;
    for address in addresses {
        resolved = true;
        check_address(&host, address.ip())?;
    }
    if !resolved {
        return Err(UrlSafetyError::DnsFailure(host));
    }
    Ok(())
}

/// True when host equals an entry or is a subdomain of one.
pub fn host_is_blocked(host: &str, blocklist: &[String]) -> bool {
    let host = normalize_host(host);
    if host.is_empty() {
        return false;
    }
    blocklist.iter().any(|entry| {
        let entry = normalize_entry(entry);
        !entry.is_empty() && (host == entry || host.ends_with(&format!(".{entry}")))
    })
}

/// First blocked host found in free text: a search query, a site: operator or a URL.
pub fn blocked_host_in_text(text: &str, blocklist: &[String]) -> Option<String> {
    if blocklist.is_empty() {
        return None;
    }
    for raw in text.split(|ch: char| {
        ch.is_whitespace()
            || matches!(
                ch,
                ',' | ';' | '"' | '\'' | '(' | ')' | '[' | ']' | '<' | '>'
            )
    }) {
        let token = raw.trim();
        if token.is_empty() {
            continue;
        }
        let token = token.strip_prefix("site:").unwrap_or(token);
        if let Some(host) = candidate_host(token).filter(|host| host_is_blocked(host, blocklist)) {
            return Some(host);
        }
    }
    None
}

/// First credential-named query parameter with a non-empty value, if any.
pub fn sensitive_query_param_name(url: &str) -> Option<String> {
    let parsed = Url::parse(url).ok()?;
    let scheme = parsed.scheme().to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return None;
    }
    parsed.query()?;
    parsed.query_pairs().find_map(|(key, value)| {
        let name = key.to_ascii_lowercase();
        (!value.is_empty() && SENSITIVE_QUERY_PARAM_NAMES.contains(&name.as_str())).then_some(name)
    })
}

fn normalize_host(host: &str) -> String {
    host.trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim_end_matches('.')
        .to_ascii_lowercase()
}

fn normalize_entry(entry: &str) -> String {
    entry
        .trim()
        .trim_end_matches('.')
        .trim_start_matches('.')
        .to_ascii_lowercase()
}

/// A conservative host candidate from one text token; None for ordinary query words.
fn candidate_host(token: &str) -> Option<String> {
    let token = token.trim_matches(|ch: char| matches!(ch, '/' | '?' | '#'));
    let after_scheme = token.split("://").last().unwrap_or(token);
    let authority = after_scheme.split(['/', '?', '#']).next().unwrap_or("");
    let host = authority
        .rsplit('@')
        .next()
        .unwrap_or("")
        .split(':')
        .next()
        .unwrap_or("");
    let host = normalize_host(host);
    if host.is_empty()
        || !host.contains('.')
        || host.starts_with('.')
        || host.ends_with('.')
        || !host
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '.' || ch == '-')
    {
        return None;
    }
    Some(host)
}

fn check_address(host: &str, address: IpAddr) -> Result<(), UrlSafetyError> {
    match blocked_reason(address) {
        Some(reason) => Err(UrlSafetyError::BlockedAddress {
            host: host.to_string(),
            address: address.to_string(),
            reason: reason.to_string(),
        }),
        None => Ok(()),
    }
}

fn blocked_reason(ip: IpAddr) -> Option<&'static str> {
    match ip {
        IpAddr::V4(v4) => blocked_ipv4_reason(v4),
        IpAddr::V6(v6) => v6
            .to_ipv4_mapped()
            .and_then(blocked_ipv4_reason)
            .or_else(|| blocked_ipv6_reason(v6)),
    }
}

fn blocked_ipv4_reason(ip: Ipv4Addr) -> Option<&'static str> {
    let octets = ip.octets();
    if METADATA_IPV4.contains(&ip) {
        return Some("cloud metadata address");
    }
    if octets[0] == 127 {
        return Some("loopback address");
    }
    if octets[0] == 10
        || (octets[0] == 172 && (16..=31).contains(&octets[1]))
        || (octets[0] == 192 && octets[1] == 168)
    {
        return Some("private address");
    }
    if octets[0] == 169 && octets[1] == 254 {
        return Some("link-local address");
    }
    if octets[0] == 100 && (64..=127).contains(&octets[1]) {
        return Some("CGNAT address");
    }
    if octets[0] == 0 {
        return Some("unspecified address");
    }
    if (224..=239).contains(&octets[0]) {
        return Some("multicast address");
    }
    if octets[0] >= 240 {
        return Some("reserved address");
    }
    None
}

fn blocked_ipv6_reason(ip: Ipv6Addr) -> Option<&'static str> {
    if ip == METADATA_IPV6 {
        return Some("cloud metadata address");
    }
    if ip.is_loopback() {
        return Some("loopback address");
    }
    if ip.is_unspecified() {
        return Some("unspecified address");
    }
    if ip.is_multicast() {
        return Some("multicast address");
    }
    let segments = ip.segments();
    if (segments[0] & 0xffc0) == 0xfe80 {
        return Some("link-local address");
    }
    if (segments[0] & 0xfe00) == 0xfc00 {
        return Some("unique-local address");
    }
    None
}
