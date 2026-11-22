#!/usr/bin/env python3
"""
Runs an implementation on every fixture and compares both output tables
with the hand-written expected CSVs (exact, order-insensitive).

Usage: python run_fixtures.py FIXTURES_DIR -- CMD...
CMD gets DATA_DIR and OUTPUT_DIR as env vars and as the last two arguments.
Example: python run_fixtures.py ../fixtures -- python reference.py
"""
import os
from pathlib import Path
import subprocess
import sys
import tempfile

import polars as pl

SCHEMAS = {
    "user_daily": {"user_id": pl.String, "day": pl.Date, "sessions": pl.Int128, "events": pl.Int128,
                   "revenue_cents": pl.Int128, "top_category": pl.String},
    "campaign_daily": {"campaign_id": pl.String, "channel": pl.String, "day": pl.Date, "sessions": pl.Int128,
                       "purchasing_sessions": pl.Int128, "revenue_cents": pl.Int128, "assisted_sessions": pl.Int128},
}


def load_expected(path, schema):
    df = pl.read_csv(path, schema_overrides={k: (pl.String if v == pl.Date else v) for k, v in schema.items()})
    if df.height == 0:
        return pl.DataFrame(schema=schema)
    return df.with_columns(pl.col("day").str.to_date())


def load_actual(folder, schema):
    folder = Path(folder)
    if folder.is_symlink() or not folder.is_dir():
        raise ValueError(f"expected an output directory: {folder}")
    frames = []
    for path in sorted(folder.glob("*.parquet")):
        if path.name.startswith((".", "_")) or path.is_symlink() or not path.is_file():
            continue
        frame = pl.read_parquet(path)
        if set(frame.columns) != set(schema):
            raise ValueError(f"{path}: expected {list(schema)}, got {frame.columns}")
        for column, expected in schema.items():
            actual = frame.schema[column]
            if expected == pl.Int128 and actual.is_integer():
                continue
            if expected == pl.String and actual.base_type() in (pl.String, pl.Categorical, pl.Enum):
                continue
            if actual != expected:
                raise ValueError(f"{path}: {column} has type {actual}, expected {expected}")
        frames.append(frame.select(schema.keys()).cast(schema))
    if not frames:
        raise ValueError(f"no parquet in {folder}")
    return pl.concat(frames)


def rows(df):
    return sorted(df.rows(), key=lambda r: tuple("" if v is None else str(v) for v in r))


def main():
    sep = sys.argv.index("--")
    fx_dir, cmd = sys.argv[1], sys.argv[sep + 1:]
    failed = []
    cases = sorted(d for d in os.listdir(fx_dir) if os.path.isdir(os.path.join(fx_dir, d)))
    if not cases:
        sys.exit(f"no fixtures in {fx_dir}")
    for case in cases:
        data = os.path.join(fx_dir, case, "data")
        if not os.path.isdir(data):
            data = os.path.join(fx_dir, case, "input")
        with tempfile.TemporaryDirectory(ignore_cleanup_errors=True) as out:
            env = dict(os.environ, DATA_DIR=data, OUTPUT_DIR=out)
            p = subprocess.run(cmd + [data, out], env=env, capture_output=True, text=True)
            if p.returncode != 0:
                failed.append(case)
                print(f"FAIL {case}: exit {p.returncode}\n{p.stderr[-2000:]}")
                continue
            ok = True
            for table, schema in SCHEMAS.items():
                exp = rows(load_expected(os.path.join(fx_dir, case, "expected", f"{table}.csv"), schema))
                try:
                    act = rows(load_actual(os.path.join(out, table), schema))
                except (ValueError, OSError, pl.exceptions.PolarsError) as error:
                    ok = False
                    print(f"FAIL {case} {table}: {error}")
                    continue
                if exp != act:
                    ok = False
                    print(f"FAIL {case} {table}\n  expected {exp}\n  actual   {act}")
            if ok:
                print(f"ok   {case}")
            else:
                failed.append(case)
    print(f"\n{len(cases) - len(failed)}/{len(cases)} fixtures passed")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
