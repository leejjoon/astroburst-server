//! v2 render pipeline — the `/v2/.../render` endpoint's building blocks.
//!
//! The pointwise [`stretch`] vocabulary (issue #12) and the render [`handler`]
//! (issue #13) that composes it with scale-limit resolution (zscale/minmax/
//! percentile/manual), colormap LUTs, `max_dim` binning, overlays, and RGB8
//! PNG encoding to serve `POST /v2/sessions/:sid/render`.

pub mod handler;
pub mod stretch;
