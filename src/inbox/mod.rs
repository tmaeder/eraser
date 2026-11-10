//! Inbox monitoring: classify broker replies and extract actionable URLs.

pub mod classifier;
pub mod monitor;
pub mod parser;

mod go_url;

pub use monitor::Email;

use crate::history::{BrokerResponse, PipelineStatus};
use chrono::Local;
use classifier::{ClassifiedResponse, ResponseType};
use regex::Regex;

/// Plain-text body, else the HTML body.
pub fn body_of(email: &Email) -> String {
    if email.body.is_empty() {
        email.html_body.clone()
    } else {
        email.body.clone()
    }
}

/// The broker_responses row for a classified email (body included for reclassification).
pub fn response_record(email: &Email, c: &ClassifiedResponse) -> BrokerResponse {
    BrokerResponse {
        broker_id: email.broker_id.clone(),
        broker_name: email.broker_name.clone(),
        response_type: c.r#type.as_str().into(),
        email_from: email.from.clone(),
        email_subject: email.subject.clone(),
        email_body: body_of(email),
        form_url: c.form_url.clone(),
        confirm_url: c.confirm_url.clone(),
        confidence: c.confidence,
        needs_review: c.needs_review,
        received_at: Some(email.received_at.with_timezone(&Local)),
        ..Default::default()
    }
}

/// Pipeline stage a reply of this type moves the broker to.
pub fn pipeline_status_for(t: ResponseType) -> PipelineStatus {
    match t {
        ResponseType::Success => PipelineStatus::Confirmed,
        ResponseType::FormRequired => PipelineStatus::FormRequired,
        ResponseType::ConfirmationRequired => PipelineStatus::AwaitingConfirmation,
        ResponseType::Rejected => PipelineStatus::Rejected,
        _ => PipelineStatus::AwaitingResponse,
    }
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
