# Bottleneck finder reports (measured against 558f20c)

Five profiling agents (cachegrind/callgrind on synthetic 2M/20M-event datasets). Baseline at 558f20c: 2.07G Ir on 2M events single thread; ~750 ms wall on 20M events / 4 threads. Line numbers refer to 558f20c; origin/main has since moved.

## SIMD structural scan (src/scan.rs) and how phase1 consumes it (LineIter in src/main.rs)

**Measurements:** Profiled a debug-info copy (/home/user/out/find-scan/src-copy) with cachegrind on bench2 at PIPE_THREADS=1. Ir was 2,072,262,541, the same as baseline.
- scan::scan costs about 255M Ir (12.3%): 186M in the body, 63M in tzcnt/blsr/popcnt (uint_macros), 6M in loads. That is about 85-90 Ir per 64-byte word over about 3M words.
  - The 8 unconditional comma slots cost about 110M (36 Ir/word).
  - The 2 newline slots plus the cn>2 loop cost about 24M.
  - The `rem<64` tail test runs every word and costs about 9M.
  - scan causes 1.64M D1 write misses (19% of the program's).
- LineIter consumption in phase1 costs about 190M Ir.
  - The quoted-line slow path is about 165M: the byte loads for `q` and `closed` alone are 60.9M + 70.6M.
  - The `if ql == 0` fast-path branch mispredicts 753K of 2.67M times, which is 9.6% of all program mispredicts (7.81M total).
  - quotes_before and the comma bound check take 555K + 875K D1 read misses.
- Field data: events average 79 B/line and 21% of lines have no quotes. Clicks are 51 B/line, 41% without quotes. Links are 30 B/line, 66% without quotes.
  - Commas per 64-byte word in events never exceed 6 (5: 38.6%, 6: 33.8%, 7+: <0.05%). Newlines per word in events are at most 1 (1: 80.9%, 2: <0.05%).
  - Clicks: at most 6 commas per word, and 2 newlines in 31% of words. Links: at most 3-4 commas and 3 newlines per word.
- PIPE_TIMING on bench2, 1 thread: scan about 21-26 ms and parse about 100-177 ms. That is very noisy: the box is shared and load average was 1.5-3.
- Experiments, all in scratch copies with outputs byte-identical to baseline on bench2:
  - (e) per-kind slot counts in scan: Ir 2,051,746,272, which is -20.5M (-0.99%). Mispredicts unchanged. Median scan cpu 22 ms vs 24 ms over 16 rotated runs (noisy).
  - (c) LineIter without the `ql==0` fast path: Ir 2,141,235,985 (+69M, +3.3%), mispredicts 7,044,771 (-763K, -9.8%).
    - Isolated microbenchmark of scan+LineIter on real hardware, median of 8 interleaved runs: events 125.8 -> 110.3 ms (-12%), clicks 93.7 -> 77.4 ms (-17%).
    - Full-pipeline A/B was inconclusive under noise: parse min 91 vs 99 ms, medians equal.
  - (ce) both together: 2,120,721,124 Ir.
  - BLOCK size: 16K gives Ir +0.2%, 8K gives Ir +0.67%.
    - With a simulated 32 KB L1D matching Cascade Lake (`--D1=32768,8,64`), total D1 misses go 28.55M -> 27.30M at 16K and 21.60M at 8K (-24%). phase1's own D1 misses fall from 5.32M rd + 2.58M wr to 0.89M rd + 1.23M wr.
    - This container's L1D is 48 KB, so it cannot show the effect. Wall A/B here was flat.
  - Rejected: a fused per-line 128-byte-window parser (no materialised comma array; clmul prefix-xor plus pext quote validation, branchless).
    - Correctness: it matched the current LineIter exactly on bench2 events/clicks/links and on a 300K-line-per-arity random fuzz with quotes, wrong field counts and lines over 128 B.
    - Microbenchmark Ir: events 115.4M vs 118.6M, roughly equal; clicks 143.4M vs 131.1M (worse); links 16.3M vs 10.7M (worse). Wall time about equal. Not worth the rewrite.
- Artifacts:
  - Microbenchmark plus equivalence/fuzz harness: /home/user/out/find-scan/mb (`target/release/mb {a|b|c|e|f} N file reps`).
  - Fuzz inputs: /home/user/out/find-scan/f7.csv, f4.csv, f2.csv.
  - Patched scratch trees: /home/user/out/find-scan/src-e, src-c, src-ce.
  - Binaries: /home/user/out/find-scan/pipe-{b32,b16,b8,c,e,ce}.
  - Cachegrind outputs: /home/user/out/find-scan/cg-*.out and cgd-*.out.

- **Per-kind unconditional slot counts in scan (events 6 commas/1 newline, clicks 6/2, links 4/3)** (src/scan.rs:scan (comma 8-slot loop and newline 2-slot block); call sites in src/main.rs events_block/clicks_block/links_block; est About 1.0% total Ir (deterministic, about 20M on bench2, so about 200M on bench20). About 1.3% with the tail hoist. Roughly 5-8% of scan time.; risk low)
  - evidence: Comma/newline extraction costs about 134M of scan's 255M Ir. Measured per-word counts: events never exceed 6 commas or 1 newline per 64 B (7+ commas and 2 newlines each under 0.05%), clicks reach 6 commas/2 newlines, links 4/3. The extra unconditional slots are pure waste at 4 Ir each (tzcnt, add, store, blsr) plus one store-port slot. A scratch implementation (/home/user/out/find-scan/src-e) gave Ir 2,072.26M -> 2,051.75M (-0.99%) on bench2 with identical output. Scan cpu median was 22 vs 24 ms (noisy).
  - proposal: Make scan generic: `pub unsafe fn scan_k<const CS: usize, const NS: usize>`, keep `scan()` as `scan_k::<8,2>` for products/campaigns, and replace `for j in 0..8` / `if cc > 8 { j = 8 ... }` with CS, and the two hardcoded newline stores with `for j in 0..NS` plus `if cn > NS` spill loop. Call `scan_k::<6,1>` from events_block, `<6,2>` from clicks_block and `<4,3>` from links_block. The spill loops stay, so any data with denser separators is still correct, only slower. Optionally also take the `rem < 64` masking out of the hot loop: run full words in the main loop, then one masked tail iteration. That saves about 3 Ir/word (about 9M Ir, 0.4%).
- **Branchless LineIter: drop the `ql == 0` fast path (or make the quoted path cheaper)** (src/main.rs: LineIter::next (the `if ql == 0 { ... return true; }` block); est Uncertain: from 0 to about 2-3% phase1 cpu. The microbenchmark suggests a win; the Ir model suggests a wash.; risk low)
  - evidence: The `ql==0` branch is effectively random: 21% of event lines and 41% of click lines have no quotes. It mispredicts 753K times on bench2, 9.6% of all program mispredicts. Removing it (scratch /home/user/out/find-scan/src-c, output identical) cut mispredicts 7.81M -> 7.04M but added 69M Ir (+3.3%). In the isolated scan+LineIter microbenchmark on real hardware the branchless version was 12% faster on events (125.8 -> 110.3 ms median) and 17% faster on clicks (93.7 -> 77.4 ms). The full-pipeline A/B on this shared box was inconclusive (parse min 91 vs 99 ms, medians equal). Semantics are provably identical: with ql==0 the slow loop yields q=0 for every field, bad=0 and total=0==ql.
  - proposal: Delete the `if ql == 0` fast-path block so every line runs the branch-free per-field loop. Keep the fast path for links only if A/B shows it helps (66% of link lines have no quotes); links are only 26 MB of the 1.8 GB. Must be accepted only on interleaved A/B, at least 8 pairs on an idle machine or the Cascade Lake target, comparing phase1 cpu (the `parse` subtask). The trade-off is about 12M fewer mispredict cycles versus about 28M cycles of extra instructions per bench2 pass, so the result may land either way in the full pipeline.
- **Smaller parse BLOCK (8 KB) so text and comma arrays stay L1-resident between scan and LineIter/field parsing on a 32 KB-L1D Cascade Lake** (src/main.rs: const BLOCK (32 << 10); phase1 block loop; est Target-dependent: about 0-3% of phase1 on Cascade Lake from fewer L1 misses. Unmeasurable on this 48 KB-L1D container.; risk low)
  - evidence: BLOCK=32 KB of text plus about 10 KB of comma offsets plus qw/qcum/nls is bigger than Cascade Lake's 32 KB L1D. LineIter and the field parsers therefore re-read text and commas from L2: 875K D1 read misses at the comma bound check, 1.08M+1.18M at the quote byte loads, and 555K at quotes_before. Cachegrind with `--D1=32768,8,64`: total D1 misses 28.55M -> 21.60M (-24%) at BLOCK=8K. phase1's own misses fall from 5.32M rd + 2.58M wr to 0.89M rd + 1.23M wr. The cost is +0.67% Ir from per-block overhead (4 `Instant::now`, reserve, last_line_end) with 4x more blocks. This container has a 48 KB L1D, so wall-time A/B here was flat and cannot validate the gain.
  - proposal: Set BLOCK to 8 << 10 (also try 12K and 16K; 16K helped little in the 32 KB simulation: 28.55M -> 27.30M). To recover the +0.67% Ir, gate the four per-block `Instant::now()` calls in events_block (and the others) behind the PIPE_TIMING flag. The block-boundary logic already handles lines longer than a block (find_newline fallback), so output does not change. Accept only if wall-clock A/B on Cascade Lake, or a careful idle-machine run, shows a phase1 improvement.

## Phase-1 event/click/link row parsing (LineIter field splitting, events_block/clicks_block, ts.rs parse paths)

**Measurements:** Data shape (bench2 events, via python): 79% of event lines contain at least one quote, because each field is quoted on its own about 20% of the time. So LineIter::next takes the quoted branch most of the time, and the ql==0 branch splits 21/79 at random. ts field after stripping quotes: 13 digits 89.5%, 16 digits with leading zeros 10.0%, Z form 0.5%. ingest_ts is F1 (20 bytes) about 100%.

Baseline callgrind on bench2, PIPE_THREADS=1, debug-info copy, Ir 2,060M:
- LineIter quoted loop, per-field loads `q = text[s]=='"'` and `closed = text[e-1]=='"'`: 60.9M + 70.6M Ir.
- Whole LineIter: about 215M Ir, about 80 Ir per line.
- `if ql==0`: 753K of 2.67M branches mispredicted.
- Hot loop: 244M Ir in events_block code with no line info. Objdump shows this is mostly stack spills and reloads from register pressure, e.g. the text pointer reloaded from 0x18(%rsp) several times per row.
- ts::parse_digits: 41M Ir (2%) over 262K calls (about 156 Ir per call), almost all from parse_ts on the 16-digit ts values, plus 330K mispredicts.
- ingest parse (datetime19c via parse_f1_seconds): about 38 Ir per row, about 4% of total Ir on bench20.

Cachegrind on bench20, 1 thread, with D1 set to 32K (Cascade Lake size) and LL to 1M: Ir 20.39G.
- The two quote loads: 609M + 706M Ir and 23M D1 misses.
- parse_digits: 414M Ir.
- `ql==0`: 7.5M mispredicts (valgrind's simple predictor; real hardware would see at least about 21% of 20M lines).

Experiments, all in scratch copies under /home/user/out/find-parse/, all must and did print GATE PASS via check.sh:
- (a) BLOCK 16K / 8K: D1 misses -4% / -24%, Ir +0.2% / +0.7%. No measurable change in parse or user CPU in A/B runs (8K was slightly worse). Rejected.
- (b) Generic 9..16-digit SWAR inlined into parse_ts: parse_digits Ir -41M, but phase1 +41M from extra spills, net 0 Ir and worse simulated branches. Rejected.
- (b2) Dedicated len==16 branch inlined into parse_ts: net +9.5M Ir, same spill problem. Rejected.
- (c) tsw3, SWAR fast path inside the out-of-line parse_digits for 9..16 digits: Ir 2,072.26M -> 2,047.28M (-25.0M, -1.2%). phase1 Ir unchanged. GATE PASS. A new fuzz test for lengths 9..16 against str::parse also passes.
- (d) gat = tsw3 + AVX2 gather-based next7 for events: Ir 2,047.3M -> 1,839.7M (-207.6M, -10.0%). Unattributed hot-loop Ir halves (244M -> 117M) because the spills go away. GATE PASS.
- (e) gatc = gat + 128-bit gather next4 for clicks: Ir 1,823.4M (-16.3M more). GATE PASS.

Total vs baseline: 2,072.3M -> 1,823.4M Ir (-12.0%). Valgrind counts a gather as one instruction, so I checked wall time separately.

Wall/CPU A/B on bench20, interleaved, medians, noisy because other agents share the CPUs:
- 1 thread, 10 rounds: parse 1498 base / 1467 tsw3 / 1270 gat ms (mins 1232 / 1190 / 1044); user CPU 2996 / 3049 / 2893.
- 4 threads, 8 rounds: parse CPU 1682 base vs 1468 gat; phase1 wall 756 vs 673 ms; total 1118 vs 1022 ms; but the minimums were about equal.
- 1 thread, 8 rounds, base / gat / gatc: parse median 1382 / 1410 / 1128, min 1174 / 1021 / 1017; clicks min 272 / 213 / 227.
Overall, gather parsing cuts about 13% of parse CPU going by the minimums.

Ready-to-apply diffs: /home/user/out/find-parse/gatc-main.patch and /home/user/out/find-parse/gatc-ts.patch. Full prototype source: /home/user/out/find-parse/gatc/src/.

- **Branch-free quote handling for event lines with AVX2 byte gathers (LineIter::next7)** (src/main.rs: LineIter::next (lines ~549-598) and events_block field use (~698-760); est About -10% Ir overall (-207M on bench2). About 13% less parse CPU, roughly 150-200 ms user CPU on bench20 and roughly 40-80 ms phase1 wall at 4 threads (noisy).; risk low)
  - evidence: Baseline: LineIter is about 215M Ir on bench2 (10%); the two per-field quote loads alone are 131M Ir, and 1.3G Ir plus 23M D1 misses on bench20. The ql==0 branch mispredicts 753K times on bench2 because 79% of lines have quotes, chosen at random. There are also 244M Ir of spills in the hot loop. Prototype gat: Ir -207.6M (-10.0%), unattributed spill Ir 244M -> 117M, GATE PASS. Single-thread A/B parse CPU min 1232 -> 1044 ms, median 1498 -> 1270. 4-thread phase1 median 756 -> 673 ms (noisy).
  - proposal: Add `next7(&mut self, sb, text, fs:&mut [u32;8], fl:&mut [u32;8]) -> bool`. Keep the same le/ql/field-count logic. Then: cv=_mm256_loadu_si256(commas+ci); e=blend_epi32::<0xC0>(cv, set1(le)) gives [c0..c5,le,le]; s=blend::<1>(permutevar8x32(cv+1,[0,0,1,2,3,4,5,5]), set1(ls)). gs=_mm256_i32gather_epi32::<1>(text,s); ge=gather::<1>(text-1,e). q=cmpeq(gs&0xff,'"'); len=e-s; closed=cmpeq(ge&0xff,'"') & cmpgt(len,1). qm/cm=movemask_ps(...)&0x7f. Store fs=s-q and fl=len+2q. Return (qm&!cm)==0 && popcnt(qm)*2==ql. In events_block use fl[j] in place of fe[j]-fs[j]. Lane 7 duplicates lane 6, so every gather address stays valid, and the reads match the scalar version: text[s], text[e-1], plus up to 3 bytes past, which the scan's 63-byte overread already allows. Exact code is in /home/user/out/find-parse/gatc-main.patch.
- **SWAR fast path for 9..16-digit numbers inside out-of-line parse_digits** (src/ts.rs: parse_digits (and the call from parse_ts for len 16); est About -1.2% Ir (-25M bench2, about -370M bench20), plus fewer mispredicts. Roughly 10-15 ms user CPU on bench20.; risk low)
  - evidence: 10% of event ts values are 16 digits with leading zeros ('000' + 13). These fall through to the scalar parse_digits loop: 41M Ir (2%) on bench2 and 414M Ir on bench20, plus about 330K simulated mispredicts on bench2. Putting the fast path inline in parse_ts made phase1 spill more (+41M or +9.5M net Ir), so it has to stay in the #[inline(never)] parse_digits. Prototype tsw3: Ir -25.0M (-1.2%), phase1 Ir byte-identical, GATE PASS. A new fuzz test (3M cases, len 9..16, leading zeros and bad bytes) matches str::parse.
  - proposal: At the top of parse_digits: `if len.wrapping_sub(9) <= 7 && len <= max_digits { a=load_u64(p)^ZEROS; b=load_u64(p+len-8)^ZEROS; if digits8(a)&&digits8(b) { b &= !0u64 << ((16-len)*8); return (swar8(a)*POW10[len-8] + swar8(b)) as i64 } return BAD }` with `static POW10:[u64;9]`. Leave parse_ts unchanged. Exact code is in /home/user/out/find-parse/gatc-ts.patch.
- **Same gather treatment for click lines (LineIter::next4 with 128-bit gathers)** (src/main.rs: clicks_block (~770-828), uses LineIter::next::<4>; est About -0.8% Ir on bench2. More on click-heavy inputs; a few ms of user CPU on bench20.; risk low)
  - evidence: Prototype gatc on top of gat: Ir 1,839.7M -> 1,823.4M (-16.3M on bench2, which has 0.6M click lines). GATE PASS. Single-thread clicks CPU min 272 -> 227 ms (gat already got 213, so this is noisy). Gains should be larger on click-heavy data such as bench10b (3M clicks).
  - proposal: next4: cv=_mm_loadu_si128(commas+ci); e=_mm_blend_epi32::<0b1000>(cv,set1(le)); s=_mm_alignr_epi8::<12>(cv, set1(ls-1)) + 1, giving [ls,c0+1,c1+1,c2+1]. Then two _mm_i32gather_epi32::<1> and the same q/closed/movemask logic with no lane mask. Use fl[k] in clicks_block. Links (N=2) can stay scalar. The code is in gatc-main.patch.
- **SIMD ingest_ts (F1 'YYYY-MM-DDTHH:MM:SSZ') parse with the length/Z check folded in** (src/ts.rs: parse_f1_seconds / datetime19c (also parse_ts len==20); est Estimated -10 to -15 Ir per row, about -1.3% Ir. Unmeasured.; risk medium)
  - evidence: bench20 cachegrind: the datetime19c lines (slot 149M, cache compare 85M, HH:MM:SS SWAR plus range checks about 470M) and the len/Z check (85M) total about 800M Ir, about 4% of the program, at about 38 Ir per row. Every event row parses ingest_ts. No prototype.
  - proposal: For the 'Z' form: v=_mm_loadu_si128(p+4) covers "-MM-DDTHH:MM:SSZ". XOR with a pattern whose byte 15 is 'Z' (limit 0) and validate everything with one max_epu8/cmpeq/movemask. pshufb plus maddubs give MM, DD, HH, MI, SS lanes; one cmpgt against [12,31,23,59,59] does the range check; madd plus one horizontal add gives seconds of day. Keep the existing DateCache: same slot formula and key bytes, with date_slow on a miss, so day-in-month validation stays exact. Fuzz against the existing datetime_matches_reference test.
- **Merge the validity range compares in events_block** (src/main.rs events_block `valid` expression (~713-721); est About 0.2-0.4% Ir. Small.; risk low)
  - evidence: After gather, the `valid` lines still cost about 50M Ir on bench2 (23.3M on the first line alone plus about 4-6M for each of the others). t!=BAD & t>=ws & t<we is three compares where one would do.
  - proposal: Replace `(t != BAD) & (t >= ws) & (t < we)` with `((t.wrapping_sub(ws)) as u64) < ((we - ws) as u64)`. BAD = i64::MIN wraps to a huge unsigned value and fails the test. Keep the `ing != BAD` test: ing*1000 wraps to 0 for BAD, so it cannot be folded into the late check, and the signed `<= DAY_MS` must stay because ingest can be earlier than ts. Verify Ir with cachegrind, since codegen around this loop is sensitive to register pressure (see the rejected inline-parse_ts experiments).

## ID hashing, interning and linked-ID union-find

**Measurements:** Machine: 4 vCPU shared with other agents. 4-thread wall and user-CPU numbers moved by plus or minus 10-20% between runs, so the per-subtask PIPE_TIMING medians (shuffled/interleaved runs, 6-12 rounds) and the deterministic counters are the reliable figures.

bench20 basics: users = 1.43M; cap = 2^22 slots (64 MB, load 34%, limit 2.6M). 26.6M Intern::get calls. 884K link pairs over 1.197M distinct link IDs (most users are linked). 4.08M click lines.

1) link_people (single thread, overlaps the events parse): base link time over 12 interleaved runs at PIPE_THREADS=1 was median 77-83 ms (min 62). Instrumented split: union pass (pass 1) about 10 ms; naming + flatten passes about 50-66 ms. So the cost is the naming compare: id_bytes() reads random places in the 26 MB links text, then memcmp. Average find depth is 1.61, max 53.
   - Prefetch in the union pass plus merging passes 2 and 3: median 64.5 ms.
   - Prefix-key prototype (/home/user/out/find-intern/exp2, diff at /home/user/out/find-intern/link_people_prefixkey.diff): median 39 ms (min 32) against a base median of 83 in the same interleaved set. check.sh printed GATE PASS (36/36 fixtures, bench2/20/10b, thread counts 1 and 3).

2) Intern probe misses that cross into another cache line. Deterministic counter in an instrumented build, counting probe touches to a 64 B line that was not prefetched, out of 26.6M gets:
   - base (cap 2^22): 1,710,784 (6.4% of gets)
   - extra prefetch of slot i+1: 743K; i+2: 350K; i+3: 257K; i+4 (always the next line): 116K
   - cap 2^23 alone: 487,835; cap 2^23 plus i+2: 16,894
   - bench10b base: 1.08M of 14.3M gets.
   These misses explain why intern time depends on cap. PIPE_TIMING intern ms, medians:
   - 1 thread: base22 197, i+4 at cap22 156, base23 128, i+4 at cap23 120.
   - 4 threads: base22 187 (min 160), i+2 at cap22 179 (min 137), base23 133 (min 115), i+2 at cap23 131 (min 98).
   - Total user CPU was too noisy to show a clear difference. With prefetch variants the parse time (which contains the prefetch instructions) sometimes looked higher, possibly from the extra DRAM traffic (+0.85 GB for i+2, +1.7 GB for i+4).
   - sys time looked higher with cap23 (median 683 vs 430 ms at 4 threads, very noisy), from populating 64 MB more.
   - Outputs MATCH ref20 for cap23, i+2 and i+2 at cap23.

3) Branch mispredicts (cachegrind --branch-sim, bench2, 1 thread; total 7.8M mispredicts):
   - Intern::get `(w1>>32)==tag_lo`: 621K mispredicts out of 3.06M (20%). Roughly 6M on bench20, about 50 ms CPU.
   - hash128 `if len <= 16`: 658K mispredicts. About 13% of event_id and user_id values are 41-42 bytes, mixed at random with values of 16 bytes or less.
   - StaticTable::get tag check: 145K.

4) Cachegrind Ir (bench2, 1 thread): hash.rs + aes.rs about 71M Ir (3.4%); Intern::get about 40M Ir (2%). Slot loads miss D1 on almost every get, but cachegrind ignores software prefetch, so real stalls come only from lines that were not prefetched (point 2).

5) Experiment that failed: starting the probe at the line-aligned slot (`i & !3`, scalar loop) made intern worse (median 292 vs 214 ms at 1 thread). Each lookup then walks more mismatching slots, so there are more mispredicted tag-compare branches. Do not apply that change without a SIMD compare.

All prototypes are under /home/user/out/find-intern/ (src-copy is the debug-info baseline; exp2 is link_people; exp7 is the i+2 prefetch, diff at /home/user/out/find-intern/intern_prefetch2.diff).

- **link_people: 8-byte prefix keys for naming, merged naming+flatten pass, software prefetch** (src/main.rs: links_block (cx.links push), link_people, Shared.link_pairs/P1.links types; est About 40-45 ms single-thread CPU in phase 1. That is about 10 ms of phase-1 wall at 4 threads, because it runs alongside the events parse and phase 1 is throughput-bound. More on Cascade Lake, whose L3 is far smaller than the 260 MB here.; risk low)
  - evidence: Naming+flatten passes take about 50-66 ms of the about 80 ms link time (union pass about 10 ms). Each of the 1.77M ID visits reads random places in the 26 MB links text (id_bytes) and calls memcmp. In the prototype (exp2) the link time median went 83 -> 39 ms (min 64 -> 32) over 10 interleaved PIPE_THREADS=1 runs, and check.sh printed GATE PASS.
  - proposal: (a) In links_block, compute key = bswap(load_u64(p) masked to min(len,8) bytes) for both IDs while the text is hot, and push (a,b,ka,kb) (24 B) instead of (a,b). (b) In link_people, allocate a temporary per-number u64 array nkey (anon_huge of users*8, released at the end).
(c) Treat name_enc==0 as 'unset'. Every root is itself a pair member, so the final minimum is the same, and phase3's `ne==0 -> root` still holds.
(d) Merge passes 2 and 3 into one loop per x: r=find(x); x is better if cur==0, or k<nkey[r], or (k==nkey[r] && x!=cur-1 && id_bytes(x)<id_bytes(cur-1)). If better, set name_enc[r]=x+1 and nkey[r]=k. Then, if r!=x, set penc[x]=r+1. This is correct because a smaller zero-padded big-endian prefix implies a smaller string, and equal keys fall back to the full compare.
(e) Prefetch penc[a],penc[b] 16 pairs ahead in both loops. In the merged loop also prefetch name_enc[parent] and nkey[parent] 8 pairs ahead. The full diff is at /home/user/out/find-intern/link_people_prefixkey.diff. When porting, fix sys::release so it also unmaps the alignment head of anon_huge, or allocate nkey from an Arena.
- **Intern: second prefetch to cover probes that cross a cache line** (src/main.rs: Intern::prefetch (called from events_block, clicks_block, links_block); est Intern CPU -20 to -50 ms (of about 190) on bench20, also helping the clicks and links lookups. Net is likely +10 to 40 ms CPU after the extra DRAM traffic; needs A/B.; risk low)
  - evidence: Deterministic count: 1.71M of 26.6M gets touch a slot line that was never prefetched (linear probing from a home slot near the end of its 64 B line), each a DRAM miss in the get loop. Adding a prefetch of slot (i+2)&mask cuts this to 350K; (i+3) to 257K; (i+4, always the next line) to 116K. Measured intern CPU: 1 thread 197 -> 156 ms with i+4; 4 threads min 160 -> 137 with i+2. Outputs MATCH. Total user CPU was too noisy to confirm, and the parse time looked higher in some runs because of the extra prefetch traffic.
  - proposal: In Intern::prefetch, let i=(lo>>shift) and issue prefetch(slots+i) and prefetch(slots+((i+2)&mask)). When both land in the same line the second prefetch is nearly free. Try +2, +3 and +4 and keep the one that gives the lowest total user CPU over 10 or more interleaved bench20 runs. Diff for +2: /home/user/out/find-intern/intern_prefetch2.diff. Can be combined with the next candidate.
- **Intern table sizing: twice the slots at the same limit (load 34% -> 17%)** (src/main.rs: main() cap formula (/700) and Intern::new (slots vs limit); est Intern CPU -55 to -65 ms (more with the prefetch, down to about 100 ms). Minus the extra populate sys time, maybe 5-15 ms CPU. Net unclear; A/B is mandatory.; risk low)
  - evidence: With PIPE_CAP_BITS=23, the non-prefetched cross-line touches drop from 1.71M to 488K, or 17K when combined with the i+2 prefetch. Intern ms: 1 thread 197 -> 128; 4 threads 187 -> 133 (min 160 -> 115). Outputs MATCH. Cost: populating 64 MB more. The sys median rose from about 430 to 683 ms in noisy 4-thread runs, which has to be checked.
  - proposal: Size the slot array as 2x the current cap, but keep limit, id_ref/penc/name_enc and np derived from the old cap. That way partitions and array sizes do not change and only the table gets sparser. Alternatively change /700 to /350. Measure sys and total user time with interleaved runs; populate cost is split over the threads. Pick whichever of this and the prefetch candidate, or both, gives the best total CPU.
- **Intern::get: compare a whole line with SIMD instead of a mispredicting scalar tag branch** (src/main.rs: Intern::get; est About 30-50 ms CPU (fewer mispredicts, and fewer cross-line misses as a side effect). Overlaps with the prefetch and sizing candidates, so do those first.; risk medium)
  - evidence: Cachegrind branch-sim: the `(w1>>32)==tag_lo` branch mispredicts 621K of 3.06M times on bench2 (20%), roughly 6M on bench20, about 50 ms CPU. A line-aligned scalar probe made it worse (intern 214 -> 292 ms at 1 thread) because hits deeper in the line add mispredicts. So the line must be checked in one step.
  - proposal: Start probing at slot (home & !3) and load the 4 slots (64 B) with two 32 B loads. With AVX2, compare the high 32 bits of each slot's w1 to tag_lo and also test w1==0, giving one bitmask with movemask. Branch once: on a tag-hit bit, check w0 for that slot (keeping the existing w0==0 spin) and return. Else, on an empty bit, CAS the first empty slot (retry the line on failure). Else move to the next line. Prefetch stays one line per lookup, which also removes the cross-line problem except on line overflow. Concurrency: vector loads are not atomic across slots, but each 8 B lane is; the w1-then-w0 publish protocol and the CAS on w1 stay as they are.

## Phase-3 grouping, sorting, sessions, attribution

**Measurements:** Baseline (pipeline-base, debug-info copy at /home/user/out/find-sessions/src-copy), cachegrind on bench2 with PIPE_THREADS=1: 2,072.3M Ir. Phase 3 inclusive (callgrind) is 274M (13.3%). Breakdown: P3 closure self 113M; radix_sort 53M incl (80 calls, 868K elements, ~51 Ir/elem, 3 scatter passes with ~0.47 D1 misses per element per pass); u32 quicksort 51M (~1.25M events of persons with fewer than 2048 events, ~41 Ir/elem); ClS click sort 29.4M (404K clicks, ~72 Ir/click); (u64,u32) purchase sort 7.3M (~124 Ir per purchase); the session gap loop `while j<nev && evs[j]-prev<=GAP` 13.9M. Of the 72M-Ir memset, 64M comes from the phase2 dedup closure (#11, table fill), not phase 3.
Data skew (bench2 input): the top user has 94K events (4.4% of all). The 99 users with 2048+ events hold 40% of events, and the 2918 users with 64+ hold 69%. Clicks: 533 users with 64+ hold 54%. Event files are in random ts order, so tsbuf is not presorted.
Per-partition phase-3 time on bench20 (instrumented prototype): 338 partitions, 360 cpu ms in total, max 25 ms (partition 190: 1.73M events, 360K clicks). Wall phase 3 was about 6 ms above sum/4, so scheduling the largest partitions first could gain at most about 6 ms.
I prototyped candidates 1 to 3 in the scratch copy /home/user/out/find-sessions/proto (patch at /home/user/out/find-sessions/proto.diff, ~190 changed lines). It passes /home/user/harness/check.sh (GATE PASS: 36/36 fixtures, bench2/bench20/bench10b MATCH, threads 1 and 3 MATCH).
Ir on bench2 with PIPE_THREADS=1: base 2,072.26M, then 2,000.4M with bucket sessions (-71.9M), 1,986.9M adding click radix at 512+ (-13.5M), 1,982.2M adding purchase counting sort (-4.7M), and 1,979.85M final without instrumentation. Total -92.4M Ir (-4.46%).
bench20, 6 interleaved base/proto pairs (wall numbers are noisy because the 4 CPUs are shared): phase-3 cpu (group+sort+sessions) median about 750 ms base vs about 522 ms proto (-30%). Sort went from about 300-530 to about 95-118 cpu ms; sessions rose from about 170 to about 255 because the bucket pass now runs there. Phase-3 wall median about 200 ms base vs about 147 ms proto.

- **Sessionize with gap-width time buckets (min/max/count) instead of sorting event times** (src/main.rs phase3: sort loop (lines ~1304-1319, sort_times/radix_sort) and session loop (lines ~1350-1362); est -72M Ir on bench2 (-3.5% of total). About -150 to -200 cpu ms and roughly -25 to -40 ms phase-3 wall on bench20 (wall is noisy).; risk low)
  - evidence: Event sorting costs 104M Ir on bench2 (radix 53M + u32 quicksort 51M), plus 13.9M for the gap loop. Of baseline Ir, quicksort is 2.5% and radix 2.1%. Prototype measured -71.9M Ir on bench2 and cut bench20 phase-3 sort cpu from about 330 to about 115 ms (noisy, interleaved). GATE PASS.
  - proposal: A session only needs (start, end, count). Use buckets of width BW = GAP_MS+1 over event ts (relative to window_start). Two events in one bucket are at most GAP_MS apart, so a session can never break inside a bucket. Breaks can only fall between one non-empty bucket's max and the next one's min.
For persons with n >= ~8-16 events: in one pass over the unsorted slice, update bucket bk = ts/BW: bits[bk>>6] |= 1<<(bk&63), bmin[bk] = min, bmax[bk] = max, bcnt[bk] += 1, and track the lowest and highest bitmap word. Then walk the set bits from low to high with tzcnt, resetting each bucket as you go. Merge a bucket into the current session if bmin - cur_end <= GAP_MS; otherwise close the session and start a new one at bmin. Push (start, end, count) into a small per-person Vec and run the existing day/purchase/click code over that list.
nbk = ndays*DAY_MS/BW + 2 (about 674 for 14 days). Allocate per thread, and fall back to the old sort when nbk > 8192. Small persons keep sort_unstable plus the gap loop. radix_sort and sort_times then become dead for events. Exact prototype code is in /home/user/out/find-sessions/proto.diff.
- **LSD radix sort for large per-person click arrays (ClS)** (src/main.rs phase3 line ~1317 `cls.sort_unstable_by_key(|c| c.ts)`; est -13M Ir on bench2 (-0.65%). More wall benefit on whale partitions (300K+ element quicksorts).; risk low)
  - evidence: The ClS sort costs 29.4M Ir on bench2 (~72 Ir/click). Clicks are concentrated in whales: on bench20, partition 190 has 360K clicks. Prototype with a 3-pass (11/11/10-bit) radix on ts for cls.len() >= 512 measured -13.5M Ir on bench2. Thresholds 128, 256, 512 and 1024 gave -8M, -11.8M, -12.8M and -12.7M. GATE PASS.
  - proposal: Add radix_cls(v: &mut [ClS], tmp: &mut Vec<ClS>) as a copy of radix_sort keyed on .ts that moves the 16-byte structs. Use it when cls.len() >= 512 and keep sort_unstable_by_key below that. Order is safe: the tie-break scans all equal-ts clicks and picks the smallest click_id, so stable versus unstable order cannot change the result. Grow tmp only (no resize memset per call).
- **Group purchases by counting sort per person instead of a global (u64,u32) comparison sort** (src/main.rs phase3 lines ~1264-1272 (pus build and sort) and the purchase loop at ~1380; est -4.7M Ir on bench2 (-0.23%); risk low)
  - evidence: The pus sort_unstable_by_key costs 7.3M Ir (~124 Ir per purchase) on bench2. Prototype: counting sort by local person (pcnt/ppos arrays like cnt/pos), pbuf: Vec<u64> = ts<<32|pc, a per-person sort_unstable on the usually tiny slice, and a per-person pcur. Measured -4.7M Ir. GATE PASS.
  - proposal: Mirror the event counting sort: histogram pu records by (person-base), prefix-sum, scatter u64 (ts<<32 | pc) into pbuf. In the session loop, take pbuf[pcnt[i]..pcnt[i+1]], sort_unstable it if len > 1, and consume while key <= (end_ts<<32 | 0xffffffff). Sorting by ts then pc does not affect the sums.
- **Schedule phase-3 partitions largest-first (LPT)** (src/main.rs phase3 work loop `next.fetch_add` over 0..np_used; est 0-6 ms wall on bench20 (noisy, wall-clock only); no Ir change; risk low)
  - evidence: bench20 per-partition cpu: 338 partitions, 360 ms total, max 25 ms (partition 190), next 21, 20, 16 and 12 ms, taken in index order. Phase-3 wall was about 6 ms above (sum cpu + write)/4 in an instrumented run.
  - proposal: Before phase 3, compute n_ev per partition (sum of p2 ev.count) and sort partition indices by descending n_ev. The phase-3 loop then indexes this order array, so whale partitions never land at the tail.

## Phase-2 dedup and scatter (phase2_dedup / phase2_scatter in src/main.rs, plus the Wc<T> partition store they read and write)

**Measurements:** Setup: the baseline binary, plus debug-info and prototype copies under /home/user/out/find-dedup/. I never touched the repo. Wall time was very noisy because other agents were running pipelines (the same binary's dedup ranged 121-218 ms single-threaded). So I added a process-CPU timer (clock_gettime CLOCK_PROCESS_CPUTIME_ID) around dedup and around scatter, and a PIPE_REP=3 loop that reruns both phases in-process and reports the minimum. Dedup is idempotent, so rerunning it is safe. Each value below is a median over 5-6 interleaved runs of that minimum, on bench20, in CPU ms.

Baseline breakdown:
- bench20, 4 threads: dedup ~50 ms wall and ~133-148 ms CPU; scatter ~60 ms wall and ~130-165 ms CPU. The task's quoted 34/37 ms wall figures were on a quieter machine.
- bench20, 1 thread: dedup ~120-200 ms, scatter ~160-180 ms. That is about 6-10 ns per row for each phase.

Cachegrind, bench2, 1 thread (2.12M rows):
- Scatter: 116.7M Ir, about 55 instructions per row. objdump of the loop shows about 13 stack spills/reloads per row, and the intern.penc pointer is reloaded every row. Branch mispredicts: 1.10M.
- Dedup: 35.9M Ir (17 per row), 1.02M mispredicts. 0.56M of those come from the `s == 0` probe branch (22% mispredicted) at line 1041. The probe at line 1040 causes 1.96M D1 misses on the 256 KB per-partition table.
- About 6% of rows go to pending, which matches the 2-4% duplicated events in rules.md.

Prototypes. Every one gave MATCH against ref2, and the scatter and dedup winners also against ref10b.
- scat3 (scatter loop rewrite): scatter Ir 116.7M -> 90.7M (-22%). Whole-program Ir 2,074.4M -> 2,048.6M (-1.25%). Mispredicts down 150k. CPU: 1 thread 139.8 -> 124.6 ms median (min 129.9 -> 112.7); 4 threads 137.2 -> 128.0 median (min 118.8 -> 106.9).
- pf48 (penc prefetch distance 16 -> 48 rows, nothing else): first-run single-thread scatter medians 172 -> 135 and 160 -> 148; with PIPE_REP 143 -> 131.
- big (dedup table at about 0.15 load instead of 0.3, tsize = (total*4).next_power_of_two()): dedup CPU 4 threads 148.0 -> 124.0 median (min 133 -> 116.5). 1 thread 129-142 -> 111-113, consistently across three batches. Whole-program Ir goes UP (2,074M -> 2,137M) because of the larger memset, so judge this one on CPU ms, not Ir.

Rejected after measuring:
- Hot/cold Row split (8-byte uidx+tsv array, 8-byte ing+h2 array): dedup CPU 133 -> 156, scatter no gain.
- Bucketized AVX2 table (8 tags + 8 row refs per 64 B line): mispredicts down 505k, but dedup CPU 131-141, no better than base.
- Clearing only the used slots instead of the memset: slower (169 vs 142).
- Load factor 0.6 table: dedup 136 -> 222.
- Software prefetch of table slots 8 records ahead: no gain (176 vs 173).

Prototype sources (one diff each from HEAD):
- /home/user/out/find-dedup/exp8/src/main.rs (scat3)
- /home/user/out/find-dedup/exp4/src/main.rs (big)
- /home/user/out/find-dedup/exp6 (tracked clearing)
- /home/user/out/find-dedup/exp5 (bucket)

These copies also carry the CPU/REP instrumentation; it is in main() only.

- **Rewrite the scatter inner loop as a tight out-of-line run between purchase rows** (src/main.rs: phase2_scatter, the per-row loop at lines ~1110-1131, and Wc::push; est About 10% of scatter CPU (median). That is roughly 4-6 ms wall of the ~37-60 ms scatter at 4 threads, and -1.25% Ir overall. Likely larger on Cascade Lake, where penc lookups miss further out (smaller L3).; risk low)
  - evidence: Cachegrind: 55 instructions per row in phase2_scatter (116.7M Ir for 2.12M rows). The disassembly shows the loop is starved for registers: about 13 spills/reloads to rsp+0x8..0x190 per row, sh.intern.penc reloaded from [r10+0x150] every row, and out.ev heads/wc reloaded through the struct. Per row the loop also does `i+16<used`, `i%4`, a 5-instruction is_pur compare, and a branch on VALID, which cachegrind counts at 174k mispredicts (8% of rows). Prototype scat3 cut scatter Ir 22% (whole program -25.8M Ir, -1.25%) and mispredicts by 150k. Scatter CPU, median of interleaved runs: 1 thread 139.8 -> 124.6 ms, 4 threads 137 -> 128 ms. Min values: 129.9 -> 112.7 and 118.8 -> 106.9.
  - proposal: In phase2_scatter, walk the chunk in runs that end at the next purchase row: stop = pur[pcur].0 - first, capped at used. Call an #[inline(never)] fn scatter_run(base, from, to, used, penc, &mut Wc<PEv>, &mut Arena) for each run, then handle the single purchase row inline (ev push, plus pu push if valid) and advance pcur.

Inside scatter_run:
- Hoist heads = ev.heads.as_mut_ptr(), wc = ev.wc and penc into locals.
- Split into a main loop i < min(to, used-PF), which always prefetches penc[base[i+PF].uidx], and a tail loop with no prefetch.
- Make validity branchless: valid = tsv>>31; u = valid ? uidx : 0 (a cmov, so no out-of-bounds read of penc[u32::MAX]); always write PEv into slot cnt&7 of the line; cnt += valid; flush only if (idx==7) & valid.
- Move block allocation into a #[cold] #[inline(never)] Wc::new_block(arena, part) that pops a spare or allocates a 4 KB block, pushes the Blk and sets cur/last. The flush itself keeps the inline 2x vmovntdq.

Writing the record without advancing cnt is safe: the slot is the next free one and is overwritten later, and finish() copies only cnt&7 slots. Working code: /home/user/out/find-dedup/exp8/src/main.rs (functions scatter_run and Wc::new_block, and the new body of phase2_scatter; ignore the CPU2/REPMIN instrumentation in main).
- **Raise the penc prefetch distance in scatter from 16 to 48 rows** (src/main.rs: phase2_scatter, lines 1112-1113 (`i + 16`); est 5-15% of scatter CPU (noisy). It overlaps with candidate 1, so the gains do not add up.; risk low)
  - evidence: Each row does a random penc[uidx] lookup into a 5.7 MB array on bench20. At about 7 ns per row, 16 rows is about 110 ns of lead, roughly one DRAM latency with nothing to spare. Cachegrind: line 1113 accounts for 10% of all LL read misses and line 460 (penc load) for 4.8% of D1 read misses. Changing only the constant: single-thread first-run scatter medians 172 -> 135 ms and 160 -> 148 ms in two batches; with PIPE_REP, 143 -> 131. 96 was no better than 48 (138 vs 135).
  - proposal: Change both `16`s to 48 (a const PF). Already included in candidate 1. Do this alone if candidate 1 is not taken.
- **Halve the dedup hash-table load factor (tsize = 4x total, capped at 64K entries)** (src/main.rs: phase2_dedup, line 1025 `let tsize = (total * 2).next_power_of_two().max(64);`; est About 15% of dedup CPU: roughly 5-8 ms wall of the ~34-50 ms dedup at 4 threads. Ir rises about 3% because of the memset, so the cachegrind gate will look worse. Validate with user CPU ms and interleaved wall runs on the Cascade Lake target, where L2 is half this host's.; risk low)
  - evidence: The insert loop is bound by latency and mispredicts, not instruction count: 17 instructions per row but about 6 ns per row. Dedup time is very sensitive to load factor. At 0.3 (current) CPU was 136 ms. At about 0.6 ((total*3/2).next_pow2) it was 222 ms, +63%. At about 0.15 (total*4) it was 112-122 ms. On 4 threads, bench20: 148 -> 124 ms CPU median (min 133 -> 116.5). Cachegrind shows mispredicts down only 274k, and Ir up 62M because the memset is twice as big. So use CPU ms, not Ir, to accept this. A bucketized SIMD table cut mispredicts more (-505k) but was not faster, and tracked clearing in place of the memset was slower, so the plain bigger table is the change that wins.
  - proposal: Use tsize = (total*4).next_power_of_two().min(1<<16).max((total*2).next_power_of_two()).max(64). The 1<<16 cap (512 KB) keeps the table well inside Cascade Lake's 1 MB L2 together with the fp stream. Partitions stay at about 10-16k rows with the current pe_bits rule (pe capped at 2048), so the cap only matters for inputs much bigger than bench20. Nothing else changes; insert order and tie handling are untouched, and rules.md says equal-ingest copies are identical anyway. Prototype: /home/user/out/find-dedup/exp4/src/main.rs.

