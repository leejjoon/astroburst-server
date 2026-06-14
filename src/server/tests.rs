use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use tower::ServiceExt;

use crate::job::new_job;
use crate::router::build_router;
use crate::session::Session;
use crate::state::AppState;

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

// ── 1. Health ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn health_returns_ok() {
    let app = build_router(AppState::new());
    let resp = app
        .oneshot(Request::builder().uri("/health").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["status"], "ok");
}

// ── 2. X-Request-Id pass-through ─────────────────────────────────────────────

#[tokio::test]
async fn request_id_echoed() {
    let app = build_router(AppState::new());
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/health")
                .header("x-request-id", "echo-me-123")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.headers()
            .get("x-request-id")
            .and_then(|v| v.to_str().ok()),
        Some("echo-me-123")
    );
}

// ── 3. Session not found → 404 ────────────────────────────────────────────────

#[tokio::test]
async fn unknown_session_returns_404() {
    let app = build_router(AppState::new());
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/sessions/no-such-sid/jobs/any-jid")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ── 4. fits/header — slot not in cache → 404 ─────────────────────────────────

#[tokio::test]
async fn fits_header_unknown_slot_returns_404() {
    let state = AppState::new();
    state.sessions.insert("sid-h".into(), Session::new("sid-h".into()));

    let app = build_router(state);
    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/sessions/sid-h/fits/header")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"slot":"ghost"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ── 5. GET job — returns Running status ──────────────────────────────────────

#[tokio::test]
async fn get_job_returns_running_status() {
    let state = AppState::new();
    let session = Session::new("sid-g".into());
    let (job, _tx) = new_job("test-action");
    let jid = job.id.clone();
    session.jobs.insert(jid.clone(), Arc::clone(&job));
    state.sessions.insert("sid-g".into(), session);

    let app = build_router(state);
    let resp = app
        .oneshot(
            Request::builder()
                .uri(format!("/sessions/sid-g/jobs/{jid}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["status"], "running");
    assert_eq!(json["action"], "test-action");
}

// ── 6. DELETE job — cancels Running job ──────────────────────────────────────

#[tokio::test]
async fn cancel_job_sets_cancelled() {
    let state = AppState::new();
    let session = Session::new("sid-c".into());
    let (job, _tx) = new_job("to-cancel");
    let jid = job.id.clone();
    session.jobs.insert(jid.clone(), Arc::clone(&job));
    state.sessions.insert("sid-c".into(), session);

    let app = build_router(state);
    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::DELETE)
                .uri(format!("/sessions/sid-c/jobs/{jid}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["status"], "cancelled");
}

// ── 7. SSE stream — second subscriber → 409 ──────────────────────────────────

#[tokio::test]
async fn sse_second_subscriber_returns_409() {
    let state = AppState::new();
    let session = Session::new("sid-s".into());
    let (job, _tx) = new_job("sse-test");
    // Drain the receiver to simulate a first SSE subscriber already connected.
    let _ = job.rx.lock().unwrap().take();
    let jid = job.id.clone();
    session.jobs.insert(jid.clone(), Arc::clone(&job));
    state.sessions.insert("sid-s".into(), session);

    let app = build_router(state);
    let resp = app
        .oneshot(
            Request::builder()
                .uri(format!("/sessions/sid-s/jobs/{jid}/stream"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);
}

// ── 8. Job semaphore full → 429 ───────────────────────────────────────────────

#[tokio::test]
async fn semaphore_full_returns_429() {
    let state = AppState::new();
    state.sessions.insert("sid-t".into(), Session::new("sid-t".into()));

    // Exhaust all 4 job semaphore slots before the router sees any request.
    let _p1 = Arc::clone(&state.job_semaphore).try_acquire_owned().unwrap();
    let _p2 = Arc::clone(&state.job_semaphore).try_acquire_owned().unwrap();
    let _p3 = Arc::clone(&state.job_semaphore).try_acquire_owned().unwrap();
    let _p4 = Arc::clone(&state.job_semaphore).try_acquire_owned().unwrap();

    let app = build_router(state);
    let resp = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/sessions/sid-t/stacking/stack")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"paths":["/nonexistent.fits"]}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
}

// ── TTL helper — unit test, no HTTP ──────────────────────────────────────────

#[test]
fn ttl_skips_session_with_running_job() {
    let session = Session::new("sid-ttl".into());
    assert!(!session.has_active_jobs(), "fresh session: no active jobs");

    let (job, _tx) = new_job("bg-work");
    session.jobs.insert(job.id.clone(), Arc::clone(&job));
    assert!(session.has_active_jobs(), "running job: session is active");

    job.set_done();
    assert!(!session.has_active_jobs(), "done job: session no longer active");
}
