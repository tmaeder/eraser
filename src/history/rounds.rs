//! Autopilot bookkeeping: the round log (which doubles as the cross-process lock), reminder
//! tracking, to-do queries and progress counts. Round timestamps are RFC 3339 UTC (seconds), so
//! they compare correctly as strings.

use super::{
    PendingTask, RECORD_COLUMNS, Record, Status, Store, TASK_COLUMNS, TaskType, format_time,
    now_str, scan_record, scan_task, sum_col,
};
use anyhow::{Context, Result};
use chrono::{DateTime, Local, SecondsFormat, TimeDelta, Utc};
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use serde::Serialize;

pub(super) const SCHEMA: &str = r"
CREATE TABLE IF NOT EXISTS autopilot_rounds (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    started_at TEXT NOT NULL,
    finished_at TEXT,
    dry_run INTEGER NOT NULL DEFAULT 0,
    report TEXT NOT NULL DEFAULT ''
);
CREATE TABLE IF NOT EXISTS settings (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
";

const BLOCKED_AT: &str = "account_blocked_at";
const BLOCKED_REASON: &str = "account_blocked_reason";

/// An unfinished round older than this was interrupted (container killed) and no longer blocks.
pub const STALE_ROUND: TimeDelta = TimeDelta::hours(2);

/// Task types shown as to-dos on the Today page.
pub const TODO_TYPES: &[TaskType] = &[
    TaskType::ManualForm,
    TaskType::Captcha,
    TaskType::Confirm,
    TaskType::IdRequired,
    TaskType::UnsubscribeLink,
];

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct AutopilotRound {
    pub id: i64,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub dry_run: bool,
    /// The round report as JSON ("" while running).
    pub report: String,
}

/// Latest sent request per broker, by outcome (for the Today progress bar).
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct Progress {
    pub contacted: i64,
    pub confirmed: i64,
    pub rejected: i64,
    /// Reminder sent and still no reply.
    pub no_response: i64,
    pub waiting: i64,
}

fn ts(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn parse(s: Option<String>) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(&s?)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

/// Error text of a send left `pending` by a crashed run (never resent automatically).
pub const INTERRUPTED: &str =
    "interrupted before the result was recorded: check your Sent folder, it was not resent";

const RUNNING: &str =
    "SELECT EXISTS(SELECT 1 FROM autopilot_rounds WHERE finished_at IS NULL AND started_at > ?)";

impl Store {
    /// Starts a round unless another (non-stale) one is running; returns its id.
    pub fn begin_round(&self, dry_run: bool) -> Result<Option<i64>> {
        let mut db = self.db();
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = Utc::now();
        let busy: bool = tx.query_row(RUNNING, [ts(now - STALE_ROUND)], |r| r.get(0))?;
        if busy {
            return Ok(None);
        }
        tx.execute(
            "INSERT INTO autopilot_rounds (started_at, dry_run) VALUES (?, ?)",
            params![ts(now), i64::from(dry_run)],
        )?;
        let id = tx.last_insert_rowid();
        tx.commit().context("failed to start autopilot round")?;
        Ok(Some(id))
    }

    pub fn finish_round(&self, id: i64, report_json: &str) -> Result<()> {
        self.db()
            .execute(
                "UPDATE autopilot_rounds SET finished_at = ?, report = ? WHERE id = ?",
                params![ts(Utc::now()), report_json, id],
            )
            .context("failed to finish autopilot round")?;
        Ok(())
    }

    pub fn round_running(&self) -> Result<bool> {
        self.db()
            .query_row(RUNNING, [ts(Utc::now() - STALE_ROUND)], |r| r.get(0))
            .context("failed to query autopilot rounds")
    }

    /// Newest first.
    pub fn recent_rounds(&self, limit: i64) -> Result<Vec<AutopilotRound>> {
        let db = self.db();
        let mut stmt = db.prepare(
            "SELECT id, started_at, finished_at, dry_run, report FROM autopilot_rounds
             ORDER BY id DESC LIMIT ?",
        )?;
        let rows = stmt.query_map([limit], |r| {
            Ok(AutopilotRound {
                id: r.get(0)?,
                started_at: parse(r.get(1)?),
                finished_at: parse(r.get(2)?),
                dry_run: r.get::<_, i64>(3)? != 0,
                report: r.get(4)?,
            })
        })?;
        rows.collect::<rusqlite::Result<_>>()
            .context("failed to read autopilot rounds")
    }

    /// Latest request per broker that was sent, got no reply at all, has no reminder yet and is
    /// still open. The caller filters by age.
    pub fn reminder_candidates(&self) -> Result<Vec<Record>> {
        let sql = format!(
            "SELECT {RECORD_COLUMNS} FROM removal_requests r
             WHERE r.id IN (SELECT MAX(id) FROM removal_requests GROUP BY broker_id)
               AND r.status = 'sent' AND r.reminder_sent_at IS NULL
               AND COALESCE(r.pipeline_status, 'email_sent') IN ('email_sent', 'awaiting_response')
               AND NOT EXISTS (SELECT 1 FROM broker_responses b WHERE b.broker_id = r.broker_id)
             ORDER BY r.id"
        );
        let db = self.db();
        let mut stmt = db.prepare(&sql)?;
        let rows = stmt.query_map([], scan_record)?;
        rows.collect::<rusqlite::Result<_>>()
            .context("failed to query reminder candidates")
    }

    /// Crash-safe send, step 2: stores the outcome of a request recorded as `pending`.
    pub fn finish_send(&self, record_id: i64, result: &crate::email::SendResult) -> Result<()> {
        let (status, error) = if result.success {
            (Status::Sent, String::new())
        } else {
            (Status::Failed, result.error.clone().unwrap_or_default())
        };
        self.db()
            .execute(
                "UPDATE removal_requests SET status = ?, message_id = ?, error = ? WHERE id = ?",
                params![status, result.message_id, error, record_id],
            )
            .context("failed to record send result")?;
        if result.success {
            self.track_message(record_id, &result.message_id)?;
        }
        Ok(())
    }

    /// Requests still `pending` (sent_at before `before`) whose run crashed mid-send: flags them
    /// with [`INTERRUPTED`] once and returns the newly flagged ones. They stay `pending`, so they
    /// count as contacted and are neither resent nor reminded.
    pub fn flag_interrupted_sends(&self, before: DateTime<Local>) -> Result<Vec<Record>> {
        let db = self.db();
        let sql = format!(
            "SELECT {RECORD_COLUMNS} FROM removal_requests
             WHERE status = 'pending' AND COALESCE(error, '') = '' AND sent_at < ? ORDER BY id"
        );
        let found: Vec<Record> = db
            .prepare(&sql)?
            .query_map([format_time(&before)], scan_record)?
            .collect::<rusqlite::Result<_>>()
            .context("failed to query interrupted sends")?;
        for r in &found {
            db.execute(
                "UPDATE removal_requests SET error = ? WHERE id = ?",
                params![INTERRUPTED, r.id],
            )
            .context("failed to flag interrupted send")?;
        }
        Ok(found)
    }

    /// Claims the reminder for `record_id` before sending it (false = already claimed), so a
    /// crash mid-send never sends it twice.
    /// ponytail: a crash between claim and send loses that one reminder (at most once).
    pub fn claim_reminder(&self, record_id: i64) -> Result<bool> {
        let n = self
            .db()
            .execute(
                "UPDATE removal_requests SET reminder_sent_at = ? WHERE id = ? AND reminder_sent_at IS NULL",
                params![now_str(), record_id],
            )
            .context("failed to claim reminder")?;
        Ok(n == 1)
    }

    /// Undoes [`Store::claim_reminder`] after a failed send.
    pub fn release_reminder(&self, record_id: i64) -> Result<()> {
        self.db()
            .execute(
                "UPDATE removal_requests SET reminder_sent_at = NULL WHERE id = ?",
                [record_id],
            )
            .context("failed to release reminder")?;
        Ok(())
    }

    pub fn mark_reminder_sent(&self, record_id: i64) -> Result<()> {
        self.db()
            .execute(
                "UPDATE removal_requests SET reminder_sent_at = ? WHERE id = ?",
                params![now_str(), record_id],
            )
            .context("failed to mark reminder sent")?;
        Ok(())
    }

    /// Emails sent since `since` (see `send_times_since`).
    pub fn emails_sent_since(&self, since: DateTime<Local>) -> Result<i64> {
        Ok(self.send_times_since(since)?.len() as i64)
    }

    /// Send attempts since `since`, oldest first: requests of any status, reminders and mailto
    /// unsubscribes (ok or failed). Single source for the pacing caps.
    pub fn send_times_since(&self, since: DateTime<Local>) -> Result<Vec<DateTime<Local>>> {
        // String bounds with slack for zone abbreviations and DST; exact filter below.
        // removal_requests stores local time, company_actions RFC 3339 UTC.
        let slack = since - TimeDelta::hours(2);
        let bound = format_time(&slack);
        let utc_bound = slack
            .with_timezone(&Utc)
            .to_rfc3339_opts(SecondsFormat::Secs, false);
        let db = self.db();
        let mut stmt = db
            .prepare(
                "SELECT sent_at FROM removal_requests WHERE sent_at >= ?1
                 UNION ALL SELECT reminder_sent_at FROM removal_requests WHERE reminder_sent_at >= ?1
                 UNION ALL SELECT created_at FROM company_actions
                   WHERE action = 'unsubscribe_mailto' AND status IN ('ok', 'failed')
                   AND created_at >= ?2",
            )
            .context("failed to query send times")?;
        let rows = stmt
            .query_map([bound, utc_bound], |r| r.get::<_, String>(0))
            .context("failed to query send times")?;
        let mut times: Vec<DateTime<Local>> = rows
            .filter_map(|r| r.ok().as_deref().and_then(super::parse_time))
            .filter(|t| *t >= since)
            .collect();
        times.sort();
        Ok(times)
    }

    /// Circuit breaker: the provider rejected our account; every send path refuses until cleared.
    pub fn block_account(&self, reason: &str) -> Result<()> {
        let db = self.db();
        for (k, v) in [
            (BLOCKED_AT, now_str()),
            (BLOCKED_REASON, reason.to_string()),
        ] {
            db.execute(
                "INSERT INTO settings (key, value) VALUES (?, ?)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![k, v],
            )
            .context("failed to store account block")?;
        }
        Ok(())
    }

    /// (since, reason) while sending is blocked.
    pub fn account_blocked(&self) -> Result<Option<(DateTime<Local>, String)>> {
        let db = self.db();
        let get = |k: &str| {
            db.query_row("SELECT value FROM settings WHERE key = ?", [k], |r| {
                r.get::<_, String>(0)
            })
            .optional()
            .context("failed to read account block")
        };
        let Some(at) = get(BLOCKED_AT)? else {
            return Ok(None);
        };
        let at = super::parse_time(&at).unwrap_or_else(Local::now);
        Ok(Some((at, get(BLOCKED_REASON)?.unwrap_or_default())))
    }

    pub fn clear_account_block(&self) -> Result<()> {
        self.db()
            .execute(
                "DELETE FROM settings WHERE key IN (?, ?)",
                params![BLOCKED_AT, BLOCKED_REASON],
            )
            .context("failed to clear account block")?;
        Ok(())
    }

    /// Any task (any status) of this type for this URL: links are handled at most once.
    pub fn find_task_by_url(&self, task_type: TaskType, url: &str) -> Result<Option<PendingTask>> {
        let sql = format!(
            "SELECT {TASK_COLUMNS} FROM pending_tasks WHERE task_type = ? AND form_url = ? LIMIT 1"
        );
        self.db()
            .query_row(&sql, params![task_type, url], scan_task)
            .optional()
            .context("failed to query task")
    }

    /// Hides a to-do until `until` (it stays pending).
    pub fn skip_task_until(&self, id: i64, until: DateTime<Local>) -> Result<()> {
        self.db()
            .execute(
                "UPDATE pending_tasks SET skipped_until = ? WHERE id = ?",
                params![format_time(&until), id],
            )
            .context("failed to skip task")?;
        Ok(())
    }

    /// Pending to-dos not skipped at `now`, oldest (nearest deadline) first.
    pub fn todo_tasks(&self, now: DateTime<Local>) -> Result<Vec<PendingTask>> {
        let mut tasks: Vec<PendingTask> = self
            .get_pending_tasks(None, Some("pending"))?
            .into_iter()
            .filter(|t| TODO_TYPES.contains(&t.task_type))
            .filter(|t| t.skipped_until.is_none_or(|u| u <= now))
            .collect();
        tasks.sort_by_key(|t| (t.created_at, t.id));
        Ok(tasks)
    }

    /// Outcome counts for brokers (`companies = false`) or inbox companies (`company:` ids).
    pub fn progress(&self, companies: bool) -> Result<Progress> {
        let (contacted, confirmed, rejected, no_response) = self
            .db()
            .query_row(
                "SELECT COUNT(*),
                    SUM(CASE WHEN pipeline_status = 'confirmed' THEN 1 ELSE 0 END),
                    SUM(CASE WHEN pipeline_status = 'rejected' THEN 1 ELSE 0 END),
                    SUM(CASE WHEN reminder_sent_at IS NOT NULL
                              AND COALESCE(pipeline_status, '') NOT IN ('confirmed', 'rejected')
                         THEN 1 ELSE 0 END)
                 FROM removal_requests
                 WHERE id IN (SELECT MAX(id) FROM removal_requests WHERE status = 'sent' GROUP BY broker_id)
                   AND (broker_id LIKE 'company:%') = ?",
                [companies],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        sum_col(r, 1)?,
                        sum_col(r, 2)?,
                        sum_col(r, 3)?,
                    ))
                },
            )
            .context("failed to count progress")?;
        Ok(Progress {
            contacted,
            confirmed,
            rejected,
            no_response,
            waiting: (contacted - confirmed - rejected - no_response).max(0),
        })
    }
}
