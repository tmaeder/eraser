//! Port of internal/browser/confirm.go: clicks confirmation links from broker emails.

use anyhow::{Result, anyhow, bail};
use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::DEFAULT_USER_AGENT;

/// Outcome of clicking a confirmation link.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ConfirmationResult {
    pub success: bool,
    pub url: String,
    pub final_url: String,
    pub status_code: u16,
    pub response_body: String,
    pub error_message: String,
    pub redirect_path: Vec<String>,
}

/// Clicks confirmation links from emails, optionally restricted to known broker domains.
#[derive(Debug, Clone)]
pub struct ConfirmationHandler {
    broker_domains: BTreeSet<String>,
}

const MAX_BODY: usize = 64 * 1024;
const MAX_REDIRECTS: usize = 10;

const SUCCESS_PATTERNS: &[&str] = &[
    "successfully",
    "confirmed",
    "verification complete",
    "verified",
    "opt-out complete",
    "removal complete",
    "request received",
    "request confirmed",
    "thank you",
    "has been removed",
    "been deleted",
    "been processed",
    "unsubscribed",
    "opted out",
];

const FAILURE_PATTERNS: &[&str] = &[
    "link expired",
    "link invalid",
    "already confirmed",
    "error occurred",
    "something went wrong",
    "could not",
    "unable to",
    "failed",
];

impl ConfirmationHandler {
    /// Creates a handler that accepts the given domains plus their www./mail./email. variants.
    pub fn new<S: AsRef<str>>(broker_domains: &[S]) -> Self {
        let mut domains = BTreeSet::new();
        for d in broker_domains {
            let d = d.as_ref().to_lowercase();
            for prefix in ["www.", "mail.", "email."] {
                if !d.starts_with(prefix) {
                    domains.insert(format!("{prefix}{d}"));
                }
            }
            domains.insert(d);
        }
        Self {
            broker_domains: domains,
        }
    }

    /// Checks whether the URL belongs to a known broker domain.
    /// Returns `(valid, matched_domain_or_host)`; errors where Go's `url.Parse` errors.
    pub fn validate_domain(&self, confirm_url: &str) -> Result<(bool, String)> {
        let host =
            go_url_host(confirm_url).ok_or_else(|| anyhow!("invalid URL: {confirm_url:?}"))?;
        let mut host = go_to_lower(&host);
        if let Some(i) = host.find(':') {
            host.truncate(i);
        }
        if self.broker_domains.contains(&host) {
            return Ok((true, host));
        }
        // ponytail: Go iterates a map here (random pick among several matching parents); we take the first in sorted order.
        for domain in &self.broker_domains {
            if host.ends_with(&format!(".{domain}")) {
                return Ok((true, domain.clone()));
            }
        }
        Ok((false, host))
    }

    /// Sends a browser-like GET request to the confirmation URL, following up to 10 redirects.
    pub async fn click_confirmation_link(
        &self,
        confirm_url: &str,
        validate_domain: bool,
    ) -> Result<ConfirmationResult> {
        let mut result = ConfirmationResult {
            url: confirm_url.to_string(),
            redirect_path: vec![confirm_url.to_string()],
            ..Default::default()
        };

        if validate_domain {
            let (valid, domain) = self.validate_domain(confirm_url)?;
            if !valid {
                bail!("domain {domain} is not a known broker domain");
            }
        }

        let redirects = Arc::new(Mutex::new(Vec::new()));
        let tracked = redirects.clone();
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::custom(move |attempt| {
                if attempt.previous().len() >= MAX_REDIRECTS {
                    return attempt.error("too many redirects");
                }
                tracked.lock().unwrap().push(attempt.url().to_string());
                attempt.follow()
            }))
            .build()?;

        let mut resp = client
            .get(confirm_url)
            .header("User-Agent", DEFAULT_USER_AGENT)
            .header(
                "Accept",
                "text/html,application/xhtml+xml,application/xml;q=0.9,image/webp,*/*;q=0.8",
            )
            .header("Accept-Language", "en-US,en;q=0.5")
            .send()
            .await
            .map_err(|e| anyhow!("request failed: {e}"))?;

        result.status_code = resp.status().as_u16();
        result.final_url = resp.url().to_string();
        result
            .redirect_path
            .extend(redirects.lock().unwrap().drain(..));

        let mut body = Vec::new();
        while body.len() < MAX_BODY {
            match resp.chunk().await {
                Ok(Some(chunk)) => body.extend_from_slice(&chunk),
                Ok(None) => break,
                Err(e) => bail!("failed to read response: {e}"),
            }
        }
        body.truncate(MAX_BODY);
        result.response_body = String::from_utf8_lossy(&body).into_owned();

        result.success = is_success_response(result.status_code, &result.response_body);
        if !result.success && result.error_message.is_empty() {
            result.error_message = format!(
                "confirmation may have failed (status {})",
                result.status_code
            );
        }
        Ok(result)
    }

    /// Human-readable status for a confirmation result.
    pub fn extract_confirmation_status(&self, result: &ConfirmationResult) -> &'static str {
        extract_confirmation_status(result)
    }
}

/// Whether the response indicates a successful confirmation.
pub fn is_success_response(status_code: u16, body: &str) -> bool {
    if !(200..400).contains(&status_code) {
        return false;
    }
    let body = go_to_lower(body);
    if SUCCESS_PATTERNS.iter().any(|p| body.contains(p)) {
        return true;
    }
    if FAILURE_PATTERNS.iter().any(|p| body.contains(p)) {
        return false;
    }
    status_code == 200
}

/// Human-readable status from a confirmation result.
pub fn extract_confirmation_status(result: &ConfirmationResult) -> &'static str {
    if result.success {
        return "Confirmation successful";
    }
    let body = go_to_lower(&result.response_body);
    if body.contains("expired") {
        "Link expired"
    } else if body.contains("already") {
        "Already confirmed/processed"
    } else if body.contains("invalid") {
        "Invalid link"
    } else if result.status_code == 404 {
        "Link not found (404)"
    } else if result.status_code >= 500 {
        "Server error"
    } else {
        "Unknown status"
    }
}

/// Go's strings.ToLower is per-rune simple case mapping; Rust's to_lowercase is close enough
/// for ASCII patterns, but 'İ' expands to two chars in Rust. Map it the way Go does.
pub(crate) fn go_to_lower(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'İ' => 'i',
            c => c.to_lowercase().next().unwrap_or(c),
        })
        .collect()
}

// ---- Go net/url.Parse host extraction (subset) ----
// The `url` crate applies WHATWG rules (IDNA, percent-decoding, IPv6 normalisation) that change
// which hosts validate, so mirror Go's parser: return the unescaped Host, or None where Go errors.

fn go_url_host(raw: &str) -> Option<String> {
    let raw = raw.as_bytes();
    let (u, frag) = match raw.iter().position(|&b| b == b'#') {
        Some(i) => (&raw[..i], &raw[i + 1..]),
        None => (raw, &b""[..]),
    };
    if !valid_percent(frag) || u.iter().any(|&b| b < b' ' || b == 0x7f) {
        return None;
    }
    if u == b"*" {
        return Some(String::new());
    }
    let (scheme, rest) = split_scheme(u)?;
    let mut rest = match rest.iter().position(|&b| b == b'?') {
        Some(i) => &rest[..i],
        None => rest,
    };
    if !rest.starts_with(b"/") {
        if !scheme.is_empty() {
            return Some(String::new()); // opaque
        }
        let seg_end = rest.iter().position(|&b| b == b'/').unwrap_or(rest.len());
        if rest[..seg_end].contains(&b':') {
            return None;
        }
    }
    let mut host = Vec::new();
    if (!scheme.is_empty() || !rest.starts_with(b"///")) && rest.starts_with(b"//") {
        let mut authority = &rest[2..];
        rest = b"";
        if let Some(i) = authority.iter().position(|&b| b == b'/') {
            rest = &authority[i..];
            authority = &authority[..i];
        }
        let at = authority.iter().rposition(|&b| b == b'@');
        host = parse_host(at.map_or(authority, |i| &authority[i + 1..]))?;
        if let Some(i) = at {
            let userinfo = &authority[..i];
            if !userinfo.iter().all(|&c| valid_userinfo_byte(c)) || !valid_percent(userinfo) {
                return None;
            }
        }
    }
    if !valid_percent(rest) {
        return None;
    }
    Some(String::from_utf8_lossy(&host).into_owned())
}

fn split_scheme(raw: &[u8]) -> Option<(&[u8], &[u8])> {
    for (i, &c) in raw.iter().enumerate() {
        match c {
            b'a'..=b'z' | b'A'..=b'Z' => {}
            b'0'..=b'9' | b'+' | b'-' | b'.' if i == 0 => return Some((b"", raw)),
            b'0'..=b'9' | b'+' | b'-' | b'.' => {}
            b':' if i == 0 => return None,
            b':' => return Some((&raw[..i], &raw[i + 1..])),
            _ => return Some((b"", raw)),
        }
    }
    Some((b"", raw))
}

/// Every '%' is followed by two hex digits (Go's unescape check for non-host modes).
fn valid_percent(s: &[u8]) -> bool {
    let mut i = 0;
    while i < s.len() {
        if s[i] == b'%' {
            if i + 2 >= s.len() || !s[i + 1].is_ascii_hexdigit() || !s[i + 2].is_ascii_hexdigit() {
                return false;
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    true
}

fn valid_userinfo_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || b"-._:~!$&'()*+,;=%@".contains(&c)
}

fn unhex(c: u8) -> u8 {
    match c {
        b'0'..=b'9' => c - b'0',
        b'a'..=b'f' => c - b'a' + 10,
        _ => c - b'A' + 10,
    }
}

/// Go's shouldEscape(c, encodeHost) for ASCII bytes.
fn host_should_escape(c: u8) -> bool {
    !(c.is_ascii_alphanumeric() || b"-_.~!$&'()*+,;=:[]<>\"".contains(&c))
}

/// Go's unescape(s, encodeHost) (zone handling folded in: zone bytes after "%25" are permissive).
fn unescape_host(s: &[u8], zone: bool) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        let c = s[i];
        if c == b'%' {
            if i + 2 >= s.len() || !s[i + 1].is_ascii_hexdigit() || !s[i + 2].is_ascii_hexdigit() {
                return None;
            }
            let v = (unhex(s[i + 1]) << 4) | unhex(s[i + 2]);
            let is25 = &s[i..i + 3] == b"%25";
            if !zone && unhex(s[i + 1]) < 8 && !is25 {
                return None;
            }
            if zone && !is25 && v != b' ' && host_should_escape(v) {
                return None;
            }
            out.push(v);
            i += 3;
        } else {
            if c < 0x80 && c != b'+' && host_should_escape(c) {
                return None;
            }
            out.push(c);
            i += 1;
        }
    }
    Some(out)
}

fn valid_optional_port(port: &[u8]) -> bool {
    match port.split_first() {
        None => true,
        Some((b':', digits)) => digits.iter().all(u8::is_ascii_digit),
        Some(_) => false,
    }
}

fn parse_host(host: &[u8]) -> Option<Vec<u8>> {
    match host.iter().rposition(|&b| b == b'[') {
        Some(i) if i > 0 => None,
        Some(_) => {
            let close = host.iter().rposition(|&b| b == b']')?;
            let colon_port = &host[close + 1..];
            if !valid_optional_port(colon_port) {
                return None;
            }
            let hostname = &host[1..close];
            let mut out = match hostname.windows(3).position(|w| w == b"%25") {
                Some(z) => {
                    let mut h = unescape_host(&hostname[..z], false)?;
                    h.extend(unescape_host(&hostname[z..], true)?);
                    h
                }
                None => unescape_host(hostname, false)?,
            };
            let addr = std::str::from_utf8(&out).ok()?;
            let addr = match addr.split_once('%') {
                Some((_, "")) => return None,
                Some((a, _)) => a,
                None => addr,
            };
            addr.parse::<std::net::Ipv6Addr>().ok()?;
            out.insert(0, b'[');
            out.push(b']');
            out.extend_from_slice(colon_port);
            Some(out)
        }
        None => {
            if let Some(i) = host.iter().rposition(|&b| b == b':')
                && !valid_optional_port(&host[i..])
            {
                return None;
            }
            unescape_host(host, false)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn domain_variants_and_subdomains() {
        let h = ConfirmationHandler::new(&["Example.com", "www.foo.org"]);
        assert_eq!(
            h.validate_domain("https://www.example.com/x").unwrap(),
            (true, "www.example.com".into())
        );
        assert_eq!(
            h.validate_domain("https://a.b.example.com:443/x").unwrap(),
            (true, "example.com".into())
        );
        assert_eq!(
            h.validate_domain("https://foo.org/x").unwrap(),
            (false, "foo.org".into())
        );
        assert_eq!(
            h.validate_domain("https://example.com.evil.com/").unwrap(),
            (false, "example.com.evil.com".into())
        );
        assert!(h.validate_domain("https://exa mple.com/").is_err());
    }

    #[test]
    fn success_heuristics() {
        assert!(is_success_response(200, ""));
        assert!(!is_success_response(201, ""));
        assert!(is_success_response(302, "Confirmed"));
        assert!(!is_success_response(200, "Link expired"));
        assert!(is_success_response(200, "Thank you! Link expired"));
        assert!(!is_success_response(404, "thank you"));
    }

    #[test]
    fn status_text() {
        let r = |success, status_code, body: &str| ConfirmationResult {
            success,
            status_code,
            response_body: body.into(),
            ..Default::default()
        };
        assert_eq!(
            extract_confirmation_status(&r(true, 200, "")),
            "Confirmation successful"
        );
        assert_eq!(
            extract_confirmation_status(&r(false, 200, "EXPIRED")),
            "Link expired"
        );
        assert_eq!(
            extract_confirmation_status(&r(false, 200, "already done")),
            "Already confirmed/processed"
        );
        assert_eq!(
            extract_confirmation_status(&r(false, 200, "invalid")),
            "Invalid link"
        );
        assert_eq!(
            extract_confirmation_status(&r(false, 404, "")),
            "Link not found (404)"
        );
        assert_eq!(
            extract_confirmation_status(&r(false, 503, "")),
            "Server error"
        );
        assert_eq!(
            extract_confirmation_status(&r(false, 400, "")),
            "Unknown status"
        );
    }
}
