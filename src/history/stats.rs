//! Read-only status queries for the web UI: the state of each broker's latest request (status
//! chips, deadline bars, stage filters), the Sent → Replied → Deleted funnel and per-broker
//! event timelines.

use super::{
    INTERRUPTED, PipelineStatus, RECORD_COLUMNS, RESPONSE_COLUMNS, Status, Store, TASK_COLUMNS,
    TODO_TYPES, scan_record, scan_response, scan_task, str_col, time_col,
};
use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use serde::Serialize;
use std::collections::HashMap;

pub const INTERRUPTED_LABEL: &str = "Interrupted — check Sent folder";

/// GDPR / FADP answer deadline in days after sending.
pub const DEADLINE_DAYS: i64 = 30;

/// One status, shown the same way on every page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChipKind {
    Deleted,
    Waiting,
    Reminder,
    Rejected,
    Action,
    Failed,
    /// Recorded as pending before sending, result never stored (crash mid-send).
    Interrupted,
    NotContacted,
}

impl ChipKind {
    /// CSS / filter key.
    pub fn key(self) -> &'static str {
        match self {
            Self::Deleted => "deleted",
            Self::Waiting => "waiting",
            Self::Reminder => "reminder",
            Self::Rejected => "rejected",
            Self::Action => "action",
            Self::Failed => "failed",
            Self::Interrupted => "interrupted",
            Self::NotContacted => "none",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Chip {
    pub kind: &'static str,
    pub label: String,
}

impl Chip {
    pub fn not_contacted() -> Self {
        Self {
            kind: ChipKind::NotContacted.key(),
            label: "Not contacted".into(),
        }
    }

    /// Chip for one history row (older rows than the broker's latest one).
    pub fn for_record(r: &super::Record) -> Option<Self> {
        match r.status {
            Status::Failed => Some(Self::failed()),
            Status::Pending if r.error == INTERRUPTED => Some(Self {
                kind: ChipKind::Interrupted.key(),
                label: INTERRUPTED_LABEL.into(),
            }),
            _ => None,
        }
    }

    pub fn failed() -> Self {
        Self {
            kind: ChipKind::Failed.key(),
            label: "Failed".into(),
        }
    }
}

/// The latest request to one broker (or `company:` id), with its reply and open to-do counts.
#[derive(Debug, Clone, PartialEq)]
pub struct RequestState {
    pub broker_id: String,
    pub record_id: i64,
    pub status: Status,
    pub error: String,
    pub pipeline: Option<PipelineStatus>,
    pub sent_at: Option<DateTime<Local>>,
    pub reminder_sent_at: Option<DateTime<Local>>,
    pub replies: i64,
    /// Pending to-dos (Today types) for this broker.
    pub open_tasks: i64,
}

impl RequestState {
    pub fn kind(&self) -> ChipKind {
        match (self.status, self.pipeline) {
            (Status::Pending, _) if self.error == INTERRUPTED => ChipKind::Interrupted,
            (Status::Failed, _) | (_, Some(PipelineStatus::Failed)) => ChipKind::Failed,
            (_, Some(PipelineStatus::Confirmed)) => ChipKind::Deleted,
            (_, Some(PipelineStatus::Rejected)) => ChipKind::Rejected,
            _ if self.open_tasks > 0 => ChipKind::Action,
            _ if self.reminder_sent_at.is_some() => ChipKind::Reminder,
            _ => ChipKind::Waiting,
        }
    }

    /// Calendar days since sending (at least 1).
    pub fn day(&self, now: DateTime<Local>) -> i64 {
        self.sent_at
            .map(|s| (now.date_naive() - s.date_naive()).num_days())
            .unwrap_or(0)
            .max(1)
    }

    pub fn chip(&self, now: DateTime<Local>) -> Chip {
        let kind = self.kind();
        let label = match kind {
            ChipKind::Deleted => "Deleted".into(),
            ChipKind::Waiting => format!("Waiting · day {}/{DEADLINE_DAYS}", self.day(now)),
            ChipKind::Reminder => "Reminder sent".into(),
            ChipKind::Rejected => "Rejected".into(),
            ChipKind::Action => "Needs action".into(),
            ChipKind::Failed => "Failed".into(),
            ChipKind::Interrupted => INTERRUPTED_LABEL.into(),
            ChipKind::NotContacted => "Not contacted".into(),
        };
        Chip {
            kind: kind.key(),
            label,
        }
    }

    pub fn is_sent(&self) -> bool {
        self.status == Status::Sent
    }

    /// Still within the request lifecycle (deadline bar applies).
    pub fn is_open(&self) -> bool {
        matches!(
            self.kind(),
            ChipKind::Waiting | ChipKind::Reminder | ChipKind::Action
        )
    }

    fn resolved(&self) -> bool {
        matches!(
            self.pipeline,
            Some(PipelineStatus::Confirmed | PipelineStatus::Rejected)
        )
    }

    /// Reminder sent and still no reply at all.
    pub fn no_response(&self) -> bool {
        self.is_sent() && self.reminder_sent_at.is_some() && self.replies == 0 && !self.resolved()
    }

    /// Funnel stage / status filter (`""` matches everything; unknown keys match nothing).
    pub fn matches_stage(&self, stage: &str) -> bool {
        let k = self.kind();
        match stage {
            "" => true,
            "sent" => self.is_sent(),
            "replied" => self.is_sent() && self.replies > 0,
            "deleted" => self.is_sent() && k == ChipKind::Deleted,
            "rejected" => self.is_sent() && k == ChipKind::Rejected,
            "action" => self.is_sent() && k == ChipKind::Action,
            "no_response" => self.no_response(),
            "waiting" => k == ChipKind::Waiting,
            "reminder" => k == ChipKind::Reminder,
            "failed" => k == ChipKind::Failed,
            "interrupted" => k == ChipKind::Interrupted,
            _ => false,
        }
    }
}

/// Stage filters the funnel links to, with their human labels.
pub const STAGES: &[(&str, &str)] = &[
    ("sent", "Sent"),
    ("replied", "Replied"),
    ("deleted", "Deleted"),
    ("rejected", "Rejected"),
    ("action", "Needs you"),
    ("no_response", "No response"),
    ("waiting", "Waiting"),
    ("reminder", "Reminder sent"),
    ("failed", "Failed"),
    ("interrupted", "Interrupted"),
];

pub fn stage_label(stage: &str) -> Option<&'static str> {
    STAGES.iter().find(|(k, _)| *k == stage).map(|(_, l)| *l)
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Funnel {
    pub sent: i64,
    pub replied: i64,
    pub deleted: i64,
    pub rejected: i64,
    pub needs_you: i64,
    pub no_response: i64,
}

impl Funnel {
    pub fn of<'a>(states: impl IntoIterator<Item = &'a RequestState>) -> Self {
        let mut f = Self::default();
        for s in states {
            let add = |stage: &str, n: &mut i64| *n += s.matches_stage(stage) as i64;
            add("sent", &mut f.sent);
            add("replied", &mut f.replied);
            add("deleted", &mut f.deleted);
            add("rejected", &mut f.rejected);
            add("action", &mut f.needs_you);
            add("no_response", &mut f.no_response);
        }
        f
    }
}

/// One entry of a broker's timeline.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TimelineEvent {
    pub at: Option<DateTime<Local>>,
    /// sent, failed, reminder, reply, confirm, task, task_done
    pub kind: &'static str,
    pub title: String,
    pub detail: String,
}

fn response_label(t: &str) -> &'static str {
    match t {
        "success" => "deletion confirmed",
        "rejected" => "request rejected",
        "form_required" => "web form required",
        "confirmation_required" => "confirmation link",
        "pending" => "processing",
        "bounced" => "bounced",
        _ => "needs review",
    }
}

fn task_label(t: super::TaskType) -> &'static str {
    use super::TaskType::*;
    match t {
        ManualForm => "web form",
        Captcha => "captcha",
        Confirm => "confirmation link",
        IdRequired => "ID requested",
        UnsubscribeLink => "unsubscribe link",
        Review => "review reply",
    }
}

impl Store {
    /// The latest request per broker id (brokers and `company:` ids alike).
    pub fn request_states(&self) -> Result<HashMap<String, RequestState>> {
        let todo = TODO_TYPES
            .iter()
            .map(|t| format!("'{}'", t.as_str()))
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT r.broker_id, r.id, r.status, r.pipeline_status, r.sent_at, r.reminder_sent_at,
                r.error,
                (SELECT COUNT(*) FROM broker_responses b WHERE b.broker_id = r.broker_id),
                (SELECT COUNT(*) FROM pending_tasks t WHERE t.broker_id = r.broker_id
                    AND t.status = 'pending' AND t.task_type IN ({todo}))
             FROM removal_requests r
             WHERE r.id IN (SELECT MAX(id) FROM removal_requests GROUP BY broker_id)"
        );
        let db = self.db();
        let mut stmt = db.prepare(&sql).context("failed to query request states")?;
        let rows = stmt.query_map([], |r| {
            Ok(RequestState {
                broker_id: r.get(0)?,
                record_id: r.get(1)?,
                status: str_col(r, 2)?.parse().unwrap_or(Status::Pending),
                pipeline: str_col(r, 3)?.parse().ok(),
                sent_at: time_col(r, 4)?,
                reminder_sent_at: time_col(r, 5)?,
                error: str_col(r, 6)?,
                replies: r.get(7)?,
                open_tasks: r.get(8)?,
            })
        })?;
        rows.map(|s| s.map(|s| (s.broker_id.clone(), s)))
            .collect::<rusqlite::Result<_>>()
            .context("failed to read request states")
    }

    /// Requests, reminders, replies and to-dos of one broker id, oldest first.
    pub fn broker_timeline(&self, broker_id: &str) -> Result<Vec<TimelineEvent>> {
        let db = self.db();
        let mut events = Vec::new();

        let sql = format!(
            "SELECT {RECORD_COLUMNS}, reminder_sent_at FROM removal_requests WHERE broker_id = ?"
        );
        let mut stmt = db.prepare(&sql)?;
        let rows = stmt.query_map([broker_id], |r| Ok((scan_record(r)?, time_col(r, 11)?)))?;
        for row in rows {
            let (rec, reminder) = row.context("failed to read requests")?;
            let (kind, title, detail) = match rec.status {
                Status::Sent => ("sent", "Deletion request sent", rec.email.clone()),
                Status::Failed => ("failed", "Sending failed", rec.error.clone()),
                Status::Pending if rec.error == INTERRUPTED => {
                    ("failed", INTERRUPTED_LABEL, rec.error.clone())
                }
                Status::Pending => ("sent", "Sending…", rec.email.clone()),
            };
            events.push(TimelineEvent {
                at: rec.sent_at.or(rec.created_at),
                kind,
                title: title.into(),
                detail,
            });
            if let Some(at) = reminder {
                events.push(TimelineEvent {
                    at: Some(at),
                    kind: "reminder",
                    title: "Reminder sent".into(),
                    detail: String::new(),
                });
            }
        }

        let sql = format!("SELECT {RESPONSE_COLUMNS} FROM broker_responses WHERE broker_id = ?");
        let mut stmt = db.prepare(&sql)?;
        for resp in stmt.query_map([broker_id], scan_response)? {
            let resp = resp.context("failed to read replies")?;
            events.push(TimelineEvent {
                at: resp.received_at.or(resp.created_at),
                kind: "reply",
                title: format!("Reply: {}", response_label(&resp.response_type)),
                detail: resp.email_subject,
            });
        }

        let sql = format!("SELECT {TASK_COLUMNS} FROM pending_tasks WHERE broker_id = ?");
        let mut stmt = db.prepare(&sql)?;
        for task in stmt.query_map([broker_id], scan_task)? {
            let t = task.context("failed to read tasks")?;
            let label = task_label(t.task_type);
            let auto_confirm = t.task_type == super::TaskType::Confirm
                && t.notes.starts_with("Confirmed automatically");
            if auto_confirm {
                events.push(TimelineEvent {
                    at: t.created_at,
                    kind: "confirm",
                    title: "Confirmation link opened".into(),
                    detail: t.notes,
                });
                continue;
            }
            events.push(TimelineEvent {
                at: t.created_at,
                kind: "task",
                title: format!("To-do: {label}"),
                detail: t.notes.clone(),
            });
            if let Some(done) = t.completed_at {
                let what = if t.status == "skipped" {
                    "skipped"
                } else {
                    "done"
                };
                events.push(TimelineEvent {
                    at: Some(done),
                    kind: "task_done",
                    title: format!("To-do {what}: {label}"),
                    detail: String::new(),
                });
            }
        }

        events.sort_by_key(|e| e.at);
        Ok(events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::{BrokerResponse, PendingTask, Record, TaskType};
    use chrono::TimeDelta;

    fn send(store: &Store, id: &str, status: Status, days_ago: i64) -> Record {
        let mut r = Record {
            broker_id: id.into(),
            broker_name: id.into(),
            email: format!("privacy@{id}.test"),
            status,
            sent_at: Some(Local::now() - TimeDelta::days(days_ago)),
            ..Default::default()
        };
        store.add(&mut r).unwrap();
        r
    }

    fn reply(store: &Store, id: &str, t: &str) {
        store
            .add_broker_response(&mut BrokerResponse {
                broker_id: id.into(),
                broker_name: id.into(),
                response_type: t.into(),
                email_subject: format!("Re: {id}"),
                received_at: Some(Local::now()),
                ..Default::default()
            })
            .unwrap();
    }

    #[test]
    fn states_chips_funnel_and_timeline() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join("h.db")).unwrap();
        send(&store, "waiting", Status::Sent, 12);
        send(&store, "deleted", Status::Sent, 20);
        reply(&store, "deleted", "success");
        store
            .update_pipeline_status("deleted", PipelineStatus::Confirmed)
            .unwrap();
        send(&store, "rejected", Status::Sent, 20);
        reply(&store, "rejected", "rejected");
        store
            .update_pipeline_status("rejected", PipelineStatus::Rejected)
            .unwrap();
        let r = send(&store, "silent", Status::Sent, 40);
        store.mark_reminder_sent(r.id).unwrap();
        send(&store, "form", Status::Sent, 5);
        reply(&store, "form", "form_required");
        store
            .add_pending_task(&mut PendingTask {
                broker_id: "form".into(),
                broker_name: "form".into(),
                task_type: TaskType::ManualForm,
                ..Default::default()
            })
            .unwrap();
        send(&store, "broken", Status::Failed, 1);
        // a failed first attempt followed by a successful resend counts as the resend
        send(&store, "retry", Status::Failed, 3);
        send(&store, "retry", Status::Sent, 2);
        send(&store, "company:shop.test", Status::Sent, 2);
        let mut cut = Record {
            broker_id: "cut".into(),
            broker_name: "cut".into(),
            status: Status::Pending,
            sent_at: Some(Local::now() - TimeDelta::hours(3)),
            ..Default::default()
        };
        store.add(&mut cut).unwrap();
        store
            .flag_interrupted_sends(Local::now() - TimeDelta::hours(1))
            .unwrap();

        let s = store.request_states().unwrap();
        let now = Local::now();
        let chip = |id: &str| s[id].chip(now);
        assert_eq!(chip("waiting").label, "Waiting · day 12/30");
        assert_eq!(chip("deleted").kind, "deleted");
        assert_eq!(chip("rejected").kind, "rejected");
        assert_eq!(chip("silent").label, "Reminder sent");
        assert_eq!(chip("form").label, "Needs action");
        assert_eq!(chip("broken"), Chip::failed());
        assert_eq!(chip("retry").kind, "waiting");
        assert_eq!(chip("cut").label, INTERRUPTED_LABEL);
        assert!(!s["cut"].matches_stage("sent") && s["cut"].matches_stage("interrupted"));
        assert!(s["silent"].no_response() && !s["waiting"].no_response());
        assert!(s["form"].is_open() && !s["deleted"].is_open());

        let brokers = Funnel::of(s.values().filter(|x| !x.broker_id.starts_with("company:")));
        assert_eq!(
            brokers,
            Funnel {
                sent: 6,
                replied: 3,
                deleted: 1,
                rejected: 1,
                needs_you: 1,
                no_response: 1,
            }
        );
        let companies = Funnel::of(s.values().filter(|x| x.broker_id.starts_with("company:")));
        assert_eq!((companies.sent, companies.replied), (1, 0));
        assert!(s["form"].matches_stage("replied") && !s["form"].matches_stage("bogus"));
        assert_eq!(stage_label("no_response"), Some("No response"));

        let t = store.broker_timeline("form").unwrap();
        let kinds: Vec<_> = t.iter().map(|e| e.kind).collect();
        assert_eq!(kinds, ["sent", "reply", "task"]);
        assert_eq!(t[1].title, "Reply: web form required");
        let t = store.broker_timeline("silent").unwrap();
        assert_eq!(t.last().unwrap().kind, "reminder");
        let t = store.broker_timeline("retry").unwrap();
        assert_eq!(t[0].kind, "failed");
    }
}
