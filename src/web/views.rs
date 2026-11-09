//! Embedded minijinja ports of internal/web/templates and static assets, plus the template
//! filters standing in for the Go template funcs (formatTime, formatDate) and for html/template's
//! contextual behaviour that minijinja lacks (URL attribute filtering, %v float formatting).

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
    "pipeline.html", "tasks.html", "task-detail.html", "task-helper.html", "forms.html",
    "setup.html", "setup/welcome.html", "setup/profile.html", "setup/email.html",
    "setup/test.html", "setup/complete.html", "partials/broker-list.html",
    "partials/history-list.html", "partials/response-list.html", "partials/task-list.html",
);

pub const STATIC_FILES: &[(&str, &[u8], &str)] = &[
    (
        "css/tailwind.min.css",
        include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/static/css/tailwind.min.css"
        )),
        "text/css; charset=utf-8",
    ),
    (
        "js/htmx.min.js",
        include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/static/js/htmx.min.js"
        )),
        "text/javascript; charset=utf-8",
    ),
];

const GO_ZERO_TIME: &str = "0001-01-01T00:00:00Z";

fn go_format(value: &Value, fmt: &str) -> String {
    let raw = value.as_str().unwrap_or(GO_ZERO_TIME);
    match DateTime::parse_from_rfc3339(raw) {
        Ok(t) => t.format(fmt).to_string(),
        Err(_) => String::new(),
    }
}

/// Go "Jan 2, 2006 3:04 PM"
fn format_time(value: Value) -> String {
    go_format(&value, "%b %-d, %Y %-I:%M %p")
}

/// Go "Jan 2, 2006"
fn format_date(value: Value) -> String {
    go_format(&value, "%b %-d, %Y")
}

/// Go's `%v` for float64 (no trailing ".0" for integral values).
fn gofloat(value: Value) -> String {
    match f64::try_from(value.clone()) {
        Ok(f) => format!("{f}"),
        Err(_) => value.to_string(),
    }
}

/// html/template's URL attribute handling: unsafe schemes become "#ZgotmplZ", and characters
/// outside the URL set are percent-encoded (the result is then HTML-escaped as usual).
pub fn safe_url(value: Value) -> String {
    let raw = value.to_string();
    if let Some((scheme, _)) = raw.split_once(':')
        && !scheme.contains('/')
        && !["http", "https", "mailto"].contains(&scheme.to_ascii_lowercase().as_str())
    {
        return "#ZgotmplZ".into();
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
    // Go templates render missing map keys / nil fields as empty values.
    env.set_undefined_behavior(UndefinedBehavior::Chainable);
    env.add_filter("format_time", format_time);
    env.add_filter("format_date", format_date);
    env.add_filter("gofloat", gofloat);
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
        assert_eq!(format_date(Value::from(())), "Jan 1, 0001");
        assert_eq!(gofloat(Value::from(85.0)), "85");
        assert_eq!(gofloat(Value::from(0.5)), "0.5");
        assert_eq!(safe_url(Value::from("javascript:alert(1)")), "#ZgotmplZ");
        assert_eq!(
            safe_url(Value::from("https://x.com/a b?q=1")),
            "https://x.com/a%20b?q=1"
        );
        assert_eq!(safe_url(Value::from("/rel/path")), "/rel/path");
    }
}
