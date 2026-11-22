#!/usr/bin/env python3
"""cmp.py PIPE_OUT REF_OUT: byte-exact multiset compare (ref CSV raw, pipeline parquet)."""
import sys, glob, os
from collections import Counter
import pyarrow.parquet as pq
pipe, ref = sys.argv[1], sys.argv[2]
COLS = {'user_daily': ['user_id', 'day', 'sessions', 'events', 'revenue_cents', 'top_category'],
        'campaign_daily': ['campaign_id', 'channel', 'day', 'sessions', 'purchasing_sessions', 'revenue_cents', 'assisted_sessions']}
bad = False
for t, cols in COLS.items():
    data = open(f'{ref}/{t}.csv', 'rb').read().split(b'\n')
    hdr = data[0].split(b','); idx = [hdr.index(c.encode()) for c in cols]
    R = Counter()
    for l in data[1:]:
        if not l: continue
        f = l.split(b',')
        R[tuple(f[i] for i in idx)] += 1
    P = Counter()
    for p in sorted(glob.glob(f'{pipe}/{t}/*.parquet')):
        for r in pq.read_table(p).to_pylist():
            P[tuple(b'' if r[c] is None else (r[c].encode('utf-8', 'surrogateescape') if isinstance(r[c], str) else str(r[c]).encode()) for c in cols)] += 1
    a, b = P - R, R - P
    if a or b:
        bad = True
        print(t, 'pipe-only', len(a), 'ref-only', len(b))
        for x in sorted(a)[:8]: print('  PIPE', x)
        for x in sorted(b)[:8]: print('  REF ', x)
print('DIFFER' if bad else 'MATCH')
