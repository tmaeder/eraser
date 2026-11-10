use super::*;
use crate::history::{BrokerResponse, ScannedMessage, Status};
use crate::scanner::actions;
use crate::scanner::tests::{FailingSender, MockSender};
use std::sync::{Arc, Mutex};

fn cfg() -> Config {
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

fn broker(id: &str, website: &str) -> Broker {
    Broker {
        id: id.into(),
        name: format!("Broker {id}"),
        email: format!("privacy@{id}.test"),
        website: website.into(),
        region: "eu".into(),
        ..Default::default()
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    cfg: Config,
    store: Store,
    brokers: BrokerDatabase,
    engine: Engine,
    mail: MockSender,
    http: reqwest::Client,
    discoverer: Discoverer,
}

fn fixture(cfg: Config, brokers: Vec<Broker>) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    Fixture {
        store: Store::new(dir.path().join("h.db")).unwrap(),
        _dir: dir,
        cfg,
        brokers: BrokerDatabase { brokers },
        engine: Engine::new().unwrap(),
        mail: MockSender::default(),
        http: actions::http_client(),
        // closed local port: contact discovery never reaches a real website
        discoverer: Discoverer::with_origin(Arc::new(|_: &str| "http://127.0.0.1:9".to_string()))
            .unwrap(),
    }
}

impl Fixture {
    fn deps(&self) -> Deps<'_> {
        Deps {
            cfg: &self.cfg,
            store: &self.store,
            brokers: &self.brokers,
            engine: &self.engine,
            sender: &self.mail,
            http: &self.http,
            discoverer: &self.discoverer,
        }
    }

    async fn round(&self, dry_run: bool, monitor: Option<Monitor>) -> RoundReport {
        let id = self
            .store
            .begin_round(dry_run)
            .unwrap()
            .expect("no round running");
        run_round(&self.deps(), monitor, dry_run, id).await
    }

    fn sent(&self) -> Vec<Message> {
        self.mail.0.lock().unwrap().clone()
    }
}

fn sent_record(store: &Store, id: &str, days_ago: i64) -> Record {
    let mut r = Record {
        broker_id: id.into(),
        broker_name: format!("Broker {id}"),
        email: format!("privacy@{id}.test"),
        template: "gdpr".into(),
        status: Status::Sent,
        sent_at: Some(Local::now() - TimeDelta::days(days_ago)),
        ..Default::default()
    };
    store.add(&mut r).unwrap();
    r
}

/// A scanned company with one message `age_days` old; `contact` "" = no contact.
fn company(store: &Store, domain: &str, name: &str, list: bool, age_days: i64, contact: &str) {
    let msg = ScannedMessage {
        key: format!("<{domain}@x>"),
        domain: domain.into(),
        from_addr: format!("news@{domain}"),
        from_name: name.into(),
        date: Some(Utc::now() - TimeDelta::days(age_days)),
        is_list: list,
        ..Default::default()
    };
    store.add_scanned_messages(&[msg]).unwrap();
    let domains = std::collections::BTreeSet::from([domain.to_string()]);
    scanner::recompute(store, &scanner::Identity::default(), &domains).unwrap();
    if !contact.is_empty() {
        store
            .set_company_contact(
                domain,
                "website",
                contact,
                &format!("https://{domain}/privacy"),
            )
            .unwrap();
    }
}

#[test]
fn id_requests_and_next_round() {
    assert!(asks_for_id("Please send us a copy of your passport."));
    assert!(asks_for_id(
        "Bitte senden Sie uns eine Kopie Ihres Ausweises."
    ));
    assert!(!asks_for_id("Your data has been deleted."));
    let at = |h, m| Utc.with_ymd_and_hms(2026, 11, 10, h, m, 0).unwrap();
    assert_eq!(next_round(at(12, 0)), at(18, 0));
    assert_eq!(next_round(at(17, 59)), at(18, 0));
    assert_eq!(
        next_round(at(23, 30)),
        Utc.with_ymd_and_hms(2026, 11, 11, 0, 0, 0).unwrap()
    );
}

#[tokio::test]
async fn dry_run_sends_nothing() {
    let f = fixture(
        cfg(),
        vec![broker("a", ""), broker("b", ""), broker("old", "")],
    );
    sent_record(&f.store, "old", 40);
    let r = f.round(true, None).await;
    assert!(r.dry_run);
    assert_eq!(
        (r.requests_sent, r.reminders_sent),
        (2, 1),
        "counts what would be sent"
    );
    assert!(f.sent().is_empty(), "dry run sends nothing");
    assert_eq!(
        f.store.get_recent_requests(10).unwrap().len(),
        1,
        "nothing recorded"
    );
    assert_eq!(
        f.store.reminder_candidates().unwrap().len(),
        1,
        "reminder still due"
    );
    let stored: RoundReport =
        serde_json::from_str(&f.store.recent_rounds(1).unwrap()[0].report).unwrap();
    assert_eq!(stored, r);
    assert!(
        stored
            .summary()
            .starts_with("Dry run: 2 broker requests, 1 reminders"),
        "{}",
        stored.summary()
    );
}

#[tokio::test]
async fn round_sends_requests_within_daily_limit() {
    let brokers = (0..3).map(|i| broker(&format!("b{i}"), "")).collect();
    let f = fixture(cfg(), brokers);
    // 248 emails already went out today: 2 left of the SMTP limit of 250
    for i in 0..248 {
        sent_record(&f.store, &format!("old{i}"), 0);
    }
    let r = f.round(false, None).await;
    assert_eq!(r.requests_sent, 2);
    assert!(r.daily_limit_reached);
    let sent = f.sent();
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0].to, "privacy@b0.test");
    assert!(
        sent[0].subject.contains("GDPR"),
        "auto picks GDPR: {}",
        sent[0].subject
    );
    assert_eq!(
        f.store
            .get_last_request_for_broker("b0")
            .unwrap()
            .unwrap()
            .status,
        Status::Sent
    );
    let r = f.round(false, None).await;
    assert_eq!(r.requests_sent, 0, "the limit holds across rounds");
}

#[tokio::test]
async fn reminder_sent_once_and_only_after_n_days() {
    let f = fixture(cfg(), vec![broker("old", ""), broker("fresh", "")]);
    sent_record(&f.store, "old", 31);
    sent_record(&f.store, "fresh", 29);
    let r = f.round(false, None).await;
    assert_eq!((r.reminders_sent, r.requests_sent), (1, 0));
    let sent = f.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].to, "privacy@old.test");
    assert!(
        sent[0]
            .subject
            .starts_with("Reminder: data deletion request of ")
    );
    assert!(
        sent[0].body.contains("Article 12(3) GDPR"),
        "{}",
        sent[0].body
    );
    assert!(sent[0].body.contains("31 days have passed"));
    let r = f.round(false, None).await;
    assert_eq!(r.reminders_sent, 0, "at most one reminder");
    assert_eq!(f.sent().len(), 1);
    assert_eq!(f.store.progress(false).unwrap().no_response, 1);
}

#[tokio::test]
async fn lock_prevents_parallel_rounds() {
    let f = fixture(cfg(), vec![]);
    let id = f.store.begin_round(false).unwrap().unwrap();
    assert_eq!(
        f.store.begin_round(true).unwrap(),
        None,
        "second round refused"
    );
    run_round(&f.deps(), None, false, id).await;
    assert!(
        f.store.begin_round(true).unwrap().is_some(),
        "free again afterwards"
    );
}

#[tokio::test]
async fn web_form_replies_become_todos_once() {
    let mut c = cfg();
    c.autopilot.broker_requests = false;
    let f = fixture(c, vec![]);
    f.store
        .add_broker_response(&mut BrokerResponse {
            broker_id: "spk".into(),
            broker_name: "Spokeo".into(),
            response_type: "form_required".into(),
            form_url: "https://spk.test/optout".into(),
            email_subject: "Use our form".into(),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(f.round(false, None).await.todos_added, 1);
    assert_eq!(f.round(false, None).await.todos_added, 0);
    let t = &f.store.todo_tasks(Local::now()).unwrap()[0];
    assert_eq!(
        (t.task_type, t.form_url.as_str()),
        (TaskType::ManualForm, "https://spk.test/optout")
    );
    assert_eq!(
        f.store.get_forms_with_status().unwrap()[0].status,
        "pending"
    );
}

/// Serves `router` on 127.0.0.1 and returns `http://localhost:<port>`.
async fn serve(router: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("http://localhost:{port}")
}

type Hits = Arc<Mutex<Vec<String>>>;

/// Fake broker site: `/captcha` shows a reCAPTCHA, every other path confirms.
async fn fake_broker_site() -> (String, Hits) {
    let hits: Hits = Arc::default();
    let log = hits.clone();
    let app = axum::Router::new().fallback(move |uri: axum::http::Uri| {
        let log = log.clone();
        async move {
            log.lock().unwrap().push(uri.path().to_string());
            match uri.path() {
                "/captcha" => axum::response::Html(r#"<div class="g-recaptcha"></div>"#),
                _ => axum::response::Html("Your removal request has been confirmed. Thank you!"),
            }
        }
    });
    (serve(app).await, hits)
}

#[tokio::test]
async fn confirm_links_only_on_broker_domain_and_once() {
    let (origin, hits) = fake_broker_site().await;
    let mut c = cfg();
    c.autopilot.broker_requests = false;
    let f = fixture(c, vec![broker("acme", &origin)]);
    for (subject, url) in [
        ("Confirm 1", format!("{origin}/ok")),
        ("Confirm 2", format!("{origin}/captcha")),
        ("Confirm 3", "https://evil.test/confirm".to_string()),
    ] {
        f.store
            .add_broker_response(&mut BrokerResponse {
                broker_id: "acme".into(),
                broker_name: "Acme".into(),
                response_type: "confirmation_required".into(),
                email_subject: subject.into(),
                confirm_url: url,
                ..Default::default()
            })
            .unwrap();
    }

    let dry = f.round(true, None).await;
    assert_eq!(
        dry.links_confirmed, 2,
        "would open both broker-domain links"
    );
    assert_eq!(dry.todos_added, 1, "foreign domain becomes a to-do");
    assert!(hits.lock().unwrap().is_empty(), "dry run opens nothing");

    let r = f.round(false, None).await;
    assert_eq!(
        (r.links_confirmed, r.todos_added),
        (1, 1),
        "captcha becomes a to-do"
    );
    let mut opened = hits.lock().unwrap().clone();
    opened.sort();
    assert_eq!(opened, ["/captcha", "/ok"]);
    let todos = f.store.todo_tasks(Local::now()).unwrap();
    assert_eq!(todos.len(), 2);
    assert!(
        todos
            .iter()
            .any(|t| t.form_url == "https://evil.test/confirm")
    );
    assert!(todos.iter().any(|t| t.form_url.ends_with("/captcha")));
    let done = f
        .store
        .find_task_by_url(TaskType::Confirm, &format!("{origin}/ok"))
        .unwrap()
        .unwrap();
    assert_eq!(done.status, "completed", "the click is logged");

    f.round(false, None).await;
    assert_eq!(hits.lock().unwrap().len(), 2, "each link at most once");
}

#[tokio::test]
async fn keep_list_blocks_requests() {
    let mut c = cfg();
    c.autopilot.keep_domains = vec!["club.test".into()];
    c.autopilot.broker_requests = false;
    let f = fixture(c, vec![]);
    company(&f.store, "ubs.com", "UBS", false, 400, "privacy@ubs.com");
    company(
        &f.store,
        "club.test",
        "Club",
        false,
        400,
        "privacy@club.test",
    );
    company(
        &f.store,
        "meinebank.test",
        "Meine Bank",
        false,
        400,
        "dpo@meinebank.test",
    );
    company(
        &f.store,
        "shop.test",
        "Shop",
        false,
        400,
        "privacy@shop.test",
    );

    let dry = f.round(true, None).await;
    assert_eq!((dry.kept, dry.company_requests), (3, 1));
    assert!(f.sent().is_empty(), "dry run sends nothing");
    assert!(
        f.store
            .get_company("shop.test")
            .unwrap()
            .unwrap()
            .requested_at
            .is_none()
    );

    let r = f.round(false, None).await;
    assert_eq!((r.kept, r.company_requests), (3, 1));
    let to: Vec<String> = f.sent().into_iter().map(|m| m.to).collect();
    assert_eq!(
        to,
        ["privacy@shop.test"],
        "only the company not on the keep list"
    );
    assert!(
        f.store
            .get_company("shop.test")
            .unwrap()
            .unwrap()
            .requested_at
            .is_some()
    );
    assert_eq!(
        f.round(false, None).await.company_requests,
        0,
        "requested once"
    );
}

#[tokio::test]
async fn only_inactive_or_newsletter_only_companies_with_contact() {
    let mut c = cfg();
    c.autopilot.broker_requests = false;
    let f = fixture(c, vec![]);
    company(
        &f.store,
        "active.test",
        "Active",
        false,
        10,
        "privacy@active.test",
    );
    company(&f.store, "news.test", "News", true, 10, "privacy@news.test");
    company(&f.store, "nocontact.test", "NoContact", false, 400, "");
    let r = f.round(false, None).await;
    assert_eq!(r.company_requests, 1);
    let to: Vec<String> = f.sent().into_iter().map(|m| m.to).collect();
    assert_eq!(to, ["privacy@news.test"]);
}

#[tokio::test]
async fn plain_unsubscribe_links_become_todos_once() {
    let mut c = cfg();
    c.autopilot.broker_requests = false;
    c.autopilot.company_requests = false;
    let f = fixture(c, vec![]);
    let msg = ScannedMessage {
        key: "<n@x>".into(),
        domain: "news.test".into(),
        from_addr: "n@news.test".into(),
        from_name: "News".into(),
        date: Some(Utc::now()),
        is_list: true,
        unsub_https: "https://news.test/unsub?u=1".into(),
        ..Default::default()
    };
    f.store.add_scanned_messages(&[msg]).unwrap();
    let domains = std::collections::BTreeSet::from(["news.test".to_string()]);
    scanner::recompute(&f.store, &scanner::Identity::default(), &domains).unwrap();

    assert_eq!(f.round(false, None).await.todos_added, 1);
    assert_eq!(
        f.round(false, None).await.todos_added,
        0,
        "one to-do per link"
    );
    let t = &f.store.todo_tasks(Local::now()).unwrap()[0];
    assert_eq!(
        (t.task_type, t.broker_id.as_str(), t.form_url.as_str()),
        (
            TaskType::UnsubscribeLink,
            "company:news.test",
            "https://news.test/unsub?u=1"
        )
    );
    assert_eq!(f.sent(), []);
    assert!(
        f.store
            .get_company("news.test")
            .unwrap()
            .unwrap()
            .unsubscribed_at
            .is_none()
    );
}

// Run: ERASER_IMAP_IT=1 cargo test --lib autopilot::tests::greenmail -- --nocapture
// Requires docker (greenmail/standalone:2.1.3).
#[tokio::test]
async fn greenmail_round_end_to_end() {
    if std::env::var("ERASER_IMAP_IT").as_deref() != Ok("1") {
        eprintln!("skipping greenmail test (set ERASER_IMAP_IT=1)");
        return;
    }
    use crate::inbox::monitor::tests::{connect_with_retry, crlf, it_monitor, start_greenmail};
    let (_container, port) = start_greenmail();
    let (origin, hits) = fake_broker_site().await;

    let mut c = cfg();
    c.profile.email = "test@localhost".into();
    c.email.from = "test@localhost".into();
    c.inbox = crate::config::InboxConfig {
        enabled: true,
        provider: "imap".into(),
        server: "localhost".into(),
        port: port.into(),
        email: "test@localhost".into(),
        password: "secret".into(),
        folder: "INBOX".into(),
        auto_archive: true,
        archive_folder: "Eraser".into(),
        ..Default::default()
    };
    let brokers = vec![broker("acme", &origin), broker("other", "")];
    let f = fixture(c, brokers.clone());
    sent_record(&f.store, "acme", 2);

    let mut seed = it_monitor(port, &[]);
    connect_with_retry(&mut seed).await;
    let now = Utc::now().to_rfc2822();
    let mail = |headers: &str, id: &str, body: &str| {
        crlf(&format!(
            "{headers}\nTo: test@localhost\nDate: {now}\nMessage-ID: <{id}@test>\nContent-Type: text/plain; charset=utf-8\n\n{body}\n"
        ))
    };
    for raw in [
        mail(
            "From: Acme <privacy@acme.test>\nSubject: Confirm your email",
            "c1",
            &format!(
                "Please confirm your email address. Click here to confirm: {origin}/confirm?t=1"
            ),
        ),
        mail(
            "From: Acme <privacy@acme.test>\nSubject: Identity check",
            "i1",
            "Before we can proceed, please send us a copy of your passport.",
        ),
        mail(
            "From: Shop <news@shop.test>\nSubject: Deals\nList-Unsubscribe: <https://shop.test/unsub?u=1>",
            "n1",
            "deals",
        ),
    ] {
        seed.append_raw("INBOX", &raw).await.unwrap();
    }
    seed.disconnect().await.unwrap();

    let monitor = || it_monitor(port, &scanner::inbox_brokers(&brokers, &f.store));
    let r = f.round(false, Some(monitor())).await;
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    assert_eq!(r.replies, 2);
    assert_eq!(r.links_confirmed, 1);
    assert_eq!(*hits.lock().unwrap(), ["/confirm"]);
    assert_eq!(r.requests_sent, 1, "only the broker never contacted");
    assert_eq!(f.sent()[0].to, "privacy@other.test");
    assert_eq!(
        r.new_messages, 3,
        "headers scanned from INBOX and the archive folder"
    );
    let kinds: Vec<TaskType> = f
        .store
        .todo_tasks(Local::now())
        .unwrap()
        .iter()
        .map(|t| t.task_type)
        .collect();
    assert!(kinds.contains(&TaskType::IdRequired), "{kinds:?}");
    assert!(kinds.contains(&TaskType::UnsubscribeLink), "{kinds:?}");
    assert_eq!(
        f.store
            .get_company("acme.test")
            .unwrap()
            .unwrap()
            .contact_status,
        "broker",
        "contact discovery for new companies"
    );

    let r = f.round(false, Some(monitor())).await;
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    assert_eq!(
        (
            r.replies,
            r.links_confirmed,
            r.requests_sent,
            r.new_messages
        ),
        (0, 0, 0, 0),
        "second round: nothing new"
    );
    assert_eq!(hits.lock().unwrap().len(), 1);
}

/// Sends like MockSender but panics (a crash mid-send) for one recipient.
struct CrashOn(&'static str, MockSender);

impl Sender for CrashOn {
    fn send<'a>(
        &'a self,
        msg: &'a Message,
        seq: Option<usize>,
    ) -> futures::future::BoxFuture<'a, SendResult> {
        assert_ne!(msg.to, self.0, "simulated crash mid-send");
        self.1.send(msg, seq)
    }
    fn name(&self) -> &'static str {
        "crash"
    }
}

#[tokio::test]
async fn crash_mid_send_is_never_resent_and_round_goes_on() {
    let f = fixture(cfg(), vec![broker("a", ""), broker("b", "")]);
    let crash = CrashOn("privacy@a.test", f.mail.clone());
    let deps = Deps {
        sender: &crash,
        ..f.deps()
    };
    let id = f.store.begin_round(false).unwrap().unwrap();
    let r = run_round(&deps, None, false, id).await;
    assert_eq!(
        r.errors,
        vec!["broker requests: internal error, step skipped".to_string()]
    );
    assert!(f.sent().is_empty(), "nothing went out after the crash");
    let a = f.store.get_last_request_for_broker("a").unwrap().unwrap();
    assert_eq!(a.status, Status::Pending, "recorded before sending");

    // Restart: a is never resent, b (never attempted) is sent.
    let r = f.round(false, None).await;
    assert_eq!(r.requests_sent, 1);
    let to: Vec<String> = f.sent().into_iter().map(|m| m.to).collect();
    assert_eq!(to, vec!["privacy@b.test".to_string()]);
    assert_eq!(r.todos_added, 0, "a fresh pending row is not flagged yet");
}

#[tokio::test]
async fn interrupted_sends_are_flagged_once_and_not_resent() {
    let f = fixture(cfg(), vec![broker("a", "")]);
    // A pending row from a run that crashed an hour ago (broker request) …
    let mut rec = Record {
        broker_id: "a".into(),
        broker_name: "Broker a".into(),
        email: "privacy@a.test".into(),
        template: "gdpr".into(),
        status: Status::Pending,
        sent_at: Some(Local::now() - TimeDelta::hours(1)),
        ..Default::default()
    };
    f.store.add(&mut rec).unwrap();
    // … and one for an inbox company with a verified contact.
    company(&f.store, "shop.de", "Shop", false, 400, "privacy@shop.de");
    let shop = f.store.get_company("shop.de").unwrap().unwrap();
    let mut rec = Record {
        broker_id: scanner::company_broker(&shop).id,
        broker_name: "Shop".into(),
        email: "privacy@shop.de".into(),
        template: "gdpr".into(),
        status: Status::Pending,
        sent_at: Some(Local::now() - TimeDelta::hours(1)),
        ..Default::default()
    };
    f.store.add(&mut rec).unwrap();

    let r = f.round(false, None).await;
    assert!(f.sent().is_empty(), "{:?}", f.sent());
    assert_eq!((r.requests_sent, r.company_requests), (0, 0));
    assert_eq!(r.todos_added, 2);
    let reviews = f
        .store
        .get_pending_tasks(Some(TaskType::Review), Some("pending"))
        .unwrap();
    assert_eq!(reviews.len(), 2);
    assert!(reviews[0].notes.contains("check your Sent folder"));
    let a = f.store.get_last_request_for_broker("a").unwrap().unwrap();
    assert_eq!(
        (a.status, a.error.as_str()),
        (Status::Pending, crate::history::INTERRUPTED)
    );

    let r = f.round(false, None).await;
    assert_eq!(r.todos_added, 0, "flagged only once");
    assert_eq!(f.sent(), []);
}

#[tokio::test]
async fn failed_reminder_is_released_for_the_next_round() {
    struct Fails;
    impl Sender for Fails {
        fn send<'a>(
            &'a self,
            _: &'a Message,
            _: Option<usize>,
        ) -> futures::future::BoxFuture<'a, SendResult> {
            Box::pin(async {
                SendResult {
                    success: false,
                    message_id: String::new(),
                    error: Some("SMTP error: check your configuration".into()),
                }
            })
        }
        fn name(&self) -> &'static str {
            "fails"
        }
    }
    let mut c = cfg();
    c.autopilot.company_requests = false;
    let f = fixture(c, vec![broker("old", "")]);
    sent_record(&f.store, "old", 31);
    let deps = Deps {
        sender: &Fails,
        ..f.deps()
    };
    let id = f.store.begin_round(false).unwrap().unwrap();
    let r = run_round(&deps, None, false, id).await;
    assert_eq!(r.reminders_sent, 0);
    assert_eq!(f.store.reminder_candidates().unwrap().len(), 1, "released");
    let r = f.round(false, None).await;
    assert_eq!(r.reminders_sent, 1);
}

#[test]
fn long_errors_are_shortened() {
    let mut r = RoundReport {
        errors: vec!["é".repeat(500), "short".into()],
        ..Default::default()
    };
    shorten_errors(&mut r);
    assert_eq!(r.errors[0].chars().count(), MAX_ERROR_LEN + 1);
    assert_eq!(r.errors[1], "short");
}

#[tokio::test]
async fn breaker_trips_on_the_first_rejection_and_stops_the_round() {
    let brokers = (0..3).map(|i| broker(&format!("b{i}"), "")).collect();
    let f = fixture(cfg(), brokers);
    let failing = FailingSender::new("SMTP authentication failed");
    let mut deps = f.deps();
    deps.sender = &failing;
    let id = f.store.begin_round(false).unwrap().unwrap();
    let r = run_round(&deps, None, false, id).await;
    assert_eq!(failing.calls(), 1, "no further attempts");
    assert!(
        r.auth_failed && r.summary().contains("sending paused"),
        "{}",
        r.summary()
    );
    assert!(f.store.account_blocked().unwrap().is_some());
    // the next round sends nothing while blocked
    let id = f.store.begin_round(false).unwrap().unwrap();
    let r = run_round(&deps, None, false, id).await;
    assert_eq!(failing.calls(), 1);
    assert!(r.auth_failed);
}

#[tokio::test]
async fn round_stops_at_the_hourly_cap() {
    let brokers = (0..3).map(|i| broker(&format!("b{i}"), "")).collect();
    let mut c = cfg();
    c.account.provider = "icloud".into();
    let f = fixture(c, brokers);
    for i in 0..25 {
        let mut r = Record {
            broker_id: format!("old{i}"),
            broker_name: "Old".into(),
            email: "p@old.test".into(),
            template: "gdpr".into(),
            status: Status::Failed,
            sent_at: Some(Local::now() - TimeDelta::minutes(30)),
            ..Default::default()
        };
        f.store.add(&mut r).unwrap();
    }
    let r = f.round(false, None).await;
    assert_eq!(r.requests_sent, 0);
    assert!(r.hourly_limit_reached && !r.daily_limit_reached, "{r:?}");
    assert!(f.sent().is_empty());
}
