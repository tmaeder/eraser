//! Autopilot bookkeeping: the round log (which doubles as the cross-process lock), reminder
//! tracking, to-do queries and progress counts. Round timestamps are RFC 3339 UTC (seconds), so
//! they compare correctly as strings.

use super::{
    PendingTask, RECORD_COLUMNS, Record, Store, TASK_COLUMNS, TaskType, format_time, now_str,
    scan_record, scan_task, sum_col,
};
use anyhow::{Context, Result};
use chrono::{DateTime, Local, SecondsFormat, TimeDelta, Utc};
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use serde::Serialize;

pub(super) const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS autopilot_rounds (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    started_at TEXT NOT NULL,
    finished_at TEXT,
    dry_run INTEGER NOT NULL DEFAULT 0,
    report TEXT NOT NULL DEFAULT ''
);
"#;

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

#[derive(Debug, Clone, PartialEq, Serialize)]
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
            params![ts(now), dry_run as i64],
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

    pub fn mark_reminder_sent(&self, record_id: i64) -> Result<()> {
        self.db()
            .execute(
                "UPDATE removal_requests SET reminder_sent_at = ? WHERE id = ?",
                params![now_str(), record_id],
            )
            .context("failed to mark reminder sent")?;
        Ok(())
    }

    /// Emails sent since `since`: request attempts (any status) plus reminders.
    pub fn emails_sent_since(&self, since: DateTime<Local>) -> Result<i64> {
        self.db()
            .query_row(
                "SELECT (SELECT COUNT(*) FROM removal_requests WHERE sent_at >= ?1)
                      + (SELECT COUNT(*) FROM removal_requests WHERE reminder_sent_at >= ?1)",
                [format_time(&since)],
                |r| r.get(0),
            )
            .context("failed to count sent emails")
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
