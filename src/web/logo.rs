//! Broker / company logos: fetched server-side from the company's own site and cached in
//! ~/.eraser/logos, so the browser never contacts brokers or third parties.

use super::AppState;
use super::handlers::{St, http_error};
use axum::extract::Path;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use std::collections::HashMap;
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Semaphore;

const MAX_HTML: usize = 1 << 20;
const MAX_IMAGE: usize = 256 << 10;
const NEGATIVE_TTL: Duration = Duration::from_secs(7 * 24 * 3600);
const MAX_CANDIDATES: usize = 4;
const EXTS: &[(&str, &str)] = &[
    ("png", "image/png"),
    ("ico", "image/x-icon"),
    ("svg", "image/svg+xml"),
    ("jpg", "image/jpeg"),
    ("webp", "image/webp"),
];

pub struct Logos {
    dir: PathBuf,
    client: reqwest::Client,
    /// Tests only: plain http and private/loopback addresses allowed.
    allow_local: bool,
    fetches: Semaphore,
    in_flight: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            !(v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.is_unspecified()
                || v4.is_multicast()
                || a == 0
                || a >= 240
                || (a == 100 && (64..128).contains(&b)))
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_public(IpAddr::V4(v4));
            }
            let first = v6.segments()[0];
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (first & 0xfe00) == 0xfc00
                || (first & 0xffc0) == 0xfe80)
        }
    }
}

/// DNS resolver that drops non-public addresses (SSRF guard; also covers redirects).
struct PublicOnly;

impl Resolve for PublicOnly {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_string();
        Box::pin(async move {
            let addrs: Vec<_> = tokio::net::lookup_host((host.as_str(), 0))
                .await?
                .filter(|a| is_public(a.ip()))
                .collect();
            if addrs.is_empty() {
                return Err(format!("{host}: no public address").into());
            }
            Ok(Box::new(addrs.into_iter()) as Addrs)
        })
    }
}

/// https with a non-private host (IP literals bypass the resolver, so they are checked here).
fn url_allowed(u: &url::Url, allow_local: bool) -> bool {
    if allow_local {
        return matches!(u.scheme(), "http" | "https");
    }
    u.scheme() == "https"
        && match u.host() {
            Some(url::Host::Domain(_)) => true,
            Some(url::Host::Ipv4(ip)) => is_public(ip.into()),
            Some(url::Host::Ipv6(ip)) => is_public(ip.into()),
            None => false,
        }
}

/// Lower-case hostname with at least one dot and a non-numeric TLD (so no IP literals).
pub fn valid_hostname(s: &str) -> Option<String> {
    let s = s.trim().trim_end_matches('.').to_ascii_lowercase();
    let labels: Vec<_> = s.split('.').collect();
    let ok = s.len() <= 253
        && labels.len() >= 2
        && labels.iter().all(|l| {
            (1..=63).contains(&l.len())
                && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                && !l.starts_with('-')
                && !l.ends_with('-')
        })
        && !labels.last()?.bytes().all(|b| b.is_ascii_digit());
    ok.then_some(s)
}

/// Icon hrefs from `<link rel=...>`, best first: apple-touch-icon, then larger `sizes`.
fn icon_links(html: &str) -> Vec<String> {
    let doc = scraper::Html::parse_document(html);
    let sel = scraper::Selector::parse("link[rel][href]").expect("valid selector");
    let mut found: Vec<(u32, String)> = doc
        .select(&sel)
        .filter_map(|el| {
            let rel = el.value().attr("rel")?.to_ascii_lowercase();
            let tokens: Vec<_> = rel.split_ascii_whitespace().collect();
            let apple = tokens
                .iter()
                .any(|t| *t == "apple-touch-icon" || *t == "apple-touch-icon-precomposed");
            if !apple && !tokens.contains(&"icon") {
                return None;
            }
            let size = el
                .value()
                .attr("sizes")
                .unwrap_or("")
                .split_ascii_whitespace()
                .filter_map(|s| match s {
                    "any" => Some(512),
                    s => s.split(['x', 'X']).next()?.parse().ok(),
                })
                .max()
                .unwrap_or(if apple { 180 } else { 16 });
            Some((
                size + if apple { 10_000 } else { 0 },
                el.value().attr("href")?.into(),
            ))
        })
        .collect();
    found.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
    found.into_iter().map(|(_, h)| h).collect()
}

fn svg_safe(data: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(data) else {
        return false;
    };
    let lower = text.to_ascii_lowercase();
    static HANDLER: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"\bon\w+\s*=").expect("valid regex"));
    !lower.contains("<script") && !HANDLER.is_match(&lower)
}

/// Reads at most `limit` bytes; None if the body is larger.
async fn read_limited(mut resp: reqwest::Response, limit: usize) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    while let Some(chunk) = resp.chunk().await.ok()? {
        if out.len() + chunk.len() > limit {
            return None;
        }
        out.extend_from_slice(&chunk);
    }
    Some(out)
}

impl Logos {
    pub fn new(dir: PathBuf, allow_local: bool) -> Self {
        let redirect = reqwest::redirect::Policy::custom(move |attempt| {
            if attempt.previous().len() >= 3 {
                attempt.error("too many redirects")
            } else if !url_allowed(attempt.url(), allow_local) {
                attempt.error("redirect target not allowed")
            } else {
                attempt.follow()
            }
        });
        let mut builder = reqwest::Client::builder()
            .timeout(Duration::from_secs(8))
            .user_agent(concat!(
                "Eraser/",
                env!("CARGO_PKG_VERSION"),
                " (logo fetch)"
            ))
            .redirect(redirect)
            .no_proxy();
        if !allow_local {
            builder = builder.dns_resolver(PublicOnly);
        }
        Self {
            dir,
            client: builder.build().expect("http client"),
            allow_local,
            fetches: Semaphore::new(4),
            in_flight: Mutex::new(HashMap::new()),
        }
    }

    fn cached(&self, key: &str) -> Option<(Vec<u8>, &'static str)> {
        EXTS.iter().find_map(|(ext, ct)| {
            std::fs::read(self.dir.join(format!("{key}.{ext}")))
                .ok()
                .map(|d| (d, *ct))
        })
    }

    fn negative(&self, key: &str) -> bool {
        std::fs::metadata(self.dir.join(format!("{key}.none")))
            .and_then(|m| m.modified())
            .is_ok_and(|t| t.elapsed().unwrap_or_default() < NEGATIVE_TTL)
    }

    /// Cached logo for `key`, fetching it from `host` on a miss. `key` must be file-name safe.
    pub async fn get(&self, key: &str, host: &str) -> Option<(Vec<u8>, &'static str)> {
        if let Some(hit) = self.cached(key) {
            return Some(hit);
        }
        if self.negative(key) {
            return None;
        }
        let lock = self
            .in_flight
            .lock()
            .unwrap()
            .entry(key.to_string())
            .or_default()
            .clone();
        let _one = lock.lock().await;
        // Another request may have finished the fetch while we waited.
        let result = match self.cached(key) {
            Some(hit) => Some(hit),
            None if self.negative(key) => None,
            None => {
                let _permit = self.fetches.acquire().await.ok()?;
                let found = self.fetch(host).await;
                self.store(key, found.as_ref());
                found
            }
        };
        self.in_flight.lock().unwrap().remove(key);
        result
    }

    fn store(&self, key: &str, found: Option<&(Vec<u8>, &'static str)>) {
        let _ = std::fs::create_dir_all(&self.dir);
        let (name, data) = match found {
            Some((data, ct)) => {
                let ext = EXTS.iter().find(|(_, c)| c == ct).map_or("png", |(e, _)| e);
                (format!("{key}.{ext}"), data.as_slice())
            }
            None => (format!("{key}.none"), &[][..]),
        };
        if let Err(e) = std::fs::write(self.dir.join(name), data) {
            tracing::warn!("logo cache {key}: {e}");
        }
    }

    async fn fetch(&self, host: &str) -> Option<(Vec<u8>, &'static str)> {
        let scheme = if self.allow_local { "http" } else { "https" };
        let base = url::Url::parse(&format!("{scheme}://{host}/")).ok()?;
        if !url_allowed(&base, self.allow_local) {
            return None;
        }
        let mut candidates = Vec::new();
        if let Ok(resp) = self.client.get(base.clone()).send().await
            && resp.status().is_success()
        {
            let page = resp.url().clone();
            if let Some(html) = read_limited(resp, MAX_HTML).await {
                candidates.extend(
                    icon_links(&String::from_utf8_lossy(&html))
                        .iter()
                        .filter_map(|h| page.join(h).ok()),
                );
            }
        }
        candidates.truncate(MAX_CANDIDATES);
        candidates.push(base.join("/favicon.ico").ok()?);
        for url in candidates {
            if let Some(img) = self.fetch_image(url).await {
                return Some(img);
            }
        }
        None
    }

    async fn fetch_image(&self, url: url::Url) -> Option<(Vec<u8>, &'static str)> {
        if !url_allowed(&url, self.allow_local) {
            return None;
        }
        let resp = self.client.get(url).send().await.ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let ct = resp
            .headers()
            .get(header::CONTENT_TYPE)?
            .to_str()
            .ok()?
            .split(';')
            .next()?
            .trim()
            .to_ascii_lowercase();
        let ct = match ct.as_str() {
            "image/png" => "image/png",
            "image/x-icon" | "image/vnd.microsoft.icon" => "image/x-icon",
            "image/svg+xml" => "image/svg+xml",
            "image/jpeg" => "image/jpeg",
            "image/webp" => "image/webp",
            _ => return None,
        };
        let data = read_limited(resp, MAX_IMAGE).await?;
        if data.is_empty() || (ct == "image/svg+xml" && !svg_safe(&data)) {
            return None;
        }
        Some((data, ct))
    }
}

fn logo_response(found: Option<(Vec<u8>, &'static str)>) -> Response {
    let Some((data, ct)) = found else {
        return http_error(StatusCode::NOT_FOUND, "no logo");
    };
    let mut resp = (
        [
            (header::CONTENT_TYPE, ct),
            (header::CACHE_CONTROL, "private, max-age=604800"),
        ],
        data,
    )
        .into_response();
    if ct == "image/svg+xml" {
        resp.headers_mut().insert(
            header::CONTENT_SECURITY_POLICY,
            header::HeaderValue::from_static("default-src 'none'; style-src 'unsafe-inline'"),
        );
    }
    resp
}

/// Host to fetch a broker's logo from: its website, else its email domain.
fn broker_host(st: &AppState, id: &str) -> Option<String> {
    let b = st.brokers.find_by_id(id)?;
    let from_site = url::Url::parse(&b.website)
        .ok()
        .and_then(|u| u.host_str().map(String::from));
    from_site
        .or_else(|| b.email.rsplit_once('@').map(|(_, d)| d.to_string()))
        .and_then(|h| valid_hostname(&h))
}

/// GET /logo/{broker_id}
pub async fn broker_logo(axum::extract::State(st): St, Path(id): Path<String>) -> Response {
    let id = id.to_ascii_lowercase();
    if !id
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
    {
        return logo_response(None);
    }
    let Some(host) = broker_host(&st, &id) else {
        return logo_response(None);
    };
    logo_response(st.logos.get(&format!("broker_{id}"), &host).await)
}

/// GET /logo/domain/{domain}
pub async fn domain_logo(axum::extract::State(st): St, Path(domain): Path<String>) -> Response {
    let Some(host) = valid_hostname(&domain) else {
        return logo_response(None);
    };
    logo_response(st.logos.get(&format!("domain_{host}"), &host).await)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::routing::get;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\nfake";

    /// Fake site on 127.0.0.1; returns its host:port.
    async fn site(router: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        addr.to_string()
    }

    fn img(ct: &'static str, body: Vec<u8>) -> impl Fn() -> std::future::Ready<Response> + Clone {
        move || std::future::ready(([(header::CONTENT_TYPE, ct)], body.clone()).into_response())
    }

    fn html(body: &'static str) -> impl Fn() -> std::future::Ready<Response> + Clone {
        img("text/html", body.as_bytes().to_vec())
    }

    #[test]
    fn helpers() {
        assert!(!is_public("127.0.0.1".parse().unwrap()));
        assert!(!is_public("10.1.2.3".parse().unwrap()));
        assert!(!is_public("169.254.169.254".parse().unwrap()));
        assert!(!is_public("100.64.0.1".parse().unwrap()));
        assert!(!is_public("::1".parse().unwrap()));
        assert!(!is_public("fd00::1".parse().unwrap()));
        assert!(!is_public("::ffff:192.168.0.1".parse().unwrap()));
        assert!(is_public("93.184.216.34".parse().unwrap()));
        assert_eq!(
            valid_hostname("WWW.Example.com."),
            Some("www.example.com".into())
        );
        for bad in [
            "localhost",
            "127.0.0.1",
            "a..b",
            "-a.com",
            "a b.com",
            "x.com/..",
            "a.com:80",
        ] {
            assert_eq!(valid_hostname(bad), None, "{bad}");
        }
        let u = |s| url::Url::parse(s).unwrap();
        assert!(url_allowed(&u("https://example.com/x"), false));
        assert!(!url_allowed(&u("http://example.com/x"), false));
        assert!(!url_allowed(&u("https://127.0.0.1/"), false));
        assert!(!url_allowed(&u("https://[::1]/"), false));
        assert!(svg_safe(b"<svg><path d='M0'/></svg>"));
        assert!(!svg_safe(b"<svg><SCRIPT>alert(1)</script></svg>"));
        assert!(!svg_safe(b"<svg onload =\"x()\"></svg>"));
        assert_eq!(
            icon_links(
                r#"<link rel="icon" href="/a.png" sizes="16x16"><link rel="shortcut icon" href="/b.ico">
                <link rel="icon" href="/c.png" sizes="64x64"><link rel="apple-touch-icon" href="/d.png">
                <link rel="stylesheet" href="/s.css">"#
            ),
            ["/d.png", "/c.png", "/a.png", "/b.ico"]
        );
    }

    #[tokio::test]
    async fn discovers_icons_and_caches() {
        let hits = Arc::new(AtomicUsize::new(0));
        let h = hits.clone();
        let host = site(
            Router::new()
                .route(
                    "/",
                    get(move || {
                        h.fetch_add(1, Ordering::SeqCst);
                        html(r#"<link rel="icon" href="/missing.png"><link rel="apple-touch-icon" href="/big.png">"#)()
                    }),
                )
                .route("/big.png", get(img("image/png", PNG.to_vec()))),
        )
        .await;
        let dir = tempfile::tempdir().unwrap();
        let logos = Logos::new(dir.path().into(), true);
        assert_eq!(
            logos.get("k", &host).await,
            Some((PNG.to_vec(), "image/png"))
        );
        assert!(dir.path().join("k.png").exists());
        assert_eq!(logos.get("k", &host).await.unwrap().1, "image/png");
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "second call served from cache"
        );
    }

    #[tokio::test]
    async fn favicon_fallback_and_limits() {
        let host = site(
            Router::new()
                .route("/", get(html("<html>no icons</html>")))
                .route(
                    "/favicon.ico",
                    get(img("image/vnd.microsoft.icon", b"ico".to_vec())),
                )
                .route("/huge.png", get(img("image/png", vec![0; MAX_IMAGE + 1]))),
        )
        .await;
        let dir = tempfile::tempdir().unwrap();
        let logos = Logos::new(dir.path().into(), true);
        assert_eq!(
            logos.get("f", &host).await,
            Some((b"ico".to_vec(), "image/x-icon"))
        );
        // Too large / wrong type are skipped.
        let u = url::Url::parse(&format!("http://{host}/huge.png")).unwrap();
        assert_eq!(logos.fetch_image(u).await, None);
        let u = url::Url::parse(&format!("http://{host}/")).unwrap();
        assert_eq!(logos.fetch_image(u).await, None, "text/html is no image");
    }

    #[tokio::test]
    async fn rejects_svg_with_script_and_negative_caches() {
        let hits = Arc::new(AtomicUsize::new(0));
        let h = hits.clone();
        let host = site(
            Router::new()
                .route(
                    "/",
                    get(move || {
                        h.fetch_add(1, Ordering::SeqCst);
                        html(r#"<link rel="icon" href="/x.svg" sizes="any">"#)()
                    }),
                )
                .route(
                    "/x.svg",
                    get(img(
                        "image/svg+xml",
                        b"<svg><script>alert(1)</script></svg>".to_vec(),
                    )),
                ),
        )
        .await;
        let dir = tempfile::tempdir().unwrap();
        let logos = Logos::new(dir.path().into(), true);
        assert_eq!(logos.get("s", &host).await, None);
        assert!(dir.path().join("s.none").exists());
        assert_eq!(logos.get("s", &host).await, None);
        assert_eq!(hits.load(Ordering::SeqCst), 1, "negative cache hit");
    }

    #[tokio::test]
    async fn ssrf_guard_refuses_localhost() {
        let hits = Arc::new(AtomicUsize::new(0));
        let h = hits.clone();
        let host = site(Router::new().fallback(move || {
            h.fetch_add(1, Ordering::SeqCst);
            img("image/png", PNG.to_vec())()
        }))
        .await;
        let dir = tempfile::tempdir().unwrap();
        let logos = Logos::new(dir.path().into(), false);
        assert_eq!(logos.get("a", &host).await, None);
        assert_eq!(logos.fetch("localhost").await, None);
        assert_eq!(
            hits.load(Ordering::SeqCst),
            0,
            "no request reached the local server"
        );
    }
}
