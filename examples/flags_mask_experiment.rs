//! Dev-only experiment: how much does masking FLAGS bits before Rice
//! compression help, and which bits are actually costing us? Answers a
//! specific question about the real sample file without touching any
//! production code path. Run with:
//!
//!   cargo run --example flags_mask_experiment

use astroburst_lib::infra::fits::compress::rice::RiceParams;
use astroburst_lib::infra::fits::compress::rice_encode::rice_encode;
use astroburst_lib::infra::fits::reader::parse_header_at;

const SOURCE_PATH: &str = "sphx_fits/level2_2025W23_1C_0165_1D4_spx_l2b-v19-2025-251.fits";
const MP_SOURCE_BIT: i64 = 1 << 21;

fn read_flags(bytes: &[u8]) -> (Vec<i64>, usize, usize) {
    let primary = parse_header_at(bytes, 0).unwrap();
    let hdu1 = parse_header_at(bytes, primary.next_hdu_offset).unwrap();
    let hdu2 = parse_header_at(bytes, hdu1.next_hdu_offset).unwrap(); // FLAGS
    let header = &hdu2.header;
    let ncols = header.get_i64("NAXIS1").unwrap() as usize;
    let nrows = header.get_i64("NAXIS2").unwrap() as usize;
    let raw = &bytes[hdu2.data_start..hdu2.data_start + ncols * nrows * 4];
    let ints: Vec<i64> = raw
        .chunks_exact(4)
        .map(|c| i32::from_be_bytes([c[0], c[1], c[2], c[3]]) as i64)
        .collect();
    (ints, ncols, nrows)
}

fn compressed_size(pixels: &[i64], ncols: usize, nrows: usize) -> usize {
    let params = RiceParams { blocksize: 32, bytepix: 4, signed: true };
    (0..nrows)
        .map(|r| rice_encode(&pixels[r * ncols..(r + 1) * ncols], &params).len())
        .sum()
}

fn main() {
    let bytes = std::fs::read(SOURCE_PATH).expect("read source file");
    let (flags, ncols, nrows) = read_flags(&bytes);
    let total_pixels = ncols * nrows;
    let orig_bytes = total_pixels * 4;

    let report = |label: &str, masked: &Vec<i64>| {
        let nonzero = masked.iter().filter(|&&v| v != 0).count();
        let comp = compressed_size(masked, ncols, nrows);
        println!(
            "{label:<32} nonzero={:>8} ({:>6.2}%)  compressed={:>10} bytes  ratio={:.2}x",
            nonzero,
            100.0 * nonzero as f64 / total_pixels as f64,
            comp,
            orig_bytes as f64 / comp as f64
        );
    };

    println!("Original FLAGS array: {total_pixels} pixels, {orig_bytes} raw bytes\n");

    report("all bits (current)", &flags);

    let source_only: Vec<i64> = flags.iter().map(|&v| v & MP_SOURCE_BIT).collect();
    report("SOURCE bit only", &source_only);

    let without_source: Vec<i64> = flags.iter().map(|&v| v & !MP_SOURCE_BIT).collect();
    report("all bits EXCEPT source", &without_source);

    let zeroed: Vec<i64> = vec![0i64; total_pixels];
    report("all zero (upper bound)", &zeroed);

    // A few individual "noisy" bits in isolation, to see which one costs the most.
    for (name, bit) in [
        ("DICHROIC only (bit 7)", 7),
        ("TRANSIENT only (bit 0)", 0),
        ("SUR_ERROR only (bit 2)", 2),
        ("NONFUNC only (bit 6)", 6),
    ] {
        let mask = 1i64 << bit;
        let masked: Vec<i64> = flags.iter().map(|&v| v & mask).collect();
        report(name, &masked);
    }
}
