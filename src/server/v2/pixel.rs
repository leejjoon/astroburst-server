//! v2 pixel point-query handler.
//!
//! `POST /v2/sessions/:sid/pixel` is the cheap "is that white speck real or a
//! hot pixel?" sanity check from astro-image-api.md §5: given `{x, y, box}` it
//! returns the value at the pixel plus min/max/mean over the surrounding
//! `box`×`box` neighborhood, and — when the image carries a WCS — the sky
//! coordinate at that point. A single hot pixel has no elevated neighbors,
//! which distinguishes it from a real source.
//!
//! Purely a slice/read over the already-cached array (0-indexed `(x, y) =
//! (column, row)`, matching the doc's pixel convention) plus an optional
//! [`WcsTransform::pixel_to_world`]. No new core algorithms.

use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use astroburst_lib::core::astrometry::wcs::WcsTransform;

use crate::error::{AppError, Result};
use crate::extractors::SessionExtractor;
use crate::session::Session;

/// Default neighborhood side length when `box` is omitted (just the pixel).
fn default_box() -> u32 {
    1
}

#[derive(Deserialize)]
pub struct PixelParams {
    /// 0-based column.
    pub x: f64,
    /// 0-based row.
    pub y: f64,
    /// Side length of the neighborhood to summarize (centered on the pixel).
    /// Defaults to 1 (the pixel alone). Even sizes are honored as
    /// `[c - box/2, c + box/2]`.
    #[serde(default = "default_box", rename = "box")]
    pub box_size: u32,
    /// Optional target ref; defaults to the session's active ref.
    #[serde(default, alias = "ref")]
    pub image_ref: Option<String>,
}

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

/// POST /v2/sessions/:sid/pixel
pub async fn pixel(
    SessionExtractor(session): SessionExtractor,
    Json(params): Json<PixelParams>,
) -> Result<Json<Value>> {
    let target = target_ref(&session, params.image_ref).await?;

    let entry = session
        .cache
        .get(&target)
        .ok_or_else(|| AppError::NotFound(format!("image ref {target} not found in session")))?;
    let arr = entry.arr();
    let (rows, cols) = arr.dim();

    // Round the query point to the containing integer pixel index.
    let cx = params.x.floor() as i64;
    let cy = params.y.floor() as i64;

    if cx < 0 || cy < 0 || cx >= cols as i64 || cy >= rows as i64 {
        return Err(AppError::BadRequestWithHint {
            code: "pixel_out_of_bounds",
            message: format!(
                "pixel ({}, {}) is outside the image extent {}×{}",
                params.x, params.y, cols, rows
            ),
            hint: Some(format!(
                "x must be in [0, {}) and y in [0, {})",
                cols, rows
            )),
        });
    }

    let value = arr[[cy as usize, cx as usize]];

    // Summarize the box×box neighborhood, clipped to the image and skipping
    // NaNs (which are reported separately, per the doc's NaN policy).
    let half = (params.box_size / 2) as i64;
    let x0 = (cx - half).max(0);
    let x1 = (cx + half).min(cols as i64 - 1);
    let y0 = (cy - half).max(0);
    let y1 = (cy + half).min(rows as i64 - 1);

    let mut min = f64::INFINITY;
    let mut max = f64::NEG_INFINITY;
    let mut sum = 0.0f64;
    let mut n_finite: u64 = 0;
    let mut n_nan: u64 = 0;
    for yy in y0..=y1 {
        for xx in x0..=x1 {
            let v = arr[[yy as usize, xx as usize]] as f64;
            if v.is_finite() {
                min = min.min(v);
                max = max.max(v);
                sum += v;
                n_finite += 1;
            } else {
                n_nan += 1;
            }
        }
    }

    let (min_v, max_v, mean_v) = if n_finite > 0 {
        (json!(min), json!(max), json!(sum / n_finite as f64))
    } else {
        (Value::Null, Value::Null, Value::Null)
    };

    // Optional sky coordinate — omitted (null) when the image has no usable WCS.
    let sky = entry
        .header()
        .and_then(|h| WcsTransform::from_header(h).ok())
        .map(|wcs| {
            let c = wcs.pixel_to_world(params.x, params.y);
            json!({ "ra": c.ra, "dec": c.dec })
        })
        .unwrap_or(Value::Null);

    Ok(Json(json!({
        "ref": target,
        "x": params.x,
        "y": params.y,
        "value": if (value as f64).is_finite() { json!(value) } else { Value::Null },
        "box": params.box_size,
        "neighborhood": {
            "min": min_v,
            "max": max_v,
            "mean": mean_v,
            "n_pixels": n_finite,
            "n_nan": n_nan,
        },
        "sky": sky,
    })))
}
