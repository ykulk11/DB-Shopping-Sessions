#!/bin/sh
# Cross-check the pipeline against the independent Rust reference on a large dataset.
# usage: tools/xbig.sh DATA_DIR NAME
set -eu
ROOT=$(cd "$(dirname "$0")/.." && pwd)
DATA=$1; NAME=$2
OUT=${XOUT:-/home/user/out}/xbig/$NAME
REF=/home/user/out/refcache_rs/$NAME
rm -rf "$OUT"; mkdir -p "$OUT"
"$ROOT/target/release/pipeline" "$DATA" "$OUT"
if [ ! -f "$REF/user_daily.csv" ]; then
  mkdir -p "$REF"
  "$ROOT/tools/refimpl/target/release/refimpl" "$DATA" "$REF"
fi
python3 "$ROOT/scripts/compare.py" "$OUT" "$REF" | tail -1
