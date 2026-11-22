#!/bin/bash
# Adversarial differential test written by the review agent: one generated dirty dataset
# (quotes, separators and line ends at awkward offsets, long fields, odd files, tricky
# duplicates, clicks and links) run with several settings and compared byte for byte with
# the Python reference.
# usage: tools/adv/run.sh SEED [EVENT_BYTES]     prints "sSEED OK" when every setting matches
HERE=$(cd "$(dirname "$0")" && pwd)
REPO=$(cd "$HERE/../.." && pwd)
O=${XOUT:-/home/user/out}/adv
mkdir -p "$O/data" "$O/res"
S=$1; B=${2:-2400000}
D=$O/data/s$S
rm -rf "$D"; python3 "$HERE/gen.py" "$S" "$D" "$B" || exit 2
python3 "$REPO/tools/pyref/ref.py" run "$D" "$O/res/s$S-ref" >/dev/null || { echo "s$S REF-FAIL"; exit 2; }
fail=0
for cfg in "4 1" "8 3" "32 8" "4 8" "1024 2" "16 4 PIPE_CAP_BITS=15 PIPE_PE_BITS=1" "32 4 PIPE_SPLIT_ROWS=64 PIPE_NO_DIRECT=1"; do
  set -- $cfg
  OUT=$O/res/s$S-b$1-t$2
  rm -rf "$OUT"
  if ! env $3 $4 PIPE_BLOCK_KB=$1 PIPE_THREADS=$2 timeout 60 "$REPO/target/release/pipeline" "$D" "$OUT" >/dev/null 2>"$OUT.err"; then echo "s$S b$1 t$2 CRASH"; fail=1; continue; fi
  r=$(python3 "$HERE/cmp.py" "$OUT" "$O/res/s$S-ref" 2>&1 | head -20)
  if echo "$r" | grep -q '^MATCH'; then rm -rf "$OUT" "$OUT.err"; else echo "s$S b$1 t$2 DIFF"; echo "$r"; fail=1; fi
done
[ $fail = 0 ] && echo "s$S OK" && rm -rf "$D" "$O/res/s$S-ref"
exit $fail
