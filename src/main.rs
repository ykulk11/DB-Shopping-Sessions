//! Shopping Sessions pipeline. See rules.md for the specification.
//!
//! Phase 1 (parallel over ~2 MB file chunks): parse links, events and clicks.
//!   User IDs are interned into dense numbers in a shared lock-free table.
//!   Event rows become 28-byte records scattered by event_id hash. As soon as
//!   every link chunk is parsed, one thread joins linked IDs (union-find).
//! Phase 2 (parallel over event-hash partitions): dedup on event_id keeping
//!   the smallest ingest_ts, drop invalid winners, scatter survivors by person.
//! Phase 3 (parallel over person partitions): sort each person's events,
//!   build sessions, attribute clicks, aggregate, write Parquet.

mod hash;
mod parquet;
mod scan;
mod sys;
mod ts;

use std::marker::PhantomData;
use std::mem::size_of;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Instant;

use hash::{hash128, StaticTable};
use sys::{Arena, Mapped};
use ts::{BAD, DAY_MS};

const CHUNK: usize = 2 << 20;
const GAP_MS: u32 = 1_800_000;
const WEEK_MS: i64 = 604_800_000;
const VALID: u32 = 0x8000_0000;

const KIND_LINKS: u8 = 0;
const KIND_EVENTS: u8 = 1;
const KIND_CLICKS: u8 = 2;

// ---------------------------------------------------------------------------
// Records
// ---------------------------------------------------------------------------

/// One parsed event row, kept in arrival order in the parsing thread's row store.
#[repr(C)]
#[derive(Clone, Copy)]
struct Row {
    /// Interned user ID (only meaningful when valid).
    uidx: u32,
    /// `VALID | (ts - window_start)` in ms when the row passes validation and
    /// the late rule, else 0. Dedup clears it on the copies that lose.
    tsv: u32,
    /// ingest_ts in seconds from a fixed base; `u32::MAX` when unparsable.
    ing: u32,
    /// 32 bits of the event_id hash, to confirm that two rows are the same event.
    h2: u32,
}

/// Rows per chunk of a row store (2 MB), and the layout of a row reference:
/// thread number + 1 in the top 5 bits, chunk in the next 10, row in the low 17.
const ROW_BITS: u32 = 17;
const ROW_CHUNK: usize = 1 << ROW_BITS;
const ROW_CHUNKS_MAX: usize = 1 << 10;
const MAX_THREADS: usize = 31;

struct RowStore {
    /// (first row, rows used) per chunk.
    chunks: Vec<(*mut Row, u32)>,
}

impl RowStore {
    /// Room for `n` consecutive rows: a pointer to the first and its number in this store.
    #[inline(always)]
    unsafe fn reserve(&mut self, arena: &mut Arena, n: usize) -> (*mut Row, u32) {
        let full = match self.chunks.last() {
            Some(&(_, used)) => used as usize + n > ROW_CHUNK,
            None => true,
        };
        if full {
            if n > ROW_CHUNK || self.chunks.len() == ROW_CHUNKS_MAX {
                sys::die("row store limit exceeded");
            }
            let p = arena.alloc(ROW_CHUNK * size_of::<Row>(), 4096) as *mut Row;
            self.chunks.push((p, 0));
        }
        let c = self.chunks.len() - 1;
        let (base, used) = *self.chunks.get_unchecked(c);
        (base.add(used as usize), ((c as u32) << ROW_BITS) | used)
    }

    #[inline(always)]
    unsafe fn commit(&mut self, n: usize) {
        self.chunks.last_mut().unwrap_unchecked().1 += n as u32;
    }
}

#[inline(always)]
unsafe fn row_ptr(p1: &[P1], r: u32) -> *mut Row {
    let t = (r >> (ROW_BITS + 10)) as usize - 1;
    let c = ((r >> ROW_BITS) & (ROW_CHUNKS_MAX as u32 - 1)) as usize;
    p1.get_unchecked(t).rows.chunks.get_unchecked(c).0.add((r & (ROW_CHUNK as u32 - 1)) as usize)
}

/// A valid click, stored in its person's partition.
#[repr(C)]
#[derive(Clone, Copy)]
struct Click {
    /// ts - (window_start - 7 days), in ms.
    ts: u32,
    /// Person's index within the partition in the high 12 bits, campaign below.
    lc: u32,
    /// The click_id bytes, as `idref` packs them.
    idref: u64,
}

const CAMP_BITS: u32 = 20;
const CAMP_MASK: u32 = (1 << CAMP_BITS) - 1;

#[repr(C)]
#[derive(Clone, Copy)]
struct PEv {
    person: u32,
    ts: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct PPu {
    person: u32,
    ts: u32,
    pc: u32,
    pad: u32,
}

#[derive(Clone, Copy)]
struct Pend {
    lo: u64,
    hi: u64,
    idref: u64,
    rec: u32,
}

// ---------------------------------------------------------------------------
// Partitioned append-only storage. Each partition has one cache line of
// buffer; a full line goes to the partition's current 4 KB block with
// non-temporal stores, so the destination is never read before it is written.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct WcHead {
    cur: *mut u8,
    cnt: u32,
    last: u32,
}

#[derive(Clone, Copy)]
struct Blk {
    ptr: *mut u8,
    prev: u32,
}

/// `T` must be 8, 16 or 32 bytes.
struct Wc<T: Copy> {
    wc: *mut u8,
    /// Records waiting in each partition's buffer line. Kept apart from
    /// `heads` so the common push touches only this small array and the line.
    pos: Vec<u8>,
    heads: Vec<WcHead>,
    blocks: Vec<Blk>,
    /// 4 KB blocks no longer in use elsewhere, taken before new memory.
    spare: Vec<*mut u8>,
    _p: PhantomData<T>,
}

unsafe impl<T: Copy> Send for Wc<T> {}
unsafe impl<T: Copy> Sync for Wc<T> {}

impl<T: Copy> Wc<T> {
    const SZ: usize = size_of::<T>();
    const PER_LINE: u32 = (64 / size_of::<T>()) as u32;
    const PER_BLOCK: usize = 4096 / size_of::<T>();

    unsafe fn new(parts: usize, arena: &mut Arena) -> Self {
        assert!(Self::SZ == 8 || Self::SZ == 16 || Self::SZ == 32);
        Wc {
            wc: arena.alloc(parts * 64, 4096),
            pos: vec![0u8; parts],
            heads: vec![WcHead { cur: std::ptr::null_mut(), cnt: 0, last: u32::MAX }; parts],
            blocks: Vec::with_capacity(4096),
            spare: Vec::new(),
            _p: PhantomData,
        }
    }

    #[inline(always)]
    unsafe fn push(&mut self, arena: &mut Arena, part: usize, rec: T) {
        let pos = self.pos.get_unchecked_mut(part);
        let idx = *pos as u32;
        let line = self.wc.add(part * 64);
        std::ptr::write(line.add(idx as usize * Self::SZ) as *mut T, rec);
        if idx == Self::PER_LINE - 1 {
            *pos = 0;
            self.flush_line(arena, part, line);
        } else {
            *pos = idx as u8 + 1;
        }
    }

    /// Moves a full buffer line to the partition's current block.
    #[inline(always)]
    unsafe fn flush_line(&mut self, arena: &mut Arena, part: usize, line: *mut u8) {
        use std::arch::x86_64::*;
        let h = self.heads.get_unchecked_mut(part);
        let c = h.cnt;
        if (c / Self::PER_LINE) & 63 == 0 {
            let b = match self.spare.pop() {
                Some(b) => b,
                None => arena.alloc(4096, 4096),
            };
            self.blocks.push(Blk { ptr: b, prev: h.last });
            h.last = (self.blocks.len() - 1) as u32;
            h.cur = b;
        }
        let a = _mm256_load_si256(line as *const __m256i);
        let b = _mm256_load_si256(line.add(32) as *const __m256i);
        _mm256_stream_si256(h.cur as *mut __m256i, a);
        _mm256_stream_si256(h.cur.add(32) as *mut __m256i, b);
        h.cur = h.cur.add(64);
        h.cnt = c + Self::PER_LINE;
    }

    /// Writes out the partly filled lines. Call once, after the last push.
    unsafe fn finish(&mut self, arena: &mut Arena) {
        for part in 0..self.heads.len() {
            let rem = self.pos[part] as u32;
            let h = &mut self.heads[part];
            if rem != 0 {
                if (h.cnt / Self::PER_LINE) & 63 == 0 {
                    let b = match self.spare.pop() {
                        Some(b) => b,
                        None => arena.alloc(4096, 4096),
                    };
                    self.blocks.push(Blk { ptr: b, prev: h.last });
                    h.last = (self.blocks.len() - 1) as u32;
                    h.cur = b;
                }
                std::ptr::copy_nonoverlapping(self.wc.add(part * 64), h.cur, rem as usize * Self::SZ);
                h.cnt += rem;
                self.pos[part] = 0;
            }
        }
        std::arch::x86_64::_mm_sfence();
    }

    fn count(&self, part: usize) -> usize {
        self.heads[part].cnt as usize
    }

    /// Calls `f(first record, count)` for each block of a partition.
    #[inline(always)]
    unsafe fn for_each_block(&self, part: usize, mut f: impl FnMut(*mut T, usize)) {
        let h = *self.heads.get_unchecked(part);
        let total = h.cnt as usize;
        if total == 0 {
            return;
        }
        let nb = (total + Self::PER_BLOCK - 1) / Self::PER_BLOCK;
        let mut n = total - (nb - 1) * Self::PER_BLOCK;
        let mut bi = h.last;
        while bi != u32::MAX {
            let b = *self.blocks.get_unchecked(bi as usize);
            f(b.ptr as *mut T, n);
            bi = b.prev;
            n = Self::PER_BLOCK;
        }
    }

    /// Calls `f` for each record of a partition. The blocks are scattered
    /// pages, so each block's successor is requested line by line while the
    /// current one is read.
    #[inline(always)]
    unsafe fn for_each(&self, part: usize, mut f: impl FnMut(*mut T)) {
        let h = *self.heads.get_unchecked(part);
        let total = h.cnt as usize;
        if total == 0 {
            return;
        }
        let nb = (total + Self::PER_BLOCK - 1) / Self::PER_BLOCK;
        let mut n = total - (nb - 1) * Self::PER_BLOCK;
        let mut bi = h.last;
        while bi != u32::MAX {
            let b = *self.blocks.get_unchecked(bi as usize);
            let base = b.ptr as *mut T;
            if b.prev != u32::MAX {
                let next = self.blocks.get_unchecked(b.prev as usize).ptr;
                let per_line = Self::PER_LINE as usize;
                for i in 0..n {
                    if i % per_line == 0 {
                        prefetch(next.add(i * Self::SZ));
                    }
                    f(base.add(i));
                }
            } else {
                for i in 0..n {
                    f(base.add(i));
                }
            }
            bi = b.prev;
            n = Self::PER_BLOCK;
        }
    }
}

// ---------------------------------------------------------------------------
// User ID interning: a shared lock-free open-addressing table.
// ---------------------------------------------------------------------------

const ID_BLOCK: u32 = 256;

struct Intern {
    slots: *mut [AtomicU64; 2],
    bytes: usize,
    shift: u32,
    mask: usize,
    /// Numbers handed out never reach this; the arrays below hold this many.
    limit: u32,
    /// Per number: pointer to the ID bytes in the low 48 bits, length above.
    id_ref: *mut u64,
    /// Per number: 0 for a root, else the root's number + 1 (after linking).
    penc: *mut u32,
    /// Per root: 0 when the root's own ID names the person, else number + 1.
    name_enc: *mut u32,
    next: AtomicU32,
    overflow: AtomicBool,
}

unsafe impl Send for Intern {}
unsafe impl Sync for Intern {}

struct InternLocal {
    next: u32,
    end: u32,
}

#[inline(always)]
fn idref(p: *const u8, len: usize) -> u64 {
    (p as u64) | ((len.min(0xffff) as u64) << 48)
}

/// The bytes of an interned ID.
#[inline(always)]
unsafe fn id_bytes<'a>(r: u64) -> &'a [u8] {
    let p = (r & 0x0000_ffff_ffff_ffff) as *const u8;
    let mut len = (r >> 48) as usize;
    if len == 0xffff {
        // Longer than fits: the field ends at the next separator or quote.
        while !matches!(*p.add(len), b',' | b'\n' | b'"') {
            len += 1;
        }
    }
    std::slice::from_raw_parts(p, len)
}

impl Intern {
    unsafe fn new(cap: usize) -> Self {
        let bytes = cap * 16;
        Intern {
            slots: sys::anon_huge(bytes) as *mut [AtomicU64; 2],
            bytes,
            shift: 64 - cap.trailing_zeros(),
            mask: cap - 1,
            limit: (cap / 8 * 5) as u32,
            id_ref: sys::anon_huge(cap / 8 * 5 * 8) as *mut u64,
            penc: sys::anon_huge(cap / 8 * 5 * 4) as *mut u32,
            name_enc: sys::anon_huge(cap / 8 * 5 * 4) as *mut u32,
            next: AtomicU32::new(0),
            overflow: AtomicBool::new(false),
        }
    }

    /// Returns the tables to the kernel without touching the struct.
    unsafe fn unmap(&self) {
        sys::release(self.slots as *mut u8, self.bytes);
        sys::release(self.id_ref as *mut u8, self.limit as usize * 8);
        sys::release(self.penc as *mut u8, self.limit as usize * 4);
        sys::release(self.name_enc as *mut u8, self.limit as usize * 4);
    }

    unsafe fn free(&mut self) {
        sys::release(self.slots as *mut u8, self.bytes);
        sys::release(self.id_ref as *mut u8, self.limit as usize * 8);
        sys::release(self.penc as *mut u8, self.limit as usize * 4);
        sys::release(self.name_enc as *mut u8, self.limit as usize * 4);
    }

    /// Faults in thread `tid`'s share of the table. Lookups read a slot before
    /// writing it; on untouched memory that is two page faults and a TLB
    /// shootdown per page unless the pages are already there.
    unsafe fn populate(&self, tid: usize, threads: usize) {
        let page = 2usize << 20;
        let per = (self.bytes / threads + page - 1) & !(page - 1);
        let from = (tid * per).min(self.bytes);
        let to = ((tid + 1) * per).min(self.bytes);
        sys::populate_write((self.slots as *mut u8).add(from), to - from);
    }

    #[inline(always)]
    unsafe fn prefetch(&self, lo: u64) {
        use std::arch::x86_64::{_mm_prefetch, _MM_HINT_T0};
        _mm_prefetch::<_MM_HINT_T0>(self.slots.add((lo >> self.shift) as usize) as *const i8);
    }

    #[cold]
    #[inline(never)]
    fn refill(&self, st: &mut InternLocal) -> bool {
        let start = self.next.fetch_add(ID_BLOCK, Ordering::Relaxed);
        if start as u64 + ID_BLOCK as u64 > self.limit as u64 {
            self.overflow.store(true, Ordering::Relaxed);
            return false;
        }
        st.next = start;
        st.end = start + ID_BLOCK;
        true
    }

    /// The number for an ID, assigning a new one on first sight.
    #[inline(always)]
    unsafe fn get(&self, st: &mut InternLocal, lo: u64, hi: u64, idref: u64) -> u32 {
        let tag_lo = lo & 0xffff_ffff;
        let tag_hi = hi | 1;
        let mut i = (lo >> self.shift) as usize;
        loop {
            let s = &*self.slots.add(i);
            let mut w1 = s[1].load(Ordering::Relaxed);
            if w1 == 0 {
                if st.next == st.end && !self.refill(st) {
                    return 0;
                }
                let idx = st.next;
                *self.id_ref.add(idx as usize) = idref;
                match s[1].compare_exchange(
                    0,
                    (tag_lo << 32) | (idx as u64 + 1),
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => {
                        s[0].store(tag_hi, Ordering::Release);
                        st.next += 1;
                        return idx;
                    }
                    Err(cur) => w1 = cur,
                }
            }
            if (w1 >> 32) == tag_lo {
                let mut w0 = s[0].load(Ordering::Acquire);
                while w0 == 0 {
                    std::hint::spin_loop();
                    w0 = s[0].load(Ordering::Acquire);
                }
                if w0 == tag_hi {
                    return (w1 as u32).wrapping_sub(1);
                }
            }
            i = (i + 1) & self.mask;
        }
    }

    #[inline(always)]
    unsafe fn prefetch_person(&self, uidx: u32) {
        use std::arch::x86_64::{_mm_prefetch, _MM_HINT_T0};
        _mm_prefetch::<_MM_HINT_T0>(self.penc.wrapping_add(uidx as usize) as *const i8);
    }

    #[inline(always)]
    unsafe fn person(&self, uidx: u32) -> u32 {
        let e = *self.penc.add(uidx as usize);
        if e == 0 {
            uidx
        } else {
            e - 1
        }
    }
}

#[inline(always)]
unsafe fn uf_find(penc: *mut u32, mut x: u32) -> u32 {
    loop {
        let e = *penc.add(x as usize);
        if e == 0 {
            return x;
        }
        let p = e - 1;
        let ge = *penc.add(p as usize);
        if ge != 0 {
            *penc.add(x as usize) = ge;
        }
        x = p;
    }
}

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct Chunk {
    file: u32,
    kind: u8,
    k: u32,
}

struct Shared {
    /// Window start, end and click-window start, in ms.
    ws: i64,
    we: i64,
    cs: i64,
    ing_base: i64,
    ws_mod_day: u64,
    day0: i32,
    ndays: usize,
    products: Products,
    cat_names: Vec<Vec<u8>>,
    unknown_cat: u32,
    campaigns: StaticTable,
    camp_names: Vec<Vec<u8>>,
    camp_channels: Vec<Vec<u8>>,
    files: Vec<Mapped>,
    chunks: Vec<Chunk>,
    n_link_chunks: usize,
    next_chunk: AtomicUsize,
    links_done: AtomicUsize,
    link_pairs: Mutex<Vec<Vec<(u32, u32)>>>,
    /// Set once linked IDs are joined; clicks are parsed only after that.
    links_ready: AtomicBool,
    intern: Intern,
    pe_bits: u32,
    /// Number of person partitions; a person's partition is `person >> PSHIFT`.
    np: usize,
    /// Bytes of text scanned and parsed at a time.
    block: usize,
    /// Dedup splits partitions with more rows than this.
    split_rows: usize,
    /// CPUs to bind worker threads to; empty leaves placement to the kernel.
    pin: Vec<usize>,
}

impl Shared {
    fn pin(&self, tid: usize) {
        if !self.pin.is_empty() {
            sys::pin_to(self.pin[tid % self.pin.len()]);
        }
    }
}

/// product_id -> category. When every ID in products.csv is `p` followed by
/// a decimal number without leading zeros (as generated catalogs are), a
/// byte per number answers the lookup from a table that stays in cache;
/// otherwise, and for every ID when that is not so, the hash table does.
struct Products {
    /// Category + 1 by product number, 0 for none; empty when not usable.
    direct: Vec<u8>,
    table: StaticTable,
}

/// The number of an ID of the form `p` + 1 to 7 decimal digits with no
/// leading zero (`p0` is fine), else `None`. Distinct IDs of that form have
/// distinct numbers. May read up to 8 bytes past the ID.
#[inline(always)]
unsafe fn product_number(p: *const u8, len: usize) -> Option<usize> {
    if len < 2 || len > 8 || *p != b'p' {
        return None;
    }
    let nd = len - 1;
    let mask = (1u64 << (8 * nd)) - 1;
    // Digits become their values; the first digit is the lowest byte.
    let x = ((p.add(1) as *const u64).read_unaligned() ^ 0x3030_3030_3030_3030) & mask;
    if (x.wrapping_add(0x7676_7676_7676_7676) | x) & 0x8080_8080_8080_8080 & mask != 0 {
        return None;
    }
    if nd > 1 && x & 0xff == 0 {
        return None;
    }
    let mut v = 0usize;
    for i in 0..nd {
        v = v * 10 + ((x >> (8 * i)) & 0xff) as usize;
    }
    Some(v)
}

impl Products {
    fn new(rows: &[(&[u8], &[u8])], cat_of: impl Fn(&[u8]) -> u32) -> Self {
        let mut table = StaticTable::new(rows.len());
        let mut numbers = Vec::with_capacity(rows.len());
        let mut direct_ok = std::env::var("PIPE_NO_DIRECT").is_err();
        for (id, cat) in rows {
            let c = cat_of(cat);
            // The mapped file is readable well past any ID.
            table.insert(unsafe { hash128(id.as_ptr(), id.len()) }, c);
            match unsafe { product_number(id.as_ptr(), id.len()) } {
                Some(n) if c < 255 => numbers.push((n, c)),
                _ => direct_ok = false,
            }
        }
        let mut direct = Vec::new();
        let top = numbers.iter().map(|x| x.0).max().unwrap_or(0);
        // Only for a dense catalog, where the byte table is the smaller one.
        if direct_ok && !numbers.is_empty() && top < rows.len() * 8 + 4096 {
            direct = vec![0u8; top + 1];
            for &(n, c) in &numbers {
                // The first row for an ID counts, as in the hash table.
                if direct[n] == 0 {
                    direct[n] = c as u8 + 1;
                }
            }
        }
        Products { direct, table }
    }

    /// The category of a product ID, or `unknown`.
    #[inline(always)]
    unsafe fn get(&self, p: *const u8, len: usize, unknown: u32) -> u32 {
        if !self.direct.is_empty() {
            return match product_number(p, len) {
                Some(n) if n < self.direct.len() && *self.direct.get_unchecked(n) != 0 => {
                    *self.direct.get_unchecked(n) as u32 - 1
                }
                _ => unknown,
            };
        }
        match self.table.get(hash128(p, len)) {
            Some(c) => c,
            None => unknown,
        }
    }
}

/// Each person partition covers this many consecutive interned numbers (2^12).
const PSHIFT: u32 = 12;

// ---------------------------------------------------------------------------
// Field splitting
// ---------------------------------------------------------------------------

/// Walks the lines of a scanned block.
struct LineIter {
    /// Index in `seps` of the newline before the next line.
    k: usize,
    /// Lines handed out so far.
    line: usize,
}

impl LineIter {
    fn new() -> Self {
        LineIter { k: 0, line: 0 }
    }

    /// Advances to the next line (the caller checks `line < lines` first) and
    /// fills the bounds of its `N` fields, quotes stripped. Returns false
    /// when the line does not have exactly `N` fields or has a double quote
    /// anywhere except around a whole field. `N` is at most 7.
    #[inline(always)]
    unsafe fn next<const N: usize>(&mut self, sb: &scan::ScanBuf, fs: &mut [u32; 8], fe: &mut [u32; 8]) -> bool {
        use std::arch::x86_64::*;
        const ONES: u64 = 0x0101_0101_0101_0101;
        self.line += 1;
        let k = self.k;
        let sf = sb.sf.as_ptr();
        // Flags of the separator before each field, and of the one after it.
        let a = (sf.add(k) as *const u64).read_unaligned();
        let b = (sf.add(k + 1) as *const u64).read_unaligned();
        let fields = (1u64 << (8 * N)) - 1;
        // The first newline is the Nth separator, and no field has a bad quote.
        let shape = (b & (ONES * (scan::F_NL | scan::F_BAD) as u64) & fields) ^ (1u64 << (8 * (N - 1)));
        // A field opens with a quote exactly when it closes with one.
        let pairs = ((a >> 1) ^ (b >> 2)) & ONES & fields;
        if (shape | pairs) != 0 {
            let mut j = k + 1;
            while *sf.add(j) & scan::F_NL == 0 {
                j += 1;
            }
            self.k = j;
            return false;
        }
        self.k = k + N;
        let seps = sb.seps.as_ptr();
        let before = _mm256_loadu_si256(seps.add(k) as *const __m256i);
        let after = _mm256_loadu_si256(seps.add(k + 1) as *const __m256i);
        let qa = _mm256_cvtepu8_epi32(_mm_cvtsi64_si128(((a >> 1) & ONES) as i64));
        let qb = _mm256_cvtepu8_epi32(_mm_cvtsi64_si128(((b >> 2) & ONES) as i64));
        let start = _mm256_add_epi32(_mm256_sub_epi32(before, _mm256_set1_epi32(-1)), qa);
        _mm256_storeu_si256(fs.as_mut_ptr() as *mut __m256i, start);
        _mm256_storeu_si256(fe.as_mut_ptr() as *mut __m256i, _mm256_sub_epi32(after, qb));
        true
    }
}

#[inline(always)]
unsafe fn event_type(p: *const u8, len: usize) -> u32 {
    if len == 4 {
        let w = (p as *const u32).read_unaligned();
        (w == u32::from_le_bytes(*b"view") || w == u32::from_le_bytes(*b"cart")) as u32
    } else if len == 8 {
        ((p as *const u64).read_unaligned() == u64::from_le_bytes(*b"purchase")) as u32 * 2
    } else {
        0
    }
}

// ---------------------------------------------------------------------------
// Phase 1
// ---------------------------------------------------------------------------

struct P1 {
    sb: scan::ScanBuf,
    dc: ts::DateCache,
    pend: Vec<Pend>,
    crecs: Vec<Click>,
    il: InternLocal,
    arena: Arena,
    /// Every event row this thread parsed, in order.
    rows: RowStore,
    /// (row number, price and category) of this thread's purchases, in row order.
    pur: Vec<(u32, u32)>,
    /// Per event-hash partition: 32 hash bits and a row reference per row.
    fp: Wc<u64>,
    /// This thread's number + 1, shifted into place for row references.
    ref_base: u32,
    /// Valid clicks by person partition.
    cl: Wc<Click>,
    cuid: Vec<u32>,
    links: Vec<(u32, u32)>,
    /// Seconds spent in: scan, parse, intern, scatter, linking, clicks, links.
    tm: [f64; 7],
}

unsafe impl Send for P1 {}
unsafe impl Sync for P1 {}

#[inline(always)]
unsafe fn line_start_at_or_after(p: *const u8, x: usize, len: usize) -> usize {
    if *p.add(x - 1) == b'\n' {
        x
    } else {
        let n = scan::find_newline(p, x, len);
        if n >= len {
            len
        } else {
            n + 1
        }
    }
}

/// The byte range of the lines that start inside chunk `k` of a file.
unsafe fn chunk_range(f: &Mapped, k: usize) -> (usize, usize) {
    let len = f.len;
    let p = f.ptr;
    let nominal = k * CHUNK;
    if nominal >= len {
        return (0, 0);
    }
    let begin = if k == 0 {
        // The first line of every file is its header.
        let h = scan::find_newline(p, 0, len);
        if h >= len {
            return (0, 0);
        }
        h + 1
    } else {
        line_start_at_or_after(p, nominal, len)
    };
    let stop = ((k + 1) * CHUNK).min(len);
    let fin = if stop >= len { len } else { line_start_at_or_after(p, stop, len) };
    (begin, fin)
}

unsafe fn events_block(sh: &Shared, cx: &mut P1, text: *const u8, len: usize) {
    let t_a = Instant::now();
    scan::scan(text, len, &mut cx.sb);
    let t_b = Instant::now();
    let lines = cx.sb.lines;
    if cx.pend.capacity() < lines {
        cx.pend = Vec::with_capacity(lines + lines / 2 + 16);
    }
    let (rows, first) = cx.rows.reserve(&mut cx.arena, lines);
    let shift = 24 - sh.pe_bits;
    let rbase = (cx.ref_base | first) as u64;
    let pend = cx.pend.as_mut_ptr();
    let mut nrec = 0usize;
    let mut npend = 0usize;
    let ws = sh.ws;
    let we = sh.we;
    let mut it = LineIter::new();
    let mut fs = [0u32; 8];
    let mut fe = [0u32; 8];
    while it.line < lines {
        if !it.next::<7>(&cx.sb, &mut fs, &mut fe) {
            continue;
        }
        let l0 = (fe[0] - fs[0]) as usize;
        if l0 == 0 {
            continue;
        }
        let (h1, h2) = hash128(text.add(fs[0] as usize), l0);
        let l1 = (fe[1] - fs[1]) as usize;
        let t = ts::parse_ts(text.add(fs[2] as usize), (fe[2] - fs[2]) as usize, &mut cx.dc);
        let ing = ts::parse_f1_seconds(text.add(fs[3] as usize), (fe[3] - fs[3]) as usize, &mut cx.dc);
        let ty = event_type(text.add(fs[4] as usize), (fe[4] - fs[4]) as usize);
        let l5 = (fe[5] - fs[5]) as usize;
        let mut valid = (l1 != 0)
            & (t != BAD)
            & (t >= ws)
            & (t < we)
            & (ty != 0)
            & (l5 != 0)
            & (ing != BAD)
            & (ing.wrapping_mul(1000).wrapping_sub(t) <= DAY_MS);
        if valid && ty == 2 {
            let price = ts::parse_price(text.add(fs[6] as usize), (fe[6] - fs[6]) as usize);
            if price == 0 {
                valid = false;
            } else {
                let cat = sh.products.get(text.add(fs[5] as usize), l5, sh.unknown_cat);
                cx.pur.push((first + nrec as u32, price | (cat << 24)));
            }
        }
        let ing_key = if ing == BAD {
            u32::MAX
        } else {
            (ing - sh.ing_base).clamp(0, u32::MAX as i64 - 1) as u32
        };
        let tsv = if valid { (t - ws) as u32 | VALID } else { 0 };
        *rows.add(nrec) = Row { uidx: u32::MAX, tsv, ing: ing_key, h2: h2 as u32 };
        // Hash bits 8..40 as the tag, the row reference below.
        cx.fp.push(
            &mut cx.arena,
            ((h1 >> 40) as usize) >> shift,
            ((h1 << 24) & 0xffff_ffff_0000_0000) | (rbase + nrec as u64),
        );
        if valid {
            let up = text.add(fs[1] as usize);
            let (lo, hi) = hash128(up, l1);
            sh.intern.prefetch(lo);
            *pend.add(npend) = Pend { lo, hi, idref: idref(up, l1), rec: nrec as u32 };
            npend += 1;
        }
        nrec += 1;
    }
    let t_c = Instant::now();
    for i in 0..npend {
        let p = *pend.add(i);
        let u = sh.intern.get(&mut cx.il, p.lo, p.hi, p.idref);
        (*rows.add(p.rec as usize)).uidx = u;
    }
    let t_d = Instant::now();
    cx.rows.commit(nrec);
    let t_e = Instant::now();
    cx.tm[0] += (t_b - t_a).as_secs_f64();
    cx.tm[1] += (t_c - t_b).as_secs_f64();
    cx.tm[2] += (t_d - t_c).as_secs_f64();
    cx.tm[3] += (t_e - t_d).as_secs_f64();
}

unsafe fn clicks_block(sh: &Shared, cx: &mut P1, text: *const u8, len: usize) {
    let t_a = Instant::now();
    scan::scan(text, len, &mut cx.sb);
    let lines = cx.sb.lines;
    if cx.crecs.capacity() < lines {
        cx.crecs = Vec::with_capacity(lines + lines / 2 + 16);
    }
    if cx.pend.capacity() < lines {
        cx.pend = Vec::with_capacity(lines + lines / 2 + 16);
    }
    if cx.cuid.capacity() < lines {
        cx.cuid = Vec::with_capacity(lines + lines / 2 + 16);
    }
    let recs = cx.crecs.as_mut_ptr();
    let pend = cx.pend.as_mut_ptr();
    let cuid = cx.cuid.as_mut_ptr();
    let mut n = 0usize;
    let mut it = LineIter::new();
    let mut fs = [0u32; 8];
    let mut fe = [0u32; 8];
    while it.line < lines {
        if !it.next::<4>(&cx.sb, &mut fs, &mut fe) {
            continue;
        }
        let l0 = (fe[0] - fs[0]) as usize;
        let l1 = (fe[1] - fs[1]) as usize;
        if l0 == 0 || l1 == 0 {
            continue;
        }
        let t = ts::parse_ts(text.add(fs[3] as usize), (fe[3] - fs[3]) as usize, &mut cx.dc);
        if t == BAD || t < sh.cs || t >= sh.we {
            continue;
        }
        let camp = match sh.campaigns.get(hash128(text.add(fs[2] as usize), (fe[2] - fs[2]) as usize)) {
            Some(c) => c,
            None => continue,
        };
        let up = text.add(fs[1] as usize);
        let (lo, hi) = hash128(up, l1);
        sh.intern.prefetch(lo);
        *pend.add(n) = Pend { lo, hi, idref: idref(up, l1), rec: n as u32 };
        *recs.add(n) = Click { ts: (t - sh.cs) as u32, lc: camp, idref: idref(text.add(fs[0] as usize), l0) };
        n += 1;
    }
    for i in 0..n {
        let p = *pend.add(i);
        let u = sh.intern.get(&mut cx.il, p.lo, p.hi, p.idref);
        sh.intern.prefetch_person(u);
        *cuid.add(i) = u;
    }
    for i in 0..n {
        let mut r = *recs.add(i);
        let person = sh.intern.person(*cuid.add(i));
        r.lc |= (person & ((1 << PSHIFT) - 1)) << CAMP_BITS;
        cx.cl.push(&mut cx.arena, (person >> PSHIFT) as usize, r);
    }
    cx.tm[5] += t_a.elapsed().as_secs_f64();
}

unsafe fn links_block(sh: &Shared, cx: &mut P1, text: *const u8, len: usize) {
    let t_a = Instant::now();
    scan::scan(text, len, &mut cx.sb);
    let lines = cx.sb.lines;
    if cx.pend.capacity() < lines * 2 {
        cx.pend = Vec::with_capacity(lines * 3 + 16);
    }
    let pend = cx.pend.as_mut_ptr();
    let mut n = 0usize;
    let mut it = LineIter::new();
    let mut fs = [0u32; 8];
    let mut fe = [0u32; 8];
    while it.line < lines {
        if !it.next::<2>(&cx.sb, &mut fs, &mut fe) {
            continue;
        }
        let l0 = (fe[0] - fs[0]) as usize;
        let l1 = (fe[1] - fs[1]) as usize;
        if l0 == 0 || l1 == 0 {
            continue;
        }
        for (s, l) in [(fs[0], l0), (fs[1], l1)] {
            let p = text.add(s as usize);
            let (lo, hi) = hash128(p, l);
            sh.intern.prefetch(lo);
            *pend.add(n) = Pend { lo, hi, idref: idref(p, l), rec: 0 };
            n += 1;
        }
    }
    let mut i = 0;
    while i < n {
        let pa = *pend.add(i);
        let pb = *pend.add(i + 1);
        let a = sh.intern.get(&mut cx.il, pa.lo, pa.hi, pa.idref);
        let b = sh.intern.get(&mut cx.il, pb.lo, pb.hi, pb.idref);
        if a != b {
            cx.links.push((a, b));
        }
        i += 2;
    }
    cx.tm[6] += t_a.elapsed().as_secs_f64();
}

/// Joins linked IDs into people and names each person. Runs on one thread
/// once every link line has been parsed.
unsafe fn link_people(sh: &Shared) {
    let lists = sh.link_pairs.lock().unwrap();
    let penc = sh.intern.penc;
    let name_enc = sh.intern.name_enc;
    let id_ref = sh.intern.id_ref;
    for list in lists.iter() {
        for &(a, b) in list {
            let ra = uf_find(penc, a);
            let rb = uf_find(penc, b);
            if ra != rb {
                *penc.add(rb as usize) = ra + 1;
            }
        }
    }
    for list in lists.iter() {
        for &(a, b) in list {
            for x in [a, b] {
                let r = uf_find(penc, x);
                let cur = *name_enc.add(r as usize);
                let cur_id = if cur == 0 { r } else { cur - 1 };
                if x != cur_id
                    && id_bytes(*id_ref.add(x as usize)) < id_bytes(*id_ref.add(cur_id as usize))
                {
                    *name_enc.add(r as usize) = x + 1;
                }
            }
        }
    }
    for list in lists.iter() {
        for &(a, b) in list {
            for x in [a, b] {
                let r = uf_find(penc, x);
                if r != x {
                    *penc.add(x as usize) = r + 1;
                }
            }
        }
    }
}

unsafe fn phase1(sh: &Shared, tid: usize, threads: usize) -> P1 {
    sh.intern.populate(tid, threads);
    let pe = 1usize << sh.pe_bits;
    let mut arena = Arena::new();
    let fp = Wc::new(pe, &mut arena);
    let cl = Wc::new(sh.np, &mut arena);
    let mut cx = P1 {
        sb: scan::ScanBuf::new(),
        dc: ts::DateCache::new(),
        pend: Vec::new(),
        crecs: Vec::new(),
        il: InternLocal { next: 0, end: 0 },
        arena,
        rows: RowStore { chunks: Vec::new() },
        pur: Vec::new(),
        fp,
        ref_base: ((tid + 1) as u32) << (ROW_BITS + 10),
        cl,
        cuid: Vec::new(),
        links: Vec::new(),
        tm: [0.0; 7],
    };
    loop {
        if sh.intern.overflow.load(Ordering::Relaxed) {
            break;
        }
        let c = sh.next_chunk.fetch_add(1, Ordering::Relaxed);
        if c >= sh.chunks.len() {
            break;
        }
        let ch = sh.chunks[c];
        if ch.kind == KIND_CLICKS {
            // A click needs its person, so linking has to be finished.
            while !sh.links_ready.load(Ordering::Acquire) {
                if sh.intern.overflow.load(Ordering::Relaxed) {
                    break;
                }
                std::thread::yield_now();
            }
        }
        let f = &sh.files[ch.file as usize];
        let (begin, fin) = chunk_range(f, ch.k as usize);
        let p = f.ptr;
        let mut pos = begin;
        while pos < fin {
            let mut bend = (pos + sh.block).min(fin);
            if bend < fin {
                bend = match scan::last_line_end(p, pos, bend) {
                    Some(e) => e,
                    None => (scan::find_newline(p, bend, fin) + 1).min(fin),
                };
            }
            match ch.kind {
                KIND_EVENTS => events_block(sh, &mut cx, p.add(pos), bend - pos),
                KIND_CLICKS => clicks_block(sh, &mut cx, p.add(pos), bend - pos),
                _ => links_block(sh, &mut cx, p.add(pos), bend - pos),
            }
            pos = bend;
        }
        if ch.kind == KIND_LINKS {
            let pairs = std::mem::take(&mut cx.links);
            sh.link_pairs.lock().unwrap().push(pairs);
            if sh.links_done.fetch_add(1, Ordering::AcqRel) + 1 == sh.n_link_chunks {
                let t_a = Instant::now();
                link_people(sh);
                sh.links_ready.store(true, Ordering::Release);
                cx.tm[4] += t_a.elapsed().as_secs_f64();
            }
        }
    }
    cx.fp.finish(&mut cx.arena);
    cx.cl.finish(&mut cx.arena);
    cx
}

// ---------------------------------------------------------------------------
// Phase 2
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct Params {
    threads: usize,
    /// Person partitions that can hold anyone.
    np_used: usize,
}

struct P2 {
    arena: Arena,
    ev: Wc<PEv>,
    pu: Wc<PPu>,
}

unsafe impl Send for P2 {}
unsafe impl Sync for P2 {}

/// Phase 2a: dedup on event_id within event-hash partitions. Of the rows
/// with one event_id, all but the one with the smallest ingest_ts are cleared.
/// The hash table of one dedup bucket: fingerprints, 0 for an empty slot.
struct DedupTable {
    slots: Vec<u64>,
    mask: usize,
    /// (slot, fingerprint) of rows that look like copies of an earlier one.
    pending: Vec<(u32, u64)>,
}

impl DedupTable {
    /// Empties the table and sizes it for `count` rows: a quarter to half full.
    fn reset(&mut self, count: usize) {
        let tsize = (count * 2).next_power_of_two().max(64);
        if self.slots.len() < tsize {
            self.slots = vec![0u64; tsize];
        } else {
            self.slots[..tsize].fill(0);
        }
        self.mask = tsize - 1;
        self.pending.clear();
    }

    #[inline(always)]
    unsafe fn insert(&mut self, p1: &[P1], fp: u64) {
        let tab = self.slots.as_mut_ptr();
        let tag = fp >> 32;
        let mut i = (tag as usize) & self.mask;
        loop {
            let s = *tab.add(i);
            if s == 0 {
                *tab.add(i) = fp;
                break;
            }
            if s >> 32 == tag {
                // Probably a copy of the same event. The rows are far away:
                // ask for them now and settle it after the bucket.
                prefetch(row_ptr(p1, s as u32) as *const u8);
                prefetch(row_ptr(p1, fp as u32) as *const u8);
                self.pending.push((i as u32, fp));
                break;
            }
            i = (i + 1) & self.mask;
        }
    }

    /// Settles the pending rows: of two rows with the same event_id the one
    /// with the smaller ingest_ts stays, the other gets its `tsv` cleared.
    unsafe fn resolve(&mut self, p1: &[P1]) {
        let tab = self.slots.as_mut_ptr();
        let mask = self.mask;
        for &(at, fp) in self.pending.iter() {
            let tag = fp >> 32;
            let mut i = at as usize;
            loop {
                let s = *tab.add(i);
                if s == 0 {
                    *tab.add(i) = fp;
                    break;
                }
                if s >> 32 == tag {
                    let a = row_ptr(p1, s as u32);
                    let b = row_ptr(p1, fp as u32);
                    if (*a).h2 == (*b).h2 {
                        if (*b).ing < (*a).ing {
                            (*a).tsv = 0;
                            *tab.add(i) = fp;
                        } else {
                            (*b).tsv = 0;
                        }
                        break;
                    }
                }
                i = (i + 1) & mask;
            }
        }
    }
}

unsafe fn phase2_dedup(sh: &Shared, p1: &[P1], next: &AtomicUsize) {
    let pe = 1usize << sh.pe_bits;
    let mut table = DedupTable { slots: Vec::new(), mask: 0, pending: Vec::new() };
    let mut buckets: Vec<Vec<u64>> = Vec::new();
    loop {
        let item = next.fetch_add(1, Ordering::Relaxed);
        if item >= pe {
            break;
        }
        let total: usize = p1.iter().map(|t| t.fp.count(item)).sum();
        if total == 0 {
            continue;
        }
        if total <= sh.split_rows {
            table.reset(total);
            for t in p1 {
                t.fp.for_each(item, |q| table.insert(p1, *q));
            }
            table.resolve(p1);
            continue;
        }
        // A big partition: split it on the top bits of the tag (which neither
        // the partition nor the table index uses) so each table stays in cache.
        let mut bits = 1u32;
        while bits < 6 && (total >> bits) > sh.split_rows {
            bits += 1;
        }
        let nb = 1usize << bits;
        if buckets.len() < nb {
            buckets.resize_with(nb, Vec::new);
        }
        for b in buckets[..nb].iter_mut() {
            b.clear();
            b.reserve(total);
        }
        let bp = buckets.as_mut_ptr();
        for t in p1 {
            t.fp.for_each(item, |q| {
                let fp = *q;
                let b = &mut *bp.add((fp >> (64 - bits)) as usize);
                let n = b.len();
                *b.as_mut_ptr().add(n) = fp;
                b.set_len(n + 1);
            });
        }
        for b in buckets[..nb].iter() {
            if !b.is_empty() {
                table.reset(b.len());
                for &fp in b.iter() {
                    table.insert(p1, fp);
                }
                table.resolve(p1);
            }
        }
    }
}

#[inline(always)]
unsafe fn prefetch(p: *const u8) {
    std::arch::x86_64::_mm_prefetch::<{ std::arch::x86_64::_MM_HINT_T0 }>(p as *const i8);
}

/// Phase 2b: scatter the surviving events (and purchases) by person.
unsafe fn phase2_scatter(sh: &Shared, p1: &[P1], units: &[(u32, u32)], next: &AtomicUsize, tid: usize) -> P2 {
    let mut arena = Arena::new();
    let mut ev: Wc<PEv> = Wc::new(sh.np, &mut arena);
    let pu = Wc::new(sh.np, &mut arena);
    // The fingerprint blocks of phase 1 are finished with: write the person records into them.
    ev.spare = p1[tid].fp.blocks.iter().map(|b| b.ptr).collect();
    let mut out = P2 { arena, ev, pu };
    loop {
        let item = next.fetch_add(1, Ordering::Relaxed);
        if item >= units.len() {
            break;
        }
        let (t, c) = units[item];
        let src = &p1[t as usize];
        let (base, used) = src.rows.chunks[c as usize];
        let used = used as usize;
        let first = c << ROW_BITS;
        let pur = &src.pur;
        let mut pcur = pur.partition_point(|x| x.0 < first);
        for i in 0..used {
            if i + 16 < used {
                sh.intern.prefetch_person((*base.add(i + 16)).uidx);
            }
            if i % 4 == 0 {
                // One page ahead in the row store.
                prefetch(base.add(i + 256) as *const u8);
            }
            let r = *base.add(i);
            let is_pur = pcur < pur.len() && pur.get_unchecked(pcur).0 == first + i as u32;
            if r.tsv & VALID != 0 {
                let person = sh.intern.person(r.uidx);
                let part = (person >> PSHIFT) as usize;
                let ts = r.tsv & !VALID;
                out.ev.push(&mut out.arena, part, PEv { person, ts });
                if is_pur {
                    out.pu.push(&mut out.arena, part, PPu { person, ts, pc: pur.get_unchecked(pcur).1, pad: 0 });
                }
            }
            pcur += is_pur as usize;
        }
    }
    out.ev.finish(&mut out.arena);
    out.pu.finish(&mut out.arena);
    out
}

// ---------------------------------------------------------------------------
// Phase 3
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct ClS {
    ts: u32,
    camp: u32,
    idref: u64,
}

struct UserCols {
    rows: usize,
    uid: Vec<u8>,
    day: Vec<u8>,
    sessions: Vec<u8>,
    events: Vec<u8>,
    revenue: Vec<u8>,
    cat: Vec<u8>,
    cat_valid: Vec<u8>,
}

/// Totals per campaign and day. The two counters every attributed session
/// touches are kept apart from the two only purchases touch, so the busy
/// part stays small.
struct Agg {
    /// [sessions, assisted sessions].
    hot: Vec<[u32; 2]>,
    /// (revenue, purchasing sessions).
    cold: Vec<(u64, u32)>,
}

struct P3 {
    rows: usize,
    agg: Agg,
    /// Seconds spent in: grouping, sorting, sessions, writing.
    tm: [f64; 4],
}

#[inline(always)]
unsafe fn click_id<'a>(c: &ClS) -> &'a [u8] {
    id_bytes(c.idref)
}

unsafe fn phase3(
    sh: &Shared,
    p1: &[P1],
    p2: &[P2],
    next: &AtomicUsize,
    pp: Params,
    tid: usize,
    out_dir: &Path,
) -> P3 {
    let l = 1usize << PSHIFT;
    let mut cnt = vec![0u32; l + 1];
    let mut pos = vec![0u32; l + 1];
    let mut ccnt = vec![0u32; l + 1];
    let mut cpos = vec![0u32; l + 1];
    let mut tsbuf: Vec<u32> = Vec::new();
    let mut cbuf: Vec<ClS> = Vec::new();
    let mut pus: Vec<(u64, u32)> = Vec::new();
    let ncamp = sh.camp_names.len();
    let ndays = sh.ndays;
    let ncat = sh.cat_names.len();
    let cells = (ncamp + 1) * ndays;
    let mut agg = Agg { hot: vec![[0u32; 2]; cells], cold: vec![(0u64, 0u32); cells] };
    let mut wcnt = vec![0u32; ncamp + 1];
    let mut wpos = vec![0u32; ncamp + 1];
    let mut active: Vec<u32> = Vec::with_capacity(ncamp + 1);
    let mut catsum = vec![0u64; ncat];
    let mut radix_tmp: Vec<u32> = Vec::new();
    let mut out = UserCols {
        rows: 0,
        uid: Vec::new(),
        day: Vec::new(),
        sessions: Vec::new(),
        events: Vec::new(),
        revenue: Vec::new(),
        cat: Vec::new(),
        cat_valid: Vec::new(),
    };
    let week = WEEK_MS as u64;
    let mut names = vec![0u64; l];
    let mut tm = [0.0f64; 4];
    loop {
        let q = next.fetch_add(1, Ordering::Relaxed);
        if q >= pp.np_used {
            break;
        }
        let n_ev: usize = p2.iter().map(|t| t.ev.count(q)).sum();
        if n_ev == 0 {
            continue;
        }
        let base = (q << PSHIFT) as u32;
        let t_a = Instant::now();

        // Group event times by person with a counting sort.
        cnt.fill(0);
        for t in p2 {
            t.ev.for_each(q, |r| {
                *cnt.get_unchecked_mut(((*r).person - base) as usize + 1) += 1;
            });
        }
        let mut acc = 0u32;
        for c in cnt.iter_mut() {
            acc += *c;
            *c = acc;
        }
        pos.copy_from_slice(&cnt);
        tsbuf.clear();
        tsbuf.reserve(n_ev);
        tsbuf.set_len(n_ev);
        for t in p2 {
            t.ev.for_each(q, |r| {
                let li = ((*r).person - base) as usize;
                let at = *pos.get_unchecked(li);
                *tsbuf.get_unchecked_mut(at as usize) = (*r).ts;
                *pos.get_unchecked_mut(li) = at + 1;
            });
        }

        pus.clear();
        for t in p2 {
            t.pu.for_each(q, |r| {
                pus.push(((((*r).person - base) as u64) << 32 | (*r).ts as u64, (*r).pc));
            });
        }
        if pus.len() > 1 {
            pus.sort_unstable_by_key(|x| x.0);
        }

        let n_cl: usize = p1.iter().map(|t| t.cl.count(q)).sum();
        if n_cl > 0 {
            ccnt.fill(0);
            for t in p1 {
                t.cl.for_each(q, |r| {
                    *ccnt.get_unchecked_mut(((*r).lc >> CAMP_BITS) as usize + 1) += 1;
                });
            }
            let mut acc = 0u32;
            for c in ccnt.iter_mut() {
                acc += *c;
                *c = acc;
            }
            cpos.copy_from_slice(&ccnt);
            cbuf.clear();
            cbuf.reserve(n_cl);
            cbuf.set_len(n_cl);
            for t in p1 {
                t.cl.for_each(q, |r| {
                    let li = ((*r).lc >> CAMP_BITS) as usize;
                    let at = *cpos.get_unchecked(li);
                    *cbuf.get_unchecked_mut(at as usize) =
                        ClS { ts: (*r).ts, camp: (*r).lc & CAMP_MASK, idref: (*r).idref };
                    *cpos.get_unchecked_mut(li) = at + 1;
                });
            }
        }

        let t_b = Instant::now();
        // Sort every person's events and clicks by time, and find their names.
        for i in 0..l {
            let a = cnt[i] as usize;
            let b = cnt[i + 1] as usize;
            if a == b {
                continue;
            }
            let evs = &mut tsbuf[a..b];
            if evs.len() > 1 {
                sort_times(evs, &mut radix_tmp);
            }
            if n_cl > 0 {
                let cls = &mut cbuf[ccnt[i] as usize..ccnt[i + 1] as usize];
                if cls.len() > 1 {
                    cls.sort_unstable_by_key(|c| c.ts);
                }
            }
            let root = base + i as u32;
            let ne = *sh.intern.name_enc.add(root as usize);
            let r = *sh.intern.id_ref.add(if ne == 0 { root } else { ne - 1 } as usize);
            names[i] = r;
            std::arch::x86_64::_mm_prefetch::<{ std::arch::x86_64::_MM_HINT_T0 }>(
                (r & 0x0000_ffff_ffff_ffff) as *const i8,
            );
        }
        let t_c = Instant::now();

        let mut pcur = 0usize;
        for i in 0..l {
            let a = cnt[i] as usize;
            let b = cnt[i + 1] as usize;
            if a == b {
                continue;
            }
            let evs = &tsbuf[a..b];
            let cls: &[ClS] = if n_cl > 0 { &cbuf[ccnt[i] as usize..ccnt[i + 1] as usize] } else { &[] };
            let name = id_bytes(names[i]);
            let pkey = (i as u64) << 32;

            let mut cur_day = usize::MAX;
            let mut d_sessions = 0u32;
            let mut d_events = 0u32;
            let mut d_rev = 0u64;
            let mut d_has = false;
            let mut lo = 0usize;
            let mut hi = 0usize;

            let mut k = 0usize;
            let nev = evs.len();
            while k < nev {
                // One session: events k..j.
                let start = evs[k];
                let mut prev = start;
                let mut j = k + 1;
                while j < nev && evs[j] - prev <= GAP_MS {
                    prev = evs[j];
                    j += 1;
                }
                let n_events = (j - k) as u32;
                k = j;

                let d = ((sh.ws_mod_day + start as u64) / DAY_MS as u64) as usize;
                if d != cur_day {
                    if d_sessions > 0 {
                        emit_row(sh, &mut out, name, cur_day, d_sessions, d_events, d_rev, d_has, &mut catsum);
                    }
                    cur_day = d;
                    d_sessions = 0;
                    d_events = 0;
                    d_rev = 0;
                    d_has = false;
                }

                // Purchases in the session: those of this person up to its last event.
                let mut rev = 0u64;
                let mut has = false;
                let last_key = pkey | prev as u64;
                while pcur < pus.len() && pus[pcur].0 <= last_key {
                    let pc = pus[pcur].1;
                    let price = (pc & 0x00ff_ffff) as u64;
                    rev += price;
                    *catsum.get_unchecked_mut((pc >> 24) as usize) += price;
                    has = true;
                    pcur += 1;
                }
                d_sessions += 1;
                d_events += n_events;
                d_rev += rev;
                d_has |= has;

                // Clicks from the 7 days up to the start, as a sliding window.
                let cell;
                if cls.is_empty() {
                    cell = ncamp * ndays + d;
                } else {
                    let s = start as u64;
                    while hi < cls.len() && cls[hi].ts as u64 <= s + week {
                        let c = cls[hi].camp as usize;
                        if wcnt[c] == 0 {
                            wpos[c] = active.len() as u32;
                            active.push(c as u32);
                        }
                        wcnt[c] += 1;
                        hi += 1;
                    }
                    while lo < hi && (cls[lo].ts as u64) < s {
                        let c = cls[lo].camp as usize;
                        wcnt[c] -= 1;
                        if wcnt[c] == 0 {
                            let at = wpos[c] as usize;
                            let moved = *active.last().unwrap();
                            active[at] = moved;
                            wpos[moved as usize] = at as u32;
                            active.pop();
                        }
                        lo += 1;
                    }
                    if hi > lo {
                        // Latest click; on a tie the byte-wise smallest click_id.
                        let mut best = hi - 1;
                        let top = cls[best].ts;
                        let mut x = hi - 1;
                        while x > lo && cls[x - 1].ts == top {
                            x -= 1;
                            if cls[x].camp != cls[best].camp && click_id(&cls[x]) < click_id(&cls[best]) {
                                best = x;
                            } else if cls[x].camp == cls[best].camp {
                                // Same campaign either way; keep the smaller ID for later comparisons.
                                if click_id(&cls[x]) < click_id(&cls[best]) {
                                    best = x;
                                }
                            }
                        }
                        cell = cls[best].camp as usize * ndays + d;
                        for &c in active.iter() {
                            agg.hot.get_unchecked_mut(c as usize * ndays + d)[1] += 1;
                        }
                    } else {
                        cell = ncamp * ndays + d;
                    }
                }
                agg.hot.get_unchecked_mut(cell)[0] += 1;
                if has {
                    let c = agg.cold.get_unchecked_mut(cell);
                    c.0 += rev;
                    c.1 += 1;
                }
            }
            if d_sessions > 0 {
                emit_row(sh, &mut out, name, cur_day, d_sessions, d_events, d_rev, d_has, &mut catsum);
            }
            for &c in active.iter() {
                wcnt[c as usize] = 0;
            }
            active.clear();
        }
        let t_d = Instant::now();
        tm[0] += (t_b - t_a).as_secs_f64();
        tm[1] += (t_c - t_b).as_secs_f64();
        tm[2] += (t_d - t_c).as_secs_f64();
    }

    let t_w = Instant::now();
    if out.rows > 0 {
        let path = out_dir.join("user_daily").join(format!("part-{tid:03}.parquet"));
        write_user_daily(&path, &out);
    }
    tm[3] = t_w.elapsed().as_secs_f64();
    P3 { rows: out.rows, agg, tm }
}

/// Sorts event times: a three-pass radix sort for large groups (whales and
/// shared devices), the standard sort for the rest.
#[inline(always)]
fn sort_times(v: &mut [u32], tmp: &mut Vec<u32>) {
    if v.len() < 2048 {
        v.sort_unstable();
        return;
    }
    radix_sort(v, tmp);
}

#[inline(never)]
fn radix_sort(v: &mut [u32], tmp: &mut Vec<u32>) {
    let n = v.len();
    tmp.clear();
    tmp.resize(n, 0);
    let mut c0 = [0u32; 2048];
    let mut c1 = [0u32; 2048];
    let mut c2 = [0u32; 1024];
    for &x in v.iter() {
        c0[(x & 2047) as usize] += 1;
        c1[((x >> 11) & 2047) as usize] += 1;
        c2[(x >> 22) as usize] += 1;
    }
    let prefix = |c: &mut [u32]| {
        let mut acc = 0u32;
        for e in c.iter_mut() {
            let t = *e;
            *e = acc;
            acc += t;
        }
    };
    prefix(&mut c0);
    prefix(&mut c1);
    prefix(&mut c2);
    for &x in v.iter() {
        let b = (x & 2047) as usize;
        tmp[c0[b] as usize] = x;
        c0[b] += 1;
    }
    for &x in tmp.iter() {
        let b = ((x >> 11) & 2047) as usize;
        v[c1[b] as usize] = x;
        c1[b] += 1;
    }
    for &x in v.iter() {
        let b = (x >> 22) as usize;
        tmp[c2[b] as usize] = x;
        c2[b] += 1;
    }
    v.copy_from_slice(tmp);
}

#[inline(always)]
fn emit_row(
    sh: &Shared,
    out: &mut UserCols,
    name: &[u8],
    day: usize,
    sessions: u32,
    events: u32,
    revenue: u64,
    has: bool,
    catsum: &mut [u64],
) {
    out.uid.extend_from_slice(&(name.len() as u32).to_le_bytes());
    out.uid.extend_from_slice(name);
    out.day.extend_from_slice(&(sh.day0 + day as i32).to_le_bytes());
    out.sessions.extend_from_slice(&(sessions as i32).to_le_bytes());
    out.events.extend_from_slice(&(events as i32).to_le_bytes());
    out.revenue.extend_from_slice(&(revenue as i64).to_le_bytes());
    if out.rows % 8 == 0 {
        out.cat_valid.push(0);
    }
    if has {
        // Categories are numbered in byte order, so the first maximum wins ties.
        let mut best = 0usize;
        let mut best_sum = 0u64;
        for (c, s) in catsum.iter_mut().enumerate() {
            if *s > best_sum {
                best_sum = *s;
                best = c;
            }
            *s = 0;
        }
        let cat = &sh.cat_names[best];
        out.cat.extend_from_slice(&(cat.len() as u32).to_le_bytes());
        out.cat.extend_from_slice(cat);
        *out.cat_valid.last_mut().unwrap() |= 1 << (out.rows % 8);
    }
    out.rows += 1;
}

fn write_user_daily(path: &Path, out: &UserCols) {
    use parquet::{Column, Kind};
    let cols = [
        Column { name: "user_id", kind: Kind::Str, data: &out.uid, validity: None },
        Column { name: "day", kind: Kind::Date, data: &out.day, validity: None },
        Column { name: "sessions", kind: Kind::I32, data: &out.sessions, validity: None },
        Column { name: "events", kind: Kind::I32, data: &out.events, validity: None },
        Column { name: "revenue_cents", kind: Kind::I64, data: &out.revenue, validity: None },
        Column { name: "top_category", kind: Kind::Str, data: &out.cat, validity: Some(&out.cat_valid) },
    ];
    if let Err(e) = parquet::write_file(path, out.rows, &cols) {
        sys::die(&format!("write {}: {e}", path.display()));
    }
}

// ---------------------------------------------------------------------------
// Setup
// ---------------------------------------------------------------------------

fn list_csv(dir: &Path) -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let name = e.file_name();
            let name = name.to_string_lossy();
            if name.ends_with(".csv") && !name.starts_with('.') {
                let p = e.path();
                if p.is_file() {
                    v.push(p);
                }
            }
        }
    }
    v.sort();
    v
}

/// The lines of a small two-column lookup file, header skipped, as byte slices.
unsafe fn lookup_rows<'a>(m: &'a Mapped) -> Vec<(&'a [u8], &'a [u8])> {
    let mut rows = Vec::new();
    if m.len == 0 {
        return rows;
    }
    let h = scan::find_newline(m.ptr, 0, m.len);
    if h >= m.len {
        return rows;
    }
    let text = m.ptr.add(h + 1);
    let len = m.len - h - 1;
    if len == 0 {
        return rows;
    }
    let mut sb = scan::ScanBuf::new();
    scan::scan(text, len, &mut sb);
    let mut it = LineIter::new();
    let mut fs = [0u32; 8];
    let mut fe = [0u32; 8];
    while it.line < sb.lines {
        if !it.next::<2>(&sb, &mut fs, &mut fe) {
            continue;
        }
        rows.push((
            std::slice::from_raw_parts(text.add(fs[0] as usize), (fe[0] - fs[0]) as usize),
            std::slice::from_raw_parts(text.add(fs[1] as usize), (fe[1] - fs[1]) as usize),
        ));
    }
    rows
}

fn manifest_time(text: &[u8], key: &[u8]) -> i64 {
    let at = match text.windows(key.len()).position(|w| w == key) {
        Some(a) => a + key.len(),
        None => sys::die("manifest.json: missing window key"),
    };
    let rest = &text[at..];
    // Skip the key's closing quote, the colon, and find the value's opening quote.
    let mut i = 0;
    let mut quotes = 0;
    while i < rest.len() {
        if rest[i] == b'"' {
            quotes += 1;
            if quotes == 2 {
                break;
            }
        }
        i += 1;
    }
    let start = i + 1;
    let mut end = start;
    while end < rest.len() && rest[end] != b'"' {
        end += 1;
    }
    let mut padded = rest[start..end.min(rest.len())].to_vec();
    let len = padded.len();
    padded.extend_from_slice(&[0u8; 32]);
    let t = unsafe { ts::parse_ts(padded.as_ptr(), len, &mut ts::DateCache::new()) };
    if t == BAD {
        sys::die("manifest.json: bad window timestamp");
    }
    t
}

/// A number from the environment, for tuning runs.
fn env_num(name: &str, default: usize) -> usize {
    std::env::var(name).ok().and_then(|v| v.parse::<usize>().ok()).unwrap_or(default)
}

fn main() {
    let t0 = Instant::now();
    let args: Vec<String> = std::env::args().collect();
    let env = |k: &str, d: &str| std::env::var(k).unwrap_or_else(|_| d.to_string());
    let data_dir = PathBuf::from(if args.len() > 2 { args[1].clone() } else { env("DATA_DIR", "/data") });
    let out_dir = PathBuf::from(if args.len() > 2 { args[2].clone() } else { env("OUTPUT_DIR", "/output") });
    let timing = std::env::var("PIPE_TIMING").is_ok();
    let threads = std::env::var("PIPE_THREADS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(8, usize::from))
        .clamp(1, MAX_THREADS);
    if std::env::var("PIPE_TOUCH").is_ok() {
        sys::set_touch_only(true);
    }
    if std::env::var("PIPE_POPULATE").is_ok() {
        sys::set_populate(true);
    }
    if std::env::var("PIPE_NO_HUGE").is_ok() {
        sys::set_huge(false);
    }

    let _ = std::fs::create_dir_all(out_dir.join("user_daily"));
    let _ = std::fs::create_dir_all(out_dir.join("campaign_daily"));

    // Window.
    let manifest = std::fs::read(data_dir.join("manifest.json")).unwrap_or_else(|e| sys::die(&format!("manifest.json: {e}")));
    let ws = manifest_time(&manifest, b"window_start");
    let we = manifest_time(&manifest, b"window_end");
    let cs = ws - WEEK_MS;
    if we - cs >= (1i64 << 32) || we - ws >= (1i64 << 31) {
        sys::die("window longer than this build supports");
    }
    let day0 = ws.div_euclid(DAY_MS);
    let ws_mod_day = ws.rem_euclid(DAY_MS) as u64;
    let ndays = if we > ws { ((ws_mod_day + (we - ws) as u64 - 1) / DAY_MS as u64) as usize + 1 } else { 1 };

    // Lookups.
    let products_map = sys::map_file(&data_dir.join("products.csv"));
    let campaigns_map = sys::map_file(&data_dir.join("campaigns.csv"));
    let product_rows = unsafe { lookup_rows(&products_map) };
    let campaign_rows = unsafe { lookup_rows(&campaigns_map) };
    // The distinct categories (a handful) plus "unknown", numbered in byte order.
    let mut cat_names: Vec<Vec<u8>> = vec![b"unknown".to_vec()];
    let mut seen = StaticTable::new(256);
    for (_, cat) in &product_rows {
        let h = unsafe { hash128(cat.as_ptr(), cat.len()) };
        if seen.get(h).is_none() {
            seen.insert(h, 0);
            if !cat_names.iter().any(|c| c.as_slice() == *cat) {
                cat_names.push(cat.to_vec());
            }
            if cat_names.len() > 256 {
                sys::die("more than 255 categories");
            }
        }
    }
    cat_names.sort();
    let unknown_cat = cat_names.iter().position(|c| c.as_slice() == b"unknown").unwrap() as u32;
    let mut cat_rank = StaticTable::new(256);
    for (i, c) in cat_names.iter().enumerate() {
        // Hashing reads up to 16 bytes past the name: give it room.
        let mut padded = c.clone();
        padded.extend_from_slice(&[0u8; 64]);
        cat_rank.insert(unsafe { hash128(padded.as_ptr(), c.len()) }, i as u32);
    }
    let products =
        Products::new(&product_rows, |cat| cat_rank.get(unsafe { hash128(cat.as_ptr(), cat.len()) }).unwrap());
    let mut campaigns = StaticTable::new(campaign_rows.len());
    let mut camp_names = Vec::with_capacity(campaign_rows.len());
    let mut camp_channels = Vec::with_capacity(campaign_rows.len());
    for (id, channel) in &campaign_rows {
        let h = unsafe { hash128(id.as_ptr(), id.len()) };
        if campaigns.get(h).is_none() {
            campaigns.insert(h, camp_names.len() as u32);
            camp_names.push(id.to_vec());
            camp_channels.push(channel.to_vec());
        }
    }

    // Input files and chunks.
    let mut files: Vec<Mapped> = Vec::new();
    let mut chunks: Vec<Chunk> = Vec::new();
    let mut bytes = [0usize; 3];
    for (kind, dir) in [(KIND_LINKS, "links"), (KIND_EVENTS, "events"), (KIND_CLICKS, "clicks")] {
        let mut list: Vec<Mapped> = list_csv(&data_dir.join(dir)).iter().map(|p| sys::map_file(p)).collect();
        // Largest first, so the tail of the queue is small work.
        list.sort_by(|a, b| b.len.cmp(&a.len));
        for m in list {
            bytes[kind as usize] += m.len;
            let n = m.len.div_ceil(CHUNK);
            for k in 0..n {
                chunks.push(Chunk { file: files.len() as u32, kind, k: k as u32 });
            }
            files.push(m);
        }
    }
    let n_link_chunks = chunks.iter().filter(|c| c.kind == KIND_LINKS).count();
    if camp_names.len() > CAMP_MASK as usize {
        sys::die("more campaigns than this build supports");
    }

    // Event partitions: about 16k rows each.
    let est_rows = bytes[KIND_EVENTS as usize] / 80;
    let pe_max = env_num("PIPE_PE_BITS", 11).min(13) as u32;
    let mut pe_bits = 0u32;
    while pe_bits < pe_max && (est_rows >> pe_bits) > 16_384 {
        pe_bits += 1;
    }

    // About 1 distinct user ID per 1,200 input bytes in generated data; the
    // table gets four or more times that, and is rebuilt larger if it fills.
    let mut cap = ((bytes[KIND_EVENTS as usize] + bytes[KIND_CLICKS as usize]) / 700 + bytes[KIND_LINKS as usize] / 64)
        .next_power_of_two()
        .max(1 << 18);
    if let Some(v) = std::env::var("PIPE_CAP_BITS").ok().and_then(|v| v.parse::<u32>().ok()) {
        // A test switch for the rebuild path. Below 2^15 slots the ID limit
        // would not be a whole number of person partitions.
        cap = 1usize << v.clamp(15, 34);
    }

    let t_setup = t0.elapsed();

    let mut sh = Shared {
        ws,
        we,
        cs,
        ing_base: ws / 1000 - (1i64 << 31),
        ws_mod_day,
        day0: day0 as i32,
        ndays,
        products,
        cat_names,
        unknown_cat,
        campaigns,
        camp_names,
        camp_channels,
        files,
        chunks,
        n_link_chunks,
        next_chunk: AtomicUsize::new(0),
        links_done: AtomicUsize::new(0),
        link_pairs: Mutex::new(Vec::new()),
        links_ready: AtomicBool::new(n_link_chunks == 0),
        intern: unsafe { Intern::new(cap) },
        pe_bits,
        np: (cap / 8 * 5) >> PSHIFT,
        block: env_num("PIPE_BLOCK_KB", 32).clamp(4, 1024) << 10,
        split_rows: env_num("PIPE_SPLIT_ROWS", 16_384).max(16),
        pin: if std::env::var("PIPE_PIN").is_ok() { sys::allowed_cpus() } else { Vec::new() },
    };

    // Phase 1, repeated with a larger ID table in the unlikely case it fills.
    let mut p1: Vec<P1>;
    loop {
        let shr = &sh;
        p1 = std::thread::scope(|s| {
            let hs: Vec<_> = (0..threads)
                .map(|tid| {
                    s.spawn(move || {
                        shr.pin(tid);
                        unsafe { phase1(shr, tid, threads) }
                    })
                })
                .collect();
            hs.into_iter().map(|h| h.join().unwrap()).collect()
        });
        if !sh.intern.overflow.load(Ordering::Relaxed) {
            break;
        }
        for t in p1.iter_mut() {
            unsafe { t.arena.release_all() };
        }
        cap *= 4;
        unsafe {
            sh.intern.free();
            sh.intern = Intern::new(cap);
        }
        sh.np = (cap / 8 * 5) >> PSHIFT;
        sh.next_chunk.store(0, Ordering::Relaxed);
        sh.links_done.store(0, Ordering::Relaxed);
        sh.links_ready.store(sh.n_link_chunks == 0, Ordering::Relaxed);
        sh.link_pairs.lock().unwrap().clear();
    }
    let t_p1 = t0.elapsed();

    let users = sh.intern.next.load(Ordering::Relaxed).min(sh.intern.limit) as usize;
    let pp = Params { threads, np_used: users.div_ceil(1 << PSHIFT).min(sh.np) };

    let next = AtomicUsize::new(0);
    let shr = &sh;
    let p1r = &p1;
    std::thread::scope(|s| {
        let nextr = &next;
        for tid in 0..pp.threads {
            s.spawn(move || {
                shr.pin(tid);
                unsafe { phase2_dedup(shr, p1r, nextr) }
            });
        }
    });
    let t_p2a = t0.elapsed();
    let mut units: Vec<(u32, u32)> = Vec::new();
    for (t, th) in p1.iter().enumerate() {
        for c in 0..th.rows.chunks.len() {
            units.push((t as u32, c as u32));
        }
    }
    let next = AtomicUsize::new(0);
    let unitsr = &units;
    let p2: Vec<P2> = std::thread::scope(|s| {
        let nextr = &next;
        let hs: Vec<_> = (0..pp.threads)
            .map(|tid| {
                s.spawn(move || {
                    shr.pin(tid);
                    unsafe { phase2_scatter(shr, p1r, unitsr, nextr, tid) }
                })
            })
            .collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let t_p2 = t0.elapsed();

    let next = AtomicUsize::new(0);
    let p2r = &p2;
    let out_ref = &out_dir;
    let nextr = &next;
    let p3: Vec<P3> = std::thread::scope(|s| {
        let hs: Vec<_> = (0..pp.threads)
            .map(|tid| {
                s.spawn(move || {
                    shr.pin(tid);
                    unsafe { phase3(shr, p1r, p2r, nextr, pp, tid, out_ref) }
                })
            })
            .collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let t_p3 = t0.elapsed();

    // Give the big mappings back in parallel while the last file is written:
    // otherwise the kernel tears them down on one core as the process exits.
    let teardown = std::env::var("PIPE_NO_TEARDOWN").is_err();
    let tear_done = AtomicUsize::new(0);
    let tear_ref = &tear_done;
    std::thread::scope(|s| {
        if teardown {
            for tid in 0..pp.threads {
                s.spawn(move || unsafe {
                    p1r[tid].arena.unmap_all();
                    p2r[tid].arena.unmap_all();
                    for (j, f) in shr.files.iter().enumerate() {
                        if j % pp.threads == tid {
                            f.unmap();
                        }
                    }
                    if tid == 0 {
                        shr.intern.unmap();
                    }
                    tear_ref.fetch_add(1, Ordering::Relaxed);
                });
            }
        }
        finish_outputs(shr, &p3, out_ref);
    });
    let _ = tear_done.load(Ordering::Relaxed);

    if timing {
        eprintln!("phase2 dedup {:.1} ms, scatter {:.1} ms", (t_p2a - t_p1).as_secs_f64() * 1e3, (t_p2 - t_p2a).as_secs_f64() * 1e3);
        report(shr, &p1, &p3, threads, pp, cap, [t_setup, t_p1, t_p2, t_p3, t0.elapsed()]);
    }
    sys::exit_now(0);
}

/// Writes campaign_daily, and an empty user_daily file when there are no rows.
fn finish_outputs(sh: &Shared, p3: &[P3], out_dir: &Path) {
    let ndays = sh.ndays;
    // user_daily needs at least one file even with no rows.
    let total_rows: usize = p3.iter().map(|t| t.rows).sum();
    if total_rows == 0 {
        let empty = UserCols {
            rows: 0,
            uid: Vec::new(),
            day: Vec::new(),
            sessions: Vec::new(),
            events: Vec::new(),
            revenue: Vec::new(),
            cat: Vec::new(),
            cat_valid: Vec::new(),
        };
        write_user_daily(&out_dir.join("user_daily").join("part-000.parquet"), &empty);
    }

    // campaign_daily.
    let ncamp = sh.camp_names.len();
    let cells = (ncamp + 1) * ndays;
    let mut sessions = vec![0u64; cells];
    let mut purchasing = vec![0u64; cells];
    let mut assisted = vec![0u64; cells];
    let mut revenue = vec![0u64; cells];
    for t in p3 {
        for i in 0..cells {
            sessions[i] += t.agg.hot[i][0] as u64;
            assisted[i] += t.agg.hot[i][1] as u64;
            revenue[i] += t.agg.cold[i].0;
            purchasing[i] += t.agg.cold[i].1 as u64;
        }
    }
    let mut c_id = Vec::new();
    let mut c_ch = Vec::new();
    let mut c_day = Vec::new();
    let mut c_s = Vec::new();
    let mut c_p = Vec::new();
    let mut c_r = Vec::new();
    let mut c_a = Vec::new();
    let mut rows = 0usize;
    for c in 0..=ncamp {
        let (name, channel): (&[u8], &[u8]) =
            if c == ncamp { (b"organic", b"organic") } else { (&sh.camp_names[c], &sh.camp_channels[c]) };
        for d in 0..ndays {
            let i = c * ndays + d;
            if sessions[i] == 0 && assisted[i] == 0 {
                continue;
            }
            c_id.extend_from_slice(&(name.len() as u32).to_le_bytes());
            c_id.extend_from_slice(name);
            c_ch.extend_from_slice(&(channel.len() as u32).to_le_bytes());
            c_ch.extend_from_slice(channel);
            c_day.extend_from_slice(&(sh.day0 + d as i32).to_le_bytes());
            c_s.extend_from_slice(&(sessions[i] as i64).to_le_bytes());
            c_p.extend_from_slice(&(purchasing[i] as i64).to_le_bytes());
            c_r.extend_from_slice(&(revenue[i] as i64).to_le_bytes());
            c_a.extend_from_slice(&(assisted[i] as i64).to_le_bytes());
            rows += 1;
        }
    }
    {
        use parquet::{Column, Kind};
        let cols = [
            Column { name: "campaign_id", kind: Kind::Str, data: &c_id, validity: None },
            Column { name: "channel", kind: Kind::Str, data: &c_ch, validity: None },
            Column { name: "day", kind: Kind::Date, data: &c_day, validity: None },
            Column { name: "sessions", kind: Kind::I64, data: &c_s, validity: None },
            Column { name: "purchasing_sessions", kind: Kind::I64, data: &c_p, validity: None },
            Column { name: "revenue_cents", kind: Kind::I64, data: &c_r, validity: None },
            Column { name: "assisted_sessions", kind: Kind::I64, data: &c_a, validity: None },
        ];
        let path = out_dir.join("campaign_daily").join("part-000.parquet");
        if let Err(e) = parquet::write_file(&path, rows, &cols) {
            sys::die(&format!("write {}: {e}", path.display()));
        }
    }

}

fn report(sh: &Shared, p1: &[P1], p3: &[P3], threads: usize, pp: Params, cap: usize, t: [std::time::Duration; 5]) {
    let [t_setup, t_p1, t_p2, t_p3, t_end] = t;
    let total_rows: usize = p3.iter().map(|t| t.rows).sum();
    let users = sh.intern.next.load(Ordering::Relaxed);
    {
        let mut tm = [0.0f64; 7];
        for t in p1 {
            for i in 0..7 {
                tm[i] += t.tm[i];
            }
        }
        eprintln!(
            "phase1 cpu ms: scan {:.0} parse {:.0} intern {:.0} scatter {:.0} link {:.0} clicks {:.0} links {:.0}",
            tm[0] * 1e3, tm[1] * 1e3, tm[2] * 1e3, tm[3] * 1e3, tm[4] * 1e3, tm[5] * 1e3, tm[6] * 1e3
        );
        let mut t3 = [0.0f64; 4];
        for t in p3 {
            for i in 0..4 {
                t3[i] += t.tm[i];
            }
        }
        eprintln!(
            "phase3 cpu ms: group {:.0} sort {:.0} sessions {:.0} write {:.0}",
            t3[0] * 1e3, t3[1] * 1e3, t3[2] * 1e3, t3[3] * 1e3
        );
        eprintln!(
            "threads={threads} users={users} pe={} pp={} cap={cap} rows={total_rows}\nsetup {:.1} ms | phase1 {:.1} | phase2 {:.1} | phase3 {:.1} | finish {:.1} | total {:.1} ms",
            1usize << sh.pe_bits,
            pp.np_used,
            t_setup.as_secs_f64() * 1e3,
            (t_p1 - t_setup).as_secs_f64() * 1e3,
            (t_p2 - t_p1).as_secs_f64() * 1e3,
            (t_p3 - t_p2).as_secs_f64() * 1e3,
            (t_end - t_p3).as_secs_f64() * 1e3,
            t_end.as_secs_f64() * 1e3
        );
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    /// The CSV dialect by the book: the bounds of the `n` fields of one line
    /// (without its newline), quotes stripped, or `None` when it is malformed.
    fn split_reference(line: &[u8], n: usize) -> Option<Vec<(usize, usize)>> {
        let mut fields = Vec::new();
        let mut start = 0;
        for i in 0..=line.len() {
            if i == line.len() || line[i] == b',' {
                fields.push((start, i));
                start = i + 1;
            }
        }
        if fields.len() != n {
            return None;
        }
        let mut out = Vec::new();
        for (s, e) in fields {
            let f = &line[s..e];
            let quotes = f.iter().filter(|&&c| c == b'"').count();
            if quotes == 0 {
                out.push((s, e));
            } else if quotes == 2 && f.len() >= 2 && f[0] == b'"' && f[f.len() - 1] == b'"' {
                out.push((s + 1, e - 1));
            } else {
                return None;
            }
        }
        Some(out)
    }

    /// The direct product table must answer exactly like the hash table, for
    /// catalog IDs and for every near miss of one.
    #[test]
    fn product_lookup_matches_hash_table() {
        let mut seed = 0x1234_5678_9abc_def1u64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        // IDs live in one padded buffer, as they do in a mapped file.
        let mut store = vec![0u8; 1 << 23];
        let base = store.as_mut_ptr();
        let mut used = 64usize;
        let mut put = |bytes: &[u8]| -> (usize, usize) {
            let at = used;
            unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), base.add(at), bytes.len()) };
            used += bytes.len() + 24;
            (at, bytes.len())
        };
        let mut catalog = Vec::new();
        for _ in 0..40_000 {
            let n = next() % 60_000;
            catalog.push((put(format!("p{n}").as_bytes()), (next() % 7) as u32));
        }
        catalog.push((put(b"p0"), 3));
        let mut probes = Vec::new();
        for _ in 0..200_000 {
            let n = next() % 70_000;
            let text = match next() % 12 {
                0 => format!("p0{n}"),
                1 => format!("P{n}"),
                2 => format!(" p{n}"),
                3 => format!("p{n} "),
                4 => format!("p{n}\r"),
                5 => format!("q{n}"),
                6 => format!("p{}", n * 100_003),
                7 => format!("p{n}a"),
                8 => String::from("p"),
                9 => String::new(),
                _ => format!("p{n}"),
            };
            probes.push(put(text.as_bytes()));
        }
        probes.push(put("p１２".as_bytes()));
        probes.push(put(b"p-1"));
        probes.push(put(b"p+1"));
        probes.push(put(b"p00"));
        probes.push(put(b"p0"));
        let rows: Vec<(&[u8], &[u8])> = catalog
            .iter()
            .map(|&((at, len), _)| (unsafe { std::slice::from_raw_parts(base.add(at), len) }, &b"x"[..]))
            .collect();
        let cats: Vec<u32> = catalog.iter().map(|c| c.1).collect();
        let idx = std::cell::Cell::new(0usize);
        let products = Products::new(&rows, |_| {
            let i = idx.get();
            idx.set(i + 1);
            cats[i]
        });
        assert!(!products.direct.is_empty(), "the direct table should be in use");
        for &(at, len) in probes.iter().chain(catalog.iter().map(|c| &c.0)) {
            let p = unsafe { base.add(at) };
            let want = match products.table.get(unsafe { hash128(p, len) }) {
                Some(c) => c,
                None => 99,
            };
            let got = unsafe { products.get(p, len, 99) };
            assert_eq!(got, want, "{:?}", String::from_utf8_lossy(unsafe { std::slice::from_raw_parts(p, len) }));
        }
        // A catalog with one ID of another shape falls back to the hash table.
        let odd = put(b"sku-7");
        let mut rows2 = rows.clone();
        rows2.push((unsafe { std::slice::from_raw_parts(base.add(odd.0), odd.1) }, &b"x"[..]));
        let fallback = Products::new(&rows2, |_| 1);
        assert!(fallback.direct.is_empty());
        assert_eq!(unsafe { fallback.get(base.add(odd.0), odd.1, 99) }, 1);
        assert!(used < store.len());
    }

    /// Random blocks over a small alphabet: the scan and the splitter must
    /// agree with the reference on every line, for every field count in use,
    /// whatever bytes follow the block.
    #[test]
    fn splitter_matches_reference() {
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let mut sb = scan::ScanBuf::new();
        let mut checked = 0u64;
        for round in 0..400_000u32 {
            // Per block: how common each kind of byte is.
            let w_sep = 1 + next() % 40;
            let w_quote = next() % 30;
            let w_nl = 1 + next() % 12;
            let w_other = 1 + next() % 80;
            let total = w_sep + w_quote + w_nl + w_other;
            let len = if round % 64 == 0 { 2000 + (next() % 9000) as usize } else { (next() % 420) as usize };
            let lead = (next() % 67) as usize;
            let mut buf = vec![b'x'; lead + len + 1 + 200];
            for b in buf.iter_mut() {
                let r = next() % total;
                *b = if r < w_sep {
                    b','
                } else if r < w_sep + w_quote {
                    b'"'
                } else if r < w_sep + w_quote + w_nl {
                    b'\n'
                } else if r % 7 == 0 {
                    b'\r'
                } else {
                    b'a'
                };
            }
            buf[lead + len] = b'\n';
            let text = &buf[lead..lead + len + 1];
            unsafe { scan::scan(text.as_ptr(), text.len(), &mut sb) };
            let line_count = text.iter().filter(|&&c| c == b'\n').count();
            assert_eq!(sb.lines, line_count, "line count, round {round}");
            for n in [2usize, 4, 7] {
                let mut it = LineIter::new();
                let mut fs = [0u32; 8];
                let mut fe = [0u32; 8];
                let mut line_start = 0usize;
                for (i, &c) in text.iter().enumerate() {
                    if c != b'\n' {
                        continue;
                    }
                    let line = &text[line_start..i];
                    assert!(it.line < sb.lines);
                    let ok = match n {
                        2 => unsafe { it.next::<2>(&sb, &mut fs, &mut fe) },
                        4 => unsafe { it.next::<4>(&sb, &mut fs, &mut fe) },
                        _ => unsafe { it.next::<7>(&sb, &mut fs, &mut fe) },
                    };
                    let want = split_reference(line, n);
                    assert_eq!(ok, want.is_some(), "round {round} n {n} line {:?}", String::from_utf8_lossy(line));
                    if let Some(want) = want {
                        for j in 0..n {
                            assert_eq!(
                                (fs[j] as usize, fe[j] as usize),
                                (line_start + want[j].0, line_start + want[j].1),
                                "round {round} n {n} field {j} line {:?}",
                                String::from_utf8_lossy(line)
                            );
                        }
                        checked += 1;
                    }
                    line_start = i + 1;
                }
                assert_eq!(it.line, sb.lines);
            }
        }
        assert!(checked > 100_000, "too few well-formed lines were checked: {checked}");
    }
}
