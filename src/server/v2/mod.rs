//! v2 API surface — mounted at `/v2/...` alongside the frozen v1 routes.
//!
//! Shares `Session` / `AppState` / `ImageCache` / `SessionExtractor` with v1
//! unchanged; the only session-state addition is the additive
//! [`crate::session::V2SessionState`]. This module owns the session + image
//! lifecycle handlers and the shared [`region`] resolver that later slices
//! (cutout, stats, render, ...) build on.

pub mod images;
pub mod inspect;
// `region` is shared plumbing consumed by later slices (cutout, stats, ...);
// its public surface is exercised by unit tests but not yet by a live handler.
#[allow(dead_code)]
pub mod region;
pub mod sessions;
pub mod wcs;
