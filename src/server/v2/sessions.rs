//! v2 session-lifecycle handlers: status, delete, keepalive.
//!
//! Session *creation* reuses the v1 `handlers::sessions::create` verbatim (the
//! router points `POST /v2/sessions` at it), so it is not re-implemented here.

use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::error::{AppError, Result};
use crate::extractors::SessionExtractor;
use crate::session::Session;
use crate::state::AppState;

/// GET /v2/sessions
///
/// Summaries of every live session, oldest first — the dashboard's left
/// rail (issue #3). Deliberately does *not* touch `last_accessed`: watching
/// the server must not keep idle sessions alive past their TTL.
pub async fn list(State(state): State<AppState>) -> Result<Json<Value>> {
    let sessions: Vec<Arc<Session>> =
        state.sessions.iter().map(|e| Arc::clone(e.value())).collect();

    let mut summaries = Vec::with_capacity(sessions.len());
    for s in &sessions {
        summaries.push(json!({
            "session_id": s.id,
            "created_unix": s.created_unix,
            "idle_secs": s.idle_secs().await,
            "active_ref": s.v2.active_ref.read().await.clone(),
            "image_count": s.v2.meta.len(),
            "cache_bytes": s.cache.memory_estimate_bytes(),
            "running_jobs": s.running_jobs(),
            // Highest activity seq — lets a poller spot new history without
            // fetching it.
            "last_seq": s.activity.last_seq(),
        }));
    }
    summaries.sort_by(|a, b| {
        let key = |v: &Value| {
            (
                v["created_unix"].as_u64().unwrap_or(0),
                v["session_id"].as_str().unwrap_or("").to_owned(),
            )
        };
        key(a).cmp(&key(b))
    });

    Ok(Json(json!({
        "count": summaries.len(),
        "sessions": summaries,
    })))
}

#[derive(Deserialize)]
pub struct HistoryParams {
    /// Return only events with `seq` strictly greater than this (default 0).
    #[serde(default)]
    pub since_seq: u64,
    /// Cap on returned events; the newest ones win (default: whole ring).
    pub limit: Option<usize>,
}

/// GET /v2/sessions/:sid/history?since_seq=N&limit=M
///
/// The session's activity ring, oldest first. Resolves the session manually
/// (not via `SessionExtractor`) so polling the history does not refresh the
/// idle TTL, and is itself excluded from recording (see `activity.rs`).
/// `first_seq > since_seq + 1` means the ring overflowed and events were lost.
pub async fn history(
    Path(sid): Path<String>,
    Query(params): Query<HistoryParams>,
    State(state): State<AppState>,
) -> Result<Json<Value>> {
    let session = state
        .sessions
        .get(&sid)
        .map(|e| Arc::clone(e.value()))
        .ok_or_else(|| AppError::NotFound(format!("session {sid} not found")))?;

    let (events, first_seq, last_seq) = session
        .activity
        .events_since(params.since_seq, params.limit.unwrap_or(usize::MAX));

    Ok(Json(json!({
        "session_id": session.id,
        "first_seq": first_seq,
        "last_seq": last_seq,
        "events": events,
    })))
}

/// GET /v2/sessions/:sid
///
/// Session status: active ref, number of registered images, and memory
/// footprint of the session's image cache.
pub async fn status(SessionExtractor(session): SessionExtractor) -> Result<Json<Value>> {
    let active = session.v2.active_ref.read().await.clone();
    Ok(Json(json!({
        "session_id": session.id,
        "active_ref": active,
        "image_count": session.v2.meta.len(),
        "cache_bytes": session.cache.memory_estimate_bytes(),
    })))
}

/// DELETE /v2/sessions/:sid
///
/// Remove the session (and everything hanging off it) from the server. A
/// subsequent request against the id 404s via `SessionExtractor`.
pub async fn delete(
    SessionExtractor(session): SessionExtractor,
    State(state): State<AppState>,
) -> Result<StatusCode> {
    state.sessions.remove(&session.id);
    Ok(StatusCode::NO_CONTENT)
}

/// POST /v2/sessions/:sid/keepalive
///
/// No-op: `SessionExtractor` already refreshed `last_accessed` on resolution.
/// Returns 200 so the caller can confirm the session is still alive.
pub async fn keepalive(SessionExtractor(session): SessionExtractor) -> Result<Json<Value>> {
    Ok(Json(json!({ "session_id": session.id, "status": "ok" })))
}
