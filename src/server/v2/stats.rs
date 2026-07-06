//! v2 region-scoped statistics handler.
//!
//! `POST /v2/sessions/:sid/stats` computes pixel statistics over a region (or
//! the full frame). The unclipped min/max/median/mad/sigma/mean/valid_count all
//! come straight from [`compute_image_stats`] on the region slice; on top of
//! that this handler layers two optional blocks:
//!
//! - **`sigma_clip`** → a `clipped` block. Delegates to the existing
//!   [`sigma_clipped_stats`] (iterative within-region reject-and-recompute using
//!   a robust MAD-based sigma). That helper only returns `(median, sigma)` and
//!   does *not* pre-filter NaN, so this handler filters non-finite values first
//!   and derives `mean` / `n_rejected` from the survivors it leaves the `Vec`
//!   `retain`-ed down to.
//! - **`percentiles`** → nearest-rank percentiles via [`percentile`] (no
//!   interpolation), one per requested value.
//!
//! Region resolution uses the strict [`resolve_region`] resolver: a region that
//! does not fit the image is a `region_out_of_bounds` 400, unless `clip: true`
//! clamps it to the image bounds.

use axum::Json;
use ndarray::{s, Array2};
use serde::Deserialize;
use serde_json::{json, Value};

use astroburst_lib::core::astrometry::wcs::WcsTransform;
use astroburst_lib::core::imaging::stats::{compute_image_stats, percentile};
use astroburst_lib::math::sigma_clipped_stats;

use crate::error::{AppError, Result};
use crate::extractors::SessionExtractor;
use crate::session::Session;

use super::region::{resolve_region, RegionSpec, ResolvedRegion};

// ── request body ─────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct StatsParams {
    /// Optional target ref; defaults to the session's active ref.
    #[serde(default, alias = "ref")]
    pub image_ref: Option<String>,
    /// Optional region; when omitted, stats run over the full frame.
    #[serde(default)]
    pub region: Option<RegionSpec>,
    /// Optional sigma-clip config; when `null`/omitted the `clipped` block is
    /// omitted entirely.
    #[serde(default)]
    pub sigma_clip: Option<SigmaClipParams>,
    /// Percentiles to compute, on a 0–100 scale. Empty → no `percentiles` block.
    #[serde(default)]
    pub percentiles: Vec<f64>,
}

#[derive(Deserialize)]
pub struct SigmaClipParams {
    #[serde(default = "default_sigma")]
    pub sigma: f32,
    #[serde(default = "default_maxiters")]
    pub maxiters: usize,
}

fn default_sigma() -> f32 {
    3.0
}
fn default_maxiters() -> usize {
    5
}

// ── helpers ──────────────────────────────────────────────────────────────────

/// Resolve the ref an operation targets: the explicit `ref` if given, else the
/// session's active ref (400 if neither is available).
async fn target_ref(session: &Session, explicit: Option<String>) -> Result<String> {
    match explicit {
        Some(r) => Ok(r),
        None => session
            .v2
            .active_ref
            .read()
            .await
            .clone()
            .ok_or_else(|| {
                AppError::BadRequest("no active image in this session; open a file first".into())
            }),
    }
}

// ── handler ──────────────────────────────────────────────────────────────────

/// POST /v2/sessions/:sid/stats
pub async fn stats(
    SessionExtractor(session): SessionExtractor,
    Json(params): Json<StatsParams>,
) -> Result<Json<Value>> {
    let target = target_ref(&session, params.image_ref).await?;
    let entry = session
        .cache
        .get(&target)
        .ok_or_else(|| AppError::NotFound(format!("image ref {target} not found in session")))?;
    let arr = entry.arr();
    let (rows, cols) = arr.dim();

    // Resolve the region (or the full frame) to concrete pixel coordinates and
    // materialize a contiguous owned slice to compute over.
    let (region_arr, resolved): (Array2<f32>, ResolvedRegion) = match &params.region {
        Some(spec) => {
            let wcs = entry.header().and_then(|h| WcsTransform::from_header(h).ok());
            let r = resolve_region(spec, cols, rows, wcs.as_ref())?;
            let sub = arr
                .slice(s![r.y..r.y + r.height, r.x..r.x + r.width])
                .to_owned();
            (sub, r)
        }
        None => (
            arr.to_owned(),
            ResolvedRegion { x: 0, y: 0, width: cols, height: rows, clipped: false },
        ),
    };

    let slice = region_arr
        .as_slice()
        .expect("region_arr is standard-layout after to_owned()");
    let n_nan = slice.iter().filter(|v| v.is_nan()).count() as u64;

    let base = compute_image_stats(&region_arr);

    let mut body = json!({
        "ref": target,
        "region": resolved,
        "min": base.min,
        "max": base.max,
        "median": base.median,
        "mad": base.mad,
        "sigma": base.sigma,
        "mean": base.mean,
        "valid_count": base.valid_count,
        "n_nan": n_nan,
    });

    // Optional sigma-clipped block.
    if let Some(sc) = &params.sigma_clip {
        // sigma_clipped_stats does not filter NaN — the caller must.
        let mut vals: Vec<f32> = slice.iter().copied().filter(|v| v.is_finite()).collect();
        let n_input = vals.len();
        let (median, std) = sigma_clipped_stats(&mut vals, sc.sigma, sc.maxiters);
        // `vals` is now retained down to the survivors; derive the rest.
        let n_survivors = vals.len();
        let n_rejected = n_input - n_survivors;
        let mean = if n_survivors > 0 {
            vals.iter().map(|&v| v as f64).sum::<f64>() / n_survivors as f64
        } else {
            0.0
        };
        body["clipped"] = json!({
            "mean": mean,
            "median": median,
            "std": std,
            "n_rejected": n_rejected,
        });
    }

    // Optional percentiles block (nearest-rank, no interpolation).
    if !params.percentiles.is_empty() {
        let mut finite: Vec<f32> = slice.iter().copied().filter(|v| v.is_finite()).collect();
        let results: Vec<Value> = params
            .percentiles
            .iter()
            .map(|&p| {
                let v = percentile(&mut finite, p / 100.0);
                json!({ "percentile": p, "value": v })
            })
            .collect();
        body["percentiles"] = json!(results);
    }

    Ok(Json(body))
}
