//! Handler-level tests that need access to `AppState` internals.

use super::*;
use crate::broker::Broker;
use crate::scanner::tests::MockSender;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, Method, Uri, header};

fn state(cfg: Config, dir: &std::path::Path, mail: MockSender) -> Arc<AppState> {
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
        history: Store::new(dir.join("h.db")).unwrap(),
        data_dir: dir.join("data"),
        port: 8080,
        public_origin: None,
        sender_factory: Arc::new(move |_| Ok(Box::new(mail.clone()) as Box<dyn Sender>)),
        autopilot_token: None,
    })
    .unwrap();
    st
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
