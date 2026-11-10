//! gorilla/csrf v1.7.3 equivalent: a random 32-byte secret lives in the `_gorilla_csrf` cookie;
//! every request gets a fresh masked token (one-time pad || pad XOR secret, base64) for the hidden
//! field `gorilla.csrf.Token` or the `X-CSRF-Token` header. Unsafe methods must pass the
//! Origin/Referer checks against the trusted origins and present a token that unmasks to the secret.
//!
//! Unlike gorilla the cookie is not HMAC-signed (no MAC crate in the dependency set). Forging it
//! requires cookie-write access to the host and still fails the Origin/Referer checks.

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum_extra::extract::cookie::CookieJar;
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use std::sync::Arc;

pub const COOKIE_NAME: &str = "_gorilla_csrf";
pub const FIELD_NAME: &str = "gorilla.csrf.Token";
pub const HEADER_NAME: &str = "X-CSRF-Token";
const TOKEN_LEN: usize = 32;
const MAX_AGE_SECS: u32 = 12 * 3600;
const MAX_FORM_BYTES: usize = 10 << 20;

pub struct Csrf {
    /// Hosts (host[:port]) accepted in Origin/Referer headers.
    pub trusted_origins: Vec<String>,
    /// Sets the `Secure` cookie flag.
    pub secure: bool,
}

/// The masked per-request token, available to handlers as a request extension.
#[derive(Clone, Debug)]
pub struct CsrfToken(pub String);

fn xor(a: &[u8], b: &[u8]) -> Vec<u8> {
    a.iter().zip(b).map(|(x, y)| x ^ y).collect()
}

fn mask(real: &[u8]) -> String {
    let otp: [u8; TOKEN_LEN] = rand::random();
    let mut out = otp.to_vec();
    out.extend(xor(&otp, real));
    STANDARD.encode(out)
}

fn unmask(issued: &[u8]) -> Option<Vec<u8>> {
    (issued.len() == TOKEN_LEN * 2).then(|| xor(&issued[TOKEN_LEN..], &issued[..TOKEN_LEN]))
}

pub(super) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// (scheme, host[:port]) like Go's url.Parse; unparsable input yields empty parts.
fn scheme_host(raw: &str) -> (String, String) {
    match url::Url::parse(raw) {
        Ok(u) => {
            let host = match (u.host_str(), u.port()) {
                (Some(h), Some(p)) => format!("{h}:{p}"),
                (Some(h), None) => h.to_string(),
                _ => String::new(),
            };
            (u.scheme().to_string(), host)
        }
        Err(_) => (String::new(), String::new()),
    }
}

fn header_str(headers: &HeaderMap, name: impl header::AsHeaderName) -> &str {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
}

impl Csrf {
    fn check_origin(&self, headers: &HeaderMap) -> Result<(), &'static str> {
        let req_host = header_str(headers, header::HOST);
        let origin = header_str(headers, header::ORIGIN);
        if !origin.is_empty() {
            // gorilla assumes https unless told the request is plaintext.
            let (scheme, host) = scheme_host(origin);
            let same = scheme == "https" && host == req_host;
            if !same && !self.trusted_origins.contains(&host) {
                return Err("origin invalid");
            }
            return Ok(());
        }
        let referer = header_str(headers, header::REFERER);
        if referer.is_empty() {
            return Err("referer not supplied");
        }
        let (scheme, host) = scheme_host(referer);
        if scheme == "http" {
            return Err("referer invalid");
        }
        if !host.is_empty() && host != req_host && !self.trusted_origins.contains(&host) {
            return Err("referer invalid");
        }
        Ok(())
    }

    fn cookie(&self, real: &[u8]) -> String {
        format!(
            "{COOKIE_NAME}={}; Path=/; Max-Age={MAX_AGE_SECS}; HttpOnly{}; SameSite=Lax",
            URL_SAFE_NO_PAD.encode(real),
            if self.secure { "; Secure" } else { "" }
        )
    }
}

fn forbidden(reason: &str) -> Response {
    (
        StatusCode::FORBIDDEN,
        [
            (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        format!("Forbidden - {reason}\n"),
    )
        .into_response()
}

pub async fn protect(State(csrf): State<Arc<Csrf>>, req: Request, next: Next) -> Response {
    let (mut parts, mut body) = req.into_parts();

    let existing = CookieJar::from_headers(&parts.headers)
        .get(COOKIE_NAME)
        .and_then(|c| URL_SAFE_NO_PAD.decode(c.value()).ok())
        .filter(|t| t.len() == TOKEN_LEN);
    let (real, set_cookie) = match existing {
        Some(t) => (t, None),
        None => {
            let t: [u8; TOKEN_LEN] = rand::random();
            (t.to_vec(), Some(csrf.cookie(&t)))
        }
    };
    parts.extensions.insert(CsrfToken(mask(&real)));

    let safe = matches!(
        parts.method,
        Method::GET | Method::HEAD | Method::OPTIONS | Method::TRACE
    );
    let verdict = if safe {
        Ok(())
    } else {
        match csrf.check_origin(&parts.headers) {
            Err(e) => Err(e),
            Ok(()) => {
                let mut issued = header_str(&parts.headers, HEADER_NAME).to_string();
                let is_form = header_str(&parts.headers, header::CONTENT_TYPE)
                    .starts_with("application/x-www-form-urlencoded");
                let has_body = matches!(parts.method, Method::POST | Method::PUT | Method::PATCH);
                if issued.is_empty() && is_form && has_body {
                    let bytes = axum::body::to_bytes(body, MAX_FORM_BYTES)
                        .await
                        .unwrap_or_default();
                    issued = url::form_urlencoded::parse(&bytes)
                        .find(|(k, _)| k == FIELD_NAME)
                        .map(|(_, v)| v.into_owned())
                        .unwrap_or_default();
                    body = Body::from(bytes);
                }
                if issued.is_empty() {
                    Err("CSRF token not found in request")
                } else {
                    match STANDARD.decode(&issued).ok().and_then(|d| unmask(&d)) {
                        Some(t) if constant_time_eq(&t, &real) => Ok(()),
                        _ => Err("CSRF token invalid"),
                    }
                }
            }
        }
    };

    let mut resp = match verdict {
        Ok(()) => {
            let mut r = next.run(Request::from_parts(parts, body)).await;
            r.headers_mut()
                .append(header::VARY, "Cookie".parse().unwrap());
            r
        }
        Err(reason) => {
            tracing::warn!("CSRF check failed: {reason}");
            forbidden(reason)
        }
    };
    if let Some(c) = set_cookie {
        resp.headers_mut()
            .append(header::SET_COOKIE, c.parse().unwrap());
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mask_roundtrip() {
        let real: [u8; 32] = rand::random();
        let a = mask(&real);
        let b = mask(&real);
        assert_ne!(a, b, "tokens are masked per request");
        let decoded = STANDARD.decode(a).unwrap();
        assert_eq!(unmask(&decoded).unwrap(), real.to_vec());
        assert!(unmask(&decoded[1..]).is_none());
        assert!(!constant_time_eq(&real, &real[1..]));
    }

    #[test]
    fn origin_rules() {
        let c = Csrf {
            trusted_origins: vec!["localhost:8080".into()],
            secure: false,
        };
        let mut h = HeaderMap::new();
        h.insert(header::HOST, "localhost:8080".parse().unwrap());
        assert_eq!(c.check_origin(&h), Err("referer not supplied"));
        h.insert(header::REFERER, "http://localhost:8080/x".parse().unwrap());
        assert_eq!(c.check_origin(&h), Err("referer invalid"));
        h.insert(header::ORIGIN, "http://localhost:8080".parse().unwrap());
        assert_eq!(c.check_origin(&h), Ok(()));
        h.insert(header::ORIGIN, "https://evil.example".parse().unwrap());
        assert_eq!(c.check_origin(&h), Err("origin invalid"));
        h.insert(header::ORIGIN, "null".parse().unwrap());
        assert_eq!(c.check_origin(&h), Err("origin invalid"));
        h.insert(header::HOST, "erase.kampa.app".parse().unwrap());
        h.insert(header::ORIGIN, "https://erase.kampa.app".parse().unwrap());
        assert_eq!(c.check_origin(&h), Ok(()), "same-origin https");
    }
}
