//! Route handlers (port of the handler half of internal/web/server.go).

use super::AppState;
use super::csrf::CsrfToken;
use super::job::{Job, JobKind, JobStatus, PersistentJobState, go_json_time};
use super::views::STATIC_FILES;
use crate::broker::Broker;
use crate::config::{Config, EmailConfig, InboxConfig, Options, Profile};
use crate::email::{Message, SendResult, Sender};
use crate::history::{PipelineStatus, Record, Status, TaskType};
use crate::inbox::classifier::{ResponseType, classify_by_subject_only, classify_response};
use crate::inbox::monitor::Monitor as Inbox;
use crate::inbox::{Email, body_of, response_record};
use crate::scanner::{self, actions};
use axum::Extension;
use axum::body::{Body, Bytes};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum_extra::extract::cookie::CookieJar;
use minijinja::value::{Serde, Value};
use serde::Serialize;
use serde_json::json;
use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::time::Instant;

pub(super) type St = State<Arc<AppState>>;

const SESSION_COOKIE: &str = "eraser_session";
const SEND_TIMEOUT: Duration = Duration::from_secs(30);

// ==================== response helpers (net/http equivalents) ====================

/// http.Error
pub(super) fn http_error(status: StatusCode, msg: &str) -> Response {
    (
        status,
        [
            (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        format!("{msg}\n"),
    )
        .into_response()
}

/// http.Redirect with StatusFound (only GET/HEAD get a body).
pub(super) fn redirect(method: &Method, to: &str) -> Response {
    let mut resp = if method == Method::GET || method == Method::HEAD {
        (
            [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
            format!("<a href=\"{}\">Found</a>.\n\n", html_escape(to)),
        )
            .into_response()
    } else {
        Response::new(Body::empty())
    };
    *resp.status_mut() = StatusCode::FOUND;
    resp.headers_mut()
        .insert(header::LOCATION, HeaderValue::from_str(to).unwrap());
    resp
}

/// Subset of Go's http.DetectContentType for the bodies this server writes.
fn sniff(body: &str) -> Option<&'static str> {
    if body.is_empty() {
        return None;
    }
    let trimmed = body
        .trim_start_matches(['\t', '\n', '\x0c', '\r', ' '])
        .as_bytes();
    const SIGS: [&str; 17] = [
        "<!DOCTYPE HTML",
        "<HTML",
        "<HEAD",
        "<SCRIPT",
        "<IFRAME",
        "<H1",
        "<DIV",
        "<FONT",
        "<TABLE",
        "<A",
        "<STYLE",
        "<TITLE",
        "<B",
        "<BODY",
        "<BR",
        "<P",
        "<!--",
    ];
    let html = SIGS.iter().any(|sig| {
        let n = sig.len();
        trimmed.len() > n
            && trimmed[..n].eq_ignore_ascii_case(sig.as_bytes())
            && matches!(trimmed[n], b' ' | b'>')
    });
    Some(if html {
        "text/html; charset=utf-8"
    } else {
        "text/plain; charset=utf-8"
    })
}

/// w.Write of an HTML fragment (content type sniffed like Go).
pub(super) fn fragment(status: StatusCode, body: impl Into<String>) -> Response {
    let body = body.into();
    let mut resp = (status, body.clone()).into_response();
    match sniff(&body) {
        Some(ct) => resp
            .headers_mut()
            .insert(header::CONTENT_TYPE, HeaderValue::from_static(ct)),
        None => resp.headers_mut().remove(header::CONTENT_TYPE),
    };
    resp
}

/// json.NewEncoder(w).Encode (keys sorted, trailing newline).
pub(super) fn json_resp(status: StatusCode, v: serde_json::Value) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        format!("{v}\n"),
    )
        .into_response()
}

pub(super) fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '\'' => out.push_str("&#39;"),
            '"' => out.push_str("&#34;"),
            '\0' => out.push('\u{FFFD}'),
            c => out.push(c),
        }
    }
    out
}

pub async fn not_found() -> Response {
    http_error(StatusCode::NOT_FOUND, "404 page not found")
}

pub async fn static_file(Path(path): Path<String>) -> Response {
    match STATIC_FILES.iter().find(|(p, _, _)| *p == path) {
        Some((_, data, ct)) => (
            [
                (header::CONTENT_TYPE, *ct),
                (header::ACCEPT_RANGES, "bytes"),
            ],
            *data,
        )
            .into_response(),
        None => http_error(StatusCode::NOT_FOUND, "404 page not found"),
    }
}

/// r.FormValue: urlencoded body (POST/PUT/PATCH) first, then the query string.
pub(super) struct FormValues(HashMap<String, String>);

impl FormValues {
    pub(super) fn new(method: &Method, headers: &HeaderMap, uri: &Uri, body: &[u8]) -> Self {
        let mut map = HashMap::new();
        let is_form = headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|ct| ct.starts_with("application/x-www-form-urlencoded"));
        if is_form && matches!(*method, Method::POST | Method::PUT | Method::PATCH) {
            for (k, v) in url::form_urlencoded::parse(body) {
                map.entry(k.into_owned()).or_insert(v.into_owned());
            }
        }
        for (k, v) in url::form_urlencoded::parse(uri.query().unwrap_or("").as_bytes()) {
            map.entry(k.into_owned()).or_insert(v.into_owned());
        }
        Self(map)
    }

    pub(super) fn get(&self, key: &str) -> String {
        self.0.get(key).cloned().unwrap_or_default()
    }
}

pub(super) fn query(uri: &Uri) -> FormValues {
    FormValues::new(&Method::GET, &HeaderMap::new(), uri, b"")
}

/// fmt.Sscanf("%d"): leading (optionally signed) integer, 0 if none.
fn parse_id(s: &str) -> i64 {
    let s = s.trim_start();
    let end = s
        .char_indices()
        .find(|&(i, c)| !(c.is_ascii_digit() || (i == 0 && (c == '-' || c == '+'))))
        .map_or(s.len(), |(i, _)| i);
    s[..end].parse().unwrap_or(0)
}

// ==================== rendering ====================

fn render_value(st: &AppState, name: &str, ctx: Value) -> Response {
    let Ok(tmpl) = st.views.get_template(name) else {
        return http_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("Template not found: {name}"),
        );
    };
    match tmpl.render(ctx) {
        Ok(html) => fragment(StatusCode::OK, html),
        Err(e) => {
            tracing::error!("template {name}: {e:#}");
            http_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("Template error: {e}"),
            )
        }
    }
}

/// renderWithCSRF: page template wrapped in the layout, with the CSRF token and hidden field.
pub(super) fn render(
    st: &AppState,
    token: &CsrfToken,
    name: &str,
    data: serde_json::Value,
) -> Response {
    let field = format!(
        r#"<input type="hidden" name="gorilla.csrf.Token" value="{}">"#,
        token.0
    );
    let ctx = minijinja::context! {
        csrf_token => token.0.clone(),
        csrf_field => Value::from_safe_string(field),
        ..Value::from(Serde(data))
    };
    render_value(st, name, ctx)
}

fn render_partial(st: &AppState, name: &str, data: serde_json::Value) -> Response {
    render_value(st, name, Value::from(Serde(data)))
}

fn email_view(e: &EmailConfig) -> serde_json::Value {
    json!({ "provider": e.provider, "from": e.from, "smtp": e.smtp })
}

/// Config as the templates see it: Go structs are always present, so no skipped sections.
fn config_view(cfg: &Option<Config>) -> serde_json::Value {
    match cfg {
        None => serde_json::Value::Null,
        Some(c) => json!({
            "profile": c.profile,
            "email": email_view(&c.email),
            "options": c.options,
            "inbox": c.inbox,
            "pipeline": c.pipeline,
        }),
    }
}

// ==================== data helpers ====================

#[derive(Serialize, Default, Clone, Copy)]
struct Stats {
    total_brokers: i64,
    sent: i64,
    failed: i64,
    pending: i64,
}

#[derive(Serialize, Clone)]
pub(super) struct BrokerWithStatus {
    #[serde(flatten)]
    pub(super) broker: Broker,
    /// "never", "sent", "failed"
    status: String,
    last_sent: String,
    total_sent: i64,
}

#[derive(Serialize, Default)]
struct PipelineStats {
    email_sent: i64,
    awaiting_response: i64,
    form_required: i64,
    form_filled: i64,
    awaiting_captcha: i64,
    captcha_solved: i64,
    awaiting_confirmation: i64,
    confirmed: i64,
    rejected: i64,
    failed: i64,
    pending_tasks: i64,
    needs_review: i64,
}

impl AppState {
    fn brokers_with_status(
        &self,
        search: &str,
        category: &str,
        region: &str,
        status_filter: &str,
    ) -> Vec<BrokerWithStatus> {
        let statuses = self.history.get_all_broker_statuses().unwrap_or_default();
        let search = search.trim().to_lowercase();
        let category = category.trim().to_lowercase();
        let region = region.trim().to_lowercase();
        let status_filter = status_filter.trim().to_lowercase();

        let mut result = Vec::new();
        for b in &self.brokers.brokers {
            if !search.is_empty()
                && !b.name.to_lowercase().contains(&search)
                && !b.email.to_lowercase().contains(&search)
            {
                continue;
            }
            if !category.is_empty() && b.category.to_lowercase() != category {
                continue;
            }
            if !region.is_empty() && b.region.to_lowercase() != region {
                continue;
            }
            let mut bws = BrokerWithStatus {
                broker: b.clone(),
                status: "never".into(),
                last_sent: String::new(),
                total_sent: 0,
            };
            if let Some(s) = statuses.get(&b.id) {
                bws.status = s.status.as_str().to_string();
                bws.total_sent = s.total_sent;
                if let Some(t) = s.last_sent {
                    bws.last_sent = t.format("%b %-d, %Y").to_string();
                }
            }
            let skip = match status_filter.as_str() {
                "pending" => bws.status != "never",
                "sent" => bws.status != "sent",
                "failed" => bws.status != "failed",
                _ => false,
            };
            if !skip {
                result.push(bws);
            }
        }
        result
    }

    fn unique_values(&self, get: impl Fn(&Broker) -> &str) -> Vec<String> {
        let mut seen = std::collections::HashSet::new();
        self.brokers
            .brokers
            .iter()
            .map(get)
            .filter(|v| !v.is_empty() && seen.insert(v.to_string()))
            .map(String::from)
            .collect()
    }

    fn stats(&self) -> Stats {
        let total = self.brokers.brokers.len() as i64;
        let (sent, failed) = self
            .history
            .get_stats()
            .map(|(_, s, f)| (s, f))
            .unwrap_or((0, 0));
        Stats {
            total_brokers: total,
            sent,
            failed,
            pending: (total - sent - failed).max(0),
        }
    }

    fn recent_history(&self, limit: i64) -> Vec<Record> {
        self.history.get_recent_requests(limit).unwrap_or_default()
    }

    fn pipeline_stats(&self) -> PipelineStats {
        let mut s = PipelineStats::default();
        if let Ok(p) = self.history.get_pipeline_stats() {
            let g = |k| p.get(&k).copied().unwrap_or(0);
            s.email_sent = g(PipelineStatus::EmailSent);
            s.awaiting_response = g(PipelineStatus::AwaitingResponse);
            s.form_required = g(PipelineStatus::FormRequired);
            s.form_filled = g(PipelineStatus::FormFilled);
            s.awaiting_captcha = g(PipelineStatus::AwaitingCaptcha);
            s.captcha_solved = g(PipelineStatus::CaptchaSolved);
            s.awaiting_confirmation = g(PipelineStatus::AwaitingConfirmation);
            s.confirmed = g(PipelineStatus::Confirmed);
            s.rejected = g(PipelineStatus::Rejected);
            s.failed = g(PipelineStatus::Failed);
        }
        let pending_tasks = self
            .history
            .get_pending_task_stats()
            .map(|t| t.0)
            .unwrap_or(0);
        if let Ok(r) = self.history.get_broker_responses(None, true, 1000) {
            s.needs_review = r.len() as i64;
        }
        let pending_forms = self.history.get_form_stats().map(|f| f.0).unwrap_or(0);
        // Unified "Action Needed": pending forms + pending tasks + items needing review
        s.pending_tasks = pending_forms + pending_tasks + s.needs_review;
        s
    }

    // ---- wizard sessions ----

    fn session_cookie(&self, value: &str, max_age: i64) -> HeaderValue {
        let secure = if self.secure_cookies { "; Secure" } else { "" };
        format!(
            "{SESSION_COOKIE}={value}; Path=/; Max-Age={max_age}; HttpOnly{secure}; SameSite=Strict"
        )
        .parse()
        .unwrap()
    }

    fn session(&self, headers: &HeaderMap) -> Option<super::session::Session> {
        let id = CookieJar::from_headers(headers)
            .get(SESSION_COOKIE)?
            .value()
            .to_string();
        self.sessions.get(&id)
    }
}

fn task_type(s: &str) -> Option<Result<TaskType, ()>> {
    (!s.is_empty()).then(|| s.parse().map_err(|_| ()))
}

fn pending_tasks(
    st: &AppState,
    task_type_raw: &str,
    status: &str,
) -> Vec<crate::history::PendingTask> {
    match task_type(task_type_raw) {
        Some(Err(())) => Vec::new(), // unknown type matches no rows
        tt => st
            .history
            .get_pending_tasks(
                tt.map(|t| t.unwrap()),
                (!status.is_empty()).then_some(status),
            )
            .unwrap_or_default(),
    }
}

pub(super) fn needs_setup(cfg: &Option<Config>) -> bool {
    cfg.as_ref().is_none_or(|c| c.profile.first_name.is_empty())
}

// ==================== pages ====================

pub async fn dashboard(
    State(st): St,
    Extension(t): Extension<CsrfToken>,
    method: Method,
) -> Response {
    let cfg = st.config();
    if needs_setup(&cfg) {
        return redirect(&method, "/setup");
    }
    let cfg = cfg.unwrap();
    render(
        &st,
        &t,
        "dashboard.html",
        json!({
            "title": "Dashboard",
            "profile": cfg.profile,
            "broker_count": st.brokers.brokers.len(),
            "recent_history": st.recent_history(10),
            "stats": st.stats(),
            "pipeline_stats": st.pipeline_stats(),
        }),
    )
}

pub async fn brokers(State(st): St, Extension(t): Extension<CsrfToken>, uri: Uri) -> Response {
    let q = query(&uri);
    let (search, category, region, status) = (
        q.get("search"),
        q.get("category"),
        q.get("region"),
        q.get("status"),
    );
    let brokers = st.brokers_with_status(&search, &category, &region, &status);
    render(
        &st,
        &t,
        "brokers.html",
        json!({
            "title": "Data Brokers",
            "filtered": brokers.len(),
            "brokers": brokers,
            "categories": st.unique_values(|b| &b.category),
            "regions": st.unique_values(|b| &b.region),
            "search": search,
            "category": category,
            "region": region,
            "status": status,
            "total": st.brokers.brokers.len(),
        }),
    )
}

pub async fn history(State(st): St, Extension(t): Extension<CsrfToken>, uri: Uri) -> Response {
    let status_filter = query(&uri).get("status");
    let mut records = st.recent_history(1000);
    if status_filter == "sent" || status_filter == "failed" {
        records.retain(|r| r.status.as_str() == status_filter);
    }
    render(
        &st,
        &t,
        "history.html",
        json!({
            "title": "History",
            "history": records,
            "status_filter": status_filter,
        }),
    )
}

fn settings_page(st: &AppState, t: &CsrfToken, message: Option<(&str, bool)>) -> Response {
    let mut data = json!({ "title": "Settings", "config": config_view(&st.config()) });
    if let Some((msg, ok)) = message {
        data["inbox_message"] = json!(msg);
        data["inbox_success"] = json!(ok);
    }
    render(st, t, "settings.html", data)
}

pub async fn settings(State(st): St, Extension(t): Extension<CsrfToken>) -> Response {
    settings_page(&st, &t, None)
}

pub async fn settings_inbox(
    State(st): St,
    Extension(t): Extension<CsrfToken>,
    method: Method,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> Response {
    let form = FormValues::new(&method, &headers, &uri, &body);
    let (email, password) = (form.get("inbox_email"), form.get("inbox_password"));
    if email.is_empty() || password.is_empty() {
        return settings_page(&st, &t, Some(("Email and password are required", false)));
    }
    let saved = {
        let mut guard = st.config.write().unwrap();
        let cfg = guard.get_or_insert_with(Config::default);
        // Keep the user's folder settings; fill server/port from the provider so scans work
        // without a restart (the in-memory config is what the handlers use).
        let previous = std::mem::take(&mut cfg.inbox);
        cfg.inbox = InboxConfig {
            enabled: true,
            provider: "icloud".into(),
            email,
            password,
            folder: previous.folder,
            archive_folder: previous.archive_folder,
            auto_archive: previous.auto_archive,
            scan_folders: previous.scan_folders,
            ..Default::default()
        };
        crate::config::apply_inbox_defaults(&mut cfg.inbox);
        crate::config::save(&st.config_path, cfg)
    };
    match saved {
        Err(e) => settings_page(
            &st,
            &t,
            Some((&format!("Failed to save configuration: {e:#}"), false)),
        ),
        Ok(()) => settings_page(
            &st,
            &t,
            Some(("Inbox monitoring enabled successfully!", true)),
        ),
    }
}

pub async fn pipeline(
    State(st): St,
    Extension(t): Extension<CsrfToken>,
    method: Method,
) -> Response {
    let cfg = st.config();
    if needs_setup(&cfg) {
        return redirect(&method, "/setup");
    }
    render(
        &st,
        &t,
        "pipeline.html",
        json!({
            "title": "Pipeline Status",
            "pipeline_stats": st.pipeline_stats(),
            "recent_responses": st.history.get_broker_responses(None, false, 20).unwrap_or_default(),
            "pending_tasks": pending_tasks(&st, "", "pending"),
            "inbox_configured": cfg.unwrap().inbox.enabled,
        }),
    )
}

pub async fn forms(method: Method) -> Response {
    redirect(&method, "/tasks")
}

async fn form_status(
    st: &AppState,
    method: &Method,
    headers: &HeaderMap,
    broker_id: &str,
    status: PipelineStatus,
) -> Response {
    if st
        .history
        .update_pipeline_status(broker_id, status)
        .is_err()
    {
        return http_error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to update status");
    }
    if headers.get("HX-Request").is_some_and(|v| v == "true") {
        return (StatusCode::OK, [("HX-Redirect", "/tasks")]).into_response();
    }
    redirect(method, "/tasks")
}

pub async fn form_complete(
    State(st): St,
    method: Method,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    form_status(&st, &method, &headers, &id, PipelineStatus::Confirmed).await
}

pub async fn form_skip(
    State(st): St,
    method: Method,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    form_status(&st, &method, &headers, &id, PipelineStatus::Rejected).await
}

pub async fn tasks(
    State(st): St,
    Extension(t): Extension<CsrfToken>,
    method: Method,
    uri: Uri,
) -> Response {
    if needs_setup(&st.config()) {
        return redirect(&method, "/setup");
    }
    let q = query(&uri);
    let task_type = q.get("type");
    let mut status = q.get("status");
    if status.is_empty() {
        status = "pending".into();
    }
    let tasks = pending_tasks(&st, &task_type, "pending");
    let completed_list = pending_tasks(&st, &task_type, "completed");
    let forms = st.history.get_forms_with_status().unwrap_or_default();
    let review_items = st
        .history
        .get_broker_responses(None, true, 1000)
        .unwrap_or_default();
    let (pending_t, completed_t, skipped_t) =
        st.history.get_pending_task_stats().unwrap_or((0, 0, 0));
    let (pending_f, filled_f, captcha_f, failed_f, skipped_f) =
        st.history.get_form_stats().unwrap_or((0, 0, 0, 0, 0));
    let needs_review = review_items.len() as i64;
    render(
        &st,
        &t,
        "tasks.html",
        json!({
            "title": "Action Needed",
            "tasks": tasks,
            "completed_tasks_list": completed_list,
            "forms": forms,
            "review_items": review_items,
            "task_type": task_type,
            "status": status,
            "pending_tasks": pending_t,
            "completed_tasks": completed_t,
            "skipped_tasks": skipped_t,
            "pending_forms": pending_f,
            "filled_forms": filled_f,
            "captcha_forms": captcha_f,
            "failed_forms": failed_f,
            "skipped_forms": skipped_f,
            "needs_review": needs_review,
            "forms_needing_action": pending_f,
            "total_action_items": pending_f + pending_t + needs_review,
        }),
    )
}

pub async fn task_detail(
    State(st): St,
    Extension(t): Extension<CsrfToken>,
    Path(id): Path<String>,
) -> Response {
    match st.history.get_pending_task_by_id(parse_id(&id)) {
        Ok(Some(task)) => render(
            &st,
            &t,
            "task-detail.html",
            json!({ "title": "Task Detail", "task": task }),
        ),
        _ => http_error(StatusCode::NOT_FOUND, "Task not found"),
    }
}

async fn finish_task(st: &AppState, id: i64, status: &str) -> Response {
    if let Err(e) = st.history.complete_pending_task(id, status) {
        return fragment(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!(
                r#"<span class="text-red-600">Error: {}</span>"#,
                html_escape(&format!("{e:#}"))
            ),
        );
    }
    redirect(&Method::POST, &format!("/tasks/{id}/helper"))
}

pub async fn task_complete(
    State(st): St,
    Path(id): Path<String>,
    method: Method,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> Response {
    let mut status = FormValues::new(&method, &headers, &uri, &body).get("status");
    if status.is_empty() {
        status = "completed".into();
    }
    finish_task(&st, parse_id(&id), &status).await
}

pub async fn task_skip(State(st): St, Path(id): Path<String>) -> Response {
    finish_task(&st, parse_id(&id), "skipped").await
}

pub async fn task_helper(
    State(st): St,
    Extension(t): Extension<CsrfToken>,
    Path(id): Path<String>,
) -> Response {
    let id = parse_id(&id);
    if !matches!(st.history.get_pending_task_by_id(id), Ok(Some(_))) {
        return http_error(StatusCode::NOT_FOUND, "Task not found");
    }
    let _ = st.history.mark_task_opened(id);
    let Ok(Some(task)) = st.history.get_pending_task_by_id(id) else {
        return http_error(StatusCode::NOT_FOUND, "Task not found");
    };

    let profile_data: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(&task.browser_state)
            .map(|m: serde_json::Map<String, serde_json::Value>| {
                m.into_iter().filter(|(_, v)| v.is_string()).collect()
            })
            .unwrap_or_default();
    const FIELDS: [(&str, &str); 9] = [
        ("email", "Email"),
        ("firstName", "First Name"),
        ("lastName", "Last Name"),
        ("phone", "Phone"),
        ("address", "Address"),
        ("city", "City"),
        ("state", "State"),
        ("zipCode", "ZIP Code"),
        ("country", "Country"),
    ];
    let ordered: Vec<_> = FIELDS
        .iter()
        .filter_map(|(k, label)| {
            let v = profile_data.get(*k)?.as_str()?;
            (!v.is_empty()).then(|| json!({ "key": label, "value": v }))
        })
        .collect();
    render(
        &st,
        &t,
        "task-helper.html",
        json!({
            "title": format!("CAPTCHA Task: {}", task.broker_name),
            "task": task,
            "profile_data": profile_data,
            "ordered_profile": ordered,
        }),
    )
}

// ==================== setup wizard ====================

pub async fn setup_welcome(State(st): St, Extension(t): Extension<CsrfToken>) -> Response {
    render(
        &st,
        &t,
        "setup/welcome.html",
        json!({ "title": "Setup", "step": "welcome" }),
    )
}

pub async fn setup_profile(
    State(st): St,
    Extension(t): Extension<CsrfToken>,
    method: Method,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> Response {
    if method != Method::POST {
        let profile = st.session(&headers).map(|s| s.profile).unwrap_or_default();
        return render(
            &st,
            &t,
            "setup/profile.html",
            json!({
                "title": "Setup - Profile", "step": "profile", "profile": profile,
            }),
        );
    }
    let f = FormValues::new(&method, &headers, &uri, &body);
    let v = |k: &str| f.get(k).trim().to_string();
    let profile = Profile {
        first_name: v("first_name"),
        last_name: v("last_name"),
        email: v("email"),
        address: v("address"),
        city: v("city"),
        state: v("state"),
        zip_code: v("zip_code"),
        country: v("country"),
        phone: v("phone"),
        date_of_birth: v("dob"),
    };
    let mut errors = HashMap::new();
    if profile.first_name.is_empty() {
        errors.insert("first_name", "First name is required");
    }
    if profile.last_name.is_empty() {
        errors.insert("last_name", "Last name is required");
    }
    if profile.email.is_empty() {
        errors.insert("email", "Email is required");
    } else if crate::email::validate_email(&profile.email).is_err() {
        errors.insert("email", "Please enter a valid email address");
    }
    if !errors.is_empty() {
        return render(
            &st,
            &t,
            "setup/profile.html",
            json!({
                "title": "Setup - Profile", "step": "profile", "profile": profile, "errors": errors,
            }),
        );
    }

    // Store profile in the server-side session (cookie carries only the id)
    let existing = st.session(&headers).map(|s| s.id);
    let id = existing.clone().unwrap_or_else(|| st.sessions.create());
    st.sessions.update(&id, |s| {
        s.step = "email".into();
        s.profile = profile;
    });
    let mut resp = redirect(&method, "/setup/email");
    if existing.is_none() {
        resp.headers_mut()
            .append(header::SET_COOKIE, st.session_cookie(&id, 1800));
    }
    resp
}

pub async fn setup_email(
    State(st): St,
    Extension(t): Extension<CsrfToken>,
    method: Method,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> Response {
    let Some(session) = st
        .session(&headers)
        .filter(|s| !s.profile.first_name.is_empty())
    else {
        return redirect(&method, "/setup/profile");
    };

    if method == Method::POST {
        let f = FormValues::new(&method, &headers, &uri, &body);
        let mut e = EmailConfig {
            provider: "smtp".into(),
            ..Default::default()
        };
        e.smtp.host = f.get("smtp_host").trim().to_string();
        e.smtp.port = parse_id(&f.get("smtp_port"));
        e.smtp.username = f.get("smtp_username").trim().to_string();
        e.smtp.password = f.get("smtp_password").trim().to_string();
        e.smtp.use_tls = f.get("smtp_tls") == "on";
        // iCloud rejects mail whose From isn't one of your iCloud addresses
        e.from = e.smtp.username.clone();

        let mut errors = HashMap::new();
        if e.smtp.host.is_empty() {
            errors.insert("smtp_host", "SMTP host is required");
        }
        if e.smtp.port == 0 {
            errors.insert("smtp_port", "SMTP port is required");
        }
        if e.smtp.username.is_empty() {
            errors.insert("smtp_username", "iCloud email address is required");
        }
        if e.smtp.password.is_empty() {
            errors.insert("smtp_password", "App password is required");
        }
        if !e.smtp.use_tls && !e.smtp.username.is_empty() {
            errors.insert("smtp_tls", "TLS is required for iCloud");
        }
        if !errors.is_empty() {
            return render(
                &st,
                &t,
                "setup/email.html",
                json!({
                    "title": "Setup - iCloud", "step": "email", "profile": session.profile,
                    "email": email_view(&e), "errors": errors,
                }),
            );
        }
        st.sessions.update(&session.id, |s| {
            s.email = e;
            s.step = "test".into();
        });
        return redirect(&method, "/setup/test");
    }

    // iCloud defaults for new setups
    let mut e = session.email.clone();
    if e.smtp.host.is_empty() {
        e.smtp.host = "smtp.mail.me.com".into();
        e.smtp.port = 587;
        e.smtp.use_tls = true;
    }
    render(
        &st,
        &t,
        "setup/email.html",
        json!({
            "title": "Setup - iCloud", "step": "email", "profile": session.profile, "email": email_view(&e),
        }),
    )
}

pub async fn setup_test(
    State(st): St,
    Extension(t): Extension<CsrfToken>,
    method: Method,
    headers: HeaderMap,
) -> Response {
    let Some(session) = st
        .session(&headers)
        .filter(|s| !s.profile.first_name.is_empty())
    else {
        return redirect(&method, "/setup/profile");
    };
    if session.email.provider.is_empty() {
        return redirect(&method, "/setup/email");
    }
    render(
        &st,
        &t,
        "setup/test.html",
        json!({
            "title": "Setup - Test", "step": "test", "profile": session.profile, "email": email_view(&session.email),
        }),
    )
}

async fn send_with_timeout(sender: &dyn Sender, msg: &Message) -> SendResult {
    match tokio::time::timeout(SEND_TIMEOUT, sender.send(msg, None)).await {
        Ok(r) => r,
        Err(_) => SendResult {
            success: false,
            message_id: String::new(),
            error: Some("context deadline exceeded".into()),
        },
    }
}

pub async fn setup_test_send(State(st): St, headers: HeaderMap) -> Response {
    let Some(session) = st
        .session(&headers)
        .filter(|s| !s.email.provider.is_empty())
    else {
        return fragment(
            StatusCode::BAD_REQUEST,
            r#"<div class="text-red-600">Email not configured. Please go back to the email step.</div>"#,
        );
    };
    let sender = match (st.sender_factory)(&session.email) {
        Ok(s) => s,
        Err(e) => {
            return fragment(
                StatusCode::OK,
                format!(
                    "\n\t\t\t<div class=\"bg-red-100 border border-red-400 text-red-700 px-4 py-3 rounded\">\n\t\t\t\t<strong>Configuration error:</strong> {}\n\t\t\t\t<p class=\"mt-2 text-sm\">Please check your email settings and try again.</p>\n\t\t\t</div>\n\t\t",
                    html_escape(&format!("{e:#}"))
                ),
            );
        }
    };
    let msg = Message {
        to: session.profile.email.clone(),
        from: session.email.from.clone(),
        subject: "Eraser Test Email".into(),
        body: format!(
            "Hello {},\n\nThis is a test email from Eraser to verify your email configuration is working correctly.\n\nIf you received this email, your setup is complete and you're ready to start sending data removal requests!\n\nBest,\nEraser",
            session.profile.first_name
        ),
    };
    let result = send_with_timeout(sender.as_ref(), &msg).await;
    if !result.success {
        let err = result.error.unwrap_or_else(|| "Unknown error".into());
        return fragment(
            StatusCode::OK,
            format!(
                "\n\t\t\t<div class=\"bg-red-100 border border-red-400 text-red-700 px-4 py-3 rounded\">\n\t\t\t\t<strong>Test failed:</strong> {}\n\t\t\t\t<p class=\"mt-2 text-sm\">Please check your email configuration and try again.</p>\n\t\t\t</div>\n\t\t\t<div class=\"mt-4\">\n\t\t\t\t<a href=\"/setup/email\" class=\"text-indigo-600 hover:text-indigo-800 font-medium\">\n\t\t\t\t\tBack to Email Settings\n\t\t\t\t</a>\n\t\t\t</div>\n\t\t",
                html_escape(&err)
            ),
        );
    }
    fragment(
        StatusCode::OK,
        "\n\t\t<div class=\"bg-green-100 border border-green-400 text-green-700 px-4 py-3 rounded\">\n\t\t\t<strong>Success!</strong> Test email sent to your address.\n\t\t\t<p class=\"mt-2 text-sm\">Check your inbox (and spam folder) for the test message.</p>\n\t\t</div>\n\t\t<div class=\"mt-4\">\n\t\t\t<a href=\"/setup/complete\" class=\"inline-flex items-center px-6 py-3 bg-indigo-600 text-white font-medium rounded-md hover:bg-indigo-700\">\n\t\t\t\tComplete Setup\n\t\t\t</a>\n\t\t</div>\n\t",
    )
}

pub async fn setup_complete(
    State(st): St,
    Extension(t): Extension<CsrfToken>,
    method: Method,
    headers: HeaderMap,
) -> Response {
    let Some(session) = st
        .session(&headers)
        .filter(|s| !s.profile.first_name.is_empty() && !s.email.provider.is_empty())
    else {
        return redirect(&method, "/setup");
    };
    let cfg = Config {
        profile: session.profile.clone(),
        email: session.email.clone(),
        options: Options {
            template: "auto".into(),
            rate_limit_ms: 2000,
            ..Default::default()
        },
        ..Default::default()
    };
    if let Err(e) = crate::config::save(&st.config_path, &cfg) {
        return render(
            &st,
            &t,
            "setup/complete.html",
            json!({ "title": "Setup - Error", "error": format!("{e:#}") }),
        );
    }
    *st.config.write().unwrap() = Some(cfg);

    // Credentials now live in the config file; drop the session
    st.sessions.delete(&session.id);
    let mut resp = render(
        &st,
        &t,
        "setup/complete.html",
        json!({
            "title": "Setup Complete", "step": "complete", "profile": session.profile,
        }),
    );
    resp.headers_mut()
        .append(header::SET_COOKIE, st.session_cookie("", 0));
    resp
}

// ==================== API ====================

pub async fn api_stats(State(st): St) -> Response {
    let s = st.stats();
    (
        [(header::CONTENT_TYPE, "application/json")],
        format!(
            r#"{{"total_brokers":{},"sent":{},"failed":{}}}"#,
            s.total_brokers, s.sent, s.failed
        ),
    )
        .into_response()
}

pub async fn api_brokers(State(st): St, uri: Uri) -> Response {
    let q = query(&uri);
    let brokers = st.brokers_with_status(
        &q.get("search"),
        &q.get("category"),
        &q.get("region"),
        &q.get("status"),
    );
    render_partial(
        &st,
        "partials/broker-list.html",
        json!({
            "filtered": brokers.len(),
            "brokers": brokers,
            "total": st.brokers.brokers.len(),
        }),
    )
}

pub async fn api_history(State(st): St) -> Response {
    render_partial(
        &st,
        "partials/history-list.html",
        json!({ "history": st.recent_history(50) }),
    )
}

pub async fn api_delete_failed(State(st): St) -> Response {
    match st.history.delete_by_status(Status::Failed) {
        Ok(n) => json_resp(
            StatusCode::OK,
            json!({ "deleted": n, "message": format!("Deleted {n} failed records") }),
        ),
        Err(e) => json_resp(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({ "error": format!("{e:#}") }),
        ),
    }
}

pub async fn api_send_one(State(st): St, Path(broker_id): Path<String>) -> Response {
    if !st.rate_limiter.allow("send") {
        return fragment(
            StatusCode::TOO_MANY_REQUESTS,
            r#"<span class="text-yellow-600">Rate limit exceeded. Please wait a moment before sending more emails.</span>"#,
        );
    }
    let Some(br) = st.brokers.find_by_id(&broker_id).cloned() else {
        return fragment(
            StatusCode::NOT_FOUND,
            r#"<span class="text-red-600">Broker not found</span>"#,
        );
    };
    let Some(cfg) = st.config().filter(|c| !c.email.provider.is_empty()) else {
        return fragment(
            StatusCode::BAD_REQUEST,
            r#"<span class="text-red-600">Email not configured. <a href="/setup" class="underline">Configure now</a></span>"#,
        );
    };
    let sender = match (st.sender_factory)(&cfg.email) {
        Ok(s) => s,
        Err(e) => {
            return fragment(
                StatusCode::OK,
                format!(
                    r#"<span class="text-red-600">Error: {}</span>"#,
                    html_escape(&format!("{e:#}"))
                ),
            );
        }
    };
    let rendered = match st.engine.render(&cfg.options.template, &cfg.profile, &br) {
        Ok(r) => r,
        Err(e) => {
            return fragment(
                StatusCode::OK,
                format!(
                    r#"<span class="text-red-600">Template error: {}</span>"#,
                    html_escape(&format!("{e:#}"))
                ),
            );
        }
    };
    let msg = Message {
        to: br.email.clone(),
        from: cfg.email.from.clone(),
        subject: rendered.subject,
        body: rendered.body,
    };
    let result = send_with_timeout(sender.as_ref(), &msg).await;
    let _ = st
        .history
        .add(&mut Record::for_send(&br, &rendered.template, &result));

    if result.success {
        fragment(
            StatusCode::OK,
            r#"<span class="px-2 inline-flex text-xs leading-5 font-semibold rounded-full bg-green-100 text-green-800">Sent</span>"#,
        )
    } else {
        let err = result.error.unwrap_or_else(|| "Unknown error".into());
        fragment(
            StatusCode::OK,
            format!(
                r#"<span class="text-red-600" title="{}">Failed</span>"#,
                html_escape(&err)
            ),
        )
    }
}

pub async fn api_send_all(
    State(st): St,
    method: Method,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> Response {
    if !st.rate_limiter.allow("send-all") {
        return json_resp(
            StatusCode::TOO_MANY_REQUESTS,
            json!({ "error": "Rate limit exceeded. Please wait before sending another batch." }),
        );
    }
    if let Some(active) = st.jobs.get_active() {
        return json_resp(
            StatusCode::CONFLICT,
            json!({ "error": "A send job is already in progress", "job_id": active.id() }),
        );
    }
    let Some(cfg) = st.config().filter(|c| !c.email.provider.is_empty()) else {
        return json_resp(
            StatusCode::BAD_REQUEST,
            json!({ "error": "Email not configured. Please configure email settings first." }),
        );
    };
    let f = FormValues::new(&method, &headers, &uri, &body);
    let (search, category, region) = (f.get("search"), f.get("category"), f.get("region"));
    let mut status = f.get("status");
    if status.is_empty() {
        status = "pending".into(); // default: never sent
    }
    let to_send = st.brokers_with_status(&search, &category, &region, &status);
    if to_send.is_empty() {
        return json_resp(
            StatusCode::BAD_REQUEST,
            json!({ "error": "No pending brokers to send to." }),
        );
    }
    let sender = match (st.sender_factory)(&cfg.email) {
        Ok(s) => s,
        Err(e) => {
            return json_resp(
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({ "error": format!("{e:#}") }),
            );
        }
    };

    let total = to_send.len() as i64;
    let job = st.jobs.create(total);
    let (id, started_at) = {
        let d = job.data.lock().unwrap();
        (d.id.clone(), d.started_at)
    };
    let state = PersistentJobState {
        id: id.clone(),
        status: JobStatus::Running,
        total,
        started_at: go_json_time(&started_at),
        remaining_brokers: to_send.iter().map(|b| b.broker.id.clone()).collect(),
        search,
        category,
        region,
        status_filter: status,
        ..Default::default()
    };
    if let Err(e) = st.persistence.save(&state) {
        tracing::warn!("failed to save job state: {e:#}");
    }
    spawn_send_job(st.clone(), job, to_send, sender, cfg);
    json_resp(StatusCode::OK, json!({ "job_id": id, "total": total }))
}

pub(super) fn spawn_send_job(
    st: Arc<AppState>,
    job: Arc<Job>,
    to_send: Vec<BrokerWithStatus>,
    sender: Box<dyn Sender>,
    cfg: Config,
) {
    st.job_tasks.fetch_add(1, Ordering::SeqCst);
    tokio::spawn(async move {
        process_send_job(&st, &job, &to_send, sender.as_ref(), &cfg).await;
        st.job_tasks.fetch_sub(1, Ordering::SeqCst);
    });
}

fn save_job_progress(st: &AppState, job: &Job, sent: i64, failed: i64, remaining: &[String]) {
    let state = {
        let d = job.data.lock().unwrap();
        PersistentJobState {
            id: d.id.clone(),
            status: d.status,
            sent,
            failed,
            total: d.total,
            started_at: go_json_time(&d.started_at),
            remaining_brokers: remaining.to_vec(),
            kind: d.kind,
            ..Default::default()
        }
    };
    if let Err(e) = st.persistence.save(&state) {
        tracing::warn!("failed to save job progress: {e:#}");
    }
}

/// One broker removal request (records it in history). Err = template error (counted as failed).
async fn send_broker_item(
    st: &AppState,
    broker: &Broker,
    sender: &dyn Sender,
    cfg: &Config,
) -> Result<SendResult, ()> {
    let rendered = st
        .engine
        .render(&cfg.options.template, &cfg.profile, broker)
        .map_err(|_| ())?;
    let msg = Message {
        to: broker.email.clone(),
        from: cfg.email.from.clone(),
        subject: rendered.subject,
        body: rendered.body,
    };
    let result = send_with_timeout(sender, &msg).await;
    let _ = st
        .history
        .add(&mut Record::for_send(broker, &rendered.template, &result));
    Ok(result)
}

/// Runs one job item. Returns (result, whether an email was sent); Err = not attempted.
async fn run_job_item(
    st: &AppState,
    kind: JobKind,
    broker: &Broker,
    sender: &dyn Sender,
    cfg: &Config,
) -> Result<(SendResult, bool), ()> {
    if kind == JobKind::Broker {
        return send_broker_item(st, broker, sender, cfg)
            .await
            .map(|r| (r, true));
    }
    let domain = broker.id.strip_prefix(scanner::BROKER_PREFIX).ok_or(())?;
    let company = st.history.get_company(domain).ok().flatten().ok_or(())?;
    Ok(if kind == JobKind::CompanyRequest {
        let r = actions::send_request(&st.history, &st.engine, sender, cfg, &company, false).await;
        (r, true)
    } else {
        let emails = actions::Unsubscribe::for_company(&company).sends_email();
        let r = actions::unsubscribe(&st.history, &st.http, sender, cfg, &company, false).await;
        (r, emails)
    })
}

async fn process_send_job(
    st: &AppState,
    job: &Job,
    to_send: &[BrokerWithStatus],
    sender: &dyn Sender,
    cfg: &Config,
) {
    // A resumed job continues from its persisted counters.
    let (kind, mut sent, mut failed) = {
        let d = job.data.lock().unwrap();
        (d.kind, d.sent, d.failed)
    };
    let mut emails_this_run = 0i64;
    let rate_limit_ms = if cfg.options.rate_limit_ms == 0 {
        2000
    } else {
        cfg.options.rate_limit_ms
    };
    let daily_limit = crate::email::daily_limit(&cfg.email.provider);
    job.data.lock().unwrap().daily_limit = daily_limit;
    let ids: Vec<String> = to_send.iter().map(|b| b.broker.id.clone()).collect();

    for (i, b) in to_send.iter().enumerate() {
        if job.is_cancelled() {
            break;
        }
        if st.shutting_down.load(Ordering::SeqCst) {
            // Progress is already persisted; the job resumes on the next start.
            return;
        }
        let remaining = &ids[i..];
        if emails_this_run >= daily_limit {
            {
                let mut d = job.data.lock().unwrap();
                d.day_sent = emails_this_run;
                d.status = JobStatus::Paused;
                d.error = format!(
                    "Daily limit of {daily_limit} emails reached. Remaining {} brokers will be sent when you restart tomorrow.",
                    remaining.len()
                );
            }
            save_job_progress(st, job, sent, failed, remaining);
            tracing::info!(
                "Job paused: daily limit of {daily_limit} reached, {} remaining",
                remaining.len()
            );
            return;
        }
        let broker = &b.broker;
        job.update(sent, failed, &broker.name);

        let outcome = tokio::select! {
            r = run_job_item(st, kind, broker, sender, cfg) => r,
            _ = job.cancelled() => Ok((SendResult { success: false, message_id: String::new(), error: Some("context canceled".into()) }, false)),
        };
        let Ok((result, emailed)) = outcome else {
            failed += 1;
            job.update(sent, failed, &broker.name);
            save_job_progress(st, job, sent, failed, &ids[i + 1..]);
            continue;
        };
        if emailed {
            emails_this_run += 1;
            job.data.lock().unwrap().day_sent = emails_this_run;
        }
        if result.success {
            sent += 1;
            job.reset_auth_failures();
        } else {
            failed += 1;
            let err = result.error.clone().unwrap_or_default();
            if err.to_lowercase().contains("auth") && job.record_auth_failure() {
                save_job_progress(st, job, sent, failed, &ids[i + 1..]);
                job.stop_with_error(
                    "auth",
                    "Stopped due to repeated authentication failures. Your email provider may have rate-limited or blocked your account. Please check your email settings and try again later.",
                );
                tracing::warn!(
                    "Job stopped: repeated auth failures after {sent} sent, {failed} failed"
                );
                return;
            }
        }
        job.update(sent, failed, &broker.name);
        save_job_progress(st, job, sent, failed, &ids[i + 1..]);

        if i < to_send.len() - 1 && !job.is_cancelled() {
            tokio::time::sleep(Duration::from_millis(rate_limit_ms.max(0) as u64)).await;
        }
    }

    job.complete();
    if let Err(e) = st.persistence.clear() {
        tracing::warn!("failed to clear job state: {e:#}");
    }
}

/// checkPendingJob: resume an incomplete job from a previous run.
pub(super) fn check_pending_job(st: &Arc<AppState>) {
    let state = match st.persistence.load() {
        Ok(Some(s)) if !s.remaining_brokers.is_empty() => s,
        Ok(_) => return,
        Err(e) => {
            tracing::warn!("failed to load pending job: {e:#}");
            return;
        }
    };
    println!(
        "\nFound incomplete send job: {} of {} brokers remaining",
        state.remaining_brokers.len(),
        state.total
    );
    println!("Already sent: {}, failed: {}", state.sent, state.failed);
    let st = st.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(2)).await;
        resume_pending_job(st, state);
    });
}

pub(super) fn job_item(broker: Broker) -> BrokerWithStatus {
    BrokerWithStatus {
        broker,
        status: "never".into(),
        last_sent: String::new(),
        total_sent: 0,
    }
}

pub(super) fn resume_pending_job(st: Arc<AppState>, state: PersistentJobState) {
    let Some(cfg) = st.config().filter(|c| !c.email.provider.is_empty()) else {
        tracing::warn!("Cannot resume job: email not configured");
        let _ = st.persistence.clear();
        return;
    };
    let sender = match (st.sender_factory)(&cfg.email) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("Cannot resume job: failed to create email sender: {e:#}");
            let _ = st.persistence.clear();
            return;
        }
    };
    let to_send: Vec<BrokerWithStatus> = state
        .remaining_brokers
        .iter()
        .filter_map(|id| match id.strip_prefix(scanner::BROKER_PREFIX) {
            Some(domain) => st
                .history
                .get_company(domain)
                .ok()
                .flatten()
                .map(|c| scanner::company_broker(&c)),
            None => st.brokers.find_by_id(id).cloned(),
        })
        .map(job_item)
        .collect();
    if to_send.is_empty() {
        tracing::warn!("No valid brokers remaining in pending job");
        let _ = st.persistence.clear();
        return;
    }
    let job = st.jobs.create_kind(state.total, state.kind);
    {
        let mut d = job.data.lock().unwrap();
        d.sent = state.sent;
        d.failed = state.failed;
        if state.total > 0 {
            d.progress = ((state.sent + state.failed) * 100) / state.total;
        }
    }
    println!("Resuming send job: {} brokers remaining...", to_send.len());
    spawn_send_job(st, job, to_send, sender, cfg);
}

pub async fn api_job_active(State(st): St) -> Response {
    let job = st.jobs.get_active().map(|j| j.to_json());
    json_resp(StatusCode::OK, json!({ "job": job }))
}

pub async fn api_job_status(State(st): St, Path(id): Path<String>) -> Response {
    match st.jobs.get(&id) {
        Some(j) => json_resp(StatusCode::OK, j.to_json()),
        None => json_resp(StatusCode::NOT_FOUND, json!({ "error": "job not found" })),
    }
}

pub async fn api_job_cancel(State(st): St, Path(id): Path<String>) -> Response {
    match st.jobs.get(&id) {
        Some(j) => {
            j.cancel();
            json_resp(StatusCode::OK, json!({ "status": "cancelled" }))
        }
        None => json_resp(StatusCode::NOT_FOUND, json!({ "error": "job not found" })),
    }
}

pub async fn api_pipeline_stats(State(st): St) -> Response {
    let s = st.pipeline_stats();
    let body = format!(
        "{{\n\t\t\"email_sent\": {},\n\t\t\"awaiting_response\": {},\n\t\t\"form_required\": {},\n\t\t\"form_filled\": {},\n\t\t\"awaiting_captcha\": {},\n\t\t\"captcha_solved\": {},\n\t\t\"awaiting_confirmation\": {},\n\t\t\"confirmed\": {},\n\t\t\"rejected\": {},\n\t\t\"failed\": {},\n\t\t\"pending_tasks\": {},\n\t\t\"needs_review\": {}\n\t}}",
        s.email_sent,
        s.awaiting_response,
        s.form_required,
        s.form_filled,
        s.awaiting_captcha,
        s.captcha_solved,
        s.awaiting_confirmation,
        s.confirmed,
        s.rejected,
        s.failed,
        s.pending_tasks,
        s.needs_review
    );
    ([(header::CONTENT_TYPE, "application/json")], body).into_response()
}

pub async fn api_responses(State(st): St, uri: Uri) -> Response {
    let q = query(&uri);
    let rtype = q.get("type");
    let responses = st
        .history
        .get_broker_responses(
            (!rtype.is_empty()).then_some(rtype.as_str()),
            q.get("needs_review") == "true",
            50,
        )
        .unwrap_or_default();
    render_partial(
        &st,
        "partials/response-list.html",
        json!({ "responses": responses }),
    )
}

pub async fn api_tasks(State(st): St, uri: Uri) -> Response {
    let q = query(&uri);
    let task_type = q.get("type");
    let mut status = q.get("status");
    if status.is_empty() {
        status = "pending".into();
    }
    render_partial(
        &st,
        "partials/task-list.html",
        json!({
            "tasks": pending_tasks(&st, &task_type, &status),
            "task_type": task_type,
            "status": status,
        }),
    )
}

// ==================== inbox ====================

pub(super) const INBOX_NOT_CONFIGURED: &str = "\n\t\t\t<div class=\"bg-yellow-100 border border-yellow-400 text-yellow-800 px-4 py-3 rounded\">\n\t\t\t\t<strong>Inbox monitoring not configured.</strong>\n\t\t\t\t<p class=\"mt-1 text-sm\">Go to <a href=\"/settings\" class=\"underline\">Settings</a> to configure IMAP access.</p>\n\t\t\t</div>\n\t\t";

pub(super) fn error_box(indent: &str, title: &str, err: &str) -> Response {
    fragment(
        StatusCode::OK,
        format!(
            "\n{indent}<div class=\"bg-red-100 border border-red-400 text-red-700 px-4 py-3 rounded\">\n{indent}\t<strong>{title}</strong> {}\n{indent}</div>\n{}",
            html_escape(err),
            &indent[1..]
        ),
    )
}

/// Runs `fut` against the request-wide deadline (Go: context.WithTimeout).
pub(super) async fn within<T>(
    deadline: Instant,
    fut: impl Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<T> {
    tokio::time::timeout_at(deadline, fut)
        .await
        .unwrap_or_else(|_| Err(anyhow::anyhow!("context deadline exceeded")))
}

#[derive(Default)]
struct TypeCounts {
    success: i64,
    form_required: i64,
    confirm_required: i64,
    rejected: i64,
    pending: i64,
    unknown: i64,
}

impl TypeCounts {
    /// `pending_is_unknown`: the scan handler counts pending as unknown (as in Go).
    fn add(&mut self, t: ResponseType, pending_is_unknown: bool) {
        match t {
            ResponseType::Success => self.success += 1,
            ResponseType::FormRequired => self.form_required += 1,
            ResponseType::ConfirmationRequired => self.confirm_required += 1,
            ResponseType::Rejected => self.rejected += 1,
            ResponseType::Pending if !pending_is_unknown => self.pending += 1,
            _ => self.unknown += 1,
        }
    }
}

/// Connects and fetches INBOX + archive folder broker emails for the last `days`.
async fn fetch_all(
    inbox: &mut Inbox,
    cfg: &InboxConfig,
    days: i64,
    deadline: Instant,
) -> Result<Vec<Email>, Box<Response>> {
    if let Err(e) = within(deadline, inbox.connect()).await {
        return Err(Box::new(error_box(
            "\t\t\t",
            "Failed to connect to inbox:",
            &format!("{e:#}"),
        )));
    }
    let mut emails = match within(deadline, inbox.fetch_broker_emails(days)).await {
        Ok(e) => e,
        Err(e) => {
            let _ = inbox.disconnect().await;
            return Err(Box::new(error_box(
                "\t\t\t",
                "Failed to fetch emails:",
                &format!("{e:#}"),
            )));
        }
    };
    if !cfg.archive_folder.is_empty() {
        match within(
            deadline,
            inbox.fetch_broker_emails_from_folder(&cfg.archive_folder, days),
        )
        .await
        {
            Ok(more) => emails.extend(more),
            Err(e) => tracing::warn!(
                "failed to fetch from archive folder {}: {e:#}",
                cfg.archive_folder
            ),
        }
    }
    Ok(emails)
}

pub(super) fn inbox_config(st: &AppState) -> Option<(InboxConfig, Config)> {
    st.config()
        .filter(|c| c.inbox.enabled)
        .map(|c| (c.inbox.clone(), c))
}

pub async fn api_inbox_scan(State(st): St) -> Response {
    let Some((icfg, _)) = inbox_config(&st) else {
        return fragment(StatusCode::OK, INBOX_NOT_CONFIGURED);
    };
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut inbox = Inbox::new(
        icfg.clone(),
        &scanner::inbox_brokers(&st.brokers.brokers, &st.history),
    );
    let emails = match fetch_all(&mut inbox, &icfg, 7, deadline).await {
        Ok(e) => e,
        Err(resp) => return *resp,
    };
    if emails.is_empty() {
        let _ = inbox.disconnect().await;
        return fragment(
            StatusCode::OK,
            "\n\t\t\t<div class=\"bg-blue-100 border border-blue-400 text-blue-700 px-4 py-3 rounded\">\n\t\t\t\t<strong>No new broker emails found.</strong>\n\t\t\t\t<p class=\"mt-1 text-sm\">No emails from known data brokers in the last 7 days.</p>\n\t\t\t</div>\n\t\t",
        );
    }

    let mut counts = TypeCounts::default();
    let mut uids = Vec::new();
    for email in &emails {
        let classified = classify_response(email);
        uids.push(email.uid);
        let _ = st
            .history
            .add_broker_response(&mut response_record(email, &classified));
        counts.add(classified.r#type, true);
    }
    if icfg.auto_archive && !uids.is_empty() {
        match inbox.archive_emails(&uids, &icfg.archive_folder).await {
            Ok(()) => tracing::info!(
                "Archived {} emails to {} folder",
                uids.len(),
                icfg.archive_folder
            ),
            Err(e) => tracing::warn!("failed to archive emails: {e:#}"),
        }
    }
    let _ = inbox.disconnect().await;

    fragment(
        StatusCode::OK,
        format!(
            "\n\t\t<div class=\"bg-green-100 border border-green-400 text-green-800 px-4 py-3 rounded\">\n\t\t\t<strong>Scan complete!</strong> Found {} broker emails.\n\t\t\t<div class=\"mt-2 text-sm grid grid-cols-2 gap-2\">\n\t\t\t\t<div>Success: <span class=\"font-semibold\">{}</span></div>\n\t\t\t\t<div>Form required: <span class=\"font-semibold\">{}</span></div>\n\t\t\t\t<div>Confirm required: <span class=\"font-semibold\">{}</span></div>\n\t\t\t\t<div>Rejected: <span class=\"font-semibold\">{}</span></div>\n\t\t\t\t<div>Unknown: <span class=\"font-semibold\">{}</span></div>\n\t\t\t</div>\n\t\t\t<p class=\"mt-2 text-sm\">\n\t\t\t\t<a href=\"/tasks\" class=\"underline font-medium\">View pending tasks</a> |\n\t\t\t\t<a href=\"/pipeline\" class=\"underline\" onclick=\"window.location.reload()\">Refresh page</a>\n\t\t\t</p>\n\t\t</div>\n\t",
            emails.len(),
            counts.success,
            counts.form_required,
            counts.confirm_required,
            counts.rejected,
            counts.unknown
        ),
    )
}

pub async fn api_inbox_rescan(State(st): St, uri: Uri) -> Response {
    let Some((icfg, _)) = inbox_config(&st) else {
        return fragment(StatusCode::OK, INBOX_NOT_CONFIGURED);
    };
    if query(&uri).get("clear") == "true"
        && let Err(e) = st.history.clear_broker_responses()
    {
        return error_box("\t\t\t\t", "Failed to clear responses:", &format!("{e:#}"));
    }
    let deadline = Instant::now() + Duration::from_secs(180);
    let mut inbox = Inbox::new(
        icfg.clone(),
        &scanner::inbox_brokers(&st.brokers.brokers, &st.history),
    );
    let emails = match fetch_all(&mut inbox, &icfg, 30, deadline).await {
        Ok(e) => e,
        Err(resp) => return *resp,
    };
    let _ = inbox.disconnect().await;
    if emails.is_empty() {
        return fragment(
            StatusCode::OK,
            "\n\t\t\t<div class=\"bg-blue-100 border border-blue-400 text-blue-700 px-4 py-3 rounded\">\n\t\t\t\t<strong>No broker emails found.</strong>\n\t\t\t\t<p class=\"mt-1 text-sm\">No emails from known data brokers in the last 30 days.</p>\n\t\t\t</div>\n\t\t",
        );
    }

    let mut counts = TypeCounts::default();
    let (mut updated, mut inserted) = (0, 0);
    for email in &emails {
        let c = classify_response(email);
        let body = body_of(email);
        match st
            .history
            .find_broker_response_by_subject(&email.broker_id, &email.subject)
        {
            Ok(Some(existing)) => {
                if st
                    .history
                    .update_broker_response_classification(
                        existing.id,
                        c.r#type.as_str(),
                        &c.form_url,
                        &c.confirm_url,
                        c.confidence,
                        c.needs_review,
                    )
                    .is_ok()
                {
                    updated += 1;
                }
                if existing.email_body.is_empty() && !body.is_empty() {
                    let _ = st.history.update_broker_response_body(existing.id, &body);
                }
            }
            _ => {
                if st
                    .history
                    .add_broker_response(&mut response_record(email, &c))
                    .is_ok()
                {
                    inserted += 1;
                }
            }
        }
        counts.add(c.r#type, false);
    }

    fragment(
        StatusCode::OK,
        format!(
            "\n\t\t<div class=\"bg-green-100 border border-green-400 text-green-800 px-4 py-3 rounded\">\n\t\t\t<strong>Rescan complete!</strong> Processed {} broker emails.\n\t\t\t<div class=\"mt-2 text-sm grid grid-cols-2 gap-2\">\n\t\t\t\t<div>Updated: <span class=\"font-semibold\">{}</span></div>\n\t\t\t\t<div>New: <span class=\"font-semibold\">{}</span></div>\n\t\t\t</div>\n\t\t\t<div class=\"mt-2 text-sm grid grid-cols-3 gap-2\">\n\t\t\t\t<div>Success: <span class=\"font-semibold\">{}</span></div>\n\t\t\t\t<div>Form required: <span class=\"font-semibold\">{}</span></div>\n\t\t\t\t<div>Confirm required: <span class=\"font-semibold\">{}</span></div>\n\t\t\t\t<div>Pending: <span class=\"font-semibold\">{}</span></div>\n\t\t\t\t<div>Rejected: <span class=\"font-semibold\">{}</span></div>\n\t\t\t\t<div>Unknown: <span class=\"font-semibold\">{}</span></div>\n\t\t\t</div>\n\t\t\t<p class=\"mt-2 text-sm\">\n\t\t\t\t<a href=\"/tasks\" class=\"underline font-medium\">View action items</a> |\n\t\t\t\t<a href=\"/pipeline\" class=\"underline\" onclick=\"window.location.reload()\">Refresh page</a>\n\t\t\t</p>\n\t\t</div>\n\t",
            emails.len(),
            updated,
            inserted,
            counts.success,
            counts.form_required,
            counts.confirm_required,
            counts.pending,
            counts.rejected,
            counts.unknown
        ),
    )
}

/// Backfills missing email bodies from IMAP (best effort), then reclassifies every response.
pub async fn api_reclassify(State(st): St) -> Response {
    let mut responses = match st.history.get_all_broker_responses() {
        Ok(r) => r,
        Err(e) => return error_box("\t\t\t", "Failed to get responses:", &format!("{e:#}")),
    };
    if responses.is_empty() {
        return fragment(
            StatusCode::OK,
            "\n\t\t\t<div class=\"bg-blue-100 border border-blue-400 text-blue-700 px-4 py-3 rounded\">\n\t\t\t\t<strong>No responses to reclassify.</strong>\n\t\t\t</div>\n\t\t",
        );
    }

    let missing = responses.iter().filter(|r| r.email_body.is_empty()).count();
    let icfg = st.config().map(|c| c.inbox).unwrap_or_default();
    if missing > 0 && !icfg.server.is_empty() {
        tracing::info!("Found {missing} records missing email bodies, fetching from IMAP...");
        let deadline = Instant::now() + Duration::from_secs(60);
        let mut inbox = Inbox::new(
            icfg.clone(),
            &scanner::inbox_brokers(&st.brokers.brokers, &st.history),
        );
        match within(deadline, inbox.connect()).await {
            Err(e) => tracing::warn!("failed to connect to IMAP for body fetch: {e:#}"),
            Ok(()) => {
                let mut all = Vec::new();
                match within(deadline, inbox.fetch_recent_emails(30)).await {
                    Ok(e) => all.extend(e),
                    Err(e) => tracing::warn!("failed to fetch from INBOX: {e:#}"),
                }
                if !icfg.archive_folder.is_empty() {
                    match within(
                        deadline,
                        inbox.fetch_broker_emails_from_folder(&icfg.archive_folder, 30),
                    )
                    .await
                    {
                        Ok(e) => all.extend(e),
                        Err(e) => tracing::warn!("failed to fetch from archive folder: {e:#}"),
                    }
                }
                let _ = inbox.disconnect().await;
                tracing::info!("Fetched {} emails from IMAP", all.len());

                let bodies: HashMap<String, String> = all
                    .iter()
                    .filter(|e| !e.broker_id.is_empty())
                    .filter_map(|e| {
                        let body = body_of(e);
                        (!body.is_empty()).then(|| (format!("{}|{}", e.broker_id, e.subject), body))
                    })
                    .collect();
                let mut filled = 0;
                for resp in responses.iter_mut().filter(|r| r.email_body.is_empty()) {
                    if let Some(body) =
                        bodies.get(&format!("{}|{}", resp.broker_id, resp.email_subject))
                        && st
                            .history
                            .update_broker_response_body(resp.id, body)
                            .is_ok()
                    {
                        filled += 1;
                        resp.email_body = body.clone();
                    }
                }
                tracing::info!("Updated {filled} records with email bodies from IMAP");
            }
        }
    }

    let (mut updated, mut unchanged) = (0, 0);
    let mut counts = TypeCounts::default();
    for resp in &responses {
        let (new_type, confidence, needs_review, form_url, confirm_url) =
            if !resp.email_body.is_empty() {
                let email = Email {
                    from: resp.email_from.clone(),
                    subject: resp.email_subject.clone(),
                    body: resp.email_body.clone(),
                    ..Default::default()
                };
                let c = classify_response(&email);
                (
                    c.r#type,
                    c.confidence,
                    c.needs_review,
                    c.form_url,
                    c.confirm_url,
                )
            } else {
                let (t, conf, review) = classify_by_subject_only(&resp.email_subject);
                (
                    t,
                    conf,
                    review,
                    resp.form_url.clone(),
                    resp.confirm_url.clone(),
                )
            };
        let changed = new_type.as_str() != resp.response_type
            || (resp.response_type == "unknown" && new_type != ResponseType::Unknown);
        if changed {
            if st
                .history
                .update_broker_response_classification(
                    resp.id,
                    new_type.as_str(),
                    &form_url,
                    &confirm_url,
                    confidence,
                    needs_review,
                )
                .is_ok()
            {
                updated += 1;
            }
        } else {
            unchanged += 1;
        }
        counts.add(new_type, false);
    }

    fragment(
        StatusCode::OK,
        format!(
            "\n\t\t<div class=\"bg-green-100 border border-green-400 text-green-800 px-4 py-3 rounded\">\n\t\t\t<strong>Reclassification complete!</strong> Processed {} records.\n\t\t\t<div class=\"mt-2 text-sm grid grid-cols-2 gap-2\">\n\t\t\t\t<div>Updated: <span class=\"font-semibold\">{}</span></div>\n\t\t\t\t<div>Unchanged: <span class=\"font-semibold\">{}</span></div>\n\t\t\t</div>\n\t\t\t<div class=\"mt-2 text-sm grid grid-cols-3 gap-2\">\n\t\t\t\t<div>Pending: <span class=\"font-semibold\">{}</span></div>\n\t\t\t\t<div>Rejected: <span class=\"font-semibold\">{}</span></div>\n\t\t\t\t<div>Success: <span class=\"font-semibold\">{}</span></div>\n\t\t\t\t<div>Form required: <span class=\"font-semibold\">{}</span></div>\n\t\t\t\t<div>Confirm required: <span class=\"font-semibold\">{}</span></div>\n\t\t\t\t<div>Unknown: <span class=\"font-semibold\">{}</span></div>\n\t\t\t</div>\n\t\t\t<p class=\"mt-2 text-sm\">\n\t\t\t\t<a href=\"/tasks\" class=\"underline font-medium\">View action items</a> |\n\t\t\t\t<a href=\"/pipeline\" class=\"underline\" onclick=\"window.location.reload()\">Refresh page</a>\n\t\t\t</p>\n\t\t</div>\n\t",
            responses.len(),
            updated,
            unchanged,
            counts.pending,
            counts.rejected,
            counts.success,
            counts.form_required,
            counts.confirm_required,
            counts.unknown
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers() {
        assert_eq!(parse_id("12abc"), 12);
        assert_eq!(parse_id("abc"), 0);
        assert_eq!(sniff("<span>x</span>"), Some("text/plain; charset=utf-8"));
        assert_eq!(sniff("\n\t<div class=x>"), Some("text/html; charset=utf-8"));
        assert_eq!(
            sniff("<!DOCTYPE html>\n<html>"),
            Some("text/html; charset=utf-8")
        );
        assert_eq!(sniff(""), None);
        assert_eq!(
            html_escape(r#"<a href="x">'&"#),
            "&lt;a href=&#34;x&#34;&gt;&#39;&amp;"
        );
    }
}
