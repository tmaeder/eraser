//! Company scanner tables: header facts per message, derived per-company aggregates with the
//! user's decisions and discovered privacy contacts, and an action log.
//! Timestamps in these tables are RFC 3339 UTC strings (new tables, no Go compatibility needed).

use super::Store;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{OptionalExtension, Row, params};
use serde::Serialize;

pub(super) const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS scanned_messages (
    msg_key TEXT PRIMARY KEY,
    domain TEXT NOT NULL,
    from_addr TEXT NOT NULL,
    from_name TEXT NOT NULL DEFAULT '',
    sent_at TEXT,
    is_list INTEGER NOT NULL DEFAULT 0,
    unsub_mailto TEXT NOT NULL DEFAULT '',
    unsub_https TEXT NOT NULL DEFAULT '',
    one_click INTEGER NOT NULL DEFAULT 0,
    scanned_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_sm_domain ON scanned_messages(domain);

CREATE TABLE IF NOT EXISTS scanned_companies (
    domain TEXT PRIMARY KEY,
    display_name TEXT NOT NULL DEFAULT '',
    senders TEXT NOT NULL DEFAULT '[]',
    message_count INTEGER NOT NULL DEFAULT 0,
    first_seen TEXT,
    last_seen TEXT,
    is_mailing_list INTEGER NOT NULL DEFAULT 0,
    unsub_mailto TEXT NOT NULL DEFAULT '',
    unsub_https TEXT NOT NULL DEFAULT '',
    one_click INTEGER NOT NULL DEFAULT 0,
    excluded_reason TEXT NOT NULL DEFAULT '',
    contact_status TEXT NOT NULL DEFAULT '',
    contact_email TEXT NOT NULL DEFAULT '',
    contact_source TEXT NOT NULL DEFAULT '',
    contact_checked_at TEXT,
    decision TEXT NOT NULL DEFAULT '',
    requested_at TEXT,
    unsubscribed_at TEXT,
    updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS company_actions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    domain TEXT NOT NULL,
    action TEXT NOT NULL,
    target TEXT NOT NULL DEFAULT '',
    status TEXT NOT NULL,
    detail TEXT NOT NULL DEFAULT '',
    created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_ca_domain ON company_actions(domain);
"#;

/// One stored message's header facts (input to the per-company aggregation).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ScannedMessage {
    pub key: String,
    pub domain: String,
    pub from_addr: String,
    pub from_name: String,
    pub date: Option<DateTime<Utc>>,
    pub is_list: bool,
    pub unsub_mailto: String,
    pub unsub_https: String,
    pub one_click: bool,
}

/// Derived per-company numbers (recomputed from scanned_messages after every scan).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct CompanyAggregate {
    pub domain: String,
    pub display_name: String,
    pub senders: Vec<String>,
    pub message_count: i64,
    pub first_seen: Option<DateTime<Utc>>,
    pub last_seen: Option<DateTime<Utc>>,
    pub is_mailing_list: bool,
    pub unsub_mailto: String,
    pub unsub_https: String,
    pub one_click: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Company {
    #[serde(flatten)]
    pub agg: CompanyAggregate,
    /// "" (included), "own", "personal_domain"
    pub excluded_reason: String,
    /// "" (not checked), "broker", "website", "manual"
    pub contact_status: String,
    pub contact_email: String,
    /// URL the address was found on, or `broker:<id>`
    pub contact_source: String,
    pub contact_checked_at: Option<DateTime<Utc>>,
    /// "" or "ignored"
    pub decision: String,
    /// Last successful deletion request / unsubscribe.
    pub requested_at: Option<DateTime<Utc>>,
    pub unsubscribed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CompanyAction {
    pub id: i64,
    pub domain: String,
    /// request, unsubscribe_one_click, unsubscribe_mailto, unsubscribe_manual, ignore, unignore
    pub action: String,
    pub target: String,
    /// ok, failed, dry_run
    pub status: String,
    pub detail: String,
    pub created_at: Option<DateTime<Utc>>,
}

fn ts(t: &Option<DateTime<Utc>>) -> Option<String> {
    t.map(|t| t.to_rfc3339())
}

fn parse_ts(s: Option<String>) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(&s?)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

fn now() -> String {
    Utc::now().to_rfc3339()
}

const COMPANY_COLUMNS: &str = "domain, display_name, senders, message_count, first_seen, last_seen, is_mailing_list, unsub_mailto, unsub_https, one_click, excluded_reason, contact_status, contact_email, contact_source, contact_checked_at, decision, requested_at, unsubscribed_at";

fn scan_company(r: &Row) -> rusqlite::Result<Company> {
    Ok(Company {
        agg: CompanyAggregate {
            domain: r.get(0)?,
            display_name: r.get(1)?,
            senders: serde_json::from_str(&r.get::<_, String>(2)?).unwrap_or_default(),
            message_count: r.get(3)?,
            first_seen: parse_ts(r.get(4)?),
            last_seen: parse_ts(r.get(5)?),
            is_mailing_list: r.get::<_, i64>(6)? != 0,
            unsub_mailto: r.get(7)?,
            unsub_https: r.get(8)?,
            one_click: r.get::<_, i64>(9)? != 0,
        },
        excluded_reason: r.get(10)?,
        contact_status: r.get(11)?,
        contact_email: r.get(12)?,
        contact_source: r.get(13)?,
        contact_checked_at: parse_ts(r.get(14)?),
        decision: r.get(15)?,
        requested_at: parse_ts(r.get(16)?),
        unsubscribed_at: parse_ts(r.get(17)?),
    })
}

impl Store {
    /// Inserts messages not seen before (by key); returns how many were new.
    pub fn add_scanned_messages(&self, msgs: &[ScannedMessage]) -> Result<usize> {
        let mut db = self.db();
        let tx = db.transaction()?;
        let mut added = 0;
        {
            let mut stmt = tx.prepare(
                "INSERT OR IGNORE INTO scanned_messages (msg_key, domain, from_addr, from_name, sent_at,
                 is_list, unsub_mailto, unsub_https, one_click, scanned_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )?;
            let scanned_at = now();
            for m in msgs {
                added += stmt.execute(params![
                    m.key,
                    m.domain,
                    m.from_addr,
                    m.from_name,
                    ts(&m.date),
                    m.is_list as i64,
                    m.unsub_mailto,
                    m.unsub_https,
                    m.one_click as i64,
                    scanned_at,
                ])?;
            }
        }
        tx.commit().context("failed to store scanned messages")?;
        Ok(added)
    }

    pub fn scanned_messages_for(&self, domain: &str) -> Result<Vec<ScannedMessage>> {
        let db = self.db();
        let mut stmt = db.prepare(
            "SELECT msg_key, domain, from_addr, from_name, sent_at, is_list, unsub_mailto, unsub_https, one_click
             FROM scanned_messages WHERE domain = ?",
        )?;
        let rows = stmt.query_map([domain], |r| {
            Ok(ScannedMessage {
                key: r.get(0)?,
                domain: r.get(1)?,
                from_addr: r.get(2)?,
                from_name: r.get(3)?,
                date: parse_ts(r.get(4)?),
                is_list: r.get::<_, i64>(5)? != 0,
                unsub_mailto: r.get(6)?,
                unsub_https: r.get(7)?,
                one_click: r.get::<_, i64>(8)? != 0,
            })
        })?;
        rows.collect::<rusqlite::Result<_>>()
            .context("failed to read scanned messages")
    }

    /// Writes the derived numbers; keeps decisions and discovered contacts.
    pub fn upsert_company(&self, agg: &CompanyAggregate, excluded_reason: &str) -> Result<()> {
        self.db()
            .execute(
                "INSERT INTO scanned_companies (domain, display_name, senders, message_count, first_seen, last_seen,
                    is_mailing_list, unsub_mailto, unsub_https, one_click, excluded_reason, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
                 ON CONFLICT(domain) DO UPDATE SET display_name = ?2, senders = ?3, message_count = ?4,
                    first_seen = ?5, last_seen = ?6, is_mailing_list = ?7, unsub_mailto = ?8,
                    unsub_https = ?9, one_click = ?10, excluded_reason = ?11, updated_at = ?12",
                params![
                    agg.domain,
                    agg.display_name,
                    serde_json::to_string(&agg.senders)?,
                    agg.message_count,
                    ts(&agg.first_seen),
                    ts(&agg.last_seen),
                    agg.is_mailing_list as i64,
                    agg.unsub_mailto,
                    agg.unsub_https,
                    agg.one_click as i64,
                    excluded_reason,
                    now(),
                ],
            )
            .context("failed to store company")?;
        Ok(())
    }

    /// All companies, most messages first.
    pub fn list_companies(&self) -> Result<Vec<Company>> {
        let db = self.db();
        let mut stmt = db.prepare(&format!(
            "SELECT {COMPANY_COLUMNS} FROM scanned_companies ORDER BY message_count DESC, domain"
        ))?;
        let rows = stmt.query_map([], scan_company)?;
        rows.collect::<rusqlite::Result<_>>()
            .context("failed to list companies")
    }

    pub fn get_company(&self, domain: &str) -> Result<Option<Company>> {
        self.db()
            .query_row(
                &format!("SELECT {COMPANY_COLUMNS} FROM scanned_companies WHERE domain = ?"),
                [domain],
                scan_company,
            )
            .optional()
            .context("failed to query company")
    }

    pub fn set_company_contact(
        &self,
        domain: &str,
        status: &str,
        email: &str,
        source: &str,
    ) -> Result<()> {
        self.db()
            .execute(
                "UPDATE scanned_companies SET contact_status = ?, contact_email = ?, contact_source = ?,
                 contact_checked_at = ? WHERE domain = ?",
                params![status, email, source, now(), domain],
            )
            .context("failed to store company contact")?;
        Ok(())
    }

    pub fn set_company_decision(&self, domain: &str, decision: &str) -> Result<()> {
        self.db()
            .execute(
                "UPDATE scanned_companies SET decision = ? WHERE domain = ?",
                params![decision, domain],
            )
            .context("failed to store company decision")?;
        Ok(())
    }

    /// Stamps `requested_at` (what = "requested") or `unsubscribed_at` (what = "unsubscribed").
    pub fn mark_company(&self, domain: &str, what: &str) -> Result<()> {
        let col = match what {
            "requested" => "requested_at",
            "unsubscribed" => "unsubscribed_at",
            _ => anyhow::bail!("unknown company mark {what}"),
        };
        self.db()
            .execute(
                &format!("UPDATE scanned_companies SET {col} = ? WHERE domain = ?"),
                params![now(), domain],
            )
            .context("failed to mark company")?;
        Ok(())
    }

    pub fn add_company_action(
        &self,
        domain: &str,
        action: &str,
        target: &str,
        status: &str,
        detail: &str,
    ) -> Result<()> {
        self.db()
            .execute(
                "INSERT INTO company_actions (domain, action, target, status, detail, created_at)
                 VALUES (?, ?, ?, ?, ?, ?)",
                params![domain, action, target, status, detail, now()],
            )
            .context("failed to record company action")?;
        Ok(())
    }

    /// Action log for one company, newest first.
    pub fn company_actions(&self, domain: &str) -> Result<Vec<CompanyAction>> {
        let db = self.db();
        let mut stmt = db.prepare(
            "SELECT id, domain, action, target, status, detail, created_at FROM company_actions
             WHERE domain = ? ORDER BY id DESC",
        )?;
        let rows = stmt.query_map([domain], |r| {
            Ok(CompanyAction {
                id: r.get(0)?,
                domain: r.get(1)?,
                action: r.get(2)?,
                target: r.get(3)?,
                status: r.get(4)?,
                detail: r.get(5)?,
                created_at: parse_ts(r.get(6)?),
            })
        })?;
        rows.collect::<rusqlite::Result<_>>()
            .context("failed to read company actions")
    }
}
