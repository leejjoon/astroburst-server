//! Pointwise stretch curves for the v2 render endpoint (issue #12).
//!
//! Each curve maps a normalized `[0,1]` input to a `[0,1]` output. Upstream,
//! the render endpoint produces that normalized value by clamping
//! `(pixel - vmin) / (vmax - vmin)` to `[0,1]` after resolving the scale
//! limits; this module owns the final pointwise reshaping.
//!
//! The formulas match the `astropy.visualization` stretch classes so a render
//! can be regression-tested against the astropy oracle:
//!
//! - `linear`  — `y = x` (identity)
//! - `log`     — `y = log(A·x + 1) / log(A + 1)`, `A = 1000` (astropy's default)
//! - `sqrt`    — `y = sqrt(x)`
//! - `asinh`   — `y = asinh(x/a) / asinh(1/a)`, softening `a` (default `0.1`)
//! - `power`   — `y = x^p`, exponent `p` (default `2.0`)
//!
//! `histeq` is intentionally absent: histogram equalization is a global,
//! distribution-dependent transform (it needs the whole-image CDF, not a
//! per-pixel formula) and does not fit this pointwise model. It is deferred to
//! a later round.

use crate::error::AppError;

/// The base of the `log` stretch, matching `astropy.visualization.LogStretch`'s
/// default `a = 1000`. The render request grammar exposes no `log`-specific
/// parameter, so this is fixed.
const LOG_A: f64 = 1000.0;

/// Default softening length for the `asinh` stretch (`astro-image-api.md`).
pub const DEFAULT_ASINH_A: f64 = 0.1;

/// Default exponent for the `power` stretch (`astro-image-api.md`).
pub const DEFAULT_POWER: f64 = 2.0;

/// The five pointwise stretch curves the render endpoint understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StretchKind {
    Linear,
    Log,
    Sqrt,
    Asinh,
    Power,
}

impl StretchKind {
    /// Parse a stretch name from the request (`"linear"`, `"log"`, `"sqrt"`,
    /// `"asinh"`, `"power"`). Case-insensitive.
    ///
    /// An unrecognized name — including `"histeq"`, which is deliberately out of
    /// scope for the pointwise pipeline — is a `bad_request` with a hint listing
    /// the supported vocabulary, never a panic.
    pub fn from_name(name: &str) -> Result<Self, AppError> {
        match name.trim().to_ascii_lowercase().as_str() {
            "linear" => Ok(StretchKind::Linear),
            "log" => Ok(StretchKind::Log),
            "sqrt" => Ok(StretchKind::Sqrt),
            "asinh" => Ok(StretchKind::Asinh),
            "power" => Ok(StretchKind::Power),
            "histeq" => Err(AppError::BadRequestWithHint {
                code: "bad_request",
                message: "stretch 'histeq' is not supported by the render endpoint".into(),
                hint: Some(
                    "histogram equalization is a global transform, not a pointwise curve; \
                     use one of: linear, log, sqrt, asinh, power"
                        .into(),
                ),
            }),
            other => Err(AppError::BadRequestWithHint {
                code: "bad_request",
                message: format!("unknown stretch '{other}'"),
                hint: Some("supported stretches: linear, log, sqrt, asinh, power".into()),
            }),
        }
    }
}

/// Apply a pointwise stretch to a single normalized value.
///
/// `normalized` is clamped to `[0,1]` up front, so an out-of-range input (e.g. a
/// pixel below `vmin`) collapses cleanly to the nearest endpoint rather than
/// producing a NaN/negative from `sqrt`/`log`. The result is clamped to `[0,1]`.
///
/// `asinh_a` is the `asinh` softening length and `power` the `power` exponent;
/// each is ignored by the other four kinds.
pub fn apply_stretch(normalized: f32, kind: StretchKind, asinh_a: f64, power: f64) -> f32 {
    let x = (normalized as f64).clamp(0.0, 1.0);

    let y = match kind {
        StretchKind::Linear => x,
        StretchKind::Log => (LOG_A * x + 1.0).ln() / (LOG_A + 1.0).ln(),
        StretchKind::Sqrt => x.sqrt(),
        StretchKind::Asinh => {
            // y = asinh(x/a) / asinh(1/a). Guard a <= 0 by falling back to the
            // documented default so a bad parameter can't produce inf/NaN.
            let a = if asinh_a > 0.0 { asinh_a } else { DEFAULT_ASINH_A };
            (x / a).asinh() / (1.0 / a).asinh()
        }
        StretchKind::Power => {
            // y = x^p. A non-positive exponent would blow up at x=0, so fall
            // back to the documented default in that case.
            let p = if power > 0.0 { power } else { DEFAULT_POWER };
            x.powf(p)
        }
    };

    (y as f32).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const EPS: f32 = 1e-5;

    #[test]
    fn from_name_accepts_the_five_kinds_case_insensitively() {
        assert_eq!(StretchKind::from_name("linear").unwrap(), StretchKind::Linear);
        assert_eq!(StretchKind::from_name("LOG").unwrap(), StretchKind::Log);
        assert_eq!(StretchKind::from_name(" Sqrt ").unwrap(), StretchKind::Sqrt);
        assert_eq!(StretchKind::from_name("asinh").unwrap(), StretchKind::Asinh);
        assert_eq!(StretchKind::from_name("Power").unwrap(), StretchKind::Power);
    }

    #[test]
    fn from_name_rejects_unknown_and_histeq_without_panicking() {
        for bad in ["histeq", "gamma", "", "zscale"] {
            let err = StretchKind::from_name(bad).unwrap_err();
            match err {
                AppError::BadRequestWithHint { code, .. } => assert_eq!(code, "bad_request"),
                other => panic!("expected bad_request for {bad:?}, got {other:?}"),
            }
        }
    }

    #[test]
    fn every_kind_maps_endpoints_to_zero_and_one() {
        for kind in [
            StretchKind::Linear,
            StretchKind::Log,
            StretchKind::Sqrt,
            StretchKind::Asinh,
            StretchKind::Power,
        ] {
            let lo = apply_stretch(0.0, kind, DEFAULT_ASINH_A, DEFAULT_POWER);
            let hi = apply_stretch(1.0, kind, DEFAULT_ASINH_A, DEFAULT_POWER);
            assert!(lo.abs() < EPS, "{kind:?} T(0) = {lo}");
            assert!((hi - 1.0).abs() < EPS, "{kind:?} T(1) = {hi}");
        }
    }

    #[test]
    fn linear_is_identity() {
        for &v in &[0.0f32, 0.25, 0.5, 0.75, 1.0] {
            let y = apply_stretch(v, StretchKind::Linear, DEFAULT_ASINH_A, DEFAULT_POWER);
            assert!((y - v).abs() < EPS, "linear({v}) = {y}");
        }
    }

    #[test]
    fn sample_values_match_astropy_reference() {
        // Reference values from astropy.visualization (a=0.1, power=2.0, LOG_A=1000)
        // at x in {0, 0.25, 0.5, 0.75, 1.0}. See the module doc for the formulas.
        let log = [0.0, 0.799776, 0.899816, 0.958408, 1.0];
        let sqrt = [0.0, 0.5, 0.707107, 0.866025, 1.0];
        let asinh = [0.0, 0.549402, 0.77127, 0.904691, 1.0];
        let power = [0.0, 0.0625, 0.25, 0.5625, 1.0];
        let xs = [0.0f32, 0.25, 0.5, 0.75, 1.0];

        for (i, &x) in xs.iter().enumerate() {
            let a = apply_stretch(x, StretchKind::Log, DEFAULT_ASINH_A, DEFAULT_POWER);
            assert!((a - log[i]).abs() < EPS, "log({x}) = {a}, want {}", log[i]);
            let a = apply_stretch(x, StretchKind::Sqrt, DEFAULT_ASINH_A, DEFAULT_POWER);
            assert!((a - sqrt[i]).abs() < EPS, "sqrt({x}) = {a}, want {}", sqrt[i]);
            let a = apply_stretch(x, StretchKind::Asinh, DEFAULT_ASINH_A, DEFAULT_POWER);
            assert!((a - asinh[i]).abs() < EPS, "asinh({x}) = {a}, want {}", asinh[i]);
            let a = apply_stretch(x, StretchKind::Power, DEFAULT_ASINH_A, DEFAULT_POWER);
            assert!((a - power[i]).abs() < EPS, "power({x}) = {a}, want {}", power[i]);
        }
    }

    #[test]
    fn asinh_incorporates_its_softening_parameter() {
        // Hand-computed: asinh(0.5/0.1) / asinh(1/0.1) = asinh(5)/asinh(10)
        //              = 2.312438 / 2.998223 = 0.771270.
        let y = apply_stretch(0.5, StretchKind::Asinh, 0.1, DEFAULT_POWER);
        assert!((y - 0.771270).abs() < EPS, "asinh(0.5, a=0.1) = {y}");

        // A larger softening flattens the curve: a bright-ish midtone is pushed
        // lower as `a` grows toward the linear limit.
        let small_a = apply_stretch(0.5, StretchKind::Asinh, 0.01, DEFAULT_POWER);
        let large_a = apply_stretch(0.5, StretchKind::Asinh, 1.0, DEFAULT_POWER);
        assert!(small_a > large_a, "a=0.01 -> {small_a}, a=1.0 -> {large_a}");
    }

    #[test]
    fn power_incorporates_its_exponent() {
        // p=2 -> 0.5^2 = 0.25; p=3 -> 0.5^3 = 0.125; p=0.5 -> sqrt(0.5).
        let sq = apply_stretch(0.5, StretchKind::Power, DEFAULT_ASINH_A, 2.0);
        assert!((sq - 0.25).abs() < EPS, "power(0.5, p=2) = {sq}");
        let cube = apply_stretch(0.5, StretchKind::Power, DEFAULT_ASINH_A, 3.0);
        assert!((cube - 0.125).abs() < EPS, "power(0.5, p=3) = {cube}");
        let root = apply_stretch(0.5, StretchKind::Power, DEFAULT_ASINH_A, 0.5);
        assert!((root - 0.5f32.sqrt()).abs() < EPS, "power(0.5, p=0.5) = {root}");
    }

    #[test]
    fn every_kind_is_monotonic_nondecreasing() {
        for kind in [
            StretchKind::Linear,
            StretchKind::Log,
            StretchKind::Sqrt,
            StretchKind::Asinh,
            StretchKind::Power,
        ] {
            let mut prev = apply_stretch(0.0, kind, DEFAULT_ASINH_A, DEFAULT_POWER);
            for i in 1..=1000 {
                let x = i as f32 / 1000.0;
                let y = apply_stretch(x, kind, DEFAULT_ASINH_A, DEFAULT_POWER);
                assert!(y >= prev - EPS, "{kind:?} not monotonic at x={x}: {y} < {prev}");
                prev = y;
            }
        }
    }

    #[test]
    fn out_of_range_input_is_clamped_not_nan() {
        for kind in [
            StretchKind::Linear,
            StretchKind::Log,
            StretchKind::Sqrt,
            StretchKind::Asinh,
            StretchKind::Power,
        ] {
            let below = apply_stretch(-0.5, kind, DEFAULT_ASINH_A, DEFAULT_POWER);
            let above = apply_stretch(1.7, kind, DEFAULT_ASINH_A, DEFAULT_POWER);
            assert!(below.abs() < EPS, "{kind:?} below-range -> {below}");
            assert!((above - 1.0).abs() < EPS, "{kind:?} above-range -> {above}");
        }
    }

    #[test]
    fn degenerate_parameters_fall_back_to_defaults() {
        // a <= 0 and p <= 0 must not produce inf/NaN.
        let a0 = apply_stretch(0.5, StretchKind::Asinh, 0.0, DEFAULT_POWER);
        assert!(a0.is_finite() && (0.0..=1.0).contains(&a0));
        let p0 = apply_stretch(0.0, StretchKind::Power, DEFAULT_ASINH_A, 0.0);
        assert!(p0.is_finite() && (0.0..=1.0).contains(&p0));
    }
}
