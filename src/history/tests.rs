use super::*;
use serde_json::Value;

fn golden(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(name)
}

/// Go's JSON for time.Time / sql.NullTime -> Option (zero / invalid -> None).
fn go_time(v: &Value) -> Option<DateTime<Local>> {
    let v = match v.get("Valid") {
        Some(Value::Bool(false)) => return None,
        Some(_) => &v["Time"],
        None => v,
    };
    let t = DateTime::parse_from_rfc3339(v.as_str().unwrap()).unwrap();
    (t.year() > 1).then(|| t.with_timezone(&Local))
}

fn s(v: &Value) -> &str {
    v.as_str().unwrap()
}

fn open_go_db() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("history.db");
    std::fs::copy(golden("history_go.sqlite"), &path).unwrap();
    let store = Store::new(&path).unwrap();
    (dir, store)
}

#[test]
fn reads_database_written_by_go() {
    let want: Value =
        serde_json::from_str(&std::fs::read_to_string(golden("history_go.json")).unwrap()).unwrap();
    let (_dir, store) = open_go_db();

    let recent = store.get_recent_requests(10).unwrap();
    let go_recent = want["recent"].as_array().unwrap();
    assert_eq!(recent.len(), go_recent.len());
    for (r, g) in recent.iter().zip(go_recent) {
        assert_eq!(r.id, g["ID"].as_i64().unwrap());
        assert_eq!(r.broker_id, s(&g["BrokerID"]));
        assert_eq!(r.broker_name, s(&g["BrokerName"]));
        assert_eq!(r.email, s(&g["Email"]));
        assert_eq!(r.template, s(&g["Template"]));
        assert_eq!(r.status.as_str(), s(&g["Status"]));
        assert_eq!(r.message_id, s(&g["MessageID"]));
        assert_eq!(r.error, s(&g["Error"]));
        assert_eq!(r.sent_at, go_time(&g["SentAt"]), "sent_at of {}", r.id);
        assert_eq!(r.created_at, go_time(&g["CreatedAt"]));
    }
    assert_eq!(
        recent[1].pipeline_status,
        Some(PipelineStatus::FormRequired)
    );

    let last = store
        .get_last_request_for_broker("spokeo")
        .unwrap()
        .unwrap();
    assert_eq!(last.id, want["last_spokeo"]["ID"].as_i64().unwrap());
    assert!(
        store
            .get_last_request_for_broker("nobody")
            .unwrap()
            .is_none()
    );

    let responses = store.get_all_broker_responses().unwrap();
    let g = &want["responses"][0];
    let r = &responses[0];
    assert_eq!(responses.len(), 1);
    assert_eq!(r.response_type, s(&g["ResponseType"]));
    assert_eq!(r.email_body, s(&g["EmailBody"]));
    assert_eq!(r.form_url, s(&g["FormURL"]));
    assert_eq!(r.confidence, g["Confidence"].as_f64().unwrap());
    assert!(r.needs_review);
    assert_eq!(r.received_at, go_time(&g["ReceivedAt"]));
    assert_eq!(r.processed_at, go_time(&g["ProcessedAt"]));
    assert_eq!(r.created_at, go_time(&g["CreatedAt"]));

    let tasks = store.get_pending_tasks(None, None).unwrap();
    let go_tasks = want["tasks"].as_array().unwrap();
    assert_eq!(tasks.len(), go_tasks.len());
    for (t, g) in tasks.iter().zip(go_tasks) {
        assert_eq!(t.id, g["ID"].as_i64().unwrap());
        assert_eq!(t.task_type.as_str(), s(&g["TaskType"]));
        assert_eq!(t.status, s(&g["Status"]));
        assert_eq!(t.browser_state, s(&g["BrowserState"]));
        assert_eq!(t.notes, s(&g["Notes"]));
        assert_eq!(t.created_at, go_time(&g["CreatedAt"]));
        assert_eq!(t.opened_at, go_time(&g["OpenedAt"]));
        assert_eq!(t.completed_at, go_time(&g["CompletedAt"]));
    }

    let forms = store.get_forms_with_status().unwrap();
    let g = &want["forms"][0];
    assert_eq!(forms.len(), 1);
    assert_eq!(forms[0].status, s(&g["Status"]));
    assert_eq!(forms[0].task_id, g["TaskID"].as_i64().unwrap());
    assert_eq!(
        forms[0].pipeline_status.unwrap().as_str(),
        s(&g["PipelineStatus"])
    );
    assert_eq!(forms[0].detected_at, go_time(&g["DetectedAt"]));

    let pipeline = store.get_pipeline_stats().unwrap();
    for (k, v) in want["pipeline"].as_object().unwrap() {
        assert_eq!(
            pipeline[&k.parse::<PipelineStatus>().unwrap()],
            v.as_i64().unwrap()
        );
    }

    let (total, sent, failed) = store.get_stats().unwrap();
    assert_eq!(
        vec![total, sent, failed],
        serde_json::from_value::<Vec<i64>>(want["stats"].clone()).unwrap()
    );

    // Go's GetAllBrokerStatuses fails on MAX(sent_at); the port works.
    let statuses = store.get_all_broker_statuses().unwrap();
    assert_eq!(statuses.len(), 3);
    assert_eq!(statuses["spokeo"].total_sent, 2);
    assert_eq!(statuses["spokeo"].status, Status::Sent);
    assert_eq!(
        statuses["spokeo"].last_sent,
        go_time(&want["last_spokeo"]["SentAt"])
    );
    assert_eq!(statuses["nosend"].last_sent, None);

    // Writing to a Go database works too (and lands in the same layout).
    let mut rec = Record {
        broker_id: "spokeo".into(),
        status: Status::Sent,
        sent_at: Some(Local::now()),
        ..Default::default()
    };
    store.add(&mut rec).unwrap();
    assert_eq!(rec.id, 5);
    assert_eq!(
        store
            .get_last_request_for_broker("spokeo")
            .unwrap()
            .unwrap()
            .id,
        5
    );
}

#[test]
fn fresh_database_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().join("sub/history.db")).unwrap();
    assert_eq!(store.get_stats().unwrap(), (0, 0, 0));
    assert_eq!(store.get_pending_task_stats().unwrap(), (0, 0, 0));
    assert_eq!(store.get_monthly_stats().unwrap(), (0, 0));

    let now = Local::now();
    for (id, status) in [
        ("a", Status::Sent),
        ("b", Status::Failed),
        ("c", Status::Sent),
    ] {
        let mut r = Record {
            broker_id: id.into(),
            broker_name: id.to_uppercase(),
            email: format!("{id}@x.com"),
            template: "generic".into(),
            status,
            sent_at: Some(now),
            ..Default::default()
        };
        store.add(&mut r).unwrap();
        assert!(r.id > 0);
    }
    assert_eq!(store.get_stats().unwrap(), (3, 2, 1));
    assert_eq!(store.get_monthly_stats().unwrap(), (2, 1));
    let last = store.get_last_request_for_broker("a").unwrap().unwrap();
    assert_eq!(last.sent_at, Some(now));
    assert_eq!(last.pipeline_status, Some(PipelineStatus::EmailSent));

    store
        .update_pipeline_status("a", PipelineStatus::FormFilled)
        .unwrap();
    store
        .update_pipeline_status("b", PipelineStatus::Rejected)
        .unwrap();
    let ps = store.get_pipeline_stats().unwrap();
    assert_eq!(ps[&PipelineStatus::FormFilled], 1);
    assert_eq!(ps[&PipelineStatus::EmailSent], 1);

    let mut resp = BrokerResponse {
        broker_id: "a".into(),
        broker_name: "A".into(),
        response_type: "form_required".into(),
        email_subject: "Re: x".into(),
        email_body: "body".into(),
        form_url: "https://a/form".into(),
        received_at: Some(now),
        ..Default::default()
    };
    store.add_broker_response(&mut resp).unwrap();
    let mut resp_b = BrokerResponse {
        broker_id: "b".into(),
        broker_name: "B".into(),
        response_type: "rejected".into(),
        form_url: "https://b/f".into(),
        ..Default::default()
    };
    store.add_broker_response(&mut resp_b).unwrap();

    let found = store
        .find_broker_response_by_subject("a", "Re: x")
        .unwrap()
        .unwrap();
    assert_eq!(found.id, resp.id);
    assert_eq!(found.received_at, Some(now));
    store
        .update_broker_response_classification(
            resp.id,
            "success",
            "",
            "https://a/confirm",
            0.9,
            true,
        )
        .unwrap();
    store
        .update_broker_response_body(resp.id, "new body")
        .unwrap();
    let all = store.get_all_broker_responses().unwrap();
    let a = all.iter().find(|r| r.id == resp.id).unwrap();
    assert_eq!(
        (
            a.response_type.as_str(),
            a.confirm_url.as_str(),
            a.email_body.as_str()
        ),
        ("success", "https://a/confirm", "new body")
    );
    assert!(a.needs_review);
    assert_eq!(
        store
            .get_broker_responses(Some("success"), true, 10)
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        store
            .get_broker_responses(Some("success"), false, 10)
            .unwrap()[0]
            .email_body,
        ""
    );
    assert_eq!(store.get_broker_responses(None, true, 10).unwrap().len(), 1);
    assert_eq!(store.get_broker_responses(None, false, 1).unwrap().len(), 1);
    assert_eq!(store.get_response_stats().unwrap()["rejected"], 1);

    let forms = store.get_forms_with_status().unwrap();
    let status_of = |id: &str| {
        forms
            .iter()
            .find(|f| f.broker_id == id)
            .map(|f| f.status.clone())
    };
    assert_eq!(status_of("a"), None); // form_url cleared by reclassification
    assert_eq!(status_of("b").as_deref(), Some("skipped"));

    let mut task = PendingTask {
        broker_id: "b".into(),
        broker_name: "B".into(),
        task_type: TaskType::Captcha,
        form_url: "https://b/f".into(),
        ..Default::default()
    };
    store.add_pending_task(&mut task).unwrap();
    assert_eq!(store.get_forms_with_status().unwrap()[0].status, "captcha");
    assert_eq!(store.get_form_stats().unwrap(), (0, 0, 1, 0, 0));
    store.mark_task_opened(task.id).unwrap();
    let opened = store
        .get_pending_task_by_id(task.id)
        .unwrap()
        .unwrap()
        .opened_at;
    assert!(opened.is_some());
    store.mark_task_opened(task.id).unwrap();
    assert_eq!(
        store
            .get_pending_task_by_id(task.id)
            .unwrap()
            .unwrap()
            .opened_at,
        opened
    );
    store.complete_pending_task(task.id, "completed").unwrap();
    let t = store.get_pending_task_by_id(task.id).unwrap().unwrap();
    assert_eq!(t.status, "completed");
    assert!(t.completed_at.is_some());
    assert_eq!(store.get_forms_with_status().unwrap()[0].status, "filled");
    assert_eq!(
        store
            .get_pending_tasks(Some(TaskType::Captcha), Some("completed"))
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        store
            .get_pending_tasks(Some(TaskType::Review), None)
            .unwrap()
            .len(),
        0
    );
    assert_eq!(
        store
            .get_pending_tasks(None, Some("pending"))
            .unwrap()
            .len(),
        0
    );
    assert_eq!(store.get_pending_task_stats().unwrap(), (0, 1, 0));
    assert!(store.get_pending_task_by_id(999).unwrap().is_none());

    store.clear_broker_responses().unwrap();
    assert!(store.get_all_broker_responses().unwrap().is_empty());
    assert_eq!(store.delete_by_status(Status::Sent).unwrap(), 2);
    assert_eq!(store.get_stats().unwrap(), (1, 0, 1));

    // Reopening runs the migrations again without error.
    drop(store);
    Store::new(dir.path().join("sub/history.db")).unwrap();
}

#[test]
fn time_format_and_parse() {
    let t = Local.with_ymd_and_hms(2026, 3, 14, 9, 26, 53).unwrap()
        + chrono::Duration::nanoseconds(589_793_000);
    let s = format_time(&t);
    assert!(s.starts_with("2026-03-14 09:26:53.589793 "), "{s}");
    let re =
        regex::Regex::new(r"^\d{4}-\d\d-\d\d \d\d:\d\d:\d\d(\.\d*[1-9])? [+-]\d{4} \S+$").unwrap();
    assert!(re.is_match(&s), "{s}");
    assert_eq!(parse_time(&s), Some(t));
    assert!(
        format_time(&Local.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap())
            .starts_with("2026-01-01 00:00:00 ")
    );

    let utc = |y, mo, d, h, mi, sec| {
        Some(
            Utc.with_ymd_and_hms(y, mo, d, h, mi, sec)
                .unwrap()
                .with_timezone(&Local),
        )
    };
    assert_eq!(
        parse_time("2026-11-09 21:42:47 +0100 CET m=+0.043945543"),
        utc(2026, 11, 9, 20, 42, 47)
    );
    assert_eq!(
        parse_time("2026-11-09 20:42:47 +0000 UTC"),
        utc(2026, 11, 9, 20, 42, 47)
    );
    assert_eq!(
        parse_time("2026-11-09 20:42:47"),
        utc(2026, 11, 9, 20, 42, 47)
    ); // CURRENT_TIMESTAMP
    assert_eq!(
        parse_time("2026-11-09T20:42:47Z"),
        utc(2026, 11, 9, 20, 42, 47)
    );
    assert_eq!(
        parse_time("2026-11-09T21:42:47+01:00"),
        utc(2026, 11, 9, 20, 42, 47)
    );
    assert_eq!(parse_time("2026-11-09"), utc(2026, 11, 9, 0, 0, 0));
    assert_eq!(parse_time("0001-01-01 00:00:00 +0000 UTC"), None);
    assert_eq!(parse_time("garbage"), None);
}

#[test]
fn enums_round_trip() {
    for p in PipelineStatus::ALL {
        assert_eq!(p.as_str().parse::<PipelineStatus>().unwrap(), *p);
    }
    assert_eq!(
        serde_json::to_string(&TaskType::ManualForm).unwrap(),
        "\"manual_form\""
    );
    assert!("nope".parse::<Status>().is_err());
}

/// Writes a DB with Rust for the Go reader check: `ERASER_RUST_DB=/path cargo test -- --ignored`.
#[test]
#[ignore]
fn write_db_for_go() {
    let path = std::env::var("ERASER_RUST_DB").unwrap();
    let _ = std::fs::remove_file(&path);
    let store = Store::new(&path).unwrap();
    let mut r = Record {
        broker_id: "rs".into(),
        broker_name: "Rust".into(),
        email: "r@s.com".into(),
        template: "gdpr".into(),
        status: Status::Sent,
        sent_at: Some(Local::now()),
        ..Default::default()
    };
    store.add(&mut r).unwrap();
    let mut resp = BrokerResponse {
        broker_id: "rs".into(),
        broker_name: "Rust".into(),
        response_type: "form_required".into(),
        email_from: "a@b".into(),
        email_subject: "s".into(),
        form_url: "https://f".into(),
        received_at: Some(Local::now()),
        ..Default::default()
    };
    store.add_broker_response(&mut resp).unwrap();
    let mut t = PendingTask {
        broker_id: "rs".into(),
        broker_name: "Rust".into(),
        task_type: TaskType::ManualForm,
        ..Default::default()
    };
    store.add_pending_task(&mut t).unwrap();
    store.mark_task_opened(t.id).unwrap();
}

#[test]
fn autopilot_task_kinds_and_skip_column() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().join("h.db")).unwrap();
    let mut resp = BrokerResponse {
        broker_id: "f".into(),
        broker_name: "F".into(),
        response_type: "form_required".into(),
        form_url: "https://f.test/form".into(),
        ..Default::default()
    };
    store.add_broker_response(&mut resp).unwrap();
    let mut form = PendingTask {
        broker_id: "f".into(),
        broker_name: "F".into(),
        task_type: TaskType::ManualForm,
        form_url: "https://f.test/form".into(),
        ..Default::default()
    };
    store.add_pending_task(&mut form).unwrap();
    assert_eq!(
        store.get_forms_with_status().unwrap()[0].status,
        "pending",
        "a web-form to-do is not a captcha"
    );

    let mut id = PendingTask {
        broker_id: "g".into(),
        broker_name: "G".into(),
        task_type: TaskType::IdRequired,
        ..Default::default()
    };
    store.add_pending_task(&mut id).unwrap();
    let back = store.get_pending_task_by_id(id.id).unwrap().unwrap();
    assert_eq!(
        (back.task_type, back.skipped_until),
        (TaskType::IdRequired, None)
    );
    assert_eq!(
        "unsubscribe_link".parse::<TaskType>().unwrap(),
        TaskType::UnsubscribeLink
    );

    store.complete_pending_task(form.id, "completed").unwrap();
    assert_eq!(store.get_forms_with_status().unwrap()[0].status, "filled");
}

#[test]
fn go_database_gains_autopilot_columns() {
    let (_dir, store) = open_go_db();
    let tasks = store.get_pending_tasks(None, None).unwrap();
    assert!(!tasks.is_empty() && tasks.iter().all(|t| t.skipped_until.is_none()));
    assert_eq!(
        store.get_forms_with_status().unwrap()[0].status,
        "captcha",
        "Go captcha task unchanged"
    );
}
