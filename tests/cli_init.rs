//! `eraser init` with scripted stdin writes an iCloud config that config::load accepts.

use std::io::Write;
use std::process::{Command, Stdio};

#[test]
fn init_writes_icloud_config() {
    let home = tempfile::tempdir().unwrap();
    let answers = "Jane\nDoe\njane@example.com\n1 Main St\nZurich\nZH\n8000\nSwitzerland\n+41 44 000 00 00\nicloud\njane@icloud.com\nabcd-efgh-ijkl-mnop\nn\nccpa\n";
    let mut child = Command::new(env!("CARGO_BIN_EXE_eraser"))
        .arg("init")
        .env("HOME", home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(answers.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let path = home.path().join(".eraser/config.yaml");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains(&format!("✅ Configuration saved to: {}", path.display())));

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    let cfg = eraser::config::load(&path).unwrap();
    cfg.validate().unwrap();
    assert_eq!(cfg.profile.first_name, "Jane");
    assert_eq!(cfg.profile.last_name, "Doe");
    assert_eq!(cfg.profile.city, "Zurich");
    assert_eq!(cfg.profile.phone, "+41 44 000 00 00");
    assert_eq!(cfg.account.provider, "icloud");
    assert_eq!(cfg.account.email, "jane@icloud.com");
    assert_eq!(
        cfg.inbox.server, "imap.mail.me.com",
        "inbox uses the same account"
    );
    assert_eq!(cfg.inbox.password, "abcd-efgh-ijkl-mnop");
    assert_eq!(cfg.email.provider, "smtp");
    assert_eq!(cfg.email.smtp.host, "smtp.mail.me.com");
    assert_eq!(cfg.email.smtp.port, 587);
    assert!(cfg.email.smtp.use_tls);
    assert_eq!(cfg.email.smtp.username, "jane@icloud.com");
    assert_eq!(cfg.email.smtp.password, "abcd-efgh-ijkl-mnop");
    assert_eq!(cfg.email.from, cfg.email.smtp.username);
    assert_eq!(cfg.options.template, "ccpa");
    assert_eq!(cfg.options.rate_limit_ms, 2000);
}

#[test]
fn init_defaults_template_to_auto_on_eof() {
    let home = tempfile::tempdir().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_eraser"))
        .arg("init")
        .env("HOME", home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"Jane\nDoe\n")
        .unwrap();
    assert!(child.wait().unwrap().success());
    let cfg = eraser::config::load(home.path().join(".eraser/config.yaml")).unwrap();
    assert_eq!(cfg.options.template, "auto");
    assert_eq!(cfg.email.smtp.host, "smtp.mail.me.com");
    assert_eq!(cfg.email.from, "");
}

#[test]
fn init_rejects_unknown_template() {
    let home = tempfile::tempdir().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_eraser"))
        .arg("init")
        .env("HOME", home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let answers = "J\nD\nj@x.com\n\n\n\n\n\n\n\nj@icloud.com\npw\n\nnosuch\n";
    child
        .stdin
        .take()
        .unwrap()
        .write_all(answers.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("unknown template \"nosuch\""));
    assert!(!home.path().join(".eraser/config.yaml").exists());
}

#[test]
fn init_rejects_unknown_provider() {
    let home = tempfile::tempdir().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_eraser"))
        .arg("init")
        .env("HOME", home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all("J\nD\nj@x.com\n\n\n\n\n\n\nyahoo\n".as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("unknown provider \"yahoo\""));
}

#[test]
fn parse_errors_print_once_and_reject_stray_args() {
    let home = tempfile::tempdir().unwrap();
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_eraser"))
            .args(args)
            .env("HOME", home.path())
            .output()
            .unwrap()
    };
    let out = run(&["pipeline", "typo"]);
    assert_eq!(out.status.code(), Some(1));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.starts_with("Error: unexpected argument 'typo'"),
        "{err}"
    );
    assert!(err.contains("Usage:\n  eraser pipeline [flags]"), "{err}");
    let out = run(&["bogus"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(err.matches("bogus").count(), 1, "{err}");
}
