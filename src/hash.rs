//! 128-bit hashing of ID bytes with AES rounds. For inputs of at most 16
//! bytes the map is a bijection for each length, so equal hashes mean equal
//! bytes; longer inputs collide with probability about 2^-128.

use std::arch::x86_64::*;

static MASKS: [u8; 32] = [
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
];

const K0: (i64, i64) = (0x243f_6a88_85a3_08d3u64 as i64, 0x1319_8a2e_0370_7344u64 as i64);
const K1: (i64, i64) = (0xa409_3822_299f_31d0u64 as i64, 0x082e_fa98_ec4e_6c89u64 as i64);
const K2: (i64, i64) = (0x4528_21e6_38d0_1377u64 as i64, 0xbe54_66cf_34e9_0c6cu64 as i64);
const K3: (i64, i64) = (0xc0ac_29b7_c97c_50ddu64 as i64, 0x3f84_d5b5_b547_0917u64 as i64);
const K4: (i64, i64) = (0x9216_d5d9_8979_fb1bu64 as i64, 0xd131_0ba6_98df_b5acu64 as i64);

#[inline(always)]
unsafe fn key(k: (i64, i64)) -> __m128i {
    _mm_set_epi64x(k.1, k.0)
}

const LEN_MUL: u64 = 0x9e37_79b9_7f4a_7c15;

/// `K0 ^ [len * LEN_MUL, len]` for the lengths up to 48, so the short paths
/// start with one load instead of building the length vector.
#[repr(C, align(16))]
struct Start([[u64; 2]; 49]);

static START: Start = {
    let mut t = [[0u64; 2]; 49];
    let mut len = 0;
    while len < 49 {
        t[len] = [K0.0 as u64 ^ (len as u64).wrapping_mul(LEN_MUL), K0.1 as u64 ^ len as u64];
        len += 1;
    }
    Start(t)
};

#[inline(always)]
unsafe fn start(len: usize) -> __m128i {
    _mm_load_si128(START.0.as_ptr().add(len) as *const __m128i)
}

/// Hash of `len` bytes at `p`. May read up to 16 bytes past the end.
#[inline(always)]
pub unsafe fn hash128(p: *const u8, len: usize) -> (u64, u64) {
    let mut s;
    if len <= 16 {
        let m = _mm_loadu_si128(MASKS.as_ptr().add(16 - len) as *const __m128i);
        let v = _mm_and_si128(_mm_loadu_si128(p as *const __m128i), m);
        s = _mm_xor_si128(v, start(len));
        s = _mm_aesenc_si128(s, key(K1));
        s = _mm_aesenc_si128(s, key(K2));
        s = _mm_aesenc_si128(s, key(K3));
    } else if len <= 32 {
        let a = _mm_loadu_si128(p as *const __m128i);
        let b = _mm_loadu_si128(p.add(len - 16) as *const __m128i);
        s = _mm_xor_si128(a, start(len));
        s = _mm_aesenc_si128(s, key(K1));
        s = _mm_aesenc_si128(_mm_xor_si128(s, b), key(K2));
        s = _mm_aesenc_si128(s, key(K3));
        s = _mm_aesenc_si128(s, key(K4));
    } else if len <= 48 {
        let a = _mm_loadu_si128(p as *const __m128i);
        let b = _mm_loadu_si128(p.add(16) as *const __m128i);
        let c = _mm_loadu_si128(p.add(len - 16) as *const __m128i);
        s = _mm_xor_si128(a, start(len));
        s = _mm_aesenc_si128(s, key(K1));
        s = _mm_aesenc_si128(_mm_xor_si128(s, b), key(K2));
        s = _mm_aesenc_si128(_mm_xor_si128(s, c), key(K3));
        s = _mm_aesenc_si128(s, key(K4));
        s = _mm_aesenc_si128(s, key(K1));
    } else {
        let lenv = _mm_set_epi64x(len as i64, (len as u64).wrapping_mul(LEN_MUL) as i64);
        s = _mm_xor_si128(key(K0), lenv);
        let mut off = 0;
        while off + 16 <= len {
            let v = _mm_loadu_si128(p.add(off) as *const __m128i);
            s = _mm_aesenc_si128(_mm_xor_si128(s, v), key(K1));
            s = _mm_aesenc_si128(s, key(K2));
            off += 16;
        }
        let rest = len - off;
        if rest > 0 {
            let m = _mm_loadu_si128(MASKS.as_ptr().add(16 - rest) as *const __m128i);
            let v = _mm_and_si128(_mm_loadu_si128(p.add(off) as *const __m128i), m);
            s = _mm_aesenc_si128(_mm_xor_si128(s, v), key(K3));
        }
        s = _mm_aesenc_si128(s, key(K4));
        s = _mm_aesenc_si128(s, key(K1));
        s = _mm_aesenc_si128(s, key(K2));
    }
    (_mm_cvtsi128_si64(s) as u64, _mm_extract_epi64::<1>(s) as u64)
}

/// A small read-only map from ID bytes to a number, for products and campaigns.
pub struct StaticTable {
    slots: Vec<[u64; 2]>,
    shift: u32,
    mask: usize,
}

impl StaticTable {
    pub fn new(expected: usize) -> Self {
        // Load factor between a quarter and a half.
        let cap = (expected.max(8) * 2).next_power_of_two();
        StaticTable {
            slots: vec![[0, 0]; cap],
            shift: 64 - cap.trailing_zeros(),
            mask: cap - 1,
        }
    }

    /// Keeps the first value for a key.
    pub fn insert(&mut self, h: (u64, u64), value: u32) {
        let tag = (h.0 & 0xffff_ffff) << 32;
        let mut i = (h.0 >> self.shift) as usize;
        loop {
            let s = self.slots[i];
            if s[1] == 0 {
                self.slots[i] = [h.1, tag | (value as u64 + 1)];
                return;
            }
            if s[1] & 0xffff_ffff_0000_0000 == tag && s[0] == h.1 {
                return;
            }
            i = (i + 1) & self.mask;
        }
    }

    #[inline(always)]
    pub fn get(&self, h: (u64, u64)) -> Option<u32> {
        let tag = (h.0 & 0xffff_ffff) << 32;
        let mut i = (h.0 >> self.shift) as usize;
        loop {
            let s = unsafe { *self.slots.get_unchecked(i) };
            if s[1] == 0 {
                return None;
            }
            if s[1] & 0xffff_ffff_0000_0000 == tag && s[0] == h.1 {
                return Some((s[1] as u32).wrapping_sub(1));
            }
            i = (i + 1) & self.mask;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The hash as it was before the length table, kept verbatim as the reference.
    unsafe fn hash128_old(p: *const u8, len: usize) -> (u64, u64) {
        let lenv = _mm_set_epi64x(len as i64, (len as i64).wrapping_mul(0x9e37_79b9_7f4a_7c15u64 as i64));
        let mut s;
        if len <= 16 {
            let m = _mm_loadu_si128(MASKS.as_ptr().add(16 - len) as *const __m128i);
            let v = _mm_and_si128(_mm_loadu_si128(p as *const __m128i), m);
            s = _mm_xor_si128(_mm_xor_si128(v, key(K0)), lenv);
            s = _mm_aesenc_si128(s, key(K1));
            s = _mm_aesenc_si128(s, key(K2));
            s = _mm_aesenc_si128(s, key(K3));
        } else if len <= 32 {
            let a = _mm_loadu_si128(p as *const __m128i);
            let b = _mm_loadu_si128(p.add(len - 16) as *const __m128i);
            s = _mm_xor_si128(_mm_xor_si128(a, key(K0)), lenv);
            s = _mm_aesenc_si128(s, key(K1));
            s = _mm_aesenc_si128(_mm_xor_si128(s, b), key(K2));
            s = _mm_aesenc_si128(s, key(K3));
            s = _mm_aesenc_si128(s, key(K4));
        } else if len <= 48 {
            let a = _mm_loadu_si128(p as *const __m128i);
            let b = _mm_loadu_si128(p.add(16) as *const __m128i);
            let c = _mm_loadu_si128(p.add(len - 16) as *const __m128i);
            s = _mm_xor_si128(_mm_xor_si128(a, key(K0)), lenv);
            s = _mm_aesenc_si128(s, key(K1));
            s = _mm_aesenc_si128(_mm_xor_si128(s, b), key(K2));
            s = _mm_aesenc_si128(_mm_xor_si128(s, c), key(K3));
            s = _mm_aesenc_si128(s, key(K4));
            s = _mm_aesenc_si128(s, key(K1));
        } else {
            s = _mm_xor_si128(key(K0), lenv);
            let mut off = 0;
            while off + 16 <= len {
                let v = _mm_loadu_si128(p.add(off) as *const __m128i);
                s = _mm_aesenc_si128(_mm_xor_si128(s, v), key(K1));
                s = _mm_aesenc_si128(s, key(K2));
                off += 16;
            }
            let rest = len - off;
            if rest > 0 {
                let m = _mm_loadu_si128(MASKS.as_ptr().add(16 - rest) as *const __m128i);
                let v = _mm_and_si128(_mm_loadu_si128(p.add(off) as *const __m128i), m);
                s = _mm_aesenc_si128(_mm_xor_si128(s, v), key(K3));
            }
            s = _mm_aesenc_si128(s, key(K4));
            s = _mm_aesenc_si128(s, key(K1));
            s = _mm_aesenc_si128(s, key(K2));
        }
        (_mm_cvtsi128_si64(s) as u64, _mm_extract_epi64::<1>(s) as u64)
    }

    #[test]
    fn hash_matches_old_code() {
        let mut seed = 0x0123_4567_89ab_cdefu64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        // The input is a prefix of random bytes; the bytes after it vary too,
        // which checks they are still ignored.
        let mut buf = [0u8; 96 + 16];
        let mut n = 0u64;
        for round in 0..4_000_000u32 {
            for w in buf.chunks_mut(8) {
                let r = next().to_le_bytes();
                w.copy_from_slice(&r[..w.len()]);
            }
            if round % 4 == 0 {
                // ID-like bytes: lowercase base 36 and hex characters.
                for b in buf.iter_mut() {
                    *b = b"0123456789abcdefghijklmnopqrstuvwxyz"[(*b % 36) as usize];
                }
            }
            for len in [(round % 81) as usize, 9, 33] {
                let new = unsafe { hash128(buf.as_ptr(), len) };
                let old = unsafe { hash128_old(buf.as_ptr(), len) };
                assert_eq!(new, old, "len {len} bytes {:?}", &buf[..len]);
                n += 1;
            }
        }
        // About 49,000 inputs of each length 0..=80, plus 4 million each of 9 and 33.
        assert!(n >= 12_000_000);
    }
}
