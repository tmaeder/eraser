//! URL extraction and bounce parsing for broker reply emails (port of `internal/inbox/parser.go`).

use std::collections::HashSet;
use std::sync::LazyLock;

use regex::Regex;
use scraper::{Html, Node, Selector};

use super::{Email, go_lower, go_regex, go_url};

/// Categorized URLs from an email. Order is first-seen order, deduplicated.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ExtractedURLs {
    /// URLs that look like opt-out forms
    pub form_urls: Vec<String>,
    /// URLs that look like confirmation links
    pub confirmation_urls: Vec<String>,
    pub unsubscribe_urls: Vec<String>,
    pub all_urls: Vec<String>,
}

/// Clearly an opt-out form (+10 each).
const STRONG_FORM_PATTERNS: &[&str] = &[
    "opt-out",
    "optout",
    "opt_out",
    "do-not-sell",
    "donotsell",
    "do_not_sell",
    "removal-request",
    "removal-form",
    "removalrequest",
    "remove-my-info",
    "remove-listing",
    "remove-record",
    "data-request",
    "dsar",
    "data-subject",
    "ccpa-request",
    "gdpr-request",
    "privacy-request",
    "privacy-form",
    "/optout",
    "/opt-out",
    "/removal",
    "/remove-me",
];

/// May indicate a form (+5 each).
const MODERATE_FORM_PATTERNS: &[&str] = &[
    "suppress",
    "suppression",
    "ccpa",
    "gdpr",
    "/remove",
    "/delete",
];

/// Only counted once some positive signal exists (+2 each).
const WEAK_FORM_PATTERNS: &[&str] = &["remove", "removal", "delete", "deletion", "unsubscribe"];

/// Disqualify a URL from being a form.
const NOT_FORM_PATTERNS: &[&str] = &[
    // Policy/legal pages
    "privacy-policy",
    "privacy_policy",
    "privacypolicy",
    "terms-of-service",
    "terms_of_service",
    "termsofservice",
    "terms-and-conditions",
    "terms_and_conditions",
    "cookie-policy",
    "cookie_policy",
    "cookiepolicy",
    "/tos",
    "/terms",
    "/legal",
    "/policy",
    // Info/help pages
    "/about",
    "/contact",
    "/help",
    "/faq",
    "/support",
    "/how-to",
    "/howto",
    "/learn",
    "/info",
    // Auth pages
    "/login",
    "/signin",
    "/register",
    "/signup",
    "/auth",
    // Account/settings
    "/account",
    "/settings",
    "/preferences",
    "/profile",
    "/unsubscribe-preferences",
    "/email-preferences",
    "/manage-preferences",
    "/communication-preferences",
    // Marketing pages
    "/marketing",
    "/newsletter",
    "/subscribe",
    // Documents
    ".pdf",
    ".doc",
    ".docx",
    // Social media
    "facebook.com",
    "twitter.com",
    "linkedin.com",
    "instagram.com",
    // Generic external domains
    "google.com",
    "bit.ly",
    "tinyurl.com",
];

const CONFIRM_PATTERNS: &[&str] = &[
    "confirm",
    "verification",
    "verify",
    "activate",
    "validate",
    "click-here",
    "clickhere",
    "token=",
    "code=",
    "approve",
    "accept",
];

const TRACKING_PATTERNS: &[&str] = &[
    "track",
    "pixel",
    "beacon",
    "open.gif",
    "spacer.gif",
    "1x1",
    "unsubscribe-tracking",
];

static URL_REGEX: LazyLock<Regex> = LazyLock::new(|| go_regex(r#"https?://[^\s<>"']+"#));
static EMAIL_REGEX: LazyLock<Regex> =
    LazyLock::new(|| go_regex(r"[a-zA-Z0-9._%+-]+@[a-zA-Z0-9.-]+\.[a-zA-Z]{2,}"));
static TAG_REGEX: LazyLock<Regex> = LazyLock::new(|| go_regex(r"<[^>]+>"));
static LINK_SELECTOR: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("a[href]").expect("valid selector"));

/// Patterns that precede the bounced address in NDRs, most reliable first.
static BOUNCED_RECIPIENT_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        r"(?i)(?:the\s+following|these)\s+address(?:es)?\s+(?:had\s+permanent\s+)?(?:fatal\s+)?(?:errors?|failed)[:\s]+([a-zA-Z0-9._%+-]+@[a-zA-Z0-9.-]+\.[a-zA-Z]{2,})",
        r"(?i)delivery\s+to\s+(?:the\s+following\s+)?(?:recipient|address)(?:s)?\s+failed[:\s]+([a-zA-Z0-9._%+-]+@[a-zA-Z0-9.-]+\.[a-zA-Z]{2,})",
        r"(?i)(?:original|final)[\s-]?recipient[:\s]+(?:rfc822;)?([a-zA-Z0-9._%+-]+@[a-zA-Z0-9.-]+\.[a-zA-Z]{2,})",
        r"(?i)(?:failed|rejected)\s+recipient[:\s]+([a-zA-Z0-9._%+-]+@[a-zA-Z0-9.-]+\.[a-zA-Z]{2,})",
        r"(?i)undeliverable\s+to[:\s]+([a-zA-Z0-9._%+-]+@[a-zA-Z0-9.-]+\.[a-zA-Z]{2,})",
        r"(?i)message\s+could\s+not\s+be\s+delivered\s+to[:\s]+([a-zA-Z0-9._%+-]+@[a-zA-Z0-9.-]+\.[a-zA-Z]{2,})",
        r"(?i)<([a-zA-Z0-9._%+-]+@[a-zA-Z0-9.-]+\.[a-zA-Z]{2,})>.*(?:failed|rejected|undeliverable)",
        r"(?i)to[:\s]+<?([a-zA-Z0-9._%+-]+@[a-zA-Z0-9.-]+\.[a-zA-Z]{2,})>?\s+.*(?:failed|rejected|not\s+exist)",
    ]
    .into_iter()
    .map(go_regex)
    .collect()
});

/// Extracts and categorizes URLs from the plain-text body, then the HTML body.
pub fn parse_email_urls(email: &Email) -> ExtractedURLs {
    let mut result = ExtractedURLs::default();
    let mut all = Vec::new();
    if !email.body.is_empty() {
        all.extend(extract_urls_from_text(&email.body));
    }
    if !email.html_body.is_empty() {
        all.extend(extract_urls_from_html(&email.html_body));
    }

    let mut seen = HashSet::new();
    for raw in all {
        let clean = clean_url(&raw);
        if clean.is_empty() || !seen.insert(clean.clone()) || is_tracking_url(&clean) {
            continue;
        }
        let lower = go_lower(&clean);
        if is_form_url(&lower) {
            result.form_urls.push(clean.clone());
        }
        if is_confirmation_url(&lower) {
            result.confirmation_urls.push(clean.clone());
        }
        if lower.contains("unsubscribe") {
            result.unsubscribe_urls.push(clean.clone());
        }
        result.all_urls.push(clean);
    }
    result
}

fn extract_urls_from_text(text: &str) -> Vec<String> {
    URL_REGEX
        .find_iter(text)
        .map(|m| m.as_str().to_string())
        .collect()
}

/// `a[href]` values in document order, then URLs found in the document's text
/// (all text nodes concatenated, like goquery's `Text()`).
fn extract_urls_from_html(html: &str) -> Vec<String> {
    let doc = Html::parse_document(html);
    let mut urls: Vec<String> = doc
        .select(&LINK_SELECTOR)
        .filter_map(|a| a.value().attr("href").map(str::to_string))
        .collect();
    let text: String = doc
        .tree
        .root()
        .descendants()
        .filter_map(|n| match n.value() {
            Node::Text(t) => Some(&**t),
            _ => None,
        })
        .collect();
    urls.extend(extract_urls_from_text(&text));
    urls
}

/// Normalizes a URL the way Go's `url.Parse(..).String()` does; empty if not absolute http(s).
pub(crate) fn clean_url(raw: &str) -> String {
    let raw = raw.trim_end_matches(['.', ',', ';', ':', '!', '?', ')']);
    match go_url::parse(raw) {
        Some(u) if (u.scheme == "http" || u.scheme == "https") && !u.host.is_empty() => {
            u.to_go_string()
        }
        _ => String::new(),
    }
}

/// Higher = more likely an opt-out form; -20 = disqualified.
fn score_form_url(lower_url: &str) -> i32 {
    if NOT_FORM_PATTERNS.iter().any(|p| lower_url.contains(p)) {
        return -20;
    }
    let count =
        |patterns: &[&str]| patterns.iter().filter(|p| lower_url.contains(*p)).count() as i32;
    let mut score = count(STRONG_FORM_PATTERNS) * 10 + count(MODERATE_FORM_PATTERNS) * 5;
    if score > 0 {
        score += count(WEAK_FORM_PATTERNS) * 2;
    }
    score
}

fn is_form_url(lower_url: &str) -> bool {
    score_form_url(lower_url) > 0
}

fn is_confirmation_url(lower_url: &str) -> bool {
    CONFIRM_PATTERNS.iter().any(|p| lower_url.contains(p))
}

fn is_tracking_url(url: &str) -> bool {
    let lower = go_lower(url);
    TRACKING_PATTERNS.iter().any(|p| lower.contains(p))
        || lower.ends_with(".gif")
        || (lower.ends_with(".png") && lower.contains("pixel"))
}

/// Extracts the address that bounced from an NDR (body + stripped HTML + subject).
pub fn extract_bounced_recipient(email: &Email) -> String {
    let mut content = email.body.clone();
    if !email.html_body.is_empty() {
        content.push(' ');
        content.push_str(&TAG_REGEX.replace_all(&email.html_body, " "));
    }
    content.push(' ');
    content.push_str(&email.subject);

    for pattern in BOUNCED_RECIPIENT_PATTERNS.iter() {
        if let Some(m) = pattern.captures(&content).and_then(|c| c.get(1)) {
            return m.as_str().trim().to_string();
        }
    }

    // Fallback: first address that isn't a mail system or the user's own mailbox.
    const EXCLUDE: &[&str] = &[
        "mailer-daemon",
        "postmaster",
        "noreply",
        "no-reply",
        "@icloud.com",
        "@me.com",
        "@mac.com",
        "@gmail.com",
        "@yahoo.com",
        "@outlook.com",
        "@hotmail.com",
    ];
    EMAIL_REGEX
        .find_iter(&content)
        .map(|m| m.as_str())
        .find(|addr| {
            let lower = go_lower(addr);
            !EXCLUDE.iter().any(|e| lower.contains(e))
        })
        .unwrap_or_default()
        .to_string()
}

/// Extracts a token from a confirmation URL's query (`token`, `code`, ...) or
/// from the path segment after `/confirm/` or `/verify/` (if longer than 10 bytes).
pub fn extract_confirmation_token(confirm_url: &str) -> String {
    let Some(u) = go_url::parse(confirm_url) else {
        return String::new();
    };
    for param in ["token", "code", "verify", "confirmation", "key", "id"] {
        let v = u.query_get(param);
        if !v.is_empty() {
            return String::from_utf8_lossy(&v).into_owned();
        }
    }
    let parts: Vec<&[u8]> = u.path.split(|&b| b == b'/').collect();
    for (i, part) in parts.iter().enumerate() {
        if (*part == b"confirm" || *part == b"verify")
            && i + 1 < parts.len()
            && parts[i + 1].len() > 10
        {
            return String::from_utf8_lossy(parts[i + 1]).into_owned();
        }
    }
    String::new()
}

/// Highest-scoring form URL; +20 for URLs containing the broker domain. First wins ties.
pub fn get_primary_form_url(urls: &ExtractedURLs, broker_domain: &str) -> String {
    let domain = go_lower(broker_domain);
    let mut best: Option<(&String, i32)> = None;
    for u in &urls.form_urls {
        let lower = go_lower(u);
        let mut score = score_form_url(&lower);
        if score < 0 {
            continue;
        }
        if !domain.is_empty() && lower.contains(&domain) {
            score += 20;
        }
        if best.is_none_or(|(_, b)| score > b) {
            best = Some((u, score));
        }
    }
    best.map(|(u, _)| u.clone()).unwrap_or_default()
}

/// First confirmation URL containing `broker_domain` (case-insensitive), else the first one.
pub fn get_primary_confirmation_url(urls: &ExtractedURLs, broker_domain: &str) -> String {
    let domain = go_lower(broker_domain);
    urls.confirmation_urls
        .iter()
        .find(|u| go_lower(u).contains(&domain))
        .or(urls.confirmation_urls.first())
        .cloned()
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_url_matches_go_net_url() {
        assert_eq!(clean_url("https://Example.com"), "https://Example.com");
        assert_eq!(
            clean_url("https://example.com/a b"),
            "https://example.com/a%20b"
        );
        assert_eq!(
            clean_url("https://example.com/opt-out)."),
            "https://example.com/opt-out"
        );
        assert_eq!(clean_url("mailto:x@example.com"), "");
        assert_eq!(clean_url("https:///nohost"), "");
        assert_eq!(clean_url("https://example.com:abc/"), "");
    }

    #[test]
    fn primary_urls_prefer_broker_domain() {
        let urls = ExtractedURLs {
            form_urls: vec![
                "https://other.com/opt-out".into(),
                "https://Acme.com/opt-out".into(),
            ],
            confirmation_urls: vec![
                "https://other.com/confirm".into(),
                "https://Acme.com/confirm".into(),
            ],
            ..Default::default()
        };
        assert_eq!(
            get_primary_confirmation_url(&urls, "ACME.com"),
            "https://Acme.com/confirm"
        );
        assert_eq!(
            get_primary_confirmation_url(&urls, "nope.com"),
            "https://other.com/confirm"
        );
        assert_eq!(
            get_primary_form_url(&urls, "acme.com"),
            "https://Acme.com/opt-out"
        );
        assert_eq!(get_primary_form_url(&ExtractedURLs::default(), "x"), "");
    }

    #[test]
    fn confirmation_token() {
        assert_eq!(
            extract_confirmation_token("https://x.com/c?token=a%20b&code=z"),
            "a b"
        );
        assert_eq!(
            extract_confirmation_token("https://x.com/verify/abcdefghijkl"),
            "abcdefghijkl"
        );
        assert_eq!(extract_confirmation_token("https://x.com/verify/short"), "");
    }
}
