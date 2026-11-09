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

// ==================== Companies (inbox company scanner) ====================

fn seed_companies(store: &Store) {
    use eraser::history::ScannedMessage;
    let m = |key: &str, from: &str, list: bool, mailto: &str| ScannedMessage {
        key: key.into(),
        domain: from.rsplit_once('@').unwrap().1.into(),
        from_addr: from.into(),
        from_name: "Sender".into(),
        date: Some(chrono::Utc::now()),
        is_list: list,
        unsub_mailto: mailto.into(),
        ..Default::default()
    };
    store
        .add_scanned_messages(&[
            m(
                "<1>",
                "news@shop.de",
                true,
                "mailto:unsub@shop.de?subject=stop",
            ),
            m(
                "<2>",
                "news@shop.de",
                true,
                "mailto:unsub@shop.de?subject=stop",
            ),
            m("<3>", "friend@gmail.com", false, ""),
            m("<4>", "info@nocontact.ch", true, ""),
        ])
        .unwrap();
    let domains = ["shop.de", "gmail.com", "nocontact.ch"]
        .map(String::from)
        .into_iter()
        .collect();
    eraser::scanner::recompute(store, &eraser::scanner::Identity::default(), &domains).unwrap();
    store
        .set_company_contact(
            "shop.de",
            "website",
            "privacy@shop.de",
            "https://shop.de/datenschutz",
        )
        .unwrap();
    store
        .set_company_contact("nocontact.ch", "manual", "", "")
        .unwrap();
}

async fn wait_job(h: &mut Harness, id: &str) -> serde_json::Value {
    let mut status = serde_json::Value::Null;
    for _ in 0..300 {
        status = serde_json::from_str(&h.get(&format!("/api/job/{id}/status")).await.body).unwrap();
        if status["status"] == "completed" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    status
}

#[tokio::test]
async fn companies_page_filters_and_detail() {
    let mut h = harness(Some(configured()), None);
    seed_companies(&h.history);
    let r = h.get("/companies").await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(
        r.body
            .contains(r#"href="/companies" class="nav-link active""#)
    );
    assert!(r.body.contains("shop.de") && r.body.contains("nocontact.ch"));
    assert!(
        !r.body.contains("gmail.com"),
        "personal domains hidden by default"
    );
    let r = h.get("/companies?filter=excluded").await;
    assert!(r.body.contains("gmail.com") && r.body.contains("personal_domain"));
    let r = h.get("/companies?filter=manual").await;
    assert!(r.body.contains("nocontact.ch") && !r.body.contains(">shop.de<"));
    let r = h.get("/companies?q=shop").await;
    assert!(!r.body.contains("nocontact.ch"));

    let r = h.get("/companies/shop.de").await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert!(r.body.contains("privacy@shop.de"));
    assert!(
        r.body
            .replace("&#x2f;", "/")
            .contains("https://shop.de/datenschutz"),
        "source URL shown"
    );
    assert!(r.body.contains("news@shop.de"), "sender addresses");
    assert_eq!(
        h.get("/companies/unknown.example").await.status,
        StatusCode::NOT_FOUND
    );

    // ignore / un-ignore
    let t = token_from(&r.body);
    let r = h
        .post_form("/companies/shop.de/ignore", &t, &[("ignore", "1")])
        .await;
    assert_eq!(r.status, StatusCode::FOUND);
    assert_eq!(
        h.history.get_company("shop.de").unwrap().unwrap().decision,
        "ignored"
    );
    assert!(!h.get("/companies").await.body.contains(">shop.de<"));
    h.post_form("/companies/shop.de/ignore", &t, &[("ignore", "0")])
        .await;
    assert_eq!(
        h.history.get_company("shop.de").unwrap().unwrap().decision,
        ""
    );
    let log = h.history.company_actions("shop.de").unwrap();
    assert_eq!(log.len(), 2);
}

#[tokio::test]
async fn companies_bulk_request_and_unsubscribe_with_csrf() {
    let mut h = harness(Some(configured()), None);
    seed_companies(&h.history);
    let t = h.token("/companies").await;

    // CSRF: no token → 403, nothing planned or sent
    let r = h
        .call(
            h.req("POST", "/companies/confirm")
                .header(header::ORIGIN, h.origin)
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from("action=request&domain=shop.de"))
                .unwrap(),
        )
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let r = h
        .post_form(
            "/companies/execute",
            "bogus",
            &[
                ("action", "request"),
                ("confirmed", "yes"),
                ("domain", "shop.de"),
            ],
        )
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);

    // confirmation page lists exactly who gets contacted
    let r = h
        .post_form(
            "/companies/confirm",
            &t,
            &[
                ("action", "request"),
                ("domain", "shop.de"),
                ("domain", "nocontact.ch"),
            ],
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert!(
        r.body
            .contains("1 of 2 selected companies will be contacted")
    );
    assert!(r.body.contains("privacy@shop.de"));
    assert!(r.body.contains("no published privacy address (manual)"));
    assert!(
        r.body
            .contains(r#"<input type="hidden" name="domain" value="shop.de">"#)
    );
    assert!(!r.body.contains(r#"name="domain" value="nocontact.ch""#));
    assert!(h.mail.0.lock().unwrap().is_empty(), "confirm sends nothing");

    // execute without the confirmation flag is refused
    let r = h
        .post_form(
            "/companies/execute",
            &t,
            &[("action", "request"), ("domain", "shop.de")],
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);

    // execute: server re-plans, the manual company is skipped even if posted
    let r = h
        .post_form(
            "/companies/execute",
            &t,
            &[
                ("action", "request"),
                ("confirmed", "yes"),
                ("domain", "shop.de"),
                ("domain", "nocontact.ch"),
            ],
        )
        .await;
    assert_eq!(r.status, StatusCode::FOUND);
    let id = location(&r)
        .strip_prefix("/companies?job=")
        .unwrap()
        .to_string();
    let job = wait_job(&mut h, &id).await;
    assert_eq!(job["status"], "completed", "{job}");
    assert_eq!(
        (job["sent"].as_i64(), job["total"].as_i64()),
        (Some(1), Some(1))
    );
    assert_eq!(job["kind"], "company_request");
    {
        let sent = h.mail.0.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].to, "privacy@shop.de");
        assert!(
            sent[0].subject.contains("GDPR"),
            "auto → .de is EU: {}",
            sent[0].subject
        );
    }
    let rec = h
        .history
        .get_last_request_for_broker("company:shop.de")
        .unwrap()
        .unwrap();
    assert_eq!((rec.template.as_str(), rec.status), ("gdpr", Status::Sent));
    assert!(
        h.history
            .get_company("shop.de")
            .unwrap()
            .unwrap()
            .requested_at
            .is_some()
    );
    assert!(
        h.get("/companies?filter=requested")
            .await
            .body
            .contains("shop.de")
    );
    assert!(!h.dir.path().join(".eraser/pending_job.json").exists());

    // unsubscribe via mailto (subject from the URI)
    let r = h
        .post_form(
            "/companies/confirm",
            &t,
            &[
                ("action", "unsubscribe"),
                ("domain", "shop.de"),
                ("domain", "nocontact.ch"),
            ],
        )
        .await;
    assert!(r.body.contains("unsubscribe email") && r.body.contains("unsub@shop.de"));
    assert!(r.body.contains("no List-Unsubscribe header"));
    let r = h
        .post_form(
            "/companies/execute",
            &t,
            &[
                ("action", "unsubscribe"),
                ("confirmed", "yes"),
                ("domain", "shop.de"),
            ],
        )
        .await;
    let id = location(&r)
        .strip_prefix("/companies?job=")
        .unwrap()
        .to_string();
    assert_eq!(wait_job(&mut h, &id).await["sent"], 1);
    {
        let sent = h.mail.0.lock().unwrap();
        assert_eq!(
            (sent[1].to.as_str(), sent[1].subject.as_str()),
            ("unsub@shop.de", "stop")
        );
        assert_eq!(sent[1].from, "ada@example.com");
    }
    let detail = h.get("/companies/shop.de").await.body;
    assert!(detail.contains("unsubscribe_mailto") && detail.contains("request"));

    // nothing selected
    let r = h
        .post_form("/companies/confirm", &t, &[("action", "request")])
        .await;
    assert_eq!(r.status, StatusCode::FOUND);
}

#[tokio::test]
async fn companies_scan_without_inbox_and_public_origin() {
    let mut h = harness(Some(configured()), Some("https://erase.kampa.app"));
    h.origin = "https://erase.kampa.app";
    h.host = "erase.kampa.app";
    let t = h.token("/companies").await;
    let r = h.post_htmx("/api/companies/scan", &t, "days=30").await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.body.contains("Inbox monitoring not configured"));
    let r = h.post_htmx("/api/companies/discover", &t, "").await;
    assert!(r.body.contains("All companies checked"));
}

#[tokio::test]
async fn task_helper_forms_post_with_csrf() {
    let mut h = harness(Some(configured()), None);
    let mut task = eraser::history::PendingTask {
        broker_id: "acme".into(),
        broker_name: "Acme".into(),
        task_type: eraser::history::TaskType::Captcha,
        form_url: "https://acme.example/optout".into(),
        ..Default::default()
    };
    h.history.add_pending_task(&mut task).unwrap();
    let page = h.get(&format!("/tasks/{}/helper", task.id)).await;
    assert!(page.body.contains(r#"name="gorilla.csrf.Token""#));
    assert!(!page.body.contains(r#"name="csrf_token""#));
    // submit the helper form exactly as the browser would (hidden field from the page)
    let start = page
        .body
        .find(r#"name="gorilla.csrf.Token" value=""#)
        .unwrap()
        + 33;
    let field = &page.body[start..start + page.body[start..].find('"').unwrap()];
    let field = field
        .replace("&#x2f;", "/")
        .replace("&#43;", "+")
        .replace("&#x3d;", "=");
    let r = h
        .post_form(&format!("/tasks/{}/complete", task.id), &field, &[])
        .await;
    assert_eq!(r.status, StatusCode::FOUND, "{}", r.body);
    let done = h.history.get_pending_task_by_id(task.id).unwrap().unwrap();
    assert_eq!(done.status, "completed");
}
