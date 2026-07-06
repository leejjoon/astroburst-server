//! Shared region specification + resolver for the v2 surface.
//!
//! `RegionSpec` is the JSON-facing shape agents send to request a rectangular
//! region of an image, either in pixel coordinates or by sky center + angular
//! size. `resolve_region` turns that into a concrete pixel rectangle against a
//! specific image, enforcing bounds.
//!
//! The default behavior is *strict*: a region that does not fully fit the image
//! is rejected with `region_out_of_bounds` (the error hint names the image
//! extent). Passing `clip: true` opts into clamping to the image bounds instead.
//! Sky regions require a WCS on the image (else `wcs_required`).
//!
//! NOTE: the cutout slice deliberately does *not* use this strict resolver — it
//! has its own partial-overlap variant that tolerates regions hanging off the
//! edge and NaN-fills the missing area. This one is for endpoints that need the
//! region to genuinely lie inside the pixel grid.

use serde::{Deserialize, Serialize};

use astroburst_lib::core::astrometry::wcs::WcsTransform;

use crate::error::AppError;

/// Angular size for a sky region: either a single value (a square, in arcmin)
/// or an explicit `[width, height]` pair (arcmin).
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum SkySize {
    Square(f64),
    Rect([f64; 2]),
}

impl SkySize {
    /// Returns `(width_arcmin, height_arcmin)`.
    pub(crate) fn wh(&self) -> (f64, f64) {
        match self {
            SkySize::Square(s) => (*s, *s),
            SkySize::Rect([w, h]) => (*w, *h),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum RegionSpec {
    /// Pixel-space rectangle: `(x, y)` is the lower-left corner (0-indexed,
    /// x=column, y=row), with `width`/`height` in pixels.
    Pixel {
        x: i64,
        y: i64,
        width: usize,
        height: usize,
        clip: Option<bool>,
    },
    /// Sky rectangle centered on `(ra, dec)` (ICRS degrees) with an angular
    /// `size_arcmin`. Requires a WCS on the target image.
    Sky {
        ra: f64,
        dec: f64,
        size_arcmin: SkySize,
        clip: Option<bool>,
    },
}

/// A region resolved to concrete, in-bounds pixel coordinates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResolvedRegion {
    /// Lower-left corner column (0-indexed), guaranteed `>= 0` and `< img_w`.
    pub x: usize,
    /// Lower-left corner row (0-indexed), guaranteed `>= 0` and `< img_h`.
    pub y: usize,
    pub width: usize,
    pub height: usize,
    /// True when the requested region overran the image and `clip` clamped it.
    pub clipped: bool,
}

/// Resolve a `RegionSpec` against an image of size `img_w` × `img_h`.
///
/// `wcs` is required for `Sky` specs and ignored for `Pixel` specs.
pub fn resolve_region(
    spec: &RegionSpec,
    img_w: usize,
    img_h: usize,
    wcs: Option<&WcsTransform>,
) -> Result<ResolvedRegion, AppError> {
    let (x0, y0, w, h, clip) = match spec {
        RegionSpec::Pixel { x, y, width, height, clip } => {
            (*x, *y, *width, *height, clip.unwrap_or(false))
        }
        RegionSpec::Sky { ra, dec, size_arcmin, clip } => {
            let wcs = wcs.ok_or_else(|| AppError::BadRequestWithHint {
                code: "wcs_required",
                message: "sky region requires a WCS on the image, but none is present".into(),
                hint: Some("open an image whose header carries WCS keywords, or use a pixel region".into()),
            })?;
            let (cx, cy) = wcs.world_to_pixel(*ra, *dec);
            if !cx.is_finite() || !cy.is_finite() {
                return Err(AppError::BadRequestWithHint {
                    code: "region_out_of_bounds",
                    message: format!("sky position ({ra}, {dec}) does not project onto the image"),
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
            (x0, y0, wpx, hpx, clip.unwrap_or(false))
        }
    };

    if w == 0 || h == 0 {
        return Err(AppError::BadRequestWithHint {
            code: "region_out_of_bounds",
            message: "region width and height must both be > 0".into(),
            hint: Some(format!("image extent is 0..{img_w} x 0..{img_h} px")),
        });
    }

    let x1 = x0 + w as i64; // exclusive right edge
    let y1 = y0 + h as i64; // exclusive top edge

    let fully_inside = x0 >= 0 && y0 >= 0 && x1 <= img_w as i64 && y1 <= img_h as i64;

    if fully_inside {
        return Ok(ResolvedRegion {
            x: x0 as usize,
            y: y0 as usize,
            width: w,
            height: h,
            clipped: false,
        });
    }

    if !clip {
        return Err(AppError::BadRequestWithHint {
            code: "region_out_of_bounds",
            message: format!(
                "region [x={x0}, y={y0}, w={w}, h={h}] does not fit the image"
            ),
            hint: Some(format!("image extent is 0..{img_w} x 0..{img_h} px; pass clip=true to clamp")),
        });
    }

    // Clamp to the image bounds.
    let cx0 = x0.clamp(0, img_w as i64);
    let cy0 = y0.clamp(0, img_h as i64);
    let cx1 = x1.clamp(0, img_w as i64);
    let cy1 = y1.clamp(0, img_h as i64);
    let cw = (cx1 - cx0).max(0) as usize;
    let ch = (cy1 - cy0).max(0) as usize;

    if cw == 0 || ch == 0 {
        return Err(AppError::BadRequestWithHint {
            code: "region_out_of_bounds",
            message: "region does not overlap the image at all".into(),
            hint: Some(format!("image extent is 0..{img_w} x 0..{img_h} px")),
        });
    }

    Ok(ResolvedRegion {
        x: cx0 as usize,
        y: cy0 as usize,
        width: cw,
        height: ch,
        clipped: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code_of(e: &AppError) -> Option<&'static str> {
        match e {
            AppError::BadRequestWithHint { code, .. } => Some(code),
            _ => None,
        }
    }

    fn hint_of(e: &AppError) -> Option<String> {
        match e {
            AppError::BadRequestWithHint { hint, .. } => hint.clone(),
            _ => None,
        }
    }

    #[test]
    fn pixel_region_fully_inside_resolves_unchanged() {
        let spec = RegionSpec::Pixel { x: 10, y: 20, width: 30, height: 40, clip: None };
        let r = resolve_region(&spec, 100, 100, None).unwrap();
        assert_eq!(
            r,
            ResolvedRegion { x: 10, y: 20, width: 30, height: 40, clipped: false }
        );
    }

    #[test]
    fn oversized_pixel_region_errors_out_of_bounds_by_default() {
        // 80-wide region starting at x=50 runs off a 100px image.
        let spec = RegionSpec::Pixel { x: 50, y: 0, width: 80, height: 10, clip: None };
        let err = resolve_region(&spec, 100, 100, None).unwrap_err();
        assert_eq!(code_of(&err), Some("region_out_of_bounds"));
        // The hint must name the image extent so an agent can correct itself.
        let hint = hint_of(&err).expect("hint present");
        assert!(hint.contains("100"), "hint should name the extent: {hint}");
    }

    #[test]
    fn oversized_pixel_region_clamps_when_clip_true() {
        let spec = RegionSpec::Pixel { x: 50, y: 0, width: 80, height: 10, clip: Some(true) };
        let r = resolve_region(&spec, 100, 100, None).unwrap();
        assert_eq!(
            r,
            ResolvedRegion { x: 50, y: 0, width: 50, height: 10, clipped: true }
        );
    }

    #[test]
    fn negative_origin_clamps_when_clip_true() {
        let spec = RegionSpec::Pixel { x: -5, y: -10, width: 20, height: 20, clip: Some(true) };
        let r = resolve_region(&spec, 100, 100, None).unwrap();
        assert_eq!(
            r,
            ResolvedRegion { x: 0, y: 0, width: 15, height: 10, clipped: true }
        );
    }

    #[test]
    fn sky_region_without_wcs_errors_wcs_required() {
        let spec = RegionSpec::Sky {
            ra: 10.0,
            dec: 20.0,
            size_arcmin: SkySize::Square(1.0),
            clip: None,
        };
        let err = resolve_region(&spec, 100, 100, None).unwrap_err();
        assert_eq!(code_of(&err), Some("wcs_required"));
    }

    #[test]
    fn zero_size_region_is_rejected() {
        let spec = RegionSpec::Pixel { x: 0, y: 0, width: 0, height: 10, clip: Some(true) };
        let err = resolve_region(&spec, 100, 100, None).unwrap_err();
        assert_eq!(code_of(&err), Some("region_out_of_bounds"));
    }
}
