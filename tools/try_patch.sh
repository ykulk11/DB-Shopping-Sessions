#!/bin/sh
# Applies a patch to this checkout and runs the checks that decide whether to keep it:
# unit tests, the quick gate and the instruction count. Leaves the change in the working tree.
# usage: tools/try_patch.sh PATCH_FILE
set -u
ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT" || exit 2
X=${XOUT:-/home/user/out/t1}
export XOUT="$X"
if [ -n "$(git status --porcelain -- src)" ]; then echo "working tree has changes in src/: commit or revert first"; exit 2; fi
git apply --3way "$1" 2>&1 | tail -3
if git diff --name-only --diff-filter=U | grep -q .; then echo "CONFLICTS: resolve by hand"; exit 1; fi
git diff --cached --stat | tail -4
cargo build --release 2>&1 | grep -E "^error|Finished" -A8 | head -14
cargo test --release 2>&1 | grep -E "test result|panicked"
tools/gate.sh quick 2>&1 | grep -E "FAIL|GATE"
tools/icount.sh t1 2>&1 | grep -E "MATCH|DIFFER|PROGRAM TOTALS"
