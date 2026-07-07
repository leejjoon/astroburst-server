//! v2 render pipeline — the `/v2/.../render` endpoint's building blocks.
//!
//! This slice contributes only the pointwise [`stretch`] vocabulary
//! (issue #12): the five per-pixel curves the render endpoint composes with a
//! normalized `[0,1]` input. The scale-limit resolution, colormap LUTs, PNG
//! encoding, and the HTTP handler itself land in later slices.

pub mod stretch;
