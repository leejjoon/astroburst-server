//! Dev-only experiment: how much does masking the science IMAGE with the
//! FLAGS bitmap (excluding the near-universal SOURCE bit) help RICE
//! compression? Run with:
//!
//!   cargo run --example image_mask_experiment

use astroburst_lib::infra::fits::compress::quantize::{quantize_tile, QuantizeResult};
use astroburst_lib::infra::fits::compress::rice::RiceParams;
use astroburst_lib::infra::fits::compress::rice_encode::rice_encode;
use astroburst_lib::infra::fits::reader::{decode_pixels, parse_header_at};

const SOURCE_PATH: &str = "sphx_fits/level2_2025W23_1C_0165_1D4_spx_l2b-v19-2025-251.fits";
const MP_SOURCE_BIT: i32 = 1 << 21;
const QUANTIZE_LEVEL: f64 = 16.0;

fn main() {
    let bytes = std::fs::read(SOURCE_PATH).expect("read source file");

    let primary = parse_header_at(&bytes, 0).unwrap();
    let hdu1 = parse_header_at(&bytes, primary.next_hdu_offset).unwrap(); // IMAGE
    let hdu2 = parse_header_at(&bytes, hdu1.next_hdu_offset).unwrap(); // FLAGS

    let ncols = hdu1.header.get_i64("NAXIS1").unwrap() as usize;
    let nrows = hdu1.header.get_i64("NAXIS2").unwrap() as usize;
    let bzero = hdu1.header.get_f64("BZERO").unwrap_or(0.0);
    let bscale = hdu1.header.get_f64("BSCALE").unwrap_or(1.0);
    let img_raw = &bytes[hdu1.data_start..hdu1.data_start + ncols * nrows * 4];
    let image = decode_pixels(img_raw, -32, bscale, bzero);

    let flags_raw = &bytes[hdu2.data_start..hdu2.data_start + ncols * nrows * 4];
    let flags: Vec<i32> = flags_raw
        .chunks_exact(4)
        .map(|c| i32::from_be_bytes([c[0], c[1], c[2], c[3]]))
        .collect();

    let non_source_flagged: Vec<bool> = flags.iter().map(|&f| (f & !MP_SOURCE_BIT) != 0).collect();
    let n_flagged = non_source_flagged.iter().filter(|&&b| b).count();
    println!(
        "Pixels flagged (any bit except SOURCE): {n_flagged} / {} ({:.3}%)\n",
        ncols * nrows,
        100.0 * n_flagged as f64 / (ncols * nrows) as f64
    );

    let compressed_size_and_error = |pixels: &[f32]| -> (usize, f64, f64) {
        let mut total = 0usize;
        let mut max_err = 0.0f64;
        let mut sum_err = 0.0f64;
        let mut n = 0usize;
        for (r, row) in pixels.chunks(ncols).enumerate() {
            match quantize_tile(row, QUANTIZE_LEVEL, 1, r) {
                QuantizeResult::Ints { values, quant, .. } => {
                    let params = RiceParams { blocksize: 32, bytepix: 4, signed: true };
                    total += rice_encode(&values, &params).len();
                    for (&orig, &v) in row.iter().zip(values.iter()) {
                        if orig.is_finite() {
                            let recon = v as f64 * quant.scale + quant.zero;
                            let e = (orig as f64 - recon).abs();
                            max_err = max_err.max(e);
                            sum_err += e;
                            n += 1;
                        }
                    }
                }
                QuantizeResult::Overflow => {
                    // gzip fallback -- approximate with raw size for this experiment
                    total += row.len() * 4;
                }
            }
        }
        (total, max_err, sum_err / n.max(1) as f64)
    };

    let orig_bytes = ncols * nrows * 4;

    let (comp, max_err, mean_err) = compressed_size_and_error(&image);
    println!(
        "unmasked (current)              compressed={:>10} bytes  ratio={:.2}x  max_err={:.4} mean_err={:.4}",
        comp, orig_bytes as f64 / comp as f64, max_err, mean_err
    );

    let masked_zero: Vec<f32> = image
        .iter()
        .zip(non_source_flagged.iter())
        .map(|(&v, &flagged)| if flagged { 0.0 } else { v })
        .collect();
    let (comp, max_err, mean_err) = compressed_size_and_error(&masked_zero);
    println!(
        "masked to 0.0                    compressed={:>10} bytes  ratio={:.2}x  max_err={:.4} mean_err={:.4}",
        comp, orig_bytes as f64 / comp as f64, max_err, mean_err
    );

    let masked_nan: Vec<f32> = image
        .iter()
        .zip(non_source_flagged.iter())
        .map(|(&v, &flagged)| if flagged { f32::NAN } else { v })
        .collect();
    let (comp, max_err, mean_err) = compressed_size_and_error(&masked_nan);
    println!(
        "masked to NaN                    compressed={:>10} bytes  ratio={:.2}x  max_err={:.4} mean_err={:.4}",
        comp, orig_bytes as f64 / comp as f64, max_err, mean_err
    );
}
