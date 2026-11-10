//! Port of internal/history/history.go: SQLite-backed record of removal requests, broker
//! responses and pending tasks. Schema-compatible with databases written by the Go version.
//!
//! Timestamps are stored as TEXT in Go's `time.Time.String()` layout, which is what the Go
//! modernc sqlite driver writes: `2026-03-14 09:26:53.589793238 +0100 CET` (Go may append
//! ` m=+0.0123` for monotonic clock readings; that suffix is ignored on read and never written).
//! Keeping the layout matters because the queries compare and sort these strings lexically.

use anyhow::{Context, Result};
use chrono::{DateTime, Datelike, FixedOffset, Local, NaiveDate, NaiveDateTime, TimeZone, Utc};
use rusqlite::types::{ToSqlOutput, ValueRef};
use rusqlite::{Connection, OptionalExtension, Row, ToSql, params};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

macro_rules! str_enum {
    ($(#[$m:meta])* $name:ident { $($variant:ident = $s:literal),+ $(,)? }) => {
        $(#[$m])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, serde::Deserialize)]
        pub enum $name { $(#[serde(rename = $s)] $variant),+ }

        impl $name {
            pub const ALL: &'static [$name] = &[$($name::$variant),+];

            pub fn as_str(&self) -> &'static str {
                match self { $($name::$variant => $s),+ }
            }
        }

        impl std::str::FromStr for $name {
            type Err = anyhow::Error;
            fn from_str(s: &str) -> Result<Self> {
                match s {
                    $($s => Ok($name::$variant),)+
                    _ => anyhow::bail!(concat!("unknown ", stringify!($name), ": {}"), s),
                }
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl ToSql for $name {
            fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
                Ok(ToSqlOutput::from(self.as_str()))
            }
        }
    };
}

str_enum!(Status {
    Sent = "sent",
    Failed = "failed",
    Pending = "pending",
});

str_enum!(
    /// Current stage in the removal pipeline.
    PipelineStatus {
        EmailSent = "email_sent",
        AwaitingResponse = "awaiting_response",
        FormRequired = "form_required",
        FormFilled = "form_filled",
        AwaitingCaptcha = "awaiting_captcha",
        CaptchaSolved = "captcha_solved",
        AwaitingConfirmation = "awaiting_confirmation",
        Confirmed = "confirmed",
        Failed = "failed",
        Rejected = "rejected",
    }
);

str_enum!(
    /// Type of a pending task.
    TaskType {
        Captcha = "captcha",
        ManualForm = "manual_form",
        Review = "review",
        Confirm = "confirm",
        // The broker asks for an identity document (never sent automatically).
        IdRequired = "id_required",
        // A plain (non RFC 8058) unsubscribe link the user opens.
        UnsubscribeLink = "unsubscribe_link",
    }
);

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Record {
    pub id: i64,
    pub broker_id: String,
    pub broker_name: String,
    pub email: String,
    pub template: String,
    pub status: Status,
    pub message_id: String,
    pub error: String,
    pub sent_at: Option<DateTime<Local>>,
    pub created_at: Option<DateTime<Local>>,
    /// Current stage in pipeline (None if the column is NULL/unknown).
    pub pipeline_status: Option<PipelineStatus>,
}

impl Default for Record {
    fn default() -> Self {
        Self {
            id: 0,
            broker_id: String::new(),
            broker_name: String::new(),
            email: String::new(),
            template: String::new(),
            status: Status::Pending,
            message_id: String::new(),
            error: String::new(),
            sent_at: None,
            created_at: None,
            pipeline_status: None,
        }
    }
}

/// A classified response from a broker.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct BrokerResponse {
    pub id: i64,
    pub broker_id: String,
    pub broker_name: String,
    /// form_required, confirmation_required, success, rejected, pending, unknown
    pub response_type: String,
    pub email_from: String,
    pub email_subject: String,
    /// Stored for reclassification
    pub email_body: String,
    pub form_url: String,
    pub confirm_url: String,
    pub confidence: f64,
    pub needs_review: bool,
    pub received_at: Option<DateTime<Local>>,
    pub processed_at: Option<DateTime<Local>>,
    pub created_at: Option<DateTime<Local>>,
}

/// A task that needs human intervention.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PendingTask {
    pub id: i64,
    pub broker_id: String,
    pub broker_name: String,
    pub task_type: TaskType,
    pub form_url: String,
    pub screenshot_path: String,
    /// JSON serialized browser state (profile data for helper page)
    pub browser_state: String,
    pub notes: String,
    /// pending, completed, skipped
    pub status: String,
    pub created_at: Option<DateTime<Local>>,
    /// When user first opened the helper page
    pub opened_at: Option<DateTime<Local>>,
    pub completed_at: Option<DateTime<Local>>,
    /// Hidden from the Today to-do list until then ("Skip"); the task stays pending.
    pub skipped_until: Option<DateTime<Local>>,
}

impl Default for PendingTask {
    fn default() -> Self {
        Self {
            id: 0,
            broker_id: String::new(),
            broker_name: String::new(),
            task_type: TaskType::Review,
            form_url: String::new(),
            screenshot_path: String::new(),
            browser_state: String::new(),
            notes: String::new(),
            status: String::new(),
            created_at: None,
            opened_at: None,
            completed_at: None,
            skipped_until: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BrokerStatus {
    pub broker_id: String,
    pub last_sent: Option<DateTime<Local>>,
    pub status: Status,
    pub total_sent: i64,
}

/// A form detected from email with its current fill status.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FormWithStatus {
    pub broker_id: String,
    pub broker_name: String,
    pub form_url: String,
    pub email_subject: String,
    pub detected_at: Option<DateTime<Local>>,
    /// pending, filled, captcha, failed, skipped
    pub status: String,
    /// Pending task id, 0 if none
    pub task_id: i64,
    /// None when the broker has no removal request (Go: "")
    pub pipeline_status: Option<PipelineStatus>,
}

/// Thread-safe handle; clones share one connection.
#[derive(Clone)]
pub struct Store {
    conn: Arc<Mutex<Connection>>,
}

pub fn default_db_path() -> PathBuf {
    match dirs::home_dir() {
        Some(home) => home.join(".eraser").join("history.db"),
        None => PathBuf::from("eraser_history.db"),
    }
}

// ==================== Timestamp encoding ====================

/// Formats like Go's `time.Time.String()` (without the monotonic suffix).
pub fn format_time<Tz: TimeZone>(t: &DateTime<Tz>) -> String {
    let local = t.with_timezone(&Local);
    let mut s = local.format("%Y-%m-%d %H:%M:%S").to_string();
    let nanos = local.timestamp_subsec_nanos();
    if nanos > 0 {
        let frac = format!("{nanos:09}");
        s.push('.');
        s.push_str(frac.trim_end_matches('0'));
    }
    let abbrev = zone_abbreviation(local.timestamp()).unwrap_or_else(|| {
        // Go prints the numeric offset when a zone has no abbreviation.
        local.format("%z").to_string()
    });
    s.push(' ');
    s.push_str(&local.format("%z").to_string());
    s.push(' ');
    s.push_str(&abbrev);
    s
}

#[cfg(unix)]
fn zone_abbreviation(ts: i64) -> Option<String> {
    let t = ts as libc::time_t;
    // SAFETY: localtime_r writes into the provided tm; tm_zone points at static tz data.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&t, &mut tm).is_null() || tm.tm_zone.is_null() {
            return None;
        }
        let name = std::ffi::CStr::from_ptr(tm.tm_zone).to_str().ok()?;
        (!name.is_empty()).then(|| name.to_string())
    }
}

#[cfg(not(unix))]
fn zone_abbreviation(_ts: i64) -> Option<String> {
    None
}

/// Parses every layout the Go driver can produce or accept (see modernc sqlite parseTime).
/// Go's zero time ("0001-01-01 00:00:00 +0000 UTC") maps to None.
pub fn parse_time(raw: &str) -> Option<DateTime<Local>> {
    let s = raw.find(" m=").map_or(raw, |i| &raw[..i]).trim();

    // Go time.String(): "2006-01-02 15:04:05.999999999 -0700 MST"
    let parsed = s
        .rsplit_once(' ')
        .and_then(|(head, _abbrev)| {
            DateTime::<FixedOffset>::parse_from_str(head, "%Y-%m-%d %H:%M:%S%.f %z").ok()
        })
        .or_else(|| {
            let ts = s.strip_suffix('Z').unwrap_or(s);
            ["%Y-%m-%d %H:%M:%S%.f%:z", "%Y-%m-%dT%H:%M:%S%.f%:z"]
                .iter()
                .find_map(|f| DateTime::parse_from_str(ts, f).ok())
                .or_else(|| {
                    // Zone-less layouts are UTC, as in Go's time.Parse.
                    [
                        "%Y-%m-%d %H:%M:%S%.f",
                        "%Y-%m-%dT%H:%M:%S%.f",
                        "%Y-%m-%d %H:%M",
                        "%Y-%m-%dT%H:%M",
                    ]
                    .iter()
                    .find_map(|f| NaiveDateTime::parse_from_str(ts, f).ok())
                    .or_else(|| {
                        NaiveDate::parse_from_str(ts, "%Y-%m-%d")
                            .ok()
                            .and_then(|d| d.and_hms_opt(0, 0, 0))
                    })
                    .map(|n| Utc.from_utc_datetime(&n).fixed_offset())
                })
        })?;

    if parsed.year() <= 1 {
        return None;
    }
    Some(parsed.with_timezone(&Local))
}

/// Reads a timestamp column that may be TEXT (normal), INTEGER (unix seconds) or NULL.
fn time_col(row: &Row, idx: usize) -> rusqlite::Result<Option<DateTime<Local>>> {
    Ok(match row.get_ref(idx)? {
        ValueRef::Text(t) => parse_time(&String::from_utf8_lossy(t)),
        ValueRef::Integer(i) => Local.timestamp_opt(i, 0).single(),
        _ => None,
    })
}

fn str_col(row: &Row, idx: usize) -> rusqlite::Result<String> {
    Ok(row.get::<_, Option<String>>(idx)?.unwrap_or_default())
}

fn opt_time(t: &Option<DateTime<Local>>) -> Option<String> {
    t.as_ref().map(format_time)
}

fn now_str() -> String {
    format_time(&Local::now())
}

// ==================== Store ====================

const RECORD_COLUMNS: &str = "id, broker_id, broker_name, email, template, status, message_id, error, sent_at, created_at, pipeline_status";
const RESPONSE_COLUMNS: &str = "id, broker_id, broker_name, response_type, email_from, email_subject, email_body, form_url, confirm_url, confidence, needs_review, received_at, processed_at, created_at";
const TASK_COLUMNS: &str = "id, broker_id, broker_name, task_type, form_url, screenshot_path, browser_state, notes, status, created_at, opened_at, completed_at, skipped_until";

fn scan_record(row: &Row) -> rusqlite::Result<Record> {
    Ok(Record {
        id: row.get(0)?,
        broker_id: row.get(1)?,
        broker_name: row.get(2)?,
        email: row.get(3)?,
        template: row.get(4)?,
        status: str_col(row, 5)?.parse().unwrap_or(Status::Pending),
        message_id: str_col(row, 6)?,
        error: str_col(row, 7)?,
        sent_at: time_col(row, 8)?,
        created_at: time_col(row, 9)?,
        pipeline_status: str_col(row, 10)?.parse().ok(),
    })
}

fn scan_response(row: &Row) -> rusqlite::Result<BrokerResponse> {
    Ok(BrokerResponse {
        id: row.get(0)?,
        broker_id: row.get(1)?,
        broker_name: row.get(2)?,
        response_type: row.get(3)?,
        email_from: str_col(row, 4)?,
        email_subject: str_col(row, 5)?,
        email_body: str_col(row, 6)?,
        form_url: str_col(row, 7)?,
        confirm_url: str_col(row, 8)?,
        confidence: row.get::<_, Option<f64>>(9)?.unwrap_or_default(),
        needs_review: row.get::<_, Option<i64>>(10)? == Some(1),
        received_at: time_col(row, 11)?,
        processed_at: time_col(row, 12)?,
        created_at: time_col(row, 13)?,
    })
}

fn scan_task(row: &Row) -> rusqlite::Result<PendingTask> {
    Ok(PendingTask {
        id: row.get(0)?,
        broker_id: row.get(1)?,
        broker_name: row.get(2)?,
        task_type: str_col(row, 3)?.parse().unwrap_or(TaskType::Review),
        form_url: str_col(row, 4)?,
        screenshot_path: str_col(row, 5)?,
        browser_state: str_col(row, 6)?,
        notes: str_col(row, 7)?,
        status: str_col(row, 8)?,
        created_at: time_col(row, 9)?,
        opened_at: time_col(row, 10)?,
        completed_at: time_col(row, 11)?,
        skipped_until: time_col(row, 12)?,
    })
}

fn sum_col(row: &Row, idx: usize) -> rusqlite::Result<i64> {
    Ok(row.get::<_, Option<i64>>(idx)?.unwrap_or(0))
}

impl Store {
    pub fn new(db_path: impl AsRef<Path>) -> Result<Self> {
        let db_path = db_path.as_ref();
        if let Some(dir) = db_path.parent().filter(|d| !d.as_os_str().is_empty()) {
            crate::config::create_private_dir(dir).context("failed to create history directory")?;
        }
        let conn = Connection::open(db_path).context("failed to open database")?;
        let store = Self {
            conn: Arc::new(Mutex::new(conn)),
        };
        store.migrate()?;
        Ok(store)
    }

    fn db(&self) -> MutexGuard<'_, Connection> {
        // A panic while holding the lock cannot leave SQLite in a bad state; keep going.
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn migrate(&self) -> Result<()> {
        let db = self.db();
        // Add new columns to existing databases first (errors = already present / no table yet).
        let _ = db.execute(
            "ALTER TABLE removal_requests ADD COLUMN pipeline_status TEXT DEFAULT 'email_sent'",
            [],
        );
        let _ = db.execute(
            "ALTER TABLE pending_tasks ADD COLUMN opened_at DATETIME",
            [],
        );

        db.execute_batch(
            r#"
	CREATE TABLE IF NOT EXISTS removal_requests (
		id INTEGER PRIMARY KEY AUTOINCREMENT,
		broker_id TEXT NOT NULL,
		broker_name TEXT NOT NULL,
		email TEXT NOT NULL,
		template TEXT NOT NULL,
		status TEXT NOT NULL,
		message_id TEXT,
		error TEXT,
		sent_at DATETIME,
		created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
		pipeline_status TEXT DEFAULT 'email_sent'
	);

	CREATE INDEX IF NOT EXISTS idx_broker_id ON removal_requests(broker_id);
	CREATE INDEX IF NOT EXISTS idx_sent_at ON removal_requests(sent_at);
	CREATE INDEX IF NOT EXISTS idx_status ON removal_requests(status);
	CREATE INDEX IF NOT EXISTS idx_pipeline_status ON removal_requests(pipeline_status);

	-- Broker responses table (stores classified email responses)
	CREATE TABLE IF NOT EXISTS broker_responses (
		id INTEGER PRIMARY KEY AUTOINCREMENT,
		broker_id TEXT NOT NULL,
		broker_name TEXT NOT NULL,
		response_type TEXT NOT NULL,
		email_from TEXT,
		email_subject TEXT,
		form_url TEXT,
		confirm_url TEXT,
		confidence REAL,
		needs_review INTEGER DEFAULT 0,
		received_at DATETIME,
		processed_at DATETIME,
		created_at DATETIME DEFAULT CURRENT_TIMESTAMP
	);

	CREATE INDEX IF NOT EXISTS idx_br_broker_id ON broker_responses(broker_id);
	CREATE INDEX IF NOT EXISTS idx_br_response_type ON broker_responses(response_type);
	CREATE INDEX IF NOT EXISTS idx_br_needs_review ON broker_responses(needs_review);

	-- Pending tasks table (for CAPTCHAs, manual forms, etc.)
	CREATE TABLE IF NOT EXISTS pending_tasks (
		id INTEGER PRIMARY KEY AUTOINCREMENT,
		broker_id TEXT NOT NULL,
		broker_name TEXT NOT NULL,
		task_type TEXT NOT NULL,
		form_url TEXT,
		screenshot_path TEXT,
		browser_state TEXT,
		notes TEXT,
		status TEXT DEFAULT 'pending',
		created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
		opened_at DATETIME,
		completed_at DATETIME
	);

	CREATE INDEX IF NOT EXISTS idx_pt_broker_id ON pending_tasks(broker_id);
	CREATE INDEX IF NOT EXISTS idx_pt_task_type ON pending_tasks(task_type);
	CREATE INDEX IF NOT EXISTS idx_pt_status ON pending_tasks(status);
	"#,
        )
        .context("failed to migrate database")?;

        db.execute_batch(companies::SCHEMA)
            .context("failed to migrate company tables")?;
        db.execute_batch(rounds::SCHEMA)
            .context("failed to migrate autopilot table")?;

        // The Go schema never created email_body although AddBrokerResponse writes it.
        let _ = db.execute(
            "ALTER TABLE broker_responses ADD COLUMN email_body TEXT",
            [],
        );
        // Autopilot columns (NULL for existing rows; errors = already present).
        let _ = db.execute(
            "ALTER TABLE pending_tasks ADD COLUMN skipped_until DATETIME",
            [],
        );
        let _ = db.execute(
            "ALTER TABLE removal_requests ADD COLUMN reminder_sent_at DATETIME",
            [],
        );
        Ok(())
    }

    /// Inserts the record and sets `record.id`. Uses the default pipeline_status ('email_sent').
    pub fn add(&self, record: &mut Record) -> Result<()> {
        let db = self.db();
        db.execute(
            "INSERT INTO removal_requests (broker_id, broker_name, email, template, status, message_id, error, sent_at, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                record.broker_id,
                record.broker_name,
                record.email,
                record.template,
                record.status,
                record.message_id,
                record.error,
                opt_time(&record.sent_at),
                now_str(),
            ],
        )
        .context("failed to insert record")?;
        record.id = db.last_insert_rowid();
        Ok(())
    }

    pub fn get_last_request_for_broker(&self, broker_id: &str) -> Result<Option<Record>> {
        let sql = format!(
            "SELECT {RECORD_COLUMNS} FROM removal_requests WHERE broker_id = ? ORDER BY sent_at DESC LIMIT 1"
        );
        self.db()
            .query_row(&sql, [broker_id], scan_record)
            .optional()
            .context("failed to query record")
    }

    pub fn get_recent_requests(&self, limit: i64) -> Result<Vec<Record>> {
        let sql =
            format!("SELECT {RECORD_COLUMNS} FROM removal_requests ORDER BY sent_at DESC LIMIT ?");
        let db = self.db();
        let mut stmt = db.prepare(&sql).context("failed to query records")?;
        let rows = stmt
            .query_map([limit], scan_record)
            .context("failed to query records")?;
        rows.collect::<rusqlite::Result<_>>()
            .context("failed to scan record")
    }

    /// Returns (total, sent, failed).
    pub fn get_stats(&self) -> Result<(i64, i64, i64)> {
        self.db()
            .query_row(
                "SELECT COUNT(*), SUM(CASE WHEN status='sent' THEN 1 ELSE 0 END),
                 SUM(CASE WHEN status='failed' THEN 1 ELSE 0 END) FROM removal_requests",
                [],
                |r| Ok((r.get(0)?, sum_col(r, 1)?, sum_col(r, 2)?)),
            )
            .context("failed to get stats")
    }

    /// Returns (sent, failed) for requests sent since the start of the current local month.
    pub fn get_monthly_stats(&self) -> Result<(i64, i64)> {
        let now = Local::now();
        let start = Local
            .with_ymd_and_hms(now.year(), now.month(), 1, 0, 0, 0)
            .earliest()
            .context("failed to compute start of month")?;
        self.db()
            .query_row(
                "SELECT SUM(CASE WHEN status='sent' THEN 1 ELSE 0 END),
                 SUM(CASE WHEN status='failed' THEN 1 ELSE 0 END) FROM removal_requests WHERE sent_at >= ?",
                [format_time(&start)],
                |r| Ok((sum_col(r, 0)?, sum_col(r, 1)?)),
            )
            .context("failed to get monthly stats")
    }

    pub fn get_all_broker_statuses(&self) -> Result<HashMap<String, BrokerStatus>> {
        let db = self.db();
        let mut stmt = db
            .prepare(
                "SELECT broker_id, MAX(sent_at) as last_sent,
                 (SELECT status FROM removal_requests r2 WHERE r2.broker_id = r.broker_id ORDER BY sent_at DESC LIMIT 1),
                 COUNT(*) FROM removal_requests r GROUP BY broker_id",
            )
            .context("failed to query broker statuses")?;
        let rows = stmt
            .query_map([], |r| {
                Ok(BrokerStatus {
                    broker_id: r.get(0)?,
                    last_sent: time_col(r, 1)?,
                    status: str_col(r, 2)?.parse().unwrap_or(Status::Pending),
                    total_sent: r.get(3)?,
                })
            })
            .context("failed to query broker statuses")?;
        let mut statuses = HashMap::new();
        for bs in rows {
            let bs = bs.context("failed to scan broker status")?;
            statuses.insert(bs.broker_id.clone(), bs);
        }
        Ok(statuses)
    }

    /// Deletes all records with the given status; returns the number deleted.
    pub fn delete_by_status(&self, status: Status) -> Result<usize> {
        self.db()
            .execute("DELETE FROM removal_requests WHERE status = ?", [status])
            .context("failed to delete records")
    }

    // ==================== Broker Response Methods ====================

    /// Stores a classified response and sets `resp.id`.
    pub fn add_broker_response(&self, resp: &mut BrokerResponse) -> Result<()> {
        let db = self.db();
        let now = now_str();
        db.execute(
            "INSERT INTO broker_responses (broker_id, broker_name, response_type, email_from, email_subject, email_body,
                form_url, confirm_url, confidence, needs_review, received_at, processed_at, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                resp.broker_id,
                resp.broker_name,
                resp.response_type,
                resp.email_from,
                resp.email_subject,
                resp.email_body,
                resp.form_url,
                resp.confirm_url,
                resp.confidence,
                resp.needs_review as i64,
                opt_time(&resp.received_at),
                now,
                now,
            ],
        )
        .context("failed to insert broker response")?;
        resp.id = db.last_insert_rowid();
        Ok(())
    }

    /// Finds an existing response by broker_id and email_subject.
    pub fn find_broker_response_by_subject(
        &self,
        broker_id: &str,
        subject: &str,
    ) -> Result<Option<BrokerResponse>> {
        let sql = format!(
            "SELECT {RESPONSE_COLUMNS} FROM broker_responses WHERE broker_id = ? AND email_subject = ? LIMIT 1"
        );
        self.db()
            .query_row(&sql, [broker_id, subject], scan_response)
            .optional()
            .context("failed to find broker response")
    }

    pub fn update_broker_response_classification(
        &self,
        id: i64,
        response_type: &str,
        form_url: &str,
        confirm_url: &str,
        confidence: f64,
        needs_review: bool,
    ) -> Result<()> {
        self.db()
            .execute(
                "UPDATE broker_responses SET response_type = ?, form_url = ?, confirm_url = ?,
                 confidence = ?, needs_review = ?, processed_at = ? WHERE id = ?",
                params![
                    response_type,
                    form_url,
                    confirm_url,
                    confidence,
                    needs_review as i64,
                    now_str(),
                    id
                ],
            )
            .context("failed to update broker response")?;
        Ok(())
    }

    pub fn update_broker_response_body(&self, id: i64, body: &str) -> Result<()> {
        self.db()
            .execute(
                "UPDATE broker_responses SET email_body = ? WHERE id = ?",
                params![body, id],
            )
            .context("failed to update broker response body")?;
        Ok(())
    }

    /// Removes all broker responses (for full re-scan).
    pub fn clear_broker_responses(&self) -> Result<()> {
        self.db()
            .execute("DELETE FROM broker_responses", [])
            .context("failed to clear broker responses")?;
        Ok(())
    }

    /// All broker responses including bodies, newest first (for reclassification).
    pub fn get_all_broker_responses(&self) -> Result<Vec<BrokerResponse>> {
        let sql =
            format!("SELECT {RESPONSE_COLUMNS} FROM broker_responses ORDER BY created_at DESC");
        let db = self.db();
        let mut stmt = db
            .prepare(&sql)
            .context("failed to query all broker responses")?;
        let rows = stmt
            .query_map([], scan_response)
            .context("failed to query all broker responses")?;
        rows.collect::<rusqlite::Result<_>>()
            .context("failed to scan broker response")
    }

    /// Responses filtered by type and/or needs_review, newest first. Like Go, the returned
    /// responses have an empty `email_body`.
    pub fn get_broker_responses(
        &self,
        response_type: Option<&str>,
        needs_review: bool,
        limit: i64,
    ) -> Result<Vec<BrokerResponse>> {
        let mut conds = Vec::new();
        let mut args: Vec<&dyn ToSql> = Vec::new();
        if let Some(rt) = &response_type {
            conds.push("response_type = ?");
            args.push(rt);
        }
        if needs_review {
            conds.push("needs_review = 1");
        }
        args.push(&limit);
        let where_clause = if conds.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", conds.join(" AND "))
        };
        let sql = format!(
            "SELECT id, broker_id, broker_name, response_type, email_from, email_subject, NULL,
             form_url, confirm_url, confidence, needs_review, received_at, processed_at, created_at
             FROM broker_responses {where_clause} ORDER BY created_at DESC LIMIT ?"
        );
        let db = self.db();
        let mut stmt = db
            .prepare(&sql)
            .context("failed to query broker responses")?;
        let rows = stmt
            .query_map(args.as_slice(), scan_response)
            .context("failed to query broker responses")?;
        rows.collect::<rusqlite::Result<_>>()
            .context("failed to scan broker response")
    }

    /// Counts of responses per response_type.
    pub fn get_response_stats(&self) -> Result<HashMap<String, i64>> {
        let db = self.db();
        let mut stmt = db
            .prepare("SELECT response_type, COUNT(*) FROM broker_responses GROUP BY response_type")
            .context("failed to query response stats")?;
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .context("failed to query response stats")?;
        rows.collect::<rusqlite::Result<_>>()
            .context("failed to scan response stat")
    }

    /// All detected forms (one per broker, most recent first) with their current status.
    pub fn get_forms_with_status(&self) -> Result<Vec<FormWithStatus>> {
        let db = self.db();
        let mut stmt = db
            .prepare(
                "SELECT
                    br.broker_id,
                    br.broker_name,
                    br.form_url,
                    br.email_subject,
                    br.created_at as detected_at,
                    COALESCE(pt.id, 0) as task_id,
                    COALESCE(pt.status, '') as task_status,
                    COALESCE(rr.pipeline_status, '') as pipeline_status,
                    COALESCE(pt.task_type, '') as task_type
                FROM broker_responses br
                LEFT JOIN pending_tasks pt ON br.broker_id = pt.broker_id AND pt.task_type IN ('captcha', 'manual_form')
                LEFT JOIN (
                    SELECT broker_id, pipeline_status
                    FROM removal_requests
                    WHERE id IN (SELECT MAX(id) FROM removal_requests GROUP BY broker_id)
                ) rr ON br.broker_id = rr.broker_id
                WHERE br.form_url IS NOT NULL AND br.form_url != ''
                ORDER BY br.created_at DESC",
            )
            .context("failed to query forms")?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    FormWithStatus {
                        broker_id: r.get(0)?,
                        broker_name: r.get(1)?,
                        form_url: str_col(r, 2)?,
                        email_subject: str_col(r, 3)?,
                        detected_at: time_col(r, 4)?,
                        status: String::new(),
                        task_id: r.get(5)?,
                        pipeline_status: None,
                    },
                    str_col(r, 6)?,
                    str_col(r, 7)?,
                    str_col(r, 8)?,
                ))
            })
            .context("failed to query forms")?;

        let mut forms = Vec::new();
        let mut seen = HashSet::new(); // Dedupe by broker_id, keep first (most recent)
        for row in rows {
            let (mut f, task_status, pipeline_status, task_type) =
                row.context("failed to scan form")?;
            if !seen.insert(f.broker_id.clone()) {
                continue;
            }
            f.pipeline_status = pipeline_status.parse().ok();
            f.status = match (task_status.as_str(), f.pipeline_status) {
                ("completed", _) => "filled",
                ("skipped", _) => "skipped",
                // A pending manual_form to-do leaves the form "pending".
                ("pending", _) if f.task_id > 0 && task_type == "captcha" => "captcha",
                (_, Some(PipelineStatus::FormFilled | PipelineStatus::Confirmed)) => "filled",
                (_, Some(PipelineStatus::Failed)) => "failed",
                (_, Some(PipelineStatus::Rejected)) => "skipped",
                _ => "pending",
            }
            .to_string();
            forms.push(f);
        }
        Ok(forms)
    }

    /// Returns (pending, filled, captcha, failed, skipped) form counts.
    pub fn get_form_stats(&self) -> Result<(i64, i64, i64, i64, i64)> {
        let mut c = (0, 0, 0, 0, 0);
        for f in self.get_forms_with_status()? {
            match f.status.as_str() {
                "pending" => c.0 += 1,
                "filled" => c.1 += 1,
                "captcha" => c.2 += 1,
                "failed" => c.3 += 1,
                "skipped" => c.4 += 1,
                _ => {}
            }
        }
        Ok(c)
    }

    // ==================== Pending Task Methods ====================

    /// Creates a new pending task (status "pending") and sets `task.id`.
    pub fn add_pending_task(&self, task: &mut PendingTask) -> Result<()> {
        let db = self.db();
        db.execute(
            "INSERT INTO pending_tasks (broker_id, broker_name, task_type, form_url, screenshot_path,
                browser_state, notes, status, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                task.broker_id,
                task.broker_name,
                task.task_type,
                task.form_url,
                task.screenshot_path,
                task.browser_state,
                task.notes,
                "pending",
                now_str(),
            ],
        )
        .context("failed to insert pending task")?;
        task.id = db.last_insert_rowid();
        Ok(())
    }

    /// Tasks filtered by type and/or status, newest first.
    pub fn get_pending_tasks(
        &self,
        task_type: Option<TaskType>,
        status: Option<&str>,
    ) -> Result<Vec<PendingTask>> {
        let mut conds = Vec::new();
        let mut args: Vec<&dyn ToSql> = Vec::new();
        if let Some(tt) = &task_type {
            conds.push("task_type = ?");
            args.push(tt);
        }
        if let Some(st) = &status {
            conds.push("status = ?");
            args.push(st);
        }
        let where_clause = if conds.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", conds.join(" AND "))
        };
        let sql = format!(
            "SELECT {TASK_COLUMNS} FROM pending_tasks {where_clause} ORDER BY created_at DESC"
        );
        let db = self.db();
        let mut stmt = db.prepare(&sql).context("failed to query pending tasks")?;
        let rows = stmt
            .query_map(args.as_slice(), scan_task)
            .context("failed to query pending tasks")?;
        rows.collect::<rusqlite::Result<_>>()
            .context("failed to scan pending task")
    }

    pub fn get_pending_task_by_id(&self, id: i64) -> Result<Option<PendingTask>> {
        let sql = format!("SELECT {TASK_COLUMNS} FROM pending_tasks WHERE id = ?");
        self.db()
            .query_row(&sql, [id], scan_task)
            .optional()
            .context("failed to query pending task")
    }

    /// Sets the task status (e.g. "completed", "skipped") and completed_at.
    pub fn complete_pending_task(&self, id: i64, status: &str) -> Result<()> {
        self.db()
            .execute(
                "UPDATE pending_tasks SET status = ?, completed_at = ? WHERE id = ?",
                params![status, now_str(), id],
            )
            .context("failed to complete pending task")?;
        Ok(())
    }

    /// Sets opened_at (only if not already set).
    pub fn mark_task_opened(&self, id: i64) -> Result<()> {
        self.db()
            .execute(
                "UPDATE pending_tasks SET opened_at = ? WHERE id = ? AND opened_at IS NULL",
                params![now_str(), id],
            )
            .context("failed to mark task opened")?;
        Ok(())
    }

    /// Returns (pending, completed, skipped) task counts.
    pub fn get_pending_task_stats(&self) -> Result<(i64, i64, i64)> {
        self.db()
            .query_row(
                "SELECT
                    SUM(CASE WHEN status='pending' THEN 1 ELSE 0 END),
                    SUM(CASE WHEN status='completed' THEN 1 ELSE 0 END),
                    SUM(CASE WHEN status='skipped' THEN 1 ELSE 0 END)
                 FROM pending_tasks",
                [],
                |r| Ok((sum_col(r, 0)?, sum_col(r, 1)?, sum_col(r, 2)?)),
            )
            .context("failed to get task stats")
    }

    // ==================== Pipeline Status Methods ====================

    /// Updates the pipeline status of the broker's most recent request.
    pub fn update_pipeline_status(&self, broker_id: &str, status: PipelineStatus) -> Result<()> {
        self.db()
            .execute(
                "UPDATE removal_requests SET pipeline_status = ? WHERE broker_id = ? AND id = (
                    SELECT id FROM removal_requests WHERE broker_id = ? ORDER BY sent_at DESC LIMIT 1
                )",
                params![status, broker_id, broker_id],
            )
            .context("failed to update pipeline status")?;
        Ok(())
    }

    /// Counts of each broker's latest request by pipeline status (NULL counts as email_sent).
    pub fn get_pipeline_stats(&self) -> Result<HashMap<PipelineStatus, i64>> {
        let db = self.db();
        let mut stmt = db
            .prepare(
                "SELECT pipeline_status, COUNT(*) FROM removal_requests
                 WHERE id IN (SELECT MAX(id) FROM removal_requests GROUP BY broker_id)
                 GROUP BY pipeline_status",
            )
            .context("failed to query pipeline stats")?;
        let rows = stmt
            .query_map([], |r| {
                Ok((r.get::<_, Option<String>>(0)?, r.get::<_, i64>(1)?))
            })
            .context("failed to query pipeline stats")?;
        let mut stats = HashMap::new();
        for row in rows {
            let (status, count) = row.context("failed to scan pipeline stat")?;
            // ponytail: unknown non-NULL values are folded into email_sent (Go keeps the raw string).
            let status = status
                .and_then(|s| s.parse().ok())
                .unwrap_or(PipelineStatus::EmailSent);
            *stats.entry(status).or_insert(0) += count;
        }
        Ok(stats)
    }
}

mod companies;
mod rounds;
pub use companies::{Company, CompanyAction, CompanyAggregate, ScannedMessage};
pub use rounds::{AutopilotRound, Progress, STALE_ROUND, TODO_TYPES};

#[cfg(test)]
mod tests;
