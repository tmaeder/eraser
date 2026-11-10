//! Port of internal/template/template.go. Templates are embedded minijinja ports of the Go
//! text/template files and render byte-identical output.

use crate::broker::Broker;
use crate::config::Profile;
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Datelike, Local, TimeZone};
use serde::Serialize;

const TEMPLATES: [(&str, &str); 4] = [
    ("gdpr", include_str!("../templates/email/gdpr.txt")),
    ("ccpa", include_str!("../templates/email/ccpa.txt")),
    ("fadp", include_str!("../templates/email/fadp.txt")),
    ("generic", include_str!("../templates/email/generic.txt")),
];

const REMINDER: &str = include_str!("../templates/email/reminder.txt");

/// All data available to email templates (field names match the Go struct).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct EmailData {
    pub first_name: String,
    pub last_name: String,
    pub full_name: String,
    pub email: String,
    pub address: String,
    pub city: String,
    pub state: String,
    pub zip_code: String,
    pub country: String,
    pub phone: String,
    pub date_of_birth: String,

    pub broker_name: String,
    pub broker_email: String,
    pub broker_website: String,
    pub broker_opt_out: String,

    pub date: String,
    pub year: i32,
    pub month: String,
    pub template: String,
}

/// A rendered email ready to send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Email {
    pub subject: String,
    pub body: String,
    /// Template actually used (resolved when "auto").
    pub template: String,
}

pub struct Engine {
    env: minijinja::Environment<'static>,
}

impl Engine {
    pub fn new() -> Result<Self> {
        let mut env = minijinja::Environment::new();
        let syntax = minijinja::syntax::SyntaxConfig::builder()
            .keep_trailing_newline(true) // Go text/template keeps it
            .build()?;
        env.set_syntax(syntax);
        env.set_auto_escape_callback(|_| minijinja::AutoEscape::None);
        for (name, src) in TEMPLATES {
            env.add_template(name, src)
                .with_context(|| format!("failed to parse template {name}"))?;
        }
        env.add_template("reminder", REMINDER)
            .context("failed to parse template reminder")?;
        Ok(Self { env })
    }

    pub fn render(&self, template_name: &str, profile: &Profile, broker: &Broker) -> Result<Email> {
        self.render_at(template_name, profile, broker, Local::now())
    }

    /// Same as [`Engine::render`] with an explicit "now" (for deterministic output).
    pub fn render_at<Tz: TimeZone>(
        &self,
        template_name: &str,
        profile: &Profile,
        broker: &Broker,
        now: DateTime<Tz>,
    ) -> Result<Email>
    where
        Tz::Offset: std::fmt::Display,
    {
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
        self.render_reminder_at(
            template_name,
            profile,
            broker,
            original,
            days_since,
            Local::now(),
        )
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
    }

    pub fn available_templates(&self) -> Vec<String> {
        TEMPLATES.iter().map(|(n, _)| n.to_string()).collect()
    }
}

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

/// Resolves "auto" to the law that binds the broker: Swiss FADP for Swiss brokers,
/// GDPR for EU/UK brokers, the multi-law generic letter otherwise.
pub fn for_broker<'a>(template_name: &'a str, broker: &Broker) -> &'a str {
    if template_name != "auto" {
        return template_name;
    }
    match broker.region.to_lowercase().as_str() {
        "ch" => "fadp",
        "eu" => "gdpr",
        _ => "generic",
    }
}

fn subject(template_name: &str) -> &'static str {
    match template_name {
        "gdpr" => "Erasure request and objection under Articles 17 and 21 GDPR",
        "ccpa" => "CCPA Request to Delete and to Opt Out of Sale/Sharing",
        "fadp" => "Deletion request and objection under Articles 30 and 32 FADP (DSG)",
        _ => "Personal Data Deletion and Opt-Out Request",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use std::path::Path;

    pub(crate) fn full_profile() -> Profile {
        Profile {
            first_name: "Jane".into(),
            last_name: "Döe".into(),
            email: "jane@example.com".into(),
            address: "1 <Main> & St".into(),
            city: "Zürich".into(),
            state: "ZH".into(),
            zip_code: "8001".into(),
            country: "Switzerland".into(),
            phone: "+41 44 000 00 00".into(),
            date_of_birth: "1990-01-15".into(),
        }
    }

    fn minimal_profile() -> Profile {
        Profile {
            first_name: "Min".into(),
            last_name: "Imal".into(),
            email: "min@example.com".into(),
            ..Default::default()
        }
    }

    fn broker() -> Broker {
        Broker {
            id: "acme".into(),
            name: "Acme \"Data\" Inc".into(),
            email: "privacy@acme.com".into(),
            website: "https://acme.com".into(),
            opt_out_url: "https://acme.com/optout".into(),
            region: "us".into(),
            ..Default::default()
        }
    }

    /// Golden files produced by the Go Engine (date replaced by `__DATE__`).
    #[test]
    fn matches_go_output_byte_for_byte() {
        let engine = Engine::new().unwrap();
        let now = Utc.with_ymd_and_hms(2026, 1, 5, 12, 0, 0).unwrap();
        let golden = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/template");
        for name in ["gdpr", "ccpa", "fadp", "generic"] {
            for (pname, profile) in [("full", full_profile()), ("minimal", minimal_profile())] {
                let got = engine.render_at(name, &profile, &broker(), now).unwrap();
                let want = std::fs::read_to_string(golden.join(format!("{name}_{pname}.txt")))
                    .unwrap()
                    .replace("__DATE__", "January 5, 2026");
                assert_eq!(got.body, want, "{name}/{pname}");
                let want_subject =
                    std::fs::read_to_string(golden.join(format!("{name}_subject.txt"))).unwrap();
                assert_eq!(got.subject, want_subject);
            }
        }
    }

    #[test]
    fn unknown_template_and_list() {
        let engine = Engine::new().unwrap();
        let err = engine
            .render("nope", &full_profile(), &broker())
            .unwrap_err();
        assert_eq!(err.to_string(), "unknown template: nope");
        assert_eq!(
            engine.available_templates(),
            ["gdpr", "ccpa", "fadp", "generic"]
        );
    }

    #[test]
    fn auto_picks_law_by_broker_region() {
        let engine = Engine::new().unwrap();
        for (region, want) in [
            ("ch", "fadp"),
            ("CH", "fadp"),
            ("eu", "gdpr"),
            ("us", "generic"),
            ("global", "generic"),
        ] {
            let b = Broker {
                region: region.into(),
                ..broker()
            };
            assert_eq!(
                engine
                    .render("auto", &minimal_profile(), &b)
                    .unwrap()
                    .template,
                want
            );
        }
        let ch = Broker {
            region: "ch".into(),
            ..broker()
        };
        assert_eq!(for_broker("ccpa", &ch), "ccpa");
    }

    #[test]
    fn subjects_are_ascii() {
        for (name, _) in TEMPLATES {
            assert!(subject(name).is_ascii(), "{name}");
        }
    }
    #[test]
    fn reminder_cites_original_date_deadline_and_law() {
        let engine = Engine::new().unwrap();
        let now = Utc.with_ymd_and_hms(2026, 2, 10, 12, 0, 0).unwrap();
        let original = Local.with_ymd_and_hms(2026, 1, 5, 12, 0, 0).unwrap();
        for (region, law, cite) in [
            ("eu", "gdpr", "Article 12(3) GDPR you must act"),
            (
                "ch",
                "fadp",
                "Art. 18 para. 1 of the Data Protection Ordinance",
            ),
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
            assert_eq!(
                r.subject,
                "Reminder: data deletion request of January 5, 2026"
            );
            assert!(r.subject.is_ascii());
            assert!(
                r.body.contains("On January 5, 2026 I sent you"),
                "{}",
                r.body
            );
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
}
