//! IEEE 754 half precision, the storage form of the full-precision vectors
//! (see the crate docs for why f16 and not f32).

use std::sync::OnceLock;

/// f32 -> f16 bits, round to nearest, ties to even (overflow -> infinity).
pub fn f32_to_f16(x: f32) -> u16 {
    let b = x.to_bits();
    let sign = ((b >> 16) & 0x8000) as u16;
    let exp = ((b >> 23) & 0xff) as i32;
    let man = b & 0x7f_ffff;
    if exp == 0xff {
        return sign | 0x7c00 | if man != 0 { 0x200 } else { 0 };
    }
    let e = exp - 127 + 15;
    if e >= 0x1f {
        return sign | 0x7c00;
    }
    if e <= 0 {
        if e < -10 {
            return sign;
        }
        let m = man | 0x80_0000;
        let shift = (14 - e) as u32;
        let half = 1u32 << (shift - 1);
        let mut r = m >> shift;
        let rem = m & ((1u32 << shift) - 1);
        if rem > half || (rem == half && (r & 1) == 1) {
            r += 1;
        }
        return sign | r as u16;
    }
    let mut r = ((e as u32) << 10) | (man >> 13);
    let rem = man & 0x1fff;
    if rem > 0x1000 || (rem == 0x1000 && (r & 1) == 1) {
        r += 1; // a carry into the exponent is the correct rounding
    }
    sign | r as u16
}

/// f16 bits -> f32 (exact).
pub fn f16_to_f32(h: u16) -> f32 {
    let sign = ((h & 0x8000) as u32) << 16;
    let exp = ((h >> 10) & 0x1f) as u32;
    let man = (h & 0x3ff) as u32;
    let bits = if exp == 0 {
        if man == 0 {
            sign
        } else {
            let mut e = 113u32;
            let mut m = man;
            while m & 0x400 == 0 {
                m <<= 1;
                e -= 1;
            }
            sign | (e << 23) | ((m & 0x3ff) << 13)
        }
    } else if exp == 0x1f {
        sign | 0x7f80_0000 | (man << 13)
    } else {
        sign | ((exp + 112) << 23) | (man << 13)
    };
    f32::from_bits(bits)
}

/// Every f16 value as f32 (256 KB, built on first use).
pub(crate) fn table() -> &'static [f32] {
    static TABLE: OnceLock<Vec<f32>> = OnceLock::new();
    TABLE.get_or_init(|| (0..=u16::MAX).map(f16_to_f32).collect())
}

/// Dot product of an f32 query with a stored f16 row.
#[inline]
pub(crate) fn dot(query: &[f32], row: &[u16], table: &[f32]) -> f32 {
    // four accumulators: lets the compiler keep independent sums in flight
    let mut acc = [0f32; 4];
    let chunks = query.len() / 4 * 4;
    let mut i = 0;
    while i < chunks {
        acc[0] += query[i] * table[row[i] as usize];
        acc[1] += query[i + 1] * table[row[i + 1] as usize];
        acc[2] += query[i + 2] * table[row[i + 2] as usize];
        acc[3] += query[i + 3] * table[row[i + 3] as usize];
        i += 4;
    }
    let mut sum = (acc[0] + acc[1]) + (acc[2] + acc[3]);
    while i < query.len() {
        sum += query[i] * table[row[i] as usize];
        i += 1;
    }
    sum
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_rounds() {
        for x in [0.0f32, -0.0, 1.0, -2.5, 0.333_333, 65504.0, 6.0e-8, 1.0e-5, -0.7071] {
            let back = f16_to_f32(f32_to_f16(x));
            assert!((back - x).abs() <= x.abs() * 1e-3 + 6e-8, "{x} -> {back}");
        }
        assert_eq!(f32_to_f16(1.0), 0x3c00);
        assert_eq!(f32_to_f16(1e6), 0x7c00);
        // every f16 survives f16 -> f32 -> f16 (NaN aside)
        for h in 0..=u16::MAX {
            let f = f16_to_f32(h);
            if !f.is_nan() {
                assert_eq!(f32_to_f16(f), h);
            }
        }
    }
}
