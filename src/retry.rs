//! Retry with exponential backoff + jitter for transient network failures (SMTP, IMAP, HTTP).

use std::future::Future;
use std::time::Duration;

/// Longest wait honoured from a server's `Retry-After`.
pub const MAX_RETRY_AFTER: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy)]
pub struct Policy {
    /// Total tries, including the first one.
    pub attempts: u32,
    /// Delay before the second try; doubles each time.
    pub base: Duration,
    pub max_delay: Duration,
}

impl Policy {
    pub const fn new(attempts: u32, base: Duration) -> Self {
        Policy {
            attempts,
            base,
            max_delay: Duration::from_secs(10),
        }
    }

    /// Backoff before try `n + 1` (n ≥ 1): base·2^(n-1), capped, with jitter in [½, 1].
    pub fn delay(&self, n: u32) -> Duration {
        let exp = self
            .base
            .saturating_mul(1 << (n.saturating_sub(1)).min(16))
            .min(self.max_delay);
        let half = exp / 2;
        half + half.mul_f64(rand::random::<f64>())
    }
}

/// What to do after a failed try.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Permanent: give up now.
    Stop,
    /// Transient: back off and try again.
    Retry,
    /// Transient, and the server said when (capped at [`MAX_RETRY_AFTER`]).
    RetryAfter(Duration),
}

/// Runs `op` until it succeeds, `classify` says [`Verdict::Stop`], or attempts run out.
/// Returns the last result.
pub async fn run<T, E, Fut>(
    policy: Policy,
    mut op: impl FnMut() -> Fut,
    classify: impl Fn(&E) -> Verdict,
) -> Result<T, E>
where
    Fut: Future<Output = Result<T, E>>,
{
    let mut n = 1;
    loop {
        let err = match op().await {
            Ok(v) => return Ok(v),
            Err(e) => e,
        };
        let wait = match classify(&err) {
            _ if n >= policy.attempts => return Err(err),
            Verdict::Stop => return Err(err),
            Verdict::Retry => policy.delay(n),
            Verdict::RetryAfter(d) => d.min(MAX_RETRY_AFTER),
        };
        tracing::debug!("transient failure, retry {n} in {wait:?}");
        tokio::time::sleep(wait).await;
        n += 1;
    }
}

/// Parses `Retry-After` (delay-seconds or HTTP-date).
pub fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let v = headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim();
    if let Ok(secs) = v.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    let at = chrono::DateTime::parse_from_rfc2822(v).ok()?;
    Some(
        (at.to_utc() - chrono::Utc::now())
            .to_std()
            .unwrap_or_default(),
    )
}

/// Error or retryable response from [`http`].
enum HttpFailure {
    Error(reqwest::Error),
    Status(reqwest::Response),
}

/// Sends the request built by `build`, retrying connect errors always and — only when
/// `idempotent` — timeouts, 429 and 5xx (honouring `Retry-After`). A non-idempotent request
/// (e.g. a one-click unsubscribe POST) is retried only when it provably never reached the
/// server. The final 429/5xx response is returned as `Ok`, like any other status.
pub async fn http(
    policy: Policy,
    mut build: impl FnMut() -> reqwest::RequestBuilder,
    idempotent: bool,
) -> reqwest::Result<reqwest::Response> {
    let result = run(
        policy,
        || {
            let req = build();
            async move {
                let resp = req.send().await.map_err(HttpFailure::Error)?;
                let s = resp.status();
                if idempotent && (s.as_u16() == 429 || s.is_server_error()) {
                    return Err(HttpFailure::Status(resp));
                }
                Ok(resp)
            }
        },
        |f| match f {
            HttpFailure::Error(e) if e.is_connect() => Verdict::Retry,
            HttpFailure::Error(e) if idempotent && e.is_timeout() => Verdict::Retry,
            HttpFailure::Error(_) => Verdict::Stop,
            HttpFailure::Status(r) => {
                retry_after(r.headers()).map_or(Verdict::Retry, Verdict::RetryAfter)
            }
        },
    )
    .await;
    match result {
        Ok(r) | Err(HttpFailure::Status(r)) => Ok(r),
        Err(HttpFailure::Error(e)) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderMap, StatusCode};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Instant;

    const FAST: Policy = Policy::new(3, Duration::from_millis(1));

    /// Serves `responses` in order (the last one repeats); returns (base url, hit counter).
    async fn fake(responses: Vec<(u16, &'static str)>) -> (String, Arc<AtomicUsize>) {
        let hits = Arc::new(AtomicUsize::new(0));
        let h = hits.clone();
        let handler = move || {
            let h = h.clone();
            let responses = responses.clone();
            async move {
                let i = h.fetch_add(1, Ordering::SeqCst).min(responses.len() - 1);
                let (code, retry_after) = responses[i];
                let mut headers = HeaderMap::new();
                if !retry_after.is_empty() {
                    headers.insert("retry-after", retry_after.parse().unwrap());
                }
                (StatusCode::from_u16(code).unwrap(), headers, "body")
            }
        };
        let app = axum::Router::new().route("/", axum::routing::any(handler));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}/"), hits)
    }

    #[test]
    fn backoff_doubles_with_jitter_and_cap() {
        let p = Policy::new(5, Duration::from_millis(100));
        for (n, full) in [(1, 100), (2, 200), (3, 400)] {
            let d = p.delay(n).as_millis();
            assert!((full / 2..=full).contains(&d), "try {n}: {d}ms");
        }
        assert!(p.delay(30) <= p.max_delay);
    }

    #[tokio::test]
    async fn run_stops_on_permanent_and_after_max_attempts() {
        let calls = AtomicUsize::new(0);
        let r: Result<(), &str> = run(
            FAST,
            || async {
                calls.fetch_add(1, Ordering::SeqCst);
                Err("transient")
            },
            |_| Verdict::Retry,
        )
        .await;
        assert!(r.is_err());
        assert_eq!(calls.swap(0, Ordering::SeqCst), 3);

        let _: Result<(), &str> = run(
            FAST,
            || async {
                calls.fetch_add(1, Ordering::SeqCst);
                Err("permanent")
            },
            |_| Verdict::Stop,
        )
        .await;
        assert_eq!(calls.swap(0, Ordering::SeqCst), 1);

        let r = run(
            FAST,
            || async { Ok::<_, &str>(calls.fetch_add(1, Ordering::SeqCst)) },
            |_| Verdict::Retry,
        )
        .await;
        assert_eq!(r, Ok(0));
    }

    #[test]
    fn retry_after_seconds_and_date() {
        let mut h = reqwest::header::HeaderMap::new();
        assert_eq!(retry_after(&h), None);
        h.insert("retry-after", "7".parse().unwrap());
        assert_eq!(retry_after(&h), Some(Duration::from_secs(7)));
        h.insert(
            "retry-after",
            "Sun, 06 Nov 1994 08:49:37 GMT".parse().unwrap(),
        );
        assert_eq!(retry_after(&h), Some(Duration::ZERO), "past date = now");
    }

    #[tokio::test]
    async fn get_retries_5xx_and_429_honouring_retry_after() {
        let (url, hits) = fake(vec![(503, ""), (429, "1"), (200, "")]).await;
        let client = reqwest::Client::new();
        let start = Instant::now();
        let r = http(FAST, || client.get(&url), true).await.unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(hits.load(Ordering::SeqCst), 3);
        assert!(
            start.elapsed() >= Duration::from_secs(1),
            "Retry-After waited"
        );
    }

    #[tokio::test]
    async fn get_gives_up_with_last_response_and_skips_4xx() {
        let (url, hits) = fake(vec![(500, "")]).await;
        let client = reqwest::Client::new();
        let r = http(FAST, || client.get(&url), true).await.unwrap();
        assert_eq!((r.status().as_u16(), hits.load(Ordering::SeqCst)), (500, 3));

        let (url, hits) = fake(vec![(404, "")]).await;
        let r = http(FAST, || client.get(&url), true).await.unwrap();
        assert_eq!((r.status().as_u16(), hits.load(Ordering::SeqCst)), (404, 1));
    }

    #[tokio::test]
    async fn post_is_never_retried_once_it_reached_the_server() {
        let (url, hits) = fake(vec![(503, "0"), (200, "")]).await;
        let client = reqwest::Client::new();
        let r = http(FAST, || client.post(&url), false).await.unwrap();
        assert_eq!((r.status().as_u16(), hits.load(Ordering::SeqCst)), (503, 1));
    }

    #[tokio::test]
    async fn post_retries_connect_errors() {
        // Port closed on the first try, server up from the second try on.
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let url = format!("http://127.0.0.1:{port}/");
        let client = reqwest::Client::new();
        let tries = AtomicUsize::new(0);
        let r = http(
            FAST,
            || {
                if tries.fetch_add(1, Ordering::SeqCst) == 1 {
                    // Bind synchronously so the next send finds the port open.
                    let l = std::net::TcpListener::bind(("127.0.0.1", port)).unwrap();
                    l.set_nonblocking(true).unwrap();
                    let app =
                        axum::Router::new().route("/", axum::routing::post(|| async { "ok" }));
                    tokio::spawn(async move {
                        let l = tokio::net::TcpListener::from_std(l).unwrap();
                        axum::serve(l, app).await.unwrap();
                    });
                }
                client.post(&url)
            },
            false,
        )
        .await
        .unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(tries.load(Ordering::SeqCst), 2);
    }
}
