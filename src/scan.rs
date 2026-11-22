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

/// Marks the separator that ends each field holding a bad quote; `pending`
/// says such a field ran past the previous word. Returns whether one runs
/// past this word.
#[cold]
#[inline(never)]
unsafe fn mark_bad(sf: *mut u8, s: u64, mut badq: u64, pending: bool) -> bool {
    let mut carry = false;
    if pending {
        if s != 0 {
            *sf |= F_BAD;
        } else {
            carry = true;
        }
    }
    while badq != 0 {
        let p = badq.trailing_zeros();
        badq &= badq - 1;
        let above = if p == 63 { 0 } else { s & (!0u64 << (p + 1)) };
        if above == 0 {
            carry = true;
        } else {
            let idx = (s & ((1u64 << above.trailing_zeros()) - 1)).count_ones() as usize;
            *sf.add(idx) |= F_BAD;
        }
    }
    carry
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
    let mut ns = 1usize;
    let mut lines = 0usize;
    // Whether the byte before the current word is a separator, or a quote.
    let mut prev_s = 1u64;
    let mut prev_q = 0u64;
    let mut bad_pending = false;
    let comma = _mm256_set1_epi8(b',' as i8);
    let newline = _mm256_set1_epi8(b'\n' as i8);
    let quote = _mm256_set1_epi8(b'"' as i8);
    let mut i = 0usize;
    while i < len {
        // Ask for the text a little ahead: across a page boundary the
        // hardware prefetcher starts over, and this covers the gap.
        _mm_prefetch::<_MM_HINT_T0>(text.add(i + PREFETCH_AHEAD) as *const i8);
        let v0 = _mm256_loadu_si256(text.add(i) as *const __m256i);
        let v1 = _mm256_loadu_si256(text.add(i + 32) as *const __m256i);
        let mut cm = (_mm256_movemask_epi8(_mm256_cmpeq_epi8(v0, comma)) as u32 as u64)
            | ((_mm256_movemask_epi8(_mm256_cmpeq_epi8(v1, comma)) as u32 as u64) << 32);
        let mut nm = (_mm256_movemask_epi8(_mm256_cmpeq_epi8(v0, newline)) as u32 as u64)
            | ((_mm256_movemask_epi8(_mm256_cmpeq_epi8(v1, newline)) as u32 as u64) << 32);
        let mut qm = (_mm256_movemask_epi8(_mm256_cmpeq_epi8(v0, quote)) as u32 as u64)
            | ((_mm256_movemask_epi8(_mm256_cmpeq_epi8(v1, quote)) as u32 as u64) << 32);
        let rem = len - i;
        if rem < 64 {
            let keep = (1u64 << rem) - 1;
            cm &= keep;
            nm &= keep;
            qm &= keep;
        }
        let s = cm | nm;
        // The byte after this word. Past the block it is never looked at:
        // the block's last byte is a newline, not a quote.
        let nb = *text.add(i + 64);
        let next_s = ((nb == b',') | (nb == b'\n')) as u64;
        let next_q = (nb == b'"') as u64;
        let after_sep = (s << 1) | prev_s;
        let before_sep = (s >> 1) | (next_s << 63);
        let qa = s & ((qm >> 1) | (next_q << 63));
        let qb = s & ((qm << 1) | prev_q);
        prev_s = s >> 63;
        prev_q = qm >> 63;
        let base = i as u32;

        // Positions: eight slots are written whether or not they are all used.
        let k = s.count_ones() as usize;
        let dst = seps.add(ns);
        let mut t = s;
        for j in 0..8 {
            *dst.add(j) = base + t.trailing_zeros();
            t &= t.wrapping_sub(1);
        }
        // Flags, one byte per separator, in the same order.
        let nl_bits = _pext_u64(nm, s);
        let qa_bits = _pext_u64(qa, s);
        let qb_bits = _pext_u64(qb, s);
        let fl = _pdep_u64(nl_bits, ONES)
            | _pdep_u64(qa_bits, ONES << 1)
            | _pdep_u64(qb_bits, ONES << 2);
        (sf.add(ns) as *mut u64).write_unaligned(fl);
        if k > 8 {
            let mut j = 8;
            while t != 0 {
                *dst.add(j) = base + t.trailing_zeros();
                t &= t - 1;
                *sf.add(ns + j) = ((nl_bits >> j) & 1) as u8
                    | (((qa_bits >> j) & 1) << 1) as u8
                    | (((qb_bits >> j) & 1) << 2) as u8;
                j += 1;
            }
        }
        // A quote is fine only right after a separator or right before one,
        // and not both (that field would be a single quote).
        let badq = qm & !(after_sep ^ before_sep);
        if (badq != 0) | bad_pending {
            bad_pending = mark_bad(sf.add(ns), s, badq, bad_pending);
        }
        ns += k;
        lines += nm.count_ones() as usize;
        i += 64;
    }
    for j in 0..16 {
        *seps.add(ns + j) = u32::MAX;
        *sf.add(ns + j) = F_NL;
    }
    sb.lines = lines;
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
