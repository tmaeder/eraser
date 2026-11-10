//! Companies page: inbox company scan, privacy-contact discovery and user-confirmed bulk
//! deletion requests / unsubscribes.

use super::AppState;
use super::csrf::CsrfToken;
use super::handlers::{
    FormValues, INBOX_NOT_CONFIGURED, St, error_box, fragment, html_escape, http_error,
    inbox_config, job_item, needs_setup, query, redirect, render, spawn_send_job, within,
};
use super::job::{JobKind, JobStatus, PersistentJobState, go_json_time};
use super::status;
use crate::config;
use crate::history::{Chip, Company, stage_label};
use crate::inbox::monitor::Monitor;
use crate::scanner::actions::{Unsubscribe, request_target};
use crate::scanner::discovery::{CONCURRENCY, Discoverer};
use crate::scanner::headers::parse_mailto;
use crate::scanner::{self, Identity, WINDOW};
use axum::Extension;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::Response;
use chrono::Local;
use serde::Serialize;
use serde_json::json;
use std::time::Duration;
use tokio::time::Instant;

const FILTERS: &[(&str, &str)] = &[
    ("", "Active"),
    ("lists", "Mailing lists"),
    ("contact", "Has privacy contact"),
    ("manual", "Manual"),
    ("requested", "Requested"),
    ("unsubscribed", "Unsubscribed"),
    ("ignored", "Ignored"),
    ("excluded", "Excluded"),
    ("all", "All"),
];
const DAY_OPTIONS: &[i64] = &[30, 90, 180, 365, 730];

fn matches(c: &Company, filter: &str) -> bool {
    let active = c.excluded_reason.is_empty() && c.decision != "ignored";
    match filter {
        "lists" => active && c.agg.is_mailing_list,
        "contact" => !c.contact_email.is_empty(),
        "manual" => c.contact_status == "manual",
        "requested" => c.requested_at.is_some(),
        "unsubscribed" => c.unsubscribed_at.is_some(),
        "ignored" => c.decision == "ignored",
        "excluded" => !c.excluded_reason.is_empty(),
        "all" => true,
        _ => active,
    }
}

#[derive(Serialize)]
struct Row {
    #[serde(flatten)]
    c: Company,
    unsub_kind: &'static str,
    unsub_target: String,
    /// Deletion request status (None: never requested).
    chip: Option<Chip>,
}

fn request_id(c: &Company) -> String {
    format!("{}{}", scanner::BROKER_PREFIX, c.agg.domain)
}

fn row(c: Company, states: &status::States) -> Row {
    let plan = Unsubscribe::for_company(&c);
    let (kind, target) = plan.describe();
    Row {
        unsub_kind: kind,
        unsub_target: target.to_string(),
        chip: states.get(&request_id(&c)).map(|s| s.chip(Local::now())),
        c,
    }
}

/// All values of a repeated urlencoded field (FormValues keeps only the first).
fn form_list(body: &[u8], key: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for (k, v) in url::form_urlencoded::parse(body) {
        if k == key && !out.contains(&v.to_string()) {
            out.push(v.into_owned());
        }
    }
    out
}

pub async fn companies_page(
    State(st): St,
    Extension(t): Extension<CsrfToken>,
    method: Method,
    uri: Uri,
) -> Response {
    let cfg = st.config();
    if needs_setup(&cfg) {
        return redirect(&method, "/setup");
    }
    let q = query(&uri);
    let (filter, search) = (q.get("filter"), q.get("q").trim().to_lowercase());
    let stage = q.get("stage");
    let states = st.history.request_states().unwrap_or_default();
    let all = st.history.list_companies().unwrap_or_default();
    let counts: Vec<_> = FILTERS
        .iter()
        .map(|(k, label)| {
            json!({ "key": k, "label": label, "count": all.iter().filter(|c| matches(c, k)).count() })
        })
        .collect();
    let rows: Vec<Row> = all
        .into_iter()
        .filter(|c| matches(c, &filter))
        .filter(|c| {
            search.is_empty()
                || c.agg.domain.contains(&search)
                || c.agg.display_name.to_lowercase().contains(&search)
        })
        .filter(|c| {
            stage.is_empty()
                || states
                    .get(&request_id(c))
                    .is_some_and(|s| s.matches_stage(&stage))
        })
        .map(|c| row(c, &states))
        .collect();
    let pending_discovery = rows
        .iter()
        .filter(|r| r.c.contact_status.is_empty() && r.c.excluded_reason.is_empty())
        .count();
    render(
        &st,
        &t,
        "companies.html",
        json!({
            "title": "Companies",
            "companies": rows,
            "filters": counts,
            "filter": filter,
            "stage": stage,
            "stage_label": stage_label(&stage).unwrap_or_default(),
            "search": q.get("q"),
            "job_id": q.get("job"),
            "error": q.get("error"),
            "inbox_configured": cfg.is_some_and(|c| c.inbox.enabled),
            "day_options": DAY_OPTIONS,
            "default_days": scanner::DEFAULT_DAYS,
            "pending_discovery": pending_discovery,
        }),
    )
}

fn company(st: &AppState, domain: &str) -> Option<Company> {
    st.history.get_company(domain).ok().flatten()
}

fn company_not_found() -> Response {
    http_error(StatusCode::NOT_FOUND, "Company not found")
}

pub async fn company_detail(
    State(st): St,
    Extension(t): Extension<CsrfToken>,
    Path(domain): Path<String>,
) -> Response {
    let Some(c) = company(&st, &domain) else {
        return company_not_found();
    };
    let actions = st.history.company_actions(&domain).unwrap_or_default();
    let request = request_target(&c);
    let states = st.history.request_states().unwrap_or_default();
    let id = request_id(&c);
    let now = Local::now();
    let reminder_days = st
        .config()
        .map_or(config::DEFAULT_REMINDER_DAYS, |c| c.autopilot.reminder_days);
    let deadline_bar = status::deadline(states.get(&id), now, reminder_days);
    let timeline = st.history.broker_timeline(&id).unwrap_or_default();
    render(
        &st,
        &t,
        "company-detail.html",
        json!({
            "title": "Companies",
            "company": row(c, &states),
            "deadline_bar": deadline_bar,
            "timeline": timeline,
            "actions": actions,
            "request_target": request.clone().unwrap_or_default(),
            "request_blocker": request.err().unwrap_or_default(),
        }),
    )
}

pub async fn company_ignore(
    State(st): St,
    Path(domain): Path<String>,
    method: Method,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> Response {
    let Some(c) = company(&st, &domain) else {
        return company_not_found();
    };
    let ignore = FormValues::new(&method, &headers, &uri, &body).get("ignore") != "0";
    let (decision, action) = if ignore {
        ("ignored", "ignore")
    } else {
        ("", "unignore")
    };
    let _ = st.history.set_company_decision(&c.agg.domain, decision);
    let _ = st
        .history
        .add_company_action(&c.agg.domain, action, "", "ok", "");
    redirect(&method, &format!("/companies/{}", c.agg.domain))
}

pub async fn company_mark_unsubscribed(
    State(st): St,
    Path(domain): Path<String>,
    method: Method,
) -> Response {
    let Some(c) = company(&st, &domain) else {
        return company_not_found();
    };
    let _ = scanner::actions::mark_manual_unsubscribed(&st.history, &c);
    redirect(&method, &format!("/companies/{}", c.agg.domain))
}

/// Re-runs contact discovery for one company.
pub async fn company_discover(
    State(st): St,
    Path(domain): Path<String>,
    method: Method,
) -> Response {
    let Some(c) = company(&st, &domain) else {
        return company_not_found();
    };
    if let Ok(d) = Discoverer::new() {
        d.discover_and_store(
            &st.history,
            &st.brokers.brokers,
            std::slice::from_ref(&c.agg.domain),
        )
        .await;
    }
    redirect(&method, &format!("/companies/{}", c.agg.domain))
}

// ==================== bulk: confirm → execute ====================

#[derive(Serialize)]
struct PlanRow {
    domain: String,
    name: String,
    /// email (deletion request), one_click, mailto, manual (link only), skip
    kind: &'static str,
    target: String,
    note: String,
}

fn plan_for(c: &Company, action: &str) -> PlanRow {
    let mut p = PlanRow {
        domain: c.agg.domain.clone(),
        name: c.agg.display_name.clone(),
        kind: "skip",
        target: String::new(),
        note: String::new(),
    };
    if action == "request" {
        match request_target(c) {
            Ok(to) => {
                p.kind = "email";
                p.note = format!("source: {}", c.contact_source);
                p.target = to;
            }
            Err(e) => p.note = e.into(),
        }
        return p;
    }
    match Unsubscribe::for_company(c) {
        Unsubscribe::OneClick(u) => {
            p.kind = "one_click";
            p.note = "HTTPS POST List-Unsubscribe=One-Click (RFC 8058)".into();
            p.target = u;
        }
        Unsubscribe::Mailto(u) => {
            let m = parse_mailto(&u).unwrap_or_default();
            p.kind = "mailto";
            p.note = if m.subject.is_empty() {
                "unsubscribe email".into()
            } else {
                format!("email, subject: {}", m.subject)
            };
            p.target = m.to;
        }
        Unsubscribe::Manual(u) => {
            p.kind = "manual";
            p.note = "link only – open it yourself, it is never fetched automatically".into();
            p.target = u;
        }
        Unsubscribe::None => p.note = "no List-Unsubscribe header".into(),
    }
    p
}

fn executable(p: &PlanRow) -> bool {
    matches!(p.kind, "email" | "one_click" | "mailto")
}

fn plans(st: &AppState, domains: &[String], action: &str) -> Vec<PlanRow> {
    domains
        .iter()
        .filter_map(|d| st.history.get_company(d).ok().flatten())
        .map(|c| plan_for(&c, action))
        .collect()
}

fn bulk_action(raw: &str) -> Option<&'static str> {
    match raw {
        "request" => Some("request"),
        "unsubscribe" => Some("unsubscribe"),
        _ => None,
    }
}

pub async fn companies_confirm(
    State(st): St,
    Extension(t): Extension<CsrfToken>,
    method: Method,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> Response {
    let f = FormValues::new(&method, &headers, &uri, &body);
    let Some(action) = bulk_action(&f.get("action")) else {
        return http_error(StatusCode::BAD_REQUEST, "Unknown action");
    };
    let domains = form_list(&body, "domain");
    if domains.is_empty() {
        return redirect(&method, "/companies?error=Select+at+least+one+company");
    }
    let rows = plans(&st, &domains, action);
    let runnable = rows.iter().filter(|p| executable(p)).count();
    let emails = rows
        .iter()
        .filter(|p| matches!(p.kind, "email" | "mailto"))
        .count();
    render(
        &st,
        &t,
        "companies-confirm.html",
        json!({
            "title": "Companies",
            "action": action,
            "rows": rows,
            "runnable": runnable,
            "emails": emails,
            "from": st.config().map(|c| c.email.from).unwrap_or_default(),
        }),
    )
}

pub async fn companies_execute(
    State(st): St,
    method: Method,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> Response {
    let f = FormValues::new(&method, &headers, &uri, &body);
    let Some(action) = bulk_action(&f.get("action")) else {
        return http_error(StatusCode::BAD_REQUEST, "Unknown action");
    };
    if f.get("confirmed") != "yes" {
        return http_error(StatusCode::BAD_REQUEST, "Confirmation required");
    }
    if st.jobs.get_active().is_some() {
        return redirect(&method, "/companies?error=A+send+job+is+already+running");
    }
    let Some(cfg) = st.config().filter(|c| !c.email.provider.is_empty()) else {
        return redirect(&method, "/companies?error=Email+is+not+configured");
    };
    // Re-plan server side: only what is executable right now is sent.
    let items: Vec<_> = form_list(&body, "domain")
        .iter()
        .filter_map(|d| st.history.get_company(d).ok().flatten())
        .filter(|c| executable(&plan_for(c, action)))
        .map(|c| job_item(scanner::company_broker(&c)))
        .collect();
    if items.is_empty() {
        return redirect(&method, "/companies?error=Nothing+to+send");
    }
    let sender = match (st.sender_factory)(&cfg.email) {
        Ok(s) => s,
        Err(e) => return http_error(StatusCode::INTERNAL_SERVER_ERROR, &format!("{e:#}")),
    };
    let kind = if action == "request" {
        JobKind::CompanyRequest
    } else {
        JobKind::CompanyUnsubscribe
    };
    let total = items.len() as i64;
    let job = st.jobs.create_kind(total, kind);
    let (id, started_at) = {
        let d = job.data.lock().unwrap();
        (d.id.clone(), d.started_at)
    };
    let state = PersistentJobState {
        id: id.clone(),
        status: JobStatus::Running,
        total,
        started_at: go_json_time(&started_at),
        remaining_brokers: items.iter().map(|b| b.broker.id.clone()).collect(),
        kind,
        ..Default::default()
    };
    if let Err(e) = st.persistence.save(&state) {
        tracing::warn!("failed to save job state: {e:#}");
    }
    spawn_send_job(st.clone(), job, items, sender, cfg);
    redirect(&method, &format!("/companies?job={id}"))
}

// ==================== HTMX: scan + discovery (chained requests) ====================

fn chain(endpoint: &str, vals: serde_json::Value) -> String {
    format!(
        r##"<div hx-post="{endpoint}" hx-trigger="load delay:300ms" hx-vals='{vals}' hx-target="#scan-result" hx-swap="innerHTML"></div>"##
    )
}

fn info_box(color: &str, html: &str) -> Response {
    fragment(
        StatusCode::OK,
        format!(
            r#"<div class="bg-{color}-100 border border-{color}-400 text-{color}-800 px-4 py-3 rounded">{html}</div>"#
        ),
    )
}

fn parse_num<T: std::str::FromStr>(s: &str, default: T) -> T {
    s.trim().parse().unwrap_or(default)
}

pub async fn api_scan(
    State(st): St,
    method: Method,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> Response {
    let Some((icfg, cfg)) = inbox_config(&st) else {
        return fragment(StatusCode::OK, INBOX_NOT_CONFIGURED);
    };
    let f = FormValues::new(&method, &headers, &uri, &body);
    let days: i64 = parse_num(&f.get("days"), scanner::DEFAULT_DAYS).clamp(1, 3650);
    let fi: usize = parse_num(&f.get("fi"), 0);
    let off: usize = parse_num(&f.get("off"), 0);
    let deadline = Instant::now() + Duration::from_secs(90);
    let id = Identity::from_config(&cfg);

    let mut inbox = Monitor::new(icfg.clone(), &[]);
    if let Err(e) = within(deadline, inbox.connect()).await {
        return error_box("\t\t\t", "Failed to connect to inbox:", &format!("{e:#}"));
    }
    let result = async {
        let existing = within(deadline, inbox.list_folders()).await?;
        let folders = scanner::pick_folders(&existing, &icfg, &[]);
        let Some(folder) = folders.get(fi).cloned() else {
            return Ok((folders, None));
        };
        let r = within(
            deadline,
            scanner::scan_window(&mut inbox, &st.history, &id, &folder, days, off),
        )
        .await?;
        Ok::<_, anyhow::Error>((folders, Some((folder, r))))
    }
    .await;
    let _ = inbox.disconnect().await;
    let (folders, window) = match result {
        Ok(v) => v,
        Err(e) => return error_box("\t\t\t", "Scan failed:", &format!("{e:#}")),
    };
    if let Some((folder, r)) = window {
        let (next_fi, next_off) = if off + WINDOW < r.total {
            (fi, off + WINDOW)
        } else {
            (fi + 1, 0)
        };
        if next_fi < folders.len() {
            let done = (off + WINDOW).min(r.total);
            return info_box(
                "blue",
                &format!(
                    "<strong>Scanning…</strong> {} ({} of {} folders): {done} / {} messages, {} new.{}",
                    html_escape(&folder),
                    fi + 1,
                    folders.len(),
                    r.total,
                    r.new,
                    chain(
                        "/api/companies/scan",
                        json!({ "days": days.to_string(), "fi": next_fi.to_string(), "off": next_off.to_string() })
                    )
                ),
            );
        }
    }
    let all = st.history.list_companies().unwrap_or_default();
    let active = all.iter().filter(|c| matches(c, "")).count();
    let lists = all.iter().filter(|c| matches(c, "lists")).count();
    info_box(
        "green",
        &format!(
            r##"<strong>Scan complete!</strong> {} folders scanned ({}): {active} companies ({lists} mailing lists), {} excluded.
            <p class="mt-2 text-sm"><button class="underline font-medium" hx-post="/api/companies/discover" hx-target="#scan-result" hx-swap="innerHTML">Find privacy contacts</button> |
            <a href="/companies" class="underline">Refresh list</a></p>"##,
            folders.len(),
            html_escape(&folders.join(", ")),
            all.iter().filter(|c| !c.excluded_reason.is_empty()).count(),
        ),
    )
}

/// Discovers contacts for the next `CONCURRENCY` unchecked active companies, then chains.
pub async fn api_discover(State(st): St) -> Response {
    let pending: Vec<String> = st
        .history
        .list_companies()
        .unwrap_or_default()
        .into_iter()
        .filter(|c| matches(c, "") && c.contact_status.is_empty())
        .map(|c| c.agg.domain)
        .collect();
    if pending.is_empty() {
        return info_box(
            "green",
            r#"<strong>All companies checked.</strong> <a href="/companies" class="underline">Refresh list</a>"#,
        );
    }
    let batch: Vec<String> = pending.iter().take(CONCURRENCY).cloned().collect();
    let d = match Discoverer::new() {
        Ok(d) => d,
        Err(e) => return error_box("\t\t\t", "Discovery failed:", &format!("{e:#}")),
    };
    let found = d
        .discover_and_store(&st.history, &st.brokers.brokers, &batch)
        .await;
    let summary: Vec<String> = found
        .iter()
        .map(|(dom, c)| {
            format!(
                "{} → {}",
                html_escape(dom),
                if c.email.is_empty() {
                    "manual".to_string()
                } else {
                    html_escape(&c.email)
                }
            )
        })
        .collect();
    let left = pending.len() - batch.len();
    if left == 0 {
        return info_box(
            "green",
            &format!(
                r#"<strong>All companies checked.</strong> <a href="/companies" class="underline">Refresh list</a><div class="mt-1 text-xs">{}</div>"#,
                summary.join("<br>")
            ),
        );
    }
    info_box(
        "blue",
        &format!(
            "<strong>Finding privacy contacts…</strong> {left} remaining.<div class=\"mt-1 text-xs\">{}</div>{}",
            summary.join("<br>"),
            chain("/api/companies/discover", json!({}))
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn form_list_collects_repeated_fields() {
        assert_eq!(
            form_list(b"domain=a.com&x=1&domain=b.de&domain=a.com", "domain"),
            ["a.com", "b.de"]
        );
    }

    #[test]
    fn filters() {
        let mut c = Company::default();
        c.agg.is_mailing_list = true;
        assert!(matches(&c, "") && matches(&c, "lists"));
        c.decision = "ignored".into();
        assert!(!matches(&c, "") && matches(&c, "ignored") && matches(&c, "all"));
        c.excluded_reason = "personal_domain".into();
        assert!(matches(&c, "excluded"));
    }
}
