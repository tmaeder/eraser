//! Helper for the Go-vs-Rust side-by-side smoke check (run manually):
//! HOME=$tmp SMOKE_PORT=18081 SMOKE_CONFIG=... SMOKE_BROKERS=... cargo test --test web_smoke -- --ignored

#[tokio::test]
#[ignore]
async fn serve_for_smoke() {
    let env = |k: &str| std::env::var(k).unwrap_or_else(|_| panic!("{k} not set"));
    eraser::web::serve(eraser::web::ServeOptions {
        host: "127.0.0.1".into(),
        port: env("SMOKE_PORT").parse().unwrap(),
        public_origin: None,
        config_path: env("SMOKE_CONFIG").into(),
        broker_path: env("SMOKE_BROKERS").into(),
    })
    .await
    .unwrap();
}

/// Seeds a demo HOME for manual UI checks (run manually):
/// SEED_HOME=$tmp cargo test --test web_smoke seed_demo_home -- --ignored
/// then HOME=$tmp cargo run -- serve --port 18090
#[test]
#[ignore]
fn seed_demo_home() {
    use chrono::{Local, TimeDelta, Utc};
    use eraser::history::{
        BrokerResponse, PendingTask, PipelineStatus, Record, ScannedMessage, Status, Store,
        TaskType,
    };
    let home = std::path::PathBuf::from(std::env::var("SEED_HOME").expect("SEED_HOME not set"));
    let dir = home.join(".eraser");
    let mut cfg = eraser::config::Config::default();
    cfg.profile.first_name = "Ada".into();
    cfg.profile.last_name = "Lovelace".into();
    cfg.profile.email = "ada@example.com".into();
    cfg.email.provider = "smtp".into();
    cfg.email.from = "ada@example.com".into();
    cfg.email.smtp.host = "smtp.invalid".into();
    cfg.email.smtp.port = 587;
    cfg.autopilot.enabled = true;
    cfg.autopilot.reminder_days = 20;
    eraser::config::save(dir.join("config.yaml"), &cfg).unwrap();

    let store = Store::new(dir.join("history.db")).unwrap();
    let db = eraser::broker::load_from_file("data/brokers.yaml").unwrap();
    let now = Local::now();
    let mut n = 0;
    for b in db.brokers.iter().filter(|b| !b.email.is_empty()).take(180) {
        n += 1;
        let days = (n * 7) % 45;
        let mut r = Record {
            broker_id: b.id.clone(),
            broker_name: b.name.clone(),
            email: b.email.clone(),
            template: "gdpr".into(),
            status: if n % 23 == 0 {
                Status::Failed
            } else {
                Status::Sent
            },
            error: if n % 23 == 0 {
                "550 mailbox unavailable".into()
            } else {
                String::new()
            },
            sent_at: Some(now - TimeDelta::days(days)),
            ..Default::default()
        };
        store.add(&mut r).unwrap();
        let token = format!("{n:0>22}");
        let mid = format!("<{token}@example.com>");
        store.track_message(r.id, &mid).unwrap();
        match n % 5 {
            0 | 2 => drop(store.record_open(&token, "other", Utc::now())),
            1 => drop(store.record_open(&token, "apple-mpp", Utc::now())),
            _ => {}
        }
        if n % 7 == 0 {
            store.record_read_receipt(&[mid], Utc::now()).unwrap();
        }
        let reply = |t: &str, subject: &str| {
            store
                .add_broker_response(&mut BrokerResponse {
                    broker_id: b.id.clone(),
                    broker_name: b.name.clone(),
                    response_type: t.into(),
                    email_from: b.email.clone(),
                    email_subject: subject.into(),
                    received_at: Some(now - TimeDelta::days(days / 2)),
                    ..Default::default()
                })
                .unwrap()
        };
        match n % 9 {
            0..=2 => {
                reply("success", "Your data has been deleted");
                store
                    .update_pipeline_status(&b.id, PipelineStatus::Confirmed)
                    .unwrap();
            }
            3 => {
                reply("rejected", "We cannot process your request");
                store
                    .update_pipeline_status(&b.id, PipelineStatus::Rejected)
                    .unwrap();
            }
            4 => {
                reply("form_required", "Please use our web form");
                store
                    .add_pending_task(&mut PendingTask {
                        broker_id: b.id.clone(),
                        broker_name: b.name.clone(),
                        task_type: TaskType::ManualForm,
                        form_url: format!("https://{}.example/optout", b.id),
                        notes: "Fill in the opt-out form".into(),
                        ..Default::default()
                    })
                    .unwrap();
            }
            5 if days >= 20 => store.mark_reminder_sent(r.id).unwrap(),
            6 => reply("pending", "We received your request"),
            _ => {}
        }
    }
    let mut cut = Record {
        broker_id: "interrupted-demo".into(),
        broker_name: "Interrupted Demo Inc".into(),
        email: "privacy@demo.example".into(),
        status: Status::Pending,
        sent_at: Some(now - TimeDelta::hours(5)),
        ..Default::default()
    };
    store.add(&mut cut).unwrap();
    store
        .flag_interrupted_sends(now - TimeDelta::hours(1))
        .unwrap();

    let msg = |key: &str, from: &str| ScannedMessage {
        key: key.into(),
        domain: from.rsplit_once('@').unwrap().1.into(),
        from_addr: from.into(),
        from_name: "Shop".into(),
        date: Some(Utc::now()),
        is_list: true,
        unsub_mailto: format!("mailto:unsub@{}", from.rsplit_once('@').unwrap().1),
        ..Default::default()
    };
    let domains = ["shop.de", "travel.ch", "books.eu", "games.us"];
    let msgs: Vec<_> = domains
        .iter()
        .enumerate()
        .map(|(i, d)| msg(&format!("<{i}>"), &format!("news@{d}")))
        .collect();
    store.add_scanned_messages(&msgs).unwrap();
    let set = domains.map(String::from).into_iter().collect();
    eraser::scanner::recompute(&store, &eraser::scanner::Identity::default(), &set).unwrap();
    for (i, d) in domains.iter().take(3).enumerate() {
        let mut r = Record {
            broker_id: format!("company:{d}"),
            broker_name: d.to_string(),
            email: format!("privacy@{d}"),
            status: Status::Sent,
            sent_at: Some(now - TimeDelta::days(4 + i as i64 * 9)),
            ..Default::default()
        };
        store.add(&mut r).unwrap();
        store.mark_company(d, "requested").unwrap();
    }
    store
        .update_pipeline_status("company:books.eu", PipelineStatus::Confirmed)
        .unwrap();

    for i in 0..14 {
        let report = eraser::autopilot::RoundReport {
            dry_run: i == 3,
            requests_sent: [0, 40, 60, 25, 0, 12, 0, 8, 30, 0, 5, 0, 18, 2][i],
            reminders_sent: i % 4,
            links_confirmed: [0, 1, 0, 2, 0, 0, 3, 0, 1, 0, 0, 1, 0, 0][i],
            unsubscribed: [2, 0, 0, 5, 1, 0, 0, 0, 3, 0, 0, 0, 1, 0][i],
            todos_added: [1, 0, 2, 0, 0, 1, 0, 0, 0, 2, 0, 0, 1, 0][i],
            ..Default::default()
        };
        let id = store.begin_round(report.dry_run).unwrap().unwrap();
        store
            .finish_round(id, &serde_json::to_string(&report).unwrap())
            .unwrap();
    }
}
