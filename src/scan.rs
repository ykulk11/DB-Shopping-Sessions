//! Stage 1 of CSV parsing: find every comma, newline and double quote in a
//! block of whole lines, 64 bytes at a time.

use std::arch::x86_64::*;

/// Positions are byte offsets from the block start.
pub struct ScanBuf {
    /// Offset of every comma, in order, followed by at least 16 `u32::MAX`.
    pub commas: Vec<u32>,
    /// Offset of every newline, in order.
    pub nls: Vec<u32>,
    /// Per 64-byte word: bitmask of double quotes.
    pub qw: Vec<u64>,
    /// Per 64-byte word: number of double quotes before it in the block.
    pub qcum: Vec<u32>,
    pub lines: usize,
}

impl ScanBuf {
    pub fn new() -> Self {
        ScanBuf { commas: Vec::new(), nls: Vec::new(), qw: Vec::new(), qcum: Vec::new(), lines: 0 }
    }

    /// Number of double quotes before offset `at`.
    #[inline(always)]
    pub unsafe fn quotes_before(&self, at: u32) -> u32 {
        let w = (at >> 6) as usize;
        let below = (1u64 << (at & 63)) - 1;
        *self.qcum.as_ptr().add(w) + (*self.qw.as_ptr().add(w) & below).count_ones()
    }
}

/// Scans `len` bytes at `text`. The block must end right after a newline.
/// Reads up to 63 bytes past the end.
#[inline(never)]
pub unsafe fn scan(text: *const u8, len: usize, sb: &mut ScanBuf) {
    if sb.commas.capacity() < len + 96 {
        sb.commas = Vec::with_capacity(len + 96 + len / 4);
        sb.nls = Vec::with_capacity(len + 96 + len / 4);
        sb.qw = Vec::with_capacity(len / 64 + 8 + len / 128);
        sb.qcum = Vec::with_capacity(len / 64 + 8 + len / 128);
    }
    let commas = sb.commas.as_mut_ptr();
    let nls = sb.nls.as_mut_ptr();
    let qw = sb.qw.as_mut_ptr();
    let qcum = sb.qcum.as_mut_ptr();
    let mut nc = 0usize;
    let mut nn = 0usize;
    let mut q = 0u32;
    let mut w = 0usize;
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
        *qw.add(w) = qm;
        *qcum.add(w) = q;
        q += qm.count_ones();
        w += 1;
        let base = i as u32;

        // Commas: eight slots are written whether or not they are all used.
        let cc = cm.count_ones() as usize;
        let dst = commas.add(nc);
        let mut t = cm;
        for j in 0..8 {
            *dst.add(j) = base + t.trailing_zeros();
            t &= t.wrapping_sub(1);
        }
        if cc > 8 {
            let mut j = 8;
            while t != 0 {
                *dst.add(j) = base + t.trailing_zeros();
                t &= t - 1;
                j += 1;
            }
        }
        nc += cc;

        // Newlines: two slots likewise.
        let cn = nm.count_ones() as usize;
        let dst = nls.add(nn);
        let mut t = nm;
        *dst = base + t.trailing_zeros();
        t &= t.wrapping_sub(1);
        *dst.add(1) = base + t.trailing_zeros();
        t &= t.wrapping_sub(1);
        if cn > 2 {
            let mut j = 2;
            while t != 0 {
                *dst.add(j) = base + t.trailing_zeros();
                t &= t - 1;
                j += 1;
            }
        }
        nn += cn;
        i += 64;
    }
    *qw.add(w) = 0;
    *qcum.add(w) = q;
    for j in 0..16 {
        *commas.add(nc + j) = u32::MAX;
    }
    sb.lines = nn;
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
