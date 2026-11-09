//! Port of internal/web/job.go: background send jobs and their on-disk persistence.

use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use tokio::sync::watch;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum JobStatus {
    #[default]
    Running,
    Completed,
    Cancelled,
    /// Paused due to daily limit
    Paused,
    /// Stopped due to auth/config error
    Error,
}

#[derive(Debug, Clone, Default)]
pub struct JobData {
    pub id: String,
    pub status: JobStatus,
    pub progress: i64,
    pub sent: i64,
    pub failed: i64,
    pub total: i64,
    pub current_broker: String,
    pub started_at: Option<DateTime<Local>>,
    pub completed_at: Option<DateTime<Local>>,
    pub error: String,
    pub error_type: String,
    pub daily_limit: i64,
    pub day_sent: i64,
    consecutive_auth_fails: u32,
}

pub struct Job {
    pub data: Mutex<JobData>,
    cancel: watch::Sender<bool>,
}

/// Go's time.Time JSON encoding (RFC 3339 with trimmed nanoseconds; zero time for None).
pub fn go_json_time(t: &Option<DateTime<Local>>) -> String {
    let Some(t) = t else {
        return "0001-01-01T00:00:00Z".into();
    };
    let mut s = t.format("%Y-%m-%dT%H:%M:%S").to_string();
    let nanos = t.timestamp_subsec_nanos();
    if nanos > 0 {
        s.push('.');
        s.push_str(format!("{nanos:09}").trim_end_matches('0'));
    }
    let off = t.format("%:z").to_string();
    s.push_str(if off == "+00:00" { "Z" } else { &off });
    s
}

impl Job {
    pub fn id(&self) -> String {
        self.data.lock().unwrap().id.clone()
    }

    pub fn status(&self) -> JobStatus {
        self.data.lock().unwrap().status
    }

    pub fn update(&self, sent: i64, failed: i64, current_broker: &str) {
        let mut j = self.data.lock().unwrap();
        j.sent = sent;
        j.failed = failed;
        j.current_broker = current_broker.to_string();
        if j.total > 0 {
            j.progress = ((sent + failed) * 100) / j.total;
        }
    }

    pub fn complete(&self) {
        let mut j = self.data.lock().unwrap();
        j.status = JobStatus::Completed;
        j.completed_at = Some(Local::now());
        j.progress = 100;
        j.current_broker.clear();
    }

    pub fn stop_with_error(&self, error_type: &str, msg: &str) {
        let mut j = self.data.lock().unwrap();
        j.status = JobStatus::Completed;
        j.completed_at = Some(Local::now());
        j.error = msg.to_string();
        j.error_type = error_type.to_string();
        j.current_broker.clear();
    }

    /// Returns true if the job should stop (3 consecutive auth failures).
    pub fn record_auth_failure(&self) -> bool {
        let mut j = self.data.lock().unwrap();
        j.consecutive_auth_fails += 1;
        j.consecutive_auth_fails >= 3
    }

    pub fn reset_auth_failures(&self) {
        self.data.lock().unwrap().consecutive_auth_fails = 0;
    }

    pub fn cancel(&self) {
        let mut j = self.data.lock().unwrap();
        if j.status == JobStatus::Running {
            j.status = JobStatus::Cancelled;
            j.completed_at = Some(Local::now());
            self.cancel.send_replace(true);
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.status() == JobStatus::Cancelled
    }

    /// Resolves once the job is cancelled (Go: `<-ctx.Done()`).
    pub async fn cancelled(&self) {
        let mut rx = self.cancel.subscribe();
        let _ = rx.wait_for(|c| *c).await;
    }

    pub fn to_json(&self) -> Value {
        let j = self.data.lock().unwrap();
        json!({
            "id": j.id,
            "status": j.status,
            "progress": j.progress,
            "sent": j.sent,
            "failed": j.failed,
            "total": j.total,
            "current_broker": j.current_broker,
            "started_at": go_json_time(&j.started_at),
            "completed_at": go_json_time(&j.completed_at),
            "error": j.error,
            "error_type": j.error_type,
            "daily_limit": j.daily_limit,
            "day_sent": j.day_sent,
        })
    }
}

pub fn new_uuid() -> String {
    let mut b: [u8; 16] = rand::random();
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..]
    )
}

#[derive(Default)]
pub struct JobManager {
    jobs: RwLock<HashMap<String, Arc<Job>>>,
}

impl JobManager {
    pub fn create(&self, total: i64) -> Arc<Job> {
        let id = new_uuid();
        let job = Arc::new(Job {
            data: Mutex::new(JobData {
                id: id.clone(),
                total,
                started_at: Some(Local::now()),
                ..Default::default()
            }),
            cancel: watch::channel(false).0,
        });
        self.jobs.write().unwrap().insert(id, job.clone());
        job
    }

    pub fn get(&self, id: &str) -> Option<Arc<Job>> {
        self.jobs.read().unwrap().get(id).cloned()
    }

    pub fn get_active(&self) -> Option<Arc<Job>> {
        self.jobs
            .read()
            .unwrap()
            .values()
            .find(|j| j.status() == JobStatus::Running)
            .cloned()
    }
}

/// A job that can be saved/loaded from disk.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default)]
pub struct PersistentJobState {
    pub id: String,
    pub status: JobStatus,
    pub sent: i64,
    pub failed: i64,
    pub total: i64,
    /// Go time.Time JSON
    pub started_at: String,
    /// Broker IDs still to process
    pub remaining_brokers: Vec<String>,
    pub search: String,
    pub category: String,
    pub region: String,
    pub status_filter: String,
}

pub struct JobPersistence {
    data_dir: PathBuf,
}

impl JobPersistence {
    pub fn new(data_dir: PathBuf) -> Self {
        Self { data_dir }
    }

    fn file_path(&self) -> PathBuf {
        self.data_dir.join("pending_job.json")
    }

    pub fn save(&self, state: &PersistentJobState) -> anyhow::Result<()> {
        crate::config::create_private_dir(&self.data_dir)?;
        let data = serde_json::to_vec_pretty(state)?;
        crate::config::write_with_mode(&self.file_path(), &data, 0o600)?;
        Ok(())
    }

    pub fn load(&self) -> anyhow::Result<Option<PersistentJobState>> {
        match std::fs::read(self.file_path()) {
            Ok(data) => Ok(Some(serde_json::from_slice(&data)?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn clear(&self) -> anyhow::Result<()> {
        match std::fs::remove_file(self.file_path()) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persistence_roundtrip_and_job_lifecycle() {
        let dir = tempfile::tempdir().unwrap();
        let p = JobPersistence::new(dir.path().join("d"));
        assert_eq!(p.load().unwrap(), None);
        let st = PersistentJobState {
            id: "x".into(),
            remaining_brokers: vec!["a".into()],
            total: 1,
            ..Default::default()
        };
        p.save(&st).unwrap();
        assert_eq!(p.load().unwrap(), Some(st));
        p.clear().unwrap();
        p.clear().unwrap();

        let jm = JobManager::default();
        let job = jm.create(4);
        assert_eq!(new_uuid().len(), 36);
        assert!(jm.get_active().is_some());
        job.update(1, 1, "b");
        assert_eq!(job.to_json()["progress"], 50);
        assert!(!job.record_auth_failure() && !job.record_auth_failure());
        assert!(job.record_auth_failure());
        job.cancel();
        assert!(job.is_cancelled() && jm.get_active().is_none());
        assert_eq!(job.to_json()["status"], "cancelled");
        assert_eq!(go_json_time(&None), "0001-01-01T00:00:00Z");
    }
}
