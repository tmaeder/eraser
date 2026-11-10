//! Server-side wizard sessions. The cookie only carries an
//! opaque 256-bit id; profile and credentials stay in memory on the server.

use crate::config::{AccountConfig, Config, Profile};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Default)]
pub struct Session {
    pub id: String,
    pub step: String,
    pub profile: Profile,
    pub account: AccountConfig,
    expires_at: Option<Instant>,
}

impl Session {
    /// The wizard's profile and account as a config (sending/inbox fields derived).
    pub fn config(&self) -> Config {
        let mut cfg = Config {
            profile: self.profile.clone(),
            account: self.account.clone(),
            ..Default::default()
        };
        cfg.apply_account();
        cfg
    }

    fn expired(&self, now: Instant) -> bool {
        self.expires_at.is_some_and(|t| now > t)
    }
}

pub struct SessionStore {
    sessions: Mutex<HashMap<String, Session>>,
    ttl: Duration,
}

pub(crate) fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand::fill(&mut buf[..]);
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

impl SessionStore {
    pub fn new(ttl: Duration) -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            ttl,
        }
    }

    pub fn create(&self) -> String {
        let id = random_hex(32);
        let session = Session {
            id: id.clone(),
            expires_at: Some(Instant::now() + self.ttl),
            ..Default::default()
        };
        self.sessions.lock().unwrap().insert(id.clone(), session);
        id
    }

    /// Returns a snapshot of the session, or None if missing or expired (expired ones are removed).
    pub fn get(&self, id: &str) -> Option<Session> {
        if id.is_empty() {
            return None;
        }
        let mut map = self.sessions.lock().unwrap();
        let session = map.get(id)?;
        if session.expired(Instant::now()) {
            map.remove(id);
            return None;
        }
        Some(session.clone())
    }

    /// Applies `update` and extends the expiry. Returns false if missing or expired.
    pub fn update(&self, id: &str, update: impl FnOnce(&mut Session)) -> bool {
        let mut map = self.sessions.lock().unwrap();
        let now = Instant::now();
        match map.get_mut(id) {
            Some(s) if !s.expired(now) => {
                update(s);
                s.expires_at = Some(now + self.ttl);
                true
            }
            Some(_) => {
                map.remove(id);
                false
            }
            None => false,
        }
    }

    pub fn delete(&self, id: &str) {
        self.sessions.lock().unwrap().remove(id);
    }

    pub fn cleanup(&self) {
        let now = Instant::now();
        self.sessions.lock().unwrap().retain(|_, s| !s.expired(now));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expiry_and_update() {
        let store = SessionStore::new(Duration::from_millis(30));
        let id = store.create();
        assert_eq!(id.len(), 64);
        assert!(store.update(&id, |s| s.step = "email".into()));
        assert_eq!(store.get(&id).unwrap().step, "email");
        std::thread::sleep(Duration::from_millis(40));
        assert!(store.get(&id).is_none());
        assert!(!store.update(&id, |_| {}));
        let id2 = store.create();
        std::thread::sleep(Duration::from_millis(40));
        store.cleanup();
        assert!(store.sessions.lock().unwrap().is_empty());
        assert!(store.get(&id2).is_none());
    }
}
