//! RICE_1 tile encoder -- the bit-exact inverse of `rice_decode` (rice.rs).
//!
//! Correctness requirement: whatever this emits must round-trip through the
//! existing `rice_decode` (which is itself verified bit-exact against
//! astropy/cfitsio). The *specific* split-parameter (`fs`) chosen per block
//! only affects compression ratio, not correctness -- `rice_decode` accepts
//! any valid encoding of a block via the "normal" Golomb-Rice path (unary
//! zero-run + terminator + fs-bit remainder), so this encoder does not need
//! to reproduce cfitsio's exact `fs`-selection heuristic, only to pick *a*
//! valid one. See the module test at the bottom for the modular-arithmetic
//! argument for why per-pixel differences never need special-casing for
//! wraparound.

use super::rice::{sign_extend, RiceParams};

/// MSB-first bit writer: `push_bits` appends the low `width` bits of `value`
/// to the stream, most-significant bit first, flushing whole bytes as they
/// accumulate. Mirrors the reservoir the decoder's bit-reader consumes from.
struct BitWriter {
    out: Vec<u8>,
    buf: u64,
    nbits: u32,
}

impl BitWriter {
    fn new() -> Self {
        Self { out: Vec::new(), buf: 0, nbits: 0 }
    }

    fn push_bits(&mut self, value: u32, width: u32) {
        if width == 0 {
            return;
        }
        let mask = (1u64 << width) - 1;
        self.buf = (self.buf << width) | (value as u64 & mask);
        self.nbits += width;
        while self.nbits >= 8 {
            self.nbits -= 8;
            self.out.push(((self.buf >> self.nbits) & 0xFF) as u8);
        }
    }

    /// Push `n` zero bits (the unary quotient prefix). `n` can be large for
    /// pathological/high-entropy blocks, so this chunks the write instead of
    /// requiring `n` to fit a single `push_bits` call.
    fn push_zero_bits(&mut self, mut n: u64) {
        const CHUNK: u32 = 24;
        while n >= CHUNK as u64 {
            self.push_bits(0, CHUNK);
            n -= CHUNK as u64;
        }
        if n > 0 {
            self.push_bits(0, n as u32);
        }
    }

    fn finish(self) -> Vec<u8> {
        let mut out = self.out;
        if self.nbits > 0 {
            let pad = 8 - self.nbits;
            out.push(((self.buf << pad) & 0xFF) as u8);
        }
        out
    }
}

/// Encode `pixels` (raw stored integers, same representation `rice_decode`
/// returns -- i.e. sign-extended per `params.signed`, or 0..255 for
/// unsigned byte pixels) into a RICE_1 tile byte stream decodable by
/// `rice_decode` with the same `params`.
pub fn rice_encode(pixels: &[i64], params: &RiceParams) -> Vec<u8> {
    let (fsbits, fsmax, init_bytes, width_bits): (u32, u32, usize, u32) = match params.bytepix {
        1 => (3, 6, 1, 8),
        2 => (4, 14, 2, 16),
        4 => (5, 25, 4, 32),
        other => panic!("unsupported Rice BYTEPIX {other}"),
    };
    let trunc_mask: u32 = if width_bits >= 32 {
        u32::MAX
    } else {
        (1u32 << width_bits) - 1
    };

    let nx = pixels.len();
    let mut out = Vec::new();
    if nx == 0 {
        return out;
    }

    // Truncate a raw stored integer to its `width_bits`-wide unsigned bit
    // pattern (the inverse of the decoder's final sign-extension step).
    let to_u32 = |v: i64| -> u32 { (v as u64 as u32) & trunc_mask };

    let lastpix0 = to_u32(pixels[0]);
    let be = lastpix0.to_be_bytes();
    out.extend_from_slice(&be[4 - init_bytes..]);

    let mut writer = BitWriter::new();
    let mut lastpix = lastpix0;

    let mut i = 0usize;
    while i < nx {
        let imax = (i + params.blocksize as usize).min(nx);

        // Per-pixel zigzag-encoded modular difference. Because both `pu` and
        // `lastpix` are already reduced mod 2^width_bits, `diff_target` is
        // exactly the modular delta; re-centering it via `sign_extend` picks
        // the minimal-magnitude representative, which is what makes the
        // zigzag value always fit in exactly `width_bits` bits (needed for
        // the verbatim/high-entropy fallback) regardless of how far apart
        // the two raw pixel values are.
        let mut us: Vec<u32> = Vec::with_capacity(imax - i);
        let mut cur = lastpix;
        for &pixel in &pixels[i..imax] {
            let pu = to_u32(pixel);
            let diff_target = pu.wrapping_sub(cur) & trunc_mask;
            let d = sign_extend(diff_target, width_bits);
            let u: u32 = if d >= 0 {
                (d as u64 * 2) as u32
            } else {
                ((-d) as u64 * 2 - 1) as u32
            };
            us.push(u);
            cur = pu;
        }
        lastpix = cur;

        if us.iter().all(|&u| u == 0) {
            // All pixels in this block equal the value entering it: the
            // fs<0 sentinel (raw field 0) lets the decoder repeat `lastpix`
            // for the whole block with zero further bits.
            writer.push_bits(0, fsbits);
        } else {
            // Search the normal Golomb-Rice fs range for the cheapest
            // encoding, and compare against the verbatim fallback.
            let mut best_fs = 0u32;
            let mut best_cost = u64::MAX;
            for fs in 0..fsmax {
                let cost: u64 = us
                    .iter()
                    .map(|&u| (u >> fs) as u64 + 1 + fs as u64)
                    .sum();
                if cost < best_cost {
                    best_cost = cost;
                    best_fs = fs;
                }
            }

            let verbatim_cost = us.len() as u64 * width_bits as u64;
            if verbatim_cost < best_cost {
                writer.push_bits(fsmax + 1, fsbits);
                for &u in &us {
                    writer.push_bits(u, width_bits);
                }
            } else {
                writer.push_bits(best_fs + 1, fsbits);
                for &u in &us {
                    let quotient = (u >> best_fs) as u64;
                    let remainder = u & ((1u32 << best_fs) - 1);
                    writer.push_zero_bits(quotient);
                    writer.push_bits(1, 1);
                    writer.push_bits(remainder, best_fs);
                }
            }
        }

        i = imax;
    }

    out.extend(writer.finish());
    out
}

#[cfg(test)]
mod tests {
    use super::super::rice::rice_decode;
    use super::*;

    fn roundtrip(pixels: &[i64], params: &RiceParams) {
        let encoded = rice_encode(pixels, params);
        let decoded = rice_decode(&encoded, pixels.len(), params);
        assert_eq!(decoded, pixels, "round-trip mismatch for {pixels:?}");
    }

    #[test]
    fn roundtrip_i16_small_tile() {
        // Same pixel values as rice.rs's `matches_astropy_i16_single_row`
        // fixture -- proves our encoder's bytes need not match astropy's
        // captured bytes, only that they decode back to the same pixels.
        roundtrip(
            &[5, 5, 5, 5, 100, -100, 0, 7],
            &RiceParams { blocksize: 32, bytepix: 2, signed: true },
        );
    }

    #[test]
    fn roundtrip_byte_tile_unsigned() {
        let pixels: Vec<i64> = vec![
            31, 14, 6, 36, 31, 255, 255, 255, 255, 54, 41, 59, 63, 48, 71, 50, 51, 72, 73, 56, 87,
            64, 92, 63, 97, 100, 103, 101, 96, 109, 95, 118, 112, 111, 116, 121, 103, 110, 117,
            115,
        ];
        roundtrip(&pixels, &RiceParams { blocksize: 32, bytepix: 1, signed: false });
    }

    #[test]
    fn roundtrip_constant_block() {
        // All-zero-diff sentinel path.
        roundtrip(&[42; 64], &RiceParams { blocksize: 32, bytepix: 2, signed: true });
    }

    #[test]
    fn roundtrip_high_entropy_block() {
        // Large jumps every pixel -- exercises the verbatim fallback path.
        let pixels: Vec<i64> = (0..40)
            .map(|i| if i % 2 == 0 { 32000 } else { -32000 })
            .collect();
        roundtrip(&pixels, &RiceParams { blocksize: 32, bytepix: 2, signed: true });
    }

    #[test]
    fn roundtrip_int32_wide() {
        let pixels: Vec<i64> = (0..50).map(|i| (i * i - 500) as i64).collect();
        roundtrip(&pixels, &RiceParams { blocksize: 32, bytepix: 4, signed: true });
    }

    #[test]
    fn roundtrip_int32_beyond_f32_mantissa() {
        // Values here (~2^30) exceed f32's 24-bit exact-integer range --
        // this proves the Rice codec itself (as used by the MEF writer's
        // lossless-integer path, which reads/writes raw i64 directly, never
        // going through an f32 conversion) is exact at this magnitude, even
        // though the app's *decode* pipeline elsewhere (tiles.rs::scale_ints)
        // downconverts to f32 for general consumption.
        let base = 1_073_741_824i64; // 2^30
        let pixels: Vec<i64> = (0..40)
            .map(|i| if i % 2 == 0 { base + i } else { -base - i })
            .collect();
        roundtrip(&pixels, &RiceParams { blocksize: 32, bytepix: 4, signed: true });
    }

    #[test]
    fn roundtrip_non_block_aligned_width() {
        // nx not a multiple of blocksize, matching the decoder's comment
        // about non-block-aligned tile widths.
        let pixels: Vec<i64> = (0..37).map(|i| (i % 13) - 6).collect();
        roundtrip(&pixels, &RiceParams { blocksize: 32, bytepix: 2, signed: true });
    }

    #[test]
    fn roundtrip_single_pixel() {
        roundtrip(&[123], &RiceParams { blocksize: 32, bytepix: 4, signed: true });
    }

    #[test]
    fn roundtrip_random_stream() {
        // A simple xorshift PRNG (deterministic, no external crate) exercising
        // a mix of small/large/negative diffs across many blocks.
        let mut state: u32 = 0x9E3779B9;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state
        };
        let pixels: Vec<i64> = (0..500)
            .map(|_| (next() as i32 % 20000) as i64)
            .collect();
        roundtrip(&pixels, &RiceParams { blocksize: 32, bytepix: 4, signed: true });
    }

    #[test]
    fn roundtrip_wide_high_entropy_bytepix4_hits_zero_leftover_bits() {
        // Regression test for a real crash found compressing a full-size
        // (2040-column) int32/quantized-float32 row: rice.rs's decoder had
        // `let mut diff: u32 = b << k;` where `k = bbits - nbits` can be
        // exactly 32 (BYTEPIX=4's bbits) whenever `nbits == 0` entering a
        // high-entropy (verbatim) block -- a legitimately reachable state,
        // just one this codebase's earlier small-scale tests never
        // happened to land on. `b << 32` panics (invalid shift for u32).
        // A wide (2040, matching real image rows), full-i32-range random
        // stream reliably cycles through every possible leftover-bits
        // state across its ~64 blocks, including nbits==0, so this
        // reproduces the crash deterministically rather than by luck.
        let mut state: u32 = 0xDEADBEEF;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state
        };
        let pixels: Vec<i64> = (0..2040).map(|_| next() as i32 as i64).collect();
        roundtrip(&pixels, &RiceParams { blocksize: 32, bytepix: 4, signed: true });
    }
}
