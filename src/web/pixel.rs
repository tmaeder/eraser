//! `GET /p/{token}(.gif)`: the open-tracking pixel. Public (outside session/CSRF, no cookies)
//! and always the same GIF, so it reveals nothing about valid tokens. IPs are never read.

use super::AppState;
use crate::history::Store;
use axum::extract::{FromRef, Path, State};
use axum::http::{HeaderMap, header};
use axum::response::{IntoResponse, Response};
use std::sync::Arc;

/// 1x1 transparent GIF.
const GIF: [u8; 43] = [
    0x47, 0x49, 0x46, 0x38, 0x39, 0x61, 0x01, 0x00, 0x01, 0x00, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
    0xff, 0xff, 0xff, 0x21, 0xf9, 0x04, 0x01, 0x00, 0x00, 0x00, 0x00, 0x2c, 0x00, 0x00, 0x00, 0x00,
    0x01, 0x00, 0x01, 0x00, 0x00, 0x02, 0x02, 0x44, 0x01, 0x00, 0x3b,
];

/// Coarse client class from the User-Agent alone.
/// ponytail: Apple Mail Privacy Protection proxies send the bare macOS WebKit UA, which Apple
/// Mail on a Mac without MPP sends too; telling them apart needs Apple's proxy IP ranges.
pub fn client_class(user_agent: &str) -> &'static str {
    if user_agent.contains("GoogleImageProxy") {
        "google-proxy"
    } else if user_agent
        .trim()
        .ends_with("(Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko)")
    {
        "apple-mpp"
    } else {
        crate::history::CLIENT_OTHER
    }
}

impl FromRef<Arc<AppState>> for Store {
    fn from_ref(st: &Arc<AppState>) -> Store {
        st.history.clone()
    }
}

pub async fn pixel(
    State(history): State<Store>,
    Path(token): Path<String>,
    headers: HeaderMap,
) -> Response {
    let token = token.strip_suffix(".gif").unwrap_or(&token);
    let ua = headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    if let Err(e) = history.record_open(token, client_class(ua), chrono::Utc::now()) {
        tracing::warn!("pixel: {e:#}");
    }
    (
        [
            (header::CONTENT_TYPE, "image/gif"),
            (header::CACHE_CONTROL, "no-store, private"),
        ],
        GIF,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::{Record, Status};
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    const TOKEN: &str = "AAAAAAAAAAAAAAAAAAAAAA";

    async fn get(router: &axum::Router, path: &str, ua: &str) -> Response {
        let req = Request::get(path)
            .header(header::USER_AGENT, ua)
            .body(Body::empty())
            .unwrap();
        router.clone().oneshot(req).await.unwrap()
    }

    async fn check_gif(resp: Response) {
        assert_eq!(resp.status(), 200);
        assert!(resp.headers().get(header::SET_COOKIE).is_none());
        assert_eq!(resp.headers()[header::CONTENT_TYPE], "image/gif");
        let cache = resp.headers()[header::CACHE_CONTROL].to_str().unwrap();
        assert!(cache.contains("no-store") && cache.contains("private"));
        let body = axum::body::to_bytes(resp.into_body(), 1000).await.unwrap();
        assert_eq!(body.as_ref(), GIF.as_slice());
    }

    #[tokio::test]
    async fn records_first_open_once_and_ignores_unknown_tokens() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join("h.db")).unwrap();
        let mut rec = Record {
            broker_id: "b".into(),
            broker_name: "B".into(),
            email: "p@b.com".into(),
            template: "gdpr".into(),
            status: Status::Sent,
            message_id: format!("<{TOKEN}@example.com>"),
            ..Default::default()
        };
        store.add(&mut rec).unwrap();
        let router = axum::Router::new()
            .route("/p/{token}", axum::routing::get(pixel))
            .with_state(store.clone());

        check_gif(get(&router, &format!("/p/{TOKEN}.gif"), "GoogleImageProxy").await).await;
        check_gif(get(&router, &format!("/p/{TOKEN}"), "Mozilla/5.0 Firefox").await).await;
        check_gif(get(&router, "/p/unknownunknownunknown1.gif", "x").await).await;
        let t = &store.request_tracking().unwrap()[&rec.id];
        assert_eq!(t.open_count, 1, "second hit within 10 s ignored");
        assert_eq!(t.client, "google-proxy");
        assert!(t.first_opened_at.is_some());
        assert_eq!(store.request_tracking().unwrap().len(), 1);
    }

    #[test]
    fn client_classes() {
        assert_eq!(
            client_class(
                "Mozilla/5.0 (Windows NT 5.1; rv:11.0) Gecko Firefox/11.0 (via ggpht.com GoogleImageProxy)"
            ),
            "google-proxy"
        );
        assert_eq!(
            client_class(
                "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko)"
            ),
            "apple-mpp"
        );
        assert_eq!(
            client_class(
                "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 Safari/605.1.15"
            ),
            "other"
        );
        assert_eq!(client_class(""), "other");
    }
}
