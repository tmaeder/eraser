//! Output parity between the Go CLI (built from ./cmd/eraser) and the Rust port.
//! Both run in the repo root (so ./data/brokers.yaml is used) with HOME set to the
//! same temp dir. Skips (passes with a note) when `go` is unavailable.

use chrono::{Duration, Local};
use eraser::history::{
    BrokerResponse, PendingTask, PipelineStatus, Record, Status, Store, TaskType,
};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;

const RUST_BIN: &str = env!("CARGO_BIN_EXE_eraser");
const REPO: &str = env!("CARGO_MANIFEST_DIR");

// Dry-run safe: SMTP points at a closed local port, and only `send --dry-run` is exercised.
const FIXTURE_CONFIG: &str = "profile:
  first_name: Jane
  last_name: Doe
  email: jane@example.com
  country: Switzerland
email:
  provider: smtp
  from: jane@icloud.com
  smtp:
    host: 127.0.0.1
    port: 1
    username: jane@icloud.com
    password: not-a-password
    use_tls: true
options:
  template: gdpr
  rate_limit_ms: 1
  regions: [eu]
";

fn go_bin() -> Option<&'static Path> {
    static BIN: OnceLock<Option<PathBuf>> = OnceLock::new();
    BIN.get_or_init(|| {
        let out = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("eraser-go");
        let ok = Command::new("go")
            .args(["build", "-o"])
            .arg(&out)
            .arg("./cmd/eraser")
            .current_dir(REPO)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            eprintln!("skipping Go parity: `go build` unavailable or failed");
        }
        ok.then_some(out)
    })
    .as_deref()
}

#[derive(Debug, PartialEq)]
struct Output {
    stdout: String,
    stderr: String,
    code: i32,
}

fn run(bin: &Path, home: &Path, args: &[&str], stdin: &str) -> Output {
    let mut child = Command::new(bin)
        .args(args)
        .current_dir(REPO)
        .env("HOME", home)
        .env_remove("RUST_LOG")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    Output {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: strip_log_timestamps(&String::from_utf8_lossy(&out.stderr)),
        code: out.status.code().unwrap_or(-1),
    }
}

/// Go's log prefix "2006/01/02 15:04:05 " varies between runs.
fn strip_log_timestamps(s: &str) -> String {
    let re = regex::Regex::new(r"(?m)^\d{4}/\d\d/\d\d \d\d:\d\d:\d\d ").unwrap();
    re.replace_all(s, "").into_owned()
}

fn home_with_config() -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    let dir = home.path().join(".eraser");
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = dir.join("config.yaml");
    std::fs::write(&cfg, FIXTURE_CONFIG).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&cfg, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    home
}

fn seed_history(home: &Path) {
    let store = Store::new(home.join(".eraser/history.db")).unwrap();
    let now = Local::now();
    for i in 0..8i64 {
        let failed = i % 3 == 0;
        let mut r = Record {
            broker_id: format!("broker-{i}"),
            broker_name: format!("Broker {i}"),
            email: format!("privacy{i}@example.com"),
            template: "gdpr".into(),
            status: if failed { Status::Failed } else { Status::Sent },
            message_id: format!("<{i}@eraser>"),
            error: if failed {
                "550 mailbox unavailable".into()
            } else {
                String::new()
            },
            sent_at: Some(now - Duration::hours(i * 30)),
            ..Default::default()
        };
        store.add(&mut r).unwrap();
    }
    store
        .update_pipeline_status("broker-1", PipelineStatus::FormRequired)
        .unwrap();
    store
        .update_pipeline_status("broker-2", PipelineStatus::Confirmed)
        .unwrap();
    store
        .add_broker_response(&mut BrokerResponse {
            broker_id: "broker-1".into(),
            broker_name: "Broker 1".into(),
            response_type: "form_required".into(),
            form_url: "https://example.com/optout".into(),
            confidence: 0.8,
            received_at: Some(now),
            ..Default::default()
        })
        .unwrap();
    store
        .add_pending_task(&mut PendingTask {
            broker_id: "broker-1".into(),
            broker_name: "Broker 1".into(),
            task_type: TaskType::Captcha,
            form_url: "https://example.com/optout".into(),
            status: "pending".into(),
            ..Default::default()
        })
        .unwrap();
}

/// Runs both binaries (each in a fresh HOME prepared by `setup`) and returns (go, rust).
fn both(args: &[&str], stdin: &str, setup: fn(&Path)) -> Option<(Output, Output)> {
    let go = go_bin()?;
    let run_in_fresh_home = |bin: &Path| {
        let home = home_with_config();
        setup(home.path());
        run(bin, home.path(), args, stdin)
    };
    Some((
        run_in_fresh_home(go),
        run_in_fresh_home(Path::new(RUST_BIN)),
    ))
}

fn no_setup(_: &Path) {}

fn assert_same(args: &[&str], setup: fn(&Path)) {
    let Some((go, rs)) = both(args, "", setup) else {
        return;
    };
    assert_eq!(go, rs, "output differs for `eraser {}`", args.join(" "));
}

#[test]
fn help_matches() {
    let subcommands = [
        "init",
        "send",
        "list-brokers",
        "status",
        "add-broker",
        "monitor",
        "pipeline",
        "fill",
        "confirm",
        "cleanup-bounces",
    ];
    for sub in subcommands {
        assert_same(&[sub, "--help"], no_setup);
        assert_same(&[sub, "-h"], no_setup);
        assert_same(&["help", sub], no_setup);
    }
}

/// Intentional difference: cobra's auto-generated `completion` command is not ported.
#[test]
fn root_help_matches_except_completion() {
    for args in [&["--help"][..], &[]] {
        let Some((mut go, rs)) = both(args, "", no_setup) else {
            return;
        };
        go.stdout = go
            .stdout
            .lines()
            .filter(|l| !l.trim_start().starts_with("completion "))
            .map(|l| format!("{l}\n"))
            .collect();
        assert_eq!(go, rs);
    }
}

/// Intentional difference: serve gains --host and --public-origin (column width changes).
#[test]
fn serve_help_matches_except_new_flags() {
    let Some((go, rs)) = both(&["serve", "--help"], "", no_setup) else {
        return;
    };
    let norm = |s: &str| -> String {
        s.lines()
            .filter(|l| !l.contains("--host") && !l.contains("--public-origin"))
            .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" ") + "\n")
            .collect()
    };
    assert_eq!(norm(&go.stdout), norm(&rs.stdout));
    assert!(
        rs.stdout
            .contains("--host string            Address to listen on (default \"127.0.0.1\")")
    );
    assert_eq!((go.stderr, go.code), (rs.stderr, rs.code));
}

#[test]
fn list_brokers_matches() {
    assert_same(&["list-brokers"], no_setup);
}

#[test]
fn send_dry_run_matches() {
    assert_same(&["send", "--dry-run"], no_setup);
}

#[test]
fn status_matches() {
    assert_same(&["status"], seed_history);
    assert_same(&["status", "--limit", "5"], seed_history);
}

#[test]
fn pipeline_matches() {
    assert_same(&["pipeline"], seed_history);
    assert_same(&["pipeline"], no_setup);
}

#[test]
fn monitor_without_inbox_matches() {
    assert_same(&["monitor"], no_setup);
    assert_same(&["cleanup-bounces"], no_setup);
}

#[test]
fn missing_config_matches() {
    // Without a config file: same output apart from the OS error wording (io::Error vs Go's).
    let Some(go) = go_bin() else { return };
    let home = tempfile::tempdir().unwrap();
    let go_out = run(go, home.path(), &["send"], "");
    let rs_out = run(Path::new(RUST_BIN), home.path(), &["send"], "");
    assert_eq!(go_out.code, rs_out.code);
    for out in [&go_out, &rs_out] {
        let lines: Vec<&str> = out.stderr.lines().collect();
        assert!(lines[0].starts_with("WARNING: stat "), "{}", out.stderr);
        assert!(
            lines[1].starts_with("Error: failed to load config: failed to read config file: "),
            "{}",
            out.stderr
        );
    }
    // The usage block in between is identical.
    let usage = |o: &Output| {
        o.stderr
            .lines()
            .skip(2)
            .take(10)
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert_eq!(usage(&go_out), usage(&rs_out));
}

#[test]
fn parse_errors_match() {
    assert_same(&["bogus"], no_setup);
    assert_same(&["status", "--limit", "abc"], no_setup);
    assert_same(&["status", "--nope"], no_setup);
    assert_same(&["send", "--dry-run=maybe"], no_setup);
    assert_same(&["list-brokers", "ignored-positional"], no_setup);
}

#[test]
fn init_matches() {
    let answers = "Jane\nDoe\njane@example.com\n\nZurich\n\n8000\nSwitzerland\n\njane@icloud.com\nabcd-efgh-ijkl-mnop\n\n";
    let Some(go) = go_bin() else { return };
    // Same HOME path for both so the printed config path matches.
    let home = tempfile::tempdir().unwrap();
    let go_out = run(go, home.path(), &["init"], answers);
    let path = home.path().join(".eraser/config.yaml");
    let go_cfg = eraser::config::load(&path).unwrap();
    std::fs::remove_dir_all(home.path().join(".eraser")).unwrap();
    let rs_out = run(Path::new(RUST_BIN), home.path(), &["init"], answers);
    let rs_cfg = eraser::config::load(&path).unwrap();
    assert_eq!(go_out, rs_out);
    assert_eq!(go_cfg, rs_cfg);
}

fn seed_confirmations(home: &Path) {
    let store = Store::new(home.join(".eraser/history.db")).unwrap();
    for (id, url) in [
        (
            "spokeo",
            "https://www.spokeo.com/optout/confirm?token=abcdefghijklmnopqrstuvwxyz0123456789",
        ),
        ("shady", "https://phish.example.net/confirm?t=1"),
    ] {
        store
            .add_broker_response(&mut BrokerResponse {
                broker_id: id.into(),
                broker_name: id.into(),
                response_type: "confirmation_required".into(),
                confirm_url: url.into(),
                received_at: Some(Local::now()),
                ..Default::default()
            })
            .unwrap();
    }
}

#[test]
fn confirm_dry_run_matches() {
    assert_same(&["confirm"], no_setup);
    assert_same(&["confirm", "--pending"], no_setup);
    assert_same(&["confirm", "--pending", "--dry-run"], seed_confirmations);
    assert_same(
        &["confirm", "--broker", "spokeo", "--dry-run"],
        seed_confirmations,
    );
    assert_same(
        &["confirm", "--broker", "nobody", "--dry-run"],
        seed_confirmations,
    );
    assert_same(
        &[
            "confirm",
            "--url",
            "https://mail.spokeo.com/c?x=1",
            "--dry-run",
        ],
        no_setup,
    );
}

/// Rust-only: `send` records the template actually used ("auto" resolved per region).
/// SMTP points at a closed local port, so every send fails without leaving the machine.
#[test]
fn send_records_resolved_template() {
    let home = home_with_config();
    let cfg_path = home.path().join(".eraser/config.yaml");
    let cfg = FIXTURE_CONFIG
        .replace("template: gdpr", "template: auto")
        .replace("regions: [eu]", "regions: []");
    std::fs::write(&cfg_path, cfg).unwrap();
    let brokers = home.path().join("brokers.yaml");
    std::fs::write(
        &brokers,
        "brokers:
  - {id: swiss, name: Swiss Co, email: privacy@swiss.example, region: ch}
  - {id: euro, name: Euro Co, email: privacy@euro.example, region: eu}
  - {id: usa, name: US Co, email: privacy@us.example, region: us}
",
    )
    .unwrap();
    let out = run(
        Path::new(RUST_BIN),
        home.path(),
        &["--brokers", brokers.to_str().unwrap(), "send"],
        "",
    );
    assert_eq!(out.code, 0, "{out:?}");
    assert!(
        out.stdout.contains("📊 Complete: 0 sent, 3 failed"),
        "{out:?}"
    );
    let store = Store::new(home.path().join(".eraser/history.db")).unwrap();
    let mut got: Vec<(String, String)> = store
        .get_recent_requests(10)
        .unwrap()
        .into_iter()
        .map(|r| (r.broker_id, r.template))
        .collect();
    got.sort();
    assert_eq!(
        got,
        [("euro", "gdpr"), ("swiss", "fadp"), ("usa", "generic")]
            .map(|(a, b)| (a.to_string(), b.to_string()))
    );
}
