//! Stage 1 of CSV parsing: find every comma, newline and double quote in a
//! block of whole lines, 64 bytes at a time.

use std::arch::x86_64::*;

/// Positions are byte offsets from the block start.
pub struct ScanBuf {
    /// `sep[0]` is a sentinel (`u32::MAX`, one before the block); then the
    /// offset of every comma and newline in order.
    pub sep: Vec<u32>,
    /// For each newline: its index in `sep`.
    pub nl: Vec<u32>,
    /// For each newline: the number of double quotes before it in the block.
    pub nlq: Vec<u32>,
    pub lines: usize,
}

impl ScanBuf {
    pub fn new() -> Self {
        ScanBuf { sep: Vec::new(), nl: Vec::new(), nlq: Vec::new(), lines: 0 }
    }
}

/// Scans `len` bytes at `text`. The block must end right after a newline.
/// Reads up to 63 bytes past the end.
#[inline(never)]
pub unsafe fn scan(text: *const u8, len: usize, sb: &mut ScanBuf) {
    if sb.sep.capacity() < len + 80 {
        sb.sep = Vec::with_capacity(len + 80);
        sb.nl = Vec::with_capacity(len + 8);
        sb.nlq = Vec::with_capacity(len + 8);
    }
    let sep = sb.sep.as_mut_ptr();
    let nl = sb.nl.as_mut_ptr();
    let nlq = sb.nlq.as_mut_ptr();
    *sep = u32::MAX;
    let mut n = 1usize;
    let mut k = 0usize;
    let mut q = 0u32;
    let comma = _mm256_set1_epi8(b',' as i8);
    let newline = _mm256_set1_epi8(b'\n' as i8);
    let quote = _mm256_set1_epi8(b'"' as i8);
    let mut i = 0usize;
    while i < len {
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
        let m = cm | nm;
        let mut t = nm;
        while t != 0 {
            let below = (t & t.wrapping_neg()) - 1;
            *nl.add(k) = (n as u32) + (m & below).count_ones();
            *nlq.add(k) = q + (qm & below).count_ones();
            k += 1;
            t &= t - 1;
        }
        let base = i as u32;
        let mut t = m;
        while t != 0 {
            *sep.add(n) = base + t.trailing_zeros();
            n += 1;
            t &= t - 1;
        }
        q += qm.count_ones();
        i += 64;
    }
    sb.lines = k;
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
