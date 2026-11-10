use super::*;
use crate::email::{Message, SendResult, Sender};
use axum::Router;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use chrono::{TimeZone, Utc};
use futures::future::BoxFuture;
use std::sync::{Arc, Mutex};

fn msg(key: &str, from: &str, name: &str, day: u32, list: bool) -> ScannedMessage {
    ScannedMessage {
        key: key.into(),
        domain: domain::registrable_domain(from.rsplit_once('@').unwrap().1),
        from_addr: from.into(),
        from_name: name.into(),
        date: Some(Utc.with_ymd_and_hms(2026, 10, day, 12, 0, 0).unwrap()),
        is_list: list,
        ..Default::default()
    }
}

#[test]
fn aggregate_groups_display_name_and_unsubscribe() {
    let mut a = msg("1", "news@mail.shop.co.uk", "Shop News", 1, true);
    a.unsub_https = "https://shop.co.uk/old".into();
    let mut b = msg("2", "deals@shop.co.uk", "Shop", 5, true);
    b.unsub_https = "https://shop.co.uk/new".into();
    b.one_click = true;
    b.unsub_mailto = "mailto:u@shop.co.uk".into();
    let c = msg("3", "news@mail.shop.co.uk", "Shop News", 3, false);
    let agg = aggregate("shop.co.uk", &[a, b, c]);
    assert_eq!(agg.display_name, "Shop News", "most common From name");
    assert_eq!(agg.senders, ["deals@shop.co.uk", "news@mail.shop.co.uk"]);
    assert_eq!(agg.message_count, 3);
    assert_eq!(agg.first_seen.unwrap().format("%d").to_string(), "01");
    assert_eq!(agg.last_seen.unwrap().format("%d").to_string(), "05");
    assert!(agg.is_mailing_list);
    assert_eq!(agg.unsub_https, "https://shop.co.uk/new", "newest wins");
    assert!(agg.one_click);
    assert_eq!(agg.unsub_mailto, "mailto:u@shop.co.uk");
}

#[test]
fn exclusion_rules() {
    let mut cfg = Config::default();
    cfg.profile.email = "Tobias@Kampa.App".into();
    cfg.inbox.email = "me@icloud.com".into();
    let id = Identity::from_config(&cfg);
    assert!(id.domains.contains("kampa.app"));
    assert!(
        !id.domains.contains("icloud.com"),
        "freemail is not an own domain"
    );
    assert!(id.addresses.contains("me@icloud.com"));

    let agg = |d: &str, list: bool| CompanyAggregate {
        domain: d.into(),
        is_mailing_list: list,
        ..Default::default()
    };
    assert_eq!(exclusion_reason(&agg("kampa.app", true), &id), "own");
    assert_eq!(
        exclusion_reason(&agg("gmail.com", false), &id),
        "personal_domain"
    );
    assert_eq!(
        exclusion_reason(&agg("gmail.com", true), &id),
        "",
        "list mail kept"
    );
    assert_eq!(
        exclusion_reason(&agg("hotmail.co.uk", false), &id),
        "personal_domain"
    );
    assert_eq!(exclusion_reason(&agg("zalando.de", false), &id), "");
}

#[test]
fn folder_selection() {
    let existing: Vec<String> = [
        "INBOX",
        "Archive",
        "Eraser",
        "Sent Messages",
        "Promotions",
        "Lists/Newsletter",
    ]
    .map(String::from)
    .to_vec();
    let cfg = InboxConfig {
        folder: "INBOX".into(),
        archive_folder: "Eraser".into(),
        ..Default::default()
    };
    assert_eq!(
        pick_folders(&existing, &cfg, &[]),
        [
            "INBOX",
            "Archive",
            "Eraser",
            "Promotions",
            "Lists/Newsletter"
        ]
    );
    assert_eq!(
        pick_folders(&existing, &cfg, &["inbox".into(), "Missing".into()]),
        ["INBOX"],
        "explicit list, case-insensitive, missing folders skipped"
    );
    let cfg = InboxConfig {
        scan_folders: vec!["Sent Messages".into()],
        ..cfg
    };
    assert_eq!(pick_folders(&existing, &cfg, &[]), ["Sent Messages"]);
}

#[test]
fn store_rescan_is_incremental_and_keeps_decisions() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().join("h.db")).unwrap();
    let id = Identity::default();
    let msgs = vec![
        msg("<a@x>", "news@shop.de", "Shop", 1, true),
        msg("<b@x>", "news@shop.de", "Shop", 2, true),
    ];
    assert_eq!(store.add_scanned_messages(&msgs).unwrap(), 2);
    let domains: BTreeSet<String> = ["shop.de".to_string()].into();
    recompute(&store, &id, &domains).unwrap();
    store.set_company_decision("shop.de", "ignored").unwrap();
    store
        .set_company_contact(
            "shop.de",
            "website",
            "privacy@shop.de",
            "https://shop.de/datenschutz",
        )
        .unwrap();

    let mut again = msgs.clone();
    again.push(msg("<c@x>", "deals@shop.de", "Shop", 3, true));
    assert_eq!(
        store.add_scanned_messages(&again).unwrap(),
        1,
        "only the new one"
    );
    recompute(&store, &id, &domains).unwrap();
    let c = store.get_company("shop.de").unwrap().unwrap();
    assert_eq!(c.agg.message_count, 3);
    assert_eq!(c.decision, "ignored");
    assert_eq!(c.contact_email, "privacy@shop.de");
    let b = company_broker(&c);
    assert_eq!(
        (b.id.as_str(), b.region.as_str()),
        ("company:shop.de", "eu")
    );
}

// ==================== local fakes (never real websites / mail servers) ====================

#[derive(Default, Clone)]
pub(crate) struct MockSender(pub Arc<Mutex<Vec<Message>>>);

impl Sender for MockSender {
    fn send<'a>(&'a self, msg: &'a Message, _: Option<usize>) -> BoxFuture<'a, SendResult> {
        self.0.lock().unwrap().push(msg.clone());
        Box::pin(async {
            SendResult {
                success: true,
                message_id: "mock-1".into(),
                error: None,
            }
        })
    }
    fn name(&self) -> &'static str {
        "mock"
    }
}

/// Fails every send with `.0` and counts the attempts.
#[derive(Clone, Default)]
pub(crate) struct FailingSender(pub String, pub Arc<Mutex<usize>>);

impl FailingSender {
    pub(crate) fn new(error: &str) -> Self {
        Self(error.into(), Arc::default())
    }
    pub(crate) fn calls(&self) -> usize {
        *self.1.lock().unwrap()
    }
}

impl Sender for FailingSender {
    fn send<'a>(&'a self, _: &'a Message, _: Option<usize>) -> BoxFuture<'a, SendResult> {
        *self.1.lock().unwrap() += 1;
        Box::pin(async {
            SendResult {
                success: false,
                message_id: String::new(),
                error: Some(self.0.clone()),
            }
        })
    }
    fn name(&self) -> &'static str {
        "failing"
    }
}

/// Serves `router` on 127.0.0.1:random and returns the origin.
async fn serve(router: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("http://{addr}")
}

type Hits = Arc<Mutex<Vec<String>>>;

fn fake_page(host: &str, path: &str) -> axum::response::Response {
    let html = |s: &'static str| axum::response::Html(s).into_response();
    match (host, path) {
        // EN shop: privacy link on the homepage
        ("shop-en.test", "/") => {
            html(r#"<a href="/legal/privacy-notice">Privacy notice</a> hello@shop-en.test"#)
        }
        ("shop-en.test", "/legal/privacy-notice") => html(
            r#"<p>Contact our DPO: <a href="mailto:dpo@shop-en.test">dpo</a>; vendor x@onetrust.com</p>"#,
        ),
        // DE shop: home only on www., policy at a common path, robots blocks /privacy
        ("beispiel.test", "/robots.txt") => "User-agent: *\nDisallow: /privacy\n".into_response(),
        ("www.beispiel.test", "/") => html("<h1>Willkommen</h1>"),
        ("www.beispiel.test", "/privacy") => html("blocked@beispiel.test"),
        ("www.beispiel.test", "/datenschutz") => {
            html("<p>Kontakt: datenschutz [at] beispiel [punkt] test</p>")
        }
        // robots disallows us entirely
        ("closed.test", "/robots.txt") => "User-agent: eraser\nDisallow: /\n".into_response(),
        // FR shop with only a third-party address
        ("boutique.test", "/") => html(r#"<a href="/fr/confidentialite">Confidentialité</a>"#),
        ("boutique.test", "/fr/confidentialite") => html("dpo@cookie-vendor.test"),
        _ => axum::http::StatusCode::NOT_FOUND.into_response(),
    }
}

/// Fake company websites, virtual-hosted by the Host header on one local server.
async fn fake_sites() -> (String, Hits) {
    let hits: Hits = Arc::default();
    let log = hits.clone();
    let app = Router::new().fallback(
        move |headers: axum::http::HeaderMap, uri: axum::http::Uri| {
            let log = log.clone();
            async move {
                let h = |k: &str| {
                    headers
                        .get(k)
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("")
                        .to_string()
                };
                assert!(h("user-agent").starts_with("Eraser/"), "identifying UA");
                let host = h("host");
                log.lock().unwrap().push(format!("{host}{}", uri.path()));
                fake_page(&host, uri.path())
            }
        },
    );
    (serve(app).await, hits)
}

fn discoverer(origin: &str) -> discovery::Discoverer {
    let origin = origin.to_string();
    discovery::Discoverer::with_origin(Arc::new(move |_: &str| origin.clone())).unwrap()
}

/// Router whose `path` answers 503 on the first hit and `ok` afterwards; counts hits.
async fn flaky(path: &'static str, ok: &'static str) -> (String, Arc<Mutex<usize>>) {
    let hits = Arc::new(Mutex::new(0));
    let h = hits.clone();
    let reply = move || {
        let h = h.clone();
        async move {
            let mut n = h.lock().unwrap();
            *n += 1;
            if *n == 1 {
                axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response()
            } else {
                axum::response::Html(ok).into_response()
            }
        }
    };
    let app = Router::new().route(path, axum::routing::any(reply));
    (serve(app).await, hits)
}

#[tokio::test]
async fn discovery_retries_a_5xx_page() {
    let (origin, hits) = flaky("/privacy", "privacy@flaky.test").await;
    let c = discoverer(&origin).discover("flaky.test", &[]).await;
    assert_eq!(
        (c.status, c.email.as_str()),
        ("website", "privacy@flaky.test")
    );
    assert_eq!(*hits.lock().unwrap(), 2);
}

#[tokio::test]
async fn one_click_post_is_not_retried_after_5xx() {
    let (origin, hits) = flaky("/unsub", "ok").await;
    let code = actions::one_click_post(&actions::http_client(), &format!("{origin}/unsub"))
        .await
        .unwrap();
    assert_eq!((code, *hits.lock().unwrap()), (503, 1));
}

#[tokio::test]
async fn discovery_against_fake_websites() {
    let (origin, hits) = fake_sites().await;
    let d = discoverer(&origin);
    let count = |prefix: &str| {
        hits.lock()
            .unwrap()
            .iter()
            .filter(|h| h.starts_with(prefix) || h.starts_with(&format!("www.{prefix}")))
            .count()
    };

    let c = d.discover("shop-en.test", &[]).await;
    assert_eq!(
        (c.status, c.email.as_str()),
        ("website", "dpo@shop-en.test")
    );
    assert!(c.source.ends_with("/legal/privacy-notice"), "{}", c.source);

    let c = d.discover("beispiel.test", &[]).await;
    assert_eq!(
        (c.status, c.email.as_str()),
        ("website", "datenschutz@beispiel.test"),
        "decoded [at]/[punkt]"
    );
    assert!(c.source.ends_with("/datenschutz"));
    assert!(
        !hits
            .lock()
            .unwrap()
            .iter()
            .any(|h| h == "www.beispiel.test/privacy"),
        "robots.txt disallow respected"
    );

    let c = d.discover("closed.test", &[]).await;
    assert_eq!(c.status, "manual");
    assert_eq!(count("closed.test"), 1, "only robots.txt fetched");

    let c = d.discover("boutique.test", &[]).await;
    assert_eq!(
        (c.status, c.email.as_str()),
        ("manual", ""),
        "third-party domains rejected"
    );

    let c = d.discover("nosite.test", &[]).await;
    assert_eq!(c.status, "manual");
    assert_eq!(count("nosite.test"), 6, "fetch budget incl. robots.txt");

    let broker = crate::broker::Broker {
        id: "b1".into(),
        email: "privacy@shop-en.test".into(),
        ..Default::default()
    };
    let before = hits.lock().unwrap().len();
    let c = d.discover("shop-en.test", &[broker]).await;
    assert_eq!((c.status, c.source.as_str()), ("broker", "broker:b1"));
    assert_eq!(
        hits.lock().unwrap().len(),
        before,
        "broker match needs no fetch"
    );
}

#[tokio::test]
async fn one_click_post_against_fake_endpoint() {
    let got: Arc<Mutex<Vec<(String, String, bool)>>> = Arc::default();
    let g = got.clone();
    let redirect =
        |to: &'static str| post(move || async move { axum::response::Redirect::temporary(to) });
    let app = Router::new()
        .route(
            "/unsub",
            post(move |headers: axum::http::HeaderMap, body: String| {
                let g = g.clone();
                async move {
                    let ct = headers
                        .get("content-type")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("")
                        .to_string();
                    g.lock()
                        .unwrap()
                        .push((ct, body, headers.contains_key("cookie")));
                    ([("set-cookie", "track=1")], "ok")
                }
            }),
        )
        .route("/hop1", redirect("/hop2"))
        .route("/hop2", redirect("/hop3"))
        .route("/hop3", redirect("/hop4"))
        .route("/hop4", redirect("/unsub"))
        .route("/two", redirect("/hop4"))
        .route("/get-only", get(|| async { "x" }));
    let origin = serve(app).await;
    let client = actions::http_client();
    let post_to = |p: &str| {
        let (client, url) = (client.clone(), format!("{origin}{p}"));
        async move { actions::one_click_post(&client, &url).await }
    };

    assert_eq!(post_to("/unsub").await.unwrap(), 200);
    assert_eq!(post_to("/unsub").await.unwrap(), 200);
    let first = got.lock().unwrap()[0].clone();
    assert_eq!(
        first,
        (
            "application/x-www-form-urlencoded".to_string(),
            "List-Unsubscribe=One-Click".to_string(),
            false
        )
    );
    assert!(!got.lock().unwrap()[1].2, "no cookie jar");

    assert_eq!(post_to("/two").await.unwrap(), 200, "2 redirects followed");
    assert_eq!(
        post_to("/hop1").await.unwrap(),
        307,
        "4th redirect not followed"
    );
    assert_eq!(got.lock().unwrap().len(), 3);
    assert_eq!(post_to("/get-only").await.unwrap(), 405);
}

// ==================== GreenMail end-to-end ====================
// Run: ERASER_IMAP_IT=1 cargo test --lib scanner::tests::greenmail -- --nocapture

#[tokio::test]
async fn greenmail_scan_discover_request_unsubscribe() {
    if std::env::var("ERASER_IMAP_IT").as_deref() != Ok("1") {
        eprintln!("skipping greenmail test (set ERASER_IMAP_IT=1)");
        return;
    }
    use crate::inbox::monitor::tests::{
        connect_with_retry, crlf, it_monitor, seen_count, start_greenmail,
    };
    let (_container, port) = start_greenmail();
    let mut m = it_monitor(port, &[]);
    connect_with_retry(&mut m).await;

    let now = Utc::now().to_rfc2822();
    let mail = |headers: &str, id: &str| {
        crlf(&format!(
            "{headers}\nTo: test@localhost\nSubject: s\nDate: {now}\nMessage-ID: <{id}@test>\n\nBODY\n"
        ))
    };
    let seeds = [
        (
            "INBOX",
            mail(
                "From: Shop EN <news@mail.shop-en.test>\nList-Unsubscribe: <mailto:unsub@shop-en.test?subject=stop>\nList-Id: <news.shop-en.test>",
                "n1",
            ),
        ),
        (
            "INBOX",
            mail(
                "From: Shop EN <news@mail.shop-en.test>\nList-Unsubscribe: <mailto:unsub@shop-en.test?subject=stop>",
                "n2",
            ),
        ),
        (
            "Archive",
            mail(
                "From: =?UTF-8?Q?Beispiel_GmbH?= <info@beispiel.test>\nList-Unsubscribe: <https://beispiel.test/u?x=1>\nList-Unsubscribe-Post: List-Unsubscribe=One-Click",
                "b1",
            ),
        ),
        ("INBOX", mail("From: Friend <friend@gmail.com>", "p1")),
        ("INBOX", mail("From: Me <test@localhost>", "own1")),
        (
            "Promotions",
            mail("From: Deals <deals@boutique.test>\nPrecedence: bulk", "f1"),
        ),
    ];
    for (folder, raw) in &seeds {
        m.append_raw(folder, raw).await.unwrap();
    }

    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().join("h.db")).unwrap();
    let mut cfg = Config::default();
    cfg.profile.first_name = "Ada".into();
    cfg.profile.last_name = "Lovelace".into();
    cfg.profile.email = "test@localhost".into();
    cfg.email.from = "test@localhost".into();
    cfg.options.template = "auto".into();
    cfg.inbox.folder = "INBOX".into();
    cfg.inbox.archive_folder = "Eraser".into();
    let id = Identity::from_config(&cfg);

    let existing = m.list_folders().await.unwrap();
    let folders = pick_folders(&existing, &cfg.inbox, &[]);
    assert_eq!(folders, ["INBOX", "Archive", "Promotions"]);
    let r = scan_folders(&mut m, &store, &id, &folders, 30, |_, _, _| {})
        .await
        .unwrap();
    assert_eq!((r.total, r.new), (6, 5), "own message dropped");
    let r = scan_folders(&mut m, &store, &id, &folders, 30, |_, _, _| {})
        .await
        .unwrap();
    assert_eq!(r.new, 0, "rescan is incremental");
    assert_eq!(
        seen_count(&mut m, "INBOX").await,
        0,
        "header PEEK never sets \\Seen"
    );

    let companies = store.list_companies().unwrap();
    let get = |d: &str| {
        companies
            .iter()
            .find(|c| c.agg.domain == d)
            .unwrap()
            .clone()
    };
    let shop = get("shop-en.test");
    assert_eq!(shop.agg.message_count, 2);
    assert_eq!(shop.agg.display_name, "Shop EN");
    assert!(shop.agg.is_mailing_list && !shop.agg.one_click);
    let bsp = get("beispiel.test");
    assert_eq!(bsp.agg.display_name, "Beispiel GmbH");
    assert!(bsp.agg.one_click);
    assert_eq!(bsp.agg.unsub_https, "https://beispiel.test/u?x=1");
    assert_eq!(get("gmail.com").excluded_reason, "personal_domain");
    assert!(get("boutique.test").agg.is_mailing_list);
    assert!(!companies.iter().any(|c| c.agg.domain == "localhost"));

    // discovery against the local fake websites
    let (origin, _) = fake_sites().await;
    let d = discoverer(&origin);
    let domains = vec![
        "shop-en.test".to_string(),
        "beispiel.test".into(),
        "boutique.test".into(),
    ];
    d.discover_and_store(&store, &[], &domains).await;
    let shop = store.get_company("shop-en.test").unwrap().unwrap();
    assert_eq!(shop.contact_email, "dpo@shop-en.test");
    assert_eq!(
        store
            .get_company("boutique.test")
            .unwrap()
            .unwrap()
            .contact_status,
        "manual"
    );

    // deletion request + mailto unsubscribe through a mock sender
    let mock = MockSender::default();
    let engine = crate::template::Engine::new().unwrap();
    let r = actions::send_request(&store, &engine, &mock, &cfg, &shop, false).await;
    assert!(r.success);
    let r = actions::unsubscribe(&store, &actions::http_client(), &mock, &cfg, &shop, false).await;
    assert!(r.success, "{r:?}");
    let boutique = store.get_company("boutique.test").unwrap().unwrap();
    let r = actions::send_request(&store, &engine, &mock, &cfg, &boutique, false).await;
    assert!(!r.success, "manual companies are never contacted");

    let sent = mock.0.lock().unwrap().clone();
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0].to, "dpo@shop-en.test");
    assert!(
        sent[0].subject.contains("Personal Data"),
        "generic template for a non-EU TLD: {}",
        sent[0].subject
    );
    assert_eq!(
        (sent[1].to.as_str(), sent[1].subject.as_str()),
        ("unsub@shop-en.test", "stop")
    );

    let shop = store.get_company("shop-en.test").unwrap().unwrap();
    assert!(shop.requested_at.is_some() && shop.unsubscribed_at.is_some());
    let log: Vec<(String, String)> = store
        .company_actions("shop-en.test")
        .unwrap()
        .into_iter()
        .map(|a| (a.action, a.status))
        .collect();
    assert_eq!(
        log,
        [
            ("unsubscribe_mailto".to_string(), "ok".to_string()),
            ("request".into(), "ok".into())
        ]
    );
    let rec = store
        .get_last_request_for_broker("company:shop-en.test")
        .unwrap()
        .unwrap();
    assert_eq!(
        (rec.email.as_str(), rec.status),
        ("dpo@shop-en.test", crate::history::Status::Sent)
    );
    assert_eq!(
        store.company_actions("boutique.test").unwrap()[0].status,
        "failed"
    );

    // replies from the company are attributed to the pseudo-broker
    let brokers = inbox_brokers(&[], &store);
    assert_eq!(brokers.len(), 1);
    let mut m2 = it_monitor(port, &brokers);
    connect_with_retry(&mut m2).await;
    m2.append_raw("INBOX", &mail("From: DPO <dpo@shop-en.test>", "reply1"))
        .await
        .unwrap();
    let replies = m2
        .fetch_broker_emails_from_folder("INBOX", 30)
        .await
        .unwrap();
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0].broker_id, "company:shop-en.test");
    m2.disconnect().await.unwrap();
    m.disconnect().await.unwrap();
}
