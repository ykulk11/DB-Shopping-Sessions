#!/usr/bin/env python3
"""Run the pipeline several times and report the best and median timings.
usage: bench.py DATA_DIR [RUNS] [BINARY] [ENV=VALUE ...]"""
import os, re, resource, subprocess, sys, statistics, time
data = sys.argv[1]
runs = int(sys.argv[2]) if len(sys.argv) > 2 else 5
binary = sys.argv[3] if len(sys.argv) > 3 else os.path.join(os.path.dirname(__file__), "..", "target", "release", "pipeline")
env = dict(os.environ, PIPE_TIMING="1")
for kv in sys.argv[4:]:
    k, v = kv.split("=", 1); env[k] = v
rows = []
for _ in range(runs):
    before = resource.getrusage(resource.RUSAGE_CHILDREN)
    t = time.perf_counter()
    p = subprocess.run([binary, data, "/home/user/out/bench"], env=env, capture_output=True, text=True)
    wall = (time.perf_counter() - t) * 1e3
    after = resource.getrusage(resource.RUSAGE_CHILDREN)
    m = re.search(r"setup ([\d.]+) ms \| phase1 ([\d.]+) \| phase2 ([\d.]+) \| phase3 ([\d.]+) \| finish ([\d.]+) \| total ([\d.]+)", p.stderr)
    if p.returncode != 0 or not m:
        print("run failed", p.returncode, p.stderr[-300:]); continue
    v = [float(x) for x in m.groups()]
    rows.append(v + [wall, (after.ru_utime - before.ru_utime) * 1e3, (after.ru_stime - before.ru_stime) * 1e3])
names = ["setup", "phase1", "phase2", "phase3", "finish", "total", "wall", "user", "sys"]
print("        " + " ".join(f"{n:>8}" for n in names))
print("min     " + " ".join(f"{min(r[i] for r in rows):8.0f}" for i in range(len(names))))
print("median  " + " ".join(f"{statistics.median(r[i] for r in rows):8.0f}" for i in range(len(names))))
