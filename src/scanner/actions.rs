//! User-triggered company actions: deletion requests and unsubscribes. Every attempt is logged
//! in company_actions; deletion requests also go to removal_requests like broker sends.

use super::company_broker;
use super::headers::parse_mailto;
use crate::config::Config;
use crate::email::{Message, SendResult, Sender, validate_email};
use crate::history::{Company, Record, Status, Store};
use crate::template::Engine;
use chrono::Local;
use std::time::Duration;

const HTTP_TIMEOUT: Duration = Duration::from_secs(10);
/// Covers the SMTP sender's own retries (3 tries of up to 25 s per command).
pub const SEND_TIMEOUT: Duration = Duration::from_secs(90);

/// What an unsubscribe would do for a company (shown on the confirmation page).
#[derive(Debug, Clone, PartialEq)]
pub enum Unsubscribe {
    /// RFC 8058 POST to this https URL.
    OneClick(String),
    /// Unsubscribe email to this mailto: URI.
    Mailto(String),
    /// Plain https link: never fetched automatically, the user opens it.
    Manual(String),
    None,
}

impl Unsubscribe {
    pub fn for_company(c: &Company) -> Self {
        let a = &c.agg;
        if a.one_click && a.unsub_https.starts_with("https://") {
            Unsubscribe::OneClick(a.unsub_https.clone())
        } else if parse_mailto(&a.unsub_mailto).is_some() {
            Unsubscribe::Mailto(a.unsub_mailto.clone())
        } else if a.unsub_https.starts_with("https://") {
            Unsubscribe::Manual(a.unsub_https.clone())
        } else {
            Unsubscribe::None
        }
    }

    /// (kind, target) for display.
    pub fn describe(&self) -> (&'static str, &str) {
        match self {
            Unsubscribe::OneClick(u) => ("one_click", u),
            Unsubscribe::Mailto(u) => ("mailto", u),
            Unsubscribe::Manual(u) => ("manual", u),
            Unsubscribe::None => ("none", ""),
        }
    }

    /// Whether executing it sends an email (counts against the daily limit).
    pub fn sends_email(&self) -> bool {
        matches!(self, Unsubscribe::Mailto(_))
    }
}

/// Recipient of a deletion request, or why there is none.
pub fn request_target(c: &Company) -> Result<String, &'static str> {
    if c.contact_email.is_empty() {
        return Err(if c.contact_status.is_empty() {
            "no privacy contact discovered yet"
        } else {
            "no published privacy address (manual)"
        });
    }
    validate_email(&c.contact_email).map_err(|_| "invalid contact address")?;
    Ok(c.contact_email.clone())
}

/// Client for one-click unsubscribes: no cookie store, ≤ 3 redirects, never https → http.
pub fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(super::discovery::USER_AGENT)
        .timeout(HTTP_TIMEOUT)
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            let downgrade = attempt.url().scheme() != "https"
                && attempt.previous().iter().any(|u| u.scheme() == "https");
            if attempt.previous().len() > 3 || downgrade {
                attempt.stop()
            } else {
                attempt.follow()
            }
        }))
        .build()
        .expect("static reqwest config")
}

/// RFC 8058 one-click: `POST List-Unsubscribe=One-Click` as a form body. Returns the HTTP status.
/// Retried only on connect errors (the POST never reached the server).
pub async fn one_click_post(client: &reqwest::Client, url: &str) -> anyhow::Result<u16> {
    let policy = crate::retry::Policy::new(3, Duration::from_millis(500));
    let resp = crate::retry::http(
        policy,
        || {
            client
                .post(url)
                .header(
                    reqwest::header::CONTENT_TYPE,
                    "application/x-www-form-urlencoded",
                )
                .body("List-Unsubscribe=One-Click")
        },
        false,
    )
    .await?;
    Ok(resp.status().as_u16())
}

/// The unsubscribe email for a mailto: URI (subject/body from the URI when given).
pub fn mailto_message(uri: &str, cfg: &Config) -> Option<Message> {
    let m = parse_mailto(uri)?;
    validate_email(&m.to).ok()?;
    Some(Message {
        to: m.to,
        from: cfg.email.from.clone(),
        subject: if m.subject.trim().is_empty() {
            "unsubscribe".into()
        } else {
            m.subject.replace(['\r', '\n'], " ")
        },
        body: if m.body.is_empty() {
            "Please unsubscribe me from this mailing list.".into()
        } else {
            m.body
        },
    })
}

async fn send(sender: &dyn Sender, msg: &Message) -> SendResult {
    match tokio::time::timeout(SEND_TIMEOUT, sender.send(msg, None)).await {
        Ok(r) => r,
        Err(_) => SendResult {
            success: false,
            message_id: String::new(),
            error: Some("send timed out".into()),
        },
    }
}

/// Sends one deletion request; records it in removal_requests (broker_id `company:<domain>`)
/// and company_actions. With `dry_run` only the action log gets a dry_run entry.
pub async fn send_request(
    store: &Store,
    engine: &Engine,
    sender: &dyn Sender,
    cfg: &Config,
    c: &Company,
    dry_run: bool,
) -> SendResult {
    let fail = |e: String| SendResult {
        success: false,
        message_id: String::new(),
        error: Some(e),
    };
    let domain = &c.agg.domain;
    let to = match request_target(c) {
        Ok(t) => t,
        Err(e) => {
            let _ = store.add_company_action(domain, "request", "", "failed", e);
            return fail(e.into());
        }
    };
    let broker = company_broker(c);
    let rendered = match engine.render(&cfg.options.template, &cfg.profile, &broker) {
        Ok(r) => r,
        Err(e) => {
            let _ = store.add_company_action(domain, "request", &to, "failed", &format!("{e:#}"));
            return fail(format!("template error: {e:#}"));
        }
    };
    if dry_run {
        let _ = store.add_company_action(domain, "request", &to, "dry_run", &rendered.subject);
        return SendResult {
            success: true,
            message_id: "dry-run".into(),
            error: None,
        };
    }
    let msg = Message {
        to: to.clone(),
        from: cfg.email.from.clone(),
        subject: rendered.subject,
        body: rendered.body,
    };
    // Crash-safe: the attempt is recorded as `pending` before sending and finalized after, so
    // a restart never sends it again (see Store::flag_interrupted_sends).
    let mut record = Record {
        broker_id: broker.id.clone(),
        broker_name: broker.name.clone(),
        email: to.clone(),
        template: rendered.template.clone(),
        status: Status::Pending,
        sent_at: Some(Local::now()),
        ..Default::default()
    };
    if let Err(e) = store.add(&mut record) {
        return fail(format!("history error, not sent: {e:#}"));
    }
    let result = send(sender, &msg).await;
    if result.success {
        let _ = store.mark_company(domain, "requested");
    }
    if let Err(e) = store.finish_send(record.id, &result) {
        tracing::warn!("failed to record request to {domain}: {e:#}");
    }
    let _ = store.add_company_action(
        domain,
        "request",
        &to,
        if result.success { "ok" } else { "failed" },
        &format!(
            "template {}; {}",
            rendered.template,
            result.error.clone().unwrap_or_default()
        ),
    );
    result
}

/// Executes the automatic unsubscribe for a company (one-click POST or mailto email).
/// Manual links are never fetched; they return an error telling the user to open them.
pub async fn unsubscribe(
    store: &Store,
    http: &reqwest::Client,
    sender: &dyn Sender,
    cfg: &Config,
    c: &Company,
    dry_run: bool,
) -> SendResult {
    let domain = &c.agg.domain;
    let plan = Unsubscribe::for_company(c);
    let (kind, target) = plan.describe();
    let action = format!("unsubscribe_{kind}");
    let done = |ok: bool, detail: String| {
        let status = match (dry_run, ok) {
            (true, _) => "dry_run",
            (false, true) => "ok",
            (false, false) => "failed",
        };
        let _ = store.add_company_action(domain, &action, target, status, &detail);
        if ok && !dry_run {
            let _ = store.mark_company(domain, "unsubscribed");
        }
        SendResult {
            success: ok,
            message_id: String::new(),
            error: (!ok).then_some(detail),
        }
    };
    match &plan {
        Unsubscribe::None => done(false, "no List-Unsubscribe target".into()),
        Unsubscribe::Manual(_) => done(
            false,
            "manual link: open it yourself and mark it done".into(),
        ),
        _ if dry_run => done(true, "dry run".into()),
        Unsubscribe::OneClick(url) => match one_click_post(http, url).await {
            Ok(code) if (200..300).contains(&code) => done(true, format!("HTTP {code}")),
            Ok(code) => done(false, format!("HTTP {code}")),
            Err(e) => done(false, format!("{e:#}")),
        },
        Unsubscribe::Mailto(uri) => match mailto_message(uri, cfg) {
            None => done(false, "invalid mailto URI".into()),
            Some(msg) => {
                let r = send(sender, &msg).await;
                done(
                    r.success,
                    r.error.unwrap_or_else(|| format!("sent to {}", msg.to)),
                )
            }
        },
    }
}

/// The user opened a manual unsubscribe link and confirms it worked.
pub fn mark_manual_unsubscribed(store: &Store, c: &Company) -> anyhow::Result<()> {
    store.add_company_action(
        &c.agg.domain,
        "unsubscribe_manual",
        &c.agg.unsub_https,
        "ok",
        "marked done by user",
    )?;
    store.mark_company(&c.agg.domain, "unsubscribed")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::CompanyAggregate;

    fn company(mailto: &str, https: &str, one_click: bool) -> Company {
        Company {
            agg: CompanyAggregate {
                domain: "shop.de".into(),
                unsub_mailto: mailto.into(),
                unsub_https: https.into(),
                one_click,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn unsubscribe_plan_order() {
        let p = |m, h, o| Unsubscribe::for_company(&company(m, h, o));
        assert_eq!(
            p("mailto:u@shop.de", "https://shop.de/u", true),
            Unsubscribe::OneClick("https://shop.de/u".into())
        );
        assert_eq!(
            p("mailto:u@shop.de", "https://shop.de/u", false),
            Unsubscribe::Mailto("mailto:u@shop.de".into())
        );
        assert_eq!(
            p("", "https://shop.de/u", false),
            Unsubscribe::Manual("https://shop.de/u".into())
        );
        assert_eq!(p("mailto:bogus", "", false), Unsubscribe::None);
    }

    #[test]
    fn mailto_message_defaults_and_params() {
        let mut cfg = Config::default();
        cfg.email.from = "me@example.com".into();
        let m = mailto_message("mailto:list@shop.de", &cfg).unwrap();
        assert_eq!(
            (m.to.as_str(), m.from.as_str(), m.subject.as_str()),
            ("list@shop.de", "me@example.com", "unsubscribe")
        );
        let m = mailto_message(
            "mailto:list@shop.de?subject=Remove%0D%0Ame&body=id%3D7",
            &cfg,
        )
        .unwrap();
        assert_eq!(
            (m.subject.as_str(), m.body.as_str()),
            ("Remove  me", "id=7")
        );
    }

    #[test]
    fn request_target_requires_discovered_address() {
        let mut c = company("", "", false);
        assert!(request_target(&c).is_err());
        c.contact_status = "manual".into();
        assert_eq!(
            request_target(&c),
            Err("no published privacy address (manual)")
        );
        c.contact_email = "privacy@shop.de".into();
        assert_eq!(request_target(&c).unwrap(), "privacy@shop.de");
    }
}
