#!/bin/sh
# Correctness gate for the build in this checkout.
# usage: tools/gate.sh [quick|full]
#   XOUT=dir      where outputs go (default /home/user/out); use a private dir when several run at once
#   FUZZ_FROM=n   first fuzz seed (default 20000)
set -u
ROOT=$(cd "$(dirname "$0")/.." && pwd)
MODE=${1:-quick}
X=${XOUT:-/home/user/out}
export XOUT="$X"
mkdir -p "$X"
P="$ROOT/target/release/pipeline"
F=${FUZZ_FROM:-20000}
fail=0
cd "$ROOT" || exit 2
cargo build --release 2>&1 | tail -1
check() {
  name=$1; shift
  out=$("$@" 2>&1 | tail -1)
  case "$out" in
    MATCH|*"36/36 fixtures passed"*|*"failures=0"*|*"test result: ok"*) printf '%-32s ok   %s\n' "$name" "$out" ;;
    *) printf '%-32s FAIL %s\n' "$name" "$out"; fail=1 ;;
  esac
}
units() { cargo test --release 2>&1 | grep "test result"; }
devcmp() {
  rm -rf "$X/gate-dev"
  env "$@" "$P" /home/user/data/dev "$X/gate-dev" && python3 scripts/compare.py "$X/gate-dev" /home/user/data/dev-answer
}
fuzz() { python3 tools/fuzz/run.py "$1" "$2" --scale "$3" --keep-going --pipeline "$P" --work "$X/fuzz" | grep SUMMARY; }
fuzzalt() { PIPE_SPLIT_ROWS=16 PIPE_BLOCK_KB=4 PIPE_PE_BITS=3 fuzz "$@"; }
check "unit tests" units
check "fixtures" python3 scripts/run_fixtures.py fixtures -- "$P"
check "dev answer" devcmp
check "dev, restart path" devcmp PIPE_CAP_BITS=15
check "dev, no huge pages, 8 threads" devcmp PIPE_NO_HUGE=1 PIPE_THREADS=8
check "dev, 3 threads, touch" devcmp PIPE_TOUCH=1 PIPE_THREADS=3
check "dev, 1 thread" devcmp PIPE_THREADS=1
check "dev, dedup split forced" devcmp PIPE_SPLIT_ROWS=200
check "dev, 4 KB blocks, pinned" devcmp PIPE_BLOCK_KB=4 PIPE_PIN=1
check "dev, hash product table" devcmp PIPE_NO_DIRECT=1
check "seed 2 vs python reference" tools/xcheck.sh /home/user/scratch-agents/audit-semantics/data-s2 s2
check "seed 3 vs python reference" tools/xcheck.sh /home/user/scratch-agents/audit-semantics/data-s3 s3
if [ "$MODE" = full ]; then
  check "20M vs rust reference" tools/xbig.sh /home/user/data/s7_20m s7_20m
  check "60M vs rust reference" tools/xbig.sh /home/user/data/s9_60m s9_60m
  check "fuzz small x2000" fuzz "$F" $((F + 1999)) small
  check "fuzz medium x60" fuzz "$F" $((F + 59)) medium
  check "fuzz small x500, alt settings" fuzzalt $((F + 2000)) $((F + 2499)) small
  check "fuzz medium x30, alt settings" fuzzalt $((F + 60)) $((F + 89)) medium
else
  check "fuzz small x300" fuzz "$F" $((F + 299)) small
  check "fuzz small x200, alt settings" fuzzalt $((F + 300)) $((F + 499)) small
  check "fuzz medium x10" fuzz "$F" $((F + 9)) medium
fi
if [ $fail = 0 ]; then echo "GATE PASS ($MODE)"; else echo "GATE FAIL ($MODE)"; fi
exit $fail
