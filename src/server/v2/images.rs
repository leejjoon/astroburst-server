//! v2 image-lifecycle handlers: open a file into a session, switch HDU, list
//! registered image refs.
//!
//! Every image lives in the session's `ImageCache` under a bare ref string
//! (`img_0`, `img_1`, ...) and has an [`ImageMeta`] entry in `session.v2.meta`.
//! Opening a file, or switching HDU, always registers a *new* ref and makes it
//! active — it never mutates or evicts an existing ref.

use std::fs::File;

use axum::{extract::State, Json};
use serde::Deserialize;
use serde_json::{json, Value};

use astroburst_lib::core::astrometry::wcs::WcsTransform;
use astroburst_lib::core::imaging::stats::compute_image_stats;
use astroburst_lib::infra::asdf::converter::is_asdf_file;
use astroburst_lib::infra::asdf_bridge::extract_image_from_asdf;
use astroburst_lib::infra::cache::ImageEntry;
use astroburst_lib::infra::fits::dispatcher::resolve_single_image;
use astroburst_lib::infra::fits::file_bytes::resolved_io_for_file;
use astroburst_lib::infra::fits::reader::{extract_image_mmap, extract_image_mmap_by_index};
use astroburst_lib::types::header::HduHeader;
use astroburst_lib::types::ImageStats;
use ndarray::Array2;

use crate::error::{AppError, Result};
use crate::extractors::SessionExtractor;
use crate::session::{ImageMeta, Session};
use crate::state::AppState;

// ── request bodies ───────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct OpenParams {
    pub path: String,
    /// Optional HDU index to open. `None` auto-selects the first image HDU.
    pub hdu: Option<usize>,
    /// Optional explicit ref name. When omitted an `img_N` name is minted.
    pub name: Option<String>,
}

#[derive(Deserialize)]
pub struct HduParams {
    pub hdu: usize,
    /// Optional explicit ref name. When omitted an `img_N` name is minted.
    pub name: Option<String>,
}

// ── loading helpers ───────────────────────────────────────────────────────────

/// Auto-select the first image HDU (FITS) or the single array (ASDF).
fn load_auto(path: &str) -> anyhow::Result<(Array2<f32>, ImageStats, HduHeader)> {
    let p = std::path::Path::new(path);
    if is_asdf_file(p) {
        let r = extract_image_from_asdf(p)?;
        let stats = compute_image_stats(&r.image);
        return Ok((r.image, stats, r.header));
    }
    let (fits_path, _tmp) = resolve_single_image(path)?;
    let file = File::open(&fits_path)?;
    let r = extract_image_mmap(&file)?;
    let stats = compute_image_stats(&r.image);
    Ok((r.image, stats, r.header))
}

/// Load a specific HDU by index. FITS only — ASDF carries a single array.
fn load_by_index(path: &str, hdu: usize) -> anyhow::Result<(Array2<f32>, ImageStats, HduHeader)> {
    let p = std::path::Path::new(path);
    if is_asdf_file(p) {
        anyhow::bail!("ASDF files have no addressable HDU index; open without `hdu`");
    }
    let (fits_path, _tmp) = resolve_single_image(path)?;
    let file = File::open(&fits_path)?;
    let r = extract_image_mmap_by_index(&file, hdu)?;
    let stats = compute_image_stats(&r.image);
    Ok((r.image, stats, r.header))
}

/// The byte-source ("mmap"|"read") the FITS I/O policy resolves to for
/// `path`'s filesystem, for the response's `io` field. A policy indicator,
/// not a byte-level trace: ZIP-wrapped inputs are extracted to local tmp
/// before the FITS read, and ASDF doesn't use the FITS byte-source at all.
/// `null` if the path can't be re-opened (shouldn't happen right after a
/// successful load).
fn resolved_io_json(path: &str) -> Value {
    match File::open(path) {
        Ok(f) => json!(resolved_io_for_file(&f)),
        Err(_) => Value::Null,
    }
}

fn stats_json(s: &ImageStats) -> Value {
    json!({
        "min": s.min, "max": s.max, "median": s.median,
        "mad": s.mad, "sigma": s.sigma, "mean": s.mean,
        "valid_count": s.valid_count,
    })
}

/// Register a freshly-loaded `entry` under `image_ref`: record its metadata and
/// make it the session's active ref. Returns the JSON body shared by open/hdu.
pub(crate) fn register_and_respond(
    session: &Session,
    image_ref: String,
    source: Option<String>,
    hdu: Option<usize>,
    entry: &ImageEntry,
) -> Value {
    let (rows, cols) = entry.arr().dim();
    let stats = entry.stats();
    let header = entry.header();

    let wcs_present = header
        .map(|h| WcsTransform::from_header(h).is_ok())
        .unwrap_or(false);
    let extname = header.and_then(|h| h.get("EXTNAME").map(|s| s.to_string()));
    let header_map: Value = header
        .map(|h| serde_json::to_value(&h.index).unwrap_or(json!(null)))
        .unwrap_or(json!(null));

    let meta = ImageMeta {
        image_ref: image_ref.clone(),
        source,
        hdu,
        width: cols,
        height: rows,
        wcs_present,
        extname: extname.clone(),
    };
    session.v2.meta.insert(image_ref.clone(), meta);

    json!({
        "ref": image_ref,
        "active_ref": image_ref,
        "dims": [cols, rows],
        "hdu": hdu,
        "extname": extname,
        "wcs_present": wcs_present,
        "stats": stats_json(stats),
        "header": header_map,
    })
}

// ── handlers ───────────────────────────────────────────────────────────────

/// POST /v2/sessions/:sid/open
///
/// Load a FITS/ASDF file into a new image ref, which becomes the session's
/// active ref. Returns dims / stats / wcs_present / header.
pub async fn open(
    SessionExtractor(session): SessionExtractor,
    State(_state): State<AppState>,
    Json(params): Json<OpenParams>,
) -> Result<Json<Value>> {
    let image_ref = params
        .name
        .clone()
        .unwrap_or_else(|| session.v2.next_ref("img"));
    let path = params.path.clone();
    let hdu = params.hdu;

    let sess = session.clone();
    let ref_for_load = image_ref.clone();
    let entry = tokio::task::spawn_blocking(move || {
        sess.cache.get_or_load_full(&ref_for_load, || match hdu {
            Some(i) => load_by_index(&path, i),
            None => load_auto(&path),
        })
    })
    .await
    .map_err(|e| AppError::Internal(anyhow::anyhow!("task panic: {e}")))?
    .map_err(AppError::Internal)?;

    let mut body =
        register_and_respond(&session, image_ref.clone(), Some(params.path.clone()), hdu, &entry);
    body["io"] = resolved_io_json(&params.path);
    *session.v2.active_ref.write().await = Some(image_ref);
    Ok(Json(body))
}

/// POST /v2/sessions/:sid/hdu
///
/// Switch to a different HDU of the *active ref's source file* by loading it
/// into a brand-new ref (never mutating the existing one) and making it active.
pub async fn switch_hdu(
    SessionExtractor(session): SessionExtractor,
    State(_state): State<AppState>,
    Json(params): Json<HduParams>,
) -> Result<Json<Value>> {
    // Resolve the source file from the currently-active ref.
    let active = session.v2.active_ref.read().await.clone();
    let active = active.ok_or_else(|| {
        AppError::BadRequest("no active image in this session; open a file first".into())
    })?;
    let source = session
        .v2
        .meta
        .get(&active)
        .and_then(|m| m.source.clone())
        .ok_or_else(|| {
            AppError::BadRequest(format!("active ref {active} has no source file to re-open"))
        })?;

    let image_ref = params
        .name
        .clone()
        .unwrap_or_else(|| session.v2.next_ref("img"));
    let hdu = params.hdu;
    let path = source.clone();

    let sess = session.clone();
    let ref_for_load = image_ref.clone();
    let entry = tokio::task::spawn_blocking(move || {
        sess.cache
            .get_or_load_full(&ref_for_load, || load_by_index(&path, hdu))
    })
    .await
    .map_err(|e| AppError::Internal(anyhow::anyhow!("task panic: {e}")))?
    .map_err(AppError::Internal)?;

    let mut body =
        register_and_respond(&session, image_ref.clone(), Some(source.clone()), Some(hdu), &entry);
    body["io"] = resolved_io_json(&source);
    *session.v2.active_ref.write().await = Some(image_ref);
    Ok(Json(body))
}

/// GET /v2/sessions/:sid/images
///
/// List every image ref registered in the session, with the active ref called
/// out separately.
pub async fn list_images(
    SessionExtractor(session): SessionExtractor,
) -> Result<Json<Value>> {
    let active = session.v2.active_ref.read().await.clone();
    let mut images: Vec<ImageMeta> = session
        .v2
        .meta
        .iter()
        .map(|e| e.value().clone())
        .collect();
    // Stable ordering so the listing is deterministic for callers/tests.
    images.sort_by(|a, b| a.image_ref.cmp(&b.image_ref));

    Ok(Json(json!({
        "active_ref": active,
        "count": images.len(),
        "images": images,
    })))
}
