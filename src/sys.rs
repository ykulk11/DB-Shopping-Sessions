//! Thin wrappers over the few system calls the pipeline needs: file mapping,
//! anonymous memory, and a bump arena per thread.

use std::ffi::c_void;
use std::fs::File;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::path::Path;
use std::ptr::null_mut;

extern "C" {
    fn mmap(addr: *mut c_void, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut c_void;
    fn munmap(addr: *mut c_void, len: usize) -> i32;
    fn madvise(addr: *mut c_void, len: usize, advice: i32) -> i32;
    fn _exit(code: i32) -> !;
}

const PROT_READ: i32 = 1;
const PROT_WRITE: i32 = 2;
const MAP_PRIVATE: i32 = 2;
const MAP_FIXED: i32 = 0x10;
const MAP_ANONYMOUS: i32 = 0x20;
const MAP_NORESERVE: i32 = 0x4000;
const MADV_HUGEPAGE: i32 = 14;
const MAP_POPULATE: i32 = 0x8000;

static mut POPULATE: bool = false;

pub fn set_populate(on: bool) {
    unsafe { POPULATE = on }
}

pub const PAGE: usize = 4096;
const HUGE: usize = 2 << 20;

pub fn exit_now(code: i32) -> ! {
    unsafe { _exit(code) }
}

pub fn die(msg: &str) -> ! {
    eprintln!("pipeline: {msg}");
    exit_now(1)
}

#[inline]
fn round_up(x: usize, to: usize) -> usize {
    (x + to - 1) & !(to - 1)
}

/// Zero-filled, lazily committed memory.
pub unsafe fn anon(len: usize) -> *mut u8 {
    let p = mmap(
        null_mut(),
        len,
        PROT_READ | PROT_WRITE,
        MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE,
        -1,
        0,
    );
    if p as isize == -1 {
        die("mmap(anonymous) failed");
    }
    p as *mut u8
}

/// Zero-filled memory aligned to 2 MB and marked for transparent huge pages.
pub unsafe fn anon_huge(len: usize) -> *mut u8 {
    let len = round_up(len, HUGE);
    let raw = anon(len + HUGE);
    let p = round_up(raw as usize, HUGE) as *mut u8;
    if huge_enabled() {
        madvise(p as *mut c_void, len, MADV_HUGEPAGE);
    }
    p
}

pub unsafe fn release(p: *mut u8, len: usize) {
    munmap(p as *mut c_void, len);
}

static mut TOUCH_ONLY: bool = false;

/// For testing the path taken on kernels without MADV_POPULATE_WRITE.
pub fn set_touch_only(on: bool) {
    unsafe { TOUCH_ONLY = on }
}

/// Faults in `[p, p + len)` for writing, one page at a time in the kernel,
/// so later reads and writes take no page fault. Safe while other threads
/// use the memory.
pub unsafe fn populate_write(p: *mut u8, len: usize) {
    const MADV_POPULATE_WRITE: i32 = 23;
    if len == 0 {
        return;
    }
    if TOUCH_ONLY || madvise(p as *mut c_void, len, MADV_POPULATE_WRITE) != 0 {
        // Older kernels: an atomic no-op write per page does the same.
        use std::sync::atomic::{AtomicU8, Ordering};
        let mut at = 0;
        while at < len {
            (*(p.add(at) as *const AtomicU8)).fetch_or(0, Ordering::Relaxed);
            at += PAGE;
        }
    }
}

impl Mapped {
    /// Unmaps the file's pages (the caller must not touch them afterwards).
    pub unsafe fn unmap(&self) {
        if self.len > 0 {
            let start = (self.ptr as usize) & !(PAGE - 1);
            let end = round_up(self.ptr as usize + self.len, PAGE);
            munmap(start as *mut c_void, end - start);
        }
    }
}

static mut HUGE_ON: bool = true;

pub fn set_huge(on: bool) {
    unsafe { HUGE_ON = on }
}

fn huge_enabled() -> bool {
    unsafe { HUGE_ON }
}

/// A whole input file in memory. At least one page of readable zero bytes
/// precedes and follows the data, so vector loads may overrun either end, and
/// the data always ends with `\n` (one is appended when the file lacks it).
pub struct Mapped {
    pub ptr: *const u8,
    pub len: usize,
}

unsafe impl Send for Mapped {}
unsafe impl Sync for Mapped {}

pub fn map_file(path: &Path) -> Mapped {
    let mut file = match File::open(path) {
        Ok(f) => f,
        Err(e) => die(&format!("open {}: {e}", path.display())),
    };
    let len = match file.metadata() {
        Ok(m) => m.len() as usize,
        Err(e) => die(&format!("stat {}: {e}", path.display())),
    };
    if len == 0 {
        return Mapped { ptr: unsafe { anon(2 * PAGE).add(PAGE) }, len: 0 };
    }
    unsafe {
        let body = round_up(len + 1, PAGE);
        let base = anon(PAGE + body + PAGE);
        let p = base.add(PAGE);
        let r = mmap(
            p as *mut c_void,
            len,
            PROT_READ | PROT_WRITE,
            MAP_PRIVATE | MAP_FIXED | if POPULATE { MAP_POPULATE } else { 0 },
            file.as_raw_fd(),
            0,
        );
        if r as isize == -1 {
            // The filesystem cannot map files: put the anonymous pages back and read.
            let again = mmap(
                p as *mut c_void,
                body,
                PROT_READ | PROT_WRITE,
                MAP_PRIVATE | MAP_FIXED | MAP_ANONYMOUS | MAP_NORESERVE,
                -1,
                0,
            );
            if again as isize == -1 {
                die("mmap(fixed anonymous) failed");
            }
            let buf = std::slice::from_raw_parts_mut(p, len);
            if let Err(e) = file.read_exact(buf) {
                die(&format!("read {}: {e}", path.display()));
            }
        }
        let mut len = len;
        if *p.add(len - 1) != b'\n' {
            *p.add(len) = b'\n';
            len += 1;
        }
        Mapped { ptr: p, len }
    }
}

/// Bump allocator over large anonymous chunks. Memory is never returned
/// before exit, except through `release_all`.
pub struct Arena {
    cur: usize,
    end: usize,
    chunks: Vec<(usize, usize)>,
}

unsafe impl Send for Arena {}

const ARENA_CHUNK: usize = 32 << 20;

impl Arena {
    pub fn new() -> Self {
        Arena { cur: 0, end: 0, chunks: Vec::new() }
    }

    #[inline(always)]
    pub unsafe fn alloc(&mut self, size: usize, align: usize) -> *mut u8 {
        let p = (self.cur + align - 1) & !(align - 1);
        if p + size <= self.end {
            self.cur = p + size;
            return p as *mut u8;
        }
        self.grow(size, align)
    }

    #[cold]
    #[inline(never)]
    unsafe fn grow(&mut self, size: usize, align: usize) -> *mut u8 {
        let want = round_up(size + align, HUGE).max(ARENA_CHUNK);
        let p = anon_huge(want) as usize;
        self.chunks.push((p, want));
        let a = (p + align - 1) & !(align - 1);
        self.cur = a + size;
        self.end = p + want;
        a as *mut u8
    }

    /// Returns every chunk to the kernel; nothing allocated here may be used again.
    pub unsafe fn unmap_all(&self) {
        for &(p, len) in &self.chunks {
            release(p as *mut u8, len);
        }
    }

    pub unsafe fn release_all(&mut self) {
        for &(p, len) in &self.chunks {
            release(p as *mut u8, len);
        }
        self.chunks.clear();
        self.cur = 0;
        self.end = 0;
    }
}
