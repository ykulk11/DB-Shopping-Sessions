//! Exact timestamp and number parsing per rules.md.

pub const BAD: i64 = i64::MIN;

pub const DAY_MS: i64 = 86_400_000;

#[inline(always)]
pub fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    era * 146_097 + year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year - 719_468
}

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

static DAYS_IN_MONTH: [u32; 13] = [0, 31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];

/// `YYYY-MM-DDTHH:MM:SS` (19 bytes) as seconds since the epoch, or `BAD`.
#[inline(always)]
pub unsafe fn datetime19(p: *const u8) -> i64 {
    if *p.add(4) != b'-'
        || *p.add(7) != b'-'
        || *p.add(10) != b'T'
        || *p.add(13) != b':'
        || *p.add(16) != b':'
    {
        return BAD;
    }
    let y1 = two(p);
    let y2 = two(p.add(2));
    let mo = two(p.add(5));
    let d = two(p.add(8));
    let h = two(p.add(11));
    let mi = two(p.add(14));
    let s = two(p.add(17));
    if y1 > 99 || y2 > 99 || mo < 1 || mo > 12 || d < 1 || d > 31 || h > 23 || mi > 59 || s > 59 {
        return BAD;
    }
    let y = y1 * 100 + y2;
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let dim = *DAYS_IN_MONTH.get_unchecked(mo as usize) + (leap && mo == 2) as u32;
    if d > dim {
        return BAD;
    }
    days_from_civil(y as i64, mo as i64, d as i64) * 86_400 + (h * 3600 + mi * 60 + s) as i64
}

/// Form F1 only, as seconds since the epoch, or `BAD`.
#[inline(always)]
pub unsafe fn parse_f1_seconds(p: *const u8, len: usize) -> i64 {
    if len != 20 || *p.add(19) != b'Z' {
        return BAD;
    }
    datetime19(p)
}

/// Any of the three forms, as milliseconds since the epoch, or `BAD`. A
/// digits-only value too large to be inside any window is also `BAD`.
#[inline(always)]
pub unsafe fn parse_ts(p: *const u8, len: usize) -> i64 {
    if len == 20 && *p.add(19) == b'Z' {
        let s = datetime19(p);
        return if s == BAD { BAD } else { s * 1000 };
    }
    if len == 25 {
        let sign = *p.add(19);
        if sign == b'+' || sign == b'-' {
            if *p.add(22) != b':' {
                return BAD;
            }
            let hh = two(p.add(20));
            let mm = two(p.add(23));
            if hh > 23 || mm > 59 {
                return BAD;
            }
            let s = datetime19(p);
            if s == BAD {
                return BAD;
            }
            let off = (hh * 3600 + mm * 60) as i64;
            return (if sign == b'+' { s - off } else { s + off }) * 1000;
        }
    }
    parse_digits(p, len, 17)
}

/// Digits only (leading zeros allowed) with at most `max_digits` significant
/// digits, as a number; `BAD` otherwise, including the empty string.
#[inline(always)]
pub unsafe fn parse_digits(p: *const u8, len: usize, max_digits: usize) -> i64 {
    if len == 0 {
        return BAD;
    }
    let mut i = 0;
    while i < len && *p.add(i) == b'0' {
        i += 1;
    }
    if len - i > max_digits {
        // Still has to be all digits to be well formed, but either way it is unusable.
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
