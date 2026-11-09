//! Router-level tests for the web UI (no network, no real email).

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use eraser::broker::{Broker, BrokerDatabase};
use eraser::config::{self, Config};
use eraser::email::{Message, SendResult, Sender};
use eraser::history::{Record, Status, Store};
use eraser::web::{AppOptions, app};
use futures::future::BoxFuture;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

#[derive(Default, Clone)]
struct MockSender(Arc<Mutex<Vec<Message>>>);

impl Sender for MockSender {
    fn send<'a>(&'a self, msg: &'a Message, _seq: Option<usize>) -> BoxFuture<'a, SendResult> {
        self.0.lock().unwrap().push(msg.clone());
        Box::pin(async {
            SendResult {
                success: true,
                message_id: "mock".into(),
                error: None,
            }
        })
    }
    fn name(&self) -> &'static str {
        "mock"
    }
}

fn broker(id: &str, name: &str, category: &str, region: &str) -> Broker {
    Broker {
        id: id.into(),
        name: name.into(),
        email: format!("privacy@{id}.example"),
        category: category.into(),
        region: region.into(),
        ..Default::default()
    }
}

struct Harness {
    router: Router,
    dir: tempfile::TempDir,
    mail: MockSender,
    history: Store,
    cookies: HashMap<String, String>,
    origin: &'static str,
    host: &'static str,
}

fn configured() -> Config {
    let mut c = Config::default();
    c.profile.first_name = "Ada".into();
    c.profile.last_name = "Lovelace".into();
    c.profile.email = "ada@example.com".into();
    c.email.provider = "smtp".into();
    c.email.from = "ada@example.com".into();
    c.options.template = "auto".into();
    c.options.rate_limit_ms = 1;
    c
}

fn harness(cfg: Option<Config>, public_origin: Option<&str>) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let history = Store::new(dir.path().join("history.db")).unwrap();
    let mail = MockSender::default();
    let m = mail.clone();
    let (router, _) = app(AppOptions {
        config: cfg,
        config_path: dir.path().join(".eraser/config.yaml"),
        brokers: BrokerDatabase {
            brokers: vec![
                broker("acme", "Acme People Search", "people-search", "us"),
                broker("swissdata", "Swiss Data AG", "marketing", "ch"),
                broker("eurolist", "EuroList", "marketing", "eu"),
            ],
        },
        history: history.clone(),
        data_dir: dir.path().join(".eraser"),
        port: 8080,
        public_origin: public_origin.map(String::from),
        sender_factory: Arc::new(move |_| Ok(Box::new(m.clone()) as Box<dyn Sender>)),
    })
    .unwrap();
    Harness {
        router,
        dir,
        mail,
        history,
        cookies: HashMap::new(),
        origin: "http://localhost:8080",
        host: "localhost:8080",
    }
}

struct Resp {
    status: StatusCode,
    headers: axum::http::HeaderMap,
    body: String,
}

impl Harness {
    async fn call(&mut self, req: Request<Body>) -> Resp {
        let resp = self.router.clone().oneshot(req).await.unwrap();
        for c in resp.headers().get_all(header::SET_COOKIE) {
            let (kv, _) = c.to_str().unwrap().split_once(';').unwrap();
            let (k, v) = kv.split_once('=').unwrap();
            if v.is_empty() {
                self.cookies.remove(k);
            } else {
                self.cookies.insert(k.into(), v.into());
            }
        }
        let status = resp.status();
        let headers = resp.headers().clone();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        Resp {
            status,
            headers,
            body: String::from_utf8(bytes.to_vec()).unwrap(),
        }
    }

    fn req(&self, method: &str, uri: &str) -> axum::http::request::Builder {
        let cookie: Vec<String> = self
            .cookies
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect();
        Request::builder()
            .method(method)
            .uri(uri)
            .header(header::HOST, self.host)
            .header(header::COOKIE, cookie.join("; "))
    }

    async fn get(&mut self, uri: &str) -> Resp {
        let r = self.req("GET", uri).body(Body::empty()).unwrap();
        self.call(r).await
    }

    /// Form POST with the CSRF token from `token_page` in the hidden field.
    async fn post_form(&mut self, uri: &str, token: &str, fields: &[(&str, &str)]) -> Resp {
        let mut body = url::form_urlencoded::Serializer::new(String::new());
        body.append_pair("gorilla.csrf.Token", token);
        for (k, v) in fields {
            body.append_pair(k, v);
        }
        let r = self
            .req("POST", uri)
            .header(header::ORIGIN, self.origin)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(body.finish()))
            .unwrap();
        self.call(r).await
    }

    /// HTMX-style POST with the token in X-CSRF-Token.
    async fn post_htmx(&mut self, uri: &str, token: &str, form: &str) -> Resp {
        let r = self
            .req("POST", uri)
            .header(header::ORIGIN, self.origin)
            .header("X-CSRF-Token", token)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(form.to_string()))
            .unwrap();
        self.call(r).await
    }

    async fn token(&mut self, page: &str) -> String {
        let r = self.get(page).await;
        assert_eq!(r.status, StatusCode::OK, "{page}: {}", r.body);
        token_from(&r.body)
    }
}

fn token_from(html: &str) -> String {
    let start = html
        .find(r#"<meta name="csrf-token" content=""#)
        .expect("csrf meta")
        + 33;
    let end = start + html[start..].find('"').unwrap();
    // minijinja escapes '/' in attributes; undo HTML entities like the browser does
    html[start..end]
        .replace("&#x2f;", "/")
        .replace("&#43;", "+")
        .replace("&#x3d;", "=")
}

fn location(r: &Resp) -> &str {
    r.headers.get(header::LOCATION).unwrap().to_str().unwrap()
}

#[tokio::test]
async fn setup_wizard_full_flow() {
    let mut h = harness(None, None);
    let r = h.get("/").await;
    assert_eq!((r.status, location(&r)), (StatusCode::FOUND, "/setup"));
    assert!(h.get("/setup").await.body.contains("Welcome"));

    // email step without a session bounces back to profile
    let r = h.get("/setup/email").await;
    assert_eq!(location(&r), "/setup/profile");

    let t = h.token("/setup/profile").await;
    let r = h
        .post_form(
            "/setup/profile",
            &t,
            &[("first_name", "Ada"), ("last_name", ""), ("email", "nope")],
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(
        r.body.contains("Last name is required")
            && r.body.contains("Please enter a valid email address")
    );

    let r = h
        .post_form(
            "/setup/profile",
            &t,
            &[
                ("first_name", " Ada "),
                ("last_name", "Lovelace"),
                ("email", "ada@example.com"),
                ("country", "CH"),
            ],
        )
        .await;
    assert_eq!(
        (r.status, location(&r)),
        (StatusCode::FOUND, "/setup/email")
    );
    let session_cookie = r.headers.get(header::SET_COOKIE).unwrap().to_str().unwrap();
    assert!(
        session_cookie.starts_with("eraser_session=")
            && session_cookie.contains("HttpOnly")
            && session_cookie.contains("SameSite=Strict")
    );
    assert!(!session_cookie.contains("Secure"));

    let r = h.get("/setup/email").await;
    assert!(r.body.contains("smtp.mail.me.com"), "iCloud defaults");
    let t = token_from(&r.body);
    let r = h
        .post_form(
            "/setup/email",
            &t,
            &[
                ("smtp_host", "smtp.mail.me.com"),
                ("smtp_port", "587"),
                ("smtp_username", "ada@icloud.com"),
                ("smtp_password", ""),
            ],
        )
        .await;
    assert!(
        r.body.contains("App password is required")
            && r.body.contains("TLS is required for iCloud")
    );
    let r = h
        .post_form(
            "/setup/email",
            &t,
            &[
                ("smtp_host", "smtp.mail.me.com"),
                ("smtp_port", "587"),
                ("smtp_username", "ada@icloud.com"),
                ("smtp_password", "app-pw"),
                ("smtp_tls", "on"),
            ],
        )
        .await;
    assert_eq!(location(&r), "/setup/test");

    let t = h.token("/setup/test").await;
    let r = h.post_htmx("/setup/test/send", &t, "").await;
    assert!(r.body.contains("Success!"), "{}", r.body);
    {
        let sent = h.mail.0.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(
            (sent[0].to.as_str(), sent[0].from.as_str()),
            ("ada@example.com", "ada@icloud.com")
        );
    }

    let r = h.get("/setup/complete").await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.body.contains("Ada"));
    assert!(!h.cookies.contains_key("eraser_session"), "session cleared");
    let cfg = config::load(h.dir.path().join(".eraser/config.yaml")).unwrap();
    assert_eq!(cfg.profile.first_name, "Ada");
    assert_eq!(cfg.email.smtp.password, "app-pw");
    assert_eq!(cfg.options.template, "auto");

    let r = h.get("/").await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.body.contains("Dashboard"));
}

#[tokio::test]
async fn broker_filters() {
    let mut h = harness(Some(configured()), None);
    let r = h.get("/brokers?category=marketing").await;
    assert!(
        r.body.contains("Swiss Data AG")
            && r.body.contains("EuroList")
            && !r.body.contains("Acme People Search")
    );
    let r = h.get("/brokers?region=CH").await;
    assert!(r.body.contains("Swiss Data AG") && !r.body.contains("EuroList"));
    let r = h.get("/api/brokers?search=acme").await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.body.contains("Acme People Search") && !r.body.contains("Swiss Data AG"));
    assert!(!r.body.contains("<!DOCTYPE"), "partial has no layout");

    h.history
        .add(&mut Record {
            broker_id: "acme".into(),
            broker_name: "Acme People Search".into(),
            status: Status::Sent,
            sent_at: Some(chrono::Local::now()),
            ..Default::default()
        })
        .unwrap();
    let r = h.get("/api/brokers?status=pending").await;
    assert!(!r.body.contains("Acme People Search") && r.body.contains("EuroList"));
    let r = h.get("/api/stats").await;
    assert_eq!(r.body, r#"{"total_brokers":3,"sent":1,"failed":0}"#);
}

#[tokio::test]
async fn history_page_filters() {
    let mut h = harness(Some(configured()), None);
    for (id, status, err) in [
        ("acme", Status::Sent, ""),
        ("eurolist", Status::Failed, "smtp boom"),
    ] {
        h.history
            .add(&mut Record {
                broker_id: id.into(),
                broker_name: format!("Name-{id}"),
                status,
                error: err.into(),
                sent_at: Some(chrono::Local::now()),
                ..Default::default()
            })
            .unwrap();
    }
    let all = h.get("/history").await;
    assert!(all.body.contains("Name-acme") && all.body.contains("Name-eurolist"));
    let failed = h.get("/history?status=failed").await;
    assert!(failed.body.contains("Name-eurolist") && !failed.body.contains("Name-acme"));

    let t = token_from(&all.body);
    let r = h
        .call(
            h.req("DELETE", "/api/history/failed")
                .header(header::ORIGIN, h.origin)
                .header("X-CSRF-Token", &t)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(
        r.body,
        "{\"deleted\":1,\"message\":\"Deleted 1 failed records\"}\n"
    );
}

#[tokio::test]
async fn settings_inbox_save() {
    let mut h = harness(Some(configured()), None);
    let t = h.token("/settings").await;
    let r = h
        .post_form(
            "/settings/inbox",
            &t,
            &[("inbox_email", ""), ("inbox_password", "x")],
        )
        .await;
    assert!(r.body.contains("Email and password are required"));
    let r = h
        .post_form(
            "/settings/inbox",
            &t,
            &[("inbox_email", "ada@icloud.com"), ("inbox_password", "pw")],
        )
        .await;
    assert!(r.body.contains("Inbox monitoring enabled successfully!"));
    let cfg = config::load(h.dir.path().join(".eraser/config.yaml")).unwrap();
    assert!(cfg.inbox.enabled);
    assert_eq!(
        (cfg.inbox.provider.as_str(), cfg.inbox.email.as_str()),
        ("icloud", "ada@icloud.com")
    );
    assert_eq!(
        cfg.inbox.server, "imap.mail.me.com",
        "load fills provider defaults"
    );
}

#[tokio::test]
async fn bulk_send_job_with_mock_sender() {
    let mut h = harness(Some(configured()), None);
    let t = h.token("/brokers").await;
    let r = h
        .post_htmx("/api/send-all", &t, "search=&category=marketing&region=")
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    let v: serde_json::Value = serde_json::from_str(&r.body).unwrap();
    assert_eq!(v["total"], 2);
    let id = v["job_id"].as_str().unwrap().to_string();

    let mut status = serde_json::Value::Null;
    for _ in 0..200 {
        status = serde_json::from_str(&h.get(&format!("/api/job/{id}/status")).await.body).unwrap();
        if status["status"] == "completed" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(status["status"], "completed", "{status}");
    assert_eq!(
        (
            status["sent"].as_i64(),
            status["progress"].as_i64(),
            status["daily_limit"].as_i64()
        ),
        (Some(2), Some(100), Some(250))
    );

    let sent = h.mail.0.lock().unwrap().clone();
    let mut to: Vec<_> = sent.iter().map(|m| m.to.as_str()).collect();
    to.sort();
    assert_eq!(
        to,
        ["privacy@eurolist.example", "privacy@swissdata.example"]
    );
    let mut subjects: Vec<_> = sent
        .iter()
        .map(|m| (m.to.as_str(), m.subject.as_str()))
        .collect();
    subjects.sort();
    assert!(
        subjects[0].1.contains("GDPR") && subjects[1].1.contains("FADP"),
        "auto law selection: {subjects:?}"
    );
    let mut used: Vec<_> = h
        .history
        .get_recent_requests(10)
        .unwrap()
        .into_iter()
        .map(|r| r.template)
        .collect();
    used.sort();
    assert_eq!(
        used,
        ["fadp", "gdpr"],
        "history stores the resolved template"
    );
    assert!(
        !h.dir.path().join(".eraser/pending_job.json").exists(),
        "state cleared"
    );
    assert_eq!(h.history.get_stats().unwrap(), (2, 2, 0));
    assert_eq!(h.get("/api/job/active").await.body, "{\"job\":null}\n");

    // everything is sent now: nothing pending
    let r = h.post_htmx("/api/send-all", &t, "category=marketing").await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert!(h.get("/api/job/nope/status").await.status == StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn csrf_rejections() {
    let mut h = harness(Some(configured()), None);
    let t = h.token("/settings").await;

    let r = h
        .call(
            h.req("POST", "/settings/inbox")
                .header(header::ORIGIN, h.origin)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(
        (r.status, r.body.as_str()),
        (
            StatusCode::FORBIDDEN,
            "Forbidden - CSRF token not found in request\n"
        )
    );

    let r = h
        .call(
            h.req("POST", "/api/send/acme")
                .header(header::ORIGIN, "https://evil.example")
                .header("X-CSRF-Token", &t)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(
        (r.status, r.body.as_str()),
        (StatusCode::FORBIDDEN, "Forbidden - origin invalid\n")
    );

    let r = h
        .call(
            h.req("POST", "/api/send/acme")
                .header("X-CSRF-Token", &t)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(r.body, "Forbidden - referer not supplied\n");

    let r = h
        .call(
            h.req("POST", "/api/send/acme")
                .header(header::ORIGIN, h.origin)
                .header("X-CSRF-Token", "AAAA")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(r.body, "Forbidden - CSRF token invalid\n");

    // a token minted for another cookie secret fails
    let mut other = harness(Some(configured()), None);
    let foreign = other.token("/settings").await;
    let r = h.post_htmx("/api/send/acme", &foreign, "").await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);

    // valid token passes the middleware (send goes to the mock)
    let r = h.post_htmx("/api/send/acme", &t, "").await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.body.contains("Sent"));
    assert_eq!(
        r.headers.get(header::CONTENT_TYPE).unwrap(),
        "text/plain; charset=utf-8",
        "Go sniffs <span> as text"
    );
}

#[tokio::test]
async fn public_origin_mode() {
    let mut h = harness(None, Some("https://erase.kampa.app"));
    h.origin = "https://erase.kampa.app";
    h.host = "eraser-container:8080";
    let r = h.get("/setup/profile").await;
    let csrf_cookie = r.headers.get(header::SET_COOKIE).unwrap().to_str().unwrap();
    assert!(
        csrf_cookie.starts_with("_gorilla_csrf=") && csrf_cookie.contains("; Secure"),
        "{csrf_cookie}"
    );
    let t = token_from(&r.body);
    let r = h
        .post_form(
            "/setup/profile",
            &t,
            &[
                ("first_name", "Ada"),
                ("last_name", "L"),
                ("email", "ada@example.com"),
            ],
        )
        .await;
    assert_eq!(r.status, StatusCode::FOUND, "{}", r.body);
    let session = r.headers.get(header::SET_COOKIE).unwrap().to_str().unwrap();
    assert!(session.contains("; Secure"));

    // localhost origins stay trusted, others are rejected
    h.origin = "https://other.kampa.app";
    let r = h.post_htmx("/setup/test/send", &t, "").await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn security_headers_and_static() {
    let mut h = harness(Some(configured()), None);
    let r = h.get("/").await;
    for (k, v) in [
        ("x-frame-options", "DENY"),
        ("x-content-type-options", "nosniff"),
        ("referrer-policy", "strict-origin-when-cross-origin"),
        (
            "cache-control",
            "no-store, no-cache, must-revalidate, private",
        ),
        (
            "permissions-policy",
            "camera=(), microphone=(), geolocation=(), payment=()",
        ),
        ("vary", "Cookie"),
    ] {
        assert_eq!(r.headers.get(k).unwrap(), v, "{k}");
    }
    assert!(
        r.headers
            .get("content-security-policy")
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("default-src 'self';")
    );
    let css = h.get("/static/css/tailwind.min.css").await;
    assert_eq!(css.status, StatusCode::OK);
    assert_eq!(
        css.headers.get(header::CONTENT_TYPE).unwrap(),
        "text/css; charset=utf-8"
    );
    assert!(css.headers.get(header::CACHE_CONTROL).is_none());
    let r = h.get("/nope").await;
    assert_eq!(
        (r.status, r.body.as_str()),
        (StatusCode::NOT_FOUND, "404 page not found\n")
    );
    assert_eq!(
        h.get("/forms").await.headers.get(header::LOCATION).unwrap(),
        "/tasks"
    );
    for page in [
        "/pipeline",
        "/tasks",
        "/settings",
        "/api/history",
        "/api/pipeline/responses",
        "/api/pipeline/tasks",
    ] {
        assert_eq!(h.get(page).await.status, StatusCode::OK, "{page}");
    }
    assert!(
        h.get("/api/pipeline/stats")
            .await
            .body
            .contains("\"needs_review\": 0")
    );
}
