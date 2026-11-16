//! The dot product the exact scan runs once per stored vector, in exact
//! integer arithmetic: a port of Otzaria's `src/semantic/oxv/kernel.rs`
//! (otzaria-semantic-search d2a782a), step for step.
//!
//! - `i8-sym-dim`: the per-dimension scales are folded into the query once
//!   (`w_d = q_d * S_d / 127`), quantized to 16-bit integers under the
//!   largest factor `t` for which no sum can overflow; score = `acc / t`.
//! - `i8-sym-vec`: the query is quantized as it is; score = `(acc * s) / t`,
//!   the vector's own scale applied in f32 after the integer sum.
//! - `rot192-i8i4`: the query is rotated ONCE (`r = R q`), its head weighted
//!   by the spreads and its tail by the 4-bit steps, both quantized; score =
//!   `acc8 * s * (1 / t8) + (C + acc4 * (1 / t4))`.
//!
//! Every kernel (scalar, AVX2, NEON) computes the same integer (integer
//! addition is associative, wrapping alike), and the floats are combined in
//! one fixed order, so a scan's ranking is bit-identical on every CPU.

use crate::oxv::Codec;
use crate::{Error, Result};

pub(crate) type DotKernel = fn(&[i16], &[u8]) -> i32;
pub(crate) type NibbleKernel = fn(&[i16], &[i16], &[u8]) -> i32;

/// A query in the codec's space (kernel.rs `PreparedQuery`).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum PreparedQuery {
    Int8 { q16: Box<[i16]>, inv_scale: f32 },
    Int8PerVector { q16: Box<[i16]>, inv_scale: f32 },
    Rotated { dim: usize, head: Box<[i16]>, inv_head: f32, tail_lo: Box<[i16]>, tail_hi: Box<[i16]>, inv_tail: f32, constant: f32 },
    Float { q: Box<[f32]> },
}

impl PreparedQuery {
    /// kernel.rs `PreparedQuery::new`.
    pub(crate) fn new(query: &[f32], codec: &Codec, dim: usize) -> Result<Self> {
        if query.len() != dim {
            return Err(Error::Invalid(format!("query has {} dims, the vectors {dim}", query.len())));
        }
        Ok(match codec {
            Codec::Rot { head, scale, lo, hi, rotation, tail } => {
                let mut r = vec![0f64; head + tail];
                rotate(rotation, query, &mut r);
                let head_w: Vec<f64> = r[..*head].iter().zip(scale.iter()).map(|(r, s)| r * f64::from(*s)).collect();
                let (head_q, inv_head) = quantize_query(head_w, 127);
                let t = &r[*head..];
                let weights: Vec<f64> = t.iter().enumerate().map(|(d, r)| r * f64::from((hi[d] - lo[d]) / 15.0)).collect();
                let mut constant = 0f64;
                for (r, lo) in t.iter().zip(lo.iter()) {
                    constant += r * f64::from(*lo);
                }
                let (tail_q, inv_tail) = quantize_query(weights, 15);
                PreparedQuery::Rotated {
                    dim,
                    head: head_q,
                    inv_head,
                    tail_lo: tail_q.iter().step_by(2).copied().collect(),
                    tail_hi: tail_q.iter().skip(1).step_by(2).copied().collect(),
                    inv_tail,
                    constant: constant as f32,
                }
            }
            Codec::I8SymDim { scales } => {
                let w = query.iter().zip(scales).map(|(q, s)| f64::from(*q) * f64::from(*s) / 127.0).collect();
                let (q16, inv_scale) = quantize_query(w, 127);
                PreparedQuery::Int8 { q16, inv_scale }
            }
            Codec::I8SymVec => {
                let (q16, inv_scale) = quantize_query(query.iter().map(|q| f64::from(*q)).collect(), 127);
                PreparedQuery::Int8PerVector { q16, inv_scale }
            }
            Codec::F32 => PreparedQuery::Float { q: query.into() },
        })
    }
}

/// rotation.rs `rotate`: `out = R x`, each row summed in order in f64.
pub(crate) fn rotate(rotation: &[f32], x: &[f32], out: &mut [f64]) {
    let dim = x.len();
    for (value, row) in out.iter_mut().zip(rotation.chunks_exact(dim)) {
        let mut sum = 0f64;
        for (r, x) in row.iter().zip(x) {
            sum += f64::from(*r) * f64::from(*x);
        }
        *value = sum;
    }
}

/// kernel.rs `quantize_query`: `q16 = round(w * t)` with the largest `t`
/// keeping every `q16` in i16 and `sum |q16 * code|` in i32.
fn quantize_query(w: Vec<f64>, code_max: u32) -> (Box<[i16]>, f32) {
    let code = f64::from(code_max);
    let max = w.iter().fold(0.0f64, |max, x| max.max(x.abs()));
    let sum: f64 = w.iter().map(|x| x.abs()).sum();
    if !(max > 0.0 && max.is_finite() && sum.is_finite()) {
        return (vec![0; w.len()].into_boxed_slice(), 0.0);
    }
    let headroom = f64::from(i32::MAX) - code * w.len() as f64 / 2.0;
    let mut t = (32767.0 / max).min(headroom / (code * sum));
    loop {
        let q16: Vec<i16> = w.iter().map(|x| (x * t).round().clamp(-32767.0, 32767.0) as i16).collect();
        let bound: i64 = q16.iter().map(|q| i64::from(q.unsigned_abs()) * i64::from(code_max)).sum();
        if bound <= i64::from(i32::MAX) {
            return (q16.into_boxed_slice(), (1.0 / t) as f32);
        }
        t *= 0.999_999;
    }
}

/// kernel.rs `dot_scalar`: 16 lanes, wrapping, then the rest.
pub(crate) fn dot_scalar(q: &[i16], v: &[u8]) -> i32 {
    let n = q.len().min(v.len());
    let (q_chunks, q_rest) = q[..n].as_chunks::<16>();
    let (v_chunks, v_rest) = v[..n].as_chunks::<16>();
    let mut lanes = [0i32; 16];
    for (q, v) in q_chunks.iter().zip(v_chunks) {
        for lane in 0..16 {
            lanes[lane] = lanes[lane].wrapping_add(i32::from(q[lane]) * i32::from(v[lane] as i8));
        }
    }
    let mut total = lanes.iter().fold(0i32, |sum, lane| sum.wrapping_add(*lane));
    for (q, v) in q_rest.iter().zip(v_rest) {
        total = total.wrapping_add(i32::from(*q) * i32::from(*v as i8));
    }
    total
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn dot_avx2(q: &[i16], v: &[u8]) -> i32 {
    use std::arch::x86_64::{
        __m128i, __m256i, _mm256_add_epi32, _mm256_castsi256_si128, _mm256_cvtepi8_epi16, _mm256_extracti128_si256,
        _mm256_loadu_si256, _mm256_madd_epi16, _mm256_setzero_si256, _mm_add_epi32, _mm_cvtsi128_si32, _mm_loadu_si128,
        _mm_shuffle_epi32,
    };
    let n = q.len().min(v.len());
    let steps = n / 16;
    let mut acc = _mm256_setzero_si256();
    for step in 0..steps {
        let at = step * 16;
        // SAFETY: at + 16 <= n, both slices at least that long; unaligned loads.
        let (bytes, query) = unsafe {
            (_mm_loadu_si128(v.as_ptr().add(at).cast::<__m128i>()), _mm256_loadu_si256(q.as_ptr().add(at).cast::<__m256i>()))
        };
        acc = _mm256_add_epi32(acc, _mm256_madd_epi16(query, _mm256_cvtepi8_epi16(bytes)));
    }
    let halves = _mm_add_epi32(_mm256_castsi256_si128(acc), _mm256_extracti128_si256::<1>(acc));
    let pairs = _mm_add_epi32(halves, _mm_shuffle_epi32::<0b01_00_11_10>(halves));
    let all = _mm_add_epi32(pairs, _mm_shuffle_epi32::<0b10_11_00_01>(pairs));
    let mut total = _mm_cvtsi128_si32(all);
    for at in steps * 16..n {
        total = total.wrapping_add(i32::from(q[at]) * i32::from(v[at] as i8));
    }
    total
}

#[cfg(target_arch = "x86_64")]
fn dot_avx2_detected(q: &[i16], v: &[u8]) -> i32 {
    // SAFETY: only returned by `select` after `is_x86_feature_detected!("avx2")` (and the tests check the same).
    unsafe { dot_avx2(q, v) }
}

#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
fn dot_neon(q: &[i16], v: &[u8]) -> i32 {
    use std::arch::aarch64::{
        vaddq_s32, vaddvq_s32, vdupq_n_s32, vget_low_s16, vget_low_s8, vld1q_s16, vld1q_s8, vmlal_high_s16, vmlal_s16,
        vmovl_high_s8, vmovl_s8,
    };
    let n = q.len().min(v.len());
    let steps = n / 16;
    let (mut low, mut high) = (vdupq_n_s32(0), vdupq_n_s32(0));
    for step in 0..steps {
        let at = step * 16;
        // SAFETY: at + 16 <= n: 16 bytes of v and twice 8 lanes of q.
        let (bytes, q0, q1) =
            unsafe { (vld1q_s8(v.as_ptr().add(at).cast::<i8>()), vld1q_s16(q.as_ptr().add(at)), vld1q_s16(q.as_ptr().add(at + 8))) };
        let v0 = vmovl_s8(vget_low_s8(bytes));
        let v1 = vmovl_high_s8(bytes);
        low = vmlal_s16(low, vget_low_s16(q0), vget_low_s16(v0));
        high = vmlal_high_s16(high, q0, v0);
        low = vmlal_s16(low, vget_low_s16(q1), vget_low_s16(v1));
        high = vmlal_high_s16(high, q1, v1);
    }
    let mut total = vaddvq_s32(vaddq_s32(low, high));
    for at in steps * 16..n {
        total = total.wrapping_add(i32::from(q[at]) * i32::from(v[at] as i8));
    }
    total
}

#[cfg(target_arch = "aarch64")]
fn dot_neon_baseline(q: &[i16], v: &[u8]) -> i32 {
    // SAFETY: NEON is part of the aarch64 baseline.
    unsafe { dot_neon(q, v) }
}

/// kernel.rs `select`: AVX2 when detected, NEON on aarch64, else scalar.
pub(crate) fn select() -> (DotKernel, &'static str) {
    #[cfg(target_arch = "x86_64")]
    if std::arch::is_x86_feature_detected!("avx2") {
        return (dot_avx2_detected, "avx2");
    }
    #[cfg(target_arch = "aarch64")]
    return (dot_neon_baseline, "neon");
    #[allow(unreachable_code)]
    (dot_scalar, "scalar")
}

#[cfg(test)]
pub(crate) fn available() -> Vec<(DotKernel, &'static str)> {
    let mut kernels: Vec<(DotKernel, &'static str)> = vec![(dot_scalar, "scalar")];
    #[cfg(target_arch = "x86_64")]
    if std::arch::is_x86_feature_detected!("avx2") {
        kernels.push((dot_avx2_detected, "avx2"));
    }
    #[cfg(target_arch = "aarch64")]
    kernels.push((dot_neon_baseline, "neon"));
    kernels
}

/// kernel.rs `nibbles_scalar`: low nibble against `lo`, high against `hi`.
pub(crate) fn nibbles_scalar(lo: &[i16], hi: &[i16], packed: &[u8]) -> i32 {
    let mut total = 0i32;
    for ((lo, hi), byte) in lo.iter().zip(hi).zip(packed) {
        total = total.wrapping_add(i32::from(*lo) * i32::from(byte & 0x0F)).wrapping_add(i32::from(*hi) * i32::from(byte >> 4));
    }
    total
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn nibbles_avx2(lo: &[i16], hi: &[i16], packed: &[u8]) -> i32 {
    use std::arch::x86_64::{
        __m128i, __m256i, _mm256_add_epi32, _mm256_and_si256, _mm256_castsi256_si128, _mm256_cvtepu8_epi16,
        _mm256_extracti128_si256, _mm256_loadu_si256, _mm256_madd_epi16, _mm256_set1_epi16, _mm256_setzero_si256,
        _mm256_srli_epi16, _mm_add_epi32, _mm_cvtsi128_si32, _mm_loadu_si128, _mm_shuffle_epi32,
    };
    let n = lo.len().min(hi.len()).min(packed.len());
    let steps = n / 16;
    let mask = _mm256_set1_epi16(0x0F);
    let mut acc = _mm256_setzero_si256();
    for step in 0..steps {
        let at = step * 16;
        // SAFETY: at + 16 <= n for all three slices; unaligned loads.
        let (bytes, low_q, high_q) = unsafe {
            (
                _mm_loadu_si128(packed.as_ptr().add(at).cast::<__m128i>()),
                _mm256_loadu_si256(lo.as_ptr().add(at).cast::<__m256i>()),
                _mm256_loadu_si256(hi.as_ptr().add(at).cast::<__m256i>()),
            )
        };
        let wide = _mm256_cvtepu8_epi16(bytes);
        let low = _mm256_and_si256(wide, mask);
        let high = _mm256_srli_epi16::<4>(wide);
        acc = _mm256_add_epi32(acc, _mm256_madd_epi16(low_q, low));
        acc = _mm256_add_epi32(acc, _mm256_madd_epi16(high_q, high));
    }
    let halves = _mm_add_epi32(_mm256_castsi256_si128(acc), _mm256_extracti128_si256::<1>(acc));
    let pairs = _mm_add_epi32(halves, _mm_shuffle_epi32::<0b01_00_11_10>(halves));
    let all = _mm_add_epi32(pairs, _mm_shuffle_epi32::<0b10_11_00_01>(pairs));
    let total = _mm_cvtsi128_si32(all);
    let rest = steps * 16;
    total.wrapping_add(nibbles_scalar(&lo[rest..n], &hi[rest..n], &packed[rest..n]))
}

#[cfg(target_arch = "x86_64")]
fn nibbles_avx2_detected(lo: &[i16], hi: &[i16], packed: &[u8]) -> i32 {
    // SAFETY: only returned by `select_nibbles` after the AVX2 check.
    unsafe { nibbles_avx2(lo, hi, packed) }
}

#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
fn nibbles_neon(lo: &[i16], hi: &[i16], packed: &[u8]) -> i32 {
    use std::arch::aarch64::{
        vaddq_s32, vaddvq_s32, vandq_u16, vdupq_n_s32, vdupq_n_u16, vget_low_s16, vget_low_u8, vld1q_s16, vld1q_u8,
        vmlal_high_s16, vmlal_s16, vmovl_high_u8, vmovl_u8, vreinterpretq_s16_u16, vshrq_n_u16,
    };
    let n = lo.len().min(hi.len()).min(packed.len());
    let steps = n / 16;
    let mask = vdupq_n_u16(0x0F);
    let (mut low_acc, mut high_acc) = (vdupq_n_s32(0), vdupq_n_s32(0));
    for step in 0..steps {
        let at = step * 16;
        // SAFETY: at + 16 <= n for all three slices.
        let (bytes, lo0, lo1, hi0, hi1) = unsafe {
            (
                vld1q_u8(packed.as_ptr().add(at)),
                vld1q_s16(lo.as_ptr().add(at)),
                vld1q_s16(lo.as_ptr().add(at + 8)),
                vld1q_s16(hi.as_ptr().add(at)),
                vld1q_s16(hi.as_ptr().add(at + 8)),
            )
        };
        let (w0, w1) = (vmovl_u8(vget_low_u8(bytes)), vmovl_high_u8(bytes));
        let (l0, l1) = (vreinterpretq_s16_u16(vandq_u16(w0, mask)), vreinterpretq_s16_u16(vandq_u16(w1, mask)));
        let (h0, h1) = (vreinterpretq_s16_u16(vshrq_n_u16::<4>(w0)), vreinterpretq_s16_u16(vshrq_n_u16::<4>(w1)));
        low_acc = vmlal_s16(low_acc, vget_low_s16(lo0), vget_low_s16(l0));
        high_acc = vmlal_high_s16(high_acc, lo0, l0);
        low_acc = vmlal_s16(low_acc, vget_low_s16(lo1), vget_low_s16(l1));
        high_acc = vmlal_high_s16(high_acc, lo1, l1);
        low_acc = vmlal_s16(low_acc, vget_low_s16(hi0), vget_low_s16(h0));
        high_acc = vmlal_high_s16(high_acc, hi0, h0);
        low_acc = vmlal_s16(low_acc, vget_low_s16(hi1), vget_low_s16(h1));
        high_acc = vmlal_high_s16(high_acc, hi1, h1);
    }
    let total = vaddvq_s32(vaddq_s32(low_acc, high_acc));
    let rest = steps * 16;
    total.wrapping_add(nibbles_scalar(&lo[rest..n], &hi[rest..n], &packed[rest..n]))
}

#[cfg(target_arch = "aarch64")]
fn nibbles_neon_baseline(lo: &[i16], hi: &[i16], packed: &[u8]) -> i32 {
    // SAFETY: NEON is part of the aarch64 baseline.
    unsafe { nibbles_neon(lo, hi, packed) }
}

/// kernel.rs `select_nibbles`.
pub(crate) fn select_nibbles() -> (NibbleKernel, &'static str) {
    #[cfg(target_arch = "x86_64")]
    if std::arch::is_x86_feature_detected!("avx2") {
        return (nibbles_avx2_detected, "avx2");
    }
    #[cfg(target_arch = "aarch64")]
    return (nibbles_neon_baseline, "neon");
    #[allow(unreachable_code)]
    (nibbles_scalar, "scalar")
}

#[cfg(test)]
pub(crate) fn available_nibbles() -> Vec<(NibbleKernel, &'static str)> {
    let mut kernels: Vec<(NibbleKernel, &'static str)> = vec![(nibbles_scalar, "scalar")];
    #[cfg(target_arch = "x86_64")]
    if std::arch::is_x86_feature_detected!("avx2") {
        kernels.push((nibbles_avx2_detected, "avx2"));
    }
    #[cfg(target_arch = "aarch64")]
    kernels.push((nibbles_neon_baseline, "neon"));
    kernels
}

/// kernel.rs `dot_f32`: eight lanes summed in one fixed order.
pub(crate) fn dot_f32(q: &[f32], v: &[u8]) -> f32 {
    let n = q.len().min(v.len() / 4);
    let (q_chunks, q_rest) = q[..n].as_chunks::<8>();
    let (v_chunks, _) = v[..n * 4].as_chunks::<32>();
    let mut lanes = [0f32; 8];
    for (q, v) in q_chunks.iter().zip(v_chunks) {
        let (v, _) = v.as_chunks::<4>();
        for lane in 0..8 {
            lanes[lane] += q[lane] * f32::from_le_bytes(v[lane]);
        }
    }
    let mut total = ((lanes[0] + lanes[1]) + (lanes[2] + lanes[3])) + ((lanes[4] + lanes[5]) + (lanes[6] + lanes[7]));
    let tail = &v[q_chunks.len() * 32..n * 4];
    for (q, v) in q_rest.iter().zip(tail.as_chunks::<4>().0) {
        total += q * f32::from_le_bytes(*v);
    }
    total
}

/// kernel.rs `ordered`: an f32's total order as an i32.
pub(crate) fn ordered(score: f32) -> i32 {
    let bits = score.to_bits() as i32;
    bits ^ (((bits >> 31) as u32) >> 1) as i32
}

/// kernel.rs `unordered`: the inverse of [`ordered`].
pub(crate) fn unordered(rank: i32) -> f32 {
    f32::from_bits((rank ^ (((rank >> 31) as u32) >> 1) as i32) as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Random(u64);
    impl Random {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }
    }

    #[test]
    fn every_dot_kernel_computes_the_scalar_integer() {
        let mut r = Random(21);
        for dim in 1..=300 {
            for extreme in [false, true] {
                let q: Vec<i16> = (0..dim)
                    .map(|_| if extreme { if r.next() % 2 == 0 { 32767 } else { -32767 } } else { (r.next() % 65535) as i32 as i16 })
                    .collect();
                let v: Vec<u8> = (0..dim)
                    .map(|_| if extreme { (if r.next() % 2 == 0 { 127i8 } else { -127 }) as u8 } else { (r.next() % 255) as u8 })
                    .collect();
                let exact: i64 = q.iter().zip(&v).map(|(q, v)| i64::from(*q) * i64::from(*v as i8)).sum();
                let reference = dot_scalar(&q, &v);
                assert_eq!(i64::from(reference), exact);
                for (k, name) in available() {
                    assert_eq!(k(&q, &v), reference, "{name} dim {dim}");
                }
            }
        }
    }

    #[test]
    fn every_nibble_kernel_computes_the_scalar_integer() {
        let mut r = Random(25);
        for bytes in 1..=80 {
            let lo: Vec<i16> = (0..bytes).map(|_| (r.next() % 65535) as i32 as i16).collect();
            let hi: Vec<i16> = (0..bytes).map(|_| (r.next() % 65535) as i32 as i16).collect();
            let packed: Vec<u8> = (0..bytes).map(|_| (r.next() % 256) as u8).collect();
            let reference = nibbles_scalar(&lo, &hi, &packed);
            for (k, name) in available_nibbles() {
                assert_eq!(k(&lo, &hi, &packed), reference, "{name} at {bytes}");
            }
        }
    }

    #[test]
    fn ordered_is_a_total_order_and_reversible() {
        let xs = [-2.0f32, -0.5, -0.0, 0.0, 1e-9, 0.25, 3.0];
        for w in xs.windows(2) {
            assert!(ordered(w[0]) <= ordered(w[1]));
        }
        for x in xs {
            assert_eq!(unordered(ordered(x)).to_bits(), x.to_bits());
        }
    }
}
