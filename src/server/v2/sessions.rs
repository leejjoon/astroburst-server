//! v2 session-lifecycle handlers: status, delete, keepalive.
//!
//! Session *creation* reuses the v1 `handlers::sessions::create` verbatim (the
//! router points `POST /v2/sessions` at it), so it is not re-implemented here.

use axum::{extract::State, http::StatusCode, Json};
use serde_json::{json, Value};

use crate::error::Result;
use crate::extractors::SessionExtractor;
use crate::state::AppState;

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
