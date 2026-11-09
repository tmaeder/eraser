//! Privacy-contact discovery. Never guesses addresses: a contact is either the broker DB's
//! address or one actually published on the company's own website (with its source URL).
//!
//! Crawl etiquette: identifying User-Agent, robots.txt honoured, ≤ 6 fetches per company
//! (robots.txt included), 10 s per request, only the company's own host, 4 companies at a time.

use super::domain::{is_related_domain, registrable_domain};
use super::headers::parse_mailto;
use crate::broker::Broker;
use crate::history::Store;
use futures::StreamExt;
use regex::Regex;
use scraper::{Html, Node, Selector};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

pub const USER_AGENT: &str = concat!(
    "Eraser/",
    env!("CARGO_PKG_VERSION"),
    " (privacy-contact lookup for a GDPR/FADP request; +https://github.com/tmaeder/eraser)"
);
const ROBOTS_TOKEN: &str = "eraser";
const MAX_FETCHES: usize = 6;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_PAGE_BYTES: usize = 1 << 20;
pub const CONCURRENCY: usize = 4;

const COMMON_PATHS: &[&str] = &[
    "/privacy",
    "/privacy-policy",
    "/datenschutz",
    "/datenschutzerklaerung",
    "/legal/privacy",
    "/fr/confidentialite",
];
const POLICY_WORDS: &[&str] = &[
    "privacy",
    "datenschutz",
    "confidentialit",
    "privacidad",
    "data-protection",
    "data protection",
];
const IMPRINT_WORDS: &[&str] = &["impressum", "imprint"];
const PREFERRED_LOCAL: &[&str] = &[
    "privacy",
    "datenschutz",
    "dpo",
    "dataprotection",
    "data-protection",
    "data.protection",
    "gdpr",
    "dsgvo",
    "legal",
    "rgpd",
];
const REJECTED_LOCAL: &[&str] = &[
    "noreply",
    "no-reply",
    "donotreply",
    "do-not-reply",
    "mailer-daemon",
];

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Contact {
    /// "broker", "website" or "manual"
    pub status: &'static str,
    pub email: String,
    /// Page URL the address was found on, or `broker:<id>`.
    pub source: String,
}

impl Contact {
    fn manual() -> Self {
        Contact {
            status: "manual",
            ..Default::default()
        }
    }
}

/// (a) A broker whose email or website is on the company's registrable domain.
pub fn broker_match(domain: &str, brokers: &[Broker]) -> Option<Contact> {
    brokers.iter().find_map(|b| {
        let email_domain = b.email.rsplit_once('@').map(|(_, d)| registrable_domain(d));
        let site_domain = url::Url::parse(&b.website)
            .ok()
            .and_then(|u| u.host_str().map(registrable_domain));
        let hit = email_domain.as_deref() == Some(domain) || site_domain.as_deref() == Some(domain);
        (hit && b.email.contains('@')).then(|| Contact {
            status: "broker",
            email: b.email.to_ascii_lowercase(),
            source: format!("broker:{}", b.id),
        })
    })
}

// ==================== robots.txt ====================

#[derive(Debug, Default, Clone)]
pub struct Robots {
    /// (allow, pattern)
    rules: Vec<(bool, String)>,
}

impl Robots {
    /// Uses the groups naming our token, else the `*` groups (RFC 9309).
    pub fn parse(txt: &str, token: &str) -> Robots {
        let token = token.to_ascii_lowercase();
        let (mut ours, mut star) = (Vec::new(), Vec::new());
        let (mut agents, mut in_rules): (Vec<String>, bool) = (Vec::new(), false);
        let (mut has_ours, mut has_star) = (false, false);
        for line in txt.lines() {
            let line = line.split('#').next().unwrap_or_default().trim();
            let Some((k, v)) = line.split_once(':') else {
                continue;
            };
            let (k, v) = (k.trim().to_ascii_lowercase(), v.trim().to_string());
            match k.as_str() {
                "user-agent" => {
                    if in_rules {
                        agents.clear();
                        in_rules = false;
                    }
                    agents.push(v.to_ascii_lowercase());
                }
                "allow" | "disallow" => {
                    in_rules = true;
                    let rule = (k == "allow", v);
                    if agents
                        .iter()
                        .any(|a| a != "*" && token.contains(a.as_str()))
                    {
                        has_ours = true;
                        ours.push(rule);
                    } else if agents.iter().any(|a| a == "*") {
                        has_star = true;
                        star.push(rule);
                    }
                }
                _ => {}
            }
        }
        let rules = if has_ours {
            ours
        } else if has_star {
            star
        } else {
            Vec::new()
        };
        Robots {
            rules: rules.into_iter().filter(|(_, p)| !p.is_empty()).collect(),
        }
    }

    /// Longest matching rule wins; Allow wins ties; no match = allowed.
    pub fn allowed(&self, path: &str) -> bool {
        self.rules
            .iter()
            .filter(|(_, p)| pattern_matches(p, path))
            .max_by_key(|(allow, p)| (p.len(), *allow))
            .is_none_or(|(allow, _)| *allow)
    }
}

/// robots.txt pattern: prefix match with `*` wildcards and an optional `$` end anchor.
fn pattern_matches(pattern: &str, path: &str) -> bool {
    let (body, anchored) = match pattern.strip_suffix('$') {
        Some(p) => (p, true),
        None => (pattern, false),
    };
    let re: Vec<String> = body.split('*').map(regex::escape).collect();
    let re = format!("^{}{}", re.join(".*"), if anchored { "$" } else { "" });
    Regex::new(&re).is_ok_and(|r| r.is_match(path))
}

// ==================== email extraction ====================

static EMAIL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[A-Za-z0-9._%+-]+@[A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)*\.[A-Za-z]{2,}").unwrap()
});
// Only bracketed obfuscations are decoded: "name [at] example [dot] com". A bare " at " is
// ambiguous in prose and is never decoded.
static AT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\s*[\[\(\{]\s*(?:at|ät|@)\s*[\]\)\}]\s*").unwrap());
static DOT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\s*[\[\(\{]\s*(?:dot|punkt|point)\s*[\]\)\}]\s*").unwrap());

/// Visible text without script/style contents.
fn visible_text(doc: &Html) -> String {
    let mut out = String::new();
    for node in doc.tree.root().descendants() {
        if let Node::Text(t) = node.value() {
            let hidden = node
                .parent()
                .and_then(|p| p.value().as_element())
                .is_some_and(|e| matches!(e.name(), "script" | "style" | "noscript" | "template"));
            if !hidden {
                out.push_str(t);
                out.push(' ');
            }
        }
    }
    out
}

/// All email addresses on a page: mailto: links, plain text and bracketed obfuscations.
/// Lowercased, deduplicated, in page order.
pub fn extract_emails(html: &str) -> Vec<String> {
    let doc = Html::parse_document(html);
    let mut found: Vec<String> = Vec::new();
    let sel = Selector::parse("a[href]").unwrap();
    for a in doc.select(&sel) {
        let href = a.value().attr("href").unwrap_or_default().trim();
        if let Some(m) = parse_mailto(href) {
            found.push(m.to);
        }
    }
    let text = visible_text(&doc);
    let decoded = DOT_RE
        .replace_all(&AT_RE.replace_all(&text, "@"), ".")
        .into_owned();
    for t in [&text, &decoded] {
        found.extend(EMAIL_RE.find_iter(t).map(|m| m.as_str().to_string()));
    }
    let mut out: Vec<String> = Vec::new();
    for e in found {
        let e = e.trim_matches(['.', '-']).to_ascii_lowercase();
        let image = [".png", ".jpg", ".jpeg", ".gif", ".svg", ".webp"]
            .iter()
            .any(|x| e.ends_with(x));
        if !image && e.contains('@') && !out.contains(&e) {
            out.push(e);
        }
    }
    out
}

/// Keeps addresses on the company's (or a same-brand) domain, rejects no-reply addresses.
pub fn acceptable(email: &str, company: &str) -> bool {
    let Some((local, host)) = email.rsplit_once('@') else {
        return false;
    };
    !REJECTED_LOCAL.iter().any(|r| local.contains(r)) && is_related_domain(host, company)
}

pub fn is_preferred(email: &str) -> bool {
    let local = email.split('@').next().unwrap_or_default();
    PREFERRED_LOCAL.iter().any(|p| local.contains(p))
}

/// Picks the best address from (page url, emails) in crawl order: privacy-like local parts
/// first, then any other acceptable address.
pub fn pick_contact(company: &str, pages: &[(String, Vec<String>)]) -> Option<Contact> {
    let candidates = || {
        pages.iter().flat_map(|(url, emails)| {
            emails
                .iter()
                .filter(|e| acceptable(e, company))
                .map(move |e| (url, e))
        })
    };
    let (url, email) = candidates()
        .find(|(_, e)| is_preferred(e))
        .or_else(|| candidates().next())?;
    Some(Contact {
        status: "website",
        email: email.clone(),
        source: url.clone(),
    })
}

/// Policy links (priority 0) and imprint links (priority 2) on `base`'s host, as paths.
pub fn policy_links(html: &str, base: &url::Url) -> Vec<(u8, String)> {
    let doc = Html::parse_document(html);
    let sel = Selector::parse("a[href]").unwrap();
    let mut out: Vec<(u8, String)> = Vec::new();
    for a in doc.select(&sel) {
        let href = a.value().attr("href").unwrap_or_default();
        let Ok(u) = base.join(href) else { continue };
        if u.host_str() != base.host_str() || !matches!(u.scheme(), "http" | "https") {
            continue;
        }
        let hay = format!(
            "{} {}",
            u.path().to_lowercase(),
            a.text().collect::<String>().to_lowercase()
        );
        let prio = if POLICY_WORDS.iter().any(|w| hay.contains(w)) {
            0
        } else if IMPRINT_WORDS.iter().any(|w| hay.contains(w)) {
            2
        } else {
            continue;
        };
        let path = match u.query() {
            Some(q) => format!("{}?{q}", u.path()),
            None => u.path().to_string(),
        };
        if !out.iter().any(|(_, p)| *p == path) {
            out.push((prio, path));
        }
    }
    out
}

// ==================== crawler ====================

pub type OriginFn = Arc<dyn Fn(&str) -> String + Send + Sync>;

pub struct Discoverer {
    client: reqwest::Client,
    /// host → origin ("https://host"); tests point it at a local server.
    origin: OriginFn,
}

impl Discoverer {
    pub fn new() -> anyhow::Result<Self> {
        Self::with_origin(Arc::new(|h: &str| format!("https://{h}")))
    }

    pub fn with_origin(origin: OriginFn) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(REQUEST_TIMEOUT)
            .redirect(reqwest::redirect::Policy::limited(3))
            .build()?;
        Ok(Self { client, origin })
    }

    /// GET host+path; returns (final url, body) for 2xx text responses (capped at 1 MiB).
    async fn fetch(&self, host: &str, path: &str) -> Option<(url::Url, String)> {
        let url = format!("{}{path}", (self.origin)(host));
        // Host is the logical host (identical to the URL's host unless `origin` remaps it).
        let mut resp = self
            .client
            .get(&url)
            .header(reqwest::header::HOST, host)
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let ctype = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("text/html")
            .to_ascii_lowercase();
        if !ctype.starts_with("text/") {
            return None;
        }
        let final_url = resp.url().clone();
        let mut body = Vec::new();
        while let Ok(Some(chunk)) = resp.chunk().await {
            body.extend_from_slice(&chunk);
            if body.len() >= MAX_PAGE_BYTES {
                body.truncate(MAX_PAGE_BYTES);
                break;
            }
        }
        Some((final_url, String::from_utf8_lossy(&body).into_owned()))
    }

    /// (b) Crawls the company's website for a published privacy address.
    pub async fn crawl(&self, domain: &str) -> Option<Contact> {
        let mut budget = MAX_FETCHES;
        budget -= 1;
        let robots = match self.fetch(domain, "/robots.txt").await {
            Some((_, txt)) => Robots::parse(&txt, ROBOTS_TOKEN),
            None => Robots::default(),
        };
        let mut pages: Vec<(String, Vec<String>)> = Vec::new();
        let mut home = None;
        if robots.allowed("/") {
            for host in [domain.to_string(), format!("www.{domain}")] {
                if budget == 0 {
                    break;
                }
                budget -= 1;
                if let Some(page) = self.fetch(&host, "/").await {
                    home = Some((host, page));
                    break;
                }
            }
        }
        let host = home.as_ref().map_or(domain.to_string(), |(h, _)| h.clone());
        let mut queue: Vec<(u8, String)> = Vec::new();
        if let Some((_, (url, html))) = &home {
            pages.push((url.to_string(), extract_emails(html)));
            queue.extend(policy_links(html, url));
        }
        queue.extend(COMMON_PATHS.iter().map(|p| (1, p.to_string())));
        queue.sort_by_key(|(prio, _)| *prio); // stable: page order kept within a priority
        let mut seen = vec!["/".to_string()];
        for (_, path) in queue {
            if budget == 0
                || pages
                    .iter()
                    .flat_map(|p| &p.1)
                    .any(|e| acceptable(e, domain) && is_preferred(e))
            {
                break;
            }
            let norm = path.trim_end_matches('/').to_string();
            if seen.contains(&norm) || !robots.allowed(&path) {
                continue;
            }
            seen.push(norm);
            budget -= 1;
            if let Some((url, html)) = self.fetch(&host, &path).await {
                pages.push((url.to_string(), extract_emails(&html)));
            }
        }
        pick_contact(domain, &pages)
    }

    /// Broker DB first, then the website, else manual.
    pub async fn discover(&self, domain: &str, brokers: &[Broker]) -> Contact {
        if let Some(c) = broker_match(domain, brokers) {
            return c;
        }
        self.crawl(domain).await.unwrap_or_else(Contact::manual)
    }

    /// Discovers and stores contacts for `domains`, `CONCURRENCY` at a time.
    pub async fn discover_and_store(
        &self,
        store: &Store,
        brokers: &[Broker],
        domains: &[String],
    ) -> Vec<(String, Contact)> {
        let results: Vec<(String, Contact)> = futures::stream::iter(domains.iter().cloned())
            .map(|d| async move {
                let c = self.discover(&d, brokers).await;
                (d, c)
            })
            .buffer_unordered(CONCURRENCY)
            .collect()
            .await;
        for (d, c) in &results {
            if let Err(e) = store.set_company_contact(d, c.status, &c.email, &c.source) {
                tracing::warn!("failed to store contact for {d}: {e:#}");
            }
        }
        results
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn robots_groups_and_precedence() {
        let txt = "User-agent: *\nDisallow: /private\nAllow: /private/ok\n\nUser-agent: GoogleBot\nDisallow: /\n";
        let r = Robots::parse(txt, "eraser");
        assert!(r.allowed("/"));
        assert!(!r.allowed("/private/x"));
        assert!(r.allowed("/private/ok/page"), "longer allow wins");

        let txt = "User-agent: Eraser\nUser-agent: other\nDisallow: /datenschutz\n\nUser-agent: *\nDisallow: /\n";
        let r = Robots::parse(txt, "eraser");
        assert!(r.allowed("/"), "our group replaces *");
        assert!(!r.allowed("/datenschutz"));

        let r = Robots::parse("User-agent: *\nDisallow: /*.pdf$\nDisallow:\n", "eraser");
        assert!(!r.allowed("/files/a.pdf"));
        assert!(r.allowed("/files/a.pdf?x=1"));
        assert!(r.allowed("/privacy"));

        assert!(Robots::parse("garbage", "eraser").allowed("/x"));
        assert!(!Robots::parse("User-agent: *\nDisallow: /", "eraser").allowed("/privacy"));
    }

    const EN: &str = r#"<html><body><nav><a href="/en/privacy-policy">Privacy Policy</a>
        <a href="https://other.example/privacy">Their privacy</a></nav>
        <p>Questions? Write to <a href="mailto:privacy@shop.example?subject=Hi">our team</a>
        or support@shop.example. Analytics by tracker@onetrust.com.</p>
        <img src="logo@2x.png"><script>var x = "hidden@shop.example";</script></body></html>"#;

    const DE: &str = r#"<html><body><h1>Datenschutzerklärung</h1>
        <p>Verantwortlich: Beispiel GmbH, datenschutz [at] beispiel-shop [punkt] de</p>
        <p>Kontakt: info (at) beispiel-shop (dot) de</p>
        <p>Not decodable: max at beispiel-shop dot de</p>
        <a href="/impressum">Impressum</a></body></html>"#;

    const FR: &str = r#"<html><body><a href="/fr/confidentialite">Politique de confidentialité</a>
        <p>Délégué à la protection des données : dpo&#64;boutique.fr</p>
        <p>noreply@boutique.fr</p></body></html>"#;

    #[test]
    fn extraction_en() {
        let e = extract_emails(EN);
        assert_eq!(
            e,
            [
                "privacy@shop.example",
                "support@shop.example",
                "tracker@onetrust.com"
            ]
        );
        assert!(
            !e.iter().any(|x| x.starts_with("hidden")),
            "script text ignored"
        );
        let base = url::Url::parse("https://shop.example/").unwrap();
        assert_eq!(
            policy_links(EN, &base),
            [(0, "/en/privacy-policy".to_string())]
        );
        let c = pick_contact("shop.example", &[("https://shop.example/".into(), e)]).unwrap();
        assert_eq!(
            (c.status, c.email.as_str()),
            ("website", "privacy@shop.example")
        );
    }

    #[test]
    fn extraction_de_obfuscated() {
        let e = extract_emails(DE);
        assert_eq!(
            e,
            ["datenschutz@beispiel-shop.de", "info@beispiel-shop.de"],
            "bracketed [at]/(at) decoded, bare 'at' ignored"
        );
        let base = url::Url::parse("https://beispiel-shop.de/datenschutz").unwrap();
        assert_eq!(policy_links(DE, &base), [(2, "/impressum".to_string())]);
    }

    #[test]
    fn extraction_fr_and_filtering() {
        let e = extract_emails(FR);
        assert_eq!(e, ["dpo@boutique.fr", "noreply@boutique.fr"]);
        assert!(!acceptable("noreply@boutique.fr", "boutique.fr"));
        assert!(acceptable("dpo@boutique.fr", "boutique.fr"));
        assert!(
            acceptable("privacy@boutique.ch", "boutique.fr"),
            "same brand"
        );
        assert!(!acceptable("dpo@onetrust.com", "boutique.fr"));
        // only foreign addresses: no contact
        assert_eq!(
            pick_contact(
                "boutique.fr",
                &[("u".into(), vec!["x@onetrust.com".into()])]
            ),
            None
        );
        // a privacy address on a later page beats a generic one found first
        let c = pick_contact(
            "boutique.fr",
            &[
                ("a".into(), vec!["hello@boutique.fr".into()]),
                ("b".into(), vec!["dpo@boutique.fr".into()]),
            ],
        )
        .unwrap();
        assert_eq!(
            (c.email.as_str(), c.source.as_str()),
            ("dpo@boutique.fr", "b")
        );
    }

    #[test]
    fn broker_db_match() {
        let brokers = [Broker {
            id: "acme".into(),
            email: "Privacy@ACME.com".into(),
            website: "https://www.acme-data.com".into(),
            ..Default::default()
        }];
        let c = broker_match("acme-data.com", &brokers).unwrap();
        assert_eq!(
            (c.status, c.email.as_str(), c.source.as_str()),
            ("broker", "privacy@acme.com", "broker:acme")
        );
        assert!(broker_match("acme.com", &brokers).is_some());
        assert!(broker_match("other.com", &brokers).is_none());
    }
}
