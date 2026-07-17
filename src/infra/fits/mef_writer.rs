//! Multi-Extension FITS (MEF) compression.
//!
//! Reads every HDU of a source FITS file and writes a compressed
//! reproduction: float image extensions are lossy RICE_1 (quantized,
//! `ZQUANTIZ=SUBTRACTIVE_DITHER_1`); integer image extensions (e.g. a
//! bitmask/flag array) are **losslessly** RICE_1-compressed, reading the
//! raw stored integers directly rather than going through the app's normal
//! `Array2<f32>` pipeline (which would lose precision for large int32
//! values via f32's 24-bit mantissa); anything else -- an already-compressed
//! source extension, a genuine non-image extension (e.g. a small auxiliary
//! BinTable), or an image cube wider than 3D -- is copied through verbatim,
//! byte-for-byte.
//!
//! This intentionally does not try to be a fully generic "author any FITS
//! extension" API: it is the specific "compress this source file" pipeline
//! the MEF-serving feature needs, built from the same per-tile encoders the
//! single-image RICE writer (`writer.rs`) already uses.

use std::fs::File;
use std::io::{BufWriter, Write};

use anyhow::{bail, Context, Result};
use ndarray::Array2;

use crate::types::constants::BLOCK_SIZE;
use crate::types::HduHeader;

use super::compress::is_compressed_image_hdu;
use super::file_bytes::read_file_bytes;
use super::reader::{decode_pixels, parse_header_at};
use super::writer::{write_planes_lossless_int, write_planes_quantized, write_primary_hdu_stub};

/// One scanned HDU of the source file -- just enough to classify and
/// process it. (A local, minimal stand-in for `reader::ScannedHdu`, which
/// is private to that module; `parse_header_at` gives everything needed.)
struct SourceHdu {
    header: HduHeader,
    header_start: usize,
    data_start: usize,
    next_hdu_offset: usize,
}

fn scan_source_hdus(mmap: &[u8]) -> Result<Vec<SourceHdu>> {
    let mut hdus = Vec::new();
    let mut offset = 0usize;
    while offset + BLOCK_SIZE <= mmap.len() {
        let parsed = parse_header_at(mmap, offset)?;
        let next = parsed.next_hdu_offset;
        hdus.push(SourceHdu {
            header: parsed.header,
            header_start: parsed.header_start,
            data_start: parsed.data_start,
            next_hdu_offset: next,
        });
        offset = next;
    }
    if hdus.is_empty() {
        bail!("No HDUs found in source file");
    }
    Ok(hdus)
}

/// Decode raw big-endian integer pixel bytes directly, with NO BSCALE/BZERO
/// applied -- deliberately bypassing `reader::decode_pixels[_blank]`, which
/// converts to `f32` and would lose exactness for large int32 values.
fn read_raw_ints_be(data: &[u8], bitpix: i64) -> Result<Vec<i64>> {
    match bitpix {
        8 => Ok(data.iter().map(|&b| b as i64).collect()),
        16 => Ok(data
            .chunks_exact(2)
            .map(|c| i16::from_be_bytes([c[0], c[1]]) as i64)
            .collect()),
        32 => Ok(data
            .chunks_exact(4)
            .map(|c| i32::from_be_bytes([c[0], c[1], c[2], c[3]]) as i64)
            .collect()),
        other => bail!("read_raw_ints_be: unsupported integer BITPIX {other}"),
    }
}

/// A plain (non-compressed) image extension: `XTENSION='IMAGE'`, or the
/// primary HDU itself if it unusually carries data (`NAXIS>0`) -- the
/// common dataless-primary MEF case is handled separately by the caller,
/// which never passes HDU 0 through this check.
fn is_image_extension(header: &HduHeader) -> bool {
    match header.get("XTENSION") {
        None => header.get_i64("NAXIS").unwrap_or(0) > 0,
        Some(x) => x.trim().eq_ignore_ascii_case("IMAGE"),
    }
}

fn copy_verbatim(mmap: &[u8], writer: &mut BufWriter<File>, hdu: &SourceHdu) -> Result<()> {
    writer.write_all(&mmap[hdu.header_start..hdu.next_hdu_offset])?;
    Ok(())
}

/// Compress every HDU of `source_path` into `output_path`. See module docs
/// for the per-extension strategy. `quantize_level` is astropy/fpack's
/// `quantize_level` (e.g. 16.0), applied to every float image extension.
pub fn write_compressed_mef(source_path: &str, output_path: &str, quantize_level: f64) -> Result<()> {
    let file = File::open(source_path).with_context(|| format!("Failed to open {source_path}"))?;
    let mmap = read_file_bytes(&file)?;
    let hdus = scan_source_hdus(&mmap)?;

    let out_file = File::create(output_path).context("Failed to create output FITS file")?;
    let mut writer = BufWriter::with_capacity(4 * 1024 * 1024, out_file);

    // Primary HDU: dataless stub, passing through the source's own extra
    // cards (e.g. a pipeline VERSION card).
    write_primary_hdu_stub(&mut writer, Some(&hdus[0].header))?;

    for hdu in &hdus[1..] {
        let header = &hdu.header;

        if is_compressed_image_hdu(header) {
            // Already-compressed source extension: pass through verbatim.
            // Documented limitation -- not re-quantized/re-compressed.
            copy_verbatim(&mmap, &mut writer, hdu)?;
            continue;
        }

        if !is_image_extension(header) {
            // A genuine non-image extension (e.g. a small auxiliary
            // BinTable): copy through unchanged, no compression attempted.
            copy_verbatim(&mmap, &mut writer, hdu)?;
            continue;
        }

        let naxis = header.get_i64("NAXIS").unwrap_or(0);
        if naxis < 2 || naxis > 3 {
            // Out of scope for now (see plan risks) -- pass through.
            copy_verbatim(&mmap, &mut writer, hdu)?;
            continue;
        }

        let naxis1 = header.get_i64("NAXIS1").unwrap_or(0) as usize;
        let naxis2 = header.get_i64("NAXIS2").unwrap_or(0) as usize;
        let nplanes = if naxis == 3 {
            header.get_i64("NAXIS3").unwrap_or(1) as usize
        } else {
            1
        };
        let bitpix = header.get_i64("BITPIX").context("Missing BITPIX")?;
        let bytes_per_pixel = (bitpix.unsigned_abs() / 8) as usize;
        let plane_len = naxis1 * naxis2;
        let total_len = plane_len * nplanes;
        let data_end = hdu.data_start + total_len * bytes_per_pixel;
        if data_end > mmap.len() {
            bail!(
                "HDU data exceeds file size (EXTNAME={:?})",
                header.get("EXTNAME")
            );
        }
        let raw = &mmap[hdu.data_start..data_end];

        if bitpix < 0 {
            // Float image: lossy quantized RICE_1.
            let bzero = header.get_f64("BZERO").unwrap_or(0.0);
            let bscale = header.get_f64("BSCALE").unwrap_or(1.0);
            let pixels = decode_pixels(raw, bitpix, bscale, bzero);
            let mut planes = Vec::with_capacity(nplanes);
            for p in 0..nplanes {
                let chunk = pixels[p * plane_len..(p + 1) * plane_len].to_vec();
                planes.push(
                    Array2::from_shape_vec((naxis2, naxis1), chunk)
                        .context("Failed to reshape plane")?,
                );
            }
            write_planes_quantized(&mut writer, &planes, Some(header), quantize_level)?;
        } else {
            // Integer image: losslessly RICE_1-compressed, raw stored ints,
            // source's own (unchanged) BZERO/BSCALE/BLANK.
            let all_ints = read_raw_ints_be(raw, bitpix)?;
            let mut planes = Vec::with_capacity(nplanes);
            for p in 0..nplanes {
                planes.push(all_ints[p * plane_len..(p + 1) * plane_len].to_vec());
            }
            let bzero = header.get_f64("BZERO").unwrap_or(0.0);
            let bscale = header.get_f64("BSCALE").unwrap_or(1.0);
            let blank = header.get_i64("BLANK");
            write_planes_lossless_int(
                &mut writer, &planes, naxis1, naxis2, bitpix as i32, blank, bzero, bscale,
                Some(header),
            )?;
        }
    }

    writer.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infra::fits::compress::{decode_compressed_image, decode_compressed_planes};

    const BLOCK: usize = BLOCK_SIZE;

    fn card(key: &str, value: &str) -> Vec<u8> {
        let text = format!("{key:<8}= {value}");
        let mut bytes = text.into_bytes();
        bytes.resize(80, b' ');
        bytes
    }

    fn header_block(cards: &[(&str, String)]) -> Vec<u8> {
        let mut out = Vec::new();
        for (k, v) in cards {
            out.extend_from_slice(&card(k, v));
        }
        let mut end = b"END".to_vec();
        end.resize(80, b' ');
        out.extend_from_slice(&end);
        while out.len() % BLOCK != 0 {
            out.push(b' ');
        }
        out
    }

    fn f32_data_block(pixels: &[f32]) -> Vec<u8> {
        let mut out = Vec::with_capacity(pixels.len() * 4);
        for p in pixels {
            out.extend_from_slice(&p.to_be_bytes());
        }
        while out.len() % BLOCK != 0 {
            out.push(0);
        }
        out
    }

    fn i32_data_block(pixels: &[i32]) -> Vec<u8> {
        let mut out = Vec::with_capacity(pixels.len() * 4);
        for p in pixels {
            out.extend_from_slice(&p.to_be_bytes());
        }
        while out.len() % BLOCK != 0 {
            out.push(0);
        }
        out
    }

    /// Build a synthetic 5-HDU source FITS file mirroring the shapes of
    /// real-world extensions this feature targets: a dataless primary with
    /// an extra card, a float science image with WCS, an int32 flags array
    /// (lossless expectation), a 3-plane float cube, and a small fixed-width
    /// BinTable (passthrough expectation).
    fn write_synthetic_source(path: &std::path::Path) {
        let (w, h) = (24usize, 16usize);
        let mut buf = Vec::new();

        // HDU0: primary, dataless, with an extra VERSION card.
        buf.extend_from_slice(&header_block(&[
            ("SIMPLE", "T".into()),
            ("BITPIX", "8".into()),
            ("NAXIS", "0".into()),
            ("EXTEND", "T".into()),
            ("VERSION", "'6.4     '".into()),
        ]));

        // HDU1: float science image with WCS.
        let sci: Vec<f32> = (0..w * h).map(|i| 100.0 + (i as f32 * 0.07).sin() * 10.0).collect();
        buf.extend_from_slice(&header_block(&[
            ("XTENSION", "'IMAGE   '".into()),
            ("BITPIX", "-32".into()),
            ("NAXIS", "2".into()),
            ("NAXIS1", w.to_string()),
            ("NAXIS2", h.to_string()),
            ("PCOUNT", "0".into()),
            ("GCOUNT", "1".into()),
            ("EXTNAME", "'SCI     '".into()),
            ("CRPIX1", "12.5".into()),
            ("CRPIX2", "8.5".into()),
            ("CTYPE1", "'RA---TAN'".into()),
            ("CTYPE2", "'DEC--TAN'".into()),
        ]));
        buf.extend_from_slice(&f32_data_block(&sci));

        // HDU2: int32 flags array (sparse nonzero bits, realistic magnitude).
        let mut flags = vec![0i32; w * h];
        flags[5] = 1 << 3;
        flags[40] = (1 << 7) | (1 << 2);
        flags[200] = 1 << 15;
        buf.extend_from_slice(&header_block(&[
            ("XTENSION", "'IMAGE   '".into()),
            ("BITPIX", "32".into()),
            ("NAXIS", "2".into()),
            ("NAXIS1", w.to_string()),
            ("NAXIS2", h.to_string()),
            ("PCOUNT", "0".into()),
            ("GCOUNT", "1".into()),
            ("EXTNAME", "'FLAGS   '".into()),
        ]));
        buf.extend_from_slice(&i32_data_block(&flags));

        // HDU3: 3-plane float cube (not RGB -- a generic N-plane case).
        let (cw, ch, cn) = (8usize, 6usize, 3usize);
        let cube: Vec<f32> = (0..cw * ch * cn).map(|i| (i as f32) * 0.5 - 3.0).collect();
        buf.extend_from_slice(&header_block(&[
            ("XTENSION", "'IMAGE   '".into()),
            ("BITPIX", "-32".into()),
            ("NAXIS", "3".into()),
            ("NAXIS1", cw.to_string()),
            ("NAXIS2", ch.to_string()),
            ("NAXIS3", cn.to_string()),
            ("PCOUNT", "0".into()),
            ("GCOUNT", "1".into()),
            ("EXTNAME", "'CUBE    '".into()),
        ]));
        buf.extend_from_slice(&f32_data_block(&cube));

        // HDU4: small fixed-width BinTable (not an image) -- passthrough.
        buf.extend_from_slice(&header_block(&[
            ("XTENSION", "'BINTABLE'".into()),
            ("BITPIX", "8".into()),
            ("NAXIS", "2".into()),
            ("NAXIS1", "4".into()),
            ("NAXIS2", "1".into()),
            ("PCOUNT", "0".into()),
            ("GCOUNT", "1".into()),
            ("TFIELDS", "1".into()),
            ("EXTNAME", "'AUX     '".into()),
            ("TTYPE1", "'X       '".into()),
            ("TFORM1", "'1J      '".into()),
        ]));
        buf.extend_from_slice(&i32_data_block(&[42]));

        std::fs::write(path, &buf).unwrap();
    }

    #[test]
    fn write_compressed_mef_full_roundtrip() {
        let dir = std::env::temp_dir();
        let source_path = dir.join("ab_mef_source.fits");
        let output_path = dir.join("ab_mef_output.fits");
        write_synthetic_source(&source_path);

        write_compressed_mef(
            source_path.to_str().unwrap(),
            output_path.to_str().unwrap(),
            16.0,
        )
        .unwrap();

        let source_bytes = std::fs::read(&source_path).unwrap();
        let out_bytes = std::fs::read(&output_path).unwrap();

        // HDU0: primary, VERSION card preserved.
        let primary = parse_header_at(&out_bytes, 0).unwrap();
        assert_eq!(primary.header.get("SIMPLE").map(|v| v.trim()), Some("T"));
        assert_eq!(primary.header.get("VERSION").map(|v| v.trim()), Some("6.4"));

        // HDU1: SCI, quantized float, WCS preserved.
        let hdu1 = parse_header_at(&out_bytes, primary.next_hdu_offset).unwrap();
        assert_eq!(hdu1.header.get("ZCMPTYPE"), Some("RICE_1"));
        assert_eq!(hdu1.header.get("ZQUANTIZ"), Some("SUBTRACTIVE_DITHER_1"));
        assert_eq!(hdu1.header.get("EXTNAME").map(|v| v.trim()), Some("SCI"));
        assert_eq!(hdu1.header.get_f64("CRPIX1"), Some(12.5));
        let sci_decoded = decode_compressed_image(&out_bytes, &hdu1.header, hdu1.data_start).unwrap();
        let sci_original: Vec<f32> =
            (0..24 * 16).map(|i| 100.0 + (i as f32 * 0.07).sin() * 10.0).collect();
        for (o, d) in sci_original.iter().zip(sci_decoded.iter()) {
            assert!((o - d).abs() < 2.0, "SCI mismatch: {o} vs {d}");
        }

        // HDU2: FLAGS, lossless int32 (no ZQUANTIZ at all).
        let hdu2 = parse_header_at(&out_bytes, hdu1.next_hdu_offset).unwrap();
        assert_eq!(hdu2.header.get("ZCMPTYPE"), Some("RICE_1"));
        assert!(hdu2.header.get("ZQUANTIZ").is_none(), "FLAGS must not be quantized");
        assert_eq!(hdu2.header.get_i64("ZBITPIX"), Some(32));
        assert_eq!(hdu2.header.get("EXTNAME").map(|v| v.trim()), Some("FLAGS"));
        let flags_decoded = decode_compressed_image(&out_bytes, &hdu2.header, hdu2.data_start).unwrap();
        let mut flags_original = vec![0.0f32; 24 * 16];
        flags_original[5] = (1 << 3) as f32;
        flags_original[40] = ((1 << 7) | (1 << 2)) as f32;
        flags_original[200] = (1 << 15) as f32;
        for (o, d) in flags_original.iter().zip(flags_decoded.iter()) {
            assert_eq!(*o, *d, "FLAGS must round-trip exactly (lossless int path)");
        }

        // HDU3: CUBE, 3 quantized float planes.
        let hdu3 = parse_header_at(&out_bytes, hdu2.next_hdu_offset).unwrap();
        assert_eq!(hdu3.header.get_i64("ZNAXIS"), Some(3));
        assert_eq!(hdu3.header.get_i64("ZNAXIS3"), Some(3));
        assert_eq!(hdu3.header.get("EXTNAME").map(|v| v.trim()), Some("CUBE"));
        let planes = decode_compressed_planes(&out_bytes, &hdu3.header, hdu3.data_start).unwrap();
        assert_eq!(planes.len(), 3);
        let cube_original: Vec<f32> = (0..8 * 6 * 3).map(|i| (i as f32) * 0.5 - 3.0).collect();
        for (p, plane) in planes.iter().enumerate() {
            for (i, &d) in plane.iter().enumerate() {
                let o = cube_original[p * 8 * 6 + i];
                assert!((o - d).abs() < 2.0, "CUBE plane {p} mismatch: {o} vs {d}");
            }
        }

        // HDU4: AUX BinTable, byte-identical passthrough.
        let hdu4 = parse_header_at(&out_bytes, hdu3.next_hdu_offset).unwrap();
        assert_eq!(hdu4.header.get("XTENSION").map(|v| v.trim()), Some("BINTABLE"));
        assert_eq!(hdu4.header.get("EXTNAME").map(|v| v.trim()), Some("AUX"));

        // Locate the ORIGINAL HDU4 in the source file the same way, and
        // compare the raw bytes verbatim.
        let src_primary = parse_header_at(&source_bytes, 0).unwrap();
        let src_hdu1 = parse_header_at(&source_bytes, src_primary.next_hdu_offset).unwrap();
        let src_hdu2 = parse_header_at(&source_bytes, src_hdu1.next_hdu_offset).unwrap();
        let src_hdu3 = parse_header_at(&source_bytes, src_hdu2.next_hdu_offset).unwrap();
        let src_hdu4 = parse_header_at(&source_bytes, src_hdu3.next_hdu_offset).unwrap();
        assert_eq!(
            &out_bytes[hdu4.header_start..hdu4.next_hdu_offset],
            &source_bytes[src_hdu4.header_start..src_hdu4.next_hdu_offset],
            "AUX BinTable must be passed through byte-identical"
        );

        // (Compression-ratio assertions belong on realistic-sized data, not
        // this tiny synthetic fixture -- per-extension header overhead
        // dominates at this scale. See the real-file report instead.)

        let _ = std::fs::remove_file(&source_path);
        let _ = std::fs::remove_file(&output_path);
    }
}
