use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{FromRequestParts, Path};
use axum::http::request::Parts;

use super::error::AppError;
use super::session::Session;
use super::state::AppState;

/// Resolves the `:sid` path segment to a live `Arc<Session>`.
///
/// Touches `last_accessed` on every successful resolution so the idle TTL
/// resets naturally with request activity. Returns 404 when the session id
/// is absent from the sessions map.
pub struct SessionExtractor(pub Arc<Session>);

#[axum::async_trait]
impl FromRequestParts<AppState> for SessionExtractor {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let Path(params) = Path::<HashMap<String, String>>::from_request_parts(parts, state)
            .await
            .map_err(|e| AppError::BadRequest(format!("invalid path: {e}")))?;

        let sid = params
            .get("sid")
            .ok_or_else(|| AppError::BadRequest("route missing :sid segment".into()))?
            .clone();

        let session = state
            .sessions
            .get(&sid)
            .map(|e| Arc::clone(e.value()))
            .ok_or_else(|| AppError::NotFound(format!("session {sid} not found")))?;

        session.touch().await;
        Ok(SessionExtractor(session))
    }
}
