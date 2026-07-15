//! v2 export handler — `POST /v2/sessions/:sid/export/compressed`.
//!
//! Compresses the *entire* multi-extension source FITS file behind an
//! `image_ref` (RICE_1: lossy quantized float extensions, lossless integer
//! extensions, verbatim passthrough for anything else -- see
//! `astroburst_lib::infra::fits::mef_writer`) and streams the result back as
//! a FITS download, so a client can receive a much smaller file and
//! reconstruct the original with only the accepted pixel-value loss.

use axum::{
    http::header,
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;

use astroburst_lib::infra::fits::mef_writer::write_compressed_mef;

use crate::error::{AppError, Result};
use crate::extractors::SessionExtractor;
use crate::session::Session;

/// astropy/fpack's own default (`ZSCALE = noise / quantize_level`); see the
/// compression report for how ratio/error trade off around this value.
const DEFAULT_QUANTIZE_LEVEL: f64 = 16.0;

#[derive(Deserialize, Default)]
pub struct ExportCompressedParams {
    /// Optional target ref; defaults to the session's active ref.
    #[serde(default, alias = "ref", alias = "image")]
    pub image_ref: Option<String>,
    /// Noise-relative quantization level applied to every float extension
    /// (smaller = more aggressive/lossy, smaller file). Defaults to 16.0.
    #[serde(default)]
    pub quantize_level: Option<f64>,
}

/// Resolve the ref an operation targets: the explicit `ref` if given, else the
/// session's active ref (400 if neither is available). Mirrors
/// `v2::render::handler::target_ref`.
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

/// Header values must not carry raw control/quote characters (CRLF
/// injection); the ref name is caller-suppliable, so sanitize before using
/// it in `Content-Disposition`.
fn sanitize_filename_component(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
        .collect()
}

/// POST /v2/sessions/:sid/export/compressed
pub async fn export_compressed(
    SessionExtractor(session): SessionExtractor,
    Json(params): Json<ExportCompressedParams>,
) -> Result<Response> {
    let target = target_ref(&session, params.image_ref).await?;
    let meta = session
        .v2
        .meta
        .get(&target)
        .ok_or_else(|| AppError::NotFound(format!("image ref {target} not found in session")))?;
    let source_path = meta.source.clone().ok_or_else(|| {
        AppError::BadRequest(format!(
            "ref {target} has no source file to compress (derived/cutout refs aren't supported)"
        ))
    })?;
    drop(meta);
    let quantize_level = params.quantize_level.unwrap_or(DEFAULT_QUANTIZE_LEVEL);

    let bytes = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<u8>> {
        let tmp = tempfile::Builder::new().suffix(".fits").tempfile()?;
        let tmp_path = tmp.path().to_string_lossy().to_string();
        write_compressed_mef(&source_path, &tmp_path, quantize_level)?;
        let bytes = std::fs::read(&tmp_path)?;
        // `tmp` (NamedTempFile) is removed on drop here.
        Ok(bytes)
    })
    .await
    .map_err(|e| AppError::Internal(anyhow::anyhow!("task panic: {e}")))?
    .map_err(AppError::Internal)?;

    let filename = format!("{}_compressed.fits", sanitize_filename_component(&target));
    Ok((
        [
            (header::CONTENT_TYPE, "application/fits".to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{filename}\""),
            ),
        ],
        bytes,
    )
        .into_response())
}
