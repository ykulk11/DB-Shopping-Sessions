//! Stage 1 of CSV parsing: find every comma, newline and double quote in a
//! block of whole lines, 64 bytes at a time.

use std::arch::x86_64::*;

const PREFETCH_AHEAD: usize = 4096;

/// Flag bits of a separator.
pub const F_NL: u8 = 1; // the separator is a newline
pub const F_QA: u8 = 2; // the byte after it is a double quote
pub const F_QB: u8 = 4; // the byte before it is a double quote
pub const F_BAD: u8 = 8; // the field it ends holds a quote that wraps nothing

const ONES: u64 = 0x0101_0101_0101_0101;

/// The separators (commas and newlines) of a block, in order, with one flag
/// byte each. Entry 0 stands for a separator just before the block, so the
/// first line is handled like every other; real entries follow, then padding.
/// Positions are byte offsets from the block start.
pub struct ScanBuf {
    pub seps: Vec<u32>,
    pub sf: Vec<u8>,
    pub lines: usize,
}

impl ScanBuf {
    pub fn new() -> Self {
        ScanBuf { seps: Vec::new(), sf: Vec::new(), lines: 0 }
    }
}

/// Marks the separator that ends each field holding a bad quote, among the
/// `popcnt(s)` separators whose flags are at `sf`. A field that runs past the
/// word ends at the first separator after it, whose flag slot is `sf + k`;
/// that slot is remembered in `*pend` and marked once its flag byte has been
/// written (at a later call, or by `scan` at the end), because the flag store
/// of the next word overwrites it.
#[cold]
#[inline(never)]
unsafe fn mark_bad(sf: *mut u8, s: u64, mut badq: u64, pend: *mut *mut u8) {
    let end = sf.add(s.count_ones() as usize);
    let pp = *pend;
    if !pp.is_null() && pp < end {
        *pp |= F_BAD;
        *pend = std::ptr::null_mut();
    }
    while badq != 0 {
        let p = badq.trailing_zeros();
        badq &= badq - 1;
        let above = if p == 63 { 0 } else { s & (!0u64 << (p + 1)) };
        if above == 0 {
            *pend = end;
        } else {
            let idx = (s & ((1u64 << above.trailing_zeros()) - 1)).count_ones() as usize;
            *sf.add(idx) |= F_BAD;
        }
    }
}

/// Separator and quote masks of the 64 bytes at `p`, and the newline
/// compare results, turned into a mask only when the word is recorded (this
/// keeps one general register free while the next word is in flight).
#[inline(always)]
unsafe fn masks(p: *const u8, comma: __m256i, newline: __m256i, quote: __m256i) -> (u64, (__m256i, __m256i), u64) {
    let v0 = _mm256_loadu_si256(p as *const __m256i);
    let v1 = _mm256_loadu_si256(p.add(32) as *const __m256i);
    let n0 = _mm256_cmpeq_epi8(v0, newline);
    let n1 = _mm256_cmpeq_epi8(v1, newline);
    let s = (_mm256_movemask_epi8(_mm256_or_si256(_mm256_cmpeq_epi8(v0, comma), n0)) as u32 as u64)
        | ((_mm256_movemask_epi8(_mm256_or_si256(_mm256_cmpeq_epi8(v1, comma), n1)) as u32 as u64) << 32);
    let qm = (_mm256_movemask_epi8(_mm256_cmpeq_epi8(v0, quote)) as u32 as u64)
        | ((_mm256_movemask_epi8(_mm256_cmpeq_epi8(v1, quote)) as u32 as u64) << 32);
    (s, (n0, n1), qm)
}

/// Newline mask from the compare results of `masks`.
#[inline(always)]
unsafe fn nmask(n: (__m256i, __m256i)) -> u64 {
    (_mm256_movemask_epi8(n.0) as u32 as u64) | ((_mm256_movemask_epi8(n.1) as u32 as u64) << 32)
}

/// Writer state of `scan`.
struct Out {
    /// Next position slot and next flag slot.
    dp: *mut u32,
    df: *mut u8,
    /// Separator and quote masks of the previous word (only bit 63 is used).
    ps: u64,
    pq: u64,
}

/// Records the separators of one word. `s`, `nm`, `qm`: separator, newline
/// and quote masks of the word; `xs`, `xq`: bit 0 says whether the byte
/// after the word is a separator, or a quote (other bits are ignored);
/// `base`: the word's offset in all eight lanes.
#[inline(always)]
unsafe fn word(o: &mut Out, s: u64, nm: u64, qm: u64, xs: u64, xq: u64, base: __m256i, pend: *mut *mut u8) {
    let after_sep = (s << 1) | (o.ps >> 63);
    let before_sep = (s >> 1) | (xs << 63);
    // Flag sources; pext with mask `s` looks only at separator bits.
    let qa = (qm >> 1) | (xq << 63);
    let qb = (qm << 1) | (o.pq >> 63);
    o.ps = s;
    o.pq = qm;

    // Positions: eight slots are written whether or not they are all used.
    let k = s.count_ones() as usize;
    let mut t = s;
    let mut p = [0u32; 8];
    for j in 0..8 {
        p[j] = t.trailing_zeros();
        t &= t.wrapping_sub(1);
    }
    let pv = _mm256_loadu_si256(p.as_ptr() as *const __m256i);
    _mm256_storeu_si256(o.dp as *mut __m256i, _mm256_add_epi32(pv, base));
    // Flags, one byte per separator, in the same order.
    let nl_bits = _pext_u64(nm, s);
    let qa_bits = _pext_u64(qa, s);
    let qb_bits = _pext_u64(qb, s);
    // One mask constant for all three; the bit planes do not overlap, so
    // the sum is their union.
    let fl = _pdep_u64(nl_bits, ONES) + _pdep_u64(qa_bits, ONES) * 2 + _pdep_u64(qb_bits, ONES) * 4;
    (o.df as *mut u64).write_unaligned(fl);
    if k > 8 {
        let b = _mm256_extract_epi32::<0>(base) as u32;
        let mut j = 8;
        while t != 0 {
            *o.dp.add(j) = b + t.trailing_zeros();
            t &= t - 1;
            *o.df.add(j) = ((nl_bits >> j) & 1) as u8
                | (((qa_bits >> j) & 1) << 1) as u8
                | (((qb_bits >> j) & 1) << 2) as u8;
            j += 1;
        }
    }
    // A quote is fine only right after a separator or right before one,
    // and not both (that field would be a single quote).
    let badq = qm & !(after_sep ^ before_sep);
    if badq != 0 {
        mark_bad(o.df, s, badq, pend);
    }
    o.dp = o.dp.add(k);
    o.df = o.df.add(k);
}

/// Number of flag bytes in `[p, e)` with F_NL set.
#[inline(always)]
unsafe fn count_nl(mut p: *const u8, e: *const u8) -> usize {
    let bit = _mm256_set1_epi8(F_NL as i8);
    let mut acc = _mm256_setzero_si256();
    while p.add(32) <= e {
        let v = _mm256_and_si256(_mm256_loadu_si256(p as *const __m256i), bit);
        acc = _mm256_add_epi64(acc, _mm256_sad_epu8(v, _mm256_setzero_si256()));
        p = p.add(32);
    }
    let h = _mm_add_epi64(_mm256_castsi256_si128(acc), _mm256_extracti128_si256::<1>(acc));
    let mut n = (_mm_cvtsi128_si64(h) + _mm_extract_epi64::<1>(h)) as usize;
    while p < e {
        n += (*p & F_NL) as usize;
        p = p.add(1);
    }
    n
}

/// Scans `len` bytes at `text`. The block must end right after a newline.
/// Reads up to 64 bytes past the end.
#[inline(never)]
pub unsafe fn scan(text: *const u8, len: usize, sb: &mut ScanBuf) {
    if sb.seps.capacity() < len + 160 {
        sb.seps = Vec::with_capacity(len + 160 + len / 4);
        sb.sf = Vec::with_capacity(len + 160 + len / 4);
    }
    let seps = sb.seps.as_mut_ptr();
    let sf = sb.sf.as_mut_ptr();
    *seps = u32::MAX;
    *sf = F_NL | if *text == b'"' { F_QA } else { 0 };
    // The byte before the block counts as a separator.
    let mut o = Out { dp: seps.add(1), df: sf.add(1), ps: 1 << 63, pq: 0 };
    // Flag slot of a separator still to be marked F_BAD (see mark_bad).
    let mut pend: *mut u8 = std::ptr::null_mut();
    let pd: *mut *mut u8 = &mut pend;
    let comma = _mm256_set1_epi8(b',' as i8);
    let newline = _mm256_set1_epi8(b'\n' as i8);
    let quote = _mm256_set1_epi8(b'"' as i8);
    let step = _mm256_set1_epi32(64);
    let mut base = _mm256_setzero_si256();
    if len != 0 {
        let mut p = text;
        let (mut s, mut nm, mut qm) = masks(p, comma, newline, quote);
        // Whole words. The masks of the following word (which reach at most
        // 63 bytes past the block) tell what the byte after this one is.
        let whole = text.add(len & !63);
        // Two words per iteration: the loop overhead is shared and the
        // masks alternate between two register sets instead of moving.
        let pairs = text.add(len & !127);
        while p < pairs {
            _mm_prefetch::<_MM_HINT_T0>(p.add(PREFETCH_AHEAD) as *const i8);
            let (s2, nm2, qm2) = masks(p.add(64), comma, newline, quote);
            word(&mut o, s, nmask(nm), qm, s2, qm2, base, pd);
            base = _mm256_add_epi32(base, step);
            _mm_prefetch::<_MM_HINT_T0>(p.add(PREFETCH_AHEAD + 64) as *const i8);
            let (s3, nm3, qm3) = masks(p.add(128), comma, newline, quote);
            word(&mut o, s2, nmask(nm2), qm2, s3, qm3, base, pd);
            base = _mm256_add_epi32(base, step);
            s = s3;
            nm = nm3;
            qm = qm3;
            p = p.add(128);
        }
        if p < whole {
            // Ask for the text a little ahead: across a page boundary the
            // hardware prefetcher starts over, and this covers the gap.
            _mm_prefetch::<_MM_HINT_T0>(p.add(PREFETCH_AHEAD) as *const i8);
            let (s2, nm2, qm2) = masks(p.add(64), comma, newline, quote);
            word(&mut o, s, nmask(nm), qm, s2, qm2, base, pd);
            s = s2;
            nm = nm2;
            qm = qm2;
            base = _mm256_add_epi32(base, step);
            p = p.add(64);
        }
        let rem = len & 63;
        if rem != 0 {
            // The last, partial word. The byte after it is never looked at:
            // the block's last byte is a newline, not a quote.
            let keep = (1u64 << rem) - 1;
            let nb = *p.add(64);
            let xs = ((nb == b',') | (nb == b'\n')) as u64;
            let xq = (nb == b'"') as u64;
            word(&mut o, s & keep, nmask(nm) & keep, qm & keep, xs, xq, base, pd);
        }
    }
    let ns = o.dp.offset_from(seps) as usize;
    // A pending mark can only refer to a separator inside the block (the
    // block ends with a newline); the bound keeps the old behaviour anyway.
    if !pend.is_null() && pend < sf.add(ns) {
        *pend |= F_BAD;
    }
    sb.lines = count_nl(sf.add(1), sf.add(ns));
    for j in 0..16 {
        *seps.add(ns + j) = u32::MAX;
        *sf.add(ns + j) = F_NL;
    }
}

/// Offset of the first `\n` at or after `from`, or `len` when there is none.
#[inline]
pub unsafe fn find_newline(text: *const u8, from: usize, len: usize) -> usize {
    let newline = _mm256_set1_epi8(b'\n' as i8);
    let mut i = from;
    while i < len {
        let v = _mm256_loadu_si256(text.add(i) as *const __m256i);
        let m = _mm256_movemask_epi8(_mm256_cmpeq_epi8(v, newline)) as u32;
        if m != 0 {
            let at = i + m.trailing_zeros() as usize;
            return at.min(len);
        }
        i += 32;
    }
    len
}

/// Offset just after the last `\n` in `[from, to)`, or `None`.
#[inline]
pub unsafe fn last_line_end(text: *const u8, from: usize, to: usize) -> Option<usize> {
    let mut i = to;
    while i > from {
        i -= 1;
        if *text.add(i) == b'\n' {
            return Some(i + 1);
        }
    }
    None
}
