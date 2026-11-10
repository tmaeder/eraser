//! Handler-level tests that need access to `AppState` internals.

use super::*;
use crate::broker::Broker;
use crate::scanner::tests::{FailingSender, MockSender};
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, Method, Uri, header};

fn state(cfg: Config, dir: &std::path::Path, mail: MockSender) -> Arc<AppState> {
    state_with(cfg, dir, Box::new(mail))
}

fn state_with(cfg: Config, dir: &std::path::Path, mail: Box<dyn SenderClone>) -> Arc<AppState> {
    state_info(cfg, dir, mail, Default::default())
}

fn state_info(
    cfg: Config,
    dir: &std::path::Path,
    mail: Box<dyn SenderClone>,
    broker_info: crate::broker::BrokerInfoMap,
) -> Arc<AppState> {
    let brokers = (1..=4)
        .map(|i| Broker {
            id: format!("b{i}"),
            name: format!("B{i}"),
            email: format!("privacy@b{i}.example"),
            region: "us".into(),
            ..Default::default()
        })
        .collect();
    let (_, st) = app(AppOptions {
        config: Some(cfg),
        config_error: None,
        config_path: dir.join("config.yaml"),
        brokers: BrokerDatabase { brokers },
        broker_info,
        history: Store::new(dir.join("h.db")).unwrap(),
        data_dir: dir.join("data"),
        port: 8080,
        public_origin: None,
        sender_factory: Arc::new(move |_| Ok(mail.boxed())),
        autopilot_token: None,
    })
    .unwrap();
    st
}

trait SenderClone: Send + Sync {
    fn boxed(&self) -> Box<dyn Sender>;
}

impl<T: Sender + Clone + 'static> SenderClone for T {
    fn boxed(&self) -> Box<dyn Sender> {
        Box::new(self.clone())
    }
}

async fn wait_done(job: &job::Job) {
    for _ in 0..500 {
        if job.status() != JobStatus::Running {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("job still running");
}

fn pending(ids: &[&str]) -> PersistentJobState {
    PersistentJobState {
        id: "j".into(),
        status: JobStatus::Running,
        total: ids.len() as i64,
        remaining_brokers: ids.iter().map(|s| s.to_string()).collect(),
        ..Default::default()
    }
}

fn configured() -> Config {
    let mut c = Config::default();
    c.profile.first_name = "Ada".into();
    c.profile.last_name = "Lovelace".into();
    c.profile.email = "ada@example.com".into();
    c.email.provider = "smtp".into();
    c.email.from = "ada@example.com".into();
    c.options.template = "generic".into();
    c.options.rate_limit_ms = 1;
    c
}

#[tokio::test]
async fn inbox_options_use_the_account_and_keep_its_secret() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = configured();
    cfg.account = crate::config::AccountConfig {
        provider: "icloud".into(),
        email: "ada@icloud.com".into(),
        password: "app-pw".into(),
        ..Default::default()
    };
    cfg.apply_account();
    let st = state(cfg, dir.path(), MockSender::default());
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        "application/x-www-form-urlencoded".parse().unwrap(),
    );
    super::account::settings_inbox(
        State(st.clone()),
        axum::Extension(csrf::CsrfToken("t".into())),
        Method::POST,
        headers,
        Uri::from_static("/settings/inbox"),
        Bytes::from_static(b"enabled=on&folder=&archive_folder=Done&scan_folders=INBOX%2C+News"),
    )
    .await;
    let cfg = st.config().unwrap();
    let inbox = &cfg.inbox;
    assert_eq!(
        (inbox.server.as_str(), inbox.port),
        ("imap.mail.me.com", 993)
    );
    assert_eq!(
        (inbox.email.as_str(), inbox.password.as_str()),
        ("ada@icloud.com", "app-pw")
    );
    assert_eq!(
        (inbox.folder.as_str(), inbox.archive_folder.as_str()),
        ("INBOX", "Done")
    );
    assert_eq!(inbox.scan_folders, ["INBOX", "News"]);
    assert!(cfg.validate_inbox().is_ok(), "scan works without restart");
    let saved = crate::config::load(dir.path().join("config.yaml")).unwrap();
    assert_eq!(saved.account.password, "app-pw", "secret kept");
    assert!(saved.inbox.enabled);
}

#[tokio::test]
async fn resumed_job_keeps_its_counters() {
    let dir = tempfile::tempdir().unwrap();
    let mail = MockSender::default();
    let st = state(configured(), dir.path(), mail.clone());
    handlers::resume_pending_job(
        st.clone(),
        PersistentJobState {
            id: "old".into(),
            status: JobStatus::Running,
            sent: 5,
            failed: 2,
            total: 9,
            remaining_brokers: vec!["b1".into(), "b2".into()],
            ..Default::default()
        },
    );
    let job = st.jobs.get_active().expect("resumed job");
    assert_eq!(job.to_json()["sent"], 5, "restored before the first send");
    for _ in 0..200 {
        if job.status() == JobStatus::Completed {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let j = job.to_json();
    assert_eq!(
        (j["sent"].as_i64(), j["failed"].as_i64()),
        (Some(7), Some(2))
    );
    assert_eq!(j["progress"], 100);
    assert_eq!(mail.0.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn resumed_bulk_job_keeps_its_country_filter() {
    let dir = tempfile::tempdir().unwrap();
    let mail = MockSender::default();
    let info = [("b1", "CH"), ("b2", "US"), ("b3", "CH"), ("b4", "DE")]
        .into_iter()
        .map(|(id, c)| {
            let i = crate::broker::BrokerInfo {
                country: c.into(),
                ..Default::default()
            };
            (id.to_string(), i)
        })
        .collect();
    let st = state_info(configured(), dir.path(), Box::new(mail.clone()), info);
    // "restart" before the first send: the job stops with its state persisted
    st.shutting_down.store(true, Ordering::SeqCst);
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        "application/x-www-form-urlencoded".parse().unwrap(),
    );
    let r = handlers::api_send_all(
        State(st.clone()),
        Method::POST,
        headers,
        Uri::from_static("/api/send-all"),
        Bytes::from_static(b"status=pending&country=ch"),
    )
    .await;
    assert_eq!(r.status(), axum::http::StatusCode::OK);
    let old = st.jobs.get_active().expect("job");
    for _ in 0..100 {
        if st.job_tasks.load(Ordering::SeqCst) == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    old.cancel();
    st.shutting_down.store(false, Ordering::SeqCst);
    assert!(mail.0.lock().unwrap().is_empty());

    let mut saved = st.persistence.load().unwrap().expect("pending job");
    assert_eq!(saved.country, "ch");
    assert_eq!(saved.remaining_brokers, ["b1", "b3"]);
    saved.remaining_brokers = vec!["b1".into(), "b2".into(), "b3".into(), "b4".into()];
    handlers::resume_pending_job(st.clone(), saved);
    wait_done(&st.jobs.get_active().expect("resumed job")).await;
    let mut to: Vec<String> = mail
        .0
        .lock()
        .unwrap()
        .iter()
        .map(|m| m.to.clone())
        .collect();
    to.sort();
    assert_eq!(to, ["privacy@b1.example", "privacy@b3.example"]);
}

#[tokio::test]
async fn autopilot_tick_rejected_without_server_token() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = configured();
    cfg.autopilot.enabled = true;
    let st = state(cfg, dir.path(), MockSender::default());
    let mut headers = HeaderMap::new();
    headers.insert(today::TOKEN_HEADER, "".parse().unwrap());
    let r = today::tick(State(st.clone()), headers).await;
    assert_eq!(r.status(), axum::http::StatusCode::UNAUTHORIZED);
    assert_eq!(st.history.recent_rounds(1).unwrap(), []);
}

#[tokio::test]
async fn bulk_job_keeps_the_minimum_interval() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = configured();
    cfg.options.rate_limit_ms = 80; // custom provider: the configured interval
    let mail = MockSender::default();
    let st = state(cfg, dir.path(), mail.clone());
    let started = std::time::Instant::now();
    handlers::resume_pending_job(st.clone(), pending(&["b1", "b2", "b3"]));
    let job = st.jobs.get_active().expect("job");
    wait_done(&job).await;
    assert_eq!(mail.0.lock().unwrap().len(), 3);
    assert!(
        started.elapsed() >= std::time::Duration::from_millis(160),
        "two gaps of 80 ms: {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn breaker_stops_the_job_on_the_first_rejection_until_a_test_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = configured();
    cfg.account.provider = "icloud".into();
    cfg.email.from = "ada@icloud.com".into();
    cfg.autopilot.enabled = true;
    let mail = FailingSender::new("Rejected by mail server: 550 5.1.1 Mailbox does not exist");
    let st = state_with(cfg, dir.path(), Box::new(mail.clone()));
    handlers::resume_pending_job(st.clone(), pending(&["b1", "b2", "b3", "b4"]));
    let job = st.jobs.get_active().expect("job");
    wait_done(&job).await;
    assert_eq!(mail.calls(), 1, "no attempt after the first rejection");
    let j = job.to_json();
    assert_eq!(j["error_type"], "account");
    assert!(j["error"].as_str().unwrap().contains("550 5.1.1"), "{j}");
    let (_, reason) = st.history.account_blocked().unwrap().expect("blocked");
    assert!(reason.contains("Mailbox does not exist"));
    assert!(!st.config().unwrap().autopilot.enabled, "autopilot paused");
    let saved = crate::config::load(dir.path().join("config.yaml")).unwrap();
    assert!(!saved.autopilot.enabled, "pause saved");

    // every send path refuses while blocked
    handlers::resume_pending_job(st.clone(), pending(&["b2"]));
    wait_done(&st.jobs.get_active().expect("job")).await;
    assert_eq!(mail.calls(), 1);

    // banner on Settings
    let page = handlers::settings_render(&st, &csrf::CsrfToken("t".into()), serde_json::json!({}));
    let body = axum::body::to_bytes(page.into_body(), usize::MAX)
        .await
        .unwrap();
    let html = String::from_utf8_lossy(&body);
    assert!(
        html.contains("Your mail provider rejected Eraser&#x27;s login")
            || html.contains("Your mail provider rejected Eraser's login"),
        "banner"
    );
    assert!(html.contains("Sending is paused to protect your account"));

    // a failed test keeps the block; both checks OK clear it, autopilot stays paused
    let check = |ok| account::Check {
        name: "x",
        ok,
        detail: String::new(),
    };
    assert!(account::unblock_if_ok(&st, &[check(true), check(false)]).is_none());
    assert!(st.history.account_blocked().unwrap().is_some());
    let note = account::unblock_if_ok(&st, &[check(true), check(true)]).unwrap();
    assert!(note.contains("Autopilot is paused"), "{note}");
    assert!(st.history.account_blocked().unwrap().is_none());
    assert!(!st.config().unwrap().autopilot.enabled);
}

#[tokio::test]
async fn single_send_refuses_at_the_hourly_cap() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = configured();
    cfg.account.provider = "icloud".into();
    let mail = MockSender::default();
    let st = state(cfg, dir.path(), mail.clone());
    for i in 0..25 {
        let mut r = crate::history::Record {
            broker_id: format!("old{i}"),
            broker_name: "Old".into(),
            email: "p@old.test".into(),
            template: "gdpr".into(),
            status: crate::history::Status::Failed,
            sent_at: Some(chrono::Local::now() - chrono::TimeDelta::minutes(30)),
            ..Default::default()
        };
        st.history.add(&mut r).unwrap();
    }
    let r = handlers::api_send_one(State(st.clone()), axum::extract::Path("b1".into())).await;
    let body = axum::body::to_bytes(r.into_body(), usize::MAX)
        .await
        .unwrap();
    let html = String::from_utf8_lossy(&body);
    assert!(
        html.contains("Hourly limit reached (25/h for iCloud)"),
        "{html}"
    );
    assert!(mail.0.lock().unwrap().is_empty());
}
