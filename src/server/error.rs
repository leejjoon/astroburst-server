use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

pub type Result<T> = std::result::Result<T, AppError>;

#[derive(Debug)]
pub enum AppError {
    NotFound(String),
    Conflict(String),
    TooManyRequests,
    BadRequest(String),
    /// A 400 that carries a stable machine-readable `code` and an optional
    /// human-facing `hint` alongside the message. Used by the v2 surface (e.g.
    /// `region_out_of_bounds`, `wcs_required`) so agents can branch on `code`
    /// while still surfacing the `hint` to a human.
    BadRequestWithHint {
        code: &'static str,
        message: String,
        hint: Option<String>,
    },
    ServiceUnavailable(String),
    Internal(anyhow::Error),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        // The hint (when present) is threaded into the error body separately so
        // it doesn't lose its structure by being folded into the message.
        let mut hint: Option<String> = None;
        let (status, code, msg) = match self {
            AppError::NotFound(m) => (StatusCode::NOT_FOUND, "not_found", m),
            AppError::Conflict(m) => (StatusCode::CONFLICT, "conflict", m),
            AppError::TooManyRequests => (
                StatusCode::TOO_MANY_REQUESTS,
                "too_many_requests",
                "job queue full (max 4 concurrent)".into(),
            ),
            AppError::BadRequest(m) => (StatusCode::BAD_REQUEST, "bad_request", m),
            AppError::BadRequestWithHint { code, message, hint: h } => {
                hint = h;
                (StatusCode::BAD_REQUEST, code, message)
            }
            AppError::ServiceUnavailable(m) => (StatusCode::SERVICE_UNAVAILABLE, "service_unavailable", m),
            AppError::Internal(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                format!("{:#}", e),
            ),
        };
        let mut err = json!({ "code": code, "message": msg });
        if let Some(h) = hint {
            err["hint"] = json!(h);
        }
        (status, Json(json!({ "success": false, "error": err }))).into_response()
    }
}

impl From<anyhow::Error> for AppError {
    fn from(e: anyhow::Error) -> Self {
        AppError::Internal(e)
    }
}
