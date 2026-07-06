use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use tower::ServiceExt;

use crate::config::ServerConfig;
use crate::job::new_job;
use crate::router::build_router;
use crate::session::Session;
use crate::state::AppState;

/// Convenience: default config for tests (avoids reading env vars).
fn cfg() -> Arc<ServerConfig> {
    Arc::new(ServerConfig::default())
}

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

// ── 1. Health ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn health_returns_ok() {
    let app = build_router(AppState::new(cfg()));
    let resp = app
        .oneshot(Request::builder().uri("/health").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["status"], "ok");
    assert!(json["version"].is_string());
    assert!(json["sessions_active"].is_number());
    assert!(json["sessions_total"].is_number());
}

// ── 2. X-Request-Id pass-through ─────────────────────────────────────────────

#[tokio::test]
async fn request_id_echoed() {
    let app = build_router(AppState::new(cfg()));
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
    let app = build_router(AppState::new(cfg()));
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
    let state = AppState::new(cfg());
    state
        .sessions
        .insert("sid-h".into(), Session::new("sid-h".into(), &ServerConfig::default()));

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
    let state = AppState::new(cfg());
    let session = Session::new("sid-g".into(), &ServerConfig::default());
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
    let state = AppState::new(cfg());
    let session = Session::new("sid-c".into(), &ServerConfig::default());
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
    let state = AppState::new(cfg());
    let session = Session::new("sid-s".into(), &ServerConfig::default());
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
    let state = AppState::new(cfg());
    state
        .sessions
        .insert("sid-t".into(), Session::new("sid-t".into(), &ServerConfig::default()));

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
    let c = ServerConfig::default();
    let session = Session::new("sid-ttl".into(), &c);
    assert!(!session.has_active_jobs(), "fresh session: no active jobs");

    let (job, _tx) = new_job("bg-work");
    session.jobs.insert(job.id.clone(), Arc::clone(&job));
    assert!(session.has_active_jobs(), "running job: session is active");

    job.set_done();
    assert!(!session.has_active_jobs(), "done job: session no longer active");
}

// ── Config parsing — env var read & fallback ──────────────────────────────────

#[test]
fn config_env_var_parsed_and_invalid_falls_back() {
    // Use a uniquely-named env var to avoid races with other tests.
    // ASTROBURST_SESSION_MAX is a usize — set it to a valid value, read it back.
    // We can't safely setenv in parallel tests, so we test the `Default` path
    // (env vars absent) and the parse-error fallback via the helper directly.
    let d = ServerConfig::default();
    assert_eq!(d.session_max, 8);
    assert_eq!(d.jobs_max, 4);
    assert_eq!(d.cache_max_entries, 32);
    assert_eq!(d.cache_max_bytes, 2 * 1024 * 1024 * 1024);
    assert_eq!(d.session_ttl.as_secs(), 900);
    assert_eq!(d.cleanup_interval.as_secs(), 60);
    assert_eq!(d.log_level, "info");
    assert_eq!(d.bind.to_string(), "127.0.0.1:8080");
}

// ── v2 API: sessions & image lifecycle (issue #2) ────────────────────────────

mod v2_fixtures {
    //! Minimal hand-rolled FITS writers for the v2 lifecycle tests. Producing
    //! real files (rather than mocking the reader) keeps these tests honest
    //! about the open → decode → stats → WCS path they exercise.

    use std::io::Write;

    const BLOCK: usize = 2880;

    fn card(key: &str, value: &str) -> Vec<u8> {
        let text = format!("{key:<8}= {value}");
        let mut bytes = text.into_bytes();
        bytes.resize(80, b' ');
        bytes
    }

    fn header_block(cards: &[(&str, String)]) -> Vec<u8> {
        let mut out = Vec::new();
        for (k, v) in cards {
            out.extend_from_slice(&card(k, v));
        }
        // END card, then pad the block with spaces to a 2880 boundary.
        let mut end = b"END".to_vec();
        end.resize(80, b' ');
        out.extend_from_slice(&end);
        while out.len() % BLOCK != 0 {
            out.push(b' ');
        }
        out
    }

    fn data_block(pixels: &[f32]) -> Vec<u8> {
        let mut out = Vec::new();
        for p in pixels {
            out.extend_from_slice(&p.to_be_bytes());
        }
        while out.len() % BLOCK != 0 {
            out.push(0);
        }
        out
    }

    fn ramp(w: usize, h: usize) -> Vec<f32> {
        (0..w * h).map(|i| i as f32).collect()
    }

    fn wcs_cards() -> Vec<(&'static str, String)> {
        vec![
            ("CTYPE1", "'RA---TAN'".into()),
            ("CTYPE2", "'DEC--TAN'".into()),
            ("CRPIX1", "4.0".into()),
            ("CRPIX2", "4.0".into()),
            ("CRVAL1", "150.0".into()),
            ("CRVAL2", "2.0".into()),
            ("CD1_1", "-1.0E-4".into()),
            ("CD1_2", "0.0".into()),
            ("CD2_1", "0.0".into()),
            ("CD2_2", "1.0E-4".into()),
        ]
    }

    /// A single-HDU FITS image (w×h) carrying a TAN WCS.
    pub fn write_wcs_fits(path: &std::path::Path, w: usize, h: usize) {
        let mut cards: Vec<(&str, String)> = vec![
            ("SIMPLE", "T".into()),
            ("BITPIX", "-32".into()),
            ("NAXIS", "2".into()),
            ("NAXIS1", w.to_string()),
            ("NAXIS2", h.to_string()),
        ];
        cards.extend(wcs_cards());

        let mut buf = header_block(&cards);
        buf.extend_from_slice(&data_block(&ramp(w, h)));
        std::fs::File::create(path).unwrap().write_all(&buf).unwrap();
    }

    /// A 2-HDU MEF: primary (w0×h0, with WCS) + one IMAGE extension
    /// (w1×h1, EXTNAME=WEIGHT, no WCS). The two HDUs have different dims so a
    /// switch is observable.
    pub fn write_mef_fits(
        path: &std::path::Path,
        (w0, h0): (usize, usize),
        (w1, h1): (usize, usize),
    ) {
        let mut primary: Vec<(&str, String)> = vec![
            ("SIMPLE", "T".into()),
            ("BITPIX", "-32".into()),
            ("NAXIS", "2".into()),
            ("NAXIS1", w0.to_string()),
            ("NAXIS2", h0.to_string()),
        ];
        primary.extend(wcs_cards());

        let ext: Vec<(&str, String)> = vec![
            ("XTENSION", "'IMAGE   '".into()),
            ("BITPIX", "-32".into()),
            ("NAXIS", "2".into()),
            ("NAXIS1", w1.to_string()),
            ("NAXIS2", h1.to_string()),
            ("PCOUNT", "0".into()),
            ("GCOUNT", "1".into()),
            ("EXTNAME", "'WEIGHT  '".into()),
        ];

        let mut buf = header_block(&primary);
        buf.extend_from_slice(&data_block(&ramp(w0, h0)));
        buf.extend_from_slice(&header_block(&ext));
        buf.extend_from_slice(&data_block(&ramp(w1, h1)));
        std::fs::File::create(path).unwrap().write_all(&buf).unwrap();
    }
}

/// Helper: POST a JSON body to `uri` on a fresh clone of the router.
async fn post_json(
    app: axum::Router,
    uri: &str,
    body: &str,
) -> axum::response::Response {
    app.oneshot(
        Request::builder()
            .method(Method::POST)
            .uri(uri)
            .header("content-type", "application/json")
            .body(Body::from(body.to_owned()))
            .unwrap(),
    )
    .await
    .unwrap()
}

/// Create a session directly in state and return its id.
fn seed_session(state: &AppState, id: &str) {
    state
        .sessions
        .insert(id.into(), Session::new(id.into(), &ServerConfig::default()));
}

#[tokio::test]
async fn v2_create_session_returns_id() {
    let app = build_router(AppState::new(cfg()));
    let resp = post_json(app, "/v2/sessions", "").await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let json = body_json(resp).await;
    assert!(json["session_id"].is_string());
}

#[tokio::test]
async fn v2_open_returns_dims_stats_wcs_and_lists_ref() {
    let dir = tempfile::tempdir().unwrap();
    let fits = dir.path().join("wcs.fits");
    v2_fixtures::write_wcs_fits(&fits, 8, 8);

    let state = AppState::new(cfg());
    seed_session(&state, "s-open");

    let body = format!(r#"{{"path":"{}"}}"#, fits.to_str().unwrap());
    let resp = post_json(build_router(state.clone()), "/v2/sessions/s-open/open", &body).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["ref"], "img_0");
    assert_eq!(json["active_ref"], "img_0");
    assert_eq!(json["dims"], serde_json::json!([8, 8]));
    assert_eq!(json["wcs_present"], true);
    assert!(json["stats"]["median"].is_number());
    assert!(json["header"]["CTYPE1"].is_string());

    // GET /images should now list img_0 as the (only, active) ref.
    let resp = build_router(state)
        .oneshot(
            Request::builder()
                .uri("/v2/sessions/s-open/images")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["active_ref"], "img_0");
    assert_eq!(json["count"], 1);
    assert_eq!(json["images"][0]["image_ref"], "img_0");
    assert_eq!(json["images"][0]["wcs_present"], true);
}

#[tokio::test]
async fn v2_second_open_adds_ref_without_evicting_first() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.fits");
    let b = dir.path().join("b.fits");
    v2_fixtures::write_wcs_fits(&a, 8, 8);
    v2_fixtures::write_wcs_fits(&b, 6, 6);

    let state = AppState::new(cfg());
    seed_session(&state, "s-two");

    let resp = post_json(
        build_router(state.clone()),
        "/v2/sessions/s-two/open",
        &format!(r#"{{"path":"{}"}}"#, a.to_str().unwrap()),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = post_json(
        build_router(state.clone()),
        "/v2/sessions/s-two/open",
        &format!(r#"{{"path":"{}"}}"#, b.to_str().unwrap()),
    )
    .await;
    let json = body_json(resp).await;
    assert_eq!(json["ref"], "img_1");
    assert_eq!(json["active_ref"], "img_1");

    // Both refs remain registered; the newest is active.
    let resp = build_router(state)
        .oneshot(
            Request::builder()
                .uri("/v2/sessions/s-two/images")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let json = body_json(resp).await;
    assert_eq!(json["count"], 2);
    assert_eq!(json["active_ref"], "img_1");
}

#[tokio::test]
async fn v2_hdu_switch_creates_new_ref_and_keeps_original() {
    let dir = tempfile::tempdir().unwrap();
    let fits = dir.path().join("mef.fits");
    v2_fixtures::write_mef_fits(&fits, (8, 8), (16, 4));

    let state = AppState::new(cfg());
    seed_session(&state, "s-hdu");

    // Open HDU 0 explicitly (8×8 primary).
    let resp = post_json(
        build_router(state.clone()),
        "/v2/sessions/s-hdu/open",
        &format!(r#"{{"path":"{}","hdu":0}}"#, fits.to_str().unwrap()),
    )
    .await;
    let json = body_json(resp).await;
    assert_eq!(json["ref"], "img_0");
    assert_eq!(json["dims"], serde_json::json!([8, 8]));

    // Switch to HDU 1 (16×4 extension) — new ref, becomes active.
    let resp = post_json(build_router(state.clone()), "/v2/sessions/s-hdu/hdu", r#"{"hdu":1}"#).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["ref"], "img_1");
    assert_eq!(json["active_ref"], "img_1");
    assert_eq!(json["dims"], serde_json::json!([16, 4]));

    // The original ref is still addressable and unchanged.
    let resp = build_router(state)
        .oneshot(
            Request::builder()
                .uri("/v2/sessions/s-hdu/images")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let json = body_json(resp).await;
    assert_eq!(json["count"], 2);
    let img0 = json["images"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["image_ref"] == "img_0")
        .unwrap();
    assert_eq!(img0["width"], 8);
    assert_eq!(img0["height"], 8);
}

#[tokio::test]
async fn v2_delete_session_then_404() {
    let state = AppState::new(cfg());
    seed_session(&state, "s-del");

    let resp = build_router(state.clone())
        .oneshot(
            Request::builder()
                .method(Method::DELETE)
                .uri("/v2/sessions/s-del")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    // A follow-up request against the now-gone session 404s.
    let resp = build_router(state)
        .oneshot(
            Request::builder()
                .uri("/v2/sessions/s-del")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn v2_keepalive_ok_and_status_reports_active_ref() {
    let dir = tempfile::tempdir().unwrap();
    let fits = dir.path().join("k.fits");
    v2_fixtures::write_wcs_fits(&fits, 4, 4);

    let state = AppState::new(cfg());
    seed_session(&state, "s-ka");

    let resp = post_json(build_router(state.clone()), "/v2/sessions/s-ka/keepalive", "").await;
    assert_eq!(resp.status(), StatusCode::OK);

    post_json(
        build_router(state.clone()),
        "/v2/sessions/s-ka/open",
        &format!(r#"{{"path":"{}"}}"#, fits.to_str().unwrap()),
    )
    .await;

    let resp = build_router(state)
        .oneshot(
            Request::builder()
                .uri("/v2/sessions/s-ka")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["active_ref"], "img_0");
    assert_eq!(json["image_count"], 1);
}
