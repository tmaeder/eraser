//! Embedded minijinja templates and static assets, plus the template filters (dates, URL
//! attribute filtering, number formatting).

use chrono::DateTime;
use minijinja::value::Value;
use minijinja::{Environment, UndefinedBehavior};

macro_rules! embed {
    ($dir:literal: $($name:literal),* $(,)?) => {
        &[$(($name, include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/", $dir, "/", $name)))),*]
    };
}

const TEMPLATES: &[(&str, &str)] = embed!("templates/web":
    "layout.html", "dashboard.html", "brokers.html", "history.html", "settings.html",
    "pipeline.html", "tasks.html", "task-detail.html", "task-helper.html",
    "setup/welcome.html", "setup/profile.html", "setup/email.html",
    "setup/test.html", "setup/complete.html", "partials/broker-list.html",
    "partials/history-list.html", "partials/response-list.html", "partials/task-list.html",
    "companies.html", "company-detail.html", "companies-confirm.html", "today.html",
    "partials/status.html", "broker-detail.html", "config-locked.html",
);

pub const STATIC_FILES: &[(&str, &[u8], &str)] = &[(
    "js/htmx.min.js",
    include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/static/js/htmx.min.js"
    )),
    "text/javascript; charset=utf-8",
)];

/// RFC 3339 string → `fmt`; missing or unparsable values render empty.
fn format_rfc3339(value: &Value, fmt: &str) -> String {
    value
        .as_str()
        .and_then(|raw| DateTime::parse_from_rfc3339(raw).ok())
        .map(|t| t.format(fmt).to_string())
        .unwrap_or_default()
}

/// "Jan 2, 2006 3:04 PM"
fn format_time(value: Value) -> String {
    format_rfc3339(&value, "%b %-d, %Y %-I:%M %p")
}

/// "Jan 2, 2006"
fn format_date(value: Value) -> String {
    format_rfc3339(&value, "%b %-d, %Y")
}

/// Floats without a trailing ".0" for integral values.
fn number(value: Value) -> String {
    match f64::try_from(value.clone()) {
        Ok(f) => format!("{f}"),
        Err(_) => value.to_string(),
    }
}

/// URL attributes: unsafe schemes become "#", and characters outside the URL set are
/// percent-encoded (the result is then HTML-escaped as usual).
pub fn safe_url(value: Value) -> String {
    let raw = value.to_string();
    if let Some((scheme, _)) = raw.split_once(':')
        && !scheme.contains('/')
        && !["http", "https", "mailto"].contains(&scheme.to_ascii_lowercase().as_str())
    {
        return "#".into();
    }
    let mut out = String::with_capacity(raw.len());
    for b in raw.bytes() {
        if b.is_ascii_alphanumeric() || b"!#$&*+,/:;=?@[]-._~%".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

pub fn environment() -> Environment<'static> {
    let mut env = Environment::new();
    env.set_syntax(
        minijinja::syntax::SyntaxConfig::builder()
            .keep_trailing_newline(true)
            .build()
            .expect("valid syntax config"),
    );
    // Missing keys / null fields render as empty values.
    env.set_undefined_behavior(UndefinedBehavior::Chainable);
    env.add_filter("format_time", format_time);
    env.add_filter("format_date", format_date);
    env.add_filter("number", number);
    env.add_filter("safe_url", safe_url);
    for (name, src) in TEMPLATES {
        env.add_template(name, src)
            .unwrap_or_else(|e| panic!("template {name}: {e}"));
    }
    env
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn templates_compile_and_filters() {
        let env = environment();
        assert!(env.get_template("partials/task-list.html").is_ok());
        assert_eq!(
            format_time(Value::from("2026-03-14T09:26:53.5+01:00")),
            "Mar 14, 2026 9:26 AM"
        );
        assert_eq!(format_date(Value::from(())), "");
        assert_eq!(number(Value::from(85.0)), "85");
        assert_eq!(number(Value::from(0.5)), "0.5");
        assert_eq!(safe_url(Value::from("javascript:alert(1)")), "#");
        assert_eq!(
            safe_url(Value::from("https://x.com/a b?q=1")),
            "https://x.com/a%20b?q=1"
        );
        assert_eq!(safe_url(Value::from("/rel/path")), "/rel/path");
    }

    #[test]
    fn status_chip_macro_has_icon_and_text_for_every_kind() {
        let env = environment();
        let render = |kind: &str, label: &str| {
            env.render_named_str(
                "chip.html",
                r#"{% from "partials/status.html" import chip %}{{ chip({"kind": k, "label": l}) }}"#,
                minijinja::context! { k => kind, l => label },
            )
            .unwrap()
        };
        let mut paths = std::collections::HashSet::new();
        for kind in [
            "deleted",
            "waiting",
            "reminder",
            "rejected",
            "action",
            "failed",
            "interrupted",
            "opened",
            "receipt",
            "none",
        ] {
            let html = render(kind, "Label <x>");
            assert!(
                html.starts_with(&format!(r#"<span class="chip chip-{kind}">"#)),
                "{html}"
            );
            assert!(html.contains(r#"aria-hidden="true""#) && html.contains("Label &lt;x&gt;"));
            let d = html
                .split(" d=\"")
                .nth(1)
                .unwrap()
                .split('"')
                .next()
                .unwrap()
                .to_string();
            paths.insert(d);
        }
        assert_eq!(paths.len(), 10, "one distinct icon per status");
    }
}
