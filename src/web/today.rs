//! Autopilot in the web UI: the internal tick endpoint (outside the CSRF layer, token only)
//! and starting rounds in the background.

use super::AppState;
use super::csrf::{CsrfToken, constant_time_eq};
use super::handlers::{
    FormValues, St, http_error, json_resp, needs_setup, parse_id, query, redirect, render,
    settings_with,
};
use super::status;
use crate::autopilot::{self, Deps, RoundReport};
use crate::config::{self, Config};
use crate::email;
use crate::history::{AutopilotRound, Funnel, PendingTask, STALE_ROUND, TaskType};
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
/// 409 while a round runs. Without a server token every tick is rejected (401).
pub async fn tick(State(st): St, headers: HeaderMap) -> Response {
    let expected = st.autopilot_token.as_deref().unwrap_or("");
    let given = headers
        .get(TOKEN_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if expected.is_empty() || !constant_time_eq(given.as_bytes(), expected.as_bytes()) {
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
        TaskType::Review if t.notes.contains("was interrupted") => {
            ("Interrupted send", "Show", page(""))
        }
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
            .map(|c| {
                (c + TimeDelta::days(deadline_days))
                    .format("%b %-d, %Y")
                    .to_string()
            })
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

const ERRORS_SHOWN: usize = 3;
pub(super) const INBOX_LOGIN_HINT: &str =
    "Inbox login failed — re-enter your iCloud app-specific password under Settings → Account";

#[derive(Serialize, Debug, PartialEq)]
struct RoundError {
    text: String,
    /// Settings link for errors the user can fix there.
    href: Option<&'static str>,
}

/// Errors of the latest finished round (at most [`ERRORS_SHOWN`]) and when it ran.
fn latest_round_errors(rounds: &[AutopilotRound]) -> Option<(String, Vec<RoundError>, usize)> {
    let r = rounds.iter().find(|r| r.finished_at.is_some())?;
    let report: RoundReport = serde_json::from_str(&r.report).ok()?;
    if report.errors.is_empty() {
        return None;
    }
    let at = r
        .started_at
        .map(|t| t.with_timezone(&Local).format("%b %-d, %H:%M").to_string())
        .unwrap_or_default();
    let shown = report
        .errors
        .iter()
        .take(ERRORS_SHOWN)
        .map(|e| {
            let lower = e.to_lowercase();
            if lower.starts_with("inbox") && lower.contains("login") {
                RoundError {
                    text: INBOX_LOGIN_HINT.into(),
                    href: Some("/settings#account"),
                }
            } else {
                RoundError {
                    text: e.clone(),
                    href: None,
                }
            }
        })
        .collect();
    Some((at, shown, report.errors.len()))
}

fn start_of_today(now: DateTime<Local>) -> DateTime<Local> {
    now.date_naive()
        .and_hms_opt(0, 0, 0)
        .and_then(|t| t.and_local_timezone(Local).earliest())
        .unwrap_or(now)
}

/// Pending review to-dos the autopilot added for sends interrupted mid-way.
fn interrupted_todos(st: &AppState, now: DateTime<Local>) -> Vec<PendingTask> {
    st.history
        .get_pending_tasks(Some(TaskType::Review), Some("pending"))
        .unwrap_or_default()
        .into_iter()
        .filter(|t| t.notes.contains("was interrupted"))
        .filter(|t| t.skipped_until.is_none_or(|u| u <= now))
        .collect()
}

/// GET / — the start page.
pub async fn today(
    State(st): St,
    Extension(t): Extension<CsrfToken>,
    method: Method,
    uri: Uri,
) -> Response {
    let cfg = st.config();
    if needs_setup(cfg.as_ref()) {
        return redirect(&method, "/setup");
    }
    let cfg = cfg.unwrap();
    let all = query(&uri).get("all") == "1";
    let local_now = Local::now();
    let mut tasks = st.history.todo_tasks(local_now).unwrap_or_default();
    tasks.extend(interrupted_todos(&st, local_now));
    tasks.sort_by_key(|t| (t.created_at, t.id));
    let shown = if all { tasks.len() } else { TODO_SHOWN };
    let todos: Vec<TodoItem> = tasks
        .iter()
        .take(shown)
        .map(|x| todo_item(x, cfg.autopilot.reminder_days))
        .collect();
    let now = Utc::now();
    let recent = st.history.recent_rounds(30).unwrap_or_default();
    let rounds: Vec<RoundView> = recent.iter().take(5).map(|r| round_view(r, now)).collect();
    let round_errors = latest_round_errors(&recent)
        .map(|(at, list, total)| json!({ "at": at, "list": list, "total": total }));
    let next_round = if st.autopilot_token.is_some() {
        autopilot::next_round(now)
            .with_timezone(&Local)
            .format("%b %-d, %H:%M")
            .to_string()
    } else {
        String::new()
    };
    let states = st.history.request_states().unwrap_or_default();
    let is_company = |id: &str| id.starts_with(scanner::BROKER_PREFIX);
    let broker_funnel = Funnel::of(states.values().filter(|s| !is_company(&s.broker_id)));
    let company_funnel = Funnel::of(states.values().filter(|s| is_company(&s.broker_id)));
    let remaining = st
        .brokers
        .filter(&cfg.options.regions, &cfg.options.excluded_brokers)
        .iter()
        .filter(|b| !b.email.is_empty() && !states.contains_key(&b.id))
        .count() as i64;
    let sent_today = st
        .history
        .emails_sent_since(start_of_today(local_now))
        .unwrap_or(0);
    render(
        &st,
        &t,
        "today.html",
        json!({
            "title": "Today",
            "profile": cfg.profile,
            "enabled": cfg.autopilot.enabled,
            "broker_requests": cfg.autopilot.broker_requests,
            "setup_ok": cfg.validate().is_ok(),
            "running": st.history.round_running().unwrap_or(false),
            "last_round": rounds.first().map(|r| r.started.clone()).unwrap_or_default(),
            "next_round": next_round,
            "todos": todos,
            "todo_total": tasks.len(),
            "show_all": all || tasks.len() <= TODO_SHOWN,
            "broker_total": st.brokers.brokers.len(),
            "broker_funnel": status::funnel(&broker_funnel, |k| format!("/brokers?status={k}")),
            "company_funnel": status::funnel(&company_funnel, |k| format!("/companies?filter=all&stage={k}")),
            "sending": status::sending(sent_today, email::daily_limit(&cfg.email.provider), remaining, local_now),
            "regions": status::regions(&st.brokers.brokers, &states),
            "activity": status::activity(&recent),
            "rounds": rounds,
            "round_errors": round_errors,
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
        Ok(None) => http_error(
            StatusCode::CONFLICT,
            "An autopilot round is already running",
        ),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn round(errors: &[&str], finished: bool) -> AutopilotRound {
        let report = RoundReport {
            errors: errors.iter().map(|e| e.to_string()).collect(),
            ..Default::default()
        };
        AutopilotRound {
            started_at: Some(Utc::now()),
            finished_at: finished.then(Utc::now),
            report: serde_json::to_string(&report).unwrap(),
            ..Default::default()
        }
    }

    #[test]
    fn latest_round_errors_cap_and_inbox_hint() {
        let r = round(
            &[
                "inbox: IMAP login failed: AUTHENTICATIONFAILED",
                "acme: Rejected by mail server: 550 5.1.1 Mailbox does not exist",
                "b: x",
                "c: y",
            ],
            true,
        );
        let (_, list, total) = latest_round_errors(&[r]).unwrap();
        assert_eq!((list.len(), total), (3, 4));
        assert_eq!(list[0].text, INBOX_LOGIN_HINT);
        assert_eq!(list[0].href, Some("/settings#account"));
        assert!(list[1].text.contains("550 5.1.1") && list[1].href.is_none());
        // a running round is skipped; a clean latest round shows nothing
        let running = round(&["inbox: login"], false);
        assert!(latest_round_errors(&[running, round(&[], true)]).is_none());
    }
}
