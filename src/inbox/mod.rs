//! Inbox monitoring: classify broker replies and extract actionable URLs.

pub mod classifier;
pub mod parser;
// pub mod monitor;

mod go_url;

use chrono::{DateTime, Utc};
use regex::Regex;

/// An email fetched from the inbox.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Email {
    /// IMAP UID for operations like move/delete
    pub uid: u32,
    pub message_id: String,
    pub from: String,
    /// Sender display name (e.g. "Mail Delivery System")
    pub from_name: String,
    pub from_domain: String,
    pub subject: String,
    pub body: String,
    pub html_body: String,
    pub received_at: DateTime<Utc>,
    /// Matched broker ID (if found)
    pub broker_id: String,
    /// Matched broker name (if found)
    pub broker_name: String,
}

/// Go's `strings.ToLower`: per-rune simple lowercase mapping. Differs from
/// `str::to_lowercase` for 'İ' (Go: "i") and final sigma (Go: always 'σ').
pub(crate) fn go_lower(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c == 'İ' {
                'i'
            } else {
                c.to_lowercase().next().unwrap_or(c)
            }
        })
        .collect()
}

/// Compiles a Go RE2 pattern with Go's ASCII-only `\s`, `\d` and `\w`
/// (Rust's are Unicode-aware, and its ASCII `\s` also includes `\v`).
pub(crate) fn go_regex(pattern: &str) -> Regex {
    let translated = pattern
        .replace(r"\s", r"[\t\n\f\r ]")
        .replace(r"\d", "[0-9]")
        .replace(r"\w", "[0-9A-Za-z_]");
    Regex::new(&translated).unwrap_or_else(|e| panic!("invalid pattern {pattern}: {e}"))
}
