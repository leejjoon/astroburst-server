//! Per-session activity log — the server-side half of the TUI dashboard
//! (issue #3, Phase 0).
//!
//! Every session-scoped request is recorded into a bounded ring buffer on the
//! session by [`record_activity`], an axum middleware layered over the whole
//! router. The ring is served by `GET /v2/sessions/:sid/history`, which polls
//! incrementally via a monotonic per-session `seq`.
//!
//! Observability endpoints themselves (`history`, session status, `images`,
//! `keepalive`, job polling) are *not* recorded, so a dashboard refreshing
//! every second doesn't flood the log it is displaying.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use axum::body::{to_bytes, Body};
use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::Response;
use serde::Serialize;

use super::state::AppState;

/// Ring-buffer capacity per session. Old events are dropped once exceeded;
/// `first_seq`/`last_seq` in the history response let a poller detect gaps.
pub const ACTIVITY_CAPACITY: usize = 200;

/// Only sniff request bodies up to this size for an `image_ref`/`target_ref`
/// field. Larger (or length-less) bodies pass through unbuffered.
const MAX_SNIFF_BODY_BYTES: u64 = 64 * 1024;

#[derive(Debug, Clone, Serialize)]
pub struct ActivityEvent {
    /// Monotonic per-session sequence number, starting at 1.
    pub seq: u64,
    /// Wall-clock time of the request, milliseconds since the unix epoch.
    pub unix_ms: u64,
    pub method: String,
    /// Route path relative to the session, e.g. `open`, `render`,
    /// `wcs/pix2sky`, `export/compressed` (v1 routes appear as `fits/open`,
    /// `image/render`, ...).
    pub endpoint: String,
    /// The image ref the request addressed: the `image_ref`/`target_ref`
    /// from the request body when given, else the session's active ref at
    /// completion time (which for `open`/`hdu` is the ref just created).
    pub image_ref: Option<String>,
    pub status: u16,
    pub duration_ms: u64,
}

#[derive(Default)]
struct ActivityInner {
    next_seq: u64,
    events: VecDeque<ActivityEvent>,
}

/// Bounded, seq-numbered event ring hung off every [`crate::session::Session`].
#[derive(Default)]
pub struct ActivityLog {
    inner: Mutex<ActivityInner>,
}

impl ActivityLog {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append an event, assigning it the next sequence number.
    pub fn record(&self, mut event: ActivityEvent) {
        let mut inner = self.inner.lock().unwrap();
        inner.next_seq += 1;
        event.seq = inner.next_seq;
        inner.events.push_back(event);
        if inner.events.len() > ACTIVITY_CAPACITY {
            inner.events.pop_front();
        }
    }

    /// Highest sequence number assigned so far (0 when nothing recorded).
    pub fn last_seq(&self) -> u64 {
        self.inner.lock().unwrap().next_seq
    }

    /// Events with `seq > since_seq`, oldest first, capped at `limit`
    /// (newest `limit` events win when more qualify). Also returns
    /// `(first_seq_in_buffer, last_seq)` so pollers can detect ring overflow.
    pub fn events_since(
        &self,
        since_seq: u64,
        limit: usize,
    ) -> (Vec<ActivityEvent>, u64, u64) {
        let inner = self.inner.lock().unwrap();
        let first_seq = inner.events.front().map(|e| e.seq).unwrap_or(0);
        let mut events: Vec<ActivityEvent> = inner
            .events
            .iter()
            .filter(|e| e.seq > since_seq)
            .cloned()
            .collect();
        if events.len() > limit {
            events.drain(..events.len() - limit);
        }
        (events, first_seq, inner.next_seq)
    }
}

/// Split a session-scoped path into `(sid, endpoint)` — handles both the v1
/// (`/sessions/:sid/...`) and v2 (`/v2/sessions/:sid/...`) surfaces.
fn split_session_path(path: &str) -> Option<(&str, &str)> {
    let rest = path
        .strip_prefix("/v2/sessions/")
        .or_else(|| path.strip_prefix("/sessions/"))?;
    match rest.split_once('/') {
        Some((sid, endpoint)) => Some((sid, endpoint.trim_end_matches('/'))),
        None => Some((rest, "")),
    }
}

/// True for endpoints excluded from the log: the observability/lifecycle
/// reads a dashboard polls in a loop (plus job polling and SSE streams,
/// whose lifetimes don't fit a request-duration event).
fn is_excluded(method: &str, endpoint: &str) -> bool {
    matches!(endpoint, "" | "history" | "images" | "keepalive")
        || (method == "GET" && endpoint.starts_with("jobs/"))
}

/// True for sessionless paths recorded into the process-global activity ring:
/// the `/v2/fs/*` data endpoints. Everything else non-session (`/health`, the
/// session list/create, the `GET /v2/activity` read itself) is deliberately
/// excluded so the ring isn't flooded by the dashboard's own polling.
fn global_endpoint(path: &str) -> bool {
    path.starts_with("/v2/fs/")
}

/// Shorten a query string for display in the activity feed (the `image_ref`
/// column is repurposed to carry it for global events). `None` for empty.
fn truncate_query(query: &str) -> Option<String> {
    const MAX: usize = 80;
    if query.is_empty() {
        return None;
    }
    let mut s: String = query.chars().take(MAX).collect();
    if query.chars().count() > MAX {
        s.push('…');
    }
    Some(s)
}

/// Extract `image_ref` (or `target_ref`) from a JSON request body.
fn ref_from_body(bytes: &[u8]) -> Option<String> {
    let json: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    ["image_ref", "target_ref"]
        .iter()
        .find_map(|k| json.get(k)?.as_str().map(str::to_owned))
}

/// Router-wide middleware recording session-scoped requests into the
/// session's [`ActivityLog`]. Non-session routes (`/health`, `/v2/sessions`
/// create/list) pass through untouched.
pub async fn record_activity(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Response {
    let path = req.uri().path().to_owned();
    let method = req.method().as_str().to_owned();

    let target = match split_session_path(&path) {
        Some((sid, endpoint)) if !is_excluded(&method, endpoint) => {
            Some((sid.to_owned(), endpoint.to_owned()))
        }
        _ => None,
    };
    let Some((sid, endpoint)) = target else {
        // Not session-scoped. Record the sessionless data endpoints (`/v2/fs/*`)
        // into the process-global ring; everything else passes through untouched.
        if global_endpoint(&path) {
            let query = req.uri().query().and_then(truncate_query);
            let started = Instant::now();
            let response = next.run(req).await;
            let duration_ms = started.elapsed().as_millis() as u64;
            state.global_activity.record(ActivityEvent {
                seq: 0, // assigned by record()
                unix_ms: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0),
                method,
                endpoint: path,
                image_ref: query,
                status: response.status().as_u16(),
                duration_ms,
            });
            return response;
        }
        return next.run(req).await;
    };

    // Sniff small JSON bodies for the ref they address. The body is consumed
    // and re-attached; requests without a (small) declared length skip this.
    let (req, body_ref) = match req
        .headers()
        .get(axum::http::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok()?.parse::<u64>().ok())
    {
        Some(len) if len > 0 && len <= MAX_SNIFF_BODY_BYTES => {
            let (parts, body) = req.into_parts();
            match to_bytes(body, MAX_SNIFF_BODY_BYTES as usize).await {
                Ok(bytes) => {
                    let r = ref_from_body(&bytes);
                    (Request::from_parts(parts, Body::from(bytes)), r)
                }
                Err(_) => (Request::from_parts(parts, Body::empty()), None),
            }
        }
        _ => (req, None),
    };

    let started = Instant::now();
    let response = next.run(req).await;
    let duration_ms = started.elapsed().as_millis() as u64;

    // The session may legitimately be gone (DELETE :sid, TTL race) — then
    // there is nowhere to record and nothing to observe.
    if let Some(session) = state.sessions.get(&sid).map(|e| e.value().clone()) {
        // Explicit body ref wins; otherwise attribute successful requests to
        // the active ref at completion (for open/hdu that's the new ref).
        let image_ref = match body_ref {
            Some(r) => Some(r),
            None if response.status().is_success() => {
                session.v2.active_ref.read().await.clone()
            }
            None => None,
        };
        session.activity.record(ActivityEvent {
            seq: 0, // assigned by record()
            unix_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
            method,
            endpoint,
            image_ref,
            status: response.status().as_u16(),
            duration_ms,
        });
    }

    response
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(endpoint: &str) -> ActivityEvent {
        ActivityEvent {
            seq: 0,
            unix_ms: 0,
            method: "POST".into(),
            endpoint: endpoint.into(),
            image_ref: None,
            status: 200,
            duration_ms: 1,
        }
    }

    #[test]
    fn ring_caps_at_capacity_and_keeps_seq_monotonic() {
        let log = ActivityLog::new();
        for i in 0..(ACTIVITY_CAPACITY + 50) {
            log.record(event(&format!("e{i}")));
        }
        let (events, first_seq, last_seq) = log.events_since(0, usize::MAX);
        assert_eq!(events.len(), ACTIVITY_CAPACITY);
        assert_eq!(last_seq, (ACTIVITY_CAPACITY + 50) as u64);
        assert_eq!(first_seq, 51);
        assert_eq!(events.first().unwrap().seq, 51);
        assert_eq!(events.last().unwrap().seq, last_seq);
    }

    #[test]
    fn events_since_filters_and_limits() {
        let log = ActivityLog::new();
        for i in 0..10 {
            log.record(event(&format!("e{i}")));
        }
        let (events, _, last) = log.events_since(7, usize::MAX);
        assert_eq!(last, 10);
        assert_eq!(
            events.iter().map(|e| e.seq).collect::<Vec<_>>(),
            vec![8, 9, 10]
        );
        // Newest `limit` events win.
        let (events, _, _) = log.events_since(0, 2);
        assert_eq!(
            events.iter().map(|e| e.seq).collect::<Vec<_>>(),
            vec![9, 10]
        );
    }

    #[test]
    fn path_split_and_exclusions() {
        assert_eq!(
            split_session_path("/v2/sessions/abc/wcs/pix2sky"),
            Some(("abc", "wcs/pix2sky"))
        );
        assert_eq!(
            split_session_path("/sessions/abc/fits/open"),
            Some(("abc", "fits/open"))
        );
        assert_eq!(split_session_path("/v2/sessions/abc"), Some(("abc", "")));
        assert_eq!(split_session_path("/health"), None);
        assert_eq!(split_session_path("/v2/sessions"), None);

        assert!(is_excluded("GET", ""));           // status / DELETE :sid
        assert!(is_excluded("GET", "history"));
        assert!(is_excluded("GET", "images"));
        assert!(is_excluded("POST", "keepalive"));
        assert!(is_excluded("GET", "jobs/j1"));
        assert!(is_excluded("GET", "jobs/j1/stream"));
        assert!(!is_excluded("DELETE", "jobs/j1")); // cancel is real work
        assert!(!is_excluded("POST", "open"));
        assert!(!is_excluded("GET", "structure"));  // image inspection is work
    }

    #[test]
    fn ref_extraction_prefers_image_ref() {
        assert_eq!(
            ref_from_body(br#"{"image_ref":"img_3","quantize_level":8}"#),
            Some("img_3".into())
        );
        assert_eq!(
            ref_from_body(br#"{"target_ref":"cut_1"}"#),
            Some("cut_1".into())
        );
        assert_eq!(ref_from_body(br#"{"path":"/x.fits"}"#), None);
        assert_eq!(ref_from_body(b"not json"), None);
    }

    #[test]
    fn global_endpoint_matches_only_fs() {
        assert!(global_endpoint("/v2/fs/raw"));
        assert!(global_endpoint("/v2/fs/list"));
        assert!(global_endpoint("/v2/fs/exists"));
        // Not recorded globally: infra + the dashboard's own polling targets.
        assert!(!global_endpoint("/health"));
        assert!(!global_endpoint("/v2/sessions"));
        assert!(!global_endpoint("/v2/activity"));
        assert!(!global_endpoint("/v2/sessions/abc/open"));
    }

    #[test]
    fn truncate_query_shortens_and_drops_empty() {
        assert_eq!(truncate_query(""), None);
        assert_eq!(
            truncate_query("path=/data/x.fits&compress=lossless"),
            Some("path=/data/x.fits&compress=lossless".to_string())
        );
        let long = "a".repeat(200);
        let got = truncate_query(&long).unwrap();
        assert_eq!(got.chars().count(), 81); // 80 chars + ellipsis
        assert!(got.ends_with('…'));
    }
}
