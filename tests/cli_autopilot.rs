//! `eraser autopilot --dry-run` runs one round from the CLI without sending anything.

use std::process::Command;

#[test]
fn autopilot_dry_run_sends_nothing() {
    let home = tempfile::tempdir().unwrap();
    let dir = home.path().join(".eraser");
    std::fs::create_dir_all(&dir).unwrap();
    let cfg_path = dir.join("config.yaml");
    std::fs::write(
        &cfg_path,
        "profile:\n  first_name: Ada\n  last_name: Lovelace\n  email: ada@example.com\nemail:\n  provider: smtp\n  from: ada@example.com\n  smtp:\n    host: 127.0.0.1\n    port: 1\noptions:\n  template: auto\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&cfg_path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let brokers = home.path().join("brokers.yaml");
    std::fs::write(
        &brokers,
        "brokers:\n  - id: acme\n    name: Acme\n    email: privacy@acme.example\n    region: eu\n    category: people-search\n",
    )
    .unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_eraser"))
        .args(["autopilot", "--dry-run", "--brokers"])
        .arg(&brokers)
        .env("HOME", home.path())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains("Dry run: 1 broker requests"), "{stdout}");

    let store = eraser::history::Store::new(dir.join("history.db")).unwrap();
    assert!(
        store.get_recent_requests(10).unwrap().is_empty(),
        "nothing sent"
    );
    assert_eq!(store.recent_rounds(5).unwrap().len(), 1);
}
