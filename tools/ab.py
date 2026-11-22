#!/usr/bin/env python3
"""Alternating A/B timing of pipeline variants.
usage: ab.py DATA_DIR ROUNDS "LABEL=BINARY [ENV=VAL ...]" "LABEL=BINARY [ENV=VAL ...]" ...
Each round runs every variant once, in order; prints min and median of total, phase times, user and sys."""
import os, re, resource, statistics, subprocess, sys, time, shutil
data = sys.argv[1]; rounds = int(sys.argv[2])
variants = []
for spec in sys.argv[3:]:
    label, rest = spec.split("=", 1)
    parts = rest.split()
    variants.append((label, parts[0], dict(kv.split("=", 1) for kv in parts[1:])))
out = os.environ.get("XOUT", "/home/user/out") + "/ab"
res = {v[0]: [] for v in variants}
for _ in range(rounds):
    for label, binary, env in variants:
        shutil.rmtree(out, ignore_errors=True)
        e = dict(os.environ, PIPE_TIMING="1", **env)
        b = resource.getrusage(resource.RUSAGE_CHILDREN); t = time.perf_counter()
        p = subprocess.run([binary, data, out], env=e, capture_output=True, text=True)
        wall = (time.perf_counter() - t) * 1e3; a = resource.getrusage(resource.RUSAGE_CHILDREN)
        m = re.search(r"phase1 ([\d.]+) \| phase2 ([\d.]+) \| phase3 ([\d.]+) \| finish ([\d.]+) \| total ([\d.]+)", p.stderr)
        if p.returncode != 0 or not m:
            print("run failed", label, p.returncode, p.stderr[-200:]); continue
        v = [float(x) for x in m.groups()]
        res[label].append([wall, v[0], v[1], v[2], (a.ru_utime - b.ru_utime) * 1e3, (a.ru_stime - b.ru_stime) * 1e3])
names = ["wall", "phase1", "phase2", "phase3", "user", "sys"]
print("%-22s %s" % ("variant", " ".join("%13s" % (n + " min/med") for n in names)))
for label, _, _ in variants:
    r = res[label]
    if not r: continue
    print("%-22s %s" % (label, " ".join("%6.0f/%-6.0f" % (min(x[i] for x in r), statistics.median(x[i] for x in r)) for i in range(len(names)))))
