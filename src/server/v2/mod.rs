//! v2 API surface — mounted at `/v2/...` alongside the frozen v1 routes.
//!
//! Shares `Session` / `AppState` / `ImageCache` / `SessionExtractor` with v1
//! unchanged; the only session-state addition is the additive
//! [`crate::session::V2SessionState`]. This module owns the session + image
//! lifecycle handlers and the shared [`region`] resolver that later slices
//! (cutout, stats, render, ...) build on.

pub mod bin;
pub mod cutout;
pub mod histogram;
pub mod images;
pub mod inspect;
pub mod pixel;
// `render` hosts the pointwise stretch vocabulary (issue #12) and the live
// render handler (issue #13, `POST /v2/sessions/:sid/render`).
pub mod render;
// `region` is shared plumbing; the stats/histogram/render slices are its live
// consumers. Some helpers remain exercised only by unit tests until then.
#[allow(dead_code)]
pub mod region;
pub mod sessions;
pub mod stats;
pub mod wcs;
