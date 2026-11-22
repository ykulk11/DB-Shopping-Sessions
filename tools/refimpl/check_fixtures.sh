#!/bin/sh
# Runs refimpl on every fixture and compares both tables with the hand-written expected CSVs.
# usage: tools/refimpl/check_fixtures.sh [FIXTURES_DIR]     (build first: cargo build --release)
# Prints PASS or FAIL per case and a final count; exits 1 if any case fails.
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
BIN=$HERE/target/release/refimpl
FIXTURES=${1:-$ROOT/fixtures}
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
if [ ! -x "$BIN" ]; then
  echo "missing $BIN: run cargo build --release in $HERE" >&2
  exit 2
fi
passed=0
total=0
for input in "$FIXTURES"/*/input; do
  [ -d "$input" ] || continue
  case_dir=$(dirname "$input")
  name=$(basename "$case_dir")
  total=$((total + 1))
  out=$TMP/$name
  if "$BIN" "$input" "$out" >"$TMP/$name.log" 2>&1 &&
     python3 "$ROOT/scripts/compare.py" "$out" "$case_dir/expected" >>"$TMP/$name.log" 2>&1; then
    echo "PASS $name"
    passed=$((passed + 1))
  else
    echo "FAIL $name"
    sed 's/^/    /' "$TMP/$name.log"
  fi
done
echo "$passed/$total fixtures passed"
[ "$total" -gt 0 ] && [ "$passed" -eq "$total" ]
