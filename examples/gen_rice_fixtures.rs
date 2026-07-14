//! Dev-only fixture generator for verifying our RICE_1 tile-compression
//! writer against astropy/cfitsio. Not part of the shipped binary or the
//! test suite -- run manually with:
//!
//!   cargo run --example gen_rice_fixtures -- <output_dir>
//!
//! then check the files with `uv run --with astropy <script>` (see
//! docs/agents or the writer.rs test module comments for the check itself).

use ndarray::Array2;

use astroburst_lib::infra::fits::writer::{write_fits_mono_rice, write_fits_rgb_rice};

fn smooth_image(nrows: usize, ncols: usize) -> Array2<f32> {
    Array2::from_shape_fn((nrows, ncols), |(y, x)| {
        let i = (y * ncols + x) as f32;
        100.0 + 10.0 * (i * 0.05).sin() + (y as f32) * 0.2
    })
}

fn main() {
    let dir = std::env::args().nth(1).unwrap_or_else(|| "/tmp/rice_fixtures".to_string());
    std::fs::create_dir_all(&dir).expect("create output dir");

    let mono = smooth_image(48, 64);
    let mut mono_with_nan = mono.clone();
    mono_with_nan[[5, 10]] = f32::NAN;
    mono_with_nan[[20, 30]] = f32::NAN;

    let mut mono_with_outlier = mono.clone();
    mono_with_outlier[[7, 40]] = 1.0e15;

    write_fits_mono_rice(&format!("{dir}/mono_i16.fits"), &mono, None, 16, 16.0)
        .expect("write mono i16");
    write_fits_mono_rice(&format!("{dir}/mono_f32.fits"), &mono, None, -32, 16.0)
        .expect("write mono f32");
    write_fits_mono_rice(&format!("{dir}/mono_f32_nan.fits"), &mono_with_nan, None, -32, 16.0)
        .expect("write mono f32 with NaN");
    write_fits_mono_rice(
        &format!("{dir}/mono_f32_gzip_fallback.fits"),
        &mono_with_outlier,
        None,
        -32,
        16.0,
    )
    .expect("write mono f32 with forced GZIP fallback tile");

    let r = mono.clone();
    let g = mono.mapv(|v| v * 0.5);
    let b = mono.mapv(|v| v + 5.0);
    write_fits_rgb_rice(&format!("{dir}/rgb_f32.fits"), &r, &g, &b, None, -32, 16.0)
        .expect("write rgb f32 cube");
    write_fits_rgb_rice(&format!("{dir}/rgb_i16.fits"), &r, &g, &b, None, 16, 16.0)
        .expect("write rgb i16 cube");

    println!("Wrote fixtures to {dir}:");
    for entry in std::fs::read_dir(&dir).unwrap() {
        let entry = entry.unwrap();
        println!("  {}", entry.path().display());
    }
}
