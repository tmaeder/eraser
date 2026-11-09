//! Inbox company scanner: header-only IMAP scan → per-company aggregates (grouped by
//! registrable sender domain) → privacy-contact discovery → user-triggered deletion requests
//! and unsubscribes.

pub mod actions;
pub mod discovery;
pub mod domain;
pub mod headers;

use crate::broker::Broker;
use crate::config::{Config, InboxConfig};
use crate::history::{CompanyAggregate, ScannedMessage, Store};
use crate::inbox::monitor::Monitor;
use anyhow::Result;
use std::collections::{BTreeSet, HashMap, HashSet};

pub const DEFAULT_DAYS: i64 = 365;
/// Folder names scanned by default when they exist (compared case-insensitively, last path part).
const EXTRA_FOLDERS: &[&str] = &["archive", "newsletter", "newsletters", "promotions"];
/// Messages per scan window (keeps a web request well under proxy timeouts).
pub const WINDOW: usize = 1000;
pub const BROKER_PREFIX: &str = "company:";

/// The user's own addresses and (non-freemail) domains, excluded from the company list.
#[derive(Debug, Clone, Default)]
pub struct Identity {
    pub addresses: HashSet<String>,
    pub domains: HashSet<String>,
}

impl Identity {
    pub fn from_config(cfg: &Config) -> Self {
        let mut id = Identity::default();
        for a in [
            &cfg.profile.email,
            &cfg.email.from,
            &cfg.inbox.email,
            &cfg.email.smtp.username,
        ] {
            let a = a.trim().to_ascii_lowercase();
            let Some((_, host)) = a.rsplit_once('@') else {
                continue;
            };
            let d = domain::registrable_domain(host);
            if !domain::is_personal_domain(&d) {
                id.domains.insert(d);
            }
            id.addresses.insert(a);
        }
        id
    }
}

/// Why a company is hidden by default ("" = shown).
pub fn exclusion_reason(agg: &CompanyAggregate, id: &Identity) -> &'static str {
    if id.domains.contains(&agg.domain) {
        "own"
    } else if domain::is_personal_domain(&agg.domain) && !agg.is_mailing_list {
        "personal_domain"
    } else {
        ""
    }
}

/// Aggregates one domain's messages.
pub fn aggregate(domain: &str, msgs: &[ScannedMessage]) -> CompanyAggregate {
    let mut names: HashMap<&str, usize> = HashMap::new();
    for m in msgs.iter().filter(|m| !m.from_name.is_empty()) {
        *names.entry(m.from_name.as_str()).or_default() += 1;
    }
    let display_name = names
        .into_iter()
        .max_by(|a, b| a.1.cmp(&b.1).then_with(|| b.0.cmp(a.0)))
        .map(|(n, _)| n.to_string())
        .unwrap_or_default();
    let mut by_date: Vec<&ScannedMessage> = msgs.iter().collect();
    by_date.sort_by_key(|m| std::cmp::Reverse(m.date));
    let newest_https = by_date.iter().find(|m| !m.unsub_https.is_empty());
    CompanyAggregate {
        domain: domain.to_string(),
        display_name,
        senders: msgs
            .iter()
            .map(|m| m.from_addr.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect(),
        message_count: msgs.len() as i64,
        first_seen: msgs.iter().filter_map(|m| m.date).min(),
        last_seen: msgs.iter().filter_map(|m| m.date).max(),
        is_mailing_list: msgs.iter().any(|m| m.is_list),
        unsub_mailto: by_date
            .iter()
            .find(|m| !m.unsub_mailto.is_empty())
            .map(|m| m.unsub_mailto.clone())
            .unwrap_or_default(),
        unsub_https: newest_https
            .map(|m| m.unsub_https.clone())
            .unwrap_or_default(),
        one_click: newest_https.is_some_and(|m| m.one_click),
    }
}

/// Recomputes the stored aggregates of `domains` from scanned_messages.
pub fn recompute(store: &Store, id: &Identity, domains: &BTreeSet<String>) -> Result<()> {
    for d in domains {
        let msgs = store.scanned_messages_for(d)?;
        if msgs.is_empty() {
            continue;
        }
        let agg = aggregate(d, &msgs);
        store.upsert_company(&agg, exclusion_reason(&agg, id))?;
    }
    Ok(())
}

/// Folders to scan: explicit list, else the configured `scan_folders`, else the inbox folder,
/// "Archive", the archive folder and any existing Archive/Newsletter/Promotions folders.
/// Only folders that exist on the server are returned.
pub fn pick_folders(existing: &[String], cfg: &InboxConfig, explicit: &[String]) -> Vec<String> {
    let wanted: Vec<String> = if !explicit.is_empty() {
        explicit.to_vec()
    } else if !cfg.scan_folders.is_empty() {
        cfg.scan_folders.clone()
    } else {
        let mut w = vec![
            if cfg.folder.is_empty() {
                "INBOX".to_string()
            } else {
                cfg.folder.clone()
            },
            "Archive".into(),
            cfg.archive_folder.clone(),
        ];
        w.extend(existing.iter().filter(|f| {
            let leaf = f.rsplit(['/', '.']).next().unwrap_or(f).to_lowercase();
            EXTRA_FOLDERS.contains(&leaf.as_str())
        }).cloned());
        w
    };
    let mut out: Vec<String> = Vec::new();
    for w in wanted.into_iter().filter(|w| !w.is_empty()) {
        if let Some(found) = existing.iter().find(|e| e.eq_ignore_ascii_case(&w))
            && !out.contains(found)
        {
            out.push(found.clone());
        }
    }
    out
}

#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct WindowResult {
    /// Messages in range in this folder (all windows).
    pub total: usize,
    pub fetched: usize,
    pub new: usize,
}

/// Scans one window of one folder and updates the affected companies.
pub async fn scan_window(
    monitor: &mut Monitor,
    store: &Store,
    id: &Identity,
    folder: &str,
    days: i64,
    offset: usize,
) -> Result<WindowResult> {
    let batch = monitor
        .fetch_scan_headers(folder, days, offset, WINDOW)
        .await?;
    let msgs: Vec<ScannedMessage> = batch
        .headers
        .iter()
        .filter_map(|(uid, raw)| {
            headers::parse_header_block(raw, &format!("{folder}/{}/{uid}", batch.uid_validity))
        })
        .filter(|h| !id.addresses.contains(&h.from_addr))
        .map(|h| ScannedMessage {
            key: h.key,
            domain: h.domain,
            from_addr: h.from_addr,
            from_name: h.from_name,
            date: h.date,
            is_list: h.is_list,
            unsub_mailto: h.unsub_mailto,
            unsub_https: h.unsub_https,
            one_click: h.one_click,
        })
        .collect();
    let new = store.add_scanned_messages(&msgs)?;
    let domains: BTreeSet<String> = msgs.iter().map(|m| m.domain.clone()).collect();
    recompute(store, id, &domains)?;
    Ok(WindowResult {
        total: batch.total,
        fetched: batch.headers.len(),
        new,
    })
}

/// Full scan of `folders` (connect/disconnect is the caller's job). `progress(folder, done, total)`.
pub async fn scan_folders(
    monitor: &mut Monitor,
    store: &Store,
    id: &Identity,
    folders: &[String],
    days: i64,
    mut progress: impl FnMut(&str, usize, usize),
) -> Result<WindowResult> {
    let mut sum = WindowResult::default();
    for folder in folders {
        let mut offset = 0;
        loop {
            let r = scan_window(monitor, store, id, folder, days, offset).await?;
            offset += WINDOW;
            sum.fetched += r.fetched;
            sum.new += r.new;
            progress(folder, offset.min(r.total), r.total);
            if offset >= r.total {
                sum.total += r.total;
                break;
            }
        }
    }
    Ok(sum)
}

/// The synthetic broker a company deletion request is sent as.
pub fn company_broker(c: &crate::history::Company) -> Broker {
    Broker {
        id: format!("{BROKER_PREFIX}{}", c.agg.domain),
        name: if c.agg.display_name.is_empty() {
            c.agg.domain.clone()
        } else {
            c.agg.display_name.clone()
        },
        email: c.contact_email.clone(),
        website: format!("https://{}", c.agg.domain),
        region: domain::tld_region(&c.agg.domain).into(),
        category: "company".into(),
        ..Default::default()
    }
}

/// Brokers plus every company a deletion request went to, so the inbox monitor attributes
/// their replies (by contact-address domain and company domain).
pub fn inbox_brokers(brokers: &[Broker], store: &Store) -> Vec<Broker> {
    let mut all = brokers.to_vec();
    if let Ok(companies) = store.list_companies() {
        all.extend(
            companies
                .iter()
                .filter(|c| c.requested_at.is_some() && !c.contact_email.is_empty())
                .map(company_broker),
        );
    }
    all
}

#[cfg(test)]
mod tests;
