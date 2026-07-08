# wcs-rs / mapproj bugs — fix upstream, then retire AstroBurst's workarounds

Filed against: [`mapproj`](https://github.com/cds-astro/cds-mapproj-rust) v0.4.0 (pinned transitively via [`wcs`](https://github.com/cds-astro/wcs-rs) v0.4.2, both crates.io). Not yet reported upstream — do that as part of fixing this.

**Context:** AstroBurst migrated its WCS engine to `wcs`-rs (see `docs/adr/0001-wcs-rs-for-wcs-engine.md`). Bugs 1–2 are SIP-specific and already worked around: `WcsTransform` strips any `-SIP` CTYPE suffix before constructing the engine, and applies/inverts SIP with AstroBurst's own math (`sip_forward`/`sip_inverse` in `src/core/astrometry/wcs.rs`). Once fixed upstream (and a new release is cut), that workaround can be deleted and AstroBurst can hand SIP off to wcs-rs directly. **Bug 3 is unrelated to SIP** — it affects the plain linear CD-matrix step every WCS uses, SIP or not — and currently has **no workaround**; see its section for why.

## Bug 1 (the important one): `SipCoeff::p` evaluates the wrong polynomial entirely

`mapproj-0.4.0/src/sip.rs:38-55`:

```rust
pub fn p(&self, u: f64, v: f64) -> f64 {
    let mut k = 0;
    let mut p = 0_f64;
    let mut x = u;
    for i in 0..self.order {
        let l = self.order - i;
        let mut y = v;
        for _ in 0..l {
            p += x * y * self.c[k];
            k += 1;
            y *= y; // <-- squares y instead of multiplying by v
        }
        x *= x; // <-- squares x instead of multiplying by u
    }
    p
}
```

The coefficients `self.c` are stored for the standard SIP triangular basis: index `k` corresponds to `(p, q)` where the outer loop `i` is `p` (0..=n) and the inner loop is `q` (0..=n-p). Evaluating the basis requires `x == u^i` and `y == v^j` at each step — i.e. **multiply by `u`/`v` to advance the exponent by one**. Instead the code **squares** `x`/`y`, and also starts them at `u^1`/`v^1` instead of `u^0`/`v^0`. For SIP order 2 (coefficients `0_0,0_1,0_2,1_0,1_1,2_0`) it computes `u·v, u·v², u·v⁴, u²·v, u²·v², u⁴·v` instead of `1, v, v², u, u·v, u²`.

This isn't a precision issue — it's evaluating a different function altogether. Caught by cross-checking AstroBurst's `pixel_to_world` against astropy: a TAN-SIP header agreed with astropy to <1e-8° everywhere *except* through this evaluator, where the corner pixel came back ~83° off (not "slightly imprecise" — flagrantly wrong), for SIP coefficients with typical distortion coefficient magnitudes (~1e-6) at large pixel offsets (~1000px from CRPIX). `mapproj`'s own `dpdu`/`dpdv` (used only by the dead `bivariate_newton`, see Bug 2) have the identical squaring bug and a probable coefficient-indexing bug too (`dpdu`'s outer loop starts at `i=1` but `k` isn't advanced past the `i=0` row first) — lower priority since currently unreachable, but check it while you're in there.

### Fix

```rust
pub fn p(&self, u: f64, v: f64) -> f64 {
    let mut k = 0;
    let mut p = 0_f64;
    let mut x = 1.0; // u^0
    for i in 0..self.order {
        let l = self.order - i;
        let mut y = 1.0; // v^0
        for _ in 0..l {
            p += x * y * self.c[k];
            k += 1;
            y *= v;
        }
        x *= u;
    }
    p
}
```

Verify against the SIP order-2 case above: should reproduce `c[0] + c[1]·v + c[2]·v² + c[3]·u + c[4]·u·v + c[5]·u²`.

## Bug 2: `Sip::inverse` never falls back to `bivariate_newton`

`mapproj-0.4.0/src/sip.rs:215-224`:

```rust
pub fn inverse(&self, fuv: f64, guv: f64) -> Option<ImgXY> {
    if self.has_polynomial_deproj() {
        let u = self.u(fuv, guv).unwrap();
        let v = self.v(fuv, guv).unwrap();
        Some(ImgXY::new(u, v))
    } else {
        // Make a grid and a 2-d tree to find the starting point (then multi-variate Newton)
        None
    }
}
```

For a header with forward-only SIP (`A_`/`B_` present, no `AP_`/`BP_` inverse polynomials — legal and common), this **always** returns `None`. The comment describes a grid-search-then-Newton fallback, but `bivariate_newton` (which implements exactly that) is never called from here or anywhere else in the crate — confirmed via `grep -rn bivariate_newton src/`, only its own definition matches. It's dead code. Net effect: `wcs::WCS::proj()` (sky→pixel) can never succeed for a forward-only-SIP header, full stop — not degraded, non-functional.

### Fix

Call `self.bivariate_newton(fuv, guv)` in the `else` branch instead of returning `None`. Fix Bug 1 first — `bivariate_newton` depends on `dfdu`/`dfdv`/`dgdu`/`dgdv`, which call `dpdu`/`dpdv`, which have the same squaring bug (and the indexing issue noted above).

## Bug 3: `WcsImgXY2ProjXY::inverse()` swaps the CD-matrix's off-diagonal terms

`mapproj-0.4.0/src/img2proj.rs:263-276`:

```rust
fn inverse(&self) -> Self::T {
  // Compute the determinant of the CD matrix
  let det = self.cd11 * self.cd22 - self.cd12 * self.cd21;
  // Compute the coefficient of the inverse matrix
  WcsProjXY2ImgXY {
    crpix1: self.crpix1,
    crpix2: self.crpix2,
    icd11:  self.cd22 / det,
    icd12: -self.cd21 / det,   // <-- should be -self.cd12 / det
    icd21: -self.cd12 / det,   // <-- should be -self.cd21 / det
    icd22:  self.cd11 / det,
  }
}
```

For a 2×2 matrix `M = [[cd11,cd12],[cd21,cd22]]`, the standard adjugate inverse is `(1/det)·[[cd22,-cd12],[-cd21,cd11]]`. This code computes `(1/det)·[[cd22,-cd21],[-cd12,cd11]]` instead — the **off-diagonal terms are transposed**. `icd11`/`icd22` (the diagonal) are correct; only `icd12`/`icd21` are swapped.

**Why every existing test missed this:** for a diagonal CD matrix (`cd12 = cd21 = 0` — true of essentially every synthetic test fixture, since most WCS test headers use `CDELT`+no-rotation or a small `CROTA2`), swapping two zeros is invisible: `icd12` and `icd21` both come out `0/det = 0` regardless of which of `cd12`/`cd21` is negated. The bug only manifests when the CD matrix has **significant, comparable-magnitude off-diagonal terms** — i.e. real rotation, which axis-aligned test fixtures never exercise.

**Impact:** any `world_to_pixel`/`WCS::proj()` call (sky → pixel) against a header with a rotated CD/PC matrix returns a pixel position off by a fraction of a pixel to several pixels, depending on rotation angle and distance from CRPIX — silently, with no error. `pixel_to_world`/`unproj()` (pixel → sky) is unaffected (it never calls `.inverse()`). Confirmed empirically on AstroBurst's `exampleFits/sample-data/656nmos.fits` fixture (a real WFPC2 frame, CD matrix rotated ~-47°, no SIP): `pixel_to_world` → `world_to_pixel` round-trips off by 0.27–0.38 px. Isolated with a standalone repro exercising *only* `WcsImgXY2ProjXY::img2proj`/`.inverse().proj2img()` — no TAN projection, no spherical rotation, no AstroBurst code at all — reproducing the identical error, and confirmed the fix (swapping `icd12`/`icd21` back) reduces it to ~1e-14 (machine precision). See `src/core/astrometry/wcs.rs::test_known_mapproj_bug_rotated_cd_matrix_roundtrip_error` for the regression test tracking this in AstroBurst.

**No current workaround.** Unlike SIP (an additive term AstroBurst can compute itself around the edges of wcs-rs's engine), the CD-matrix inverse is buried inside the single opaque `engine.proj()` call — there's no seam to intercept just this one internal step without either patching the dependency (e.g. a `[patch]` entry pointing at a forked/patched `mapproj`) or reimplementing the entire sky→pixel pipeline (spherical rotation + TAN deprojection + linear inverse) in AstroBurst itself. `pixel_to_world` (the far more heavily used direction — every render/cutout/stats endpoint) is unaffected; only `world_to_pixel`/`sky2pix`/pixel-type `separation` carry this error, and only for rotated WCS headers.

### Fix

```rust
icd12: -self.cd12 / det,
icd21: -self.cd21 / det,
```

### How to verify

Unit-test `WcsImgXY2ProjXY::inverse()` directly with a rotated CD matrix (off-diagonal terms of comparable magnitude to the diagonal — a diagonal-only test will not catch a regression of this exact bug): round-trip `img2proj()` then `.inverse().proj2img()` on several points and assert the result matches the input to float precision (~1e-9), not just "doesn't panic."

## How to verify a fix (Bugs 1–2, SIP)

1. Unit-test `SipCoeff::p` directly against the closed-form polynomial for a known coefficient set (the order-2 worked example above is a good starting fixture).
2. In AstroBurst: revert the `-SIP`-suffix-stripping workaround in `WcsTransform::from_header` (`src/core/astrometry/wcs.rs`), bump the `wcs` dependency, and re-run `cargo test --lib core::astrometry::wcs` — the astropy-oracle test (`test_astropy_oracle`) and the real-fixture test (`test_real_fixture_end_to_end`, `#[ignore]`d, needs `--ignored`) should still pass with wcs-rs doing the SIP math instead of AstroBurst's own.
3. Only then delete AstroBurst's own `sip_forward`/`sip_inverse`/`SipPoly` and delegate fully to wcs-rs.

## How to verify a fix (Bug 3, CD-matrix inverse)

1. Apply the one-line fix above, bump the `wcs`/`mapproj` dependency.
2. Re-run `cargo test --lib core::astrometry::wcs`. `test_known_mapproj_bug_rotated_cd_matrix_roundtrip_error` asserts the round-trip error is in `0.01..1.0` px — once fixed it'll drop to ~1e-9 and that assertion will start **failing** (by design, as a tripwire). At that point, tighten the assertion to `< 1e-6` and delete the "known bug" framing from the test name/comment.
