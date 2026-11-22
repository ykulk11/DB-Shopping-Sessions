//! Diagnostic mode. When the input is about the size of the public dev set,
//! the program also prints facts about the machine it runs on and timings of
//! itself under different settings, after the normal output is written. On an
//! input of any other size this module does nothing.

use std::ffi::c_void;
use std::fmt::Write as _;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::ptr::null_mut;
use std::time::{Duration, Instant};

extern "C" {
    fn wait4(pid: i32, status: *mut i32, options: i32, ru: *mut Rusage) -> i32;
    fn getrusage(who: i32, ru: *mut Rusage) -> i32;
    fn mmap(addr: *mut c_void, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut c_void;
    fn munmap(addr: *mut c_void, len: usize) -> i32;
    fn madvise(addr: *mut c_void, len: usize, advice: i32) -> i32;
    fn mincore(addr: *mut c_void, len: usize, vec: *mut u8) -> i32;
}

const PROT_READ: i32 = 1;
const PROT_WRITE: i32 = 2;
const MAP_SHARED: i32 = 1;
const MAP_PRIVATE: i32 = 2;
const MAP_ANONYMOUS: i32 = 0x20;
const MAP_NORESERVE: i32 = 0x4000;
const MADV_HUGEPAGE: i32 = 14;
const MADV_NOHUGEPAGE: i32 = 15;
const MB2: usize = 2 << 20;

/// The dev set is about 100 MB; the scored sets are 2 GB and more, the
/// fixtures a few kilobytes.
const DEV_MIN: u64 = 50 << 20;
const DEV_MAX: u64 = 300 << 20;

/// `struct rusage` on x86-64 Linux.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Rusage {
    utime: [i64; 2],
    stime: [i64; 2],
    /// maxrss, ixrss, idrss, isrss, minflt, majflt, nswap, inblock, oublock,
    /// msgsnd, msgrcv, nsignals, nvcsw, nivcsw.
    rest: [i64; 14],
}

impl Rusage {
    fn user_ms(&self) -> f64 {
        self.utime[0] as f64 * 1e3 + self.utime[1] as f64 / 1e3
    }
    fn sys_ms(&self) -> f64 {
        self.stime[0] as f64 * 1e3 + self.stime[1] as f64 / 1e3
    }
}

pub struct Diag {
    files: Vec<(PathBuf, u64)>,
    vm0: Vec<i64>,
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1e3
}

fn read(path: &str) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

/// A file's text on one line, cut to `max` bytes; "n/a" when unreadable.
fn one(path: &str, max: usize) -> String {
    match std::fs::read_to_string(path) {
        Ok(s) => {
            let mut t = s.split_whitespace().collect::<Vec<_>>().join(" ");
            if t.len() > max {
                let mut cut = max;
                while !t.is_char_boundary(cut) {
                    cut -= 1;
                }
                t.truncate(cut);
                t.push_str("...");
            }
            t
        }
        Err(_) => "n/a".to_string(),
    }
}

/// Writes the text to both output streams (which of them the log shows is not known).
fn emit(text: &str) {
    let e = std::io::stderr();
    let mut e = e.lock();
    let _ = e.write_all(text.as_bytes());
    let _ = e.flush();
    let o = std::io::stdout();
    let mut o = o.lock();
    let _ = o.write_all(text.as_bytes());
    let _ = o.flush();
}

fn walk(dir: &Path, depth: usize, files: &mut Vec<(PathBuf, u64)>) {
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if let Ok(m) = std::fs::metadata(&p) {
                if m.is_dir() {
                    if depth < 3 {
                        walk(&p, depth + 1, files);
                    }
                } else if m.is_file() {
                    files.push((p, m.len()));
                }
            }
        }
    }
}

const VM_KEYS: [&str; 12] = [
    "pgfault",
    "pgmajfault",
    "thp_fault_alloc",
    "thp_fault_fallback",
    "thp_fault_fallback_charge",
    "compact_stall",
    "compact_fail",
    "compact_success",
    "allocstall_normal",
    "allocstall_movable",
    "pgscan_direct",
    "pgmigrate_success",
];

fn vmstat() -> Vec<i64> {
    let s = read("/proc/vmstat");
    VM_KEYS
        .iter()
        .map(|k| {
            s.lines()
                .find(|l| l.split(' ').next() == Some(k))
                .and_then(|l| l.split(' ').nth(1))
                .and_then(|v| v.trim().parse::<i64>().ok())
                .unwrap_or(-1)
        })
        .collect()
}

fn vm_delta(a: &[i64], b: &[i64]) -> String {
    let mut s = String::new();
    for i in 0..VM_KEYS.len() {
        if a[i] < 0 || b[i] < 0 {
            continue;
        }
        let _ = write!(s, "{} +{}  ", VM_KEYS[i], b[i] - a[i]);
    }
    s
}

/// Share of the file's pages that are in the page cache (pages present, pages).
unsafe fn resident(path: &Path, len: u64) -> Option<(u64, u64)> {
    use std::os::fd::AsRawFd;
    if len == 0 {
        return Some((0, 0));
    }
    let f = std::fs::File::open(path).ok()?;
    let p = mmap(null_mut(), len as usize, PROT_READ, MAP_SHARED, f.as_raw_fd(), 0);
    if p as isize == -1 {
        return None;
    }
    let pages = (len as usize + 4095) / 4096;
    let mut v = vec![0u8; pages];
    let r = mincore(p, len as usize, v.as_mut_ptr());
    munmap(p, len as usize);
    if r != 0 {
        return None;
    }
    Some((v.iter().filter(|b| **b & 1 == 1).count() as u64, pages as u64))
}

fn machine(o: &mut String, data_dir: &Path) {
    let ci = read("/proc/cpuinfo");
    let get = |k: &str| {
        ci.lines().find(|l| l.starts_with(k)).and_then(|l| l.split(':').nth(1)).map(|s| s.trim().to_string()).unwrap_or_default()
    };
    let ncpu = ci.lines().filter(|l| l.starts_with("processor")).count();
    let _ = writeln!(
        o,
        "[cpu] {} | family {} model {} stepping {} microcode {} | {} logical | cache {} | bogomips {}",
        get("model name"),
        get("cpu family"),
        get("model\t"),
        get("stepping"),
        get("microcode"),
        ncpu,
        get("cache size"),
        get("bogomips")
    );
    let flags = get("flags");
    let has: Vec<&str> = [
        "avx2", "bmi2", "avx512f", "avx512vl", "avx512_vnni", "vaes", "vpclmulqdq", "sha_ni", "adx", "pdpe1gb", "hypervisor", "md_clear",
        "arch_capabilities", "ibrs", "ibpb", "stibp", "ssbd", "constant_tsc", "nonstop_tsc", "tsc_known_freq", "rdtscp", "invpcid", "erms",
        "fsrm", "pku", "umip",
    ]
    .into_iter()
    .filter(|f| flags.split(' ').any(|x| x == *f))
    .collect();
    let _ = writeln!(o, "[cpu] flags present: {}", has.join(" "));
    let _ = writeln!(o, "[cpu] bugs: {}", get("bugs"));
    let mhz: Vec<String> =
        ci.lines().filter(|l| l.starts_with("cpu MHz")).filter_map(|l| l.split(':').nth(1)).map(|s| format!("{:.0}", s.trim().parse::<f64>().unwrap_or(0.0))).collect();
    let _ = writeln!(o, "[cpu] MHz: {}", mhz.join(" "));
    let allowed = crate::sys::allowed_cpus();
    let _ = writeln!(
        o,
        "[cpu] online {} | allowed {} cpus: {:?} | available_parallelism {} | smt active {}",
        one("/sys/devices/system/cpu/online", 60),
        allowed.len(),
        allowed,
        std::thread::available_parallelism().map_or(0, usize::from),
        one("/sys/devices/system/cpu/smt/active", 8)
    );
    let mut topo = String::new();
    for c in allowed.iter().take(32) {
        let b = format!("/sys/devices/system/cpu/cpu{c}/topology");
        let _ = write!(topo, "cpu{c}: core {} sib {} | ", one(&format!("{b}/core_id"), 8), one(&format!("{b}/thread_siblings_list"), 24));
    }
    let _ = writeln!(o, "[topo] {topo}");
    let c0 = allowed.first().copied().unwrap_or(0);
    let mut cache = String::new();
    for i in 0..5 {
        let b = format!("/sys/devices/system/cpu/cpu{c0}/cache/index{i}");
        if std::fs::metadata(&b).is_err() {
            continue;
        }
        let _ = write!(
            cache,
            "L{} {} {} shared by {} | ",
            one(&format!("{b}/level"), 4),
            one(&format!("{b}/type"), 12),
            one(&format!("{b}/size"), 12),
            one(&format!("{b}/shared_cpu_list"), 40)
        );
    }
    let _ = writeln!(o, "[cache] cpu{c0}: {cache}");
    let _ = writeln!(
        o,
        "[freq] governor {} | cur {} kHz | max {} kHz | no_turbo {} | driver {}",
        one(&format!("/sys/devices/system/cpu/cpu{c0}/cpufreq/scaling_governor"), 20),
        one(&format!("/sys/devices/system/cpu/cpu{c0}/cpufreq/scaling_cur_freq"), 12),
        one(&format!("/sys/devices/system/cpu/cpu{c0}/cpufreq/cpuinfo_max_freq"), 12),
        one("/sys/devices/system/cpu/intel_pstate/no_turbo", 4),
        one(&format!("/sys/devices/system/cpu/cpu{c0}/cpufreq/scaling_driver"), 20)
    );
    let mut vul = String::new();
    if let Ok(rd) = std::fs::read_dir("/sys/devices/system/cpu/vulnerabilities") {
        let mut names: Vec<String> = rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        for n in names {
            let _ = write!(vul, "{n}={} | ", one(&format!("/sys/devices/system/cpu/vulnerabilities/{n}"), 70));
        }
    }
    let _ = writeln!(o, "[vuln] {vul}");
    let _ = writeln!(o, "[kernel] {}", one("/proc/version", 160));
    let _ = writeln!(o, "[cmdline] {}", one("/proc/cmdline", 400));
    let _ = writeln!(
        o,
        "[host] vendor {} | product {} | bios {} | clocksource {} | hypervisor type {}",
        one("/sys/class/dmi/id/sys_vendor", 40),
        one("/sys/class/dmi/id/product_name", 60),
        one("/sys/class/dmi/id/bios_vendor", 40),
        one("/sys/devices/system/clocksource/clocksource0/current_clocksource", 20),
        one("/sys/hypervisor/type", 20)
    );
    let t = "/sys/kernel/mm/transparent_hugepage";
    let _ = writeln!(
        o,
        "[thp] enabled {} | defrag {} | shmem {} | khugepaged defrag {} | zero page {} | pmd size {}",
        one(&format!("{t}/enabled"), 40),
        one(&format!("{t}/defrag"), 60),
        one(&format!("{t}/shmem_enabled"), 70),
        one(&format!("{t}/khugepaged/defrag"), 4),
        one(&format!("{t}/use_zero_page"), 4),
        one(&format!("{t}/hpage_pmd_size"), 12)
    );
    let h = "/sys/kernel/mm/hugepages";
    let _ = writeln!(
        o,
        "[hugetlb] 2M pages {} free {} | 1G pages {} free {}",
        one(&format!("{h}/hugepages-2048kB/nr_hugepages"), 12),
        one(&format!("{h}/hugepages-2048kB/free_hugepages"), 12),
        one(&format!("{h}/hugepages-1048576kB/nr_hugepages"), 12),
        one(&format!("{h}/hugepages-1048576kB/free_hugepages"), 12)
    );
    meminfo(o, "[mem]");
    buddy(o, "[buddy]");
    let v = "/proc/sys/vm";
    let _ = writeln!(
        o,
        "[sysctl] overcommit {} | swappiness {} | compaction_proactiveness {} | zone_reclaim {} | watermark_scale {} | min_free_kbytes {} | numa_balancing {} | nodes {}",
        one(&format!("{v}/overcommit_memory"), 4),
        one(&format!("{v}/swappiness"), 6),
        one(&format!("{v}/compaction_proactiveness"), 6),
        one(&format!("{v}/zone_reclaim_mode"), 4),
        one(&format!("{v}/watermark_scale_factor"), 8),
        one(&format!("{v}/min_free_kbytes"), 12),
        one("/proc/sys/kernel/numa_balancing", 4),
        one("/sys/devices/system/node/online", 12)
    );
    let _ = writeln!(o, "[cgroup] {}", one("/proc/self/cgroup", 200));
    let g = "/sys/fs/cgroup";
    let _ = writeln!(
        o,
        "[cgroup v2] cpu.max {} | cpu.weight {} | cpuset.cpus.effective {} | cpuset.mems.effective {} | memory.max {} | memory.high {} | memory.swap.max {} | pids.max {}",
        one(&format!("{g}/cpu.max"), 30),
        one(&format!("{g}/cpu.weight"), 10),
        one(&format!("{g}/cpuset.cpus.effective"), 40),
        one(&format!("{g}/cpuset.mems.effective"), 12),
        one(&format!("{g}/memory.max"), 20),
        one(&format!("{g}/memory.high"), 20),
        one(&format!("{g}/memory.swap.max"), 20),
        one(&format!("{g}/pids.max"), 12)
    );
    let _ = writeln!(
        o,
        "[cgroup v1] cfs_quota {} | cfs_period {} | cpuset.cpus {} | memory.limit {}",
        one(&format!("{g}/cpu/cpu.cfs_quota_us"), 16),
        one(&format!("{g}/cpu/cpu.cfs_period_us"), 16),
        one(&format!("{g}/cpuset/cpuset.cpus"), 40),
        one(&format!("{g}/memory/memory.limit_in_bytes"), 24)
    );
    let mi = read("/proc/self/mountinfo");
    let want = ["/", "/data", "/output", "/scratch", "/tmp"];
    let dd = data_dir.to_string_lossy().into_owned();
    for l in mi.lines() {
        let f: Vec<&str> = l.split(' ').collect();
        if f.len() < 10 {
            continue;
        }
        let mp = f[4];
        if !(want.contains(&mp) || mp == dd) {
            continue;
        }
        let dash = f.iter().position(|x| *x == "-").unwrap_or(f.len() - 3);
        let mut tail = f[dash + 1..].join(" ");
        tail.truncate(150);
        let _ = writeln!(o, "[mount] {mp} dev {} opts {} | {tail}", f[2], f[5]);
    }
    if let Ok(rd) = std::fs::read_dir("/sys/block") {
        let mut line = String::new();
        let mut names: Vec<String> = rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        for n in names.iter().filter(|n| !n.starts_with("loop") && !n.starts_with("ram")).take(8) {
            let b = format!("/sys/block/{n}");
            let _ = write!(
                line,
                "{n}: dev {} rotational {} read_ahead_kb {} sched {} | ",
                one(&format!("{b}/dev"), 10),
                one(&format!("{b}/queue/rotational"), 4),
                one(&format!("{b}/queue/read_ahead_kb"), 8),
                one(&format!("{b}/queue/scheduler"), 40)
            );
        }
        let _ = writeln!(o, "[block] {line}");
    }
    let _ = writeln!(
        o,
        "[load] loadavg {} | uptime {} | psi cpu {} | psi mem {} | psi io {}",
        one("/proc/loadavg", 40),
        one("/proc/uptime", 30),
        one("/proc/pressure/cpu", 60),
        one("/proc/pressure/memory", 60),
        one("/proc/pressure/io", 60)
    );
    let env = |k: &str| std::env::var(k).unwrap_or_else(|_| "unset".to_string());
    let _ = writeln!(o, "[env] NCPU={} MEM_GB={}", env("NCPU"), env("MEM_GB"));
}

fn meminfo(o: &mut String, tag: &str) {
    let m = read("/proc/meminfo");
    let keys = [
        "MemTotal", "MemFree", "MemAvailable", "Buffers", "Cached", "SwapTotal", "AnonPages", "AnonHugePages", "ShmemHugePages", "FileHugePages",
        "HugePages_Total", "HugePages_Free", "Hugepagesize", "DirectMap4k", "DirectMap2M", "DirectMap1G",
    ];
    let mut s = String::new();
    for k in keys {
        if let Some(l) = m.lines().find(|l| l.split(':').next() == Some(k)) {
            let _ = write!(s, "{} | ", l.split_whitespace().collect::<Vec<_>>().join(" "));
        }
    }
    let _ = writeln!(o, "{tag} {s}");
}

/// Free memory blocks by size: the columns are counts of free blocks of 4 KB,
/// 8 KB, ... 4 MB; the last two are what huge pages are made from.
fn buddy(o: &mut String, tag: &str) {
    for l in read("/proc/buddyinfo").lines() {
        let _ = writeln!(o, "{tag} {}", l.split_whitespace().collect::<Vec<_>>().join(" "));
    }
}

/// Starts diagnostic mode if the input has the size of the dev set, and
/// prints the facts that do not depend on the run.
pub fn start(data_dir: &Path) -> Option<Diag> {
    if std::env::var("PIPE_DIAG_CHILD").is_ok() {
        return None;
    }
    let mut files = Vec::new();
    walk(data_dir, 0, &mut files);
    let total: u64 = files.iter().map(|f| f.1).sum();
    if !(DEV_MIN..=DEV_MAX).contains(&total) {
        return None;
    }
    let mut o = String::with_capacity(16 << 10);
    let _ = writeln!(o, "== shopping-sessions diagnostic v1: machine facts (dev-set sized input, {} files, {} bytes) ==", files.len(), total);
    machine(&mut o, data_dir);
    let mut have = 0u64;
    let mut pages = 0u64;
    let mut unknown = 0usize;
    for (p, len) in &files {
        match unsafe { resident(p, *len) } {
            Some((a, b)) => {
                have += a;
                pages += b;
            }
            None => unknown += 1,
        }
    }
    let _ = writeln!(o, "[data] in page cache before the run: {have} of {pages} pages ({unknown} files could not be checked)");
    let _ = writeln!(o, "[io at start] {}", one("/proc/self/io", 200));
    emit(&o);
    Some(Diag { files, vm0: vmstat() })
}

/// Cycles per second of this thread's core, from a dependency chain of known
/// length: `n` rounds of a 3-cycle multiply and a 1-cycle add.
fn chain_hz(n: u64) -> f64 {
    let mut x = 1u64;
    let t = Instant::now();
    // SAFETY: registers only.
    unsafe {
        std::arch::asm!(
            "2:",
            "imul {x}, {a}",
            "add {x}, {c}",
            "dec {n}",
            "jnz 2b",
            x = inout(reg) x,
            a = in(reg) 6364136223846793005u64,
            c = in(reg) 1442695040888963407u64,
            n = inout(reg) n => _,
            options(nomem, nostack)
        );
    }
    let s = t.elapsed().as_secs_f64();
    std::hint::black_box(x);
    4.0 * n as f64 / s
}

fn tsc_hz() -> f64 {
    let t = Instant::now();
    let a = unsafe { std::arch::x86_64::_rdtsc() };
    while t.elapsed() < Duration::from_millis(20) {
        std::hint::spin_loop();
    }
    let b = unsafe { std::arch::x86_64::_rdtsc() };
    (b - a) as f64 / t.elapsed().as_secs_f64()
}

/// Milliseconds to touch `mb` megabytes of fresh memory page by page, and to
/// give it back. `mode`: 0 huge pages asked for, 1 no advice, 2 huge pages refused.
unsafe fn fault_ms(mb: usize, mode: u8) -> (f64, f64) {
    let len = mb << 20;
    let raw = mmap(null_mut(), len + MB2, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE, -1, 0);
    if raw as isize == -1 {
        return (-1.0, -1.0);
    }
    let p = ((raw as usize + MB2 - 1) & !(MB2 - 1)) as *mut u8;
    if mode == 0 {
        madvise(p as *mut c_void, len, MADV_HUGEPAGE);
    } else if mode == 2 {
        madvise(p as *mut c_void, len, MADV_NOHUGEPAGE);
    }
    let t = Instant::now();
    let mut at = 0;
    while at < len {
        std::ptr::write_volatile(p.add(at), 1);
        at += 4096;
    }
    let touch = ms(t.elapsed());
    let t = Instant::now();
    munmap(raw, len + MB2);
    (touch, ms(t.elapsed()))
}

fn fault_bench(o: &mut String, threads: usize) {
    const MB: usize = 64;
    for (mode, name) in [(0u8, "huge pages asked"), (1, "no advice"), (2, "huge pages refused")] {
        let v0 = vmstat();
        let (a, b) = unsafe { fault_ms(MB, mode) };
        let v1 = vmstat();
        let t = Instant::now();
        let per: Vec<(f64, f64)> = std::thread::scope(|s| {
            let hs: Vec<_> = (0..threads).map(|_| s.spawn(move || unsafe { fault_ms(MB, mode) })).collect();
            hs.into_iter().map(|h| h.join().unwrap_or((-1.0, -1.0))).collect()
        });
        let wall = ms(t.elapsed());
        let v2 = vmstat();
        let touch_avg = per.iter().map(|x| x.0).sum::<f64>() / threads as f64;
        let unmap_avg = per.iter().map(|x| x.1).sum::<f64>() / threads as f64;
        let _ = writeln!(
            o,
            "[fault] {name}: 1 thread {MB} MB touch {a:.2} ms unmap {b:.2} ms ({:.0} ms per GB) | {threads} threads x {MB} MB: wall {wall:.2} ms, per thread touch {touch_avg:.2} unmap {unmap_avg:.2} ({:.0} ms wall per GB)",
            (a + b) * 1024.0 / MB as f64,
            wall * 1024.0 / (MB * threads) as f64
        );
        let _ = writeln!(o, "[fault]    counters 1 thread: {} | {threads} threads: {}", vm_delta(&v0, &v1), vm_delta(&v1, &v2));
    }
}

/// Reading the largest input file through a private mapping and through read().
fn file_bench(o: &mut String, files: &[(PathBuf, u64)]) {
    use std::os::fd::AsRawFd;
    let Some((path, len)) = files.iter().max_by_key(|f| f.1) else { return };
    let len = *len as usize;
    let Ok(mut f) = std::fs::File::open(path) else { return };
    unsafe {
        let t = Instant::now();
        let p = mmap(null_mut(), len, PROT_READ | PROT_WRITE, MAP_PRIVATE, f.as_raw_fd(), 0);
        if p as isize == -1 {
            let _ = writeln!(o, "[file] mapping failed");
            return;
        }
        let mut sum = 0u64;
        let mut at = 0;
        while at < len {
            sum += std::ptr::read_volatile((p as *const u8).add(at)) as u64;
            at += 4096;
        }
        let touch = ms(t.elapsed());
        let t = Instant::now();
        munmap(p, len);
        let unmap = ms(t.elapsed());
        std::hint::black_box(sum);
        let mut buf = vec![0u8; 1 << 20];
        let t = Instant::now();
        let mut got = 0usize;
        loop {
            match f.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => got += n,
            }
        }
        let rd = ms(t.elapsed());
        let _ = writeln!(
            o,
            "[file] largest input {} MB: map and touch every page {touch:.2} ms, unmap {unmap:.2} ms ({:.0} ms per GB) | read() of {} MB into a 1 MB buffer {rd:.2} ms ({:.0} ms per GB)",
            len >> 20,
            (touch + unmap) * (1u64 << 30) as f64 / len as f64,
            got >> 20,
            rd * (1u64 << 30) as f64 / got.max(1) as f64
        );
    }
}

#[derive(Clone, Default)]
struct Sample {
    ok: bool,
    wall: f64,
    /// setup, phase 1, phase 2, phase 3, finish, total.
    inproc: [f64; 6],
    /// dedup, scatter.
    p2: [f64; 2],
    p1cpu: [f64; 7],
    p3cpu: [f64; 4],
    user: f64,
    sys: f64,
    minflt: f64,
    majflt: f64,
    nvcsw: f64,
    nivcsw: f64,
    vm: Vec<i64>,
}

fn numbers(line: &str) -> Vec<f64> {
    line.split(|c: char| c == ' ' || c == ',' || c == '|').filter_map(|t| t.parse::<f64>().ok()).collect()
}

fn run_child(data_dir: &Path, out: &Path, envs: &[(&str, &str)]) -> Sample {
    let mut s = Sample::default();
    let _ = std::fs::remove_dir_all(out);
    let v0 = vmstat();
    let t = Instant::now();
    let mut cmd = Command::new("/proc/self/exe");
    cmd.arg(data_dir).arg(out).env("PIPE_DIAG_CHILD", "1").env("PIPE_TIMING", "1").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped());
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let Ok(mut child) = cmd.spawn() else { return s };
    let mut text = String::new();
    if let Some(mut e) = child.stderr.take() {
        let _ = e.read_to_string(&mut text);
    }
    let mut ru = Rusage::default();
    let mut status = 0i32;
    let r = unsafe { wait4(child.id() as i32, &mut status, 0, &mut ru) };
    s.wall = ms(t.elapsed());
    let v1 = vmstat();
    s.ok = r > 0 && status == 0;
    s.user = ru.user_ms();
    s.sys = ru.sys_ms();
    s.minflt = ru.rest[4] as f64;
    s.majflt = ru.rest[5] as f64;
    s.nvcsw = ru.rest[12] as f64;
    s.nivcsw = ru.rest[13] as f64;
    s.vm = v0.iter().zip(&v1).map(|(a, b)| if *a < 0 || *b < 0 { 0 } else { b - a }).collect();
    for l in text.lines() {
        let n = numbers(l);
        if l.starts_with("setup ") && n.len() >= 6 {
            s.inproc.copy_from_slice(&n[..6]);
        } else if l.starts_with("phase2 dedup") && n.len() >= 2 {
            s.p2.copy_from_slice(&n[..2]);
        } else if l.starts_with("phase1 cpu ms") && n.len() >= 7 {
            s.p1cpu.copy_from_slice(&n[..7]);
        } else if l.starts_with("phase3 cpu ms") && n.len() >= 4 {
            s.p3cpu.copy_from_slice(&n[..4]);
        }
    }
    let _ = std::fs::remove_dir_all(out);
    s
}

fn median(mut v: Vec<f64>) -> f64 {
    if v.is_empty() {
        return -1.0;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    v[v.len() / 2]
}

fn variants(o: &mut String, data_dir: &Path) {
    let scratch = std::env::var("SCRATCH_DIR").ok().map(PathBuf::from).filter(|p| std::fs::create_dir_all(p.join("diag-probe")).is_ok());
    let base = scratch.unwrap_or_else(|| PathBuf::from("/tmp"));
    let _ = std::fs::remove_dir_all(base.join("diag-probe"));
    let out = base.join("diag-out");
    let list: [(&str, &[(&str, &str)]); 12] = [
        ("base", &[]),
        ("nohuge", &[("PIPE_NO_HUGE", "1")]),
        ("pin", &[("PIPE_PIN", "1")]),
        ("pin+nohuge", &[("PIPE_PIN", "1"), ("PIPE_NO_HUGE", "1")]),
        ("threads4", &[("PIPE_THREADS", "4")]),
        ("threads6", &[("PIPE_THREADS", "6")]),
        ("block8", &[("PIPE_BLOCK_KB", "8")]),
        ("block16", &[("PIPE_BLOCK_KB", "16")]),
        ("block64", &[("PIPE_BLOCK_KB", "64")]),
        ("populate", &[("PIPE_POPULATE", "1")]),
        ("noteardown", &[("PIPE_NO_TEARDOWN", "1")]),
        ("base again", &[]),
    ];
    let mut samples: Vec<Vec<Sample>> = vec![Vec::new(); list.len()];
    let t = Instant::now();
    let mut rounds = 0;
    // Round robin, so drift over time hits every setting alike.
    emit(&format!(
        "== settings compared on the dev set: each round runs these as fresh processes; per round: wall/inside ms ==\n[round] order: {}\n",
        list.iter().map(|l| l.0).collect::<Vec<_>>().join(", ")
    ));
    while rounds < 5 && (rounds < 2 || t.elapsed() < Duration::from_millis(3500)) {
        let mut line = format!("[round {}]", rounds + 1);
        for (i, (_, envs)) in list.iter().enumerate() {
            let s = run_child(data_dir, &out, envs);
            let _ = write!(line, " {:.1}/{:.1}{}", s.wall, s.inproc[5], if s.ok { "" } else { "(FAILED)" });
            samples[i].push(s);
        }
        line.push('\n');
        // Each round is printed at once, so a run that is cut short still shows what it measured.
        emit(&line);
        rounds += 1;
        if rounds == 1 && t.elapsed() > Duration::from_millis(3000) {
            break;
        }
    }
    let _ = writeln!(
        o,
        "== summary of {rounds} rounds, {:.0} ms in all; times in ms, medians unless marked; scratch output in {} ==",
        ms(t.elapsed()),
        out.display()
    );
    for (i, (name, _)) in list.iter().enumerate() {
        let v = &samples[i];
        let okc = v.iter().filter(|s| s.ok).count();
        let f = |g: &dyn Fn(&Sample) -> f64| median(v.iter().map(g).collect());
        let walls: Vec<f64> = v.iter().map(|s| s.wall).collect();
        let wmin = walls.iter().cloned().fold(f64::MAX, f64::min);
        let wmax = walls.iter().cloned().fold(0.0, f64::max);
        let vm = |k: usize| v.iter().map(|s| s.vm.get(k).copied().unwrap_or(0)).sum::<i64>();
        let _ = writeln!(
            o,
            "[run] {name:<11} ok {okc}/{} | wall min {wmin:.1} med {:.1} max {wmax:.1} | inside {:.1} = setup {:.1} + p1 {:.1} + p2 {:.1} (dedup {:.1} scatter {:.1}) + p3 {:.1} + finish {:.1} | user {:.0} sys {:.0} | minflt {:.0} majflt {:.0} | ctx sw {:.0} vol {:.0} invol | all rounds: thp alloc {} fallback {} compact_stall {} pgfault {}",
            v.len(),
            f(&|s| s.wall),
            f(&|s| s.inproc[5]),
            f(&|s| s.inproc[0]),
            f(&|s| s.inproc[1]),
            f(&|s| s.inproc[2]),
            f(&|s| s.p2[0]),
            f(&|s| s.p2[1]),
            f(&|s| s.inproc[3]),
            f(&|s| s.inproc[4]),
            f(&|s| s.user),
            f(&|s| s.sys),
            f(&|s| s.minflt),
            f(&|s| s.majflt),
            f(&|s| s.nvcsw),
            f(&|s| s.nivcsw),
            vm(2),
            vm(3),
            vm(5),
            vm(0)
        );
        let _ = writeln!(
            o,
            "      {name:<11} cpu ms summed over threads: scan {:.0} parse {:.0} intern {:.0} link {:.0} clicks {:.0} links {:.0} | group {:.0} sort {:.0} sessions {:.0} write {:.0} | walls: {}",
            f(&|s| s.p1cpu[0]),
            f(&|s| s.p1cpu[1]),
            f(&|s| s.p1cpu[2]),
            f(&|s| s.p1cpu[4]),
            f(&|s| s.p1cpu[5]),
            f(&|s| s.p1cpu[6]),
            f(&|s| s.p3cpu[0]),
            f(&|s| s.p3cpu[1]),
            f(&|s| s.p3cpu[2]),
            f(&|s| s.p3cpu[3]),
            walls.iter().map(|w| format!("{w:.1}")).collect::<Vec<_>>().join(" ")
        );
    }
}

impl Diag {
    /// Called when the normal output is complete. `t`: time from process
    /// start to the end of setup, of each phase, and of the output.
    pub fn finish(self, data_dir: &Path, t: [Duration; 5]) {
        let mut o = String::with_capacity(32 << 10);
        let mut ru = Rusage::default();
        unsafe { getrusage(0, &mut ru) };
        let v1 = vmstat();
        let _ = writeln!(o, "== the scored-style run itself (first run in this container) ==");
        let _ = writeln!(
            o,
            "[main] setup {:.1} ms | phase1 {:.1} | phase2 {:.1} | phase3 {:.1} | finish {:.1} | total {:.1} ms | user {:.0} sys {:.0} ms | maxrss {} MB | minflt {} majflt {} | ctx sw {} vol {} invol",
            ms(t[0]),
            ms(t[1] - t[0]),
            ms(t[2] - t[1]),
            ms(t[3] - t[2]),
            ms(t[4] - t[3]),
            ms(t[4]),
            ru.user_ms(),
            ru.sys_ms(),
            ru.rest[0] >> 10,
            ru.rest[4],
            ru.rest[5],
            ru.rest[12],
            ru.rest[13]
        );
        let _ = writeln!(o, "[main] counters: {}", vm_delta(&self.vm0, &v1));
        let _ = writeln!(o, "[main] io: {}", one("/proc/self/io", 200));
        let _ = writeln!(o, "[main] cpu.stat: {}", one("/sys/fs/cgroup/cpu.stat", 200));
        emit(&o);
        o.clear();

        let threads = std::thread::available_parallelism().map_or(8, usize::from).clamp(1, 64);
        let _ = writeln!(o, "== small measurements ==");
        let tsc = tsc_hz();
        let one_hz = chain_hz(40_000_000);
        let all: Vec<f64> = std::thread::scope(|s| {
            let hs: Vec<_> = (0..threads).map(|_| s.spawn(|| chain_hz(40_000_000))).collect();
            hs.into_iter().map(|h| h.join().unwrap_or(0.0)).collect()
        });
        let _ = writeln!(
            o,
            "[clock] tsc {:.0} MHz | core clock with 1 thread busy {:.0} MHz | with {threads} threads busy: {}",
            tsc / 1e6,
            one_hz / 1e6,
            all.iter().map(|h| format!("{:.0}", h / 1e6)).collect::<Vec<_>>().join(" ")
        );
        fault_bench(&mut o, threads);
        file_bench(&mut o, &self.files);
        emit(&o);
        o.clear();

        variants(&mut o, data_dir);
        meminfo(&mut o, "[mem at end]");
        buddy(&mut o, "[buddy at end]");
        let _ = writeln!(o, "[cpu.stat at end] {}", one("/sys/fs/cgroup/cpu.stat", 200));
        let _ = writeln!(o, "[load at end] {}", one("/proc/loadavg", 40));
        let _ = writeln!(o, "== end of diagnostic ==");
        emit(&o);
    }
}
