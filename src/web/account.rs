//! The one mail account (Settings → Account, setup wizard step 3), inbox options and the
//! connection test (SMTP login + IMAP login, nothing sent).

use super::csrf::CsrfToken;
use super::handlers::{FormValues, St, fragment, html_escape, redirect, render, settings_render};
use crate::config::{self, AccountConfig, Config, PROVIDERS};
use crate::email::{SmtpSender, validate_email};
use crate::inbox::monitor;
use axum::Extension;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::Response;
use serde::Serialize;
use serde_json::json;
use std::collections::HashMap;

type Errors = HashMap<&'static str, &'static str>;

/// The account from a form; an empty password keeps `existing`'s.
fn account_from_form(f: &FormValues, existing: &AccountConfig) -> (AccountConfig, Errors) {
    let num = |k: &str| f.get(k).trim().parse::<i64>().unwrap_or(0);
    let mut a = AccountConfig {
        provider: f.get("provider").trim().to_string(),
        email: f.get("account_email").trim().to_string(),
        send_as: f.get("send_as").trim().to_string(),
        password: f.get("account_password").trim().to_string(),
        ..Default::default()
    };
    if a.provider.is_empty() {
        a.provider = "icloud".into();
    }
    if a.provider == "custom" {
        a.smtp_host = f.get("smtp_host").trim().to_string();
        a.smtp_port = num("smtp_port");
        a.imap_host = f.get("imap_host").trim().to_string();
        a.imap_port = num("imap_port");
    }
    if a.password.is_empty() {
        a.password.clone_from(&existing.password);
    }
    let mut errors = Errors::new();
    if !PROVIDERS.contains(&a.provider.as_str()) {
        errors.insert("provider", "Choose a provider");
    }
    if validate_email(&a.email).is_err() || !a.email.contains('@') {
        errors.insert("account_email", "Email address is required");
    }
    if !a.send_as.is_empty() && (validate_email(&a.send_as).is_err() || !a.send_as.contains('@')) {
        errors.insert("send_as", "Send as must be an email address");
    }
    if a.password.is_empty() {
        errors.insert("account_password", "App password is required");
    }
    if a.provider == "custom" && (a.smtp_host.is_empty() || a.smtp_port <= 0) {
        errors.insert("smtp_host", "SMTP server and port are required");
    }
    (a, errors)
}

/// The account as templates see it (never the password).
pub(super) fn account_view(a: &AccountConfig) -> serde_json::Value {
    let (smtp_host, smtp_port) = a.smtp_server();
    let (imap_host, imap_port) = a.imap_server();
    json!({
        "provider": if a.provider.is_empty() { "icloud" } else { a.provider.as_str() },
        "email": a.email,
        "send_as": a.send_as,
        "has_password": !a.password.is_empty(),
        "smtp_host": smtp_host,
        "smtp_port": smtp_port,
        "imap_host": imap_host,
        "imap_port": imap_port,
    })
}

#[derive(Serialize, Debug)]
pub(super) struct Check {
    name: &'static str,
    ok: bool,
    detail: String,
}

/// SMTP login and IMAP login + logout, in parallel, each bounded by `email::TEST_TIMEOUT`.
pub(super) async fn connection_checks(cfg: &Config) -> Vec<Check> {
    let smtp = SmtpSender::new(cfg.email.smtp.clone());
    let imap = async {
        if cfg.inbox.server.is_empty() || cfg.inbox.port <= 0 {
            return Err("no IMAP server configured".to_string());
        }
        monitor::test_login(cfg.inbox.clone())
            .await
            .map_err(|e| format!("{e:#}"))
    };
    let (smtp, imap) = tokio::join!(smtp.test_connection(), imap);
    let check = |name, r: Result<(), String>| Check {
        name,
        ok: r.is_ok(),
        detail: r.map_or_else(
            |e| alias_hint(&cfg.email.smtp.username, e),
            |()| "Login OK".into(),
        ),
    };
    vec![
        check("Sending (SMTP)", smtp),
        check("Reading replies (IMAP)", imap),
    ]
}

/// iCloud logs in with the primary address only; an alias fails with these errors.
fn alias_hint(login: &str, detail: String) -> String {
    let d = detail.to_lowercase();
    let apple = ["@icloud.com", "@me.com", "@mac.com"]
        .iter()
        .any(|s| login.to_lowercase().ends_with(s));
    let alias_error = d.contains("5.1.1")
        || d.contains("mailbox does not exist")
        || d.contains("authenticationfailed")
        || d.contains("authentication failed");
    if apple && alias_error {
        format!(
            "{detail}. If this address is an alias, enter your primary iCloud Mail address here and put the alias under Send as."
        )
    } else {
        detail
    }
}

fn checks_html(checks: &[Check]) -> String {
    let rows: String = checks
        .iter()
        .map(|c| {
            format!(
                r#"<li class="{}">{} {}: {}</li>"#,
                if c.ok { "text-success" } else { "text-error" },
                if c.ok { "✓" } else { "✗" },
                c.name,
                html_escape(&c.detail)
            )
        })
        .collect();
    format!(r#"<ul class="space-y-1 text-sm">{rows}</ul>"#)
}

/// POST /settings/account — saves the account and email.tracking, then tests the connection.
pub async fn settings_account(
    State(st): St,
    Extension(t): Extension<CsrfToken>,
    method: Method,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> Response {
    let f = FormValues::new(&method, &headers, &uri, &body);
    let existing = st.config().unwrap_or_default();
    let (account, errors) = account_from_form(&f, &existing.account);
    if !errors.is_empty() {
        let msg = errors.values().copied().collect::<Vec<_>>().join(". ");
        return settings_render(
            &st,
            &t,
            json!({ "account_message": msg, "account_success": false }),
        );
    }
    let (saved, cfg) = {
        let mut guard = st.config.write().unwrap();
        let cfg = guard.get_or_insert_with(Config::default);
        cfg.account = account;
        cfg.email.tracking = f.get("tracking") == "on";
        cfg.apply_account();
        (config::save(&st.config_path, cfg), cfg.clone())
    };
    if let Err(e) = saved {
        return settings_render(
            &st,
            &t,
            json!({ "account_message": format!("Failed to save configuration: {e:#}"), "account_success": false }),
        );
    }
    let checks = connection_checks(&cfg).await;
    settings_render(
        &st,
        &t,
        json!({ "account_message": "Account saved.", "account_success": true, "account_checks": checks }),
    )
}

/// POST /settings/account/test — tests the saved account (HTMX fragment).
pub async fn settings_account_test(State(st): St) -> Response {
    let Some(cfg) = st.config() else {
        return fragment(StatusCode::OK, "No account configured.");
    };
    fragment(StatusCode::OK, checks_html(&connection_checks(&cfg).await))
}

/// POST /settings/inbox — inbox options only; never touches the account.
pub async fn settings_inbox(
    State(st): St,
    Extension(t): Extension<CsrfToken>,
    method: Method,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> Response {
    let f = FormValues::new(&method, &headers, &uri, &body);
    let saved = {
        let mut guard = st.config.write().unwrap();
        let cfg = guard.get_or_insert_with(Config::default);
        let inbox = &mut cfg.inbox;
        inbox.enabled = f.get("enabled") == "on";
        inbox.auto_archive = f.get("auto_archive") == "on";
        inbox.folder = f.get("folder").trim().to_string();
        inbox.archive_folder = f.get("archive_folder").trim().to_string();
        inbox.scan_folders = f
            .get("scan_folders")
            .split([',', '\n'])
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect();
        config::apply_inbox_defaults(inbox);
        cfg.apply_account();
        config::save(&st.config_path, cfg)
    };
    let (msg, ok) = match saved {
        Ok(()) => ("Inbox options saved.".to_string(), true),
        Err(e) => (format!("Failed to save configuration: {e:#}"), false),
    };
    settings_render(
        &st,
        &t,
        json!({ "inbox_message": msg, "inbox_success": ok }),
    )
}

/// GET/POST /setup/email — wizard step 3: provider, email, app password.
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
    let page = |a: &AccountConfig, errors: Errors| {
        render(
            &st,
            &t,
            "setup/email.html",
            json!({
                "title": "Setup - Mail account", "step": "email", "profile": session.profile,
                "account": account_view(a), "errors": errors,
            }),
        )
    };
    if method != Method::POST {
        let mut a = session.account.clone();
        if a.email.is_empty() {
            a = st.config().map(|c| c.account).unwrap_or_default();
        }
        return page(&a, Errors::new());
    }
    let f = FormValues::new(&method, &headers, &uri, &body);
    let existing = if session.account.email.is_empty() {
        st.config().map(|c| c.account).unwrap_or_default()
    } else {
        session.account.clone()
    };
    let (a, errors) = account_from_form(&f, &existing);
    if !errors.is_empty() {
        return page(&a, errors);
    }
    st.sessions.update(&session.id, |s| {
        s.account = a;
        s.step = "test".into();
    });
    redirect(&method, "/setup/test")
}

/// POST /setup/test/connection — tests the wizard's account (HTMX fragment).
pub async fn setup_test_connection(State(st): St, headers: HeaderMap) -> Response {
    let Some(session) = st.session(&headers).filter(|s| !s.account.email.is_empty()) else {
        return fragment(
            StatusCode::BAD_REQUEST,
            r#"<div class="text-error">Mail account not configured. Please go back one step.</div>"#,
        );
    };
    let cfg = session.config();
    fragment(StatusCode::OK, checks_html(&connection_checks(&cfg).await))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn form(body: &str) -> FormValues {
        let mut h = HeaderMap::new();
        h.insert(
            axum::http::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded".parse().unwrap(),
        );
        FormValues::new(&Method::POST, &h, &Uri::from_static("/"), body.as_bytes())
    }

    #[test]
    fn account_form_keeps_password_and_validates() {
        let old = AccountConfig {
            password: "old-pw".into(),
            ..Default::default()
        };
        let (a, e) = account_from_form(
            &form("provider=gmail&account_email=a%40gmail.com&account_password="),
            &old,
        );
        assert!(e.is_empty(), "{e:?}");
        assert_eq!(
            (a.provider.as_str(), a.password.as_str()),
            ("gmail", "old-pw")
        );
        let (_, e) = account_from_form(
            &form("provider=custom&account_email=x"),
            &Default::default(),
        );
        assert!(e.contains_key("account_email") && e.contains_key("account_password"));
        assert!(e.contains_key("smtp_host"));
        let (a, e) = account_from_form(
            &form(
                "provider=custom&account_email=a%40x.org&account_password=p&smtp_host=mx.x.org&smtp_port=465&imap_host=mx.x.org&imap_port=993",
            ),
            &Default::default(),
        );
        assert!(e.is_empty());
        assert_eq!(a.send_as, "");
        assert_eq!(a.smtp_server(), ("mx.x.org".to_string(), 465));
        assert_eq!(a.imap_server(), ("mx.x.org".to_string(), 993));
        let (a, e) = account_from_form(
            &form("account_email=me%40icloud.com&send_as=alias%40me.com&account_password=p"),
            &Default::default(),
        );
        assert!(e.is_empty(), "{e:?}");
        assert_eq!(a.send_as, "alias@me.com");
        let (_, e) = account_from_form(
            &form("account_email=me%40icloud.com&send_as=nope&account_password=p"),
            &Default::default(),
        );
        assert!(e.contains_key("send_as"));
    }

    #[test]
    fn alias_hint_only_for_apple_alias_errors() {
        let smtp = "Rejected by mail server: 550 5.1.1 Mailbox does not exist".to_string();
        assert!(alias_hint("x@me.com", smtp.clone()).ends_with("put the alias under Send as."));
        assert!(
            alias_hint("x@icloud.com", "NO [AUTHENTICATIONFAILED] invalid".into())
                .contains("primary iCloud Mail address")
        );
        assert_eq!(alias_hint("x@gmail.com", smtp.clone()), smtp);
        assert_eq!(alias_hint("x@mac.com", "timeout".into()), "timeout");
    }

    #[tokio::test]
    async fn connection_checks_report_both_protocols() {
        let mut cfg = Config {
            account: AccountConfig {
                provider: "custom".into(),
                email: "a@x.org".into(),
                password: "p".into(),
                smtp_host: "127.0.0.1".into(),
                smtp_port: 1,
                ..Default::default()
            },
            ..Default::default()
        };
        cfg.apply_account();
        let checks = connection_checks(&cfg).await;
        assert_eq!(checks.len(), 2);
        assert!(
            !checks[0].ok && checks[0].detail.starts_with("SMTP error"),
            "{checks:?}"
        );
        assert_eq!(checks[1].detail, "no IMAP server configured");
        let html = checks_html(&checks);
        assert!(html.contains("✗ Sending (SMTP)"));
    }
}
