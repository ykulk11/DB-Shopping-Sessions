//! Exact timestamp and number parsing per rules.md.

use std::arch::x86_64::*;

pub const BAD: i64 = i64::MIN;

pub const DAY_MS: i64 = 86_400_000;

/// Two ASCII digits as a number, or a value over 99 when either is not a digit.
#[inline(always)]
unsafe fn two(p: *const u8) -> u32 {
    let a = (*p).wrapping_sub(b'0') as u32;
    let b = (*p.add(1)).wrapping_sub(b'0') as u32;
    if a > 9 || b > 9 {
        1000
    } else {
        a * 10 + b
    }
}

static DIM: [[u8; 16]; 2] = [
    [0, 31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31, 0, 0, 0],
    [0, 31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31, 0, 0, 0],
];
static CUM: [[u16; 16]; 2] = [
    [0, 0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334, 0, 0, 0],
    [0, 0, 31, 60, 91, 121, 152, 182, 213, 244, 274, 305, 335, 0, 0, 0],
];

/// Four ASCII digits as a number, or `u32::MAX`.
#[inline(always)]
unsafe fn year4(p: *const u8) -> u32 {
    let x = (p as *const u32).read_unaligned() ^ 0x3030_3030;
    if (x.wrapping_add(0x7676_7676) | x) & 0x8080_8080 != 0 {
        return u32::MAX;
    }
    let x = (x.wrapping_mul(10).wrapping_add(x >> 8)) & 0x00ff_00ff;
    (x.wrapping_mul(100).wrapping_add(x >> 16)) & 0xffff
}

/// `YYYY-MM-DDTHH:MM:SS` (19 bytes) as seconds since the epoch, or `BAD`.
/// Rejects anything that is not a real calendar date and time of day.
#[inline(always)]
pub unsafe fn datetime19(p: *const u8) -> i64 {
    let y = year4(p);
    // Bytes 3..19: the last year digit through the seconds.
    let v = _mm_loadu_si128(p.add(3) as *const __m128i);
    let pat = _mm_setr_epi8(
        b'0' as i8, b'-' as i8, b'0' as i8, b'0' as i8, b'-' as i8, b'0' as i8, b'0' as i8, b'T' as i8,
        b'0' as i8, b'0' as i8, b':' as i8, b'0' as i8, b'0' as i8, b':' as i8, b'0' as i8, b'0' as i8,
    );
    let lim = _mm_setr_epi8(9, 0, 9, 9, 0, 9, 9, 0, 9, 9, 0, 9, 9, 0, 9, 9);
    // A digit becomes its value, a matching separator 0, anything else exceeds the limit.
    let t = _mm_xor_si128(v, pat);
    let ok = _mm_movemask_epi8(_mm_cmpeq_epi8(_mm_max_epu8(t, lim), lim));
    let dg = _mm_shuffle_epi8(t, _mm_setr_epi8(2, 3, 5, 6, 8, 9, 11, 12, 14, 15, -1, -1, -1, -1, -1, -1));
    // 16-bit lanes: month, day, hour, minute, second.
    let x = _mm_maddubs_epi16(dg, _mm_setr_epi8(10, 1, 10, 1, 10, 1, 10, 1, 10, 1, 0, 0, 0, 0, 0, 0));
    let hi = _mm_setr_epi16(12, 31, 23, 59, 59, 0, 0, 0);
    let lo = _mm_setr_epi16(1, 1, 0, 0, 0, 0, 0, 0);
    let out = _mm_movemask_epi8(_mm_or_si128(_mm_cmpgt_epi16(x, hi), _mm_cmpgt_epi16(lo, x)));
    if ok != 0xffff || out != 0 || y == u32::MAX {
        return BAD;
    }
    let sod = _mm_madd_epi16(x, _mm_setr_epi16(0, 0, 3600, 60, 1, 0, 0, 0));
    let sec = (_mm_extract_epi32::<1>(sod) + _mm_extract_epi32::<2>(sod)) as i64;
    let mo = (_mm_extract_epi16::<0>(x) & 15) as usize;
    let d = _mm_extract_epi16::<1>(x) as u32;
    let leap = ((y & 3 == 0) && (y % 25 != 0 || y & 15 == 0)) as usize;
    if d > *DIM.get_unchecked(leap).get_unchecked(mo) as u32 {
        return BAD;
    }
    // Days from 0000-01-01 to the start of year y, then to 1970-01-01.
    let days = (365 * y + (y + 3) / 4 - (y + 99) / 100 + (y + 399) / 400) as i64 - 719_528
        + *CUM.get_unchecked(leap).get_unchecked(mo) as i64
        + d as i64
        - 1;
    days * 86_400 + sec
}

/// Remembers the dates seen recently, so `YYYY-MM-DD` is validated and
/// converted once per distinct date instead of once per row. Indexed by the
/// day-of-month digits: a window of a few weeks never has two dates sharing a slot.
pub struct DateCache {
    a: [u64; 64],
    b: [u32; 64],
    base: [i64; 64],
}

impl DateCache {
    pub fn new() -> Self {
        DateCache { a: [0; 64], b: [0; 64], base: [0; 64] }
    }
}

/// `YYYY-MM-DDT` (11 bytes) as seconds since the epoch at that day's midnight, or `BAD`.
#[cold]
#[inline(never)]
unsafe fn date_slow(p: *const u8) -> i64 {
    if *p.add(4) != b'-' || *p.add(7) != b'-' || *p.add(10) != b'T' {
        return BAD;
    }
    let y = year4(p);
    let mo = two(p.add(5));
    let d = two(p.add(8));
    if y == u32::MAX || mo < 1 || mo > 12 || d < 1 {
        return BAD;
    }
    let leap = ((y & 3 == 0) && (y % 25 != 0 || y & 15 == 0)) as usize;
    if d > DIM[leap][mo as usize] as u32 {
        return BAD;
    }
    let days = (365 * y + (y + 3) / 4 - (y + 99) / 100 + (y + 399) / 400) as i64 - 719_528
        + CUM[leap][mo as usize] as i64
        + d as i64
        - 1;
    days * 86_400
}

/// Same result as `datetime19`, using the date cache and one 8-byte load for the time.
#[inline(always)]
pub unsafe fn datetime19c(p: *const u8, dc: &mut DateCache) -> i64 {
    match datetime19o(p, dc) {
        Some(s) => s,
        None => BAD,
    }
}

/// `datetime19c` as an `Option`, so callers that scale or offset the result
/// branch straight to their own failure path instead of re-testing for `BAD`.
#[inline(always)]
unsafe fn datetime19o(p: *const u8, dc: &mut DateCache) -> Option<i64> {
    let a = (p as *const u64).read_unaligned();
    let b = (p.add(7) as *const u32).read_unaligned();
    // The day-of-month digits' low bits (tens: 2 bits, ones: 4 bits) as a 6-bit slot.
    let slot = _pext_u32(b, 0x000f_0300) as usize;
    let base = if *dc.a.get_unchecked(slot) == a && *dc.b.get_unchecked(slot) == b {
        *dc.base.get_unchecked(slot)
    } else {
        let base = date_slow(p);
        if base == BAD {
            return None;
        }
        *dc.a.get_unchecked_mut(slot) = a;
        *dc.b.get_unchecked_mut(slot) = b;
        *dc.base.get_unchecked_mut(slot) = base;
        base
    };
    Some(base + time_of_day(p.add(11))?)
}

/// `HH:MM:SS` (8 bytes) as seconds since midnight, or `None` unless every
/// digit is an ASCII digit, both colons are present, and the values are in
/// range. The low half of `tod_pair`: one 64-bit lane validates and converts.
#[inline(always)]
unsafe fn time_of_day(p: *const u8) -> Option<i64> {
    let v = _mm_loadl_epi64(p as *const __m128i);
    // Digits become their values, the colons zero; anything else exceeds the limit.
    // The upper half stays zero, so it passes every check and adds nothing.
    let t = _mm_xor_si128(v, _mm_set_epi64x(0, 0x3030_3a30_303a_3030));
    let over = _mm_subs_epu8(t, _mm_set1_epi64x(0x0909_0009_0900_0909));
    let dg = _mm_shuffle_epi8(t, _mm_setr_epi8(0, 1, 3, 4, 6, 7, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1));
    // 16-bit lanes: hour, minute, second, then zeros.
    let x = _mm_maddubs_epi16(dg, _mm_set1_epi16(0x010a));
    let range = _mm_cmpgt_epi16(x, _mm_setr_epi16(23, 59, 59, 0, 0, 0, 0, 0));
    let bad = _mm_or_si128(over, range);
    if _mm_testz_si128(bad, bad) == 0 {
        return None;
    }
    // 32-bit lanes: h * 3600 + m * 60, s; then their sum.
    let s = _mm_madd_epi16(x, _mm_setr_epi16(3600, 60, 1, 0, 0, 0, 0, 0));
    let s = _mm_add_epi32(s, _mm_srli_epi64::<32>(s));
    Some(_mm_cvtsi128_si32(s) as i64)
}

/// `HH:MM` (5 bytes, hours 0..=23, minutes 0..=59) as seconds, or `None`.
/// Reads 8 bytes; the 3 after the field are ignored.
#[inline(always)]
unsafe fn offset(p: *const u8) -> Option<i64> {
    let x = ((p as *const u64).read_unaligned() ^ 0x30_303a_3030) & 0xff_ffff_ffff;
    // Byte 0 = hours, byte 3 = minutes; see `time_of_day`.
    let y = x.wrapping_mul(10).wrapping_add(x >> 8);
    let z = y & 0xff00_00ff;
    let bad = (x.wrapping_add(0x76_767f_7676) | x | z.wrapping_add(0x4400_0068)) & 0x80_8080_8080;
    if bad != 0 {
        return None;
    }
    // z * (60 + 3600 << 24): bits 24..41 hold h * 3600 + m * 60, h * 60 stays below bit 11.
    Some(((z.wrapping_mul(60 | (3600 << 24)) >> 24) & 0x3_ffff) as i64)
}

/// Form F1 only, as seconds since the epoch, or `BAD`.
#[inline(always)]
pub unsafe fn parse_f1_seconds(p: *const u8, len: usize, dc: &mut DateCache) -> i64 {
    if len != 20 || *p.add(19) != b'Z' {
        return BAD;
    }
    datetime19c(p, dc)
}

/// Thirteen ASCII digits as a number, or `None` unless all thirteen are digits.
/// Reads 16 bytes; the 3 after the field are ignored. One 128-bit pass: the
/// digits go to lanes 3..16 behind three zeros, then pairs, quads and octets.
#[inline(always)]
unsafe fn digits13(p: *const u8) -> Option<i64> {
    let t = _mm_sub_epi8(_mm_loadu_si128(p as *const __m128i), _mm_set1_epi8(b'0' as i8));
    let d = _mm_shuffle_epi8(t, _mm_setr_epi8(-1, -1, -1, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12));
    // A digit is 0..=9 after the subtraction; anything else (wrapped or not) exceeds 9.
    let over = _mm_subs_epu8(d, _mm_set1_epi8(9));
    if _mm_testz_si128(over, over) == 0 {
        return None;
    }
    // 16-bit lanes: two digits each; 32-bit lanes: four digits each.
    let x = _mm_maddubs_epi16(d, _mm_set1_epi16(0x010a));
    let x = _mm_madd_epi16(x, _mm_set1_epi32(0x0001_0064));
    Some(digits13_finish(x))
}

/// The end of `digits13`: four 32-bit lanes of 4-digit values (each at most
/// 9999) to the 13-digit number. The low 16 bits of each lane are the whole
/// value, so one byte shuffle packs them (no saturation is ever needed).
#[inline(always)]
unsafe fn digits13_finish(x: __m128i) -> i64 {
    let x = _mm_shuffle_epi8(x, _mm_setr_epi8(0, 1, 4, 5, 8, 9, 12, 13, -1, -1, -1, -1, -1, -1, -1, -1));
    // Two 8-digit values.
    let x = _mm_madd_epi16(x, _mm_set1_epi32(0x0001_2710));
    let v = _mm_cvtsi128_si64(x) as u64;
    ((v & 0xffff_ffff) * 100_000_000 + (v >> 32)) as i64
}

/// Any of the three forms, as milliseconds since the epoch, or `BAD`. A
/// digits-only value too large to be inside any window is also `BAD`.
#[inline(always)]
pub unsafe fn parse_ts(p: *const u8, len: usize, dc: &mut DateCache) -> i64 {
    if len == 20 {
        if *p.add(19) == b'Z' {
            return match datetime19o(p, dc) {
                Some(s) => s * 1000,
                None => BAD,
            };
        }
    } else if len == 13 {
        // The usual F3: thirteen digits.
        return match digits13(p) {
            Some(t) => t,
            None => BAD,
        };
    } else if len == 25 {
        let sign = *p.add(19);
        // '+' (0x2b) or '-' (0x2d) in one test.
        if sign.wrapping_sub(b'+') & 0xfd == 0 {
            let off = match offset(p.add(20)) {
                Some(off) => off,
                None => return BAD,
            };
            let s = match datetime19o(p, dc) {
                Some(s) => s,
                None => return BAD,
            };
            // '+' subtracts the offset, '-' adds it; a mask instead of a branch on the sign.
            let neg = ((sign == b'+') as i64).wrapping_neg();
            return (s + ((off ^ neg) - neg)) * 1000;
        }
    }
    parse_digits(p, len, 17)
}

/// The cached `YYYY-MM-DDT` part of `datetime19o`: seconds at that day's midnight.
#[inline(always)]
unsafe fn date_base(p: *const u8, dc: &mut DateCache) -> Option<i64> {
    let a = (p as *const u64).read_unaligned();
    let b = (p.add(7) as *const u32).read_unaligned();
    let slot = _pext_u32(b, 0x000f_0300) as usize;
    if *dc.a.get_unchecked(slot) == a && *dc.b.get_unchecked(slot) == b {
        Some(*dc.base.get_unchecked(slot))
    } else {
        let base = date_slow(p);
        if base == BAD {
            return None;
        }
        *dc.a.get_unchecked_mut(slot) = a;
        *dc.b.get_unchecked_mut(slot) = b;
        *dc.base.get_unchecked_mut(slot) = base;
        Some(base)
    }
}

/// `date_base(q)` when `p`'s date was just looked up: the same 11 bytes give
/// the same day without a second lookup (ingest_ts usually shares ts's date).
#[inline(always)]
unsafe fn date_base_after(q: *const u8, p: *const u8, pbase: i64, dc: &mut DateCache) -> Option<i64> {
    let x = (q as *const u64).read_unaligned() ^ (p as *const u64).read_unaligned();
    let y = (q.add(7) as *const u32).read_unaligned() ^ (p.add(7) as *const u32).read_unaligned();
    if x | y as u64 == 0 {
        return Some(pbase);
    }
    date_base(q, dc)
}

/// Two `HH:MM:SS` fields at once: the same result as `time_of_day` on each,
/// but `None` unless both are valid. One 128-bit pass validates and converts.
#[inline(always)]
unsafe fn tod_pair(p: *const u8, q: *const u8) -> Option<(i64, i64)> {
    let v = _mm_castpd_si128(_mm_loadh_pd(_mm_castsi128_pd(_mm_loadl_epi64(p as *const __m128i)), q as *const f64));
    // Digits become their values, the colons zero; anything else exceeds the limit.
    let t = _mm_xor_si128(v, _mm_set1_epi64x(0x3030_3a30_303a_3030));
    let over = _mm_subs_epu8(t, _mm_set1_epi64x(0x0909_0009_0900_0909));
    let dg = _mm_shuffle_epi8(t, _mm_setr_epi8(0, 1, 3, 4, 6, 7, -1, -1, 8, 9, 11, 12, 14, 15, -1, -1));
    // 16-bit lanes: hour, minute, second, 0 for each field.
    let x = _mm_maddubs_epi16(dg, _mm_set1_epi16(0x010a));
    let range = _mm_cmpgt_epi16(x, _mm_setr_epi16(23, 59, 59, 0, 23, 59, 59, 0));
    let bad = _mm_or_si128(over, range);
    if _mm_testz_si128(bad, bad) == 0 {
        return None;
    }
    // 32-bit lanes: h * 3600 + m * 60, s, for each field; then add the pairs.
    let s = _mm_madd_epi16(x, _mm_setr_epi16(3600, 60, 1, 0, 3600, 60, 1, 0));
    let s = _mm_add_epi32(s, _mm_srli_epi64::<32>(s));
    Some((_mm_cvtsi128_si32(s) as i64, _mm_extract_epi32::<2>(s) as i64))
}

/// `(parse_ts(tp, tl), parse_f1_seconds(ip, il))` for an event line, with both
/// times of day handled in one pass when ts is a datetime form.
#[inline(always)]
pub unsafe fn parse_event_times(tp: *const u8, tl: usize, ip: *const u8, il: usize, dc: &mut DateCache) -> (i64, i64) {
    let ing_shape = il == 20 && *ip.add(19) == b'Z';
    if tl == 20 {
        if ing_shape && *tp.add(19) == b'Z' {
            if let Some((tt, ti)) = tod_pair(tp.add(11), ip.add(11)) {
                if let Some(bt) = date_base(tp, dc) {
                    if let Some(bi) = date_base_after(ip, tp, bt, dc) {
                        return ((bt + tt) * 1000, bi + ti);
                    }
                }
            }
        }
    } else if tl == 13 {
        if ing_shape {
            if let Some(tt) = digits13(tp) {
                if let Some(ti) = datetime19o(ip, dc) {
                    return (tt, ti);
                }
            }
        }
    } else if tl == 25 {
        let sign = *tp.add(19);
        if ing_shape && sign.wrapping_sub(b'+') & 0xfd == 0 {
            if let (Some((tt, ti)), Some(off)) = (tod_pair(tp.add(11), ip.add(11)), offset(tp.add(20))) {
                if let (Some(bt), Some(bi)) = (date_base(tp, dc), date_base(ip, dc)) {
                    let neg = ((sign == b'+') as i64).wrapping_neg();
                    return ((bt + tt + ((off ^ neg) - neg)) * 1000, bi + ti);
                }
            }
        }
    }
    (parse_ts(tp, tl, dc), parse_f1_seconds(ip, il, dc))
}

/// Digits only (leading zeros allowed) with at most `max_digits` significant
/// digits, as a number; `BAD` otherwise, including the empty string.
#[inline(never)]
pub unsafe fn parse_digits(p: *const u8, len: usize, max_digits: usize) -> i64 {
    if len == 0 {
        return BAD;
    }
    let mut i = 0;
    while i < len && *p.add(i) == b'0' {
        i += 1;
    }
    if len - i > max_digits {
        return BAD;
    }
    let mut v: i64 = 0;
    while i < len {
        let d = (*p.add(i)).wrapping_sub(b'0');
        if d > 9 {
            return BAD;
        }
        v = v * 10 + d as i64;
        i += 1;
    }
    v
}

/// A purchase price: digits only, 1 to 10,000,000. Zero means invalid.
#[inline(always)]
pub unsafe fn parse_price(p: *const u8, len: usize) -> u32 {
    let v = parse_digits(p, len, 8);
    if v < 1 || v > 10_000_000 {
        0
    } else {
        v as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
        let year = if month <= 2 { year - 1 } else { year };
        let era = year.div_euclid(400);
        let year_of_era = year - era * 400;
        let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
        era * 146_097 + year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year - 719_468
    }

    /// Plain scalar parse of the same 19 bytes, written independently.
    fn reference(b: &[u8]) -> i64 {
        let digit = |i: usize| -> Option<i64> { if b[i].is_ascii_digit() { Some((b[i] - b'0') as i64) } else { None } };
        let num = |r: std::ops::Range<usize>| -> Option<i64> {
            let mut v = 0;
            for i in r {
                v = v * 10 + digit(i)?;
            }
            Some(v)
        };
        if b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[13] != b':' || b[16] != b':' {
            return BAD;
        }
        let (Some(y), Some(mo), Some(d), Some(h), Some(mi), Some(s)) =
            (num(0..4), num(5..7), num(8..10), num(11..13), num(14..16), num(17..19))
        else {
            return BAD;
        };
        if !(1..=12).contains(&mo) || h > 23 || mi > 59 || s > 59 || d < 1 {
            return BAD;
        }
        let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
        let dim = [31, if leap { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31][mo as usize - 1];
        if d > dim {
            return BAD;
        }
        days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60 + s
    }

    fn padded(s: &[u8]) -> Vec<u8> {
        let mut v = vec![0u8; 32];
        v.extend_from_slice(s);
        v.extend_from_slice(&[0u8; 64]);
        v
    }

    #[test]
    fn datetime_matches_reference() {
        let mut seed = 0x1234_5678_9abc_def0u64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let alphabet = b"0123456789-T:Z tz+./";
        let mut good = 0;
        let mut dc = DateCache::new();
        for round in 0..3_000_000u32 {
            let mut s = format!(
                "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
                match next() % 6 { 0 => next() % 10000, 1 => 2028, 2 => 2100, 3 => 2000, _ => 2025 + next() % 4 },
                next() % 14,
                next() % 33,
                next() % 26,
                next() % 62,
                next() % 62
            )
            .into_bytes();
            if round % 3 == 0 {
                let at = (next() % 19) as usize;
                s[at] = alphabet[(next() % alphabet.len() as u64) as usize];
            }
            if round % 50 == 0 {
                s[(next() % 19) as usize] = (next() % 256) as u8;
            }
            let buf = padded(&s);
            let got = unsafe { datetime19(buf.as_ptr().add(32)) };
            let want = reference(&s);
            assert_eq!(got, want, "{:?}", String::from_utf8_lossy(&s));
            let cached = unsafe { datetime19c(buf.as_ptr().add(32), &mut dc) };
            assert_eq!(cached, want, "cached {:?}", String::from_utf8_lossy(&s));
            if got != BAD {
                good += 1;
            }
        }
        assert!(good > 100_000);
    }

    #[test]
    fn timestamp_forms() {
        let mut dc = DateCache::new();
        let mut ts = |s: &str| {
            let buf = padded(s.as_bytes());
            unsafe { parse_ts(buf.as_ptr().add(32), s.len(), &mut dc) }
        };
        assert_eq!(ts("2026-10-03T23:58:10Z"), 1_791_071_890_000);
        assert_eq!(ts("2026-10-03T19:58:10-04:00"), 1_791_071_890_000);
        assert_eq!(ts("1791071890000"), 1_791_071_890_000);
        assert_eq!(ts("0001791071890000"), 1_791_071_890_000);
        assert_eq!(ts("1791071890123"), 1_791_071_890_123);
        assert_eq!(ts("86400000"), 86_400_000);
        assert_eq!(ts("1970-01-01T00:00:00Z"), 0);
        assert_eq!(ts("2028-02-29T12:00:00Z"), 1_835_438_400_000);
        assert_eq!(ts("2026-10-04T01:30:00+02:00"), ts("2026-10-03T23:30:00Z"));
        assert_eq!(ts("2026-10-03T23:58:10-00:00"), 1_791_071_890_000);
        assert_eq!(ts("2026-10-03T23:58:10+23:59"), 1_791_071_890_000 - (23 * 3600 + 59 * 60) * 1000);
        for bad in [
            "", "2026-10-03T23:58:10.000Z", "2026-10-03T23:58:10z", "2026-10-03t23:58:10Z",
            "2026-10-03 23:58:10Z", "2026-10-03T23:58:10", "1791071890000.0", "1.79e12", "-5", "+5",
            "2026-13-03T23:58:10Z", "2026-02-30T23:58:10Z", "2027-02-29T23:58:10Z", "2026-10-03T24:58:10Z",
            "2026-10-03T23:58:60Z", "2026-10-03T23:60:10Z", "2026-10-03T23:58:10+24:00",
            "2026-10-03T23:58:10+05:60", "2026-10-03T23:58:10+0200", "2026-00-03T23:58:10Z",
            "2026-10-00T23:58:10Z", "2026-10-32T23:58:10Z", "2026-04-31T23:58:10Z", "179107189000a",
            "17910718900 0", " 1791071890000", "999999999999999999999999999999", "2100-02-29T00:00:00Z",
            "2026-10-03T23:58:10+02:0", "2026-10-03T23:58:10 02:00", "20261003T235810Z",
        ] {
            assert_eq!(ts(bad), BAD, "{bad:?}");
        }
        assert_ne!(ts("2000-02-29T00:00:00Z"), BAD);
        assert_ne!(ts("2028-02-29T00:00:00Z"), BAD);
    }

    /// Scalar `HH:MM:SS` / `HH:MM` references for the SWAR fast paths.
    fn tod_reference(b: &[u8]) -> Option<i64> {
        let d = |i: usize| if b[i].is_ascii_digit() { Some((b[i] - b'0') as i64) } else { None };
        if b[2] != b':' || b[5] != b':' {
            return None;
        }
        let (h, m, s) = (d(0)? * 10 + d(1)?, d(3)? * 10 + d(4)?, d(6)? * 10 + d(7)?);
        if h > 23 || m > 59 || s > 59 {
            return None;
        }
        Some(h * 3600 + m * 60 + s)
    }

    fn offset_reference(b: &[u8]) -> Option<i64> {
        let d = |i: usize| if b[i].is_ascii_digit() { Some((b[i] - b'0') as i64) } else { None };
        if b[2] != b':' {
            return None;
        }
        let (h, m) = (d(0)? * 10 + d(1)?, d(3)? * 10 + d(4)?);
        if h > 23 || m > 59 {
            return None;
        }
        Some(h * 3600 + m * 60)
    }

    #[test]
    fn time_of_day_matches_reference() {
        // Every two-digit hour, minute and second (0..=99 each).
        for h in 0..100 {
            for m in 0..100 {
                for s in 0..100 {
                    let t = format!("{h:02}:{m:02}:{s:02}");
                    let buf = padded(t.as_bytes());
                    assert_eq!(unsafe { time_of_day(buf.as_ptr().add(32)) }, tod_reference(t.as_bytes()), "{t}");
                }
            }
        }
        // Every offset with two-digit values.
        for h in 0..100 {
            for m in 0..100 {
                let t = format!("{h:02}:{m:02}");
                let buf = padded(t.as_bytes());
                assert_eq!(unsafe { offset(buf.as_ptr().add(32)) }, offset_reference(t.as_bytes()), "{t}");
            }
        }
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        // Every byte value at every position of several bases, with random bytes after the field.
        for base in ["00:00:00", "23:59:59", "19:09:09", "09:50:05", "12:34:56", "20:00:00", "24:60:60"] {
            for at in 0..8 {
                for v in 0..=255u8 {
                    let mut t = base.as_bytes().to_vec();
                    t[at] = v;
                    let mut buf = padded(&t);
                    buf[40] = next() as u8;
                    assert_eq!(unsafe { time_of_day(buf.as_ptr().add(32)) }, tod_reference(&t), "{t:?}");
                    // An offset is the first five bytes; what follows must not matter.
                    for k in 5..8 {
                        buf[32 + k] = next() as u8;
                    }
                    assert_eq!(unsafe { offset(buf.as_ptr().add(32)) }, offset_reference(&t), "offset {t:?}");
                }
            }
        }
        // Random strings over a digit-heavy alphabet, and fully random bytes.
        let alphabet = b"0123456789:/;+- \x00\xff\x8a\x7f\x89\xb0";
        for round in 0..3_000_000u32 {
            let mut t = [0u8; 8];
            for (i, c) in t.iter_mut().enumerate() {
                *c = if round % 7 == 0 {
                    next() as u8
                } else if (i == 2 || i == 5) && next() % 8 != 0 {
                    b':'
                } else {
                    alphabet[(next() % alphabet.len() as u64) as usize]
                };
            }
            let mut buf = padded(&t);
            buf[40] = next() as u8;
            assert_eq!(unsafe { time_of_day(buf.as_ptr().add(32)) }, tod_reference(&t), "{t:?}");
            assert_eq!(unsafe { offset(buf.as_ptr().add(32)) }, offset_reference(&t), "offset {t:?}");
        }
    }

    /// The whole F2 form through `parse_ts`, against the scalar references.
    #[test]
    fn offset_form_matches_reference() {
        let mut seed = 0xdead_beef_cafe_f00du64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let alphabet = b"0123456789-T:Z+ ";
        let mut dc = DateCache::new();
        let mut good = 0;
        for round in 0..2_000_000u32 {
            let mut s = format!(
                "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}{}{:02}:{:02}",
                2025 + next() % 4,
                1 + next() % 12,
                1 + next() % 31,
                next() % 25,
                next() % 61,
                next() % 61,
                if next() % 2 == 0 { '+' } else { '-' },
                next() % 26,
                next() % 62
            )
            .into_bytes();
            if round % 3 == 0 {
                let at = (next() % 25) as usize;
                s[at] = alphabet[(next() % alphabet.len() as u64) as usize];
            }
            if round % 50 == 0 {
                s[(next() % 25) as usize] = next() as u8;
            }
            let mut buf = padded(&s);
            for k in 0..8 {
                buf[32 + 25 + k] = next() as u8;
            }
            let got = unsafe { parse_ts(buf.as_ptr().add(32), 25, &mut dc) };
            // One mutation never makes 25 bytes all digits, so this is F2 or nothing.
            assert!(!s.iter().all(|c| c.is_ascii_digit()));
            let dt = reference(&s[..19]);
            let want = match (s[19], offset_reference(&s[20..25])) {
                (b'+', Some(off)) if dt != BAD => (dt - off) * 1000,
                (b'-', Some(off)) if dt != BAD => (dt + off) * 1000,
                _ => BAD,
            };
            assert_eq!(got, want, "{:?}", String::from_utf8_lossy(&s));
            if got != BAD {
                good += 1;
            }
        }
        assert!(good > 100_000);
    }

    #[test]
    fn digits_match_reference() {
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let mut dc = DateCache::new();
        for round in 0..2_000_000u32 {
            let v = next() % 10_000_000_000_000;
            let mut s = format!("{v:013}").into_bytes();
            if round % 4 == 0 {
                s[(next() % 13) as usize] = b"0123456789 .-+e/:"[(next() % 17) as usize];
            }
            // The bytes after the field are digits or anything else; they must not matter.
            let mut buf = padded(&s);
            for k in 0..3 {
                buf[32 + 13 + k] = if next() % 2 == 0 { b'0' + (next() % 10) as u8 } else { next() as u8 };
            }
            let got = unsafe { parse_ts(buf.as_ptr().add(32), 13, &mut dc) };
            let want = match std::str::from_utf8(&s).ok().filter(|t| t.bytes().all(|c| c.is_ascii_digit())) {
                Some(t) => t.parse::<i64>().unwrap(),
                None => BAD,
            };
            assert_eq!(got, want, "{:?}", String::from_utf8_lossy(&s));
        }
    }

    /// `parse_event_times` against `parse_ts` and `parse_f1_seconds` on random pairs.
    #[test]
    fn event_times_match_separate() {
        let mut seed = 0x0bad_5eed_1234_9876u64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let gen = |next: &mut dyn FnMut() -> u64, date: Option<(u64, u64, u64)>, ing: bool| -> (Vec<u8>, (u64, u64, u64)) {
            let (y, mo, d) = date.unwrap_or_else(|| {
                (
                    match next() % 8 { 0 => next() % 10000, 1 => 2100, 2 => 2000, _ => 2025 + next() % 4 },
                    if next() % 8 == 0 { next() % 14 } else { 1 + next() % 12 },
                    if next() % 8 == 0 { next() % 33 } else { 1 + next() % 31 },
                )
            });
            let tod = |next: &mut dyn FnMut() -> u64| {
                if next() % 6 == 0 {
                    format!("{:02}:{:02}:{:02}", next() % 26, next() % 62, next() % 62)
                } else {
                    format!("{:02}:{:02}:{:02}", next() % 24, next() % 60, next() % 60)
                }
            };
            let dt = format!("{:04}-{:02}-{:02}T{}", y, mo, d, tod(next));
            let form = if ing { if next() % 10 == 0 { next() % 4 } else { 0 } } else { next() % 4 };
            let s = match form {
                0 => format!("{dt}Z"),
                1 => format!("{:013}", next() % 10_000_000_000_000),
                2 => format!(
                    "{dt}{}{:02}:{:02}",
                    if next() % 2 == 0 { '+' } else { '-' },
                    if next() % 6 == 0 { next() % 26 } else { next() % 24 },
                    if next() % 6 == 0 { next() % 62 } else { next() % 60 }
                ),
                _ => {
                    let n = (next() % 30) as usize;
                    (0..n).map(|_| b"0123456789-T:Z+"[(next() % 15) as usize] as char).collect()
                }
            };
            (s.into_bytes(), (y, mo, d))
        };
        let alphabet = b"0123456789-T:Z+ tz./";
        let mutate = |next: &mut dyn FnMut() -> u64, s: &mut Vec<u8>| {
            if s.is_empty() {
                return;
            }
            match next() % 8 {
                0 => {
                    let at = (next() % s.len() as u64) as usize;
                    s[at] = alphabet[(next() % alphabet.len() as u64) as usize];
                }
                1 => {
                    let at = (next() % s.len() as u64) as usize;
                    s[at] = next() as u8;
                }
                2 if next() % 4 == 0 => {
                    if next() % 2 == 0 {
                        s.pop();
                    } else {
                        s.push(b"0Z+ "[(next() % 4) as usize]);
                    }
                }
                _ => {}
            }
        };
        let mut dc1 = DateCache::new();
        let mut dc2 = DateCache::new();
        let mut good = 0;
        for _ in 0..4_000_000u32 {
            let (mut a, date) = gen(&mut next, None, false);
            let same = next() % 2 == 0;
            let (mut b, _) = gen(&mut next, if same { Some(date) } else { None }, true);
            mutate(&mut next, &mut a);
            mutate(&mut next, &mut b);
            let mut buf = vec![0u8; 32];
            buf.extend_from_slice(&a);
            for _ in 0..1 + next() % 4 {
                buf.push(next() as u8);
            }
            let ib = buf.len();
            buf.extend_from_slice(&b);
            for _ in 0..64 {
                buf.push(if next() % 2 == 0 { next() as u8 } else { b'0' + (next() % 10) as u8 });
            }
            let (tp, ip) = unsafe { (buf.as_ptr().add(32), buf.as_ptr().add(ib)) };
            let got = unsafe { parse_event_times(tp, a.len(), ip, b.len(), &mut dc1) };
            let want = unsafe { (parse_ts(tp, a.len(), &mut dc2), parse_f1_seconds(ip, b.len(), &mut dc2)) };
            assert_eq!(got, want, "{:?} {:?}", String::from_utf8_lossy(&a), String::from_utf8_lossy(&b));
            if got.0 != BAD && got.1 != BAD {
                good += 1;
            }
        }
        assert!(good > 500_000, "{good}");
    }

    #[test]
    fn prices() {
        let price = |s: &str| {
            let buf = padded(s.as_bytes());
            unsafe { parse_price(buf.as_ptr().add(32), s.len()) }
        };
        assert_eq!(price("1"), 1);
        assert_eq!(price("0250"), 250);
        assert_eq!(price("10000000"), 10_000_000);
        assert_eq!(price("000000000000000000001"), 1);
        for bad in ["", "0", "00", "10000001", "250000000", "19.99", "-5", "+500", "1e3", "1000\r", " 5"] {
            assert_eq!(price(bad), 0, "{bad:?}");
        }
    }
}
