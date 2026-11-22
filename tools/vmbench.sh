#!/bin/bash
# Benchmark builds of this repository on a machine like the scorer (GCE n2-standard-8).
#
#   bash vmbench.sh [REF ...]        REF = branch or commit to measure (default: main)
#
# Optional environment: RUNS=5  SETS="d20 d60"  PERF=1 (time profile)  DOCKER=1 (container timing)
# The first run installs a compiler and Rust, builds the data generator from ~/generator.zip and
# makes a 20M-event and a 60M-event dataset (about 8 GB, a few minutes). Later runs reuse them.
set -uo pipefail
REPO=${REPO:-https://github.com/ykulk11/DB-Shopping-Sessions.git}
W=${W:-$HOME/bench}
RUNS=${RUNS:-5}
SETS=${SETS:-"d20 d60"}
mkdir -p "$W/out" "$W/data"
log() { echo "[vmbench] $*"; }

if ! command -v cc >/dev/null 2>&1 || ! command -v unzip >/dev/null 2>&1 || ! command -v git >/dev/null 2>&1; then
  log "installing build tools"
  sudo apt-get update -qq >/dev/null 2>&1
  sudo DEBIAN_FRONTEND=noninteractive apt-get install -y -qq build-essential git unzip python3 curl >/dev/null 2>&1
fi
if ! command -v cargo >/dev/null 2>&1 && [ ! -x "$HOME/.cargo/bin/cargo" ]; then
  log "installing Rust 1.97"
  curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain 1.97.0 >/dev/null 2>&1
fi
[ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"

find_gen() { find "$W/gen" -path '*target/release/generate' -type f 2>/dev/null | head -1; }
need_data=0
for D in $SETS; do [ -f "$W/data/$D/manifest.json" ] || need_data=1; done
if [ $need_data = 1 ]; then
  GEN=$(find_gen)
  if [ -z "$GEN" ]; then
    if [ ! -f "$HOME/generator.zip" ]; then
      log "generator.zip is not in $HOME: upload it there and run this again"
      exit 1
    fi
    mkdir -p "$W/gen" && unzip -q -o "$HOME/generator.zip" -d "$W/gen"
    GD=$(dirname "$(find "$W/gen" -name Cargo.toml -not -path '*/target/*' | head -1)")
    log "building the generator"
    (cd "$GD" && cargo build --release 2>&1 | tail -1)
    GEN=$(find_gen)
  fi
  for D in $SETS; do
    [ -f "$W/data/$D/manifest.json" ] && continue
    case $D in
      d20) log "generating 20M events"; "$GEN" --seed 7 --events 20000000 --clicks 5000000 --output "$W/data/d20" >/dev/null ;;
      d60) log "generating 60M events"; "$GEN" --seed 9 --events 60000000 --clicks 15000000 --output "$W/data/d60" >/dev/null ;;
      *) log "unknown set $D" ;;
    esac
  done
fi

log "cpu: $(lscpu | sed -n 's/^Model name: *//p' | head -1) | $(nproc) cpus, $(lscpu | awk -F: '/Thread\(s\) per core/{gsub(/ /,"",$2); print $2}') threads per core"
log "caches: L2 $(lscpu | sed -n 's/^L2 cache: *//p') | L3 $(lscpu | sed -n 's/^L3 cache: *//p')"
log "kernel $(uname -r) | huge pages: $(cat /sys/kernel/mm/transparent_hugepage/enabled 2>/dev/null) | defrag: $(cat /sys/kernel/mm/transparent_hugepage/defrag 2>/dev/null) | memory $(free -g | awk '/^Mem/{print $2}') GB"

[ -d "$W/repo/.git" ] || git clone -q "$REPO" "$W/repo"
cd "$W/repo" || exit 1
git fetch -q origin
[ $# -gt 0 ] || set -- main
BINS=""
for REF in "$@"; do
  git checkout -q --detach "origin/$REF" 2>/dev/null || git checkout -q --detach "$REF" 2>/dev/null || { log "unknown ref $REF"; continue; }
  SHA=$(git rev-parse --short HEAD)
  if [ ! -x "$W/bin-$SHA" ]; then
    cargo build --release 2>&1 | tail -1
    cp target/release/pipeline "$W/bin-$SHA" || { log "build of $REF failed"; continue; }
  fi
  BINS="$BINS $REF=$W/bin-$SHA"
  if [ "${DOCKER:-0}" = 1 ]; then
    command -v docker >/dev/null 2>&1 || { log "installing docker"; sudo DEBIAN_FRONTEND=noninteractive apt-get install -y -qq docker.io >/dev/null 2>&1; }
    sudo docker build -q -t "bench:$SHA" . >/dev/null 2>&1 || log "docker build of $REF failed"
  fi
done

for D in $SETS; do cat "$W/data/$D"/*.csv "$W/data/$D"/*/*.csv > /dev/null 2>&1; done

W="$W" RUNS="$RUNS" SETS="$SETS" BINS="$BINS" VARIANTS="${VARIANTS-}" python3 - <<'EOF'
import os, re, shutil, statistics, subprocess, time, resource
W = os.environ["W"]; runs = int(os.environ["RUNS"]); sets = os.environ["SETS"].split()
bins = [b.split("=", 1) for b in os.environ["BINS"].split()]
extra = [v for v in os.environ["VARIANTS"].split(";") if v.strip()]
variants = {"d20": ["", "PIPE_PIN=1", "PIPE_THREADS=4", "PIPE_NO_HUGE=1", "PIPE_BLOCK_KB=4", "PIPE_BLOCK_KB=8", "PIPE_BLOCK_KB=16",
                    "PIPE_BLOCK_KB=8 PIPE_PIN=1", "PIPE_SPLIT_ROWS=4096", "PIPE_NO_DIRECT=1"],
            "d60": ["", "PIPE_PIN=1", "PIPE_NO_HUGE=1", "PIPE_SPLIT_ROWS=100000000", "PIPE_SPLIT_ROWS=8192",
                    "PIPE_PE_BITS=12", "PIPE_BLOCK_KB=8", "PIPE_BLOCK_KB=8 PIPE_PIN=1"]}
names = ["wall", "total", "setup", "phase1", "phase2", "phase3", "finish", "user", "sys"]
print("%-18s %-4s %-22s %s  rows" % ("build", "set", "variant", " ".join("%7s" % n for n in names)))
for ref, binary in bins:
    for d in sets:
        for var in variants.get(d, [""]) + extra:
            env = dict(os.environ, PIPE_TIMING="1")
            for kv in var.split():
                k, v = kv.split("=", 1); env[k] = v
            rows = []; nrows = "?"; detail = {}
            for _ in range(runs):
                out = os.path.join(W, "out", "o"); shutil.rmtree(out, ignore_errors=True)
                b = resource.getrusage(resource.RUSAGE_CHILDREN); t = time.perf_counter()
                p = subprocess.run([binary, os.path.join(W, "data", d), out], env=env, capture_output=True, text=True)
                wall = (time.perf_counter() - t) * 1e3; a = resource.getrusage(resource.RUSAGE_CHILDREN)
                m = re.search(r"setup ([\d.]+) ms \| phase1 ([\d.]+) \| phase2 ([\d.]+) \| phase3 ([\d.]+) \| finish ([\d.]+) \| total ([\d.]+)", p.stderr)
                r = re.search(r"rows=(\d+)", p.stderr)
                if p.returncode != 0 or not m:
                    print("run failed:", ref, d, var, p.returncode, p.stderr[-200:]); continue
                if r: nrows = r.group(1)
                s = [float(x) for x in m.groups()]
                rows.append([wall, s[5], s[0], s[1], s[2], s[3], s[4], (a.ru_utime - b.ru_utime) * 1e3, (a.ru_stime - b.ru_stime) * 1e3])
                detail[wall] = [l for l in p.stderr.splitlines() if "cpu ms" in l or "threads=" in l or "dedup" in l]
            if not rows: continue
            best = min(rows, key=lambda x: x[0])
            print("%-18s %-4s %-22s %s  %s" % (ref[:18], d, (var or "default") + " min", " ".join("%7.0f" % x for x in best), nrows))
            print("%-18s %-4s %-22s %s" % ("", "", "median wall", "%7.0f" % statistics.median(x[0] for x in rows)))
            if not var:
                for l in detail.get(best[0], []):
                    print("      " + l)
EOF

if [ "${PERF:-0}" = 1 ]; then
  command -v perf >/dev/null 2>&1 || sudo DEBIAN_FRONTEND=noninteractive apt-get install -y -qq linux-tools-common "linux-tools-$(uname -r)" >/dev/null 2>&1
  for B in $BINS; do
    log "time profile of ${B%%=*} on d20 (share of samples by function)"
    sudo perf record -q -e cpu-clock -F 1999 -o "$W/perf.data" -- "${B#*=}" "$W/data/d20" "$W/out/o" >/dev/null 2>&1
    sudo perf report -i "$W/perf.data" --stdio --sort symbol 2>/dev/null | grep -v '^#' | grep -v '^$' | head -28 | cut -c1-120
  done
fi

if [ "${DOCKER:-0}" = 1 ]; then
  for B in $BINS; do
    SHA=$(basename "${B#*=}"); SHA=${SHA#bin-}
    log "container timing of ${B%%=*} on d20 with the scorer's flags: start-to-exit seconds per run"
    for i in 1 2 3 4 5; do
      O=$(mktemp -d); S=$(mktemp -d); chmod 777 "$O" "$S"
      C=$(sudo docker create --network none --read-only --tmpfs /tmp:rw,size=64m,mode=1777 --cap-drop ALL \
        --security-opt no-new-privileges --pids-limit 4096 --cpus 8 --memory 24g --memory-swap 24g --user 1000:1000 \
        -v "$W/data/d20":/data:ro -v "$O":/output -v "$S":/scratch \
        -e DATA_DIR=/data -e OUTPUT_DIR=/output -e SCRATCH_DIR=/scratch -e HOME=/scratch -e NCPU=8 -e MEM_GB=24 "bench:$SHA")
      T0=$(date +%s.%N); sudo docker start -a "$C" >/dev/null 2>&1; T1=$(date +%s.%N)
      sudo docker inspect -f '{{.State.StartedAt}} {{.State.FinishedAt}} {{.State.ExitCode}}' "$C" | python3 -c "
import sys, datetime
a, b, code = sys.stdin.read().split()
f = lambda s: datetime.datetime.fromisoformat(s[:26].rstrip('Z')).timestamp()
print('  run $i: started-to-finished %.3f s, docker start wall %.3f s, exit %s' % (f(b) - f(a), $T1 - $T0, code))"
      sudo docker rm "$C" >/dev/null; sudo rm -rf "$O" "$S"
    done
  done
fi
log "done"
