//! Autopilot: one unattended round. Steps (each switchable under `autopilot:` in config.yaml):
//! 1. new broker replies from the inbox, 2. confirmation links on the broker's own domain,
//! 3. one reminder after `reminder_days` plus new broker requests up to the daily limit,
//! 4. incremental inbox company scan, 5. RFC 8058 one-click unsubscribes,
//! 6. requests to inactive / newsletter-only companies with a verified contact.
//!
//! Whatever needs a human becomes a to-do (pending_tasks). A dry run contacts no one (no email,
//! no link click, no unsubscribe, no IMAP move) but still records what it read.

pub mod keep;

use crate::broker::{Broker, BrokerDatabase};
use crate::browser::{ConfirmationHandler, detect_captcha_from_html, extract_confirmation_status};
use crate::config::Config;
use crate::email::{self, Message, SendResult, Sender};
use crate::history::{Company, PendingTask, PipelineStatus, Record, Status, Store, TaskType};
use crate::inbox::classifier::{ResponseType, classify_response};
use crate::inbox::monitor::Monitor;
use crate::scanner::actions::{self, Unsubscribe};
use crate::scanner::{self, Identity, discovery::Discoverer, domain};
use crate::template::Engine;
use chrono::{DateTime, Datelike, Local, TimeDelta, TimeZone, Timelike, Utc};
use futures::FutureExt;
use serde::{Deserialize, Serialize};
use std::panic::AssertUnwindSafe;
use std::time::Duration;

/// Hours between hosted rounds (Worker cron `0 */6 * * *`, UTC).
pub const ROUND_HOURS: u32 = 6;
/// Inbox look-back per round (a week covers missed rounds).
const INBOX_DAYS: i64 = 7;
/// Company scan window once a first scan exists.
const SCAN_DAYS: i64 = 7;
/// ponytail: contact discovery capped per round so a first scan cannot run for hours.
const MAX_DISCOVER: usize = 50;
/// A request still `pending` after this was interrupted by a crash (sends time out long before).
const INTERRUPTED_AFTER: TimeDelta = TimeDelta::minutes(10);
/// Round errors are cut to this many characters (the report is shown as a list).
const MAX_ERROR_LEN: usize = 200;

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
    match tokio::time::timeout(actions::SEND_TIMEOUT, d.sender.send(msg, None)).await {
        Ok(res) => res,
        Err(_) => SendResult {
            success: false,
            message_id: String::new(),
            error: Some("send timed out".into()),
        },
    }
}

async fn pause(cfg: &Config) {
    tokio::time::sleep(Duration::from_millis(
        cfg.options.rate_limit_ms.max(0) as u64
    ))
    .await;
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
    for rec in due
        .iter()
        .filter(|rec| rec.sent_at.is_some_and(|t| t <= cutoff))
    {
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
        // Claimed before sending: a crash mid-send never sends the reminder twice.
        match d.store.claim_reminder(rec.id) {
            Ok(true) => {}
            Ok(false) => continue,
            Err(e) => {
                r.errors.push(format!("reminder {}: {e:#}", rec.broker_id));
                continue;
            }
        }
        let res = send(d, &msg).await;
        if res.success {
            r.reminders_sent += 1;
        } else {
            let _ = d.store.release_reminder(rec.id);
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
        // Crash-safe: recorded as `pending` first, so a restart counts it as contacted and never
        // sends it again (run_round flags it for the user instead).
        let mut rec = Record {
            status: Status::Pending,
            ..Record::for_send(b, &rendered.template, &SendResult::default())
        };
        if let Err(e) = d.store.add(&mut rec) {
            r.errors
                .push(format!("{}: not sent, history error: {e:#}", b.id));
            continue;
        }
        let res = send(d, &msg).await;
        if let Err(e) = d.store.finish_send(rec.id, &res) {
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
        if matches!(
            d.store.find_task_by_url(TaskType::Confirm, url),
            Ok(Some(_))
        ) {
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
                r.errors
                    .push(format!("reply from {}: {e:#}", email.broker_id));
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
    if inbox.auto_archive
        && !dry_run
        && !archive_uids.is_empty()
        && !inbox.archive_folder.is_empty()
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
    let first = d
        .store
        .list_companies()
        .map(|c| c.is_empty())
        .unwrap_or(true);
    let days = if first {
        scanner::DEFAULT_DAYS
    } else {
        SCAN_DAYS
    };
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
                    r.errors
                        .push(format!("unsubscribe {}: {err}", c.agg.domain));
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
    let statuses = match d.store.get_all_broker_statuses() {
        Ok(s) => s,
        Err(e) => {
            r.errors.push(format!("company requests: {e:#}"));
            return;
        }
    };
    // An interrupted earlier send (still `pending`) is never repeated automatically.
    let interrupted = |c: &Company| {
        statuses
            .get(&scanner::company_broker(c).id)
            .is_some_and(|s| s.status == Status::Pending)
    };
    let verified = |c: &&Company| {
        c.requested_at.is_none()
            && !interrupted(c)
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

/// Web-form replies without a to-do get one (forms are always filled by the user).
fn sync_form_todos(store: &Store, r: &mut RoundReport) {
    let forms = match store.get_forms_with_status() {
        Ok(f) => f,
        Err(e) => {
            r.errors.push(format!("forms: {e:#}"));
            return;
        }
    };
    for f in forms
        .into_iter()
        .filter(|f| f.status == "pending" && f.task_id == 0)
    {
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

/// Sends left `pending` by a crashed run become to-dos once; they are never resent.
fn flag_interrupted(store: &Store, r: &mut RoundReport) {
    let found = match store.flag_interrupted_sends(Local::now() - INTERRUPTED_AFTER) {
        Ok(f) => f,
        Err(e) => {
            r.errors.push(format!("interrupted sends: {e:#}"));
            return;
        }
    };
    for rec in found {
        add_todo(
            store,
            r,
            PendingTask {
                broker_id: rec.broker_id,
                broker_name: rec.broker_name,
                task_type: TaskType::Review,
                notes: format!(
                    "Sending to {} was interrupted: check your Sent folder. It was not resent.",
                    rec.email
                ),
                ..Default::default()
            },
        );
    }
}

/// Runs one step; a panic is recorded as an error and the round goes on with the next step.
macro_rules! step {
    ($r:ident, $name:literal, $fut:expr) => {
        if AssertUnwindSafe($fut).catch_unwind().await.is_err() {
            $r.errors
                .push(concat!($name, ": internal error, step skipped").to_string());
        }
    };
}

/// Cuts each error to [`MAX_ERROR_LEN`] characters.
fn shorten_errors(r: &mut RoundReport) {
    for e in &mut r.errors {
        if let Some((i, _)) = e.char_indices().nth(MAX_ERROR_LEN) {
            e.truncate(i);
            e.push('…');
        }
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
    let mut budget = match d.store.emails_sent_since(start_of_today()) {
        Ok(n) => email::daily_limit(&d.cfg.email.provider) - n,
        Err(e) => {
            r.errors.push(format!("daily limit: {e:#}; nothing sent"));
            0
        }
    };
    flag_interrupted(d.store, &mut r);

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

    if connected
        && steps.check_inbox
        && let Some(m) = monitor.as_mut()
    {
        step!(r, "inbox", check_inbox(d, m, dry_run, &mut r));
    }
    if steps.confirm_links {
        step!(r, "confirm links", confirm_links(d, dry_run, &mut r));
    }
    if steps.broker_requests {
        step!(r, "reminders", reminders(d, dry_run, &mut budget, &mut r));
        step!(
            r,
            "broker requests",
            broker_requests(d, dry_run, &mut budget, &mut r)
        );
    }
    if connected
        && steps.scan_inbox
        && let Some(m) = monitor.as_mut()
    {
        step!(r, "scan", scan_companies(d, m, &mut r));
    }
    let cands = if steps.unsubscribe || steps.company_requests {
        candidates(d, &mut r)
    } else {
        Vec::new()
    };
    if steps.unsubscribe {
        step!(r, "unsubscribe", unsubscribe(d, &cands, dry_run, &mut r));
    }
    if steps.company_requests {
        step!(
            r,
            "company requests",
            company_requests(d, &cands, dry_run, &mut budget, &mut r)
        );
    }
    sync_form_todos(d.store, &mut r);

    if connected && let Some(m) = monitor.as_mut() {
        let _ = m.disconnect().await;
    }
    shorten_errors(&mut r);
    finish(d.store, round_id, &r);
    r
}

#[cfg(test)]
mod tests;
