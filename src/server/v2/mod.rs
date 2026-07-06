//! v2 API surface — mounted at `/v2/...` alongside the frozen v1 routes.
//!
//! Shares `Session` / `AppState` / `ImageCache` / `SessionExtractor` with v1
//! unchanged; the only session-state addition is the additive
//! [`crate::session::V2SessionState`]. This module owns the session + image
//! lifecycle handlers and the shared [`region`] resolver that later slices
//! (cutout, stats, render, ...) build on.

pub mod bin;
pub mod cutout;
pub mod images;
pub mod inspect;
pub mod pixel;
// `region` is shared plumbing; the stats slice (issue #8) is its first live
// consumer, and the cutout/render slices will follow. Some helpers remain
// exercised only by unit tests until then.
#[allow(dead_code)]
pub mod region;
pub mod sessions;
pub mod stats;
pub mod wcs;
