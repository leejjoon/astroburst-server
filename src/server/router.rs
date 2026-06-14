use axum::{
    extract::Request,
    http::HeaderValue,
    middleware::{self, Next},
    response::Response,
    routing::{self, get},
    Json, Router,
};
use serde_json::json;
use uuid::Uuid;

use super::handlers::{io, jobs, pipeline, render, stacking};
use super::state::AppState;

pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        // T7 — FITS I/O
        .route("/sessions/:sid/fits/open",   routing::post(io::fits_open))
        .route("/sessions/:sid/fits/header", routing::post(io::fits_header))
        // T8 — Render
        .route("/sessions/:sid/image/render",   routing::post(render::render))
        .route("/sessions/:sid/image/stf",      routing::post(render::auto_render))
        .route("/sessions/:sid/image/viewport", routing::post(render::viewport))
        // T10 — Job management + SSE
        .route(
            "/sessions/:sid/jobs/:jid",
            routing::get(jobs::get_job).delete(jobs::cancel_job),
        )
        .route("/sessions/:sid/jobs/:jid/stream", routing::get(jobs::stream_job))
        // T11 — Stacking
        .route("/sessions/:sid/stacking/stack",   routing::post(stacking::stack))
        .route("/sessions/:sid/stacking/drizzle", routing::post(stacking::drizzle))
        // T12 — Pipeline
        .route("/sessions/:sid/pipeline/run", routing::post(pipeline::run))
        .with_state(state)
        .layer(middleware::from_fn(request_id))
}

async fn health() -> Json<serde_json::Value> {
    Json(json!({ "status": "ok" }))
}

/// Echo or mint an X-Request-Id header on every response.
async fn request_id(req: Request, next: Next) -> Response {
    let id = req
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_owned())
        .unwrap_or_else(|| Uuid::new_v4().to_string());

    let mut res = next.run(req).await;
    if let Ok(val) = HeaderValue::from_str(&id) {
        res.headers_mut().insert("x-request-id", val);
    }
    res
}
