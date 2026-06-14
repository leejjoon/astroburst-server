use axum::{extract::State, http::StatusCode, Json};
use serde_json::{json, Value};

use crate::error::{AppError, Result};
use crate::state::AppState;

/// POST /sessions
///
/// Create a new session and return its ID.  Sessions hold an image cache
/// and a job registry; all other endpoints are scoped under
/// `/sessions/:sid/`.
///
/// Returns 503 when the server-wide session cap (`ASTROBURST_SESSION_MAX`,
/// default 8) is reached — caller should retry after existing sessions expire.
pub async fn create(State(state): State<AppState>) -> Result<(StatusCode, Json<Value>)> {
    let session = state.create_session().ok_or_else(|| {
        AppError::ServiceUnavailable(format!(
            "session cap reached (max {})",
            state.config.session_max
        ))
    })?;

    Ok((
        StatusCode::CREATED,
        Json(json!({ "session_id": session.id })),
    ))
}
