//! Inbox monitoring: classify broker replies and extract actionable URLs.

pub mod classifier;
pub mod monitor;
pub mod parser;

mod go_url;

pub use monitor::Email;

use regex::Regex;

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
