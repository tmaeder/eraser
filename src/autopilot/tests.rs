use super::*;
use crate::history::{BrokerResponse, ScannedMessage, Status};
use crate::scanner::actions;
use crate::scanner::tests::MockSender;
use std::sync::Arc;

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
