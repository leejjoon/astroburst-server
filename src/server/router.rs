use axum::{
    extract::{Request, State},
    http::HeaderValue,
    middleware::{self, Next},
    response::Response,
    routing::{self, get},
    Json, Router,
};
use serde_json::json;
use std::sync::atomic::Ordering;
use uuid::Uuid;

use super::handlers::{io, jobs, pipeline, render, sessions, stacking};
use super::state::AppState;
use super::v2;

pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/sessions", routing::post(sessions::create))
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
        // ── v2 API: sessions & image lifecycle (issue #2) ────────────────────
        // Session creation reuses the v1 handler verbatim.
        .route("/v2/sessions", routing::post(sessions::create))
        .route(
            "/v2/sessions/:sid",
            get(v2::sessions::status).delete(v2::sessions::delete),
        )
        .route("/v2/sessions/:sid/keepalive", routing::post(v2::sessions::keepalive))
        .route("/v2/sessions/:sid/open", routing::post(v2::images::open))
        .route("/v2/sessions/:sid/hdu", routing::post(v2::images::switch_hdu))
        .route("/v2/sessions/:sid/images", get(v2::images::list_images))
        // ── v2 API: cutout — region crop into a derived ref (issue #5) ───────
        .route("/v2/sessions/:sid/cutout", routing::post(v2::cutout::cutout))
        // ── v2 API: WCS coordinate transforms (issue #3) ─────────────────────
        .route("/v2/sessions/:sid/wcs/pix2sky", routing::post(v2::wcs::pix2sky))
        .route("/v2/sessions/:sid/wcs/sky2pix", routing::post(v2::wcs::sky2pix))
        .route("/v2/sessions/:sid/wcs/separation", routing::post(v2::wcs::separation))
        .with_state(state)
        .layer(middleware::from_fn(request_id))
}

/// GET /health — PRD §7.3
async fn health(State(state): State<AppState>) -> Json<serde_json::Value> {
    Json(json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        "sessions_active": state.sessions.len(),
        "sessions_total": state.created_total.load(Ordering::Relaxed),
    }))
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
