# Autopilot Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Eraser runs unattended rounds (inbox check, confirmation links, broker requests + one reminder, company scan, one-click unsubscribe, company requests) every 6 hours on erase.kampa.app and via `eraser autopilot [--dry-run]`; the user only handles the to-dos shown on a new "Today" start page.

**Architecture:** A new library module `src/autopilot/` exposes `run_round(&Deps, Option<Monitor>, dry_run, round_id) -> RoundReport`, reusing the existing sender, IMAP monitor, classifier, scanner, discovery, confirmation handler and daily limit. Rounds are serialized through a row in the new `autopilot_rounds` table (`Store::begin_round`), which also stores the JSON report. To-dos are rows in the existing `pending_tasks` table (two new kinds, a `skipped_until` column). The web UI gets a CSRF-exempt, token-protected `POST /internal/autopilot/tick` (202 + background round, 409 while running), a Today page and a Settings → Autopilot form. The Cloudflare Worker gets a `scheduled` handler (cron `0 */6 * * *`) that calls the tick with a Worker secret passed to the container as `ERASER_AUTOPILOT_TOKEN`, and answers 404 to `/internal/*` from the internet.

**Tech Stack:** Rust 2024, axum 0.8, minijinja 3, rusqlite (bundled SQLite), async-imap, reqwest, clap; Cloudflare Workers + `@cloudflare/containers` 0.3.7, wrangler 4; Node 24 built-in test runner (`node --test`, native type stripping) for the Worker guard.

---

## Design decisions resolved in this plan

- **Dry run** never contacts anyone: no email, no confirmation click, no unsubscribe POST, no IMAP move/archive. Local bookkeeping still happens (replies are classified and stored, scanned headers and discovered contacts are stored, to-dos for blocked links/forms are created) and the round row is written with "would" counts.
- **Master switch:** `autopilot.enabled` (default `false`) gates only the scheduled tick. "Pause" on the Today page sets it to `false`. It can only be turned on when `Config::validate()` passes ("setup complete"). `eraser autopilot` and the "Run now"/"Dry run now" buttons are explicit user actions; the CLI runs regardless of the switch, the "Run now" button requires the switch to be on, "Dry run now" always works.
- **Lock:** an unfinished `autopilot_rounds` row younger than 2 h (`STALE_ROUND`) blocks a new round; the check+insert runs in a `BEGIN IMMEDIATE` transaction, so it also works across processes (CLI vs. server). Older unfinished rows count as "interrupted".
- **"No response":** a request whose reminder was sent and that still has no broker reply counts as "no response" (`reminder_sent_at IS NOT NULL`); no new pipeline status is introduced. A broker "replied" if any `broker_responses` row exists for its id. Reminders go to inbox-company requests (`company:<domain>`) too.
- **Daily limit:** `email::daily_limit(provider)` (moved out of the web job, 250 for SMTP) minus the emails already sent today (`removal_requests.sent_at` + `reminder_sent_at` since local midnight). Reminders are taken first, then new broker requests, then company requests.
- **Confirmation links:** allowed hosts = registrable domains of the broker's website and of its privacy address (`ConfirmationHandler` adds `www.`/`mail.`/`email.`). Each link is handled at most once: any `confirm` task with that URL (any status) marks it handled. Success → completed `confirm` task (the log) + pipeline `confirmed`; captcha (`detect_captcha_from_html`), foreign domain, network error or non-success page → pending `confirm` to-do.
- **ID requests:** the existing (Go-golden-tested) classifier is untouched; `autopilot::asks_for_id` keyword-checks the stored reply body and creates an `id_required` to-do.
- **Web-form brokers:** every pending detected form without a task gets a `manual_form` to-do. `get_forms_with_status` now shows "captcha" only for pending *captcha* tasks, so a pending `manual_form` to-do leaves the form "pending" (Go golden data unaffected: its only task is a captcha task).
- **Unsubscribe:** only `Unsubscribe::OneClick` (RFC 8058) is executed. `Mailto` and `Manual` links become `unsubscribe_link` to-dos (one per link); "Done" on such a to-do also marks the company unsubscribed. Unsubscribe and company requests share one eligibility rule: shown (not excluded/ignored), not on the keep list, and inactive for `inactive_months` (last mail older than `30 × months` days) or newsletter-only (every scanned message carries list headers).
- **Verified privacy contact:** `contact_status` is `broker` or `website` and `scanner::actions::request_target` accepts the address.
- **Keep list:** the built-in rules (CH/EU banks/insurers/health insurers, government suffixes and cantonal `.ch` domains, keywords in domain or sender name, the user's own non-freemail email domains) are always on and shown read-only; the editor edits `keep_domains` (one per line). The keep list applies to inbox companies only, not to brokers from `data/brokers.yaml`.
- **Company scan:** 365 days on the very first scan (no companies yet), otherwise the last 7 days; contact discovery only for companies without a contact status, not kept, at most 50 per round.
- **Inbox check:** last 7 days of the inbox folder and the archive folder with `BODY.PEEK` (nothing marked read); replies already stored (same broker + subject) are skipped; `inbox.auto_archive` is honoured (not in dry run).
- **Start page:** `/` becomes "Today"; the previous dashboard moves to `/dashboard`.
- **Next round** is shown only when the server has a tick token (hosted); it is the next 6-hour UTC slot.
- **Tick when the autopilot is off:** `200 {"status":"disabled"}`, no round row.

## File Structure

| File | Status | Responsibility |
|------|--------|----------------|
| `src/config.rs` | Modify | `AutopilotConfig` (`autopilot:` section), defaults, normalization in `load` |
| `src/autopilot/mod.rs` | Create | `Deps`, `RoundReport`, `run_round` and the six steps, `next_round`, `asks_for_id`, `default_monitor` |
| `src/autopilot/keep.rs` | Create | Keep-list classifier (`keep_reason`, built-in lists, `summary`) |
| `src/autopilot/tests.rs` | Create | Round tests (mock sender, fake HTTP server, GreenMail) |
| `src/lib.rs` | Modify | `pub mod autopilot;` |
| `src/history.rs` | Modify | New task kinds, `PendingTask.skipped_until`, column migrations, `Record::for_send`, forms status fix, `mod rounds` |
| `src/history/rounds.rs` | Create | `autopilot_rounds` table + lock, reminder queries, to-do queries, progress counts |
| `src/history/tests.rs` | Modify | Store tests for the new tables/columns |
| `templates/email/reminder.txt` | Create | Reminder letter (original date, deadline, law by region) |
| `src/template.rs` | Modify | `Engine::render_reminder[_at]`, shared `email_data` |
| `src/email.rs` | Modify | `daily_limit(provider)` |
| `src/inbox/mod.rs` | Modify | Shared `body_of`, `response_record`, `pipeline_status_for` |
| `src/web/handlers.rs` | Modify | Use the shared helpers, `settings_with`, `parse_id` visibility, config view gets `autopilot` |
| `src/cli/inbox_cmds.rs` | Modify | Use shared `pipeline_status_for` |
| `src/web/csrf.rs` | Modify | `constant_time_eq` visible to the web module |
| `src/web/today.rs` | Create | Tick endpoint, background round start, Today page, pause/run, to-do Done/Skip, Settings → Autopilot |
| `src/web/mod.rs` | Modify | `autopilot_token` option/state, routes, CSRF-exempt internal router, env token |
| `src/web/views.rs` | Modify | Register `today.html` |
| `templates/web/today.html` | Create | Today page |
| `templates/web/layout.html` | Modify | Nav: Today + Dashboard |
| `templates/web/settings.html` | Modify | Autopilot card |
| `src/web/tests.rs` | Modify | New `AppOptions` field |
| `tests/web.rs` | Modify | Router tests: tick, Today, settings, CSRF |
| `src/cli/args.rs` | Modify | `Cmd::Autopilot { dry_run }` |
| `src/cli/commands.rs` | Modify | Dispatch + `run_autopilot` |
| `tests/cli_autopilot.rs` | Create | `eraser autopilot --dry-run` end to end |
| `deploy/src/guard.ts` | Create | `isInternalPath` |
| `deploy/test/guard.test.ts` | Create | Node test for the guard |
| `deploy/src/index.ts` | Modify | Block `/internal/*`, container env var, `scheduled` handler |
| `deploy/wrangler.jsonc` | Modify | Cron trigger |
| `deploy/package.json` | Modify | `test` script |
| `README.md`, `CLAUDE.md` | Modify | Autopilot docs, security note, structure |

---

### Task 1: Config `autopilot:` section

**Files:**
- Modify: `src/config.rs` (struct `Config` ~line 23–35, `load` ~line 185–210, tests module at the end)

- [ ] **Step 1: Write the failing test.** Append inside `mod tests` in `src/config.rs` (before its closing `}`):

```rust
    #[test]
    fn autopilot_defaults_and_partial_section() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.yaml");
        std::fs::write(
            &path,
            "autopilot:\n  enabled: true\n  unsubscribe: false\n  reminder_days: 0\n  keep_domains:\n    - Example.ORG\n",
        )
        .unwrap();
        let cfg = load(&path).unwrap();
        let a = &cfg.autopilot;
        assert!(a.enabled && !a.unsubscribe);
        assert!(a.check_inbox && a.confirm_links && a.broker_requests);
        assert!(a.scan_inbox && a.company_requests);
        assert_eq!((a.reminder_days, a.inactive_months), (30, 12), "0 falls back to the default");
        assert_eq!(a.keep_domains, ["Example.ORG"]);

        let d = Config::default();
        assert!(!d.autopilot.enabled, "off by default");
        assert!(d.autopilot.check_inbox, "steps default on");
        let yaml = serde_yaml_ng::to_string(&d).unwrap();
        assert!(!yaml.contains("autopilot"), "default section is omitted");
    }
```

- [ ] **Step 2: Run it, expect a compile failure.**

Run: `cargo test --lib config::tests::autopilot_defaults_and_partial_section`
Expected: `error[E0609]: no field `autopilot` on type `Config``

- [ ] **Step 3: Implement.** In `src/config.rs`, in `struct Config` replace

```rust
    #[serde(skip_serializing_if = "is_default", deserialize_with = "nullable")]
    pub pipeline: Pipeline,
}
```

with

```rust
    #[serde(skip_serializing_if = "is_default", deserialize_with = "nullable")]
    pub pipeline: Pipeline,
    #[serde(skip_serializing_if = "is_default", deserialize_with = "nullable")]
    pub autopilot: AutopilotConfig,
}

pub const DEFAULT_REMINDER_DAYS: i64 = 30;
pub const DEFAULT_INACTIVE_MONTHS: i64 = 12;

/// Unattended rounds (`eraser autopilot`, hosted cron). The scheduled rounds only run when
/// `enabled`; every step can be switched off on its own.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AutopilotConfig {
    pub enabled: bool,
    pub check_inbox: bool,
    pub confirm_links: bool,
    /// New broker requests plus the one reminder.
    pub broker_requests: bool,
    pub scan_inbox: bool,
    /// RFC 8058 one-click unsubscribes.
    pub unsubscribe: bool,
    pub company_requests: bool,
    /// Days without any reply before the single reminder.
    pub reminder_days: i64,
    /// Months without mail before an inbox company counts as inactive.
    pub inactive_months: i64,
    /// Domains never contacted, on top of the built-in keep rules.
    #[serde(skip_serializing_if = "Vec::is_empty", deserialize_with = "nullable")]
    pub keep_domains: Vec<String>,
}

impl Default for AutopilotConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            check_inbox: true,
            confirm_links: true,
            broker_requests: true,
            scan_inbox: true,
            unsubscribe: true,
            company_requests: true,
            reminder_days: DEFAULT_REMINDER_DAYS,
            inactive_months: DEFAULT_INACTIVE_MONTHS,
            keep_domains: Vec::new(),
        }
    }
}
```

In `load`, replace

```rust
    if cfg.pipeline.browser_timeout_sec == 0 {
        cfg.pipeline.browser_timeout_sec = 30;
    }
```

with

```rust
    if cfg.pipeline.browser_timeout_sec == 0 {
        cfg.pipeline.browser_timeout_sec = 30;
    }
    if cfg.autopilot.reminder_days <= 0 {
        cfg.autopilot.reminder_days = DEFAULT_REMINDER_DAYS;
    }
    if cfg.autopilot.inactive_months <= 0 {
        cfg.autopilot.inactive_months = DEFAULT_INACTIVE_MONTHS;
    }
```

- [ ] **Step 4: Run the config tests, expect pass.**

Run: `cargo test --lib config::tests`
Expected: `test result: ok. 5 passed`

- [ ] **Step 5: Commit.**

```bash
git commit -m "Config: autopilot section (off by default, per-step switches)" -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>" -- src/config.rs
```

---

### Task 2: Keep-list classifier

**Files:**
- Create: `src/autopilot/mod.rs`, `src/autopilot/keep.rs`
- Modify: `src/lib.rs`
- Test: inline `#[cfg(test)] mod tests` in `src/autopilot/keep.rs`

- [ ] **Step 1: Create the module skeleton and the failing test.** Create `src/autopilot/mod.rs`:

```rust
//! Autopilot: unattended rounds (see `run_round`).

pub mod keep;
```

In `src/lib.rs` replace

```rust
pub mod broker;
```

with

```rust
pub mod autopilot;
pub mod broker;
```

Create `src/autopilot/keep.rs` with only the test first:

```rust
//! Keep list: companies the autopilot never contacts.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keep_rules() {
        let mut cfg = Config::default();
        cfg.profile.email = "me@employer.example".into();
        cfg.inbox.email = "me@icloud.com".into();
        cfg.autopilot.keep_domains = vec!["Club.Example".into(), " ".into()];
        let k = |d: &str, n: &str| keep_reason(d, n, &cfg);
        assert_eq!(k("ubs.com", "UBS"), Some("bank or insurer"));
        assert_eq!(k("news.helsana.ch", ""), Some("bank or insurer"));
        assert_eq!(k("estv.admin.ch", ""), Some("government"));
        assert_eq!(k("steuern.zh.ch", ""), Some("government"));
        assert_eq!(k("irs.gov", ""), Some("government"));
        assert_eq!(k("bmf.gv.at", ""), Some("government"));
        assert_eq!(k("meinebank.de", ""), Some("finance, health or tax"));
        assert_eq!(k("praxis.de", "Dr. med. Arzt Praxis"), Some("finance, health or tax"));
        assert_eq!(k("Mail.Employer.Example", ""), Some("own domain"));
        assert_eq!(k("icloud.com", "Apple"), None, "freemail is not an own domain");
        assert_eq!(k("news.club.example", ""), Some("keep list"));
        assert_eq!(k("zalando.de", "Zalando"), None);
        assert_eq!(k("booking.com", "Booking.com"), None, "suffix match needs a dot");
        assert!(summary().contains("versicherung"));
    }
}
```

- [ ] **Step 2: Run it, expect a compile failure.**

Run: `cargo test --lib autopilot::keep`
Expected: `error[E0425]: cannot find function `keep_reason` in this scope`

- [ ] **Step 3: Implement.** Insert above the `#[cfg(test)]` line in `src/autopilot/keep.rs`:

```rust
use crate::config::Config;
use crate::scanner::Identity;
use crate::scanner::domain::registrable_domain;

/// Built-in Swiss and EU banks, insurers and health insurers (registrable domains).
pub const BUILTIN_DOMAINS: &[&str] = &[
    // Swiss banks
    "ubs.com", "credit-suisse.com", "postfinance.ch", "raiffeisen.ch", "zkb.ch", "bcv.ch",
    "bcge.ch", "lukb.ch", "bekb.ch", "valiant.ch", "migrosbank.ch", "cler.ch", "yuh.com",
    "neon-free.ch", "swissquote.ch", "juliusbaer.com", "vontobel.com", "twint.ch",
    // Swiss insurers and health insurers
    "css.ch", "helsana.ch", "swica.ch", "sanitas.com", "visana.ch", "concordia.ch",
    "groupemutuel.ch", "kpt.ch", "assura.ch", "sympany.ch", "oekk.ch", "atupri.ch", "egk.ch",
    "axa.ch", "mobiliar.ch", "zurich.ch", "allianz.ch", "generali.ch", "helvetia.com",
    "baloise.ch", "vaudoise.ch", "swisslife.ch", "suva.ch",
    // EU banks, insurers and health insurers
    "axa.com", "zurich.com", "allianz.de", "allianz.com", "ergo.de", "huk.de", "debeka.de",
    "generali.com", "deutsche-bank.de", "commerzbank.de", "sparkasse.de", "ing.de", "ing.com",
    "dkb.de", "comdirect.de", "n26.com", "revolut.com", "bawag.at", "erstebank.at",
    "raiffeisen.at", "bnpparibas.com", "societegenerale.fr", "credit-agricole.fr", "abnamro.nl",
    "rabobank.nl", "santander.com", "bbva.com", "unicredit.it", "intesasanpaolo.com", "tk.de",
    "aok.de", "barmer.de", "dak.de",
];

/// Government domains and suffixes (".admin.ch", ".gov", ".gv.at", …).
pub const GOVERNMENT_SUFFIXES: &[&str] = &[
    "admin.ch", "ch.ch", "gov", "mil", "gv.at", "gov.uk", "gouv.fr", "bund.de", "europa.eu",
    "gob.es", "gov.it", "overheid.nl", "fgov.be", "belgium.be", "llv.li",
];

/// Cantonal government domains are `<canton>.ch` (e.g. `steuern.zh.ch` → `zh.ch`).
pub const CANTONS: &[&str] = &[
    "ag", "ai", "ar", "be", "bl", "bs", "fr", "ge", "gl", "gr", "ju", "lu", "ne", "nw", "ow",
    "sg", "sh", "so", "sz", "tg", "ti", "ur", "vd", "vs", "zg", "zh",
];

/// Substrings of a domain or sender name that mark finance, insurance, health or tax mail.
pub const KEYWORDS: &[&str] = &[
    "bank", "insurance", "versicherung", "krankenkasse", "clinic", "klinik", "arzt", "steuer",
];

fn matches(domain: &str, entry: &str) -> bool {
    domain == entry || domain.ends_with(&format!(".{entry}"))
}

/// Why the company at `domain` (sender display `name`) must never be contacted, or None.
pub fn keep_reason(domain: &str, name: &str, cfg: &Config) -> Option<&'static str> {
    let domain = registrable_domain(domain);
    let d = domain.as_str();
    let user_kept = cfg
        .autopilot
        .keep_domains
        .iter()
        .map(|k| registrable_domain(k.trim()))
        .any(|k| !k.is_empty() && matches(d, &k));
    if user_kept {
        return Some("keep list");
    }
    if Identity::from_config(cfg).domains.contains(d) {
        return Some("own domain");
    }
    if BUILTIN_DOMAINS.iter().any(|b| matches(d, b)) {
        return Some("bank or insurer");
    }
    if GOVERNMENT_SUFFIXES.iter().any(|g| matches(d, g))
        || CANTONS.iter().any(|c| d == format!("{c}.ch"))
    {
        return Some("government");
    }
    let name = name.to_lowercase();
    if KEYWORDS.iter().any(|k| d.contains(k) || name.contains(k)) {
        return Some("finance, health or tax");
    }
    None
}

/// The always-on rules, for the settings page.
pub fn summary() -> String {
    format!(
        "Always kept, in addition to your list: {} built-in Swiss and EU banks, insurers and health insurers; government domains (.admin.ch, cantonal .ch, .gov, .gv.at, …); domains or sender names containing {}; your own email domain.",
        BUILTIN_DOMAINS.len(),
        KEYWORDS.join(", ")
    )
}

```

- [ ] **Step 4: Run it, expect pass.**

Run: `cargo test --lib autopilot::keep`
Expected: `test autopilot::keep::tests::keep_rules ... ok`

- [ ] **Step 5: Commit.**

```bash
git add src/autopilot/mod.rs src/autopilot/keep.rs
git commit -m "Autopilot: keep-list classifier (banks, insurers, government, keywords, own domain)" -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>" -- src/autopilot/mod.rs src/autopilot/keep.rs src/lib.rs
```

---

### Task 3: DB – new to-do kinds, `skipped_until`, `reminder_sent_at`

**Files:**
- Modify: `src/history.rs` (TaskType ~line 78–86, PendingTask ~144–180, TASK_COLUMNS ~327, scan_task ~364–379, migrate ~484–490, get_forms_with_status ~759–823)
- Test: `src/history/tests.rs`

- [ ] **Step 1: Write the failing tests.** Append to `src/history/tests.rs`:

```rust
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
    assert_eq!((back.task_type, back.skipped_until), (TaskType::IdRequired, None));
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
```

- [ ] **Step 2: Run them, expect a compile failure.**

Run: `cargo test --lib history::tests::autopilot_task_kinds_and_skip_column`
Expected: `error[E0599]: no variant or associated item named `IdRequired` found for enum `TaskType``

- [ ] **Step 3: Implement.** In `src/history.rs` replace

```rust
    TaskType {
        Captcha = "captcha",
        ManualForm = "manual_form",
        Review = "review",
        Confirm = "confirm",
    }
```

with

```rust
    TaskType {
        Captcha = "captcha",
        ManualForm = "manual_form",
        Review = "review",
        Confirm = "confirm",
        // The broker asks for an identity document (never sent automatically).
        IdRequired = "id_required",
        // A plain (non RFC 8058) unsubscribe link the user opens.
        UnsubscribeLink = "unsubscribe_link",
    }
```

In `struct PendingTask` replace

```rust
    pub completed_at: Option<DateTime<Local>>,
}

impl Default for PendingTask {
```

with

```rust
    pub completed_at: Option<DateTime<Local>>,
    /// Hidden from the Today to-do list until then ("Skip"); the task stays pending.
    pub skipped_until: Option<DateTime<Local>>,
}

impl Default for PendingTask {
```

and in its `Default` impl replace

```rust
            completed_at: None,
        }
```

with

```rust
            completed_at: None,
            skipped_until: None,
        }
```

Replace

```rust
const TASK_COLUMNS: &str = "id, broker_id, broker_name, task_type, form_url, screenshot_path, browser_state, notes, status, created_at, opened_at, completed_at";
```

with

```rust
const TASK_COLUMNS: &str = "id, broker_id, broker_name, task_type, form_url, screenshot_path, browser_state, notes, status, created_at, opened_at, completed_at, skipped_until";
```

In `scan_task` replace

```rust
        completed_at: time_col(row, 11)?,
    })
```

with

```rust
        completed_at: time_col(row, 11)?,
        skipped_until: time_col(row, 12)?,
    })
```

In `migrate` replace

```rust
        let _ = db.execute(
            "ALTER TABLE broker_responses ADD COLUMN email_body TEXT",
            [],
        );
        Ok(())
```

with

```rust
        let _ = db.execute(
            "ALTER TABLE broker_responses ADD COLUMN email_body TEXT",
            [],
        );
        // Autopilot columns (NULL for existing rows; errors = already present).
        let _ = db.execute(
            "ALTER TABLE pending_tasks ADD COLUMN skipped_until DATETIME",
            [],
        );
        let _ = db.execute(
            "ALTER TABLE removal_requests ADD COLUMN reminder_sent_at DATETIME",
            [],
        );
        Ok(())
```

In `get_forms_with_status` replace

```rust
                    COALESCE(rr.pipeline_status, '') as pipeline_status
                FROM broker_responses br
```

with

```rust
                    COALESCE(rr.pipeline_status, '') as pipeline_status,
                    COALESCE(pt.task_type, '') as task_type
                FROM broker_responses br
```

replace

```rust
                    str_col(r, 6)?,
                    str_col(r, 7)?,
                ))
```

with

```rust
                    str_col(r, 6)?,
                    str_col(r, 7)?,
                    str_col(r, 8)?,
                ))
```

replace

```rust
            let (mut f, task_status, pipeline_status) = row.context("failed to scan form")?;
```

with

```rust
            let (mut f, task_status, pipeline_status, task_type) =
                row.context("failed to scan form")?;
```

and replace

```rust
                ("pending", _) if f.task_id > 0 => "captcha",
```

with

```rust
                // A pending manual_form to-do leaves the form "pending".
                ("pending", _) if f.task_id > 0 && task_type == "captcha" => "captcha",
```

- [ ] **Step 4: Run all history tests, expect pass.**

Run: `cargo test --lib history::`
Expected: `test result: ok.` (all history tests, including `reads_database_written_by_go`)

- [ ] **Step 5: Commit.**

```bash
git commit -m "History: id_required/unsubscribe_link tasks, skipped_until, reminder_sent_at" -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>" -- src/history.rs src/history/tests.rs
```

---

### Task 4: DB – rounds lock, reminders, to-dos, progress

**Files:**
- Create: `src/history/rounds.rs`
- Modify: `src/history.rs` (migrate ~line 481, module list ~line 988)
- Test: `src/history/tests.rs`

- [ ] **Step 1: Write the failing tests.** Append to `src/history/tests.rs`:

```rust
#[test]
fn autopilot_rounds_lock_and_stale() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().join("h.db")).unwrap();
    let id = store.begin_round(true).unwrap().expect("first round starts");
    assert!(store.round_running().unwrap());
    assert_eq!(store.begin_round(false).unwrap(), None, "one round at a time");
    store.finish_round(id, r#"{"dry_run":true}"#).unwrap();
    assert!(!store.round_running().unwrap());

    let id2 = store.begin_round(false).unwrap().unwrap();
    store
        .db()
        .execute(
            "UPDATE autopilot_rounds SET started_at = '2000-01-01T00:00:00Z' WHERE id = ?",
            [id2],
        )
        .unwrap();
    assert!(!store.round_running().unwrap(), "stale rounds do not count");
    assert!(store.begin_round(false).unwrap().is_some());

    let rounds = store.recent_rounds(5).unwrap();
    assert_eq!(rounds.len(), 3);
    assert_eq!(rounds[2].id, id);
    assert_eq!(rounds[2].report, r#"{"dry_run":true}"#);
    assert!(rounds[2].dry_run && rounds[2].finished_at.is_some());
    assert!(rounds[0].finished_at.is_none());
}

#[test]
fn reminders_todos_and_progress() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().join("h.db")).unwrap();
    let add = |id: &str, status: Status| {
        let mut r = Record {
            broker_id: id.into(),
            broker_name: id.into(),
            email: format!("p@{id}.test"),
            template: "gdpr".into(),
            status,
            sent_at: Some(Local::now()),
            ..Default::default()
        };
        store.add(&mut r).unwrap();
        r
    };
    let a = add("a", Status::Sent);
    add("b", Status::Sent);
    add("c", Status::Failed);
    add("company:shop.test", Status::Sent);
    let mut resp = BrokerResponse {
        broker_id: "b".into(),
        broker_name: "b".into(),
        response_type: "pending".into(),
        ..Default::default()
    };
    store.add_broker_response(&mut resp).unwrap();

    let ids: Vec<String> = store
        .reminder_candidates()
        .unwrap()
        .into_iter()
        .map(|r| r.broker_id)
        .collect();
    assert_eq!(ids, ["a", "company:shop.test"], "b replied, c failed");
    let hour_ago = Local::now() - chrono::TimeDelta::hours(1);
    assert_eq!(store.emails_sent_since(hour_ago).unwrap(), 4);
    store.mark_reminder_sent(a.id).unwrap();
    assert_eq!(store.emails_sent_since(hour_ago).unwrap(), 5, "reminders count");
    assert_eq!(store.reminder_candidates().unwrap().len(), 1, "one reminder per request");

    store
        .update_pipeline_status("b", PipelineStatus::Confirmed)
        .unwrap();
    assert_eq!(
        store.progress(false).unwrap(),
        Progress {
            contacted: 2,
            confirmed: 1,
            rejected: 0,
            no_response: 1,
            waiting: 0
        }
    );
    assert_eq!(store.progress(true).unwrap().contacted, 1);

    let mut link = PendingTask {
        broker_id: "a".into(),
        broker_name: "A".into(),
        task_type: TaskType::Confirm,
        form_url: "https://a.test/c".into(),
        ..Default::default()
    };
    store.add_pending_task(&mut link).unwrap();
    let mut review = PendingTask {
        broker_id: "a".into(),
        broker_name: "A".into(),
        task_type: TaskType::Review,
        ..Default::default()
    };
    store.add_pending_task(&mut review).unwrap();
    assert_eq!(
        store
            .find_task_by_url(TaskType::Confirm, "https://a.test/c")
            .unwrap()
            .unwrap()
            .id,
        link.id
    );
    assert!(
        store
            .find_task_by_url(TaskType::UnsubscribeLink, "https://a.test/c")
            .unwrap()
            .is_none()
    );
    let now = Local::now();
    assert_eq!(store.todo_tasks(now).unwrap().len(), 1, "review items are not to-dos");
    store
        .skip_task_until(link.id, now + chrono::TimeDelta::days(30))
        .unwrap();
    assert!(store.todo_tasks(now).unwrap().is_empty());
    assert_eq!(
        store
            .todo_tasks(now + chrono::TimeDelta::days(31))
            .unwrap()
            .len(),
        1
    );
}
```

- [ ] **Step 2: Run them, expect a compile failure.**

Run: `cargo test --lib history::tests::autopilot_rounds_lock_and_stale`
Expected: `error[E0599]: no method named `begin_round` found for struct `Store``

- [ ] **Step 3: Implement.** Create `src/history/rounds.rs`:

```rust
//! Autopilot bookkeeping: the round log (which doubles as the cross-process lock), reminder
//! tracking, to-do queries and progress counts. Round timestamps are RFC 3339 UTC (seconds), so
//! they compare correctly as strings.

use super::{
    PendingTask, RECORD_COLUMNS, Record, Store, TASK_COLUMNS, TaskType, format_time, now_str,
    scan_record, scan_task, sum_col,
};
use anyhow::{Context, Result};
use chrono::{DateTime, Local, SecondsFormat, TimeDelta, Utc};
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use serde::Serialize;

pub(super) const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS autopilot_rounds (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    started_at TEXT NOT NULL,
    finished_at TEXT,
    dry_run INTEGER NOT NULL DEFAULT 0,
    report TEXT NOT NULL DEFAULT ''
);
"#;

/// An unfinished round older than this was interrupted (container killed) and no longer blocks.
pub const STALE_ROUND: TimeDelta = TimeDelta::hours(2);

/// Task types shown as to-dos on the Today page.
pub const TODO_TYPES: &[TaskType] = &[
    TaskType::ManualForm,
    TaskType::Captcha,
    TaskType::Confirm,
    TaskType::IdRequired,
    TaskType::UnsubscribeLink,
];

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AutopilotRound {
    pub id: i64,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub dry_run: bool,
    /// The round report as JSON ("" while running).
    pub report: String,
}

/// Latest sent request per broker, by outcome (for the Today progress bar).
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct Progress {
    pub contacted: i64,
    pub confirmed: i64,
    pub rejected: i64,
    /// Reminder sent and still no reply.
    pub no_response: i64,
    pub waiting: i64,
}

fn ts(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn parse(s: Option<String>) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(&s?)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

const RUNNING: &str =
    "SELECT EXISTS(SELECT 1 FROM autopilot_rounds WHERE finished_at IS NULL AND started_at > ?)";

impl Store {
    /// Starts a round unless another (non-stale) one is running; returns its id.
    pub fn begin_round(&self, dry_run: bool) -> Result<Option<i64>> {
        let mut db = self.db();
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = Utc::now();
        let busy: bool = tx.query_row(RUNNING, [ts(now - STALE_ROUND)], |r| r.get(0))?;
        if busy {
            return Ok(None);
        }
        tx.execute(
            "INSERT INTO autopilot_rounds (started_at, dry_run) VALUES (?, ?)",
            params![ts(now), dry_run as i64],
        )?;
        let id = tx.last_insert_rowid();
        tx.commit().context("failed to start autopilot round")?;
        Ok(Some(id))
    }

    pub fn finish_round(&self, id: i64, report_json: &str) -> Result<()> {
        self.db()
            .execute(
                "UPDATE autopilot_rounds SET finished_at = ?, report = ? WHERE id = ?",
                params![ts(Utc::now()), report_json, id],
            )
            .context("failed to finish autopilot round")?;
        Ok(())
    }

    pub fn round_running(&self) -> Result<bool> {
        self.db()
            .query_row(RUNNING, [ts(Utc::now() - STALE_ROUND)], |r| r.get(0))
            .context("failed to query autopilot rounds")
    }

    /// Newest first.
    pub fn recent_rounds(&self, limit: i64) -> Result<Vec<AutopilotRound>> {
        let db = self.db();
        let mut stmt = db.prepare(
            "SELECT id, started_at, finished_at, dry_run, report FROM autopilot_rounds
             ORDER BY id DESC LIMIT ?",
        )?;
        let rows = stmt.query_map([limit], |r| {
            Ok(AutopilotRound {
                id: r.get(0)?,
                started_at: parse(r.get(1)?),
                finished_at: parse(r.get(2)?),
                dry_run: r.get::<_, i64>(3)? != 0,
                report: r.get(4)?,
            })
        })?;
        rows.collect::<rusqlite::Result<_>>()
            .context("failed to read autopilot rounds")
    }

    /// Latest request per broker that was sent, got no reply at all, has no reminder yet and is
    /// still open. The caller filters by age.
    pub fn reminder_candidates(&self) -> Result<Vec<Record>> {
        let sql = format!(
            "SELECT {RECORD_COLUMNS} FROM removal_requests r
             WHERE r.id IN (SELECT MAX(id) FROM removal_requests GROUP BY broker_id)
               AND r.status = 'sent' AND r.reminder_sent_at IS NULL
               AND COALESCE(r.pipeline_status, 'email_sent') IN ('email_sent', 'awaiting_response')
               AND NOT EXISTS (SELECT 1 FROM broker_responses b WHERE b.broker_id = r.broker_id)
             ORDER BY r.id"
        );
        let db = self.db();
        let mut stmt = db.prepare(&sql)?;
        let rows = stmt.query_map([], scan_record)?;
        rows.collect::<rusqlite::Result<_>>()
            .context("failed to query reminder candidates")
    }

    pub fn mark_reminder_sent(&self, record_id: i64) -> Result<()> {
        self.db()
            .execute(
                "UPDATE removal_requests SET reminder_sent_at = ? WHERE id = ?",
                params![now_str(), record_id],
            )
            .context("failed to mark reminder sent")?;
        Ok(())
    }

    /// Emails sent since `since`: request attempts (any status) plus reminders.
    pub fn emails_sent_since(&self, since: DateTime<Local>) -> Result<i64> {
        self.db()
            .query_row(
                "SELECT (SELECT COUNT(*) FROM removal_requests WHERE sent_at >= ?1)
                      + (SELECT COUNT(*) FROM removal_requests WHERE reminder_sent_at >= ?1)",
                [format_time(&since)],
                |r| r.get(0),
            )
            .context("failed to count sent emails")
    }

    /// Any task (any status) of this type for this URL: links are handled at most once.
    pub fn find_task_by_url(&self, task_type: TaskType, url: &str) -> Result<Option<PendingTask>> {
        let sql = format!(
            "SELECT {TASK_COLUMNS} FROM pending_tasks WHERE task_type = ? AND form_url = ? LIMIT 1"
        );
        self.db()
            .query_row(&sql, params![task_type, url], scan_task)
            .optional()
            .context("failed to query task")
    }

    /// Hides a to-do until `until` (it stays pending).
    pub fn skip_task_until(&self, id: i64, until: DateTime<Local>) -> Result<()> {
        self.db()
            .execute(
                "UPDATE pending_tasks SET skipped_until = ? WHERE id = ?",
                params![format_time(&until), id],
            )
            .context("failed to skip task")?;
        Ok(())
    }

    /// Pending to-dos not skipped at `now`, oldest (nearest deadline) first.
    pub fn todo_tasks(&self, now: DateTime<Local>) -> Result<Vec<PendingTask>> {
        let mut tasks: Vec<PendingTask> = self
            .get_pending_tasks(None, Some("pending"))?
            .into_iter()
            .filter(|t| TODO_TYPES.contains(&t.task_type))
            .filter(|t| t.skipped_until.is_none_or(|u| u <= now))
            .collect();
        tasks.sort_by_key(|t| (t.created_at, t.id));
        Ok(tasks)
    }

    /// Outcome counts for brokers (`companies = false`) or inbox companies (`company:` ids).
    pub fn progress(&self, companies: bool) -> Result<Progress> {
        let (contacted, confirmed, rejected, no_response) = self
            .db()
            .query_row(
                "SELECT COUNT(*),
                    SUM(CASE WHEN pipeline_status = 'confirmed' THEN 1 ELSE 0 END),
                    SUM(CASE WHEN pipeline_status = 'rejected' THEN 1 ELSE 0 END),
                    SUM(CASE WHEN reminder_sent_at IS NOT NULL
                              AND COALESCE(pipeline_status, '') NOT IN ('confirmed', 'rejected')
                         THEN 1 ELSE 0 END)
                 FROM removal_requests
                 WHERE id IN (SELECT MAX(id) FROM removal_requests WHERE status = 'sent' GROUP BY broker_id)
                   AND (broker_id LIKE 'company:%') = ?",
                [companies],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        sum_col(r, 1)?,
                        sum_col(r, 2)?,
                        sum_col(r, 3)?,
                    ))
                },
            )
            .context("failed to count progress")?;
        Ok(Progress {
            contacted,
            confirmed,
            rejected,
            no_response,
            waiting: (contacted - confirmed - rejected - no_response).max(0),
        })
    }
}
```

In `src/history.rs` `migrate`, replace

```rust
        db.execute_batch(companies::SCHEMA)
            .context("failed to migrate company tables")?;
```

with

```rust
        db.execute_batch(companies::SCHEMA)
            .context("failed to migrate company tables")?;
        db.execute_batch(rounds::SCHEMA)
            .context("failed to migrate autopilot table")?;
```

and replace

```rust
mod companies;
pub use companies::{Company, CompanyAction, CompanyAggregate, ScannedMessage};
```

with

```rust
mod companies;
mod rounds;
pub use companies::{Company, CompanyAction, CompanyAggregate, ScannedMessage};
pub use rounds::{AutopilotRound, Progress, STALE_ROUND, TODO_TYPES};
```

- [ ] **Step 4: Run all history tests, expect pass.**

Run: `cargo test --lib history::`
Expected: `test result: ok.` including `autopilot_rounds_lock_and_stale` and `reminders_todos_and_progress`

- [ ] **Step 5: Commit.**

```bash
git add src/history/rounds.rs
git commit -m "History: autopilot_rounds lock/log, reminder, to-do and progress queries" -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>" -- src/history.rs src/history/rounds.rs src/history/tests.rs
```

---

### Task 5: Reminder email template

**Files:**
- Create: `templates/email/reminder.txt`
- Modify: `src/template.rs`
- Test: `src/template.rs` tests module

- [ ] **Step 1: Write the failing test.** Append inside `mod tests` in `src/template.rs`:

```rust
    #[test]
    fn reminder_cites_original_date_deadline_and_law() {
        let engine = Engine::new().unwrap();
        let now = Utc.with_ymd_and_hms(2026, 2, 10, 12, 0, 0).unwrap();
        let original = Local.with_ymd_and_hms(2026, 1, 5, 12, 0, 0).unwrap();
        for (region, law, cite) in [
            ("eu", "gdpr", "Article 12(3) GDPR you must act"),
            ("ch", "fadp", "Art. 18 para. 1 of the Data Protection Ordinance"),
            ("us", "generic", "one month under Article 12(3) GDPR"),
        ] {
            let b = Broker {
                region: region.into(),
                ..broker()
            };
            let r = engine
                .render_reminder_at("auto", &full_profile(), &b, original, 36, now)
                .unwrap();
            assert_eq!(r.template, law);
            assert_eq!(r.subject, "Reminder: data deletion request of January 5, 2026");
            assert!(r.subject.is_ascii());
            assert!(r.body.contains("On January 5, 2026 I sent you"), "{}", r.body);
            assert!(r.body.contains("36 days have passed"), "{}", r.body);
            assert!(r.body.contains(cite), "{law}: {}", r.body);
            assert!(r.body.contains("Full Name: Jane Döe"));
            assert!(r.body.ends_with("February 10, 2026\n"), "{:?}", r.body);
        }
        assert_eq!(
            engine
                .render_reminder_at("ccpa", &full_profile(), &broker(), original, 36, now)
                .unwrap()
                .template,
            "ccpa"
        );
    }
```

- [ ] **Step 2: Run it, expect a compile failure.**

Run: `cargo test --lib template::tests::reminder_cites_original_date_deadline_and_law`
Expected: `error[E0599]: no method named `render_reminder_at` found for struct `Engine``

- [ ] **Step 3: Create the template.** Create `templates/email/reminder.txt` (the file must end with exactly one newline after `{{ Date }}`):

```text
To the Data Protection Officer / Controller, {{ BrokerName }}

Reminder: my request of {{ OriginalDate }} to delete my personal data

On {{ OriginalDate }} I sent you a request to delete my personal data and an objection to its processing. {{ DaysSince }} days have passed and I have not received a reply.

{% if Template == "gdpr" -%}
Under Article 12(3) GDPR you must act on such a request without undue delay and in any event within one month of receipt. That period may be extended by two further months only if you inform me of the extension and the reasons within the first month. I have received no such notice.
{%- elif Template == "fadp" -%}
Under Art. 25 FADP and Art. 18 para. 1 of the Data Protection Ordinance (DPO, SR 235.11), the information I requested must be provided within 30 days of receipt, and my data must not be processed further against my express objection (Art. 30 para. 2 lit. b FADP). Any restriction or deferral must be justified to me (Art. 26 para. 4 FADP).
{%- elif Template == "ccpa" -%}
Under the California Consumer Privacy Act (Cal. Civ. Code § 1798.130(a)(2)) you must respond to a request to delete within 45 days of receipt.
{%- else -%}
Data protection laws set fixed response periods for such requests, for example one month under Article 12(3) GDPR, 30 days under the Swiss FADP (Art. 18 para. 1 DPO) and 45 days under the CCPA.
{%- endif %}

Please carry out the deletion and confirm it to me in writing. If you are not going to act on my request, please tell me the reasons.

{% if Template == "gdpr" -%}
If I do not receive a reply, I may lodge a complaint with a supervisory authority (Article 77 GDPR).
{%- elif Template == "fadp" -%}
If I do not receive a reply, I may report the matter to the Federal Data Protection and Information Commissioner (FDPIC / EDÖB).
{%- else -%}
If I do not receive a reply, I may contact the competent data protection authority.
{%- endif %}

To identify my records:
- Full Name: {{ FullName }}
- Email Address: {{ Email }}

Please send your reply to {{ Email }}.

Yours faithfully,
{{ FullName }}
{{ Date }}
```

- [ ] **Step 4: Implement the engine support.** In `src/template.rs` replace

```rust
/// All data available to email templates (field names match the Go struct).
```

with

```rust
const REMINDER: &str = include_str!("../templates/email/reminder.txt");

/// All data available to email templates (field names match the Go struct).
```

In `Engine::new` replace

```rust
        for (name, src) in TEMPLATES {
            env.add_template(name, src)
                .with_context(|| format!("failed to parse template {name}"))?;
        }
        Ok(Self { env })
```

with

```rust
        for (name, src) in TEMPLATES {
            env.add_template(name, src)
                .with_context(|| format!("failed to parse template {name}"))?;
        }
        env.add_template("reminder", REMINDER)
            .context("failed to parse template reminder")?;
        Ok(Self { env })
```

Replace the body of `render_at` from `let template_name = for_broker(template_name, broker);` through the closing `Ok(Email { ... })` with:

```rust
        let template_name = for_broker(template_name, broker);
        let Ok(tmpl) = self.env.get_template(template_name) else {
            bail!("unknown template: {template_name}");
        };
        let data = email_data(template_name, profile, broker, &now);
        let body = tmpl
            .render(minijinja::value::Serde(&data))
            .context("failed to render template")?;
        Ok(Email {
            subject: subject(template_name).to_string(),
            body,
            template: template_name.to_string(),
        })
    }

    /// The single reminder for a request sent on `original`, `days_since` days ago. The law
    /// follows `template_name` like [`Engine::render`] ("auto" picks it by broker region).
    pub fn render_reminder(
        &self,
        template_name: &str,
        profile: &Profile,
        broker: &Broker,
        original: DateTime<Local>,
        days_since: i64,
    ) -> Result<Email> {
        self.render_reminder_at(template_name, profile, broker, original, days_since, Local::now())
    }

    /// Same as [`Engine::render_reminder`] with an explicit "now".
    pub fn render_reminder_at<Tz: TimeZone>(
        &self,
        template_name: &str,
        profile: &Profile,
        broker: &Broker,
        original: DateTime<Local>,
        days_since: i64,
        now: DateTime<Tz>,
    ) -> Result<Email>
    where
        Tz::Offset: std::fmt::Display,
    {
        let law = for_broker(template_name, broker);
        let original = original.format("%B %-d, %Y").to_string();
        let data = serde_json::to_value(email_data(law, profile, broker, &now))?;
        let ctx = minijinja::context! {
            OriginalDate => original.clone(),
            DaysSince => days_since,
            ..minijinja::Value::from(minijinja::value::Serde(data))
        };
        let body = self
            .env
            .get_template("reminder")?
            .render(ctx)
            .context("failed to render reminder")?;
        Ok(Email {
            subject: format!("Reminder: data deletion request of {original}"),
            body,
            template: law.to_string(),
        })
```

(The method's closing `}` that followed the old `Ok(Email { ... })` now closes `render_reminder_at`.) Then add this free function right above `pub fn for_broker`:

```rust
fn email_data<Tz: TimeZone>(
    template_name: &str,
    profile: &Profile,
    broker: &Broker,
    now: &DateTime<Tz>,
) -> EmailData
where
    Tz::Offset: std::fmt::Display,
{
    EmailData {
        first_name: profile.first_name.clone(),
        last_name: profile.last_name.clone(),
        full_name: profile.full_name(),
        email: profile.email.clone(),
        address: profile.address.clone(),
        city: profile.city.clone(),
        state: profile.state.clone(),
        zip_code: profile.zip_code.clone(),
        country: profile.country.clone(),
        phone: profile.phone.clone(),
        date_of_birth: profile.date_of_birth.clone(),
        broker_name: broker.name.clone(),
        broker_email: broker.email.clone(),
        broker_website: broker.website.clone(),
        broker_opt_out: broker.opt_out_url.clone(),
        date: now.format("%B %-d, %Y").to_string(), // Go: "January 2, 2006"
        year: now.year(),
        month: now.format("%B").to_string(),
        template: template_name.to_string(),
    }
}

```

- [ ] **Step 5: Run all template tests, expect pass (golden output unchanged).**

Run: `cargo test --lib template::`
Expected: `test result: ok. 5 passed` (incl. `matches_go_output_byte_for_byte`)

- [ ] **Step 6: Commit.**

```bash
git add templates/email/reminder.txt
git commit -m "Template: reminder letter citing the original request date and deadline" -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>" -- templates/email/reminder.txt src/template.rs
```

---

### Task 6: Shared send/reply helpers

Moves helpers the round needs out of the private web module (no behaviour change).

**Files:**
- Modify: `src/email.rs`, `src/history.rs`, `src/inbox/mod.rs`, `src/web/handlers.rs`, `src/cli/inbox_cmds.rs`

- [ ] **Step 1: Write the failing test.** Append inside `mod tests` in `src/email.rs`:

```rust
    #[test]
    fn daily_limits() {
        assert_eq!(daily_limit("smtp"), 250);
        assert_eq!(daily_limit(""), 250);
        assert_eq!(daily_limit("sendgrid"), 500);
    }
```

- [ ] **Step 2: Run it, expect a compile failure.**

Run: `cargo test --lib email::tests::daily_limits`
Expected: `error[E0425]: cannot find function `daily_limit` in this scope`

- [ ] **Step 3: Implement `daily_limit`.** In `src/email.rs` add above `pub fn new_sender`:

```rust
/// Emails per day per provider (iCloud allows ~1000/day, Gmail ~500/day; stay well under both).
pub fn daily_limit(provider: &str) -> i64 {
    match provider {
        "sendgrid" | "resend" => 500,
        _ => 250,
    }
}

```

In `src/web/handlers.rs` delete

```rust
// Daily send limits by provider to avoid rate limiting
const DAILY_LIMIT_SMTP: i64 = 250; // iCloud allows ~1000/day, Gmail ~500/day; stay well under both
const DAILY_LIMIT_SENDGRID: i64 = 500;
const DAILY_LIMIT_RESEND: i64 = 500;

```

and replace

```rust
    let daily_limit = match cfg.email.provider.as_str() {
        "sendgrid" => DAILY_LIMIT_SENDGRID,
        "resend" => DAILY_LIMIT_RESEND,
        _ => DAILY_LIMIT_SMTP,
    };
```

with

```rust
    let daily_limit = crate::email::daily_limit(&cfg.email.provider);
```

- [ ] **Step 4: Move `record_for` to `Record::for_send`.** In `src/history.rs` replace

```rust
            pipeline_status: None,
        }
    }
}
```

(the end of `impl Default for Record`) with

```rust
            pipeline_status: None,
        }
    }
}

impl Record {
    /// The history row for one send attempt to `broker`.
    pub fn for_send(
        broker: &crate::broker::Broker,
        template: &str,
        result: &crate::email::SendResult,
    ) -> Self {
        let mut r = Record {
            broker_id: broker.id.clone(),
            broker_name: broker.name.clone(),
            email: broker.email.clone(),
            template: template.into(),
            sent_at: Some(Local::now()),
            ..Default::default()
        };
        if result.success {
            r.status = Status::Sent;
            r.message_id = result.message_id.clone();
        } else {
            r.status = Status::Failed;
            r.error = result.error.clone().unwrap_or_default();
        }
        r
    }
}
```

In `src/web/handlers.rs` delete the whole `fn record_for(b: &Broker, template: &str, result: &SendResult) -> Record { ... }` (≈ line 1165–1182) and replace both call sites:

`.add(&mut record_for(&br, &rendered.template, &result));` → `.add(&mut Record::for_send(&br, &rendered.template, &result));`

`.add(&mut record_for(broker, &rendered.template, &result));` → `.add(&mut Record::for_send(broker, &rendered.template, &result));`

- [ ] **Step 5: Move the reply helpers into `inbox`.** In `src/inbox/mod.rs` replace

```rust
pub use monitor::Email;

use regex::Regex;
```

with

```rust
pub use monitor::Email;

use crate::history::{BrokerResponse, PipelineStatus};
use chrono::Local;
use classifier::{ClassifiedResponse, ResponseType};
use regex::Regex;

/// Plain-text body, else the HTML body.
pub fn body_of(email: &Email) -> String {
    if email.body.is_empty() {
        email.html_body.clone()
    } else {
        email.body.clone()
    }
}

/// The broker_responses row for a classified email (body included for reclassification).
pub fn response_record(email: &Email, c: &ClassifiedResponse) -> BrokerResponse {
    BrokerResponse {
        broker_id: email.broker_id.clone(),
        broker_name: email.broker_name.clone(),
        response_type: c.r#type.as_str().into(),
        email_from: email.from.clone(),
        email_subject: email.subject.clone(),
        email_body: body_of(email),
        form_url: c.form_url.clone(),
        confirm_url: c.confirm_url.clone(),
        confidence: c.confidence,
        needs_review: c.needs_review,
        received_at: Some(email.received_at.with_timezone(&Local)),
        ..Default::default()
    }
}

/// Pipeline stage a reply of this type moves the broker to.
pub fn pipeline_status_for(t: ResponseType) -> PipelineStatus {
    match t {
        ResponseType::Success => PipelineStatus::Confirmed,
        ResponseType::FormRequired => PipelineStatus::FormRequired,
        ResponseType::ConfirmationRequired => PipelineStatus::AwaitingConfirmation,
        ResponseType::Rejected => PipelineStatus::Rejected,
        _ => PipelineStatus::AwaitingResponse,
    }
}
```

In `src/web/handlers.rs`: delete `fn body_of(email: &Email) -> String { ... }` and `fn response_record( ... ) -> BrokerResponse { ... }` (≈ line 1693–1719); replace `use crate::history::{BrokerResponse, PipelineStatus, Record, Status, TaskType};` with `use crate::history::{PipelineStatus, Record, Status, TaskType};`; replace `use crate::inbox::Email;` with `use crate::inbox::{Email, body_of, response_record};`; delete the line `use chrono::Local;`.

In `src/cli/inbox_cmds.rs`: delete the whole `fn pipeline_status_for(t: ResponseType) -> PipelineStatus { ... }`, replace `use eraser::history::{BrokerResponse, PipelineStatus};` with `use eraser::history::BrokerResponse;` and replace `use eraser::inbox::Email;` with `use eraser::inbox::{Email, pipeline_status_for};`.

- [ ] **Step 6: Run the whole test suite, expect pass and no warnings.**

Run: `cargo test 2>&1 | grep -E "^warning|test result"`
Expected: only `test result: ok.` lines, no `warning:` lines

- [ ] **Step 7: Commit.**

```bash
git commit -m "Share daily limit, send record and reply helpers outside the web module" -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>" -- src/email.rs src/history.rs src/inbox/mod.rs src/web/handlers.rs src/cli/inbox_cmds.rs
```

---

### Task 7: Round runner – broker requests, reminders, daily limit, form to-dos, dry run, lock

**Files:**
- Modify: `src/autopilot/mod.rs`
- Create: `src/autopilot/tests.rs`

- [ ] **Step 1: Write the failing tests.** Create `src/autopilot/tests.rs`:

```rust
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
        let id = self.store.begin_round(dry_run).unwrap().expect("no round running");
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
            .set_company_contact(domain, "website", contact, &format!("https://{domain}/privacy"))
            .unwrap();
    }
}

#[test]
fn id_requests_and_next_round() {
    assert!(asks_for_id("Please send us a copy of your passport."));
    assert!(asks_for_id("Bitte senden Sie uns eine Kopie Ihres Ausweises."));
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
    let f = fixture(cfg(), vec![broker("a", ""), broker("b", ""), broker("old", "")]);
    sent_record(&f.store, "old", 40);
    let r = f.round(true, None).await;
    assert!(r.dry_run);
    assert_eq!((r.requests_sent, r.reminders_sent), (2, 1), "counts what would be sent");
    assert!(f.sent().is_empty(), "dry run sends nothing");
    assert_eq!(f.store.get_recent_requests(10).unwrap().len(), 1, "nothing recorded");
    assert_eq!(f.store.reminder_candidates().unwrap().len(), 1, "reminder still due");
    let stored: RoundReport =
        serde_json::from_str(&f.store.recent_rounds(1).unwrap()[0].report).unwrap();
    assert_eq!(stored, r);
    assert!(
        stored.summary().starts_with("Dry run: 2 broker requests, 1 reminders"),
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
    assert!(sent[0].subject.contains("GDPR"), "auto picks GDPR: {}", sent[0].subject);
    assert_eq!(
        f.store.get_last_request_for_broker("b0").unwrap().unwrap().status,
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
    assert!(sent[0].subject.starts_with("Reminder: data deletion request of "));
    assert!(sent[0].body.contains("Article 12(3) GDPR"), "{}", sent[0].body);
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
    assert_eq!(f.store.begin_round(true).unwrap(), None, "second round refused");
    run_round(&f.deps(), None, false, id).await;
    assert!(f.store.begin_round(true).unwrap().is_some(), "free again afterwards");
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
    assert_eq!(f.store.get_forms_with_status().unwrap()[0].status, "pending");
}
```

(`company` is used by Task 9's tests.) Append to `src/autopilot/mod.rs`:

```rust

#[cfg(test)]
mod tests;
```

- [ ] **Step 2: Run them, expect a compile failure.**

Run: `cargo test --lib autopilot::tests`
Expected: `error[E0412]: cannot find type `Deps` in this scope` (and similar for `RoundReport`, `run_round`)

- [ ] **Step 3: Implement.** Replace the whole content of `src/autopilot/mod.rs` with:

```rust
//! Autopilot: one unattended round. Steps (each switchable under `autopilot:` in config.yaml):
//! 1. new broker replies from the inbox, 2. confirmation links on the broker's own domain,
//! 3. one reminder after `reminder_days` plus new broker requests up to the daily limit,
//! 4. incremental inbox company scan, 5. RFC 8058 one-click unsubscribes,
//! 6. requests to inactive / newsletter-only companies with a verified contact.
//! Whatever needs a human becomes a to-do (pending_tasks). A dry run contacts no one (no email,
//! no link click, no unsubscribe, no IMAP move) but still records what it read.

pub mod keep;

use crate::broker::{Broker, BrokerDatabase};
use crate::config::Config;
use crate::email::{self, Message, SendResult, Sender};
use crate::history::{PendingTask, Record, Store, TaskType};
use crate::inbox::monitor::Monitor;
use crate::scanner::{self, discovery::Discoverer};
use crate::template::Engine;
use chrono::{DateTime, Datelike, Local, TimeDelta, TimeZone, Timelike, Utc};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Hours between hosted rounds (Worker cron `0 */6 * * *`, UTC).
pub const ROUND_HOURS: u32 = 6;
const SEND_TIMEOUT: Duration = Duration::from_secs(30);

/// Everything a round uses; the web server and the CLI fill it from their own state.
pub struct Deps<'a> {
    pub cfg: &'a Config,
    pub store: &'a Store,
    pub brokers: &'a BrokerDatabase,
    pub engine: &'a Engine,
    pub sender: &'a dyn Sender,
    /// No-cookie client for one-click unsubscribes.
    pub http: &'a reqwest::Client,
    pub discoverer: &'a Discoverer,
}

/// What a round did (in a dry run: what it would have done). Stored as JSON per round.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RoundReport {
    pub dry_run: bool,
    pub replies: usize,
    pub links_confirmed: usize,
    pub requests_sent: usize,
    pub reminders_sent: usize,
    pub new_messages: usize,
    pub contacts_found: usize,
    pub unsubscribed: usize,
    pub company_requests: usize,
    pub todos_added: usize,
    /// Companies skipped because of the keep list.
    pub kept: usize,
    pub daily_limit_reached: bool,
    pub auth_failed: bool,
    pub errors: Vec<String>,
}

impl RoundReport {
    /// One line for the Today activity list and the CLI.
    pub fn summary(&self) -> String {
        let parts: Vec<String> = [
            (self.replies, "new replies"),
            (self.links_confirmed, "links confirmed"),
            (self.requests_sent, "broker requests"),
            (self.reminders_sent, "reminders"),
            (self.new_messages, "new inbox messages"),
            (self.contacts_found, "privacy contacts found"),
            (self.unsubscribed, "unsubscribed"),
            (self.company_requests, "company requests"),
            (self.todos_added, "new to-dos"),
        ]
        .iter()
        .filter(|(n, _)| *n > 0)
        .map(|(n, what)| format!("{n} {what}"))
        .collect();
        let mut s = if parts.is_empty() {
            "nothing to do".to_string()
        } else {
            parts.join(", ")
        };
        if self.daily_limit_reached {
            s.push_str("; daily limit reached");
        }
        if self.auth_failed {
            s.push_str("; SMTP authentication failed");
        }
        if !self.errors.is_empty() {
            s.push_str(&format!("; {} errors", self.errors.len()));
        }
        if self.dry_run {
            format!("Dry run: {s}")
        } else {
            s
        }
    }
}

/// Next hosted round after `now` (cron `0 */6 * * *`, UTC).
pub fn next_round(now: DateTime<Utc>) -> DateTime<Utc> {
    let hour = now.hour() - now.hour() % ROUND_HOURS;
    let slot = Utc
        .with_ymd_and_hms(now.year(), now.month(), now.day(), hour, 0, 0)
        .unwrap();
    slot + TimeDelta::hours(ROUND_HOURS as i64)
}

const ID_PHRASES: &[&str] = &[
    "copy of your id",
    "copy of your passport",
    "proof of identity",
    "identity document",
    "government-issued",
    "government issued",
    "photo id",
    "id card",
    "passport",
    "ausweis",
    "identitätskarte",
    "identitätsnachweis",
    "pièce d'identité",
    "carta d'identità",
];

/// Whether a reply asks for an identity document (never sent automatically: it becomes a to-do).
pub fn asks_for_id(text: &str) -> bool {
    let t = text.to_lowercase();
    ID_PHRASES.iter().any(|p| t.contains(p))
}

/// The IMAP monitor for a round, or None when the inbox is not configured.
pub fn default_monitor(cfg: &Config, brokers: &BrokerDatabase, store: &Store) -> Option<Monitor> {
    cfg.validate_inbox().is_ok().then(|| {
        Monitor::new(
            cfg.inbox.clone(),
            &scanner::inbox_brokers(&brokers.brokers, store),
        )
    })
}

fn start_of_today() -> DateTime<Local> {
    let midnight = Local::now().date_naive().and_hms_opt(0, 0, 0).unwrap();
    Local
        .from_local_datetime(&midnight)
        .earliest()
        .unwrap_or_else(Local::now)
}

fn add_todo(store: &Store, r: &mut RoundReport, mut task: PendingTask) {
    match store.add_pending_task(&mut task) {
        Ok(()) => r.todos_added += 1,
        Err(e) => r.errors.push(format!("to-do: {e:#}")),
    }
}

async fn send(d: &Deps<'_>, msg: &Message) -> SendResult {
    match tokio::time::timeout(SEND_TIMEOUT, d.sender.send(msg, None)).await {
        Ok(res) => res,
        Err(_) => SendResult {
            success: false,
            message_id: String::new(),
            error: Some("send timed out".into()),
        },
    }
}

async fn pause(cfg: &Config) {
    tokio::time::sleep(Duration::from_millis(cfg.options.rate_limit_ms.max(0) as u64)).await;
}

/// Takes one email from today's budget; false when sending must stop for this round.
fn take_budget(budget: &mut i64, r: &mut RoundReport) -> bool {
    if r.auth_failed {
        return false;
    }
    if *budget <= 0 {
        r.daily_limit_reached = true;
        return false;
    }
    *budget -= 1;
    true
}

/// Logs a failed send; an authentication error stops all sending for this round.
fn record_failure(r: &mut RoundReport, what: &str, err: &str) {
    r.errors.push(format!("{what}: {err}"));
    if err.to_lowercase().contains("auth") {
        r.auth_failed = true;
    }
}

/// The broker DB entry, or the synthetic broker of an inbox company (`company:<domain>`).
fn find_broker(d: &Deps<'_>, id: &str) -> Option<Broker> {
    match id.strip_prefix(scanner::BROKER_PREFIX) {
        Some(domain) => d
            .store
            .get_company(domain)
            .ok()
            .flatten()
            .map(|c| scanner::company_broker(&c)),
        None => d.brokers.find_by_id(id).cloned(),
    }
}

/// Step 3a: one reminder for each request without any reply after `reminder_days`.
async fn reminders(d: &Deps<'_>, dry_run: bool, budget: &mut i64, r: &mut RoundReport) {
    let cutoff = Local::now() - TimeDelta::days(d.cfg.autopilot.reminder_days);
    let due = match d.store.reminder_candidates() {
        Ok(v) => v,
        Err(e) => {
            r.errors.push(format!("reminders: {e:#}"));
            return;
        }
    };
    for rec in due.iter().filter(|rec| rec.sent_at.is_some_and(|t| t <= cutoff)) {
        let Some(broker) = find_broker(d, &rec.broker_id) else {
            continue;
        };
        if !take_budget(budget, r) {
            return;
        }
        if dry_run {
            r.reminders_sent += 1;
            continue;
        }
        let original = rec.sent_at.unwrap_or_else(Local::now);
        let days_since = (Local::now() - original).num_days();
        let mail = match d.engine.render_reminder(
            &d.cfg.options.template,
            &d.cfg.profile,
            &broker,
            original,
            days_since,
        ) {
            Ok(m) => m,
            Err(e) => {
                r.errors.push(format!("reminder {}: {e:#}", rec.broker_id));
                continue;
            }
        };
        let msg = Message {
            to: rec.email.clone(),
            from: d.cfg.email.from.clone(),
            subject: mail.subject,
            body: mail.body,
        };
        let res = send(d, &msg).await;
        if res.success {
            r.reminders_sent += 1;
            if let Err(e) = d.store.mark_reminder_sent(rec.id) {
                r.errors.push(format!("reminder {}: {e:#}", rec.broker_id));
            }
        } else {
            let what = format!("reminder {}", rec.broker_id);
            record_failure(r, &what, res.error.as_deref().unwrap_or(""));
        }
        pause(d.cfg).await;
    }
}

/// Step 3b: requests to brokers never contacted (regions/exclusions from `options`).
async fn broker_requests(d: &Deps<'_>, dry_run: bool, budget: &mut i64, r: &mut RoundReport) {
    let contacted = match d.store.get_all_broker_statuses() {
        Ok(s) => s,
        Err(e) => {
            r.errors.push(format!("broker requests: {e:#}"));
            return;
        }
    };
    let pending: Vec<Broker> = d
        .brokers
        .filter(&d.cfg.options.regions, &d.cfg.options.excluded_brokers)
        .into_iter()
        .filter(|b| !b.email.is_empty() && !contacted.contains_key(&b.id))
        .collect();
    for b in &pending {
        if !take_budget(budget, r) {
            return;
        }
        if dry_run {
            r.requests_sent += 1;
            continue;
        }
        let rendered = match d.engine.render(&d.cfg.options.template, &d.cfg.profile, b) {
            Ok(x) => x,
            Err(e) => {
                r.errors.push(format!("{}: {e:#}", b.id));
                continue;
            }
        };
        let msg = Message {
            to: b.email.clone(),
            from: d.cfg.email.from.clone(),
            subject: rendered.subject,
            body: rendered.body,
        };
        let res = send(d, &msg).await;
        if let Err(e) = d.store.add(&mut Record::for_send(b, &rendered.template, &res)) {
            r.errors.push(format!("{}: {e:#}", b.id));
        }
        if res.success {
            r.requests_sent += 1;
        } else {
            record_failure(r, &b.id, res.error.as_deref().unwrap_or(""));
        }
        pause(d.cfg).await;
    }
}

/// Web-form replies without a to-do get one (forms are always filled by the user).
fn sync_form_todos(store: &Store, r: &mut RoundReport) {
    let forms = match store.get_forms_with_status() {
        Ok(f) => f,
        Err(e) => {
            r.errors.push(format!("forms: {e:#}"));
            return;
        }
    };
    for f in forms.into_iter().filter(|f| f.status == "pending" && f.task_id == 0) {
        add_todo(
            store,
            r,
            PendingTask {
                broker_id: f.broker_id,
                broker_name: f.broker_name,
                task_type: TaskType::ManualForm,
                form_url: f.form_url,
                notes: f.email_subject,
                ..Default::default()
            },
        );
    }
}

fn finish(store: &Store, id: i64, r: &RoundReport) {
    let json = serde_json::to_string(r).unwrap_or_default();
    if let Err(e) = store.finish_round(id, &json) {
        tracing::warn!("failed to store autopilot report: {e:#}");
    }
    tracing::info!("autopilot round {id}: {}", r.summary());
}

/// Runs one round and stores its report. The caller obtained `round_id` from
/// [`Store::begin_round`] (the lock); `monitor` is None when the inbox is not configured.
pub async fn run_round(
    d: &Deps<'_>,
    monitor: Option<Monitor>,
    dry_run: bool,
    round_id: i64,
) -> RoundReport {
    let mut r = RoundReport {
        dry_run,
        ..Default::default()
    };
    let steps = &d.cfg.autopilot;
    let sent_today = d.store.emails_sent_since(start_of_today()).unwrap_or(0);
    let mut budget = email::daily_limit(&d.cfg.email.provider) - sent_today;

    let mut monitor = monitor;
    let connected = match monitor.as_mut() {
        Some(m) if steps.check_inbox || steps.scan_inbox => match m.connect().await {
            Ok(()) => true,
            Err(e) => {
                r.errors.push(format!("inbox: {e:#}"));
                false
            }
        },
        _ => false,
    };

    if steps.broker_requests {
        reminders(d, dry_run, &mut budget, &mut r).await;
        broker_requests(d, dry_run, &mut budget, &mut r).await;
    }
    sync_form_todos(d.store, &mut r);

    if connected && let Some(m) = monitor.as_mut() {
        let _ = m.disconnect().await;
    }
    finish(d.store, round_id, &r);
    r
}

#[cfg(test)]
mod tests;
```

- [ ] **Step 4: Run the autopilot tests, expect pass.**

Run: `cargo test --lib autopilot::`
Expected: `test result: ok. 7 passed` (6 round tests + `keep_rules`)

- [ ] **Step 5: Commit.**

```bash
git add src/autopilot/tests.rs
git commit -m "Autopilot: round runner with broker requests, one reminder, daily limit, dry run" -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>" -- src/autopilot/mod.rs src/autopilot/tests.rs
```

---

### Task 8: Round runner – confirmation links (broker domain only, once per link)

**Files:**
- Modify: `src/autopilot/mod.rs`, `src/autopilot/tests.rs`

- [ ] **Step 1: Write the failing test.** In `src/autopilot/tests.rs` replace `use std::sync::Arc;` with `use std::sync::{Arc, Mutex};` and append:

```rust
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
    assert_eq!(dry.links_confirmed, 2, "would open both broker-domain links");
    assert_eq!(dry.todos_added, 1, "foreign domain becomes a to-do");
    assert!(hits.lock().unwrap().is_empty(), "dry run opens nothing");

    let r = f.round(false, None).await;
    assert_eq!((r.links_confirmed, r.todos_added), (1, 1), "captcha becomes a to-do");
    let mut opened = hits.lock().unwrap().clone();
    opened.sort();
    assert_eq!(opened, ["/captcha", "/ok"]);
    let todos = f.store.todo_tasks(Local::now()).unwrap();
    assert_eq!(todos.len(), 2);
    assert!(todos.iter().any(|t| t.form_url == "https://evil.test/confirm"));
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
```

- [ ] **Step 2: Run it, expect failure.**

Run: `cargo test --lib autopilot::tests::confirm_links_only_on_broker_domain_and_once`
Expected: FAIL at `assert_eq!(dry.links_confirmed, 2, ...)` with `left: 0, right: 2`

- [ ] **Step 3: Implement.** In `src/autopilot/mod.rs` replace the `use` block with:

```rust
use crate::broker::{Broker, BrokerDatabase};
use crate::browser::{ConfirmationHandler, detect_captcha_from_html, extract_confirmation_status};
use crate::config::Config;
use crate::email::{self, Message, SendResult, Sender};
use crate::history::{PendingTask, PipelineStatus, Record, Store, TaskType};
use crate::inbox::classifier::ResponseType;
use crate::inbox::monitor::Monitor;
use crate::scanner::{self, discovery::Discoverer, domain};
use crate::template::Engine;
use chrono::{DateTime, Datelike, Local, TimeDelta, TimeZone, Timelike, Utc};
use serde::{Deserialize, Serialize};
use std::time::Duration;
```

Add above `fn sync_form_todos`:

```rust
/// Allowed hosts for a broker's confirmation links: the registrable domains of its website and
/// of its privacy address.
fn broker_domains(b: &Broker) -> Vec<String> {
    let mut v = Vec::new();
    if let Ok(u) = url::Url::parse(&b.website)
        && let Some(host) = u.host_str()
    {
        v.push(domain::registrable_domain(host));
    }
    if let Some((_, host)) = b.email.rsplit_once('@') {
        v.push(domain::registrable_domain(host));
    }
    v.retain(|d| !d.is_empty());
    v
}

/// Step 2: confirmation links, only on the broker's own domain and at most once per link.
/// Captchas, foreign domains and failures become to-dos.
async fn confirm_links(d: &Deps<'_>, dry_run: bool, r: &mut RoundReport) {
    let responses = match d.store.get_broker_responses(
        Some(ResponseType::ConfirmationRequired.as_str()),
        false,
        1000,
    ) {
        Ok(v) => v,
        Err(e) => {
            r.errors.push(format!("confirm links: {e:#}"));
            return;
        }
    };
    for resp in responses.iter().filter(|x| !x.confirm_url.is_empty()) {
        let url = &resp.confirm_url;
        if matches!(d.store.find_task_by_url(TaskType::Confirm, url), Ok(Some(_))) {
            continue; // handled in an earlier round
        }
        let todo = |notes: String| PendingTask {
            broker_id: resp.broker_id.clone(),
            broker_name: resp.broker_name.clone(),
            task_type: TaskType::Confirm,
            form_url: url.clone(),
            notes,
            ..Default::default()
        };
        let allowed = find_broker(d, &resp.broker_id)
            .map(|b| broker_domains(&b))
            .unwrap_or_default();
        let handler = ConfirmationHandler::new(&allowed);
        match handler.validate_domain(url) {
            Ok((true, _)) => {}
            Ok((false, host)) => {
                let note = format!(
                    "Link is on {host}, not on the broker's own domain: open it only if you trust it."
                );
                add_todo(d.store, r, todo(note));
                continue;
            }
            Err(e) => {
                add_todo(d.store, r, todo(format!("Invalid link: {e:#}")));
                continue;
            }
        }
        if dry_run {
            r.links_confirmed += 1;
            continue;
        }
        let result = match handler.click_confirmation_link(url, false).await {
            Ok(x) => x,
            Err(e) => {
                add_todo(d.store, r, todo(format!("Opening the link failed: {e:#}")));
                continue;
            }
        };
        let captcha = detect_captcha_from_html(&result.response_body);
        if captcha.is_captcha_blocking() {
            let _ = d
                .store
                .update_pipeline_status(&resp.broker_id, PipelineStatus::AwaitingCaptcha);
            add_todo(d.store, r, todo(captcha.get_captcha_description()));
        } else if result.success {
            r.links_confirmed += 1;
            let _ = d
                .store
                .update_pipeline_status(&resp.broker_id, PipelineStatus::Confirmed);
            let mut log = todo(format!(
                "Confirmed automatically (HTTP {})",
                result.status_code
            ));
            if d.store.add_pending_task(&mut log).is_ok() {
                let _ = d.store.complete_pending_task(log.id, "completed");
            }
        } else {
            let note = format!(
                "{} (HTTP {}): please check the link yourself.",
                extract_confirmation_status(&result),
                result.status_code
            );
            add_todo(d.store, r, todo(note));
        }
    }
}

```

In `run_round` replace

```rust
    if steps.broker_requests {
        reminders(d, dry_run, &mut budget, &mut r).await;
```

with

```rust
    if steps.confirm_links {
        confirm_links(d, dry_run, &mut r).await;
    }
    if steps.broker_requests {
        reminders(d, dry_run, &mut budget, &mut r).await;
```

- [ ] **Step 4: Run the autopilot tests, expect pass.**

Run: `cargo test --lib autopilot::`
Expected: `test result: ok. 8 passed`

- [ ] **Step 5: Commit.**

```bash
git commit -m "Autopilot: confirmation links on the broker's own domain, once per link" -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>" -- src/autopilot/mod.rs src/autopilot/tests.rs
```

---

### Task 9: Round runner – company unsubscribes and requests (keep list, inactivity)

**Files:**
- Modify: `src/autopilot/mod.rs`, `src/autopilot/tests.rs`

- [ ] **Step 1: Write the failing tests.** Append to `src/autopilot/tests.rs`:

```rust
#[tokio::test]
async fn keep_list_blocks_requests() {
    let mut c = cfg();
    c.autopilot.keep_domains = vec!["club.test".into()];
    c.autopilot.broker_requests = false;
    let f = fixture(c, vec![]);
    company(&f.store, "ubs.com", "UBS", false, 400, "privacy@ubs.com");
    company(&f.store, "club.test", "Club", false, 400, "privacy@club.test");
    company(&f.store, "meinebank.test", "Meine Bank", false, 400, "dpo@meinebank.test");
    company(&f.store, "shop.test", "Shop", false, 400, "privacy@shop.test");

    let dry = f.round(true, None).await;
    assert_eq!((dry.kept, dry.company_requests), (3, 1));
    assert!(f.sent().is_empty(), "dry run sends nothing");
    assert!(f.store.get_company("shop.test").unwrap().unwrap().requested_at.is_none());

    let r = f.round(false, None).await;
    assert_eq!((r.kept, r.company_requests), (3, 1));
    let to: Vec<String> = f.sent().into_iter().map(|m| m.to).collect();
    assert_eq!(to, ["privacy@shop.test"], "only the company not on the keep list");
    assert!(f.store.get_company("shop.test").unwrap().unwrap().requested_at.is_some());
    assert_eq!(f.round(false, None).await.company_requests, 0, "requested once");
}

#[tokio::test]
async fn only_inactive_or_newsletter_only_companies_with_contact() {
    let mut c = cfg();
    c.autopilot.broker_requests = false;
    let f = fixture(c, vec![]);
    company(&f.store, "active.test", "Active", false, 10, "privacy@active.test");
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
    assert_eq!(f.round(false, None).await.todos_added, 0, "one to-do per link");
    let t = &f.store.todo_tasks(Local::now()).unwrap()[0];
    assert_eq!(
        (t.task_type, t.broker_id.as_str(), t.form_url.as_str()),
        (TaskType::UnsubscribeLink, "company:news.test", "https://news.test/unsub?u=1")
    );
    assert!(f.sent().is_empty());
    assert!(f.store.get_company("news.test").unwrap().unwrap().unsubscribed_at.is_none());
}
```

- [ ] **Step 2: Run them, expect failure.**

Run: `cargo test --lib autopilot::tests::keep_list_blocks_requests`
Expected: FAIL at `assert_eq!((dry.kept, dry.company_requests), (3, 1))` with `left: (0, 0)`

- [ ] **Step 3: Implement.** In `src/autopilot/mod.rs` replace the `use` block with:

```rust
use crate::broker::{Broker, BrokerDatabase};
use crate::browser::{ConfirmationHandler, detect_captcha_from_html, extract_confirmation_status};
use crate::config::Config;
use crate::email::{self, Message, SendResult, Sender};
use crate::history::{Company, PendingTask, PipelineStatus, Record, Store, TaskType};
use crate::inbox::classifier::ResponseType;
use crate::inbox::monitor::Monitor;
use crate::scanner::actions::{self, Unsubscribe};
use crate::scanner::{self, discovery::Discoverer, domain};
use crate::template::Engine;
use chrono::{DateTime, Datelike, Local, TimeDelta, TimeZone, Timelike, Utc};
use serde::{Deserialize, Serialize};
use std::time::Duration;
```

Add above `fn sync_form_todos`:

```rust
fn active(c: &Company) -> bool {
    c.excluded_reason.is_empty() && c.decision != "ignored"
}

/// Every scanned message from the domain carries mailing-list headers.
fn newsletter_only(store: &Store, domain: &str) -> bool {
    store
        .scanned_messages_for(domain)
        .is_ok_and(|m| !m.is_empty() && m.iter().all(|x| x.is_list))
}

/// Companies steps 5 and 6 may act on: shown (not excluded/ignored), not on the keep list
/// (counted in `kept`), and inactive for `inactive_months` or newsletter-only.
fn candidates(d: &Deps<'_>, r: &mut RoundReport) -> Vec<Company> {
    let cutoff = Utc::now() - TimeDelta::days(30 * d.cfg.autopilot.inactive_months);
    let companies = match d.store.list_companies() {
        Ok(c) => c,
        Err(e) => {
            r.errors.push(format!("companies: {e:#}"));
            return Vec::new();
        }
    };
    let mut out = Vec::new();
    for c in companies.into_iter().filter(active) {
        if keep::keep_reason(&c.agg.domain, &c.agg.display_name, d.cfg).is_some() {
            r.kept += 1;
            continue;
        }
        let inactive = c.agg.last_seen.is_some_and(|t| t < cutoff);
        if inactive || newsletter_only(d.store, &c.agg.domain) {
            out.push(c);
        }
    }
    out
}

/// Step 5: RFC 8058 one-click unsubscribes; mailto and plain links become to-dos.
async fn unsubscribe(d: &Deps<'_>, cands: &[Company], dry_run: bool, r: &mut RoundReport) {
    for c in cands
        .iter()
        .filter(|c| c.agg.is_mailing_list && c.unsubscribed_at.is_none())
    {
        match Unsubscribe::for_company(c) {
            Unsubscribe::OneClick(_) => {
                let res = actions::unsubscribe(d.store, d.http, d.sender, d.cfg, c, dry_run).await;
                if res.success {
                    r.unsubscribed += 1;
                } else {
                    let err = res.error.unwrap_or_default();
                    r.errors.push(format!("unsubscribe {}: {err}", c.agg.domain));
                }
            }
            Unsubscribe::Mailto(link) | Unsubscribe::Manual(link) => {
                if matches!(
                    d.store.find_task_by_url(TaskType::UnsubscribeLink, &link),
                    Ok(None)
                ) {
                    let b = scanner::company_broker(c);
                    add_todo(
                        d.store,
                        r,
                        PendingTask {
                            broker_id: b.id,
                            broker_name: b.name,
                            task_type: TaskType::UnsubscribeLink,
                            form_url: link,
                            notes: "Not a one-click list: open the link and unsubscribe yourself."
                                .into(),
                            ..Default::default()
                        },
                    );
                }
            }
            Unsubscribe::None => {}
        }
    }
}

/// Step 6: deletion requests to candidates with a verified privacy contact.
async fn company_requests(
    d: &Deps<'_>,
    cands: &[Company],
    dry_run: bool,
    budget: &mut i64,
    r: &mut RoundReport,
) {
    let verified = |c: &&Company| {
        c.requested_at.is_none()
            && matches!(c.contact_status.as_str(), "broker" | "website")
            && actions::request_target(c).is_ok()
    };
    for c in cands.iter().filter(verified) {
        if !take_budget(budget, r) {
            return;
        }
        let res = actions::send_request(d.store, d.engine, d.sender, d.cfg, c, dry_run).await;
        if res.success {
            r.company_requests += 1;
        } else {
            record_failure(r, &c.agg.domain, res.error.as_deref().unwrap_or(""));
        }
        if !dry_run {
            pause(d.cfg).await;
        }
    }
}

```

In `run_round` replace

```rust
    sync_form_todos(d.store, &mut r);

    if connected && let Some(m) = monitor.as_mut() {
```

with

```rust
    let cands = if steps.unsubscribe || steps.company_requests {
        candidates(d, &mut r)
    } else {
        Vec::new()
    };
    if steps.unsubscribe {
        unsubscribe(d, &cands, dry_run, &mut r).await;
    }
    if steps.company_requests {
        company_requests(d, &cands, dry_run, &mut budget, &mut r).await;
    }
    sync_form_todos(d.store, &mut r);

    if connected && let Some(m) = monitor.as_mut() {
```

- [ ] **Step 4: Run the autopilot tests, expect pass.**

Run: `cargo test --lib autopilot::`
Expected: `test result: ok. 11 passed`

- [ ] **Step 5: Commit.**

```bash
git commit -m "Autopilot: one-click unsubscribes and requests to inactive companies, keep list" -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>" -- src/autopilot/mod.rs src/autopilot/tests.rs
```

---

### Task 10: Round runner – inbox check and company scan (GreenMail end-to-end)

**Files:**
- Modify: `src/autopilot/mod.rs`, `src/autopilot/tests.rs`

- [ ] **Step 1: Write the failing test.** Append to `src/autopilot/tests.rs`:

```rust
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
    assert_eq!(r.new_messages, 3, "headers scanned from INBOX and the archive folder");
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
        f.store.get_company("acme.test").unwrap().unwrap().contact_status,
        "broker",
        "contact discovery for new companies"
    );

    let r = f.round(false, Some(monitor())).await;
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    assert_eq!(
        (r.replies, r.links_confirmed, r.requests_sent, r.new_messages),
        (0, 0, 0, 0),
        "second round: nothing new"
    );
    assert_eq!(hits.lock().unwrap().len(), 1);
}
```

- [ ] **Step 2: Run it against GreenMail, expect failure.** (Docker must be running.)

Run: `ERASER_IMAP_IT=1 cargo test --lib autopilot::tests::greenmail -- --nocapture`
Expected: FAIL at `assert_eq!(r.replies, 2)` with `left: 0`

- [ ] **Step 3: Implement.** In `src/autopilot/mod.rs` replace the `use` block with:

```rust
use crate::broker::{Broker, BrokerDatabase};
use crate::browser::{ConfirmationHandler, detect_captcha_from_html, extract_confirmation_status};
use crate::config::Config;
use crate::email::{self, Message, SendResult, Sender};
use crate::history::{Company, PendingTask, PipelineStatus, Record, Store, TaskType};
use crate::inbox::classifier::{ResponseType, classify_response};
use crate::inbox::monitor::Monitor;
use crate::scanner::actions::{self, Unsubscribe};
use crate::scanner::{self, Identity, discovery::Discoverer, domain};
use crate::template::Engine;
use chrono::{DateTime, Datelike, Local, TimeDelta, TimeZone, Timelike, Utc};
use serde::{Deserialize, Serialize};
use std::time::Duration;
```

Replace

```rust
/// Hours between hosted rounds (Worker cron `0 */6 * * *`, UTC).
pub const ROUND_HOURS: u32 = 6;
```

with

```rust
/// Hours between hosted rounds (Worker cron `0 */6 * * *`, UTC).
pub const ROUND_HOURS: u32 = 6;
/// Inbox look-back per round (a week covers missed rounds).
const INBOX_DAYS: i64 = 7;
/// Company scan window once a first scan exists.
const SCAN_DAYS: i64 = 7;
/// ponytail: contact discovery capped per round so a first scan cannot run for hours.
const MAX_DISCOVER: usize = 50;
```

Add above `fn active`:

```rust
/// Step 1: new broker replies from the inbox and archive folder (PEEK: nothing marked read).
async fn check_inbox(d: &Deps<'_>, m: &mut Monitor, dry_run: bool, r: &mut RoundReport) {
    let inbox = &d.cfg.inbox;
    let existing = match m.list_folders().await {
        Ok(f) => f,
        Err(e) => {
            r.errors.push(format!("inbox: {e:#}"));
            return;
        }
    };
    let mut archive_uids = Vec::new();
    for folder in [&inbox.folder, &inbox.archive_folder] {
        if !existing.iter().any(|f| f.eq_ignore_ascii_case(folder)) {
            continue;
        }
        let emails = match m.fetch_broker_emails_from_folder(folder, INBOX_DAYS).await {
            Ok(e) => e,
            Err(e) => {
                r.errors.push(format!("inbox {folder}: {e:#}"));
                continue;
            }
        };
        for email in emails {
            if matches!(
                d.store
                    .find_broker_response_by_subject(&email.broker_id, &email.subject),
                Ok(Some(_))
            ) {
                continue;
            }
            let c = classify_response(&email);
            let mut resp = crate::inbox::response_record(&email, &c);
            if let Err(e) = d.store.add_broker_response(&mut resp) {
                r.errors.push(format!("reply from {}: {e:#}", email.broker_id));
                continue;
            }
            r.replies += 1;
            let _ = d.store.update_pipeline_status(
                &email.broker_id,
                crate::inbox::pipeline_status_for(c.r#type),
            );
            if asks_for_id(&resp.email_body) {
                add_todo(
                    d.store,
                    r,
                    PendingTask {
                        broker_id: email.broker_id.clone(),
                        broker_name: email.broker_name.clone(),
                        task_type: TaskType::IdRequired,
                        notes: email.subject.clone(),
                        ..Default::default()
                    },
                );
            }
            if folder == &inbox.folder {
                archive_uids.push(email.uid);
            }
        }
    }
    if inbox.auto_archive && !dry_run && !archive_uids.is_empty() && !inbox.archive_folder.is_empty()
    {
        if let Err(e) = m.ensure_folder_exists(&inbox.archive_folder).await {
            r.errors.push(format!("archive: {e:#}"));
        } else if let Err(e) = m.archive_emails(&archive_uids, &inbox.archive_folder).await {
            r.errors.push(format!("archive: {e:#}"));
        }
    }
}

/// Step 4: incremental header-only company scan, then contact discovery for new companies.
async fn scan_companies(d: &Deps<'_>, m: &mut Monitor, r: &mut RoundReport) {
    let first = d.store.list_companies().map(|c| c.is_empty()).unwrap_or(true);
    let days = if first { scanner::DEFAULT_DAYS } else { SCAN_DAYS };
    let existing = match m.list_folders().await {
        Ok(f) => f,
        Err(e) => {
            r.errors.push(format!("scan: {e:#}"));
            return;
        }
    };
    let folders = scanner::pick_folders(&existing, &d.cfg.inbox, &[]);
    let id = Identity::from_config(d.cfg);
    match scanner::scan_folders(m, d.store, &id, &folders, days, |_, _, _| {}).await {
        Ok(w) => r.new_messages += w.new,
        Err(e) => {
            r.errors.push(format!("scan: {e:#}"));
            return;
        }
    }
    let pending: Vec<String> = d
        .store
        .list_companies()
        .unwrap_or_default()
        .into_iter()
        .filter(|c| active(c) && c.contact_status.is_empty())
        .filter(|c| keep::keep_reason(&c.agg.domain, &c.agg.display_name, d.cfg).is_none())
        .take(MAX_DISCOVER)
        .map(|c| c.agg.domain)
        .collect();
    for (_, contact) in d
        .discoverer
        .discover_and_store(d.store, &d.brokers.brokers, &pending)
        .await
    {
        if !contact.email.is_empty() {
            r.contacts_found += 1;
        }
    }
}

```

In `run_round` replace

```rust
    if steps.confirm_links {
        confirm_links(d, dry_run, &mut r).await;
    }
```

with

```rust
    if connected
        && steps.check_inbox
        && let Some(m) = monitor.as_mut()
    {
        check_inbox(d, m, dry_run, &mut r).await;
    }
    if steps.confirm_links {
        confirm_links(d, dry_run, &mut r).await;
    }
```

and replace

```rust
    let cands = if steps.unsubscribe || steps.company_requests {
```

with

```rust
    if connected
        && steps.scan_inbox
        && let Some(m) = monitor.as_mut()
    {
        scan_companies(d, m, &mut r).await;
    }
    let cands = if steps.unsubscribe || steps.company_requests {
```

- [ ] **Step 4: Run the unit tests and the GreenMail test, expect pass.**

Run: `cargo test --lib autopilot:: && ERASER_IMAP_IT=1 cargo test --lib autopilot::tests::greenmail -- --nocapture`
Expected: `test result: ok. 12 passed` then `test autopilot::tests::greenmail_round_end_to_end ... ok`

- [ ] **Step 5: Commit.**

```bash
git commit -m "Autopilot: inbox check and incremental company scan, GreenMail round test" -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>" -- src/autopilot/mod.rs src/autopilot/tests.rs
```

---

### Task 11: Internal tick endpoint `POST /internal/autopilot/tick`

**Files:**
- Create: `src/web/today.rs`
- Modify: `src/web/mod.rs`, `src/web/csrf.rs`, `src/web/tests.rs`, `tests/web.rs`

- [ ] **Step 1: Write the failing tests.** In `tests/web.rs` replace `use eraser::history::{Record, Status, Store};` with `use eraser::history::{PendingTask, Record, Status, Store, TaskType};`, add below `struct MockSender` + its impl:

```rust
const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef";

/// `configured()` plus SMTP host/port, so `Config::validate` passes (setup complete).
fn ready() -> Config {
    let mut c = configured();
    c.email.smtp.host = "smtp.example.com".into();
    c.email.smtp.port = 587;
    c
}

fn tick(token: Option<&str>) -> Request<Body> {
    let mut b = Request::builder()
        .method("POST")
        .uri("/internal/autopilot/tick")
        .header(header::HOST, "localhost:8080");
    if let Some(t) = token {
        b = b.header("X-Autopilot-Token", t);
    }
    b.body(Body::empty()).unwrap()
}
```

In `fn harness` replace `sender_factory: Arc::new(move |_| Ok(Box::new(m.clone()) as Box<dyn Sender>)),` with

```rust
        sender_factory: Arc::new(move |_| Ok(Box::new(m.clone()) as Box<dyn Sender>)),
        autopilot_token: Some(TOKEN.into()),
```

and append at the end of `tests/web.rs`:

```rust
#[tokio::test]
async fn autopilot_tick_needs_token_and_runs_one_round() {
    let mut cfg = ready();
    cfg.autopilot.enabled = true;
    let mut h = harness(Some(cfg), None);
    assert_eq!(h.call(tick(None)).await.status, StatusCode::UNAUTHORIZED);
    assert_eq!(h.call(tick(Some("wrong"))).await.status, StatusCode::UNAUTHORIZED);
    assert!(h.history.recent_rounds(1).unwrap().is_empty(), "no round without the token");

    // valid token: no CSRF token or Origin needed; a running round answers 409
    let busy = h.history.begin_round(false).unwrap().unwrap();
    assert_eq!(h.call(tick(Some(TOKEN))).await.status, StatusCode::CONFLICT);
    h.history.finish_round(busy, "{}").unwrap();

    let r = h.call(tick(Some(TOKEN))).await;
    assert_eq!(r.status, StatusCode::ACCEPTED, "{}", r.body);
    for _ in 0..250 {
        if !h.history.round_running().unwrap() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(h.history.recent_rounds(1).unwrap()[0].finished_at.is_some());
    assert_eq!(h.mail.0.lock().unwrap().len(), 3, "requests to the 3 brokers");
}

#[tokio::test]
async fn autopilot_tick_when_switched_off() {
    let mut h = harness(Some(ready()), None);
    let r = h.call(tick(Some(TOKEN))).await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.body.contains("disabled"), "{}", r.body);
    assert!(h.history.recent_rounds(1).unwrap().is_empty());
}
```

In `src/web/tests.rs` replace

```rust
        sender_factory: Arc::new(move |_| Ok(Box::new(mail.clone()) as Box<dyn Sender>)),
    })
```

with

```rust
        sender_factory: Arc::new(move |_| Ok(Box::new(mail.clone()) as Box<dyn Sender>)),
        autopilot_token: None,
    })
```

- [ ] **Step 2: Run them, expect a compile failure.**

Run: `cargo test --test web autopilot_tick`
Expected: `error[E0560]: struct `AppOptions` has no field named `autopilot_token``

- [ ] **Step 3: Implement.** In `src/web/csrf.rs` replace `fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {` with `pub(super) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {`.

Create `src/web/today.rs`:

```rust
//! Autopilot in the web UI: the internal tick endpoint (outside the CSRF layer, token only)
//! and starting rounds in the background.

use super::AppState;
use super::csrf::constant_time_eq;
use super::handlers::{St, http_error, json_resp};
use crate::autopilot::{self, Deps};
use crate::config::Config;
use crate::scanner::discovery::Discoverer;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::Ordering;

pub const TOKEN_HEADER: &str = "X-Autopilot-Token";

/// Starts a round in the background. `Ok(None)`: another round is running.
pub(super) fn start_round(
    st: &Arc<AppState>,
    cfg: Config,
    dry_run: bool,
) -> Result<Option<i64>, String> {
    let sender = (st.sender_factory)(&cfg.email).map_err(|e| format!("{e:#}"))?;
    let discoverer = Discoverer::new().map_err(|e| format!("{e:#}"))?;
    let Some(id) = st
        .history
        .begin_round(dry_run)
        .map_err(|e| format!("{e:#}"))?
    else {
        return Ok(None);
    };
    let st = st.clone();
    st.job_tasks.fetch_add(1, Ordering::SeqCst);
    tokio::spawn(async move {
        let monitor = autopilot::default_monitor(&cfg, &st.brokers, &st.history);
        let deps = Deps {
            cfg: &cfg,
            store: &st.history,
            brokers: &st.brokers,
            engine: &st.engine,
            sender: sender.as_ref(),
            http: &st.http,
            discoverer: &discoverer,
        };
        autopilot::run_round(&deps, monitor, dry_run, id).await;
        st.job_tasks.fetch_sub(1, Ordering::SeqCst);
    });
    Ok(Some(id))
}

/// POST /internal/autopilot/tick (Worker cron). 202 started, 200 switched off, 401 bad token,
/// 404 when the server has no token, 409 while a round runs.
pub async fn tick(State(st): St, headers: HeaderMap) -> Response {
    let Some(expected) = st.autopilot_token.as_deref() else {
        return http_error(StatusCode::NOT_FOUND, "404 page not found");
    };
    let given = headers
        .get(TOKEN_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !constant_time_eq(given.as_bytes(), expected.as_bytes()) {
        tracing::warn!("autopilot tick with a missing or wrong token");
        return http_error(StatusCode::UNAUTHORIZED, "invalid autopilot token");
    }
    let Some(cfg) = st.config() else {
        return json_resp(StatusCode::OK, json!({ "status": "not configured" }));
    };
    if !cfg.autopilot.enabled {
        return json_resp(StatusCode::OK, json!({ "status": "disabled" }));
    }
    match start_round(&st, cfg, false) {
        Ok(Some(id)) => json_resp(
            StatusCode::ACCEPTED,
            json!({ "status": "started", "round_id": id }),
        ),
        Ok(None) => json_resp(
            StatusCode::CONFLICT,
            json!({ "error": "an autopilot round is already running" }),
        ),
        Err(e) => json_resp(StatusCode::INTERNAL_SERVER_ERROR, json!({ "error": e })),
    }
}
```

In `src/web/mod.rs`:

replace `mod session;` with

```rust
mod session;
mod today;
```

in `struct AppOptions` replace

```rust
    pub sender_factory: SenderFactory,
}
```

(the first occurrence, inside `AppOptions`) with

```rust
    pub sender_factory: SenderFactory,
    /// Shared secret for POST /internal/autopilot/tick (None: endpoint answers 404).
    pub autopilot_token: Option<String>,
}
```

in `struct AppState` replace

```rust
    /// No-cookie client for one-click unsubscribes.
    http: reqwest::Client,
}
```

with

```rust
    /// No-cookie client for one-click unsubscribes.
    http: reqwest::Client,
    autopilot_token: Option<String>,
}
```

in `app()` replace `        http: crate::scanner::actions::http_client(),` with

```rust
        http: crate::scanner::actions::http_client(),
        autopilot_token: opts.autopilot_token,
```

replace

```rust
    use handlers as h;
```

with

```rust
    use handlers as h;
    // Outside the CSRF layer: the handler accepts only a valid X-Autopilot-Token.
    let internal = Router::new()
        .route("/internal/autopilot/tick", post(today::tick))
        .with_state(state.clone());
```

replace

```rust
        .layer(axum::middleware::from_fn_with_state(csrf, csrf::protect))
        .layer(axum::middleware::from_fn(security_headers))
```

with

```rust
        .layer(axum::middleware::from_fn_with_state(csrf, csrf::protect))
        .merge(internal)
        .layer(axum::middleware::from_fn(security_headers))
```

in `serve()` replace

```rust
        sender_factory: Arc::new(|cfg: &EmailConfig| email::new_sender(cfg)),
    })?;
```

with

```rust
        sender_factory: Arc::new(|cfg: &EmailConfig| email::new_sender(cfg)),
        autopilot_token: autopilot_token_from_env(),
    })?;
```

and add above `pub async fn serve`:

```rust
/// ERASER_AUTOPILOT_TOKEN (at least 32 characters) enables the internal tick endpoint.
fn autopilot_token_from_env() -> Option<String> {
    let token = std::env::var("ERASER_AUTOPILOT_TOKEN").ok()?.trim().to_string();
    if token.len() < 32 {
        if !token.is_empty() {
            tracing::warn!("ERASER_AUTOPILOT_TOKEN is shorter than 32 characters; tick disabled");
        }
        return None;
    }
    Some(token)
}

```

- [ ] **Step 4: Run the web tests, expect pass.**

Run: `cargo test --test web && cargo test --lib web::`
Expected: all `test result: ok.` (incl. `autopilot_tick_needs_token_and_runs_one_round`, `autopilot_tick_when_switched_off`, `csrf_rejections`)

- [ ] **Step 5: Commit.**

```bash
git add src/web/today.rs
git commit -m "Web: token-protected internal autopilot tick (202 background, 409 while running)" -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>" -- src/web/today.rs src/web/mod.rs src/web/csrf.rs src/web/tests.rs tests/web.rs
```

---

### Task 12: CLI `eraser autopilot [--dry-run]`

**Files:**
- Modify: `src/cli/args.rs`, `src/cli/commands.rs`
- Create: `tests/cli_autopilot.rs`

- [ ] **Step 1: Write the failing test.** Create `tests/cli_autopilot.rs`:

```rust
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
    assert!(store.get_recent_requests(10).unwrap().is_empty(), "nothing sent");
    assert_eq!(store.recent_rounds(5).unwrap().len(), 1);
}
```

- [ ] **Step 2: Run it, expect failure.**

Run: `cargo test --test cli_autopilot`
Expected: FAIL, stderr contains `Error: unknown command "autopilot" for "eraser"`

- [ ] **Step 3: Implement.** In `src/cli/args.rs` add above `#[derive(Parser)]` (the `Cli` struct attributes):

```rust
const AUTOPILOT_LONG: &str = "Run one autopilot round now: check the inbox for broker replies, open
confirmation links on the broker's own domain, send one reminder after the deadline and the next
broker requests (daily limit applies), scan the inbox for companies, one-click unsubscribe and
send requests to inactive or newsletter-only companies.

Steps follow the autopilot section of config.yaml. Unlike the hosted schedule, this command runs
even when the autopilot is switched off. Anything that needs you appears under Today in the web UI.";

```

In `enum Cmd`, insert before the `Help` variant's `#[command(` attribute:

```rust
    #[command(about = "Run one autopilot round", long_about = AUTOPILOT_LONG)]
    Autopilot {
        #[arg(long, action = ArgAction::Set, num_args = 0..=1, require_equals = true, default_missing_value = "true", default_value = "false", value_parser = BoolishValueParser::new(), help = "Show what would be done without contacting anyone")]
        dry_run: bool,
    },
```

In `src/cli/commands.rs` replace

```rust
        Cmd::Help { .. } => Ok(()),
```

with

```rust
        Cmd::Autopilot { dry_run } => run_autopilot(ctx, dry_run).await,
        Cmd::Help { .. } => Ok(()),
```

and add above `async fn run_serve(`:

```rust
async fn run_autopilot(ctx: &Ctx, dry_run: bool) -> Result<()> {
    let cfg = ctx.load_config()?;
    if !dry_run {
        cfg.validate().context("invalid config")?;
    }
    let db = ctx.load_brokers()?;
    let store = open_store("initialize")?;
    let sender = email::new_sender(&cfg.email).context("failed to initialize email sender")?;
    let engine = template::Engine::new().context("failed to initialize templates")?;
    let http = eraser::scanner::actions::http_client();
    let discoverer = eraser::scanner::discovery::Discoverer::new()?;
    let monitor = eraser::autopilot::default_monitor(&cfg, &db, &store);
    let Some(id) = store.begin_round(dry_run)? else {
        anyhow::bail!("another autopilot round is running");
    };
    println!(
        "🤖 Autopilot round{}...",
        if dry_run { " (dry run: nobody is contacted)" } else { "" }
    );
    let deps = eraser::autopilot::Deps {
        cfg: &cfg,
        store: &store,
        brokers: &db,
        engine: &engine,
        sender: sender.as_ref(),
        http: &http,
        discoverer: &discoverer,
    };
    let report = eraser::autopilot::run_round(&deps, monitor, dry_run, id).await;
    println!("✅ {}", report.summary());
    for e in &report.errors {
        println!("   ⚠️  {e}");
    }
    Ok(())
}

```

- [ ] **Step 4: Run it, expect pass; check the help text.**

Run: `cargo test --test cli_autopilot && cargo run -q -- autopilot --help | head -3`
Expected: `test autopilot_dry_run_sends_nothing ... ok`, then the first lines of `AUTOPILOT_LONG`

- [ ] **Step 5: Commit.**

```bash
git add tests/cli_autopilot.rs
git commit -m "CLI: eraser autopilot [--dry-run]" -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>" -- src/cli/args.rs src/cli/commands.rs tests/cli_autopilot.rs
```

---

### Task 13: Today start page (status, to-dos with Done/Skip, progress, activity)

**Files:**
- Create: `templates/web/today.html`
- Modify: `src/web/today.rs`, `src/web/mod.rs` (routes), `src/web/views.rs`, `src/web/handlers.rs` (`parse_id` visibility), `templates/web/layout.html`, `tests/web.rs`

- [ ] **Step 1: Write the failing tests.** Append to `tests/web.rs`:

```rust
#[tokio::test]
async fn today_page_todos_done_and_skip_with_csrf() {
    let mut h = harness(Some(ready()), None);
    let r = h.get("/").await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert!(r.body.contains("Nothing needs you right now"), "{}", r.body);
    assert!(r.body.contains(">Off<"));

    let mut id_task = PendingTask {
        broker_id: "acme".into(),
        broker_name: "Acme People Search".into(),
        task_type: TaskType::IdRequired,
        notes: "Identity check".into(),
        ..Default::default()
    };
    h.history.add_pending_task(&mut id_task).unwrap();
    let mut unsub = PendingTask {
        broker_id: "company:news.test".into(),
        broker_name: "News".into(),
        task_type: TaskType::UnsubscribeLink,
        form_url: "https://news.test/unsub".into(),
        ..Default::default()
    };
    h.history.add_pending_task(&mut unsub).unwrap();
    let page = h.get("/").await;
    assert!(page.body.contains("ID requested") && page.body.contains("Unsubscribe link"));
    assert!(page.body.contains("Show request") && page.body.contains("news.test"));
    let t = token_from(&page.body);

    // no CSRF token: refused, nothing changes
    let req = h
        .req("POST", &format!("/today/tasks/{}/done", id_task.id))
        .header(header::ORIGIN, h.origin)
        .body(Body::empty())
        .unwrap();
    assert_eq!(h.call(req).await.status, StatusCode::FORBIDDEN);
    assert_eq!(
        h.history.get_pending_task_by_id(id_task.id).unwrap().unwrap().status,
        "pending"
    );

    let r = h
        .post_form(&format!("/today/tasks/{}/done", id_task.id), &t, &[])
        .await;
    assert_eq!((r.status, location(&r)), (StatusCode::FOUND, "/"));
    assert_eq!(
        h.history.get_pending_task_by_id(id_task.id).unwrap().unwrap().status,
        "completed"
    );
    let r = h
        .post_form(&format!("/today/tasks/{}/skip", unsub.id), &t, &[])
        .await;
    assert_eq!(r.status, StatusCode::FOUND);
    assert!(h.get("/").await.body.contains("Nothing needs you right now"), "hidden 30 days");
    assert_eq!(
        h.history.get_pending_task_by_id(unsub.id).unwrap().unwrap().status,
        "pending"
    );
    assert!(h.get("/dashboard").await.body.contains("Welcome back, Ada"));
}

#[tokio::test]
async fn today_pause_resume_and_dry_run() {
    let mut h = harness(Some(configured()), None);
    let t = h.token("/").await;
    let r = h
        .post_form("/autopilot/enabled", &t, &[("enabled", "true")])
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "setup incomplete: stays off");

    let mut h = harness(Some(ready()), None);
    let t = h.token("/").await;
    let path = h.dir.path().join(".eraser/config.yaml");
    let r = h
        .post_form("/autopilot/enabled", &t, &[("enabled", "true")])
        .await;
    assert_eq!((r.status, location(&r)), (StatusCode::FOUND, "/"));
    assert!(config::load(&path).unwrap().autopilot.enabled);
    assert!(h.get("/").await.body.contains("Pause"));
    h.post_form("/autopilot/enabled", &t, &[("enabled", "false")])
        .await;
    assert!(!config::load(&path).unwrap().autopilot.enabled);

    let r = h.post_form("/autopilot/run", &t, &[("dry_run", "false")]).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "real run needs the switch on");
    let r = h.post_form("/autopilot/run", &t, &[("dry_run", "true")]).await;
    assert_eq!(r.status, StatusCode::FOUND);
    for _ in 0..250 {
        if !h.history.round_running().unwrap() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(h.mail.0.lock().unwrap().is_empty(), "dry run sends nothing");
    assert!(h.get("/").await.body.contains("Dry run: 3 broker requests"));
}
```

- [ ] **Step 2: Run them, expect failure.**

Run: `cargo test --test web today_`
Expected: FAIL — `GET /` still renders the old dashboard (assertion `Nothing needs you right now` fails)

- [ ] **Step 3: Create the template.** Create `templates/web/today.html`:

```html
{% extends "layout.html" %}
{% block content %}
<div class="fade-in-up space-y-6">
    <div class="flex flex-col md:flex-row md:items-center justify-between gap-4">
        <div>
            <h1 class="font-display text-3xl font-bold text-primary">Today</h1>
            <p class="text-secondary mt-1">Only what needs you, {{ profile.first_name }}. Eraser handles the rest.</p>
        </div>
        <a href="/dashboard" class="btn btn-ghost">Dashboard</a>
    </div>

    <section class="card p-6" aria-labelledby="status-h">
        <h2 id="status-h" class="font-display text-lg font-semibold text-primary mb-4">Status</h2>
        <div class="grid grid-cols-1 md:grid-cols-3 gap-4 text-sm">
            <div>
                <p class="text-muted">Autopilot</p>
                {% if enabled %}<span class="badge badge-success">On</span>{% else %}<span class="badge badge-neutral">Off</span>{% endif %}
                {% if running %}<span class="badge badge-neutral">Round running</span>{% endif %}
            </div>
            <div>
                <p class="text-muted">Last round</p>
                <p class="text-primary">{% if last_round %}{{ last_round }}{% else %}never{% endif %}</p>
            </div>
            <div>
                <p class="text-muted">Next round</p>
                <p class="text-primary">{% if not enabled %}paused{% elif next_round %}{{ next_round }}{% else %}not scheduled here (run <code>eraser autopilot</code>){% endif %}</p>
            </div>
        </div>
        <div class="flex flex-wrap gap-3 mt-5">
            <form method="post" action="/autopilot/enabled">
                {{ csrf_field }}
                {% if enabled %}
                <input type="hidden" name="enabled" value="false">
                <button type="submit" class="btn btn-secondary">Pause</button>
                {% else %}
                <input type="hidden" name="enabled" value="true">
                <button type="submit" class="btn btn-primary" {% if not setup_ok %}disabled title="Finish setup first"{% endif %}>Turn on</button>
                {% endif %}
            </form>
            <form method="post" action="/autopilot/run">
                {{ csrf_field }}
                <input type="hidden" name="dry_run" value="true">
                <button type="submit" class="btn btn-ghost" {% if running %}disabled{% endif %}>Dry run now</button>
            </form>
            {% if enabled %}
            <form method="post" action="/autopilot/run">
                {{ csrf_field }}
                <input type="hidden" name="dry_run" value="false">
                <button type="submit" class="btn btn-ghost" {% if running %}disabled{% endif %}>Run now</button>
            </form>
            {% endif %}
            <a href="/settings#autopilot" class="btn btn-ghost">Autopilot settings</a>
        </div>
    </section>

    <section class="card p-6" aria-labelledby="todo-h">
        <h2 id="todo-h" class="font-display text-lg font-semibold text-primary mb-4">To do <span class="text-muted text-sm font-normal">({{ todo_total }})</span></h2>
        {% if todos %}
        <ul class="divide-y divide-[var(--border-subtle)]">
            {% for it in todos %}
            <li class="py-3 flex flex-col md:flex-row md:items-center gap-3">
                <div class="flex-1 min-w-0">
                    <p class="text-primary font-medium">{{ it.name }} <span class="badge badge-neutral ml-2">{{ it.label }}</span></p>
                    {% if it.notes %}<p class="text-sm text-secondary break-words">{{ it.notes }}</p>{% endif %}
                    {% if it.due %}<p class="text-xs text-muted">Due {{ it.due }}</p>{% endif %}
                </div>
                <div class="flex flex-wrap gap-2 shrink-0">
                    <a href="{{ it.action_url|safe_url }}" class="btn btn-primary text-xs px-3 py-1.5" {% if it.external %}target="_blank" rel="noopener noreferrer"{% endif %}>{{ it.action_label }}</a>
                    <form method="post" action="/today/tasks/{{ it.id }}/done">
                        {{ csrf_field }}
                        <button type="submit" class="btn btn-secondary text-xs px-3 py-1.5">Done</button>
                    </form>
                    <form method="post" action="/today/tasks/{{ it.id }}/skip">
                        {{ csrf_field }}
                        <button type="submit" class="btn btn-ghost text-xs px-3 py-1.5" title="Hide for 30 days">Skip</button>
                    </form>
                </div>
            </li>
            {% endfor %}
        </ul>
        {% if not show_all %}<a href="/?all=1" class="text-sm underline mt-3 inline-block">Show all {{ todo_total }}</a>{% endif %}
        {% else %}
        <p class="text-muted">Nothing needs you right now.</p>
        {% endif %}
    </section>

    <section class="card p-6" aria-labelledby="progress-h">
        <h2 id="progress-h" class="font-display text-lg font-semibold text-primary mb-4">Progress</h2>
        <p class="text-sm text-secondary mb-2">Brokers: {{ brokers.p.contacted }} of {{ broker_total }} contacted · {{ brokers.p.confirmed }} confirmed deleted · {{ brokers.p.rejected }} rejected · {{ brokers.p.no_response }} no response · {{ brokers.p.waiting }} waiting</p>
        <div class="flex h-3 rounded-full overflow-hidden" style="background: var(--border-subtle);" role="img" aria-label="Broker progress">
            <div style="width: {{ brokers.confirmed_pct }}%; background: var(--success);"></div>
            <div style="width: {{ brokers.rejected_pct }}%; background: var(--error);"></div>
            <div style="width: {{ brokers.no_response_pct }}%; background: var(--warning);"></div>
            <div style="width: {{ brokers.waiting_pct }}%; background: var(--accent-muted);"></div>
        </div>
        <p class="text-sm text-secondary mt-5 mb-2">Inbox companies: {{ companies_requested }} of {{ companies_total }} requested · {{ companies_unsubscribed }} unsubscribed · {{ company_progress.p.confirmed }} confirmed deleted</p>
        <div class="flex h-3 rounded-full overflow-hidden" style="background: var(--border-subtle);" role="img" aria-label="Company progress">
            <div style="width: {{ company_progress.confirmed_pct }}%; background: var(--success);"></div>
            <div style="width: {{ company_progress.rejected_pct }}%; background: var(--error);"></div>
            <div style="width: {{ company_progress.no_response_pct }}%; background: var(--warning);"></div>
            <div style="width: {{ company_progress.waiting_pct }}%; background: var(--accent-muted);"></div>
        </div>
    </section>

    <section class="card p-6" aria-labelledby="activity-h">
        <h2 id="activity-h" class="font-display text-lg font-semibold text-primary mb-4">Activity</h2>
        {% if rounds %}
        <ul class="space-y-2">
            {% for r in rounds %}
            <li class="text-sm flex flex-col md:flex-row gap-1 md:gap-3">
                <span class="text-muted md:w-32 shrink-0">{{ r.started }}</span>
                <span class="text-primary break-words">{{ r.summary }}</span>
            </li>
            {% endfor %}
        </ul>
        {% else %}
        <p class="text-muted">No rounds yet.</p>
        {% endif %}
    </section>
</div>
{% endblock %}
```

- [ ] **Step 4: Implement the handlers.** In `src/web/handlers.rs` replace `fn parse_id(s: &str) -> i64 {` with `pub(super) fn parse_id(s: &str) -> i64 {`.

In `src/web/today.rs` replace the `use` block with:

```rust
use super::AppState;
use super::csrf::{CsrfToken, constant_time_eq};
use super::handlers::{
    FormValues, St, http_error, json_resp, needs_setup, parse_id, query, redirect, render,
};
use crate::autopilot::{self, Deps, RoundReport};
use crate::config::{self, Config};
use crate::history::{AutopilotRound, PendingTask, Progress, STALE_ROUND, TaskType};
use crate::scanner::{self, actions, discovery::Discoverer};
use axum::Extension;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::Response;
use chrono::{DateTime, Local, TimeDelta, Utc};
use serde::Serialize;
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::Ordering;
```

and append to `src/web/today.rs`:

```rust
const TODO_SHOWN: usize = 10;
const SKIP_DAYS: i64 = 30;

#[derive(Serialize)]
struct TodoItem {
    id: i64,
    label: &'static str,
    name: String,
    notes: String,
    action_url: String,
    action_label: &'static str,
    external: bool,
    due: String,
}

fn todo_item(t: &PendingTask, deadline_days: i64) -> TodoItem {
    let page = |suffix: &str| (format!("/tasks/{}{suffix}", t.id), false);
    let link = || {
        if t.form_url.is_empty() {
            page("")
        } else {
            (t.form_url.clone(), true)
        }
    };
    let (label, action_label, (action_url, external)) = match t.task_type {
        TaskType::ManualForm => ("Web form", "Open form", link()),
        TaskType::Captcha => ("Captcha", "Open helper", page("/helper")),
        TaskType::Confirm => ("Confirmation link", "Open link", link()),
        TaskType::IdRequired => ("ID requested", "Show request", page("")),
        TaskType::UnsubscribeLink => ("Unsubscribe link", "Open link", link()),
        TaskType::Review => ("Review", "Show", page("")),
    };
    TodoItem {
        id: t.id,
        label,
        name: t.broker_name.clone(),
        notes: t.notes.clone(),
        action_url,
        action_label,
        external,
        due: t
            .created_at
            .map(|c| (c + TimeDelta::days(deadline_days)).format("%b %-d, %Y").to_string())
            .unwrap_or_default(),
    }
}

#[derive(Serialize)]
struct RoundView {
    started: String,
    summary: String,
}

fn round_view(r: &AutopilotRound, now: DateTime<Utc>) -> RoundView {
    let started = r
        .started_at
        .map(|t| t.with_timezone(&Local).format("%b %-d, %H:%M").to_string())
        .unwrap_or_default();
    let summary = match (r.finished_at, r.started_at) {
        (Some(_), _) => serde_json::from_str::<RoundReport>(&r.report)
            .map(|x| x.summary())
            .unwrap_or_default(),
        (None, Some(s)) if now - s < STALE_ROUND => "running…".into(),
        _ => "interrupted (server restarted)".into(),
    };
    RoundView { started, summary }
}

fn bar(p: Progress) -> serde_json::Value {
    let pct = |n: i64| if p.contacted > 0 { n * 100 / p.contacted } else { 0 };
    json!({
        "p": p,
        "confirmed_pct": pct(p.confirmed),
        "rejected_pct": pct(p.rejected),
        "no_response_pct": pct(p.no_response),
        "waiting_pct": pct(p.waiting),
    })
}

/// GET / — the start page.
pub async fn today(
    State(st): St,
    Extension(t): Extension<CsrfToken>,
    method: Method,
    uri: Uri,
) -> Response {
    let cfg = st.config();
    if needs_setup(&cfg) {
        return redirect(&method, "/setup");
    }
    let cfg = cfg.unwrap();
    let all = query(&uri).get("all") == "1";
    let tasks = st.history.todo_tasks(Local::now()).unwrap_or_default();
    let shown = if all { tasks.len() } else { TODO_SHOWN };
    let todos: Vec<TodoItem> = tasks
        .iter()
        .take(shown)
        .map(|x| todo_item(x, cfg.autopilot.reminder_days))
        .collect();
    let now = Utc::now();
    let rounds: Vec<RoundView> = st
        .history
        .recent_rounds(5)
        .unwrap_or_default()
        .iter()
        .map(|r| round_view(r, now))
        .collect();
    let companies = st.history.list_companies().unwrap_or_default();
    let next_round = if st.autopilot_token.is_some() {
        autopilot::next_round(now)
            .with_timezone(&Local)
            .format("%b %-d, %H:%M")
            .to_string()
    } else {
        String::new()
    };
    render(
        &st,
        &t,
        "today.html",
        json!({
            "title": "Today",
            "profile": cfg.profile,
            "enabled": cfg.autopilot.enabled,
            "setup_ok": cfg.validate().is_ok(),
            "running": st.history.round_running().unwrap_or(false),
            "last_round": rounds.first().map(|r| r.started.clone()).unwrap_or_default(),
            "next_round": next_round,
            "todos": todos,
            "todo_total": tasks.len(),
            "show_all": all || tasks.len() <= TODO_SHOWN,
            "brokers": bar(st.history.progress(false).unwrap_or_default()),
            "broker_total": st.brokers.brokers.len(),
            "company_progress": bar(st.history.progress(true).unwrap_or_default()),
            "companies_requested": companies.iter().filter(|c| c.requested_at.is_some()).count(),
            "companies_unsubscribed": companies.iter().filter(|c| c.unsubscribed_at.is_some()).count(),
            "companies_total": companies
                .iter()
                .filter(|c| c.excluded_reason.is_empty() && c.decision != "ignored")
                .count(),
            "rounds": rounds,
        }),
    )
}

/// POST /autopilot/enabled — Pause / Turn on (only once setup is complete).
pub async fn set_enabled(
    State(st): St,
    method: Method,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> Response {
    let on = FormValues::new(&method, &headers, &uri, &body).get("enabled") == "true";
    let saved = {
        let mut guard = st.config.write().unwrap();
        let Some(cfg) = guard.as_mut() else {
            return redirect(&Method::POST, "/setup");
        };
        if on && cfg.validate().is_err() {
            return http_error(
                StatusCode::BAD_REQUEST,
                "Finish setup (profile and email) before turning on the autopilot",
            );
        }
        cfg.autopilot.enabled = on;
        config::save(&st.config_path, cfg)
    };
    match saved {
        Ok(()) => redirect(&Method::POST, "/"),
        Err(e) => http_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("Failed to save configuration: {e:#}"),
        ),
    }
}

/// POST /autopilot/run — "Dry run now" (always) or "Run now" (autopilot on).
pub async fn run_now(
    State(st): St,
    method: Method,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> Response {
    let dry_run = FormValues::new(&method, &headers, &uri, &body).get("dry_run") == "true";
    let Some(cfg) = st.config() else {
        return redirect(&Method::POST, "/setup");
    };
    if !dry_run && !cfg.autopilot.enabled {
        return http_error(
            StatusCode::BAD_REQUEST,
            "Turn on the autopilot first, or start a dry run",
        );
    }
    match start_round(&st, cfg, dry_run) {
        Ok(Some(_)) => redirect(&Method::POST, "/"),
        Ok(None) => http_error(StatusCode::CONFLICT, "An autopilot round is already running"),
        Err(e) => http_error(StatusCode::INTERNAL_SERVER_ERROR, &e),
    }
}

/// POST /today/tasks/{id}/done — also marks the company unsubscribed for unsubscribe links.
pub async fn todo_done(State(st): St, Path(id): Path<String>) -> Response {
    let id = parse_id(&id);
    let Ok(Some(task)) = st.history.get_pending_task_by_id(id) else {
        return http_error(StatusCode::NOT_FOUND, "Task not found");
    };
    if let Err(e) = st.history.complete_pending_task(id, "completed") {
        return http_error(StatusCode::INTERNAL_SERVER_ERROR, &format!("{e:#}"));
    }
    if task.task_type == TaskType::UnsubscribeLink
        && let Some(domain) = task.broker_id.strip_prefix(scanner::BROKER_PREFIX)
        && let Ok(Some(c)) = st.history.get_company(domain)
    {
        let _ = actions::mark_manual_unsubscribed(&st.history, &c);
    }
    redirect(&Method::POST, "/")
}

/// POST /today/tasks/{id}/skip — hides the to-do for 30 days.
pub async fn todo_skip(State(st): St, Path(id): Path<String>) -> Response {
    let until = Local::now() + TimeDelta::days(SKIP_DAYS);
    match st.history.skip_task_until(parse_id(&id), until) {
        Ok(()) => redirect(&Method::POST, "/"),
        Err(e) => http_error(StatusCode::INTERNAL_SERVER_ERROR, &format!("{e:#}")),
    }
}
```

In `src/web/mod.rs` replace

```rust
        .route("/", get(h::dashboard))
```

with

```rust
        .route("/", get(today::today))
        .route("/dashboard", get(h::dashboard))
        .route("/autopilot/enabled", post(today::set_enabled))
        .route("/autopilot/run", post(today::run_now))
        .route("/today/tasks/{task_id}/done", post(today::todo_done))
        .route("/today/tasks/{task_id}/skip", post(today::todo_skip))
```

In `src/web/views.rs` replace

```rust
    "companies.html", "company-detail.html", "companies-confirm.html",
);
```

with

```rust
    "companies.html", "company-detail.html", "companies-confirm.html", "today.html",
);
```

In `templates/web/layout.html` replace (both occurrences, desktop and mobile nav; use replace-all)

```html
<a href="/" class="nav-link {% if title == "Dashboard" %}active{% endif %}">Dashboard</a>
```

with

```html
<a href="/" class="nav-link {% if title == "Today" %}active{% endif %}">Today</a>
                        <a href="/dashboard" class="nav-link {% if title == "Dashboard" %}active{% endif %}">Dashboard</a>
```

- [ ] **Step 5: Run the web tests, expect pass.**

Run: `cargo test --test web && cargo test --lib web::`
Expected: all `test result: ok.` (incl. `today_page_todos_done_and_skip_with_csrf`, `today_pause_resume_and_dry_run`, `setup_wizard_full_flow`, `security_headers_and_static`)

- [ ] **Step 6: Look at it in the browser.** Run `cargo run -- serve` in the background (HOME with a configured `~/.eraser/config.yaml`), open http://localhost:8080/ with the Browser pane at desktop width and at the `mobile` preset (375 px): no horizontal scroll, buttons wrap, the bar renders. Stop the server.

- [ ] **Step 7: Commit.**

```bash
git add templates/web/today.html
git commit -m "Web: Today start page (status, to-dos with Done/Skip, progress, activity)" -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>" -- templates/web/today.html templates/web/layout.html src/web/today.rs src/web/mod.rs src/web/views.rs src/web/handlers.rs tests/web.rs
```

---

### Task 14: Settings → Autopilot form

**Files:**
- Modify: `src/web/handlers.rs` (`config_view`, `settings_page`), `src/web/today.rs`, `src/web/mod.rs`, `templates/web/settings.html`, `tests/web.rs`

- [ ] **Step 1: Write the failing test.** Append to `tests/web.rs`:

```rust
#[tokio::test]
async fn settings_autopilot_saves_and_requires_setup() {
    let mut h = harness(Some(configured()), None); // no SMTP host: setup incomplete
    let page = h.get("/settings").await;
    assert!(page.body.contains(r#"id="autopilot""#) && page.body.contains("versicherung"));
    let t = token_from(&page.body);

    let r = h
        .post_form("/settings/autopilot", "bogus", &[("enabled", "on")])
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN, "CSRF");

    let r = h
        .post_form(
            "/settings/autopilot",
            &t,
            &[
                ("enabled", "on"),
                ("check_inbox", "on"),
                ("reminder_days", "45"),
                ("inactive_months", "0"),
                ("keep_domains", "Club.Example\n@club.example, bank.example"),
            ],
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.body.contains("Finish setup"), "{}", r.body);
    let a = config::load(h.dir.path().join(".eraser/config.yaml"))
        .unwrap()
        .autopilot;
    assert!(!a.enabled, "stays off until setup is complete");
    assert!(a.check_inbox && !a.unsubscribe && !a.company_requests);
    assert_eq!((a.reminder_days, a.inactive_months), (45, 12));
    assert_eq!(a.keep_domains, ["club.example", "bank.example"]);
}
```

- [ ] **Step 2: Run it, expect failure.**

Run: `cargo test --test web settings_autopilot_saves_and_requires_setup`
Expected: FAIL at the first assertion (`id="autopilot"` missing)

- [ ] **Step 3: Implement.** In `src/web/handlers.rs` replace

```rust
            "inbox": c.inbox,
            "pipeline": c.pipeline,
        }),
```

with

```rust
            "inbox": c.inbox,
            "pipeline": c.pipeline,
            "autopilot": c.autopilot,
        }),
```

and replace

```rust
fn settings_page(st: &AppState, t: &CsrfToken, message: Option<(&str, bool)>) -> Response {
    let mut data = json!({ "title": "Settings", "config": config_view(&st.config()) });
    if let Some((msg, ok)) = message {
        data["inbox_message"] = json!(msg);
        data["inbox_success"] = json!(ok);
    }
    render(st, t, "settings.html", data)
}
```

with

```rust
fn settings_page(st: &AppState, t: &CsrfToken, message: Option<(&str, bool)>) -> Response {
    settings_with(st, t, "inbox", message)
}

/// Settings page with a message for one section ("inbox", "autopilot").
pub(super) fn settings_with(
    st: &AppState,
    t: &CsrfToken,
    section: &str,
    message: Option<(&str, bool)>,
) -> Response {
    let mut data = json!({
        "title": "Settings",
        "config": config_view(&st.config()),
        "keep_builtin": crate::autopilot::keep::summary(),
    });
    if let Some((msg, ok)) = message {
        data[format!("{section}_message")] = json!(msg);
        data[format!("{section}_success")] = json!(ok);
    }
    render(st, t, "settings.html", data)
}
```

In `src/web/today.rs` replace

```rust
use super::handlers::{
    FormValues, St, http_error, json_resp, needs_setup, parse_id, query, redirect, render,
};
```

with

```rust
use super::handlers::{
    FormValues, St, http_error, json_resp, needs_setup, parse_id, query, redirect, render,
    settings_with,
};
```

and append:

```rust
fn parse_keep_domains(raw: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for d in raw
        .split(|c: char| c == ',' || c.is_whitespace())
        .map(|d| d.trim().trim_start_matches('@').to_ascii_lowercase())
        .filter(|d| !d.is_empty())
    {
        if !out.contains(&d) {
            out.push(d);
        }
    }
    out
}

/// POST /settings/autopilot — switches, numbers and the keep list.
pub async fn settings_autopilot(
    State(st): St,
    Extension(t): Extension<CsrfToken>,
    method: Method,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> Response {
    let f = FormValues::new(&method, &headers, &uri, &body);
    let on = |k: &str| f.get(k) == "on";
    let num = |k: &str, default: i64, max: i64| {
        f.get(k)
            .trim()
            .parse::<i64>()
            .ok()
            .filter(|n| (1..=max).contains(n))
            .unwrap_or(default)
    };
    let (saved, setup_ok) = {
        let mut guard = st.config.write().unwrap();
        let cfg = guard.get_or_insert_with(Config::default);
        let setup_ok = cfg.validate().is_ok();
        cfg.autopilot = config::AutopilotConfig {
            enabled: on("enabled") && setup_ok,
            check_inbox: on("check_inbox"),
            confirm_links: on("confirm_links"),
            broker_requests: on("broker_requests"),
            scan_inbox: on("scan_inbox"),
            unsubscribe: on("unsubscribe"),
            company_requests: on("company_requests"),
            reminder_days: num("reminder_days", config::DEFAULT_REMINDER_DAYS, 365),
            inactive_months: num("inactive_months", config::DEFAULT_INACTIVE_MONTHS, 120),
            keep_domains: parse_keep_domains(&f.get("keep_domains")),
        };
        (config::save(&st.config_path, cfg), setup_ok)
    };
    let message = match (&saved, on("enabled") && !setup_ok) {
        (Err(e), _) => (format!("Failed to save configuration: {e:#}"), false),
        (Ok(()), true) => (
            "Saved. Finish setup (profile and email) before turning on the autopilot.".into(),
            false,
        ),
        (Ok(()), false) => ("Autopilot settings saved.".into(), true),
    };
    settings_with(&st, &t, "autopilot", Some((&message.0, message.1)))
}
```

In `src/web/mod.rs` replace

```rust
        .route("/settings/inbox", post(h::settings_inbox))
```

with

```rust
        .route("/settings/inbox", post(h::settings_inbox))
        .route("/settings/autopilot", post(today::settings_autopilot))
```

In `templates/web/settings.html` replace

```html
        <!-- Danger Zone -->
```

with

```html
        <!-- Autopilot -->
        <div class="card p-6 lg:col-span-2" id="autopilot">
            <div class="flex items-center gap-3 mb-6">
                <div class="w-10 h-10 rounded-xl flex items-center justify-center" style="background: var(--accent-muted);">
                    <svg class="w-5 h-5 text-accent" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M12 8v4l3 3m6-3a9 9 0 11-18 0 9 9 0 0118 0z"/>
                    </svg>
                </div>
                <div>
                    <h2 class="font-display text-lg font-semibold text-primary">Autopilot</h2>
                    <p class="text-sm text-muted">One round every 6 hours on the hosted copy; locally run <code>eraser autopilot</code>.</p>
                </div>
            </div>
            <form method="post" action="/settings/autopilot" class="space-y-5">
                {{ csrf_field }}
                <label class="flex items-center gap-3">
                    <input type="checkbox" name="enabled" {% if config.autopilot.enabled %}checked{% endif %}>
                    <span class="text-primary font-medium">Autopilot on</span>
                </label>
                <fieldset class="grid grid-cols-1 md:grid-cols-2 gap-3">
                    <legend class="label mb-2">Steps</legend>
                    <label class="flex items-center gap-2 text-sm"><input type="checkbox" name="check_inbox" {% if config.autopilot.check_inbox %}checked{% endif %}> Check the inbox for replies</label>
                    <label class="flex items-center gap-2 text-sm"><input type="checkbox" name="confirm_links" {% if config.autopilot.confirm_links %}checked{% endif %}> Open confirmation links (broker's own domain only)</label>
                    <label class="flex items-center gap-2 text-sm"><input type="checkbox" name="broker_requests" {% if config.autopilot.broker_requests %}checked{% endif %}> Broker requests and one reminder</label>
                    <label class="flex items-center gap-2 text-sm"><input type="checkbox" name="scan_inbox" {% if config.autopilot.scan_inbox %}checked{% endif %}> Scan the inbox for companies</label>
                    <label class="flex items-center gap-2 text-sm"><input type="checkbox" name="unsubscribe" {% if config.autopilot.unsubscribe %}checked{% endif %}> One-click unsubscribe (RFC 8058)</label>
                    <label class="flex items-center gap-2 text-sm"><input type="checkbox" name="company_requests" {% if config.autopilot.company_requests %}checked{% endif %}> Requests to inactive / newsletter-only companies</label>
                </fieldset>
                <div class="grid grid-cols-1 md:grid-cols-2 gap-4">
                    <div>
                        <label for="reminder_days" class="label">Reminder after (days without reply)</label>
                        <input type="number" id="reminder_days" name="reminder_days" min="1" max="365" class="input" value="{% if config.autopilot.reminder_days %}{{ config.autopilot.reminder_days }}{% else %}30{% endif %}">
                    </div>
                    <div>
                        <label for="inactive_months" class="label">Company inactive after (months)</label>
                        <input type="number" id="inactive_months" name="inactive_months" min="1" max="120" class="input" value="{% if config.autopilot.inactive_months %}{{ config.autopilot.inactive_months }}{% else %}12{% endif %}">
                    </div>
                </div>
                <div>
                    <label for="keep_domains" class="label">Keep list (never contacted, one domain per line)</label>
                    <textarea id="keep_domains" name="keep_domains" rows="4" class="input font-mono text-sm">{% for d in config.autopilot.keep_domains %}{{ d }}
{% endfor %}</textarea>
                    <p class="text-xs text-muted mt-2">{{ keep_builtin }}</p>
                </div>
                <button type="submit" class="btn btn-primary">Save autopilot settings</button>
            </form>
            {% if autopilot_message %}
            <div class="mt-4 p-3 rounded-lg {% if autopilot_success %}bg-green-500/10 text-green-400{% else %}bg-red-500/10 text-red-400{% endif %}">
                {{ autopilot_message }}
            </div>
            {% endif %}
        </div>

        <!-- Danger Zone -->
```

- [ ] **Step 4: Run the web tests, expect pass.**

Run: `cargo test --test web && cargo test --lib web::`
Expected: all `test result: ok.` (incl. `settings_autopilot_saves_and_requires_setup`, `settings_inbox_save`)

- [ ] **Step 5: Commit.**

```bash
git commit -m "Web: Settings → Autopilot (step switches, reminder/inactivity numbers, keep list)" -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>" -- src/web/handlers.rs src/web/today.rs src/web/mod.rs templates/web/settings.html tests/web.rs
```

---

### Task 15: Worker – cron, `/internal/*` block, token for the container

**Files:**
- Create: `deploy/src/guard.ts`, `deploy/test/guard.test.ts`
- Modify: `deploy/src/index.ts`, `deploy/wrangler.jsonc`, `deploy/package.json`

- [ ] **Step 1: Write the failing test.** Create `deploy/test/guard.test.ts`:

```ts
import { test } from "node:test";
import assert from "node:assert/strict";
import { isInternalPath } from "../src/guard.ts";

test("internal paths are blocked from the internet", () => {
  for (const p of [
    "/internal",
    "/internal/",
    "/internal/autopilot/tick",
    "//internal/autopilot/tick",
    "/INTERNAL/autopilot/tick",
    "/%69nternal/autopilot/tick",
  ]) {
    assert.equal(isInternalPath(p), true, p);
  }
  for (const p of ["/", "/internals", "/settings", "/api/internal", "/%E0%A4%A"]) {
    assert.equal(isInternalPath(p), false, p);
  }
});
```

In `deploy/package.json` replace

```json
    "dev": "wrangler dev"
```

with

```json
    "dev": "wrangler dev",
    "test": "node --test test/*.test.ts"
```

- [ ] **Step 2: Run it, expect failure.**

Run: `cd deploy && npm test`
Expected: FAIL with `ERR_MODULE_NOT_FOUND` for `../src/guard.ts`

- [ ] **Step 3: Implement the guard.** Create `deploy/src/guard.ts`:

```ts
/** Paths only the cron trigger may reach; the public fetch handler answers 404 for them. */
export function isInternalPath(pathname: string): boolean {
  let path = pathname;
  try {
    path = decodeURIComponent(pathname);
  } catch {
    // malformed escapes: check the raw path
  }
  return /^\/+internal(\/|$)/i.test(path);
}
```

- [ ] **Step 4: Run it, expect pass.**

Run: `cd deploy && npm test`
Expected: `# pass 1` `# fail 0`

- [ ] **Step 5: Wire it into the Worker.** In `deploy/src/index.ts`:

replace

```ts
export { ContainerProxy } from "@cloudflare/containers";
```

with

```ts
export { ContainerProxy } from "@cloudflare/containers";
import { isInternalPath } from "./guard";
```

replace

```ts
  ERASER_PASSWORD: string;
}
```

with

```ts
  ERASER_PASSWORD: string;
  ERASER_AUTOPILOT_TOKEN: string;
}
```

replace

```ts
  enableInternet = true; // SMTP/IMAP to iCloud
}
```

with

```ts
  enableInternet = true; // SMTP/IMAP to iCloud
  // The container's tick endpoint compares X-Autopilot-Token against this.
  envVars = { ERASER_AUTOPILOT_TOKEN: this.env.ERASER_AUTOPILOT_TOKEN ?? "" };
}
```

replace

```ts
    const url = new URL(request.url);
    if (!password) return new Response("Not configured", { status: 503 });
```

with

```ts
    const url = new URL(request.url);
    // Only the cron trigger (scheduled handler) talks to /internal/*.
    if (isInternalPath(url.pathname)) return new Response("Not found", { status: 404 });
    if (!password) return new Response("Not configured", { status: 503 });
```

and replace the end of the file

```ts
    return getContainer(env.ERASER, "main").fetch(request);
  },
};
```

with

```ts
    return getContainer(env.ERASER, "main").fetch(request);
  },

  // Cron trigger (wrangler.jsonc): one autopilot round every 6 hours. The container answers
  // 202 (started), 200 (autopilot switched off) or 409 (a round is still running).
  async scheduled(_controller: ScheduledController, env: Env): Promise<void> {
    if (!env.ERASER_AUTOPILOT_TOKEN) {
      console.warn("autopilot: ERASER_AUTOPILOT_TOKEN is not set, skipping");
      return;
    }
    const res = await getContainer(env.ERASER, "main").fetch(
      new Request("http://container/internal/autopilot/tick", {
        method: "POST",
        headers: { "X-Autopilot-Token": env.ERASER_AUTOPILOT_TOKEN },
      }),
    );
    console.log(`autopilot tick: ${res.status} ${await res.text()}`);
  },
};
```

In `deploy/wrangler.jsonc` replace

```jsonc
  "routes": [{ "pattern": "erase.kampa.app", "custom_domain": true }],
```

with

```jsonc
  "routes": [{ "pattern": "erase.kampa.app", "custom_domain": true }],
  "triggers": { "crons": ["0 */6 * * *"] },
```

- [ ] **Step 6: Type-check and re-run the test.**

Run: `cd deploy && npx tsc -p . && npm test`
Expected: no tsc output (exit 0), then `# pass 1`

- [ ] **Step 7: Commit.**

```bash
git add deploy/src/guard.ts deploy/test/guard.test.ts
git commit -m "Worker: 6-hourly autopilot cron, 404 for /internal/*, token passed to the container" -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>" -- deploy/src/guard.ts deploy/test/guard.test.ts deploy/src/index.ts deploy/wrangler.jsonc deploy/package.json
```

---

### Task 16: Full verification

**Files:** none (fix any failure in the file it points to and commit the fix path-limited, message ending with the Co-Authored-By line)

- [ ] **Step 1: Format check.** Run: `cargo fmt --check` — Expected: no output. If it lists diffs, run `cargo fmt` and commit: `git commit -m "cargo fmt" -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>" -- src tests`
- [ ] **Step 2: Lints.** Run: `cargo clippy --all-targets -- -D warnings` — Expected: `Finished` with no warnings.
- [ ] **Step 3: All tests.** Run: `cargo test` — Expected: every `test result: ok.`
- [ ] **Step 4: Docker-gated IMAP tests (Docker running).** Run: `ERASER_IMAP_IT=1 cargo test greenmail -- --nocapture` — Expected: `greenmail_round_end_to_end ... ok` and `greenmail_scan_discover_request_unsubscribe ... ok`.
- [ ] **Step 5: Worker.** Run: `cd deploy && npx tsc -p . && npm test` — Expected: exit 0, `# pass 1`.
- [ ] **Step 6: Release build.** Run: `cargo build --release` — Expected: `Finished release`.
- [ ] **Step 7: Knowledge graph.** Run: `graphify update .` (from `/Users/tobiasmaeder/eraser`) — Expected: graph updated (graphify-out/ stays untracked).

---

### Task 17: Deploy and live dry-run verification

**Files:** none

- [ ] **Step 1: Docker running.** Run: `docker info >/dev/null 2>&1 && echo docker-ok || echo docker-down` — Expected: `docker-ok`. If `docker-down`: `open -a Docker`, then poll with the same command until `docker-ok`.
- [ ] **Step 2: Disk ≥ 5 GB free.** Run: `df -g / | awk 'NR==2 {print $4 " GB free"}'` — Expected: a number ≥ 5. If lower, stop and ask the user before deleting anything (e.g. `docker builder prune`).
- [ ] **Step 3: Create the token in the Keychain (never printed).** Run:

```bash
security find-generic-password -a eraser -s eraser-autopilot-token >/dev/null 2>&1 \
  || security add-generic-password -a eraser -s eraser-autopilot-token -w "$(openssl rand -hex 32)"
security find-generic-password -a eraser -s eraser-autopilot-token >/dev/null && echo token-in-keychain
```

Expected: `token-in-keychain`

- [ ] **Step 4: Store it as a Worker secret (piped, not echoed).** Run:

```bash
cd /Users/tobiasmaeder/eraser/deploy && security find-generic-password -a eraser -s eraser-autopilot-token -w | npx wrangler secret put ERASER_AUTOPILOT_TOKEN
```

Expected: `✨ Success! Uploaded secret ERASER_AUTOPILOT_TOKEN`

- [ ] **Step 5: Deploy.** Run (timeout 600000 ms): `cd /Users/tobiasmaeder/eraser/deploy && npx wrangler deploy` — Expected: the image build finishes, output lists `erase.kampa.app (custom domain)` and `schedule: 0 */6 * * *`.
- [ ] **Step 6: `/internal/*` is blocked from the internet.** Run:

```bash
curl -s -o /dev/null -w '%{http_code}\n' -X POST https://erase.kampa.app/internal/autopilot/tick
curl -s -o /dev/null -w '%{http_code}\n' -X POST -H "X-Autopilot-Token: $(security find-generic-password -a eraser -s eraser-autopilot-token -w)" https://erase.kampa.app/internal/autopilot/tick
curl -s -o /dev/null -w '%{http_code}\n' https://erase.kampa.app/
```

Expected: `404`, `404`, `401` (login page)

- [ ] **Step 7: Live dry run in the user's Chrome.** Load the Chrome tools (`ToolSearch` with `select:mcp__claude-in-chrome__tabs_context_mcp,mcp__claude-in-chrome__navigate,mcp__claude-in-chrome__computer,mcp__claude-in-chrome__read_page,mcp__claude-in-chrome__find,mcp__claude-in-chrome__get_page_text`). Navigate to https://erase.kampa.app/. If the sign-in page appears, ask the user to sign in themselves (never type the password). On "Today": confirm Status shows "Off" and "Next round" shows "paused"; click **Dry run now**; wait ~60 s; reload; read the page text: the Activity list's first row starts with `Dry run:` (e.g. `Dry run: 250 broker requests, …` or `Dry run: nothing to do`). Open **Settings** and confirm the Autopilot card renders. Do not turn the autopilot on; tell the user it is ready and off until they switch it on.
- [ ] **Step 8: Cron reaches the container.** Run (timeout 600000 ms, stop it after the next full 6-hour UTC slot passes or after 10 minutes, whichever is first): `cd /Users/tobiasmaeder/eraser/deploy && npx wrangler tail --format pretty --search autopilot` — Expected at the next slot: `autopilot tick: 200 {"status":"disabled"}` (autopilot still off). If the slot is hours away, report that the tail check is pending instead of waiting.

---

### Task 18: README and CLAUDE.md

**Files:**
- Modify: `README.md` (after "### Inbox company scanner" ~line 78–116, commands table ~line 181–193), `CLAUDE.md`

- [ ] **Step 1: README section.** In `README.md` insert above the line `### Brokers without an email address`:

```markdown
### Autopilot

Eraser can run on its own. Turn it on under **Settings → Autopilot** (off by default; it can
only be switched on once your profile and email are set up). The hosted copy runs one round
every 6 hours; locally run `eraser autopilot` (`--dry-run` shows what would happen without
contacting anyone). A round:

1. checks the inbox for broker replies (read-only, nothing is marked as read);
2. opens confirmation links, but only on the broker's own domain and at most once per link —
   captchas, links on other domains and requests for an ID become to-dos;
3. sends one reminder to brokers without any reply after 30 days (citing the original request
   date and the deadline: GDPR Art. 12(3), Swiss FADP / Art. 18 DPO); after that they count as
   "no response". Then it sends the next broker requests up to the daily limit (250 via SMTP);
4. scans the inbox for new companies (headers only) and looks up their privacy contacts;
5. unsubscribes from RFC 8058 one-click lists; other unsubscribe links become to-dos;
6. sends deletion requests to companies that have not emailed you for 12 months or only send
   newsletters, have a verified privacy contact and are not on the keep list.

The **keep list** always protects Swiss and EU banks, insurers and health insurers (UBS,
PostFinance, Raiffeisen, ZKB, CSS, Helsana, SWICA, AXA, Mobiliar, …), government domains
(`.admin.ch`, cantonal `.ch`, `.gov`, `.gv.at`, …), domains or sender names containing
bank/insurance/versicherung/krankenkasse/clinic/klinik/arzt/steuer, and your own email domain.
Add more domains in Settings.

The start page **Today** shows the status (on/off, last and next round, Pause, Dry run now),
the to-dos that need you (Done, or Skip to hide one for 30 days), progress bars and the last
five rounds. Each step can be switched off; reminder days and inactivity months are
configurable (`autopilot:` in config.yaml). Only one round runs at a time.

Hosted: a Cloudflare Cron Trigger (`0 */6 * * *`) calls `POST /internal/autopilot/tick` in the
container with the `X-Autopilot-Token` secret (`ERASER_AUTOPILOT_TOKEN`); the Worker answers 404
to `/internal/*` from the internet.

```

In the commands table, replace

```markdown
| `eraser serve -p 3000` | Web interface on custom port |
```

with

```markdown
| `eraser serve -p 3000` | Web interface on custom port |
| `eraser autopilot` | Run one autopilot round now (`--dry-run` to preview) |
```

- [ ] **Step 2: CLAUDE.md.** In `CLAUDE.md` replace

```text
├── browser/             # Opt-out form filler, captcha detection (detection only), confirm links
```

with

```text
├── autopilot/           # Unattended rounds (run_round), keep list
├── browser/             # Opt-out form filler, captcha detection (detection only), confirm links
```

replace

```text
- Nothing is sent or unsubscribed without an explicit user action
```

with

```text
- Nothing is sent or unsubscribed without an explicit user action; turning on the autopilot (off by default) is that action, and it never contacts keep-list companies
```

and replace

```text
Deploy with `cd deploy && npx wrangler deploy` (Docker must be running).
```

with

```text
Deploy with `cd deploy && npx wrangler deploy` (Docker must be running). A cron trigger (`0 */6 * * *`) calls the container's `POST /internal/autopilot/tick` with the `ERASER_AUTOPILOT_TOKEN` Worker secret (also in the Keychain as `eraser-autopilot-token`); the Worker returns 404 for `/internal/*` from the internet. Worker tests: `cd deploy && npm test`.
```

- [ ] **Step 3: Commit.**

```bash
git commit -m "Docs: autopilot (README section, CLAUDE.md structure, security and deploy notes)" -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>" -- README.md CLAUDE.md
```

---

## Spec coverage (self-review)

| Design item | Task |
|---|---|
| Cron every 6 h wakes container, one round; same round via `eraser autopilot [--dry-run]` | 15, 11, 12 |
| Step 1 check inbox (monitor + classifier) | 10 |
| Step 2 confirm links: broker's own domain, ≤ 1 click, logged; captchas/ID → to-dos | 8, 10 |
| Step 3 batch up to daily limit (250 SMTP); one reminder after 30 days; then "no response" | 6, 7, 4 |
| Step 4 incremental header-only scan; discovery only for new companies | 10 |
| Step 5 RFC 8058 one-click only; plain links → to-dos | 9 |
| Step 6 inactive 12+ months or newsletter-only; verified contact; not on keep list | 9 |
| Step 7 round report stored and shown | 4, 7, 13 |
| Each step switchable; master switch off until setup complete; Pause; one round at a time | 1, 7, 13, 14, 4 |
| Keep list: built-in CH/EU banks/insurers, government, keywords, own domain; editable | 2, 14 |
| Today: status (on/off, last, next, Pause); to-dos sorted by deadline, 10 + show all, action + Done/Skip 30 days, 4 kinds; progress bars; last 5 rounds | 13 |
| Settings → Autopilot: switches, reminder days, inactivity months, keep list | 14 |
| `autopilot_rounds` table; `pending_tasks` kinds + `skipped_until`; `removal_requests.reminder_sent_at` | 3, 4 |
| `templates/email/reminder.txt` with original date, deadline, GDPR Art. 12(3) / FADP, "auto" law selection | 5 |
| Config `autopilot:` section with defaults | 1 |
| Tick: header token vs env, constant-time, 202 + background, 409 running, CSRF-exempt only with token | 11 |
| Worker: scheduled handler, 404 for `/internal/*`, token secret → container env, Keychain | 15, 17 |
| Tests: round with mock sender + GreenMail; dry run; lock; keep list; reminder once after N days; tick token; Worker block | 7, 9, 10, 11, 15 |
| Order: module → dashboard/settings → worker → deploy → live dry run | 1–10, 13–14, 15, 17 |
| README | 18 |
