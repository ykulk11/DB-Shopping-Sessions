#!/usr/bin/env python3
"""Differential fuzzer: gen.py datasets -> optimized pipeline vs the Python reference.

  python3 run.py FIRST_SEED LAST_SEED [--scale small|medium] [--pipeline PATH] [--keep-going]
                                      [--verify] [--work DIR]

For every seed: generate a dataset into WORK/work/seed-N (seed-N-medium for --scale medium),
compute the reference rows in-process (tools/pyref/ref.py: load + pipeline), run the pipeline
binary (60 s timeout; a non-zero exit or a timeout is a failure), read its parquet output and
compare both tables with the reference as multisets of rows. A failing seed prints one FAIL
line plus a short diff and keeps its dataset and both outputs under WORK/fail/seed-N; passing
seeds are deleted. Without --keep-going the run stops at the first failing seed. --verify also
re-reads every generated dataset and checks the input guarantees (gen.check_guarantees); on a
failing seed that check always runs. The last line is 'SUMMARY seeds=N failures=K'; the exit
code is 1 when any seed failed.
"""
import json
import os
import shutil
import subprocess
import sys
import time
import traceback
from collections import Counter

sys.dont_write_bytecode = True   # no __pycache__ next to gen.py / ref.py
HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
sys.path.insert(0, os.path.join(os.path.dirname(HERE), "pyref"))

import pyarrow as pa            # noqa: E402
import pyarrow.parquet as pq    # noqa: E402

import gen                      # noqa: E402
import ref                      # noqa: E402

DEFAULT_PIPELINE = "/home/user/db-shopping-sessions/target/release/pipeline"
DEFAULT_WORK = "/home/user/scratch-agents/fuzz"
TIMEOUT = 60

USER_COLS = ["user_id", "day", "sessions", "events", "revenue_cents", "top_category"]
CAMP_COLS = ["campaign_id", "channel", "day", "sessions", "purchasing_sessions", "revenue_cents",
             "assisted_sessions"]
STRING_COLS = {"user_id", "top_category", "campaign_id", "channel"}


class Failure(Exception):
    def __init__(self, kind, detail):
        Exception.__init__(self, detail)
        self.kind = kind
        self.detail = detail


def read_table(folder, cols):
    """The rows of one output table as a Counter of tuples (strings as bytes, day as epoch days)."""
    if not os.path.isdir(folder):
        raise Failure("OUTPUT", "missing folder %s" % os.path.basename(folder))
    names = sorted(n for n in os.listdir(folder)
                   if n.endswith(".parquet") and not n.startswith((".", "_"))
                   and os.path.isfile(os.path.join(folder, n))
                   and not os.path.islink(os.path.join(folder, n)))
    if not names:
        raise Failure("OUTPUT", "no *.parquet file in %s/" % os.path.basename(folder))
    rows = Counter()
    for name in names:
        try:
            table = pq.ParquetFile(os.path.join(folder, name)).read()
        except Exception as error:   # a truncated or invalid file
            raise Failure("OUTPUT", "%s/%s is unreadable: %s" % (os.path.basename(folder), name, error))
        if sorted(table.column_names) != sorted(cols):
            raise Failure("SCHEMA", "%s/%s has columns %s" % (os.path.basename(folder), name, table.column_names))
        columns = []
        for col in cols:
            arr = table.column(col)
            typ = arr.type
            if col == "day":
                if not pa.types.is_date32(typ):
                    raise Failure("SCHEMA", "%s.day is %s, not a Parquet DATE" % (os.path.basename(folder), typ))
                columns.append(arr.cast(pa.int32()).to_pylist())
            elif col in STRING_COLS:
                if not (pa.types.is_string(typ) or pa.types.is_large_string(typ)
                        or (hasattr(pa.types, "is_string_view") and pa.types.is_string_view(typ))):
                    raise Failure("SCHEMA", "%s.%s is %s, not a string" % (os.path.basename(folder), col, typ))
                binary = pa.large_binary() if pa.types.is_large_string(typ) else pa.binary()
                if hasattr(pa.types, "is_string_view") and pa.types.is_string_view(typ):
                    arr = arr.cast(pa.large_string())
                    binary = pa.large_binary()
                columns.append(arr.cast(binary).to_pylist())
            else:
                if not pa.types.is_integer(typ):
                    raise Failure("SCHEMA", "%s.%s is %s, not an integer" % (os.path.basename(folder), col, typ))
                columns.append(arr.to_pylist())
        rows.update(zip(*columns))
    return rows


def fmt_row(row, cols):
    out = []
    for col, value in zip(cols, row):
        if col == "day" and value is not None:
            value = ref.fmt_day(value)
        elif isinstance(value, bytes) and len(value) > 80:
            value = "%r...(%d bytes)" % (value[:40], len(value))
        out.append("%s=%r" % (col, value) if not isinstance(value, str) or col != "day" else "day=%s" % value)
    return "(" + ", ".join(out) + ")"


def diff_tables(name, cols, expected, got, limit=4):
    """[] when equal, else lines describing the first differing rows."""
    if expected == got:
        return []
    only_pipeline = got - expected
    only_reference = expected - got
    lines = ["  %s: %d row(s) only in pipeline, %d only in reference (pipeline %d rows, reference %d rows)"
             % (name, sum(only_pipeline.values()), sum(only_reference.values()),
                sum(got.values()), sum(expected.values()))]

    def key(row):
        return tuple((x is not None, x) for x in row)

    for row in sorted(only_pipeline, key=key)[:limit]:
        times = "" if only_pipeline[row] == 1 else " x%d" % only_pipeline[row]
        lines.append("    pipeline : %s%s" % (fmt_row(row, cols), times))
    for row in sorted(only_reference, key=key)[:limit]:
        lines.append("    reference: %s" % fmt_row(row, cols))
    return lines


def run_pipeline(pipeline, data, out):
    """Run the binary; retry once when it cannot be executed or exits non-zero (it may be mid-rebuild)."""
    last = None
    for attempt in range(2):
        if attempt:
            time.sleep(1.5)
            shutil.rmtree(out, ignore_errors=True)
        try:
            done = subprocess.run([pipeline, data, out], stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                  timeout=TIMEOUT)
        except subprocess.TimeoutExpired:
            raise Failure("TIMEOUT", "no result after %d s" % TIMEOUT)
        except OSError as error:
            last = Failure("EXEC", "cannot execute %s: %s" % (pipeline, error))
            continue
        if done.returncode == 0:
            return
        tail = (done.stderr or done.stdout or b"").decode("utf-8", "replace").strip().splitlines()[-3:]
        last = Failure("EXIT", "exit code %d: %s" % (done.returncode, " | ".join(tail)[:600]))
    raise last


def run_seed(seed, scale, pipeline, work_root, verify):
    """None when the seed passes, else (kind, lines)."""
    tag = "seed-%d" % seed if scale == "small" else "seed-%d-%s" % (seed, scale)
    work = os.path.join(work_root, "work", tag)
    shutil.rmtree(work, ignore_errors=True)
    data = os.path.join(work, "data")
    out = os.path.join(work, "out")
    os.makedirs(work)
    meta = gen.generate(seed, data, scale)
    kind, lines = None, []
    urows = crows = None
    try:
        if verify:
            problems = gen.check_guarantees(data)
            if problems:
                raise Failure("GENERATOR", "input guarantee broken: " + "; ".join(problems[:3]))
        try:
            urows, crows = ref.pipeline(ref.load(data))
        except Exception:
            raise Failure("REFERENCE", "ref.py raised: " + traceback.format_exc().strip().splitlines()[-1])
        run_pipeline(pipeline, data, out)
        got_u = read_table(os.path.join(out, "user_daily"), USER_COLS)
        got_c = read_table(os.path.join(out, "campaign_daily"), CAMP_COLS)
        lines = (diff_tables("user_daily", USER_COLS, Counter(urows), got_u)
                 + diff_tables("campaign_daily", CAMP_COLS, Counter(crows), got_c))
        if lines:
            kind = "DIFF"
    except Failure as failure:
        kind, lines = failure.kind, ["  " + failure.detail]
    if kind is None:
        shutil.rmtree(work, ignore_errors=True)
        return None
    if kind != "GENERATOR":
        problems = gen.check_guarantees(data)
        if problems:
            lines.append("  NOTE generator broke an input guarantee: " + "; ".join(problems[:3]))
    fail = os.path.join(work_root, "fail", tag)
    shutil.rmtree(fail, ignore_errors=True)
    os.makedirs(os.path.dirname(fail), exist_ok=True)
    shutil.move(work, fail)
    if urows is not None:
        ref.write_csv(urows, crows, os.path.join(fail, "ref"))
    with open(os.path.join(fail, "meta.json"), "w") as handle:
        json.dump(meta, handle)
    with open(os.path.join(fail, "diff.txt"), "w") as handle:
        handle.write("\n".join(lines) + "\n")
    lines.append("  kept: %s (data/, out/, ref/, meta.json)  flags=%s" % (fail, ",".join(meta["flags"]) or "-"))
    return kind, lines


def main(argv):
    args = argv[1:]
    options = {"--scale": "small", "--pipeline": DEFAULT_PIPELINE, "--work": DEFAULT_WORK}
    for name in list(options):
        if name in args:
            at = args.index(name)
            options[name] = args[at + 1]
            del args[at:at + 2]
    keep_going = "--keep-going" in args
    verify = "--verify" in args
    args = [a for a in args if not a.startswith("--")]
    if len(args) != 2 or options["--scale"] not in ("small", "medium"):
        print(__doc__)
        return 2
    first, last = int(args[0]), int(args[1])
    scale = options["--scale"]
    seeds = failures = 0
    started = time.time()
    for seed in range(first, last + 1):
        seeds += 1
        result = run_seed(seed, scale, options["--pipeline"], options["--work"], verify)
        if result is not None:
            failures += 1
            kind, lines = result
            print("FAIL seed=%d scale=%s kind=%s" % (seed, scale, kind))
            for line in lines:
                print(line)
            sys.stdout.flush()
            if not keep_going:
                break
    elapsed = time.time() - started
    print("SUMMARY seeds=%d failures=%d" % (seeds, failures))
    print("(%.1f s, %.1f seeds/s, scale=%s)" % (elapsed, seeds / elapsed if elapsed else 0.0, scale), file=sys.stderr)
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
