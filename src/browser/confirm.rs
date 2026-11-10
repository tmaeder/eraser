//! Clicks confirmation links from broker emails.

use anyhow::{Result, anyhow, bail};
use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::DEFAULT_USER_AGENT;
use crate::inbox::{go_lower, go_url};

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
    /// Returns `(valid, matched_domain_or_host)`; errors on unparsable URLs.
    pub fn validate_domain(&self, confirm_url: &str) -> Result<(bool, String)> {
        let url =
            go_url::parse(confirm_url).ok_or_else(|| anyhow!("invalid URL: {confirm_url:?}"))?;
        let mut host = go_lower(&String::from_utf8_lossy(&url.host));
        if let Some(i) = host.find(':') {
            host.truncate(i);
        }
        if self.broker_domains.contains(&host) {
            return Ok((true, host));
        }
        // Several matching parents: the first in sorted order wins.
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

        // GET: retried on connect errors, timeouts and 429/5xx (Retry-After honoured).
        let policy = crate::retry::Policy::new(3, Duration::from_secs(1));
        let mut resp = crate::retry::http(
            policy,
            || {
                redirects.lock().unwrap().clear();
                client
                    .get(confirm_url)
                    .header("User-Agent", DEFAULT_USER_AGENT)
                    .header(
                        "Accept",
                        "text/html,application/xhtml+xml,application/xml;q=0.9,image/webp,*/*;q=0.8",
                    )
                    .header("Accept-Language", "en-US,en;q=0.5")
            },
            true,
        )
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
    let body = go_lower(body);
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
    let body = go_lower(&result.response_body);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn click_retries_a_5xx_once_then_succeeds() {
        let hits = Arc::new(Mutex::new(0));
        let h = hits.clone();
        let app = axum::Router::new().route(
            "/confirm",
            axum::routing::get(move || {
                let h = h.clone();
                async move {
                    let mut n = h.lock().unwrap();
                    *n += 1;
                    if *n == 1 {
                        (axum::http::StatusCode::BAD_GATEWAY, "busy")
                    } else {
                        (
                            axum::http::StatusCode::OK,
                            "Your request has been confirmed",
                        )
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let r = ConfirmationHandler::new(&["127.0.0.1"])
            .click_confirmation_link(&format!("http://{addr}/confirm"), false)
            .await
            .unwrap();
        assert!(r.success, "{r:?}");
        assert_eq!((r.status_code, *hits.lock().unwrap()), (200, 2));
    }

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
