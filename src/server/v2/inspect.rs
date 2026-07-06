//! v2 inspection handlers: file structure, raw header cards, and a parsed WCS
//! summary. These are the read-only "what am I looking at?" endpoints an agent
//! calls first on an unfamiliar file (astro-image-api.md §3).
//!
//! - `structure` re-scans the active ref's *source file* with the existing
//!   [`list_extensions`] reader (the same call backing the desktop app's
//!   `get_fits_extensions`) and reports every HDU's index/extname/shape/BITPIX/
//!   dtype/has_data.
//! - `header` returns cards from the already-cached header — all of them, an
//!   explicit `keys=A,B` subset, or a glob (`keys=CD*_*`) — with a tiny
//!   shell-style matcher (no new parsing dependency).
//! - `wcs` returns a parsed WCS summary built from
//!   [`WcsTransform::raw_params`]/[`WcsTransform::sip_forward_terms`], with
//!   rotation/parity derived from the CD matrix here in the handler. Crucially,
//!   an image with no usable WCS is a normal state: it returns
//!   `200 {"present": false}`, never a 4xx/5xx.

use std::fs::File;

use axum::{extract::Query, Json};
use serde::Deserialize;
use serde_json::{json, Map, Value};

use astroburst_lib::core::astrometry::wcs::WcsTransform;
use astroburst_lib::infra::asdf::converter::is_asdf_file;
use astroburst_lib::infra::fits::dispatcher::resolve_single_image;
use astroburst_lib::infra::fits::reader::list_extensions;
use astroburst_lib::types::header::HduHeader;

use crate::error::{AppError, Result};
use crate::extractors::SessionExtractor;
use crate::session::Session;

// ── query params ─────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct RefQuery {
    /// Optional target ref; defaults to the session's active ref.
    #[serde(default, alias = "image")]
    pub r#ref: Option<String>,
}

#[derive(Deserialize)]
pub struct HeaderQuery {
    #[serde(default, alias = "image")]
    pub r#ref: Option<String>,
    /// Comma-separated card keys or globs (`EXPTIME,FILTER` or `CD*_*`). When
    /// omitted the full header is returned.
    pub keys: Option<String>,
}

// ── shared helpers ───────────────────────────────────────────────────────────

/// Resolve the ref an inspection targets: the explicit `ref` if given, else the
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

/// Fetch the cached header for `image_ref`, distinguishing "ref not registered"
/// (404) from "ref carries no header" (400).
fn cached_header(session: &Session, image_ref: &str) -> Result<HduHeader> {
    let entry = session
        .cache
        .get(image_ref)
        .ok_or_else(|| AppError::NotFound(format!("image ref {image_ref} not found in session")))?;
    entry
        .header()
        .cloned()
        .ok_or_else(|| AppError::BadRequest(format!("image {image_ref} carries no header")))
}

/// Human-readable dtype for a FITS BITPIX code.
fn dtype_for_bitpix(bitpix: i64) -> &'static str {
    match bitpix {
        8 => "uint8",
        16 => "int16",
        32 => "int32",
        64 => "int64",
        -32 => "float32",
        -64 => "float64",
        _ => "unknown",
    }
}

/// Row-major shape (`[ny, nx]` / `[nz, ny, nx]`) for an HDU's declared axes.
fn shape_for(naxis: i64, naxis1: i64, naxis2: i64, naxis3: i64) -> Vec<i64> {
    match naxis {
        0 => vec![],
        1 => vec![naxis1],
        2 => vec![naxis2, naxis1],
        _ => vec![naxis3, naxis2, naxis1],
    }
}

/// Case-insensitive shell-style glob supporting `*` (any run) and `?` (one
/// char). Used for `keys=CD*_*`; anchored to the whole key.
fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.to_ascii_uppercase().chars().collect();
    let t: Vec<char> = text.to_ascii_uppercase().chars().collect();
    // Classic two-pointer wildcard match with backtracking on `*`.
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star, mut mark): (Option<usize>, usize) = (None, 0);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

// ── handlers ───────────────────────────────────────────────────────────────

/// GET /v2/sessions/:sid/structure
///
/// List every HDU of the active ref's source file: index, extname, shape,
/// BITPIX, dtype, has_data. The first call an agent makes on an unfamiliar
/// file (survey products often hide the science array in extension 1).
pub async fn structure(
    SessionExtractor(session): SessionExtractor,
    Query(q): Query<RefQuery>,
) -> Result<Json<Value>> {
    let target = target_ref(&session, q.r#ref).await?;
    let source = session
        .v2
        .meta
        .get(&target)
        .and_then(|m| m.source.clone())
        .ok_or_else(|| {
            AppError::BadRequest(format!(
                "ref {target} has no source file to inspect (derived product?)"
            ))
        })?;

    // ASDF carries a single array, not addressable FITS HDUs.
    if is_asdf_file(std::path::Path::new(&source)) {
        return Err(AppError::BadRequest(
            "structure listing is FITS-only; this ref came from an ASDF file".into(),
        ));
    }

    let hdus = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<Value>> {
        let (fits_path, _tmp) = resolve_single_image(&source)?;
        let file = File::open(&fits_path)?;
        let exts = list_extensions(&file)?;
        Ok(exts
            .iter()
            .map(|h| {
                json!({
                    "index": h.index,
                    "extname": h.extname,
                    "extver": h.extver,
                    "naxis": h.naxis,
                    "shape": shape_for(h.naxis, h.naxis1, h.naxis2, h.naxis3),
                    "bitpix": h.bitpix,
                    "dtype": dtype_for_bitpix(h.bitpix),
                    "has_data": h.has_data,
                })
            })
            .collect())
    })
    .await
    .map_err(|e| AppError::Internal(anyhow::anyhow!("task panic: {e}")))?
    .map_err(AppError::Internal)?;

    Ok(Json(json!({
        "ref": target,
        "count": hdus.len(),
        "hdus": hdus,
    })))
}

/// GET /v2/sessions/:sid/header?keys=...
///
/// Return header cards from the cached header. With no `keys`, the full header;
/// with `keys=EXPTIME,FILTER`, just those; with a glob (`keys=CD*_*`), every
/// matching card. Missing explicit keys are simply absent from the result.
pub async fn header(
    SessionExtractor(session): SessionExtractor,
    Query(q): Query<HeaderQuery>,
) -> Result<Json<Value>> {
    let target = target_ref(&session, q.r#ref).await?;
    let hdr = cached_header(&session, &target)?;

    // Preserve the header's native card order; fall back to the index for
    // headers that only populated the map.
    let ordered: Vec<(String, String)> = if !hdr.cards.is_empty() {
        hdr.cards.clone()
    } else {
        hdr.index.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
    };

    let patterns: Option<Vec<String>> = q.keys.as_ref().map(|s| {
        s.split(',')
            .map(|k| k.trim().to_string())
            .filter(|k| !k.is_empty())
            .collect()
    });

    let mut out = Map::new();
    let mut seen = std::collections::HashSet::new();
    for (k, v) in &ordered {
        let keep = match &patterns {
            None => true,
            Some(ps) => ps.iter().any(|p| glob_match(p, k)),
        };
        if keep && seen.insert(k.clone()) {
            out.insert(k.clone(), json!({ "value": v }));
        }
    }

    Ok(Json(json!({
        "ref": target,
        "count": out.len(),
        "cards": Value::Object(out),
    })))
}

/// GET /v2/sessions/:sid/wcs
///
/// Parsed WCS summary: projection, CRPIX/CRVAL, CD matrix, pixel scale
/// (arcsec/px per axis), rotation (deg E of N), parity, and SIP presence.
///
/// **Contract:** no usable WCS is a normal state, not an error — this returns
/// `200 {"present": false}` so an agent can branch cleanly on whether sky
/// coordinates are available.
pub async fn wcs(
    SessionExtractor(session): SessionExtractor,
    Query(q): Query<RefQuery>,
) -> Result<Json<Value>> {
    let target = target_ref(&session, q.r#ref).await?;

    let entry = session
        .cache
        .get(&target)
        .ok_or_else(|| AppError::NotFound(format!("image ref {target} not found in session")))?;

    // No header, or a header the WCS engine can't build from, is "no WCS" —
    // reported as present:false, never an error.
    let wcs = match entry.header().and_then(|h| WcsTransform::from_header(h).ok()) {
        Some(w) => w,
        None => {
            return Ok(Json(json!({ "ref": target, "present": false })));
        }
    };

    let (crpix1, crpix2, crval1, crval2, cd, proj) = wcs.raw_params();
    let (cd11, cd12, cd21, cd22) = (cd[0][0], cd[0][1], cd[1][0], cd[1][1]);

    // Per-axis pixel scale = column norm of the CD matrix (deg/px → arcsec/px).
    let scale_x = (cd11 * cd11 + cd21 * cd21).sqrt() * 3600.0;
    let scale_y = (cd12 * cd12 + cd22 * cd22).sqrt() * 3600.0;

    // Rotation of North (+Dec, +y) from the pixel +y axis, deg E of N.
    let rotation_deg = (-cd12).atan2(cd22).to_degrees();

    // Handedness: det(CD) < 0 is the standard sky orientation (E to the left
    // when N is up); det > 0 means the image is flipped.
    let det = cd11 * cd22 - cd12 * cd21;
    let flipped = det > 0.0;

    let (sip_a, sip_b) = wcs.sip_forward_terms();
    let sip_present = sip_a.is_some() || sip_b.is_some();

    Ok(Json(json!({
        "ref": target,
        "present": true,
        "projection": proj,
        "crpix": [crpix1, crpix2],
        "crval": [crval1, crval2],
        "cd": [[cd11, cd12], [cd21, cd22]],
        "pixel_scale_arcsec": (scale_x + scale_y) / 2.0,
        "pixel_scale_x_arcsec": scale_x,
        "pixel_scale_y_arcsec": scale_y,
        "rotation_deg": rotation_deg,
        "flipped": flipped,
        "parity": if flipped { "flipped" } else { "normal" },
        "sip_present": sip_present,
    })))
}
