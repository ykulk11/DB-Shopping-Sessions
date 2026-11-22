#!/bin/sh
# Cross-check the pipeline against the independent Python reference on one dataset.
# usage: tools/xcheck.sh DATA_DIR NAME
set -eu
ROOT=$(cd "$(dirname "$0")/.." && pwd)
DATA=$1; NAME=$2
OUT=/home/user/out/xcheck/$NAME
rm -rf "$OUT"; mkdir -p "$OUT/pipe" "$OUT/ref"
"$ROOT/target/release/pipeline" "$DATA" "$OUT/pipe"
if [ ! -f "/home/user/out/refcache/$NAME/user_daily.csv" ]; then
  mkdir -p "/home/user/out/refcache/$NAME"
  python3 "$ROOT/tools/pyref/ref.py" run "$DATA" "/home/user/out/refcache/$NAME"
fi
python3 "$ROOT/scripts/compare.py" "$OUT/pipe" "/home/user/out/refcache/$NAME" | tail -4
