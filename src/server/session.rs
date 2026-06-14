use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use tokio::sync::RwLock;
use uuid::Uuid;

use astroburst_lib::infra::cache::ImageCache;

use super::job::{Job, JobId};
use super::state::MAX_SESSIONS;

pub type SessionId = String;

const SESSION_MAX_ENTRIES: usize = 32;
const SESSION_MAX_BYTES: usize = 2 * 1024 * 1024 * 1024; // 2 GB per session
const SESSION_TTL: Duration = Duration::from_secs(15 * 60);
const TTL_SWEEP_INTERVAL: Duration = Duration::from_secs(30);

pub struct Session {
    pub id: SessionId,
    pub cache: Arc<ImageCache>,
    pub jobs: DashMap<JobId, Arc<Job>>,
    last_accessed: RwLock<Instant>,
}

impl Session {
    pub fn new(id: SessionId) -> Arc<Self> {
        Arc::new(Self {
            id,
            cache: Arc::new(ImageCache::new(SESSION_MAX_ENTRIES, SESSION_MAX_BYTES)),
            jobs: DashMap::new(),
            last_accessed: RwLock::new(Instant::now()),
        })
    }

    pub async fn touch(&self) {
        *self.last_accessed.write().await = Instant::now();
    }

    /// True when any job is still running. The TTL cleaner skips such sessions.
    pub fn has_active_jobs(&self) -> bool {
        self.jobs.iter().any(|e| e.value().is_running())
    }
}

pub struct SessionManager {
    sessions: Arc<DashMap<SessionId, Arc<Session>>>,
}

impl SessionManager {
    pub fn new(sessions: Arc<DashMap<SessionId, Arc<Session>>>) -> Self {
        Self { sessions }
    }

    /// Create a new session, capped at MAX_SESSIONS. Returns None when full.
    pub fn create(&self) -> Option<Arc<Session>> {
        if self.sessions.len() >= MAX_SESSIONS {
            return None;
        }
        let id = Uuid::new_v4().to_string();
        let session = Session::new(id.clone());
        self.sessions.insert(id, Arc::clone(&session));
        Some(session)
    }

    /// Spawn the background task that evicts sessions idle past SESSION_TTL.
    /// Sessions with at least one Running job are always skipped.
    pub fn start_ttl_cleaner(sessions: Arc<DashMap<SessionId, Arc<Session>>>) {
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(TTL_SWEEP_INTERVAL);
            loop {
                interval.tick().await;
                let now = Instant::now();
                let mut expired: Vec<SessionId> = Vec::new();
                for entry in sessions.iter() {
                    let s = entry.value();
                    if s.has_active_jobs() {
                        continue;
                    }
                    let last = *s.last_accessed.read().await;
                    if now.duration_since(last) > SESSION_TTL {
                        expired.push(entry.key().clone());
                    }
                }
                for id in &expired {
                    sessions.remove(id);
                    log::info!("session {} evicted (idle TTL)", id);
                }
            }
        });
    }
}
