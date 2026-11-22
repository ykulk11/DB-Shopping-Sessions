#!/bin/sh
# Instruction count of the build in this checkout on the 1M dev set (callgrind).
# usage: tools/icount.sh [NAME]    NAME keeps concurrent runs apart (default "main")
# Prints the program total and the most expensive functions. About 1-2 minutes.
set -eu
ROOT=$(cd "$(dirname "$0")/.." && pwd)
NAME=${1:-main}
X=${XOUT:-/home/user/out}
T="$X/icount-$NAME"
mkdir -p "$T"
cd "$ROOT"
CARGO_PROFILE_RELEASE_DEBUG=line-tables-only cargo build --release --target-dir "$T/target" 2>&1 | tail -1
rm -rf "$T/out"
valgrind --tool=callgrind --callgrind-out-file="$T/callgrind.out" \
  "$T/target/release/pipeline" /home/user/data/dev "$T/out" > "$T/valgrind.log" 2>&1
python3 scripts/compare.py "$T/out" /home/user/data/dev-answer | tail -1
callgrind_annotate "$T/callgrind.out" > "$T/annotate.txt" 2>/dev/null
grep "PROGRAM TOTALS" "$T/annotate.txt"
sed -n '/file:function/,+16p' "$T/annotate.txt" | cut -c1-150
echo "full listing: $T/annotate.txt"
