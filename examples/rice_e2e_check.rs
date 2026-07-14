//! End-to-end sanity check for RICE_1 export on a REAL sample FITS file
//! (not synthetic test data): load it via the normal reader path, write it
//! back both plain and RICE_1-compressed, compare file sizes, and confirm
//! the compressed output re-reads correctly through the server's own
//! decoder. Dev-only; run with:
//!
//!   cargo run --example rice_e2e_check -- <path/to/sample.fits>

use astroburst_lib::infra::fits::compress::decode_compressed_image;
use astroburst_lib::infra::fits::reader::{load_fits_image, parse_header_at};
use astroburst_lib::infra::fits::writer::{write_fits_mono_bitpix, write_fits_mono_rice};

fn main() {
    let input = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "exampleFits/sample-data/502nmos.fits".to_string());

    println!("Loading {input} ...");
    let image = load_fits_image(&input).expect("load source FITS");
    println!("Loaded image: {:?}", image.dim());

    let plain_path = "/tmp/rice_e2e_plain.fits";
    let rice_path = "/tmp/rice_e2e_rice.fits";

    write_fits_mono_bitpix(plain_path, &image, None, -32).expect("write plain FITS");
    write_fits_mono_rice(rice_path, &image, None, -32, 16.0).expect("write RICE_1 FITS");

    let rice16_path = "/tmp/rice_e2e_rice16.fits";
    write_fits_mono_rice(rice16_path, &image, None, 16, 16.0).expect("write RICE_1 int16 FITS");
    let rice16_size = std::fs::metadata(rice16_path).unwrap().len();
    println!("RICE_1 (int16) size: {rice16_size} bytes");

    let plain_size = std::fs::metadata(plain_path).unwrap().len();
    let rice_size = std::fs::metadata(rice_path).unwrap().len();
    let ratio = plain_size as f64 / rice_size as f64;
    println!("Plain size: {plain_size} bytes");
    println!("RICE_1 size: {rice_size} bytes");
    println!("Compression ratio: {ratio:.2}x");
    assert!(rice_size < plain_size, "compressed output should be smaller");

    // Re-read the compressed output through the server's own decoder.
    let bytes = std::fs::read(rice_path).unwrap();
    let primary = parse_header_at(&bytes, 0).expect("parse primary HDU");
    assert_eq!(primary.header.get("SIMPLE").map(|v| v.trim()), Some("T"));
    let ext = parse_header_at(&bytes, primary.next_hdu_offset).expect("parse BINTABLE ext");
    let decoded = decode_compressed_image(&bytes, &ext.header, ext.data_start)
        .expect("decode RICE_1 compressed image");
    assert_eq!(decoded.dim(), image.dim());

    let mut max_abs_err = 0.0f32;
    let mut n_finite = 0usize;
    for (orig, dec) in image.iter().zip(decoded.iter()) {
        if orig.is_finite() && dec.is_finite() {
            max_abs_err = max_abs_err.max((orig - dec).abs());
            n_finite += 1;
        } else {
            assert_eq!(orig.is_finite(), dec.is_finite(), "NaN-ness mismatch");
        }
    }
    println!("Compared {n_finite} finite pixels, max abs error: {max_abs_err}");

    println!("END-TO-END CHECK PASSED");
}
