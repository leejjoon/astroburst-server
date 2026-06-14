use std::sync::Arc;

use dashmap::DashMap;
use tokio::sync::Semaphore;

use super::session::{Session, SessionId, SessionManager};

pub const MAX_SESSIONS: usize = 8;
pub const MAX_CONCURRENT_JOBS: usize = 4;

#[derive(Clone)]
pub struct AppState {
    pub sessions: Arc<DashMap<SessionId, Arc<Session>>>,
    pub job_semaphore: Arc<Semaphore>,
}

impl AppState {
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(DashMap::new()),
            job_semaphore: Arc::new(Semaphore::new(MAX_CONCURRENT_JOBS)),
        }
    }

    /// Create a new session, returning None when at the MAX_SESSIONS cap.
    pub fn create_session(&self) -> Option<Arc<Session>> {
        SessionManager::new(Arc::clone(&self.sessions)).create()
    }
}
