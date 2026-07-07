//! IRAF/`ZScaleInterval` auto-stretch limits.
//!
//! A faithful port of astropy's `astropy.visualization.ZScaleInterval`
//! (which itself follows IRAF's `zscale`): sample the image, fit a line to the
//! sorted samples with iterative k-sigma rejection, then derive display limits
//! from the fitted slope around the sample median. This is the only genuinely
//! new algorithm behind `POST /v2/sessions/:sid/render`; every other piece of
//! the render pipeline reuses existing primitives.
//!
//! Cross-checked against `ZScaleInterval(contrast=...).get_limits(data)` via
//! `uv run --with astropy --with numpy` on a deterministic synthetic fixture
//! (see the unit tests), matching to well within 4 significant figures.

use ndarray::Array2;

/// astropy's internal defaults; only `contrast` is exposed to the caller.
const NSAMPLES: usize = 1000;
const MAX_REJECT: f64 = 0.5;
const MIN_NPIXELS: usize = 5;
const KREJ: f64 = 2.5;
const MAX_ITERATIONS: usize = 5;

/// Default z-scale contrast, matching astropy's `ZScaleInterval` default.
pub const DEFAULT_CONTRAST: f64 = 0.25;

/// Compute `(vmin, vmax)` display limits for `data` using the z-scale
/// algorithm, mirroring `ZScaleInterval(contrast).get_limits(data)`.
///
/// Non-finite (NaN/inf) pixels are excluded from sampling, matching astropy's
/// masked-array behaviour. Returns `(NaN, NaN)` if no finite pixels exist, and
/// `(v, v)` for a flat/constant image — never panics.
pub fn zscale_limits(data: &Array2<f32>, contrast: f64) -> (f64, f64) {
    // _process_values: flatten (C-order) and drop non-finite pixels.
    let finite: Vec<f32> = match data.as_slice() {
        Some(s) => s.iter().copied().filter(|v| v.is_finite()).collect(),
        None => data.iter().copied().filter(|v| v.is_finite()).collect(),
    };
    zscale_limits_from_finite(finite, contrast)
}

/// Core of [`zscale_limits`] operating on an already-flattened, finite-only
/// sample vector in the original C-order. Split out so it can be unit-tested
/// directly against 1-D oracle data.
fn zscale_limits_from_finite(finite: Vec<f32>, contrast: f64) -> (f64, f64) {
    let size = finite.len();
    if size == 0 {
        return (f64::NAN, f64::NAN);
    }

    // Subsample: every `stride`-th finite pixel, capped at NSAMPLES.
    let stride = (size as f64 / NSAMPLES as f64).max(1.0) as usize;
    let mut samples: Vec<f64> = finite
        .iter()
        .step_by(stride)
        .take(NSAMPLES)
        .map(|&v| v as f64)
        .collect();
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());

    let npix = samples.len();
    let mut vmin = samples[0];
    let mut vmax = samples[npix - 1];

    // Iterative k-sigma line fit over the sorted samples.
    let minpix = MIN_NPIXELS.max((npix as f64 * MAX_REJECT) as usize);
    let ngrow = 1.max((npix as f64 * 0.01) as usize);

    // `badpix[i]` marks a rejected sample; rejections accumulate (never reset).
    let mut badpix = vec![false; npix];
    let mut ngoodpix = npix;
    let mut last_ngoodpix = npix + 1;
    let mut have_fit = false;
    let mut slope = 0.0f64;

    for _ in 0..MAX_ITERATIONS {
        if ngoodpix >= last_ngoodpix || ngoodpix < minpix {
            break;
        }

        // Ordinary least-squares line fit over the good (unrejected) samples.
        // astropy uses `polyfit(x, y, 1, w=(~badpix).astype(int))`; with 0/1
        // weights this is identical to OLS over the good subset.
        let (fit_slope, fit_intercept) = ols_line(&samples, &badpix);
        slope = fit_slope;
        have_fit = true;

        // flat = samples - fitted line.
        let flat: Vec<f64> = samples
            .iter()
            .enumerate()
            .map(|(i, &y)| y - (fit_slope * i as f64 + fit_intercept))
            .collect();

        // k-sigma threshold from the population std of the good residuals.
        let threshold = KREJ * std_pop(&flat, &badpix);

        // Reject samples further than the threshold from the fit.
        for i in 0..npix {
            if flat[i] < -threshold || flat[i] > threshold {
                badpix[i] = true;
            }
        }

        // Dilate the rejection mask by a length-`ngrow` box (np.convolve with a
        // ones kernel, mode="same", over the boolean mask).
        badpix = dilate(&badpix, ngrow);

        last_ngoodpix = ngoodpix;
        ngoodpix = badpix.iter().filter(|&&b| !b).count();
    }

    if have_fit && ngoodpix >= minpix {
        if contrast > 0.0 {
            slope /= contrast;
        }
        let center_pixel = (npix - 1) / 2;
        let median = median_sorted(&samples);
        vmin = vmin.max(median - (center_pixel as f64 - 1.0) * slope);
        vmax = vmax.min(median + (npix - center_pixel) as f64 * slope);
    }

    (vmin, vmax)
}

/// OLS slope/intercept of `samples` vs. their indices, over entries where
/// `badpix` is false. Assumes at least two good, distinct-index points.
fn ols_line(samples: &[f64], badpix: &[bool]) -> (f64, f64) {
    let mut n = 0.0;
    let (mut sx, mut sy, mut sxx, mut sxy) = (0.0, 0.0, 0.0, 0.0);
    for (i, &y) in samples.iter().enumerate() {
        if badpix[i] {
            continue;
        }
        let x = i as f64;
        n += 1.0;
        sx += x;
        sy += y;
        sxx += x * x;
        sxy += x * y;
    }
    let denom = n * sxx - sx * sx;
    if denom == 0.0 {
        // Fewer than two distinct good points: horizontal line at the mean.
        return (0.0, if n > 0.0 { sy / n } else { 0.0 });
    }
    let slope = (n * sxy - sx * sy) / denom;
    let intercept = (sy - slope * sx) / n;
    (slope, intercept)
}

/// Population standard deviation (ddof=0) of `values` over good entries,
/// matching numpy's `ndarray.std()` default.
fn std_pop(values: &[f64], badpix: &[bool]) -> f64 {
    let mut n = 0.0;
    let mut sum = 0.0;
    for (i, &v) in values.iter().enumerate() {
        if !badpix[i] {
            n += 1.0;
            sum += v;
        }
    }
    if n == 0.0 {
        return 0.0;
    }
    let mean = sum / n;
    let mut var = 0.0;
    for (i, &v) in values.iter().enumerate() {
        if !badpix[i] {
            let d = v - mean;
            var += d * d;
        }
    }
    (var / n).sqrt()
}

/// Dilate boolean mask `mask` by a length-`ngrow` ones kernel, reproducing
/// `np.convolve(mask, np.ones(ngrow), mode="same") != 0`: output position `i`
/// is true if any input in the window `[i + off - ngrow + 1, i + off]` is true,
/// where `off = (ngrow - 1) / 2` (numpy's "same"-mode centring).
fn dilate(mask: &[bool], ngrow: usize) -> Vec<bool> {
    if ngrow <= 1 {
        return mask.to_vec();
    }
    let n = mask.len();
    let off = (ngrow - 1) / 2;
    let mut out = vec![false; n];
    for i in 0..n {
        let hi = i + off;
        let lo = (i + off + 1).saturating_sub(ngrow);
        let lo = lo.min(n);
        let hi = hi.min(n.saturating_sub(1));
        if lo > hi {
            continue;
        }
        out[i] = mask[lo..=hi].iter().any(|&b| b);
    }
    out
}

/// Median of an already ascending-sorted slice (mean of the two central
/// elements for even length), matching `np.median`.
fn median_sorted(sorted: &[f64]) -> f64 {
    let n = sorted.len();
    if n == 0 {
        return f64::NAN;
    }
    if n % 2 == 1 {
        sorted[n / 2]
    } else {
        0.5 * (sorted[n / 2 - 1] + sorted[n / 2])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic pseudo-astronomical fixture: an LCG-driven Gaussian sky
    /// background plus a handful of bright single-pixel "stars". Mirrored bit
    /// for bit by the Python oracle (`state`/arithmetic are IEEE-754 double
    /// ops), so the hardcoded expectations below come straight from
    /// `ZScaleInterval(contrast).get_limits(...)`.
    fn synthetic_astro_image(ny: usize, nx: usize) -> Array2<f32> {
        let mut state: u64 = 987654321;
        let mut next_u = || -> f64 {
            state = (1103515245u64.wrapping_mul(state).wrapping_add(12345)) % 2147483648;
            state as f64 / 2147483648.0
        };
        let mut vals = Vec::with_capacity(ny * nx);
        for _ in 0..ny * nx {
            let mut g = 0.0f64;
            for _ in 0..12 {
                g += next_u();
            }
            g -= 6.0;
            vals.push((100.0 + 5.0 * g) as f32);
        }
        let mut arr = Array2::from_shape_vec((ny, nx), vals).unwrap();
        for &(y, x, amp) in &[
            (10usize, 12usize, 3000.0f32),
            (40, 50, 8000.0),
            (20, 55, 1500.0),
            (55, 10, 500.0),
            (33, 33, 20000.0),
        ] {
            arr[[y, x]] = arr[[y, x]] + amp;
        }
        arr
    }

    /// Relative-error check to 4 significant figures.
    fn assert_sig4(got: f64, want: f64) {
        let tol = want.abs() * 1e-4 + 1e-6;
        assert!(
            (got - want).abs() <= tol,
            "got {got}, want {want} (tol {tol})"
        );
    }

    #[test]
    fn matches_astropy_on_fixture() {
        let img = synthetic_astro_image(64, 64);
        // ZScaleInterval(contrast=0.25).get_limits(img)
        let (vmin, vmax) = zscale_limits(&img, 0.25);
        assert_sig4(vmin, 84.26371002);
        assert_sig4(vmax, 130.3523405);
    }

    #[test]
    fn contrast_widens_range() {
        let img = synthetic_astro_image(64, 64);
        // Lower contrast → steeper slope → wider vmax; vmin (clamped at the
        // sample low end here) is unchanged.
        let (vmin, vmax) = zscale_limits(&img, 0.1);
        assert_sig4(vmin, 84.26371002);
        assert_sig4(vmax, 175.253097);
    }

    #[test]
    fn excludes_nan_pixels() {
        let mut img = synthetic_astro_image(64, 64);
        for &(y, x) in &[(0usize, 0usize), (5, 5), (30, 30), (63, 63), (1, 2)] {
            img[[y, x]] = f32::NAN;
        }
        // ZScaleInterval(contrast=0.25).get_limits(img_with_nan)
        let (vmin, vmax) = zscale_limits(&img, 0.25);
        assert!(vmin.is_finite() && vmax.is_finite());
        assert_sig4(vmin, 83.07116699);
        assert_sig4(vmax, 130.9396412);
    }

    #[test]
    fn flat_image_does_not_panic() {
        let img = Array2::from_elem((20, 20), 42.0f32);
        let (vmin, vmax) = zscale_limits(&img, 0.25);
        assert_eq!(vmin, 42.0);
        assert_eq!(vmax, 42.0);
    }

    #[test]
    fn all_nan_returns_nan_without_panic() {
        let img = Array2::from_elem((8, 8), f32::NAN);
        let (vmin, vmax) = zscale_limits(&img, 0.25);
        assert!(vmin.is_nan() && vmax.is_nan());
    }
}
