//! v2 block-average rebinning handler.
//!
//! `POST /v2/sessions/:sid/bin` block-averages the active (or named) image by
//! an integer `factor`, producing a new derived `image_ref` at
//! `out_dims = in_dims / factor`. It wraps [`area_downsample`] directly, which
//! is already NaN-aware (a block's NaN pixels are ignored rather than
//! poisoning the whole output pixel) and rayon-parallel.
//!
//! Only `method: "mean"` is supported. `method: "sum"` is rejected with a
//! clear `bad_request` rather than approximated as `mean * factor²`: that
//! shortcut is inexact on edge blocks whenever a dimension is not evenly
//! divisible by `factor`.

use std::sync::Arc;

use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use astroburst_lib::core::alignment::downsample::area_downsample;
use astroburst_lib::core::imaging::stats::compute_image_stats;

use crate::error::{AppError, Result};
use crate::extractors::SessionExtractor;
use crate::session::{ImageMeta, Session};

// ── request body ──────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct BinParams {
    /// Integer block size. `out_dims = in_dims / factor` (floor).
    pub factor: usize,
    /// Only `"mean"` is supported in this slice; defaults to `"mean"`.
    #[serde(default = "default_method")]
    pub method: String,
    /// Optional target ref; defaults to the session's active ref.
    #[serde(default, alias = "ref")]
    pub image_ref: Option<String>,
    /// Optional explicit name for the output ref. When omitted a `bin_N` name
    /// is minted.
    pub name: Option<String>,
}

fn default_method() -> String {
    "mean".to_string()
}

// ── shared helper ──────────────────────────────────────────────────────────────

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

// ── handler ────────────────────────────────────────────────────────────────────

/// POST /v2/sessions/:sid/bin
///
/// Block-average rebin the target image by `factor`, registering the result as
/// a new active ref.
pub async fn bin(
    SessionExtractor(session): SessionExtractor,
    Json(params): Json<BinParams>,
) -> Result<Json<Value>> {
    // `sum` is in the doc's spec but intentionally unsupported here (see module
    // docs). Reject anything that is not `mean` with a clear message.
    if params.method != "mean" {
        return Err(AppError::BadRequestWithHint {
            code: "bad_request",
            message: format!(
                "unsupported bin method {:?}; only \"mean\" is supported",
                params.method
            ),
            hint: Some(
                "sum-binning is inexact on edge blocks when dims aren't divisible by factor; \
                 use method \"mean\""
                    .into(),
            ),
        });
    }

    if params.factor == 0 {
        return Err(AppError::BadRequest(
            "factor must be a positive integer".into(),
        ));
    }

    let target = target_ref(&session, params.image_ref).await?;

    let entry = session.cache.get(&target).ok_or_else(|| {
        AppError::NotFound(format!("image ref {target} not found in session"))
    })?;
    let (in_rows, in_cols) = entry.arr().dim();

    let out_rows = in_rows / params.factor;
    let out_cols = in_cols / params.factor;
    if out_rows == 0 || out_cols == 0 {
        return Err(AppError::BadRequestWithHint {
            code: "bad_request",
            message: format!(
                "factor {} is larger than image dims {}x{}, producing an empty image",
                params.factor, in_cols, in_rows
            ),
            hint: Some("choose a factor no larger than the smaller image dimension".into()),
        });
    }

    let out_ref = params
        .name
        .clone()
        .unwrap_or_else(|| session.v2.next_ref("bin"));

    // area_downsample is rayon-parallel; run it off the async runtime.
    let src = entry.data_arc();
    let binned = tokio::task::spawn_blocking(move || area_downsample(&src, out_rows, out_cols))
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("task panic: {e}")))?;

    let stats = compute_image_stats(&binned);
    let arc = Arc::new(binned);
    session
        .cache
        .insert_synthetic(&out_ref, Arc::clone(&arc), stats.clone());

    // Carry the parent's source for provenance; binning drops WCS (dims change
    // without a corresponding CRPIX/CDELT rewrite), so wcs_present is false.
    let source = session.v2.meta.get(&target).and_then(|m| m.source.clone());
    session.v2.meta.insert(
        out_ref.clone(),
        ImageMeta {
            image_ref: out_ref.clone(),
            source,
            hdu: None,
            width: out_cols,
            height: out_rows,
            wcs_present: false,
            extname: None,
        },
    );
    *session.v2.active_ref.write().await = Some(out_ref.clone());

    Ok(Json(json!({
        "ref": out_ref,
        "active_ref": out_ref,
        "from_ref": target,
        "factor": params.factor,
        "method": "mean",
        "dims": [out_cols, out_rows],
        "wcs_present": false,
        "stats": {
            "min": stats.min, "max": stats.max, "median": stats.median,
            "mad": stats.mad, "sigma": stats.sigma, "mean": stats.mean,
            "valid_count": stats.valid_count,
        },
    })))
}
