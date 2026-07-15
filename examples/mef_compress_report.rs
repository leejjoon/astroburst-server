//! Dev-only report: compress a real multi-extension FITS file via
//! `write_compressed_mef` and print per-extension size/ratio/error stats.
//! Run with:
//!
//!   cargo run --example mef_compress_report -- <path/to/file.fits> [quantize_level]
//!
//! Defaults to the sample MEF file used to design this feature. This does
//! NOT wire anything into the HTTP server -- it's the "review before we
//! bake it into an API" checkpoint.

use astroburst_lib::infra::fits::compress::{
    decode_compressed_image, decode_compressed_planes, is_compressed_image_hdu,
};
use astroburst_lib::infra::fits::mef_writer::write_compressed_mef;
use astroburst_lib::infra::fits::reader::{decode_pixels, parse_header_at};

fn read_raw_ints_be(data: &[u8], bitpix: i64) -> Vec<i64> {
    match bitpix {
        8 => data.iter().map(|&b| b as i64).collect(),
        16 => data
            .chunks_exact(2)
            .map(|c| i16::from_be_bytes([c[0], c[1]]) as i64)
            .collect(),
        32 => data
            .chunks_exact(4)
            .map(|c| i32::from_be_bytes([c[0], c[1], c[2], c[3]]) as i64)
            .collect(),
        other => panic!("unsupported integer BITPIX {other}"),
    }
}

fn is_image_extension(header: &astroburst_lib::types::HduHeader) -> bool {
    match header.get("XTENSION") {
        None => header.get_i64("NAXIS").unwrap_or(0) > 0,
        Some(x) => x.trim().eq_ignore_ascii_case("IMAGE"),
    }
}

struct Hdu {
    header: astroburst_lib::types::HduHeader,
    header_start: usize,
    data_start: usize,
    next_hdu_offset: usize,
}

fn scan(bytes: &[u8]) -> Vec<Hdu> {
    let mut hdus = Vec::new();
    let mut offset = 0usize;
    while offset + 2880 <= bytes.len() {
        let parsed = parse_header_at(bytes, offset).expect("parse HDU");
        let next = parsed.next_hdu_offset;
        hdus.push(Hdu {
            header: parsed.header,
            header_start: parsed.header_start,
            data_start: parsed.data_start,
            next_hdu_offset: next,
        });
        offset = next;
    }
    hdus
}

fn main() {
    let mut args = std::env::args().skip(1);
    let source_path = args
        .next()
        .unwrap_or_else(|| "sphx_fits/level2_2025W23_1C_0165_1D4_spx_l2b-v19-2025-251.fits".to_string());
    let quantize_level: f64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(16.0);

    let output_path = "/tmp/mef_compressed_report_output.fits";

    println!("Source: {source_path}");
    let t0 = std::time::Instant::now();
    write_compressed_mef(&source_path, output_path, quantize_level).expect("compress MEF");
    let elapsed = t0.elapsed();

    let source_bytes = std::fs::read(&source_path).expect("read source");
    let out_bytes = std::fs::read(output_path).expect("read output");

    let source_hdus = scan(&source_bytes);
    let out_hdus = scan(&out_bytes);
    assert_eq!(
        source_hdus.len(),
        out_hdus.len(),
        "HDU count mismatch: source={} output={}",
        source_hdus.len(),
        out_hdus.len()
    );

    println!(
        "\n{:<4} {:<10} {:>14} {:>14} {:>8}  {:<20} {}",
        "HDU", "EXTNAME", "orig bytes", "comp bytes", "ratio", "strategy", "max/mean abs err"
    );
    println!("{}", "-".repeat(100));

    let mut total_orig = 0usize;
    let mut total_comp = 0usize;

    for (i, (src, out)) in source_hdus.iter().zip(out_hdus.iter()).enumerate() {
        let orig_size = src.next_hdu_offset - src.header_start;
        let comp_size = out.next_hdu_offset - out.header_start;
        total_orig += orig_size;
        total_comp += comp_size;

        let extname = src.header.get("EXTNAME").map(|s| s.trim().to_string()).unwrap_or_default();
        let ratio = orig_size as f64 / comp_size.max(1) as f64;

        let (strategy, err_report) = if i == 0 {
            ("primary (dataless)".to_string(), String::new())
        } else if is_compressed_image_hdu(&src.header) {
            ("passthrough (pre-compressed)".to_string(), String::new())
        } else if !is_image_extension(&src.header) {
            ("passthrough (non-image)".to_string(), String::new())
        } else {
            let naxis = src.header.get_i64("NAXIS").unwrap_or(0);
            if !(2..=3).contains(&naxis) {
                ("passthrough (NAXIS out of scope)".to_string(), String::new())
            } else {
                let bitpix = src.header.get_i64("BITPIX").unwrap();
                let naxis1 = src.header.get_i64("NAXIS1").unwrap() as usize;
                let naxis2 = src.header.get_i64("NAXIS2").unwrap() as usize;
                let nplanes =
                    if naxis == 3 { src.header.get_i64("NAXIS3").unwrap() as usize } else { 1 };
                let plane_len = naxis1 * naxis2;
                let bytes_per_pixel = (bitpix.unsigned_abs() / 8) as usize;
                let raw = &source_bytes
                    [src.data_start..src.data_start + plane_len * nplanes * bytes_per_pixel];

                if bitpix < 0 {
                    let bzero = src.header.get_f64("BZERO").unwrap_or(0.0);
                    let bscale = src.header.get_f64("BSCALE").unwrap_or(1.0);
                    let original = decode_pixels(raw, bitpix, bscale, bzero);

                    let decoded: Vec<f32> = if nplanes > 1 {
                        decode_compressed_planes(&out_bytes, &out.header, out.data_start)
                            .expect("decode planes")
                            .iter()
                            .flat_map(|p| p.iter().copied())
                            .collect()
                    } else {
                        decode_compressed_image(&out_bytes, &out.header, out.data_start)
                            .expect("decode image")
                            .iter()
                            .copied()
                            .collect()
                    };

                    let mut max_err = 0.0f64;
                    let mut sum_err = 0.0f64;
                    let mut n = 0usize;
                    for (o, d) in original.iter().zip(decoded.iter()) {
                        if o.is_finite() && d.is_finite() {
                            let e = (*o as f64 - *d as f64).abs();
                            max_err = max_err.max(e);
                            sum_err += e;
                            n += 1;
                        }
                    }
                    (
                        format!("float{bitpix} -> quantized RICE"),
                        format!("{:.6} / {:.6}", max_err, sum_err / n.max(1) as f64),
                    )
                } else {
                    let original = read_raw_ints_be(raw, bitpix);
                    let decoded: Vec<f32> = decode_compressed_image(&out_bytes, &out.header, out.data_start)
                        .expect("decode image")
                        .iter()
                        .copied()
                        .collect();
                    let mut max_err = 0.0f64;
                    let mut n_mismatch = 0usize;
                    for (o, d) in original.iter().zip(decoded.iter()) {
                        let e = (*o as f64 - *d as f64).abs();
                        max_err = max_err.max(e);
                        if e > 0.5 {
                            n_mismatch += 1;
                        }
                    }
                    (
                        format!("int{bitpix} -> lossless RICE"),
                        format!("max={max_err:.3}, mismatches={n_mismatch}"),
                    )
                }
            }
        };

        println!(
            "{:<4} {:<10} {:>14} {:>14} {:>7.2}x  {:<30} {}",
            i, extname, orig_size, comp_size, ratio, strategy, err_report
        );
    }

    println!("{}", "-".repeat(100));
    println!(
        "TOTAL: {} bytes -> {} bytes ({:.2}x compression), elapsed {:.2}s",
        total_orig,
        total_comp,
        total_orig as f64 / total_comp as f64,
        elapsed.as_secs_f64()
    );

    let _ = std::fs::remove_file(output_path);
}
