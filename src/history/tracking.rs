//! Open tracking: one row per sent mail with our own Message-ID (`<token@domain>`), linked to
//! its removal request. Opens come from the pixel route, read receipts from inbox MDNs.
//! Times are unix seconds (they compare and aggregate as numbers).

use super::{Store, time_col};
use anyhow::{Context, Result};
use chrono::{DateTime, Local, Utc};
use rusqlite::{Connection, params};
use serde::Serialize;
use std::collections::HashMap;

pub(super) const SCHEMA: &str = r"
CREATE TABLE IF NOT EXISTS mail_tracking (
    message_id TEXT PRIMARY KEY,
    token TEXT NOT NULL UNIQUE,
    request_id INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    first_opened_at INTEGER,
    last_opened_at INTEGER,
    open_count INTEGER NOT NULL DEFAULT 0,
    client TEXT,
    read_receipt_at INTEGER
);
CREATE INDEX IF NOT EXISTS idx_mt_request ON mail_tracking(request_id);
";

/// Pixel hits closer together than this (per token) are ignored.
pub const OPEN_RATE_LIMIT_SECS: i64 = 10;

/// Client class of an open that proves a human looked at the mail.
pub const CLIENT_OTHER: &str = "other";

/// Opens and receipts of one request (its first mail and any reminders together).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Tracking {
    pub first_opened_at: Option<DateTime<Local>>,
    pub last_opened_at: Option<DateTime<Local>>,
    pub open_count: i64,
    /// "other" once any non-proxy open happened, else the proxy class ("apple-mpp",
    /// "google-proxy"); empty when never opened.
    pub client: String,
    pub read_receipt_at: Option<DateTime<Local>>,
}

/// Counts of requests (not mails).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct TrackingStats {
    /// Requests sent with tracking (own Message-ID).
    pub sent: i64,
    /// Opened by a regular client.
    pub opened: i64,
    /// Opened only through a proxy (Apple MPP prefetch, Google image proxy).
    pub proxy_opened: i64,
    /// Read receipt received.
    pub receipts: i64,
}

/// Registers a sent mail; no-op for Message-IDs we did not generate.
pub(super) fn track(db: &Connection, request_id: i64, message_id: &str) -> Result<()> {
    let Some(token) = crate::email::tracking_token(message_id) else {
        return Ok(());
    };
    db.execute(
        "INSERT OR IGNORE INTO mail_tracking (message_id, token, request_id, created_at)
         VALUES (?, ?, ?, ?)",
        params![message_id, token, request_id, Utc::now().timestamp()],
    )
    .context("failed to record tracking")?;
    Ok(())
}

impl Store {
    /// Links a mail sent for `request_id` (e.g. a reminder) to that request.
    pub fn track_message(&self, request_id: i64, message_id: &str) -> Result<()> {
        track(&self.db(), request_id, message_id)
    }

    /// Records one pixel hit. False for unknown tokens and rate-limited hits.
    pub fn record_open(&self, token: &str, client: &str, now: DateTime<Utc>) -> Result<bool> {
        let now = now.timestamp();
        let n = self
            .db()
            .execute(
                "UPDATE mail_tracking SET open_count = open_count + 1,
                    first_opened_at = COALESCE(first_opened_at, ?1), last_opened_at = ?1,
                    client = CASE WHEN client = 'other' THEN 'other' ELSE ?2 END
                 WHERE token = ?3 AND (last_opened_at IS NULL OR last_opened_at <= ?1 - ?4)",
                params![now, client, token, OPEN_RATE_LIMIT_SECS],
            )
            .context("failed to record open")?;
        Ok(n == 1)
    }

    /// Stores a read receipt for the first of `message_ids` we sent. False if none matches.
    pub fn record_read_receipt(&self, message_ids: &[String], at: DateTime<Utc>) -> Result<bool> {
        let db = self.db();
        for id in message_ids {
            let n = db
                .execute(
                    "UPDATE mail_tracking SET read_receipt_at = COALESCE(read_receipt_at, ?)
                     WHERE message_id = ?",
                    params![at.timestamp(), id],
                )
                .context("failed to record read receipt")?;
            if n == 1 {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Tracking per request id (requests without tracked mails are absent).
    pub fn request_tracking(&self) -> Result<HashMap<i64, Tracking>> {
        let db = self.db();
        let mut stmt = db.prepare(
            "SELECT request_id, MIN(first_opened_at), MAX(last_opened_at), SUM(open_count),
                CASE WHEN MAX(client = 'other') THEN 'other' ELSE COALESCE(MAX(client), '') END,
                MIN(read_receipt_at)
             FROM mail_tracking GROUP BY request_id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get(0)?,
                Tracking {
                    first_opened_at: time_col(r, 1)?,
                    last_opened_at: time_col(r, 2)?,
                    open_count: r.get(3)?,
                    client: r.get(4)?,
                    read_receipt_at: time_col(r, 5)?,
                },
            ))
        })?;
        rows.collect::<rusqlite::Result<_>>()
            .context("failed to query tracking")
    }

    pub fn tracking_stats(&self) -> Result<TrackingStats> {
        let t = self.request_tracking()?;
        let count = |f: &dyn Fn(&Tracking) -> bool| t.values().filter(|x| f(x)).count() as i64;
        Ok(TrackingStats {
            sent: t.len() as i64,
            opened: count(&|x| x.client == CLIENT_OTHER),
            proxy_opened: count(&|x| x.open_count > 0 && x.client != CLIENT_OTHER),
            receipts: count(&|x| x.read_receipt_at.is_some()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::{BrokerResponse, Record, Status};

    const MID: &str = "<AAAAAAAAAAAAAAAAAAAAAA@example.com>";
    const TOKEN: &str = "AAAAAAAAAAAAAAAAAAAAAA";

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::new(dir.path().join("h.db")).unwrap();
        (dir, s)
    }

    fn sent(s: &Store, message_id: &str) -> i64 {
        let mut r = Record {
            broker_id: "b".into(),
            broker_name: "B".into(),
            email: "p@b.com".into(),
            template: "gdpr".into(),
            status: Status::Sent,
            message_id: message_id.into(),
            ..Default::default()
        };
        s.add(&mut r).unwrap();
        r.id
    }

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000 + secs, 0).unwrap()
    }

    #[test]
    fn opens_are_rate_limited_and_classified() {
        let (_d, s) = store();
        let id = sent(&s, MID);
        sent(&s, "smtp-legacy@x.com-0"); // not ours: untracked
        assert!(
            !s.record_open("unknownunknownunknown1", "other", at(0))
                .unwrap()
        );
        assert!(s.record_open(TOKEN, "apple-mpp", at(0)).unwrap());
        assert!(
            !s.record_open(TOKEN, "other", at(5)).unwrap(),
            "within 10 s"
        );
        let t = &s.request_tracking().unwrap()[&id];
        assert_eq!((t.open_count, t.client.as_str()), (1, "apple-mpp"));
        assert_eq!(
            s.tracking_stats().unwrap(),
            TrackingStats {
                sent: 1,
                opened: 0,
                proxy_opened: 1,
                receipts: 0
            }
        );

        assert!(s.record_open(TOKEN, "other", at(10)).unwrap());
        assert!(s.record_open(TOKEN, "google-proxy", at(30)).unwrap());
        let t = &s.request_tracking().unwrap()[&id];
        assert_eq!(
            (t.open_count, t.client.as_str()),
            (3, "other"),
            "other is sticky"
        );
        assert_eq!(t.first_opened_at.unwrap().timestamp(), at(0).timestamp());
        assert_eq!(t.last_opened_at.unwrap().timestamp(), at(30).timestamp());
        assert_eq!(s.tracking_stats().unwrap().opened, 1);
    }

    #[test]
    fn finish_send_and_reminders_are_tracked() {
        let (_d, s) = store();
        let mut r = Record {
            broker_id: "b".into(),
            broker_name: "B".into(),
            email: "p@b.com".into(),
            template: "gdpr".into(),
            ..Default::default()
        };
        s.add(&mut r).unwrap(); // pending: nothing yet
        assert!(s.request_tracking().unwrap().is_empty());
        let res = crate::email::SendResult {
            success: true,
            message_id: MID.into(),
            error: None,
        };
        s.finish_send(r.id, &res).unwrap();
        let reminder = "<BBBBBBBBBBBBBBBBBBBBBB@example.com>";
        s.track_message(r.id, reminder).unwrap();
        assert!(
            s.record_read_receipt(&["<other@x>".into(), reminder.into()], at(1))
                .unwrap()
        );
        assert!(!s.record_read_receipt(&["<other@x>".into()], at(1)).unwrap());
        let t = &s.request_tracking().unwrap()[&r.id];
        assert_eq!(t.read_receipt_at.unwrap().timestamp(), at(1).timestamp());
        assert_eq!(s.tracking_stats().unwrap().receipts, 1);
    }

    #[test]
    fn read_receipts_are_not_broker_responses() {
        let (_d, s) = store();
        let id = sent(&s, MID);
        let mut resp = BrokerResponse {
            broker_id: "b".into(),
            broker_name: "B".into(),
            response_type: "unknown".into(),
            read_receipt_for: Some(vec![MID.into()]),
            ..Default::default()
        };
        s.add_broker_response(&mut resp).unwrap();
        assert_eq!(s.get_all_broker_responses().unwrap(), []);
        assert!(s.request_tracking().unwrap()[&id].read_receipt_at.is_some());
    }
}
