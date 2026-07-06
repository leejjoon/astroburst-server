//! v2 cutout handler: crop a pixel- or sky-specified region of an image into a
//! *new derived `image_ref`* in the same session's cache.
//!
//! Unlike the strict [`super::region::resolve_region`] resolver (used by the
//! stats/histogram slices), cutout tolerates **partial overlap**: a rectangle
//! that partially — or entirely — misses the parent frame is not an error.
//! Pixels outside the parent are NaN-filled and the response reports
//! `fraction_on_image`. This is a deliberately separate code path from the
//! shared strict resolver, not the same function with a flag.
//!
//! The cutout is materialized through the same `get_or_load_full` cache path the
//! `open` handler uses for on-disk loads, but with a purely-synchronous closure
//! (no disk I/O — it slices the already-decoded parent array). The parent's
//! header is cloned and its `CRPIX1`/`CRPIX2` shifted by the crop origin (via
//! [`HduHeader::set_f64`]) so the cutout's own WCS stays correct.
//!
//! Writing the cutout back to a FITS file on disk (the doc's `save` field) is
//! deliberately out of scope: no path-sandboxing precedent exists in the server
//! yet, so cutout only ever produces an in-session `image_ref`.

use axum::{extract::State, Json};
use serde::Deserialize;
use serde_json::{json, Value};

use astroburst_lib::core::astrometry::wcs::WcsTransform;
use astroburst_lib::core::imaging::stats::compute_image_stats;
use astroburst_lib::types::header::HduHeader;
use ndarray::Array2;

use super::images::register_and_respond;
use super::region::RegionSpec;
use crate::error::{AppError, Result};
use crate::extractors::SessionExtractor;
use crate::session::Session;
use crate::state::AppState;

// ── request body ─────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct CutoutParams {
    /// The region to crop, in pixel or sky coordinates (shared [`RegionSpec`]).
    pub region: RegionSpec,
    /// Optional target ref; defaults to the session's active ref.
    #[serde(default, alias = "ref")]
    pub image_ref: Option<String>,
    /// Optional explicit name for the derived ref. When omitted a `cutout_N`
    /// name is minted.
    pub name: Option<String>,
    /// Keep (and shift) the parent's WCS in the cutout header. Defaults to true.
    #[serde(default = "default_true")]
    pub preserve_wcs: bool,
}

fn default_true() -> bool {
    true
}

// ── partial-overlap region resolver (separate from the strict one) ────────────

/// A cutout rectangle in parent-pixel space. The origin may be negative and the
/// far edge may run past the parent frame — off-parent pixels become NaN.
struct CutoutRect {
    x0: i64,
    y0: i64,
    width: usize,
    height: usize,
}

/// Resolve a [`RegionSpec`] into a [`CutoutRect`] against a `img_w` × `img_h`
/// parent, *without* clamping to the frame (partial/no overlap is allowed).
/// `wcs` is required for `Sky` specs and ignored for `Pixel` specs.
fn resolve_cutout_rect(
    region: &RegionSpec,
    img_w: usize,
    img_h: usize,
    wcs: Option<&WcsTransform>,
) -> Result<CutoutRect> {
    let (x0, y0, width, height) = match region {
        RegionSpec::Pixel { x, y, width, height, .. } => (*x, *y, *width, *height),
        RegionSpec::Sky { ra, dec, size_arcmin, .. } => {
            let wcs = wcs.ok_or_else(|| AppError::BadRequestWithHint {
                code: "wcs_required",
                message: "sky region requires a WCS on the image, but none is present".into(),
                hint: Some("open an image whose header carries WCS keywords, or use a pixel region".into()),
            })?;
            let (cx, cy) = wcs.world_to_pixel(*ra, *dec);
            if !cx.is_finite() || !cy.is_finite() {
                return Err(AppError::BadRequestWithHint {
                    code: "region_out_of_bounds",
                    message: format!("sky position ({ra}, {dec}) does not project onto the image plane"),
                    hint: Some(format!("image extent is 0..{img_w} x 0..{img_h} px")),
                });
            }
            let scale = wcs.pixel_scale_arcsec(); // arcsec / px
            if !(scale.is_finite() && scale > 0.0) {
                return Err(AppError::BadRequestWithHint {
                    code: "wcs_required",
                    message: "image WCS has a degenerate pixel scale".into(),
                    hint: None,
                });
            }
            let (wa, ha) = size_arcmin.wh();
            // arcmin -> arcsec -> px
            let wpx = (wa * 60.0 / scale).round().max(1.0) as usize;
            let hpx = (ha * 60.0 / scale).round().max(1.0) as usize;
            // Center the box on the projected pixel.
            let x0 = (cx - wpx as f64 / 2.0).round() as i64;
            let y0 = (cy - hpx as f64 / 2.0).round() as i64;
            (x0, y0, wpx, hpx)
        }
    };

    if width == 0 || height == 0 {
        return Err(AppError::BadRequestWithHint {
            code: "region_out_of_bounds",
            message: "cutout width and height must both be > 0".into(),
            hint: Some(format!("image extent is 0..{img_w} x 0..{img_h} px")),
        });
    }

    Ok(CutoutRect { x0, y0, width, height })
}

/// The fraction of the requested rectangle that actually lies on the parent
/// frame — pure geometry, so it needs no per-pixel scan.
fn fraction_on_image(rect: &CutoutRect, img_w: usize, img_h: usize) -> f64 {
    let x1 = rect.x0 + rect.width as i64;
    let y1 = rect.y0 + rect.height as i64;
    let on_w = (x1.min(img_w as i64) - rect.x0.max(0)).max(0) as usize;
    let on_h = (y1.min(img_h as i64) - rect.y0.max(0)).max(0) as usize;
    (on_w * on_h) as f64 / (rect.width * rect.height) as f64
}

// ── handler ──────────────────────────────────────────────────────────────────

/// POST /v2/sessions/:sid/cutout
///
/// Crop a region of the active (or explicitly named) image into a new derived
/// `image_ref`, NaN-filling any area off the parent frame and shifting CRPIX so
/// the cutout's WCS stays correct.
pub async fn cutout(
    SessionExtractor(session): SessionExtractor,
    State(_state): State<AppState>,
    Json(params): Json<CutoutParams>,
) -> Result<Json<Value>> {
    let target = target_ref(&session, params.image_ref).await?;

    let entry = session
        .cache
        .get(&target)
        .ok_or_else(|| AppError::NotFound(format!("image ref {target} not found in session")))?;
    let (img_h, img_w) = entry.arr().dim();

    // A WCS is only needed to resolve a sky region (and to shift, below).
    let wcs = entry
        .header()
        .and_then(|h| WcsTransform::from_header(h).ok());
    let rect = resolve_cutout_rect(&params.region, img_w, img_h, wcs.as_ref())?;
    let fraction = fraction_on_image(&rect, img_w, img_h);

    // Build the cutout header: clone the parent's, shift CRPIX by the crop
    // origin, and reset NAXIS to the cutout dims. `preserve_wcs: false` (or a
    // headerless parent) yields an empty header instead.
    let header = if params.preserve_wcs {
        shifted_header(entry.header(), &rect)
    } else {
        HduHeader::empty()
    };

    let image_ref = params
        .name
        .clone()
        .unwrap_or_else(|| session.v2.next_ref("cutout"));

    // Slice the already-decoded parent array into the new ref via the same
    // get_or_load_full path open() uses — the loader does no disk I/O.
    let data = entry.data_arc();
    let CutoutRect { x0, y0, width, height } = rect;
    let sess = session.clone();
    let ref_for_load = image_ref.clone();
    let cutout_entry = tokio::task::spawn_blocking(move || {
        sess.cache.get_or_load_full(&ref_for_load, || {
            let mut out = Array2::<f32>::from_elem((height, width), f32::NAN);
            for oy in 0..height {
                let sy = y0 + oy as i64;
                if sy < 0 || sy >= img_h as i64 {
                    continue;
                }
                for ox in 0..width {
                    let sx = x0 + ox as i64;
                    if sx < 0 || sx >= img_w as i64 {
                        continue;
                    }
                    out[[oy, ox]] = data[[sy as usize, sx as usize]];
                }
            }
            let stats = compute_image_stats(&out);
            Ok((out, stats, header))
        })
    })
    .await
    .map_err(|e| AppError::Internal(anyhow::anyhow!("task panic: {e}")))?
    .map_err(AppError::Internal)?;

    // Derived product: no source file / hdu. Reuses the open/hdu response body.
    let mut body = register_and_respond(&session, image_ref.clone(), None, None, &cutout_entry);
    body["fraction_on_image"] = json!(fraction);
    body["region"] = json!({
        "x": x0, "y": y0, "width": width, "height": height,
    });
    *session.v2.active_ref.write().await = Some(image_ref);
    Ok(Json(body))
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

/// Clone `parent` and shift its reference pixel by the crop origin so the
/// cutout's WCS keeps pointing at the same sky. `CRPIX` is 1-based, so a crop
/// starting at 0-based pixel `x0` shifts `CRPIX1` by `-x0` (identically for y).
/// Returns an empty header when the parent has none.
fn shifted_header(parent: Option<&HduHeader>, rect: &CutoutRect) -> HduHeader {
    let mut hdr = match parent {
        Some(h) => h.clone(),
        None => return HduHeader::empty(),
    };
    if let Some(cr1) = hdr.get_f64("CRPIX1") {
        hdr.set_f64("CRPIX1", cr1 - rect.x0 as f64);
    }
    if let Some(cr2) = hdr.get_f64("CRPIX2") {
        hdr.set_f64("CRPIX2", cr2 - rect.y0 as f64);
    }
    hdr.set("NAXIS1", rect.width.to_string());
    hdr.set("NAXIS2", rect.height.to_string());
    hdr
}
