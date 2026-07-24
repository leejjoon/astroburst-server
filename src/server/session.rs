use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use dashmap::DashMap;
use serde::Serialize;
use tokio::sync::RwLock;
use uuid::Uuid;

use astroburst_lib::infra::cache::ImageCache;

use super::activity::ActivityLog;
use super::config::ServerConfig;
use super::job::{Job, JobId};

pub type SessionId = String;

/// Per-image metadata tracked by the v2 surface. `image_ref` is the bare
/// cache key the image is stored under (e.g. `"img_0"`, `"cutout_003"`); the
/// URL path already scopes it to a session, so it is *not* the doc's global
/// `"sid:name"` form.
#[derive(Debug, Clone, Serialize)]
pub struct ImageMeta {
    pub image_ref: String,
    /// Source file the image was loaded from (`None` for purely-derived
    /// products like cutouts once those slices land).
    pub source: Option<String>,
    /// HDU index this ref was loaded from, when it came from a specific HDU.
    pub hdu: Option<usize>,
    pub width: usize,
    pub height: usize,
    pub wcs_present: bool,
    pub extname: Option<String>,
}

/// Additive v2 state hung off every `Session`. Does not touch v1 behavior.
pub struct V2SessionState {
    /// Monotonic counter feeding auto-generated ref names (`img_0`, ...).
    counter: AtomicU64,
    /// The ref most-recently opened / switched-to, treated as "current".
    pub active_ref: RwLock<Option<String>>,
    /// Metadata for every ref registered in this session.
    pub meta: DashMap<String, ImageMeta>,
}

impl V2SessionState {
    pub fn new() -> Self {
        Self {
            counter: AtomicU64::new(0),
            active_ref: RwLock::new(None),
            meta: DashMap::new(),
        }
    }

    /// Allocate a fresh auto-name of the form `{prefix}_{n}` (e.g. `img_0`).
    pub fn next_ref(&self, prefix: &str) -> String {
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        format!("{prefix}_{n}")
    }
}

impl Default for V2SessionState {
    fn default() -> Self {
        Self::new()
    }
}

pub struct Session {
    pub id: SessionId,
    pub cache: Arc<ImageCache>,
    pub jobs: DashMap<JobId, Arc<Job>>,
    pub v2: V2SessionState,
    /// Bounded ring of recent requests against this session (issue #3).
    pub activity: ActivityLog,
    /// Wall-clock creation time, seconds since the unix epoch (for clients).
    pub created_unix: u64,
    last_accessed: RwLock<Instant>,
}

impl Session {
    pub fn new(id: SessionId, cfg: &ServerConfig) -> Arc<Self> {
        Arc::new(Self {
            id,
            cache: Arc::new(ImageCache::new(cfg.cache_max_entries, cfg.cache_max_bytes)),
            jobs: DashMap::new(),
            v2: V2SessionState::new(),
            activity: ActivityLog::new(),
            created_unix: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            last_accessed: RwLock::new(Instant::now()),
        })
    }

    pub async fn touch(&self) {
        *self.last_accessed.write().await = Instant::now();
    }

    /// Seconds since the last request touched this session.
    pub async fn idle_secs(&self) -> u64 {
        self.last_accessed.read().await.elapsed().as_secs()
    }

    /// Number of jobs currently in the Running state.
    pub fn running_jobs(&self) -> usize {
        self.jobs.iter().filter(|e| e.value().is_running()).count()
    }

    /// True when any job is still running. The TTL cleaner skips such sessions.
    pub fn has_active_jobs(&self) -> bool {
        self.jobs.iter().any(|e| e.value().is_running())
    }
}

pub struct SessionManager {
    sessions: Arc<DashMap<SessionId, Arc<Session>>>,
    config: Arc<ServerConfig>,
}

impl SessionManager {
    pub fn new(
        sessions: Arc<DashMap<SessionId, Arc<Session>>>,
        config: Arc<ServerConfig>,
    ) -> Self {
        Self { sessions, config }
    }

    /// Create a new session, capped at `config.session_max`. Returns `None` when full.
    pub fn create(&self) -> Option<Arc<Session>> {
        if self.sessions.len() >= self.config.session_max {
            return None;
        }
        let id = Uuid::new_v4().to_string();
        let session = Session::new(id.clone(), &self.config);
        self.sessions.insert(id, Arc::clone(&session));
        Some(session)
    }

    /// Spawn the background task that evicts sessions idle past `config.session_ttl`.
    /// Sessions with at least one Running job are always skipped.
    pub fn start_ttl_cleaner(
        sessions: Arc<DashMap<SessionId, Arc<Session>>>,
        config: Arc<ServerConfig>,
    ) {
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(config.cleanup_interval);
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
                    if now.duration_since(last) > config.session_ttl {
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
