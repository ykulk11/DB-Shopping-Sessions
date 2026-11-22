#!/usr/bin/env python3
"""Differential fuzz: refimpl (Rust) against tools/pyref/ref.py on tiny adversarial datasets.

usage: fuzz_vs_pyref.py WORK_DIR [CASES] [SEED]
Every case is a few hundred rows drawn from small pools of IDs and of nasty field values, so
duplicates, ties, window edges, bad forms and odd links are all frequent. The two CSV outputs
are compared byte for byte. The data keeps the promises the rules make: ingest_ts is valid F1,
copies of an event with equal ingest_ts are identical, copies of a click are identical.
A failing case is left in WORK_DIR/fail-N.
"""
import os
import random
import shutil
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.dont_write_bytecode = True  # leave no __pycache__ in tools/pyref
sys.path.insert(0, os.path.join(HERE, "..", "pyref"))
import ref  # noqa: E402

BIN = os.path.join(HERE, "target", "release", "refimpl")
SECOND, MINUTE, HOUR, DAY = 1000, 60_000, 3_600_000, 86_400_000

USERS = [b"u1", b"u2", b"u3", b"u4", b"u5", b"u10", b"U1", b" u1", b"u1 ", b"A\tB", b"u1\r", b"\xc3\xa9", b"user_id_a",
         b"user_id_b", b"z"]
PRODUCTS = [b"p1", b"p2", b"p3", b"p4", b"p9", b"P1", b" p1", b"", b"p1\r"]
TYPES = [b"view"] * 6 + [b"cart"] * 2 + [b"purchase"] * 6 + [b"Purchase", b"cart ", b"", b"views", b" view", b"VIEW"]
PRICES = [b"100", b"250", b"0250", b"1", b"10000000", b"10000001", b"0", b"00", b"", b"-5", b"+500", b"19.99", b"1e3",
          b"99999999999999999999999", b"000000000000000000000000000500", "２５０".encode(), "٢٥٠".encode(), b" 100",
          b"100 ", b"1000\r", b"500", b"500", b"700"]
CAMPAIGNS = [b"c_1"] * 4 + [b"c_2"] * 4 + [b"c_3"] * 3 + [b"c_999", b"C_1", b"organic", b" c_1", b"", b"c_1 "]
OFFSETS = [-600, -300, -1, 0, 1, 90, 330, 720, 23 * 60 + 59, -(23 * 60 + 59)]


def civil(ms):
    days, rest = divmod(ms // 1000, 86400)
    y, m, d = ref.civil_from_days(days)
    return y, m, d, rest // 3600, rest // 60 % 60, rest % 60


def f1(ms):
    return b"%04d-%02d-%02dT%02d:%02d:%02dZ" % civil(ms)


def f2(ms, offset):
    sign = b"-" if offset < 0 else b"+"
    return b"%04d-%02d-%02dT%02d:%02d:%02d" % civil(ms + offset * MINUTE) + sign + b"%02d:%02d" % divmod(abs(offset), 60)


def good_ts(rng, ms):
    draw = rng.random()
    if ms % SECOND or draw < 0.3:
        text = b"%d" % ms
        return b"0" * rng.choice([0, 0, 0, 1, 5]) + text
    if draw < 0.5:
        return f2(ms, rng.choice(OFFSETS))
    return f1(ms)


def bad_ts(rng, ms):
    whole = ms - ms % SECOND
    y, m, d, hh, mm, ss = civil(whole)
    base = f1(whole)
    return rng.choice([
        b"", base[:-1] + b"z", base.replace(b"T", b"t"), base.replace(b"T", b" "), base[:-1], base[:-1] + b".000Z",
        b"%d.0" % ms, b"1.79e12", b"-5", b"+5", b" %d" % ms, b"%d " % ms, base + b"\r",
        b"%04d-13-%02dT%02d:%02d:%02dZ" % (y, d, hh, mm, ss), b"%04d-02-30T%02d:%02d:%02dZ" % (y, hh, mm, ss),
        b"%04d-02-29T%02d:%02d:%02dZ" % (y if y % 4 else y - 1, hh, mm, ss),
        b"%04d-%02d-%02dT24:%02d:%02dZ" % (y, m, d, mm, ss), b"%04d-%02d-%02dT%02d:%02d:60Z" % (y, m, d, hh, mm),
        b"%04d-%02d-00T%02d:%02d:%02dZ" % (y, m, hh, mm, ss), b"%04d-00-%02dT%02d:%02d:%02dZ" % (y, d, hh, mm, ss),
        b"%04d-%02d-32T%02d:%02d:%02dZ" % (y, m, hh, mm, ss), b"%04d-04-31T%02d:%02d:%02dZ" % (y, hh, mm, ss),
        base[:-1] + b"+24:00", base[:-1] + b"+05:60", base[:-1] + b"+0200", base[:-1] + b"+02", base[:-1] + b"+2:00",
        base[:-1] + b" 02:00", base[:-1] + b"+02:00Z", base[:-1] + b"Z ", base[2:], b"0" + base,
        base.replace(b"-", b"/"), base.replace(b":", b"."), base[:4] + b"-" + base[4:],
        "２".encode() + base[1:], "٢٠٢٦".encode(), b"99999999999999999999999999", b"86400000", b"0", b"1e12",
        b"0x1A", b"%d" % ms + "٠".encode(), b"T", b"Z", b"2026-10-03T10:00:0Z", b"2026-10-3T10:00:00Z",
        b"-026-10-03T10:00:00Z", b"2026-10-03T10:00:00-0:30", b"2026-10-03T1 :00:00Z", b"2026-1 -03T10:00:00Z",
    ])


def quote(rng, value, rate):
    return b'"' + value + b'"' if rng.random() < rate else value


def emit(rng, fields, quote_rate):
    """One line, sometimes malformed the ways the rules list (and a few more)."""
    line = b",".join(quote(rng, f, quote_rate) for f in fields)
    draw = rng.random()
    if draw < 0.02:
        return line[:line.rfind(b",")] if b"," in line else b""
    if draw < 0.04:
        return line + b",x"
    if draw < 0.05:
        return b'"' + line
    if draw < 0.06:
        return line + b'"'
    if draw < 0.07:
        return line[line.find(b","):] if b"," in line else line
    if draw < 0.08:
        return b""
    if draw < 0.09:
        at = rng.randrange(len(line) + 1)
        return line[:at] + b'"' + line[at:]
    if draw < 0.10:
        return line + b"\r"
    if draw < 0.105:
        return line.replace(b",", b'","', 1)
    if draw < 0.11:
        return b'""' + line
    if draw < 0.115:
        return line + b","
    return line


def write_parts(rng, folder, header, lines):
    os.makedirs(folder)
    parts = rng.choice([1, 1, 2, 3])
    rng.shuffle(lines)
    for part in range(parts):
        mine = lines[part::parts]
        body = b"\n".join([header] + mine)
        if rng.random() < 0.8:
            body += b"\n"
        with open(os.path.join(folder, "part-%05d.csv" % part), "wb") as handle:
            handle.write(body)
    if rng.random() < 0.3:
        with open(os.path.join(folder, "part-%05d.csv" % parts), "wb") as handle:
            handle.write(rng.choice([header + b"\n", header, b"", b"\n", b"not the header\n"]))


def make_case(rng, folder):
    shutil.rmtree(folder, ignore_errors=True)
    os.makedirs(folder)
    leap = rng.random() < 0.3
    ws = ref.parse_ts(b"2028-02-20T00:00:00Z" if leap else b"2026-10-01T00:00:00Z")
    span = rng.choice([1, 2, 14]) * DAY
    we = ws + span
    with open(os.path.join(folder, "manifest.json"), "w") as handle:
        handle.write('{\n  "window_start": "%s",\n  "window_end": "%s"\n}\n' % (f1(ws).decode(), f1(we).decode()))
    with open(os.path.join(folder, "products.csv"), "wb") as handle:
        handle.write(b"product_id,category\np1,shoes\np2,books\np3,shoes\np4,toys\n")
    with open(os.path.join(folder, "campaigns.csv"), "wb") as handle:
        handle.write(b"campaign_id,channel\nc_1,search\nc_2,social\nc_3,search\n")
    quote_rate = rng.choice([0.0, 0.2, 0.5])
    users = rng.sample(USERS, rng.randint(1, len(USERS)))
    # A few anchors; times cluster around them at the gaps that matter.
    anchors = [ws + rng.randrange(span) for _ in range(rng.randint(1, 6))] + [ws, we - 1]
    steps = [0, 0, 1, 999, SECOND, MINUTE, 29 * MINUTE, 30 * MINUTE - 1, 30 * MINUTE, 30 * MINUTE + 1, 31 * MINUTE, HOUR,
             DAY - 1, DAY, 7 * DAY - 1, 7 * DAY, 7 * DAY + 1]

    def moment(extra=()):
        t = rng.choice(anchors)
        for _ in range(rng.choice([0, 1, 1, 2, 3])):
            t += rng.choice([-1, 1]) * rng.choice(steps)
        if extra and rng.random() < 0.2:
            t = rng.choice(extra)
        if rng.random() < 0.5:
            t -= t % SECOND
        return t

    seen = {}  # event_id -> {ingest: fields}
    lines = []
    event_ids = [b"e%d" % i for i in range(rng.randint(5, 150))] + [b"", b"E1", b"e1 ", b"e3\r", b"event_id"]
    for _ in range(rng.randint(0, 250)):
        eid = rng.choice(event_ids)
        t = moment((ws - 1, ws, we - 1, we, ws - 7 * DAY))
        ts = bad_ts(rng, t) if rng.random() < 0.06 else good_ts(rng, t)
        ingest = t - t % SECOND + rng.choice([0, 0, SECOND, MINUTE, -5 * SECOND, DAY - SECOND, DAY, DAY + SECOND,
                                             DAY + 2 * SECOND, 3 * DAY, 10 * MINUTE]) + rng.choice([0, 0, SECOND])
        user = b"" if rng.random() < 0.03 else rng.choice(users)
        fields = [eid, user, ts, f1(ingest), rng.choice(TYPES), rng.choice(PRODUCTS), rng.choice(PRICES)]
        copies = seen.setdefault(eid, {})
        if copies and rng.random() < 0.5:  # a copy of an event already written: later, earlier or the same instant
            ingest = rng.choice(list(copies)) + rng.choice([0, 0, SECOND, -SECOND, 10 * MINUTE])
            fields[3] = f1(ingest)
        fields = copies.setdefault(ingest, fields)  # equal ingest_ts: identical values
        lines.append(emit(rng, fields, quote_rate))
    write_parts(rng, os.path.join(folder, "events"), ref.EVENT_HEADER, lines)

    clicks = {}
    lines = []
    click_ids = [b"k%d" % i for i in range(rng.randint(3, 80))] + [b"", b"K1", b"k1 ", b"click_id"]
    for _ in range(rng.randint(0, 120)):
        cid = rng.choice(click_ids)
        t = moment((ws - 7 * DAY - 1, ws - 7 * DAY, we - 1, we))
        ts = bad_ts(rng, t) if rng.random() < 0.06 else good_ts(rng, t)
        user = b"" if rng.random() < 0.03 else rng.choice(users)
        fields = clicks.setdefault(cid, [cid, user, rng.choice(CAMPAIGNS), ts])  # copies are identical
        lines.append(emit(rng, fields, quote_rate))
    write_parts(rng, os.path.join(folder, "clicks"), b"click_id,user_id,campaign_id,ts", lines)

    lines = []
    pool = USERS + [b"", b"u0", b"U9", b"!", b"~"]
    for _ in range(rng.choice([0, 0, 1, 3, 8, 20])):
        lines.append(emit(rng, [rng.choice(pool), rng.choice(pool)], quote_rate))
    write_parts(rng, os.path.join(folder, "links"), b"user_id_a,user_id_b", lines)


def main():
    work = sys.argv[1]
    cases = int(sys.argv[2]) if len(sys.argv) > 2 else 300
    seed = int(sys.argv[3]) if len(sys.argv) > 3 else 1
    os.makedirs(work, exist_ok=True)
    failures = 0
    rows = 0
    for case in range(cases):
        rng = random.Random(seed * 1_000_003 + case)
        data = os.path.join(work, "case")
        make_case(rng, data)
        out_py, out_rs = os.path.join(work, "py"), os.path.join(work, "rs")
        shutil.rmtree(out_py, ignore_errors=True)
        shutil.rmtree(out_rs, ignore_errors=True)
        urows, crows = ref.pipeline(ref.load(data))
        ref.write_csv(urows, crows, out_py)
        rows += len(urows) + len(crows)
        run = subprocess.run([BIN, data, out_rs], capture_output=True)
        same = run.returncode == 0
        for table in ("user_daily.csv", "campaign_daily.csv"):
            if same:
                with open(os.path.join(out_py, table), "rb") as a, open(os.path.join(out_rs, table), "rb") as b:
                    same = a.read() == b.read()
        if not same:
            failures += 1
            keep = os.path.join(work, "fail-%d" % case)
            shutil.rmtree(keep, ignore_errors=True)
            os.makedirs(keep)
            for name, path in (("data", data), ("py", out_py), ("rs", out_rs)):
                if os.path.isdir(path):
                    shutil.copytree(path, os.path.join(keep, name))
            print("DIFF case %d (seed %d): kept in %s; refimpl exit %d %s" % (
                case, seed, keep, run.returncode, run.stderr[-300:].decode(errors="replace")))
    print("%d cases, %d output rows in all, %d differ" % (cases, rows, failures))
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
