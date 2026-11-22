#!/usr/bin/env python3
"""Adversarial dataset generator for the Shopping Sessions differential fuzzer.

  python3 gen.py SEED OUTDIR [--scale small|medium] [--check]

Writes a dataset in the /data layout (events/*.csv, clicks/*.csv, links/*.csv, products.csv,
campaigns.csv, manifest.json). Deterministic per (seed, scale). Importable: generate(seed,
outdir, scale) returns a small meta dict; check_guarantees(outdir) re-reads a dataset and lists
every violation of the input guarantees of rules.md (it must return []).

The generator stays inside what rules.md guarantees about inputs, so every dataset has exactly
one right answer:
  * every CSV file starts with the exact header line of its type;
  * products.csv / campaigns.csv are clean, unquoted, with unique IDs (campaigns c_<n>);
  * ingest_ts of every well-formed event line is a valid F1 timestamp;
  * well-formed event lines with the same event_id and the same ingest_ts are identical in all
    values (quoting may differ); well-formed click lines with the same click_id are identical;
  * no value holds a comma, a double quote or a newline;
  * the window starts and ends at midnight UTC.
Inside those guarantees it is as dirty as it can be.
"""
import json
import os
import random
import re
import sys

DAY = 86_400_000
HOUR = 3_600_000
MINUTE = 60_000
GAP = 1_800_000
REACH = 7 * DAY
EVENT_HEADER = b"event_id,user_id,ts,ingest_ts,event_type,product_id,price_cents"
CLICK_HEADER = b"click_id,user_id,campaign_id,ts"
LINK_HEADER = b"user_id_a,user_id_b"
HEADERS = {"events": EVENT_HEADER, "clicks": CLICK_HEADER, "links": LINK_HEADER}
NFIELDS = {"events": 7, "clicks": 4, "links": 2}

E_ACUTE = "é".encode()
U_UML = "ü".encode()
FULLWIDTH = [chr(0xFF10 + i).encode() for i in range(10)]
ARABIC = [chr(0x0660 + i).encode() for i in range(10)]

CATEGORY_POOL = [b"apparel", b"automotive", b"baby", b"beauty", b"books", b"electronics", b"garden",
                 b"grocery", b"toys", b"video_games", b"zoo", b"unknow", b"unknown2", b"unknown_",
                 b"un", b"v", b"tools"]
CHANNELS = [b"affiliate", b"display", b"email", b"search", b"social"]
GOOD_TYPES = [b"view", b"cart", b"purchase"]
BAD_TYPES = [b"Purchase", b"cart ", b"", b"views", b" view", b"VIEW", b"purchase\r", b"purchases",
             b"View", b"view\t", b"pur chase", b"buy", b"click", b"purchas" + E_ACUTE, b"\tcart",
             b"PURCHASE", b"cart\r", b"purchase ", b"null"]
BAD_PRICES = [b"0", b"00", b"10000001", b"19.99", b"-5", b"+500", b"1e3", b"",
              b"".join(ARABIC[int(c)] for c in "250"), b"".join(FULLWIDTH[int(c)] for c in "250"),
              b"250\r", b"99999999999999999999", str(2 ** 64 + 250).encode(), str(2 ** 32).encode(),
              str(2 ** 64 + 1).encode(), str(2 ** 63).encode(),
              b" 250", b"250 ", b"0x10", b"1_000", b"100.0", b".5", b"-0", b"+0", b"NaN", b"null",
              b"010000001", b"0000000000000000000000000000000000000000", b"10000000.0", b"1 000",
              b"$5", b"5c", b"\t250", b"2 50", b"+1", b"-1", b"1e0", b"0.0", b"00000000000000000000010000001",
              b"25" + FULLWIDTH[0], b"\r250", b"1.", b"1e-1", b"0b1", b"one"]
HUGE_NUMBERS = [b"9223372036854775807", b"18446744073709551615", b"99999999999999999999999999",
                b"9223372036854775808", b"10000000", b"1", b"4294967296"]
SPECIAL_IDS = [b"user_id_a", b"user_id_b", b"user_id", b"organic", b"unknown", b"null", b"NULL",
               b"0", b" ", b"\t", b"-", b"\\N", b"nan", E_ACUTE, "～".encode(),
               "\U0001F600".encode(), b"''", b"u1;u2", b"u1|u2", b"a b", b"#", b"=1+1", b"\x7f",
               b"\r", b"  ", b"u\r1", b"kiosk", b"event_id", b"click_id", b"None", b"0x1f", b"1e5",
               b"\xef\xbb\xbfu1", b"!", b"u1\tu2"]
BAD_CAMPAIGNS = [b"c_9999", b"C_1", b" c_1", b"", b"c_01", b"c_1 ", b"organic", b"c_", b"c_+1",
                 b"c_1.0", b"c1", b"1", b"c_" + FULLWIDTH[1], b"c_1\r", b"c_-1", b"c_0", b"c_1e0",
                 b"Organic", b"null", b"c_18446744073709551617", b"c_4294967297", b"\tc_1",
                 b"campaign_id", b"c_1x"]
OFFSETS = [0, 0, 0, 60, -60, 120, -240, -300, 330, 345, 525, 570, -570, -600, 720, 840, 1439,
           -1439, 825, -720, 1380, -1380, 1, -1, 59, -59, 765, 600, -210]


# ----------------------------------------------------------------------------- calendar

def days_from_civil(y, m, d):
    y -= m <= 2
    era = y // 400
    yoe = y - era * 400
    doy = (153 * ((m + 9) % 12) + 2) // 5 + d - 1
    return era * 146097 + yoe * 365 + yoe // 4 - yoe // 100 + doy - 719468


def civil_from_days(z):
    z += 719468
    era = z // 146097
    doe = z - era * 146097
    yoe = (doe - doe // 1460 + doe // 36524 - doe // 146096) // 365
    doy = doe - (365 * yoe + yoe // 4 - yoe // 100)
    mp = (5 * doy + 2) // 153
    d = doy - (153 * mp + 2) // 5 + 1
    m = mp + 3 if mp < 10 else mp - 9
    return yoe + era * 400 + (m <= 2), m, d


def civil(ms):
    """[year, month, day, hour, minute, second] of an instant; milliseconds are dropped."""
    days, rem = divmod(ms, DAY)
    y, m, d = civil_from_days(days)
    s = rem // 1000
    return [y, m, d, s // 3600, s // 60 % 60, s % 60]


def fmt_civil(c, sep=b"T"):
    return b"%04d-%02d-%02d%s%02d:%02d:%02d" % (c[0], c[1], c[2], sep, c[3], c[4], c[5])


def f1(ms):
    return fmt_civil(civil(ms)) + b"Z"


def f2(ms, off_min, minus_zero=False):
    body = fmt_civil(civil(ms + off_min * MINUTE))
    sign = b"-" if off_min < 0 or (off_min == 0 and minus_zero) else b"+"
    a = abs(off_min)
    return body + b"%s%02d:%02d" % (sign, a // 60, a % 60)


_F1 = re.compile(rb"([0-9]{4})-([0-9]{2})-([0-9]{2})T([0-9]{2}):([0-9]{2}):([0-9]{2})Z")


def is_valid_f1(b):
    m = _F1.fullmatch(b)
    if not m:
        return False
    y, mo, d, h, mi, s = (int(x) for x in m.groups())
    if not (1 <= mo <= 12 and h <= 23 and mi <= 59 and s <= 59):
        return False
    leap = y % 4 == 0 and (y % 100 != 0 or y % 400 == 0)
    dim = [31, 29 if leap else 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31][mo - 1]
    return 1 <= d <= dim


def split_fields(line, n):
    """Values of a well-formed line of n fields, else None (rules.md, "CSV dialect")."""
    if line == b"":
        return None
    parts = line.split(b",")
    if len(parts) != n:
        return None
    out = []
    for p in parts:
        if b'"' in p:
            if len(p) < 2 or p[:1] != b'"' or p[-1:] != b'"' or b'"' in p[1:-1]:
                return None
            p = p[1:-1]
        out.append(p)
    return out


class Person:
    __slots__ = ("ids", "ghosts")

    def __init__(self, ids, ghosts):
        self.ids = ids
        self.ghosts = ghosts


# ----------------------------------------------------------------------------- generator

class Gen:
    def __init__(self, seed, scale):
        self.seed = seed
        self.scale = scale
        self.rng = random.Random("shopping-sessions-fuzz:%d:%s" % (seed, scale))
        self.flags = []
        self.events = []      # well-formed event records (7 values), before file assignment
        self.clicks = []      # well-formed click records (4 values)
        self.links = []       # well-formed link records (2 values)
        self.raw = {"events": [], "clicks": [], "links": []}   # lines that must be malformed
        self.used_ids = set()
        self.used_eids = set()
        self.used_cids = set()
        self.n_eid = 0
        self.n_cid = 0
        self.n_stem = 0

    # ------------------------------------------------------------------ setup
    def setup(self):
        rng = self.rng
        ndays = 14 if rng.random() < 0.7 else rng.randint(1, 5)
        if rng.random() < 0.15:
            start = days_from_civil(2028, 2, 29) - rng.randint(0, ndays - 1)
            self.flags.append("feb29")
        else:
            start = rng.randint(days_from_civil(2025, 1, 1), days_from_civil(2028, 12, 31))
        self.ws = start * DAY
        self.we = (start + ndays) * DAY
        self.ndays = ndays
        self.extreme = None
        if self.seed % 20 == 0:
            # 5% of the seeds, cycling through the kinds so that any 120 consecutive seeds hold all
            self.extreme = ["no_events", "no_clicks", "no_links", "all_events_invalid",
                            "single_huge_person", "one_user_thousands"][self.seed // 20 % 6]
            self.flags.append("extreme:" + self.extreme)
        self.q_quote = rng.choice([0.0, 0.2, 0.2, 0.5, 0.9])
        self.p_malformed = rng.choice([0.02, 0.06, 0.15])
        self.p_dup = rng.choice([0.1, 0.25, 0.4])
        self.p_bad_ts = rng.choice([0.04, 0.1, 0.2])
        self.p_junk = rng.choice([0.0, 0.03, 0.1])
        # catalogs
        ncat = rng.randint(3, 4)
        cats = rng.sample(CATEGORY_POOL, ncat)
        nprod = rng.randint(5, 10)
        style = rng.choice(["p%d", "p_%d", "sku%d", "%d"])
        numbers = rng.sample(range(1, 40), nprod)
        self.products = [((style % n).encode(), rng.choice(cats)) for n in numbers]
        self.product_ids = [p for p, _ in self.products]
        ncamp = rng.randint(3, 8)
        numbers = rng.sample([1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 19, 20, 21, 99, 100, 101, 123,
                              1000, 1999, 2000], ncamp)
        if 1 not in numbers and rng.random() < 0.7:
            numbers[0] = 1
        self.campaigns = [(b"c_%d" % n, rng.choice(CHANNELS)) for n in numbers]
        self.campaign_ids = [c for c, _ in self.campaigns]
        self.amounts = rng.sample([1, 100, 100, 200, 250, 300, 500, 1000, 9999, 10000000, 5000000],
                                  rng.randint(2, 5))

    # ------------------------------------------------------------------ timestamps
    def valid_ts(self, t):
        """A valid rendering of the instant t (which may still be outside the window)."""
        rng = self.rng
        r = rng.random()
        if t % 1000 or r < 0.34 or t < 0:
            s = b"%d" % t
            if rng.random() < 0.25:
                s = b"0" * rng.choice([1, 2, 3, 7, 19, 40]) + s
            return s
        if r < 0.62:
            return f1(t)
        off = rng.choice(OFFSETS)
        return f2(t, off, rng.random() < 0.5)

    def bad_ts(self, t, lo, hi):
        """A ts that is bad for the window [lo, hi): a bad form, or a good form outside it."""
        rng = self.rng
        c = civil(t)
        tsec = t - t % 1000
        F = fmt_civil

        def with_(i, v):
            x = list(c)
            x[i] = v
            return x

        k = rng.randrange(46)
        if k == 0:
            return b""
        if k == 1:
            return F(c) + rng.choice([b".%03dZ" % (t % 1000), b".0Z", b".000Z", b".5+00:00"])
        if k == 2:
            return F(c) + b"z"
        if k == 3:
            return F(c, b"t") + b"Z"
        if k == 4:
            return F(c, b" ") + b"Z"
        if k == 5:
            return F(c)
        if k == 6:
            return F(c) + rng.choice([b"+0200", b"-0000", b"+02", b"+2:00", b"+02:0", b"+02:00:00",
                                      b"+ 2:00", b"Z+00:00", b"+00:00Z", b"+0000", b"+002:00",
                                      b"+02-00", b"+02.00", b" +02:00"])
        if k == 7:
            return F(c) + rng.choice([b"+24:00", b"+05:60", b"-24:00", b"+99:99", b"-05:60",
                                      b"+23:60", b"+25:00", b"-00:60", b"+24:30"])
        if k == 8:
            return F(with_(1, rng.choice([0, 13]))) + b"Z"
        if k == 9:
            return F(with_(2, rng.choice([0, 32]))) + b"Z"
        if k == 10:
            x = with_(1, 2)
            x[2] = rng.choice([30, 31])
            return F(x) + rng.choice([b"Z", b"+01:00"])
        if k == 11:
            x = with_(1, 2)
            x[2] = 29
            x[0] = c[0] if c[0] % 4 else rng.choice([2025, 2026, 2027, 2029, 2100, 1900])
            return F(x) + b"Z"
        if k == 12:
            x = with_(1, rng.choice([4, 6, 9, 11]))
            x[2] = 31
            return F(x) + b"Z"
        if k == 13:
            return F(with_(3, 24)) + rng.choice([b"Z", b"+00:00"])
        if k == 14:
            return F(with_(4, 60)) + b"Z"
        if k == 15:
            return F(with_(5, 60)) + rng.choice([b"Z", b"-03:00"])
        if k == 16:
            return b"%d.%02de12" % (t // 10 ** 12, t // 10 ** 10 % 100)
        if k == 17:
            return rng.choice([b"-5", b"+5", b"-%d" % t, b"+%d" % t, b"-0", b"+0"])
        if k == 18:
            return b"%d.0" % t
        if k == 19:
            return b"%d" % rng.choice([10 ** 19 + t, 2 ** 64 + t, 2 ** 63 + t, 2 ** 64 * 7 + t])
        if k == 20:
            return b"%d" % rng.choice([10 ** 29 + t, 2 ** 96 + t, 2 ** 128 + t])
        if k == 21:
            s = bytearray(b"%d" % t)
            i = rng.randrange(len(s))
            s[i:i + 1] = rng.choice([b"O", b"l", b"x", b"e", b"_", b" ", b"-"])
            return bytes(s)
        if k == 22:
            table = rng.choice([FULLWIDTH, ARABIC])
            return b"".join(table[int(ch)] for ch in str(t))
        if k == 23:
            s = f1(tsec)
            i = rng.choice([0, 3, 5, 9, 12, 15, 18])
            return s[:i] + rng.choice([FULLWIDTH, ARABIC])[s[i] - 48] + s[i + 1:]
        if k == 24:
            v = self.valid_ts(t)
            return rng.choice([b" " + v, v + b" ", b"\t" + v, v + b"\t"])
        if k == 25:
            return self.valid_ts(t) + b"\r"
        if k == 26:
            return self.valid_ts(lo - 1)
        if k == 27:
            return self.valid_ts(hi)
        if k == 28:
            return self.valid_ts(lo - 1000)
        if k == 29:
            return self.valid_ts(hi + rng.choice([1, 999, 1000, MINUTE, DAY, 3 * DAY, 400 * DAY]))
        if k == 30:
            return self.valid_ts(lo - rng.choice([2, 999, 1001, MINUTE, DAY, 8 * DAY, 400 * DAY]))
        if k == 31:
            x = list(civil(tsec))
            x[0] += rng.choice([-1, 1, -4, 4])
            if x[1] == 2 and x[2] == 29:
                x[2] = 28
            return F(x) + b"Z"
        if k == 32:
            return rng.choice([b"0", b"86400000", b"1", b"%d" % (t // 1000), b"%d" % (t * 1000),
                               b"00000", b"%d" % (t // 10), b"%d0" % t])
        if k == 33:
            s = f1(tsec)
            return rng.choice([s.replace(b"-", b"/"), s.replace(b"T", b""),
                               s.replace(b"-", b"").replace(b":", b""), s.replace(b":", b"."),
                               s.replace(b"T", b"_"), s[:16] + b"Z", s[:10]])
        if k == 34:
            s = f1(tsec)
            return rng.choice([b"0" + s, s[2:], b"+" + s, b"-" + s])
        if k == 35:
            return b"%d-%d-%dT%d:%d:%dZ" % tuple(c)
        if k == 36:
            return F(c) + rng.choice([b"ZZ", b"UTC", b" Z", b"GMT", b"+Z", b"Z ", b"z "])
        if k == 37:
            return rng.choice([b"0x%x" % t, b"%de0" % t, b"1_%d" % t, b"%dL" % t, b"%dms" % t,
                               b"0%do" % t, b"%d." % t, b".%d" % t])
        if k == 38:
            return F(c) + "−05:00".encode()
        if k == 39:
            return rng.choice([b"null", b"NULL", b"NaN", b"None", b"-", b"ts", b"T", b"Z", b"\r"])
        if k == 40:
            return f2(hi, rng.choice([-60, -600, -1439, 60, 840]))
        if k == 41:
            return f2(lo - 1000, rng.choice([60, 600, 1439, 840, -60]))
        if k == 42:
            return b"\xef\xbb\xbf" + self.valid_ts(t)
        if k == 43:
            return b"0" * rng.choice([1, 5, 30]) + b"%d" % (hi + rng.choice([0, 5]))
        if k == 44:
            s = f1(tsec)
            return s[:-1] + rng.choice([b"+00", b"+00:0", b"+0:00", b"+00:000", b"+00:00:00", b"+"])
        return F(c) + rng.choice([b"\rZ", b"Z\t", b"Z\x00"[:1] + b"\x7f"])

    # ------------------------------------------------------------------ ids
    def stem(self):
        rng = self.rng
        self.n_stem += 1
        n = self.n_stem
        r = rng.random()
        if r < 0.70:
            return b"u%d" % n
        if r < 0.80:
            return b"%032x" % rng.getrandbits(128)
        if r < 0.88:
            return b"user%d" % n
        if r < 0.94:
            return U_UML + b"%d" % n
        return b"%d" % n

    def variant(self, s):
        rng = self.rng
        k = rng.randrange(16)
        if k == 0:
            return s.upper()
        if k == 1:
            return b" " + s
        if k == 2:
            return s + b" "
        if k == 3:
            return b"\t" + s
        if k == 4:
            return s + b"\t"
        if k == 5:
            return s + b"\r"
        if k == 6:
            return b"\r" + s
        if k == 7:
            return s[:1] + b" " + s[1:]
        if k == 8:
            return s + b"0"
        if k == 9:
            return s + b"a"
        if k == 10:
            return s[:-1] if len(s) > 1 else s + b"_"
        if k == 11:
            return U_UML + s[1:]
        if k == 12:
            return s + E_ACUTE
        if k == 13:
            return s.swapcase()
        if k == 14:
            return s.capitalize()
        return s + b"\x7f"

    def new_user_id(self, near=None):
        rng = self.rng
        cand = None
        if near is not None and rng.random() < 0.6:
            cand = self.variant(near)
        elif rng.random() < 0.08:
            cand = rng.choice(SPECIAL_IDS)
        while cand is None or cand == b"" or cand in self.used_ids:
            cand = self.stem()
        self.used_ids.add(cand)
        return cand

    def _new_id(self, prefix, counter, used):
        rng = self.rng
        n = counter
        m = rng.randint(1, n) if rng.random() < 0.3 else n   # reuse an old number: case twins
        r = rng.random()
        if r < 0.50:
            cand = b"%s%d" % (prefix, m)
        elif r < 0.58:
            cand = b"%s%d" % (prefix.upper(), m)
        elif r < 0.63:
            cand = b"%s%d " % (prefix, m)
        elif r < 0.68:
            cand = b" %s%d" % (prefix, m)
        elif r < 0.72:
            cand = b"%s%d\r" % (prefix, m)
        elif r < 0.77:
            cand = b"%s%d%s" % (prefix, m, rng.choice([b"a", b"0", b"_", E_ACUTE]))
        elif r < 0.82:
            cand = b"%d" % m
        elif r < 0.86:
            cand = b"00%d" % m
        elif r < 0.93:
            cand = b"%032x" % rng.getrandbits(128)
        elif r < 0.97:
            cand = U_UML + b"%d" % m
        else:
            cand = b"%s%d-%s" % (prefix, m, b"x" * rng.choice([30, 120, 400]))
        if cand in used:
            cand = b"%s-%d-u" % (prefix, n)
        used.add(cand)
        return cand

    def new_event_id(self):
        self.n_eid += 1
        return self._new_id(b"e", self.n_eid, self.used_eids)

    def new_click_id(self):
        self.n_cid += 1
        return self._new_id(b"k", self.n_cid, self.used_cids)

    # ------------------------------------------------------------------ people and links
    def build_people(self):
        rng = self.rng
        n_people = rng.randint(5, 40)
        self.people = []
        for _ in range(n_people):
            k = rng.choice([1, 1, 1, 1, 2, 2, 2, 3, 3, 4, 5, 8])
            ids = [self.new_user_id()]
            while len(ids) < k:
                ids.append(self.new_user_id(rng.choice(ids)))
            if rng.random() < 0.03:
                n = rng.choice([100, 300, 1000, 5000, 70000])
                body = bytes([rng.choice(b"Lxyz09")]) * n
                for tail in rng.sample([b"a", b"b", b"", b"B"], rng.randint(1, 2)):
                    if body + tail not in self.used_ids:
                        self.used_ids.add(body + tail)
                        ids.append(body + tail)
                self.flags.append("long_id:%d" % n)
            ghosts = []
            if rng.random() < 0.3:
                g = rng.choice([b"\t", b" ", b"!", b"#", b"0", b"A", b"-", b"\r"]) + rng.choice(ids)
                if g not in self.used_ids:
                    self.used_ids.add(g)
                    ghosts.append(g)
            self.people.append(Person(ids, ghosts))
        for p in self.people:
            self.link_person(p)
        # links between people, malformed: they must NOT merge anybody
        for _ in range(rng.randint(0, 6)):
            a = rng.choice(rng.choice(self.people).ids)
            b = rng.choice(rng.choice(self.people).ids)
            self.raw["links"].append(rng.choice([
                a + b"," + b + b",", a, b'"' + a + b"," + b + b'"', a + b',"' + b, b'"' + a + b'",' + b + b'"',
                a + b",," + b, a + b";" + b, a + b', "' + b + b'"', b'"' + a + b'" ,' + b, a + b"," + b + b",x",
                b'"' + a + b"," + b, a + b"," + b + b'"', b'""' + a + b'",' + b, a + b',"' + b + b'"\r',
                b"," + a + b"," + b, a + b'""' + b"," + b, b'"' + a + b'""' + b + b'",' + b, a + b"\t" + b,
            ]))
        if rng.random() < 0.12 or self.extreme == "single_huge_person":
            # a kiosk ID that links many people into one
            kiosk = self.new_user_id() if rng.random() < 0.5 else b"kiosk"
            self.used_ids.add(kiosk)
            share = 1.0 if self.extreme == "single_huge_person" else rng.choice([0.2, 0.5, 1.0])
            for p in self.people:
                if rng.random() < share:
                    self.add_link(kiosk, rng.choice(p.ids + p.ghosts))
            self.flags.append("kiosk")
            if rng.random() < 0.5:
                self.people.append(Person([kiosk], []))
        if rng.random() < 0.10:
            # a long chain, linked one after the next in a random order
            n = rng.randint(50, 300)
            tag = b"ch%d_" % rng.randint(0, 9)
            ids = [b"%s%d" % (tag, i) for i in range(n)]
            ids = [x for x in ids if x not in self.used_ids]
            self.used_ids.update(ids)
            rng.shuffle(ids)
            for a, b in zip(ids, ids[1:]):
                self.add_link(a, b)
            self.people.append(Person(rng.sample(ids, min(len(ids), rng.randint(2, 6))), []))
            self.flags.append("chain:%d" % n)
        if rng.random() < 0.25:
            self.links.append((b"user_id_a", b"user_id_b"))      # a data row equal to the header
            self.flags.append("link_header_row")
            if rng.random() < 0.7:
                self.people.append(Person([rng.choice([b"user_id_a", b"user_id_b"])], []))

    def add_link(self, a, b):
        rng = self.rng
        if rng.random() < 0.5:
            a, b = b, a
        self.links.append((a, b))
        if rng.random() < 0.15:
            self.links.append(rng.choice([(a, b), (b, a)]))

    def link_person(self, p):
        rng = self.rng
        ids = p.ids + p.ghosts
        if rng.random() < 0.08:
            x = rng.choice(ids)
            self.links.append((x, x))
        if rng.random() < 0.08:
            x = rng.choice(ids)
            self.links.append(rng.choice([(x, b""), (b"", x), (b"", b"")]))
        if len(ids) < 2:
            return
        order = ids[:]
        rng.shuffle(order)
        shape = rng.choice(["chain", "star", "tree", "cycle"])
        pairs = []
        for i in range(1, len(order)):
            if shape == "chain" or shape == "cycle":
                pairs.append((order[i - 1], order[i]))
            elif shape == "star":
                pairs.append((order[0], order[i]))
            else:
                pairs.append((order[rng.randrange(i)], order[i]))
        if shape == "cycle":
            pairs.append((order[-1], order[0]))
        for a, b in pairs:
            r = rng.random()
            if r < 0.04:
                a = b""                      # an empty side drops the line: the person may split
            elif r < 0.08:
                a = a.upper()                # another ID
            elif r < 0.10:
                self.raw["links"].append(a + b"," + b + rng.choice([b",", b'"', b",x"]))
                continue
            self.add_link(a, b)

    # ------------------------------------------------------------------ events
    def pick_uid(self, p):
        rng = self.rng
        r = rng.random()
        if r < 0.04:
            return b""
        if r < 0.08:
            return self.variant(rng.choice(p.ids))        # usually an unlinked stranger
        if r < 0.11:
            return rng.choice(rng.choice(self.people).ids)
        if r < 0.55:
            return p.ids[0]
        return rng.choice(p.ids)

    def valid_price(self):
        rng = self.rng
        n = rng.choice(self.amounts) if rng.random() < 0.85 else rng.randint(1, 10_000_000)
        s = b"%d" % n
        r = rng.random()
        if r < 0.12:
            s = b"0" + s
        elif r < 0.18:
            s = b"0" * rng.choice([2, 5, 20, 40]) + s
        return s

    def pick_product(self):
        rng = self.rng
        r = rng.random()
        if r < 0.74:
            return rng.choice(self.product_ids)
        if r < 0.80:
            return b""
        known = rng.choice(self.product_ids)
        return rng.choice([b"p_999", b"zz9", known.upper(), known + b" ", b" " + known, known + b"\r",
                           known + b"0", known[:-1] or b"p", b"unknown", b"P1", known.capitalize(),
                           b"p" + E_ACUTE, b"\t" + known, b"product_id", b"0" + known])

    def pick_type_product_price(self):
        rng = self.rng
        r = rng.random()
        if r < 0.38:
            typ = b"view"
        elif r < 0.52:
            typ = b"cart"
        elif r < 0.93:
            typ = b"purchase"
        else:
            typ = rng.choice(BAD_TYPES)
        pid = self.pick_product()
        if typ == b"purchase":
            price = self.valid_price() if rng.random() < 0.8 else rng.choice(BAD_PRICES)
        else:
            r = rng.random()
            if r < 0.45:
                price = b""
            elif r < 0.70:
                price = self.valid_price()
            else:
                price = rng.choice(BAD_PRICES + HUGE_NUMBERS)
        return typ, pid, price

    def pick_ingest(self, t):
        """A whole-second ingest instant for an event at t."""
        rng = self.rng
        floor = t - t % 1000
        ceil = floor if t == floor else floor + 1000
        r = rng.random()
        if r < 0.66:
            return ceil + rng.randint(0, 3 * 3600) * 1000
        if r < 0.74:
            return floor + DAY                 # exactly 24 h (or a little under): kept
        if r < 0.82:
            return ceil + DAY + rng.choice([0, 1000])   # whole second ts + 0 is kept, the rest is late
        if r < 0.87:
            return ceil + DAY + rng.randint(1, 5 * 86400) * 1000
        if r < 0.95:
            return floor - rng.randint(0, 3 * 86400) * 1000   # ingested before ts: kept
        return floor + DAY - 1000

    def invalidate(self, v, t):
        rng = self.rng
        v = list(v)
        k = rng.randrange(5)
        if k == 0:
            v[2] = self.bad_ts(t, self.ws, self.we)
        elif k == 1:
            v[1] = b""
        elif k == 2:
            v[4] = rng.choice(BAD_TYPES)
        elif k == 3:
            v[5] = b""
        else:
            v[4] = b"purchase"
            v[6] = rng.choice(BAD_PRICES)
        return v

    def make_valid(self, v, p, t):
        rng = self.rng
        v = list(v)
        if v[1] == b"":
            v[1] = p.ids[0]
        v[2] = self.valid_ts(t)
        if v[4] not in GOOD_TYPES:
            v[4] = rng.choice(GOOD_TYPES)
        if v[5] == b"":
            v[5] = rng.choice(self.product_ids)
        if v[4] == b"purchase":
            v[6] = self.valid_price()
        return v

    def make_event(self, p, t):
        rng = self.rng
        eid = b"" if rng.random() < 0.03 else self.new_event_id()
        typ, pid, price = self.pick_type_product_price()
        ts = self.bad_ts(t, self.ws, self.we) if rng.random() < self.p_bad_ts else self.valid_ts(t)
        ingest = self.pick_ingest(t)
        base = [eid, self.pick_uid(p), ts, f1(ingest), typ, pid, price]
        if self.extreme == "all_events_invalid":
            base = self.invalidate(base, t)
        if eid == b"" or rng.random() >= self.p_dup:
            self.events.append(tuple(base))
            return
        # ---- copies; index 0 has the smallest ingest_ts
        n = rng.randint(1, 3)
        mode = rng.choice(["plain", "plain", "early_bad", "late_bad", "other_user", "other_ts", "tie",
                           "malformed_first", "early_late", "mixed", "early_bad", "tie_then_later"])
        ingests = [ingest]
        for _ in range(n):
            ingests.append(ingests[-1] + rng.randint(1, 600) * 1000)
        copies = []
        if mode == "plain":
            copies = [list(base) for _ in ingests]
        elif mode == "early_bad":
            good = self.make_valid(base, p, t)
            copies = [self.invalidate(good, t)] + [list(good) for _ in ingests[1:]]
        elif mode == "late_bad":
            good = self.make_valid(base, p, t)
            copies = [list(good)] + [self.invalidate(good, t) for _ in ingests[1:]]
        elif mode == "other_user":
            copies = [list(base)]
            for _ in ingests[1:]:
                c = list(base)
                c[1] = rng.choice(rng.choice(self.people).ids)
                copies.append(c)
        elif mode == "other_ts":
            copies = [list(base)]
            for _ in ingests[1:]:
                c = list(base)
                c[2] = self.valid_ts(t + rng.choice([1, -1, 1000, GAP, GAP + 1, -GAP - 1, DAY, -DAY, 3 * HOUR]))
                copies.append(c)
        elif mode == "tie":
            ingests = [ingest] * (n + 1)
            copies = [list(base) for _ in ingests]
        elif mode == "tie_then_later":
            ingests = [ingest, ingest] + [ingest + rng.randint(1, 600) * 1000 for _ in range(n)]
            good = self.make_valid(base, p, t)
            copies = [list(base), list(base)] + [list(good) for _ in range(n)]
        elif mode == "malformed_first":
            good = self.make_valid(base, p, t)
            first = list(base)
            first[3] = f1(ingests[0])
            line = self.render(first)
            bad = self.malform(line, 7, keep_head=True)
            if bad is not None:
                self.raw["events"].append(bad)
            copies = [None] + [list(good) for _ in ingests[1:]]
            if rng.random() < 0.5 and len(copies) > 2:
                copies[2] = self.invalidate(good, t)
        elif mode == "early_late":
            good = self.make_valid(base, p, t)
            ceil = t - t % 1000 + (1000 if t % 1000 else 0)
            ingests = [ceil + DAY + rng.choice([1000, 2000, HOUR])]
            copies = [list(good)]
            for _ in range(n):
                ingests.append(ingests[-1] + rng.randint(1, 600) * 1000)
                c = list(good)
                c[2] = self.valid_ts(ingests[-1] - rng.randint(0, 3600) * 1000)
                copies.append(c)
        else:
            copies = [list(base)]
            for _ in ingests[1:]:
                typ, pid, price = self.pick_type_product_price()
                copies.append([eid, self.pick_uid(p), self.valid_ts(t + rng.randint(-HOUR, HOUR)), b"",
                               typ, pid, price])
        for ing, c in zip(ingests, copies):
            if c is None:
                continue
            c[0] = eid
            c[3] = f1(ing)
            self.events.append(tuple(c))

    # ------------------------------------------------------------------ clicks
    def click_user(self, p):
        rng = self.rng
        r = rng.random()
        if r < 0.03:
            return b""
        if r < 0.07:
            return self.variant(rng.choice(p.ids))
        return rng.choice(p.ids + p.ghosts)

    def click_campaign(self):
        rng = self.rng
        if rng.random() < 0.12:
            known = rng.choice(self.campaign_ids)
            return rng.choice(BAD_CAMPAIGNS + [known.upper(), b" " + known, known + b" ", known + b"\r",
                                               known[:2] + b"0" + known[2:], known + b"0"])
        return rng.choice(self.campaign_ids)

    def make_click(self, p, t, cid=None, camp=None):
        rng = self.rng
        if cid is None:
            cid = b"" if rng.random() < 0.03 else self.new_click_id()
        if camp is None:
            camp = self.click_campaign()
        if rng.random() < 0.07:
            ts = self.bad_ts(t, self.ws - REACH, self.we)
        else:
            ts = self.valid_ts(t)
        vals = (cid, self.click_user(p), camp, ts)
        self.clicks.append(vals)
        if rng.random() < 0.12:
            for _ in range(rng.randint(1, 2)):
                self.clicks.append(vals)

    def tie_group(self, p, t):
        rng = self.rng
        base = self.new_click_id()
        fam = [base + b"a", base + b"0", base.upper(), base.swapcase(), base + b" ", base + E_ACUTE,
               base + b"x" * 40 + b"a", base + b"x" * 40 + b"b", base[:-1], b"0" + base, b"~" + base,
               base + b"\x7f", base + b"z", b" " + base, base + b"A"]
        ids = [base]
        for cand in rng.sample(fam, rng.randint(1, 3)):
            if cand and cand not in self.used_cids:
                self.used_cids.add(cand)
                ids.append(cand)
        camps = self.campaign_ids[:]
        rng.shuffle(camps)
        for i, cid in enumerate(ids):
            camp = camps[i % len(camps)]
            if rng.random() < 0.1:
                camp = rng.choice(BAD_CAMPAIGNS)   # an unknown campaign must not win the tie
            self.make_click(p, t, cid=cid, camp=camp)

    def session_clicks(self, p, s):
        rng = self.rng
        if rng.random() < 0.35:
            return
        for o in rng.sample(range(9), rng.randint(1, 4)):
            if o == 0:
                self.make_click(p, s - REACH)
            elif o == 1:
                self.make_click(p, s - REACH - 1)
            elif o == 2:
                self.make_click(p, s)
            elif o == 3:
                self.make_click(p, s + 1)
            elif o == 4:
                self.make_click(p, s - rng.randint(1, REACH - 1))
            elif o == 5:
                self.make_click(p, rng.randint(self.ws - REACH, self.we - 1))
            elif o == 6:
                self.tie_group(p, rng.choice([s, s - REACH, s - rng.randint(1, HOUR)]))
            elif o == 7:
                self.make_click(p, s - rng.choice([1, 999, 1000, 1001, GAP]))
            else:
                self.make_click(p, s - REACH + rng.choice([1, 999, 1000, -1000]))

    # ------------------------------------------------------------------ timelines
    def start_time(self):
        rng = self.rng
        ws, we = self.ws, self.we
        r = rng.random()
        day = ws + rng.randrange(self.ndays) * DAY
        if r < 0.50:
            t = rng.randrange(ws, we)
            return t - t % 1000 if rng.random() < 0.7 else t
        if r < 0.72:
            return day + DAY - rng.randint(1, 1800) * 1000 + rng.choice([0, 0, 1, 999])
        if r < 0.80:
            return ws
        if r < 0.86:
            return ws - rng.randint(1, 2400) * 1000
        if r < 0.94:
            return we - rng.randint(1, 2400) * 1000 + rng.choice([0, 0, 999])
        return day + rng.choice([0, 1, GAP, DAY - 1, DAY - GAP - 1])

    def gap(self):
        rng = self.rng
        r = rng.random()
        if r < 0.15:
            return 0
        if r < 0.28:
            return rng.randint(1, 999)
        if r < 0.50:
            return rng.randint(1, 600) * 1000
        if r < 0.60:
            return rng.randint(1, 29) * MINUTE
        if r < 0.68:
            return GAP
        if r < 0.76:
            return GAP + 1
        if r < 0.80:
            return rng.choice([GAP - 1, GAP + 999, GAP + 1000, GAP - 1000, GAP + 2])
        if r < 0.93:
            return rng.randint(31, 300) * MINUTE
        return rng.randint(5, 80) * HOUR + rng.choice([0, 0, rng.randint(0, 999)])

    def walk(self, p, n):
        rng = self.rng
        t = self.start_time()
        new_session = True
        for _ in range(n):
            if new_session and self.extreme != "no_clicks":
                self.session_clicks(p, t)
            self.make_event(p, t)
            g = self.gap()
            new_session = g > GAP
            t += g
            if t >= self.we + HOUR and rng.random() < 0.8:
                t = self.start_time()
                new_session = True

    def build_activity(self):
        rng = self.rng
        if self.scale == "medium":
            total = rng.randint(3400, 11500)
        else:
            total = rng.randint(32, 240)
        people = self.people
        weights = [1.0 / (i + 1) for i in range(len(people))]
        rng.shuffle(weights)
        wsum = sum(weights)
        for p, w in zip(people, weights):
            n = int(total * w / wsum + rng.random())
            while n > 0:
                k = min(n, rng.randint(1, 60 if self.scale == "medium" else 12))
                self.walk(p, k)
                n -= k
        if self.extreme == "one_user_thousands":
            p = rng.choice(people)
            for _ in range(rng.randint(20, 60)):
                self.walk(p, rng.randint(30, 150))
        # exact window edges
        for _ in range(rng.randint(2, 8)):
            p = rng.choice(people)
            t = rng.choice([self.ws, self.ws - 1, self.we - 1, self.we, self.ws + 1, self.we - 1000,
                            self.ws - 1000, self.we + 1])
            self.make_event(p, t)
            if self.extreme != "no_clicks":
                c = rng.choice([self.ws - REACH, self.ws - REACH - 1, self.we, self.we - 1, self.ws,
                                self.ws - 1, self.ws - REACH + 1, self.ws - REACH - 1000, t, t - REACH])
                if rng.random() < 0.3:
                    self.tie_group(p, c)
                else:
                    self.make_click(p, c)
        if self.extreme != "no_clicks":
            for _ in range(rng.randint(0, 6)):     # clicks of people without any event, far away
                self.make_click(rng.choice(people), rng.randint(self.ws - 9 * DAY, self.we + DAY))
            if rng.random() < 0.3:
                self.clicks.append((b"click_id", b"user_id", b"campaign_id", b"ts"))
                self.flags.append("click_header_row")

    # ------------------------------------------------------------------ lines
    def render(self, vals):
        rng = self.rng
        q = self.q_quote
        return b",".join(b'"' + v + b'"' if rng.random() < q else v for v in vals)

    def malform(self, line, n, keep_head=False):
        """A malformed version of a rendered line, or None when the attempt came out well-formed."""
        rng = self.rng
        f = line.split(b",")
        k = rng.randrange(18)
        lo = 4 if keep_head and len(f) > 4 else 0     # keep event_id .. ingest_ts readable
        i = rng.randrange(lo, len(f))
        if k == 0:
            out = b",".join(f[:-1])
        elif k == 1:
            out = line + rng.choice([b",", b",x", b',""', b",,"])
        elif k == 2:
            out = line + b'"' if keep_head else b'"' + line
        elif k == 3:
            out = line + b'"'
        elif k == 4:
            x = f[i]
            j = rng.randint(0, len(x))
            f[i] = x[:j] + b'"' + x[j:]
            out = b",".join(f)
        elif k == 5:
            f[i] = b'"'
            out = b",".join(f)
        elif k == 6:
            x = f[i].strip(b'"')
            f[i] = b'"' + x + b"," + rng.choice([b"", b"x", x]) + b'"'
            out = b",".join(f)
        elif k == 7:
            out = b""
        elif k == 8:
            out = b'"' + line + b'"'
        elif k == 9:
            x = f[i].strip(b'"')
            f[i] = rng.choice([b'"' + x + b'""' + x + b'"', b'""""', b'"""', b'"' + x + b'"x', b'x"' + x + b'"',
                               b'""' + x + b'""', b"'" + x + b'"', x + b'""'])
            out = b",".join(f)
        elif k == 10:
            x = f[i].strip(b'"')
            f[i] = rng.choice([b' "' + x + b'"', b'"' + x + b'" ', b'\t"' + x + b'"', b'"' + x + b'"\t'])
            out = b",".join(f)
        elif k == 11:
            out = b",".join(f[:-1] + [b'"' + f[-1].strip(b'"') + b'"\r'])
        elif k == 12 and len(f) > 2:
            j = rng.randrange(max(lo, 1), len(f)) if keep_head else rng.randrange(1, len(f))
            out = b",".join(f[:j - 1] + [b'"' + f[j - 1].strip(b'"') + b"," + f[j].strip(b'"') + b'"'] + f[j + 1:])
        elif k == 13:
            j = rng.randrange(max(lo, 1), len(f)) if keep_head else rng.randrange(len(f))
            out = b",".join(f[:j] + f[j + 1:])
        elif k == 14:
            out = b",".join(f[:i] + [rng.choice([b"", b"x", f[i]])] + f[i:])
        elif k == 15:
            out = line.replace(b",", b";") if not keep_head else line + b";"
            if out == line + b";":
                out = line + b',"'
        elif k == 16:
            out = line + b"," * rng.randint(1, 9)
        else:
            out = line + rng.choice([b'"', b',"', b',"x', b'"x'])
        return out if split_fields(out, n) is None else None

    def junk_line(self, n):
        rng = self.rng
        k = rng.randrange(14)
        if k == 0:
            return b""
        if k == 1:
            return rng.choice([b" ", b"\r", b"\t", b"garbage", b"#comment", b"-----", b"EOF", b"\x1a"])
        if k == 2:
            return b'"'
        if k == 3:
            return b'""'
        if k == 4:
            return b"," * rng.choice([n - 2, n, n + 1, 1]) if n > 2 else b",,"
        if k == 5:
            return b'{"a":1,"b":2}'
        if k == 6:
            return HEADERS_BY_N[n].replace(b",", b";")
        if k == 7:
            return HEADERS_BY_N[n] + b","
        if k == 8:
            return b'"' + HEADERS_BY_N[n] + b'"'
        if k == 9:
            return b",".join([b'"'] * n)
        if k == 10:
            return b",".join([b"x"] * (n + rng.choice([-1, 1, 2])))
        if k == 11:
            return b'"' * rng.choice([3, 4, 5, 6])
        if k == 12:
            return b",".join([b'""'] * (n - 1))
        return HEADERS_BY_N[n][:-1] + b'"'

    def long_junk(self, n):
        rng = self.rng
        size = rng.choice([5000, 20000, 65536, 65537, 200000, rng.randint(5000, 200000)])
        k = rng.randrange(6)
        if k == 0:
            return b"x" * size
        if k == 1:
            return b"x," * (size // 2)
        if k == 2:
            return b'"' + b"y" * size
        if k == 3:
            return b"z" * size + b'"'
        if k == 4:
            return (b'"q",' * (size // 4))[:-1]
        return b",".join([b"w" * (size // (n + 1))] * (n + 1))

    # ------------------------------------------------------------------ files
    def file_names(self, kind, count):
        rng = self.rng
        styles = ["part-%05d.csv", "%d.csv", kind + "-%d.csv", "x%d.y.csv", "data %d.csv", "PART_%d.csv",
                  "p%d.csv.csv", "é%d.csv", "part-%05d.csv", "part-%05d.csv", "%d.bak.csv", "csv%d.csv"]
        one = rng.choice(styles)
        names = []
        seen = set()
        start = rng.choice([0, 0, 1, 7, 1000])
        for i in range(count):
            style = one if rng.random() < 0.85 else rng.choice(styles)
            name = style % (start + i)
            while name in seen:
                name = "z" + name
            seen.add(name)
            names.append(name)
        return names

    def write_table(self, outdir, kind, records):
        rng = self.rng
        n = NFIELDS[kind]
        folder = os.path.join(outdir, kind)
        os.makedirs(folder, exist_ok=True)
        if self.scale == "medium":
            nfiles = rng.randint(1, {"events": 40, "clicks": 20, "links": 10}[kind])
        else:
            nfiles = rng.randint(1, {"events": 10, "clicks": 6, "links": 5}[kind])
        files = [[] for _ in range(nfiles)]
        weights = [1.0 / (i + 1) for i in range(nfiles)]
        rng.shuffle(weights)
        crlf_data = rng.random() < 0.25
        crlf = [crlf_data and rng.random() < 0.4 for _ in range(nfiles)]
        if any(crlf):
            self.flags.append("crlf:" + kind)
        registry = {}
        picks = rng.choices(range(nfiles), weights, k=len(records)) if records else []
        for vals, fi in zip(records, picks):
            vals = list(vals)
            header_row = kind == "events" and vals[0] == b"event_id" and vals[3] == b"ingest_ts"
            if crlf[fi] and rng.random() < 0.9 and not header_row:
                vals[-1] += b"\r"
            if kind == "events" and vals[0] != b"" and not header_row:
                vals = list(registry.setdefault((vals[0], vals[3]), tuple(vals)))
            elif kind == "clicks" and vals[0] != b"":
                vals = list(registry.setdefault(vals[0], tuple(vals)))
            line = self.render(vals)
            if rng.random() < self.p_malformed:
                bad = self.malform(line, n)
                if bad is not None:
                    line = bad
                    if rng.random() < 0.3:      # the well-formed row is written as well
                        files[rng.randrange(nfiles)].append(self.render(vals))
            files[fi].append(line)
        for raw in self.raw[kind]:
            if split_fields(raw, n) is None or kind == "links":
                files[rng.randrange(nfiles)].append(raw)
        njunk = int(len(records) * self.p_junk) + (1 if rng.random() < 0.3 else 0)
        for _ in range(njunk):
            j = self.junk_line(n)
            if split_fields(j, n) is None:
                files[rng.randrange(nfiles)].append(j)
        if rng.random() < 0.08:
            for _ in range(rng.randint(1, 2)):
                j = self.long_junk(n)
                if split_fields(j, n) is None:
                    files[rng.randrange(nfiles)].append(j)
            self.flags.append("long_junk:" + kind)
        for lines in files:
            rng.shuffle(lines)
        for _ in range(rng.choice([0, 0, 1, 1, 2, 3])):          # files that hold only the header
            files.insert(rng.randint(0, len(files)), [])
        if kind == "links" and self.extreme == "no_links" and rng.random() < 0.5:
            files = []
            self.flags.append("no_link_files")
        names = self.file_names(kind, len(files))
        header = HEADERS[kind]
        for name, lines in zip(names, files):
            body = b"\n".join([header] + lines) + b"\n"
            if rng.random() < 0.03 and not (lines and lines[-1] == b""):
                body = body[:-1]                                  # no final newline
                self.flags.append("no_final_newline:" + kind)
            with open(os.path.join(folder, name), "wb") as handle:
                handle.write(body)
        # files that are not *.csv and must be ignored; their rows would change the answer
        for decoy in rng.sample(["notes.txt", "part-1.csv.bak", "part-00000.csv.tmp", "x.csv~", "_SUCCESS",
                                 "README", kind + ".csv.gz", "csv"], rng.randint(0, 3)):
            lines = [header]
            for i in range(rng.randint(0, 5)):
                a = rng.choice(rng.choice(self.people).ids)
                b = rng.choice(rng.choice(self.people).ids)
                t = rng.randrange(self.ws, self.we) // 1000 * 1000
                if kind == "events":
                    lines.append(b"zz_decoy%d,%s,%s,%s,purchase,%s,7777" % (i, a, f1(t), f1(t), self.product_ids[0]))
                elif kind == "clicks":
                    lines.append(b"!decoy%d,%s,%s,%d" % (i, a, self.campaign_ids[0], t))
                else:
                    lines.append(a + b"," + b)
            with open(os.path.join(folder, decoy), "wb") as handle:
                handle.write(b"\n".join(lines) + b"\n")

    def write(self, outdir):
        rng = self.rng
        os.makedirs(outdir, exist_ok=True)
        with open(os.path.join(outdir, "products.csv"), "wb") as handle:
            handle.write(b"product_id,category\n" + b"".join(p + b"," + c + b"\n" for p, c in self.products))
        with open(os.path.join(outdir, "campaigns.csv"), "wb") as handle:
            handle.write(b"campaign_id,channel\n" + b"".join(c + b"," + ch + b"\n" for c, ch in self.campaigns))
        manifest = {"window_start": f1(self.ws).decode(), "window_end": f1(self.we).decode()}
        text = json.dumps(manifest, indent=2) if rng.random() < 0.7 else json.dumps(manifest)
        with open(os.path.join(outdir, "manifest.json"), "wb") as handle:
            handle.write(text.encode() + b"\n")
        events = [] if self.extreme == "no_events" else self.events
        clicks = [] if self.extreme == "no_clicks" else self.clicks
        links = [] if self.extreme == "no_links" else self.links
        if self.extreme == "no_events":
            self.raw["events"] = []
        if self.extreme == "no_clicks":
            self.raw["clicks"] = []
        if self.extreme == "no_links":
            self.raw["links"] = []
        if events and rng.random() < 0.05:
            # A data row equal to the header. Its ingest_ts is not F1, so strictly it is outside the
            # input guarantees; it is unique (no other copy of event_id "event_id") and is dropped
            # for its ts whatever dedup does. Flagged so a failure can be triaged at once.
            events = events + [tuple(EVENT_HEADER.split(b","))]
            self.flags.append("event_header_row")
        self.write_table(outdir, "events", events)
        self.write_table(outdir, "clicks", clicks)
        self.write_table(outdir, "links", links)
        return {"seed": self.seed, "scale": self.scale, "flags": self.flags,
                "event_records": len(events), "click_records": len(clicks), "link_records": len(links),
                "people": len(self.people), "window": [manifest["window_start"], manifest["window_end"]]}


HEADERS_BY_N = {7: EVENT_HEADER, 4: CLICK_HEADER, 2: LINK_HEADER}


def generate(seed, outdir, scale="small"):
    """Write the dataset of (seed, scale) into outdir (which should not exist or be empty)."""
    if scale not in ("small", "medium"):
        raise ValueError("scale must be small or medium")
    g = Gen(int(seed), scale)
    g.setup()
    g.build_people()
    g.build_activity()
    return g.write(outdir)


# ----------------------------------------------------------------------------- self-check

def check_guarantees(data_dir):
    """Violations of the input guarantees of rules.md in a dataset on disk ([] when clean)."""
    bad = []
    with open(os.path.join(data_dir, "manifest.json"), "rb") as handle:
        manifest = json.loads(handle.read())
    for key in ("window_start", "window_end"):
        v = manifest[key].encode()
        if not is_valid_f1(v) or not v.endswith(b"T00:00:00Z"):
            bad.append("manifest %s is not a UTC midnight: %r" % (key, v))
    for name, header, pattern in (("products.csv", b"product_id,category", rb"[A-Za-z0-9_]+"),
                                  ("campaigns.csv", b"campaign_id,channel", rb"c_[1-9][0-9]*")):
        with open(os.path.join(data_dir, name), "rb") as handle:
            data = handle.read()
        lines = data.split(b"\n")
        if lines[0] != header or lines[-1] != b"":
            bad.append("%s: bad header or no final newline" % name)
        seen = set()
        for line in lines[1:-1]:
            f = line.split(b",")
            if len(f) != 2 or not re.fullmatch(pattern, f[0]) or not re.fullmatch(rb"[a-z0-9_]+", f[1]):
                bad.append("%s: unclean line %r" % (name, line))
                continue
            if f[0] in seen:
                bad.append("%s: duplicate id %r" % (name, f[0]))
            seen.add(f[0])
            if f[1] in (b"unknown", b"organic") or f[0] == b"organic":
                bad.append("%s: reserved name in %r" % (name, line))
    for kind in ("events", "clicks", "links"):
        folder = os.path.join(data_dir, kind)
        if not os.path.isdir(folder):
            bad.append("%s/ is missing" % kind)
            continue
        copies = {}
        for name in sorted(os.listdir(folder)):
            if not name.endswith(".csv"):
                continue
            with open(os.path.join(folder, name), "rb") as handle:
                data = handle.read()
            lines = data.split(b"\n")
            if lines[-1] == b"":
                lines.pop()
            if not lines or lines[0] != HEADERS[kind]:
                bad.append("%s/%s: first line is not the header" % (kind, name))
                continue
            for line in lines[1:]:
                f = split_fields(line, NFIELDS[kind])
                if f is None:
                    continue
                if kind == "events":
                    if f == EVENT_HEADER.split(b","):
                        continue      # the flagged header-like row (see Gen.write)
                    if not is_valid_f1(f[3]):
                        bad.append("%s/%s: ingest_ts is not F1 in %r" % (kind, name, line[:200]))
                    key = (f[0], f[3])
                elif kind == "clicks":
                    key = f[0]
                else:
                    continue
                if f[0] == b"":
                    continue
                if copies.setdefault(key, f) != f:
                    bad.append("%s/%s: copies differ for %r: %r vs %r" % (kind, name, key, copies[key], f))
    return bad


def main(argv):
    args = [a for a in argv[1:] if not a.startswith("--")]
    scale = "small"
    if "--scale" in argv:
        scale = argv[argv.index("--scale") + 1]
        args.remove(scale)
    if len(args) != 2:
        print(__doc__)
        return 2
    meta = generate(int(args[0]), args[1], scale)
    if "--check" in argv:
        problems = check_guarantees(args[1])
        meta["violations"] = problems
        print(json.dumps(meta))
        return 1 if problems else 0
    print(json.dumps(meta))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
