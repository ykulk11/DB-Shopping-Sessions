#!/usr/bin/env python3
"""Rule-literal reference for Shopping Sessions (rules.md), written for the semantics audit.

Deliberately simple (dicts and lists, no dataframe library) and deliberately independent of
the optimized pipeline. It also carries switchable "mutants": named, deliberate mistakes that
an optimized implementation could make. Running the mutants against the fixtures and against
generated datasets shows which mistakes the fixtures / the dev set would actually catch.

  ref.py run DATA_DIR OUT_DIR             write user_daily.csv + campaign_daily.csv (compare.py format)
  ref.py fixtures FIXTURES_DIR            check the base implementation against every fixture
  ref.py mutants FIXTURES_DIR DATA_DIR... run every mutant on the fixtures and on each dataset
  ref.py stats DATA_DIR                   count how often each trap occurs in a dataset
"""
import bisect
import glob
import json
import os
import re
import sys
from collections import defaultdict

DAY = 86_400_000
GAP = 1_800_000
REACH = 7 * DAY
ORGANIC = b"organic"
UNKNOWN = b"unknown"
EVENT_HEADER = b"event_id,user_id,ts,ingest_ts,event_type,product_id,price_cents"
CLICK_HEADER = b"click_id,user_id,campaign_id,ts"
LINK_HEADER = b"user_id_a,user_id_b"

# ----------------------------------------------------------------------------- parsing


def split_line(line, n):
    """Unquoted fields plus a bitmask of the quoted ones, or None when malformed."""
    if not line:
        return None
    f = line.split(b",")
    if len(f) != n:
        return None
    mask = 0
    if b'"' in line:
        for i, x in enumerate(f):
            if b'"' in x:
                if len(x) < 2 or x[0] != 34 or x[-1] != 34 or b'"' in x[1:-1]:
                    return None
                f[i] = x[1:-1]
                mask |= 1 << i
    f.append(mask)
    return f


def load_dir(folder, n):
    good, bad = [], []
    for path in sorted(glob.glob(os.path.join(folder, "*.csv"))):
        with open(path, "rb") as handle:
            data = handle.read()
        lines = data.split(b"\n")
        if lines and lines[-1] == b"":
            lines.pop()
        for line in lines[1:]:  # the first line is the header, whatever it holds
            f = split_line(line, n)
            if f is None:
                bad.append(line)
            else:
                good.append(f)
    return good, bad


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


_DIM = (31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31)
_TS = re.compile(rb"(\d{4})-(\d\d)-(\d\d)T(\d\d):(\d\d):(\d\d)(?:Z|([+-])(\d\d):(\d\d))")


def parse_ts(b):
    """Milliseconds since the epoch, or None for a bad timestamp (window not checked)."""
    if b.isdigit():  # bytes.isdigit: ASCII 0-9 only, and not empty
        return int(b)
    m = _TS.fullmatch(b)  # \d in a bytes pattern is [0-9] only
    if not m:
        return None
    y, mo, d, h, mi, s = (int(m.group(i)) for i in range(1, 7))
    if not 1 <= mo <= 12:
        return None
    dim = _DIM[mo - 1] + (mo == 2 and y % 4 == 0 and (y % 100 != 0 or y % 400 == 0))
    if not (1 <= d <= dim and h <= 23 and mi <= 59 and s <= 59):
        return None
    t = (days_from_civil(y, mo, d) * 86400 + h * 3600 + mi * 60 + s) * 1000
    if m.group(7):
        oh, om = int(m.group(8)), int(m.group(9))
        if oh > 23 or om > 59:
            return None
        off = (oh * 60 + om) * 60_000
        t = t - off if m.group(7) == b"+" else t + off
    return t


def load(data_dir):
    with open(os.path.join(data_dir, "manifest.json")) as handle:
        manifest = json.load(handle)
    ws = parse_ts(manifest["window_start"].encode())
    we = parse_ts(manifest["window_end"].encode())
    ev, ev_bad = load_dir(os.path.join(data_dir, "events"), 7)
    cl, cl_bad = load_dir(os.path.join(data_dir, "clicks"), 4)
    ln, ln_bad = load_dir(os.path.join(data_dir, "links"), 2)
    events = []
    for eid, uid, ts, ing, typ, pid, price, mask in ev:
        events.append((eid, uid, parse_ts(ts), parse_ts(ing), typ, pid, price, mask, ing))
    clicks = [(cid, uid, camp, parse_ts(ts)) for cid, uid, camp, ts, _ in cl]
    links = [(a, b) for a, b, _ in ln]
    products = {}
    with open(os.path.join(data_dir, "products.csv"), "rb") as handle:
        for line in handle.read().split(b"\n")[1:]:
            if line:
                pid, cat = line.split(b",")
                products[pid] = cat
    campaigns = {}
    with open(os.path.join(data_dir, "campaigns.csv"), "rb") as handle:
        for line in handle.read().split(b"\n")[1:]:
            if line:
                cid, channel = line.split(b",")
                campaigns[cid] = channel
    return dict(ws=ws, we=we, events=events, events_bad=ev_bad, clicks=clicks, clicks_bad=cl_bad,
                links=links, links_bad=ln_bad, products=products, campaigns=campaigns)


# ----------------------------------------------------------------------------- pipeline

MUTANTS = {
    # --- events: order of the steps
    "validate_before_dedup": "drop invalid rows first, then dedup (a later valid copy rescues the event)",
    "late_before_dedup_and_validate_before_dedup": "drop invalid AND late rows first, then dedup",
    "drop_empty_user_before_dedup": "only the empty-user check runs before dedup",
    "drop_bad_ts_before_dedup": "only the bad/out-of-window ts check runs before dedup",
    "dedup_per_user": "dedup on (user_id, event_id), e.g. after partitioning rows by raw user",
    "dedup_per_person": "dedup on (person, event_id), e.g. after partitioning rows by person",
    "dedup_keep_latest_ingest": "keep the copy with the largest ingest_ts",
    "dedup_keep_first_read": "keep the first copy read (file order)",
    "dedup_keep_last_read": "keep the last copy read (file order)",
    "dedup_ingest_raw_bytes": "compare ingest_ts as raw field bytes, quotes included",
    "dedup_malformed_copy_wins": "a malformed line with a readable event_id+ingest_ts takes part in dedup",
    "no_event_dedup": "no dedup at all",
    "late_before_dedup": "rows that look late (ts parses, ingest_ts - ts > 24 h) dropped before dedup",
    "dedup_min_ingest_but_first_read_fields": "smallest ingest_ts tracked, other fields taken from the first copy read",
    "late_ingest_of_any_copy": "lateness judged on the largest ingest_ts of the copies",
    # --- late rule
    "late_ge": "drop when ingest_ts - ts >= 24 h",
    "late_unsigned": "unsigned subtraction: ingest_ts before ts counts as late",
    "no_late": "late events are not dropped",
    # --- validation
    "bad_purchase_kept_as_event": "a purchase with a bad price stays as an event worth 0",
    "sum_all_prices": "digits-only prices of view/cart rows are added to revenue",
    "price_checked_for_all_types": "view/cart with a non-empty bad price is dropped",
    "type_casefold": "event_type compared case-insensitively / trimmed",
    "window_end_inclusive": "ts == window_end accepted",
    "empty_product_ok": "an empty product_id is accepted (category unknown)",
    # --- linked ids
    "no_links": "every user_id is its own person",
    "links_only_known_ids": "union only link lines whose two IDs both occur in an event or click",
    "name_from_event_ids_only": "person named by the smallest ID seen in its surviving events",
    "name_from_used_ids_only": "person named by the smallest ID seen in any event or click row",
    "name_len_then_bytes": "smallest ID chosen by (length, bytes) instead of bytes",
    "name_largest": "person named by the largest ID",
    "ids_casefold": "user IDs lower-cased everywhere",
    "skip_header_like_rows": "a data row equal to the header is skipped",
    "links_not_transitive": "only direct links (one hop from the smallest ID) are honoured",
    # --- sessions
    "gap_ge": "a gap of exactly 30 min starts a new session",
    "gap_seconds": "gaps measured on timestamps truncated to seconds",
    "gap_from_session_start": "session ends 30 min after its first event",
    "sessions_per_user_id": "sessionize per raw user_id, then report under the person",
    "day_per_event": "events and revenue counted on each event's own UTC day",
    "day_of_last_event": "session day taken from its last event",
    # --- categories
    "top_tie_largest": "top_category tie goes to the byte-wise largest",
    "unknown_sorts_last": "ties broken by an index where unknown is last",
    "unknown_sorts_first": "ties broken by an index where unknown is first",
    "unknown_never_top": "purchases of unknown products do not compete for top_category",
    "top_by_count": "top_category by number of purchases",
    "product_casefold": "product IDs matched case-insensitively",
    # --- clicks
    "no_click_dedup": "clicks are not deduplicated",
    "click_no_window_check": "the click window check is skipped",
    "click_no_user_check": "the click empty-user check is skipped",
    "click_empty_id_kept": "clicks with an empty click_id are kept",
    "clicks_before_window_dropped": "clicks before window_start are dropped (event window applied to clicks)",
    "campaign_casefold": "campaign IDs matched case-insensitively",
    "invalid_campaign_shadows": "latest click picked first, campaign checked after (invalid one -> organic)",
    "tie_largest_id": "ts tie goes to the largest click_id",
    "tie_len_then_bytes": "ts tie goes to the smallest click_id by (length, bytes)",
    "tie_first_read": "ts tie goes to the first click read",
    "tie_last_read": "ts tie goes to the last click read",
    "tie_casefold": "ts tie compared case-insensitively",
    "reach_exclusive": "a click exactly 7 days before the start does not count",
    "reach_plus_1ms": "a click 7 days + 1 ms before the start still counts",
    "start_exclusive": "a click exactly at the session start does not count",
    "start_plus_1ms": "a click 1 ms after the session start still counts",
    "click_seconds": "click/session comparison on timestamps truncated to seconds",
    "clicks_by_raw_user": "a click only reaches sessions whose events use the click's own user_id",
    "clicks_need_event_id": "clicks from IDs that never occur in an event are ignored",
    "earliest_click": "the earliest qualifying click is credited",
    "credit_window_from_first_event_day": "7 days measured from 00:00 of the session day",
    # --- assists
    "assist_excludes_credit": "the crediting campaign does not assist",
    "assist_counts_clicks": "one assist per qualifying click, not per campaign",
    "assist_reach_exclusive": "assist window excludes the exact 7-day edge",
    "assist_only_latest_per_campaign": "only the person's latest click of each campaign is kept",
    "assist_day_of_click": "assists reported on the click's day",
    "organic_assists": "organic sessions count one organic assist",
    "no_assist_only_rows": "rows with 0 credited sessions are not written",
}


def pipeline(D, mut=frozenset(), stats=None):
    ws, we = D["ws"], D["we"]
    products, campaigns = D["products"], D["campaigns"]
    fold = "ids_casefold" in mut

    # ------------------------------------------------------------------ linked IDs
    links = D["links"]
    if "skip_header_like_rows" in mut:
        links = [l for l in links if l != (b"user_id_a", b"user_id_b")]
    used_ids = None
    if "links_only_known_ids" in mut or "name_from_used_ids_only" in mut:
        used_ids = {e[1] for e in D["events"]} | {c[1] for c in D["clicks"]}
    parent = {}

    def find(x):
        root = x
        while parent[root] != root:
            root = parent[root]
        while parent[x] != root:
            parent[x], x = root, parent[x]
        return root

    direct = defaultdict(set)
    if "no_links" not in mut:
        for a, b in links:
            if a == b"" or b == b"":
                continue
            if fold:
                a, b = a.lower(), b.lower()
            if "links_only_known_ids" in mut and not (a in used_ids and b in used_ids):
                continue
            parent.setdefault(a, a)
            parent.setdefault(b, b)
            direct[a].add(b)
            direct[b].add(a)
            ra, rb = find(a), find(b)
            if ra != rb:
                parent[ra] = rb
    members = defaultdict(list)
    for x in parent:
        members[find(x)].append(x)
    name = {}
    if "links_not_transitive" in mut:
        # person = smallest ID plus its direct neighbours; everything else falls apart
        parent2 = {}
        for root, ids in members.items():
            smallest = min(ids)
            for x in ids:
                parent2[x] = smallest if (x == smallest or x in direct[smallest]) else x
        person_of = parent2
    else:
        person_of = {}
        for root, ids in members.items():
            if "name_len_then_bytes" in mut:
                chosen = min(ids, key=lambda x: (len(x), x))
            elif "name_largest" in mut:
                chosen = max(ids)
            elif "name_from_used_ids_only" in mut:
                seen = [x for x in ids if x in used_ids]
                chosen = min(seen) if seen else min(ids)
            else:
                chosen = min(ids)
            for x in ids:
                person_of[x] = chosen

    def person(uid):
        if fold:
            uid = uid.lower()
        return person_of.get(uid, uid)

    # ------------------------------------------------------------------ events
    def validate(e):
        """(ts, price, is_purchase) for a valid row, else None."""
        eid, uid, ts, ing, typ, pid, price, mask, ing_raw = e
        if uid == b"":
            return None
        if ts is None or ts < ws or (ts > we if "window_end_inclusive" in mut else ts >= we):
            return None
        if "type_casefold" in mut:
            typ = typ.strip().lower()
        if typ not in (b"view", b"cart", b"purchase"):
            return None
        if pid == b"" and "empty_product_ok" not in mut:
            return None
        if typ == b"purchase":
            if not price.isdigit() or not 1 <= int(price) <= 10_000_000:
                if "bad_purchase_kept_as_event" in mut:
                    return ts, 0, False
                return None
            return ts, int(price), True
        if "price_checked_for_all_types" in mut and price != b"" and (
                not price.isdigit() or not 1 <= int(price) <= 10_000_000):
            return None
        if "sum_all_prices" in mut and price.isdigit() and 1 <= int(price) <= 10_000_000:
            return ts, int(price), True
        return ts, 0, False

    def is_late(ts, ing):
        if "no_late" in mut:
            return False
        if "late_unsigned" in mut and ing < ts:
            return True
        if "late_ge" in mut:
            return ing - ts >= DAY
        return ing - ts > DAY

    rows = D["events"]
    if "validate_before_dedup" in mut:
        rows = [e for e in rows if validate(e) is not None]
    if "late_before_dedup_and_validate_before_dedup" in mut:
        rows = [e for e in rows if validate(e) is not None and not is_late(e[2], e[3])]
    if "late_before_dedup" in mut:
        rows = [e for e in rows if not (e[2] is not None and e[3] - e[2] > DAY)]
    if "drop_empty_user_before_dedup" in mut:
        rows = [e for e in rows if e[1] != b""]
    if "drop_bad_ts_before_dedup" in mut:
        rows = [e for e in rows if e[2] is not None and ws <= e[2] < we]
    if "dedup_malformed_copy_wins" in mut:
        # A line that is malformed but still shows an event_id and an ingest_ts (what a
        # "peek at field 1 and field 4, validate later" fast path would see).
        extra = []
        for line in D["events_bad"]:
            f = [x.strip(b'"') for x in line.split(b",")]
            if len(f) >= 4 and f[0] != b"":
                ing = parse_ts(f[3])
                if ing is not None:
                    extra.append((f[0], b"", None, ing, b"", b"", b"", 0, f[3]))
        rows = rows + extra

    max_ing = {}
    kept = {}
    for e in rows:
        eid = e[0]
        if eid == b"":
            continue
        if "dedup_per_user" in mut:
            key = (e[1], eid)
        elif "dedup_per_person" in mut:
            key = (person(e[1]), eid)
        else:
            key = eid
        if "late_ingest_of_any_copy" in mut:
            if key not in max_ing or e[3] > max_ing[key]:
                max_ing[key] = e[3]
        cur = kept.get(key)
        if cur is None:
            kept[key] = e
        elif "no_event_dedup" in mut:
            kept[(key, len(kept))] = e
        elif "dedup_keep_first_read" in mut:
            pass
        elif "dedup_min_ingest_but_first_read_fields" in mut:
            if e[3] < cur[3]:
                kept[key] = cur[:3] + (e[3],) + cur[4:]
        elif "dedup_keep_last_read" in mut:
            kept[key] = e
        elif "dedup_keep_latest_ingest" in mut:
            if e[3] > cur[3]:
                kept[key] = e
        elif "dedup_ingest_raw_bytes" in mut:
            a = (b'"' + e[8] + b'"') if e[7] & 8 else e[8]
            c = (b'"' + cur[8] + b'"') if cur[7] & 8 else cur[8]
            if a < c:
                kept[key] = e
        elif e[3] < cur[3]:
            kept[key] = e

    by_person = defaultdict(list)
    for key, e in kept.items():
        v = validate(e)
        if v is None:
            continue
        ts, price, is_purchase = v
        ing = max_ing[key] if "late_ingest_of_any_copy" in mut else e[3]
        if is_late(ts, ing):
            continue
        pid = e[5]
        if "product_casefold" in mut:
            pid = pid.lower()
        group = (person(e[1]), e[1]) if "sessions_per_user_id" in mut else person(e[1])
        by_person[group].append((ts, price, is_purchase, pid, e[1]))

    # ------------------------------------------------------------------ clicks
    clicks_by_person = defaultdict(list)
    seen = set()
    event_ids = {e[1] for e in D["events"]} if "clicks_need_event_id" in mut else None
    crows = D["clicks"]
    if "click_empty_id_kept" in mut:
        for line in D["clicks_bad"]:
            f = line.split(b",")
            if len(f) == 4 and f[0] == b"" and b'"' not in line:
                crows = crows + [(b"", f[1], f[2], parse_ts(f[3]))]
    for order, (cid, uid, camp, ts) in enumerate(crows):
        if cid == b"" and "click_empty_id_kept" not in mut:
            continue
        if "no_click_dedup" not in mut and cid != b"":
            if cid in seen:
                continue
            seen.add(cid)
        if uid == b"" and "click_no_user_check" not in mut:
            continue
        if ts is None:
            continue
        if "click_no_window_check" not in mut and not ws - REACH <= ts < we:
            continue
        if "clicks_before_window_dropped" in mut and ts < ws:
            continue
        if "campaign_casefold" in mut:
            camp = camp.strip().lower()
        valid_campaign = camp in campaigns
        if not valid_campaign and "invalid_campaign_shadows" not in mut:
            continue
        if event_ids is not None and uid not in event_ids:
            continue
        clicks_by_person[person(uid)].append((ts, cid, camp if valid_campaign else None, order, uid))

    # ------------------------------------------------------------------ sessions, credit, assists
    user_daily = {}      # (person, day) -> [sessions, events, revenue, {cat: cents}, {cat: count}]
    campaign_daily = {}  # (campaign, day) -> [sessions, purchasing, revenue, assisted]
    st = stats if stats is not None else defaultdict(int)
    seconds = "gap_seconds" in mut

    def click_key(c):
        if "tie_len_then_bytes" in mut:
            return (c[0], len(c[1]), c[1])
        if "tie_casefold" in mut:
            return (c[0], c[1].lower())
        if "tie_first_read" in mut or "tie_last_read" in mut:
            return (c[0], c[3])
        return (c[0], c[1])

    for group, evs in by_person.items():
        p = group[0] if "sessions_per_user_id" in mut else group
        evs.sort(key=lambda x: x[0])
        # sessions: [start, events, revenue, has_purchase, [(ts, price, pid)...], user ids]
        sessions = []
        prev = None
        for ts, price, is_purchase, pid, uid in evs:
            if prev is None:
                new = True
            elif "gap_from_session_start" in mut:
                new = ts - sessions[-1][0] > GAP
            elif seconds:
                new = ts // 1000 - prev // 1000 > GAP // 1000
            elif "gap_ge" in mut:
                new = ts - prev >= GAP and ts != prev
            else:
                new = ts - prev > GAP
            if stats is not None and prev is not None:
                if ts - prev == GAP:
                    st["gap_exactly_30min"] += 1
                elif ts - prev == GAP + 1:
                    st["gap_30min_plus_1ms"] += 1
                if ts == prev:
                    st["equal_ts_neighbours"] += 1
            if new:
                sessions.append([ts, 0, 0, False, [], set(), ts])
            s = sessions[-1]
            s[1] += 1
            s[6] = ts
            s[5].add(uid)
            if is_purchase:
                s[2] += price
                s[3] = True
                s[4].append((ts, price, pid))
            elif "day_per_event" in mut:
                s[4].append((ts, 0, None))
            prev = ts

        cl = sorted(clicks_by_person.get(p, ()), key=click_key)
        if "assist_only_latest_per_campaign" in mut:
            latest = {}
            for c in cl:
                latest[c[2]] = c
            cl = sorted(latest.values(), key=click_key)
        tss = [c[0] for c in cl]
        if "click_seconds" in mut:
            tss = [t // 1000 for t in tss]

        for s in sessions:
            start = s[0]
            day = (s[6] if "day_of_last_event" in mut else start) // DAY
            if stats is not None:
                st["sessions"] += 1
                if s[6] // DAY != start // DAY:
                    st["sessions_crossing_midnight"] += 1
                st["max_session_ms"] = max(st["max_session_ms"], s[6] - start)
                st["max_session_events"] = max(st["max_session_events"], s[1])
                if len(s[5]) > 1:
                    st["sessions_using_several_ids"] += 1
            # ---- user_daily
            if "day_per_event" in mut:
                u = user_daily.setdefault((p, day), [0, 0, 0, {}, {}])
                u[0] += 1
                n_purchase_or_listed = 0
                for ts, price, pid in s[4]:
                    ue = user_daily.setdefault((p, ts // DAY), [0, 0, 0, {}, {}])
                    ue[1] += 1
                    n_purchase_or_listed += 1
                    if pid is not None:
                        ue[2] += price
                        cat = products.get(pid, UNKNOWN)
                        ue[3][cat] = ue[3].get(cat, 0) + price
                        ue[4][cat] = ue[4].get(cat, 0) + 1
                assert n_purchase_or_listed == s[1]
            else:
                u = user_daily.setdefault((p, day), [0, 0, 0, {}, {}])
                u[0] += 1
                u[1] += s[1]
                u[2] += s[2]
                for ts, price, pid in s[4]:
                    cat = products.get(pid, UNKNOWN)
                    if cat == UNKNOWN and "unknown_never_top" in mut:
                        continue
                    u[3][cat] = u[3].get(cat, 0) + price
                    u[4][cat] = u[4].get(cat, 0) + 1
            # ---- credit
            if "click_seconds" in mut:
                s_cmp, reach = start // 1000, REACH // 1000
            else:
                s_cmp, reach = start, REACH
            if "credit_window_from_first_event_day" in mut:
                low_edge = (start // DAY) * DAY - REACH
            else:
                low_edge = s_cmp - reach
            if "start_exclusive" in mut:
                hi = bisect.bisect_left(tss, s_cmp)
            elif "start_plus_1ms" in mut:
                hi = bisect.bisect_right(tss, s_cmp + 1)
            else:
                hi = bisect.bisect_right(tss, s_cmp)
            if "reach_exclusive" in mut:
                lo = bisect.bisect_right(tss, low_edge)
            elif "reach_plus_1ms" in mut:
                lo = bisect.bisect_left(tss, low_edge - 1)
            else:
                lo = bisect.bisect_left(tss, low_edge)
            credit = ORGANIC
            assists = ()
            if lo < hi:
                if "earliest_click" in mut:
                    chosen = cl[lo]
                else:
                    tmax = tss[hi - 1]
                    first = bisect.bisect_left(tss, tmax, lo, hi)
                    if "tie_largest_id" in mut or "tie_last_read" in mut:
                        chosen = cl[hi - 1]
                    else:
                        chosen = cl[first]
                    if stats is not None and hi - first > 1:
                        tied = cl[first:hi]
                        st["credit_ts_ties"] += 1
                        if len({c[1] for c in tied}) > 1:
                            st["credit_ts_ties_distinct_click_ids"] += 1
                        if len({c[2] for c in tied}) > 1:
                            st["credit_ts_ties_distinct_campaigns"] += 1
                            if cl[hi - 1][2] != chosen[2]:
                                st["credit_tie_where_largest_id_differs"] += 1
                            if len({len(c[1]) for c in tied}) > 1:
                                st["credit_tie_ids_of_different_length"] += 1
                            if any(c[1][:1].isupper() for c in tied):
                                st["credit_tie_with_uppercase_id"] += 1
                        st["max_credit_tie_size"] = max(st["max_credit_tie_size"], hi - first)
                credit = chosen[2] if chosen[2] is not None else ORGANIC
                if "assist_reach_exclusive" in mut:
                    alo = bisect.bisect_right(tss, low_edge)
                else:
                    alo = lo
                if "assist_counts_clicks" in mut:
                    assists = [c[2] for c in cl[alo:hi] if c[2] is not None]
                else:
                    assists = {c[2] for c in cl[alo:hi] if c[2] is not None}
                if "assist_excludes_credit" in mut:
                    assists = [a for a in assists if a != credit]
                if stats is not None:
                    st["credited_sessions"] += 1
                    st["max_qualifying_clicks"] = max(st["max_qualifying_clicks"], hi - lo)
                    if len(assists) > 1:
                        st["sessions_with_several_assisting_campaigns"] += 1
                    st["max_assisting_campaigns"] = max(st["max_assisting_campaigns"], len(assists))
                    if tss[hi - 1] == start:
                        st["credit_click_exactly_at_start"] += 1
                    if tss[lo] == start - REACH:
                        st["qualifying_click_exactly_7d_before"] += 1
                    if chosen[0] == start - REACH:
                        st["credit_click_exactly_7d_before"] += 1
                    if chosen[0] < ws:
                        st["credit_click_before_window_start"] += 1
                    if chosen[4] not in s[5]:
                        st["credit_click_from_other_id_than_session_events"] += 1
                    if hi < len(tss) and tss[hi] == start + 1:
                        st["click_1ms_after_start"] += 1
                    if lo > 0 and tss[lo - 1] == start - REACH - 1:
                        st["click_7d_plus_1ms_before"] += 1
            elif stats is not None:
                if hi < len(tss) and tss[hi] == start + 1:
                    st["organic_with_click_1ms_after_start"] += 1
                if lo > 0 and tss[lo - 1] == start - REACH - 1:
                    st["organic_with_click_7d_plus_1ms_before"] += 1
            if "clicks_by_raw_user" in mut and lo < hi:
                # redo credit and assists with only the clicks whose own ID is used in the session
                own = [c for c in cl[lo:hi] if c[4] in s[5]]
                if own:
                    tmax = max(c[0] for c in own)
                    credit = min(c for c in own if c[0] == tmax)[2]
                    assists = {c[2] for c in own}
                else:
                    credit, assists = ORGANIC, ()
            c = campaign_daily.setdefault((credit, day), [0, 0, 0, 0])
            c[0] += 1
            c[1] += s[3]
            c[2] += s[2]
            if credit == ORGANIC and "organic_assists" in mut:
                c[3] += 1
            for a in assists:
                aday = day
                if "assist_day_of_click" in mut:
                    aday = max(c2[0] for c2 in cl[lo:hi] if c2[2] == a) // DAY
                campaign_daily.setdefault((a, aday), [0, 0, 0, 0])[3] += 1

    if "name_from_event_ids_only" in mut:
        smallest_used = {}
        for group, evs in by_person.items():
            smallest_used[group] = min(x[4] for x in evs)
        user_daily = {(smallest_used[p], day): v for (p, day), v in user_daily.items()}

    # ------------------------------------------------------------------ output rows
    cat_index = None
    if "unknown_sorts_last" in mut or "unknown_sorts_first" in mut:
        names = sorted(set(products.values()))
        names = names + [UNKNOWN] if "unknown_sorts_last" in mut else [UNKNOWN] + names
        cat_index = {n: i for i, n in enumerate(names)}
    urows = set()
    for (p, day), (n, ev, rev, cats, counts) in user_daily.items():
        top = None
        measure = counts if "top_by_count" in mut else cats
        if measure:
            best = max(measure.values())
            tied = [c for c, v in measure.items() if v == best]
            if stats is not None and len(tied) > 1:
                st["top_category_ties"] += 1
                if UNKNOWN in tied:
                    st["top_category_ties_with_unknown"] += 1
                    if min(tied) != UNKNOWN and max(tied) != UNKNOWN:
                        st["top_category_ties_unknown_in_the_middle"] += 1
                    if b"video_games" in tied:
                        st["top_category_ties_unknown_vs_video_games"] += 1
            if "top_tie_largest" in mut:
                top = max(tied)
            elif cat_index is not None:
                top = min(tied, key=cat_index.get)
            else:
                top = min(tied)
        if n == 0 and "day_per_event" in mut:
            # a day that only received spill-over events: such a pipeline would still write a row
            pass
        urows.add((p, day, n, ev, rev, top))
        if stats is not None:
            st["max_events_per_person_day"] = max(st["max_events_per_person_day"], ev)
            st["max_sessions_per_person_day"] = max(st["max_sessions_per_person_day"], n)
            st["max_revenue_per_person_day"] = max(st["max_revenue_per_person_day"], rev)
            if top == UNKNOWN:
                st["top_category_unknown"] += 1
    crows_out = set()
    for (camp, day), (n, pn, rev, assisted) in campaign_daily.items():
        if n == 0 and "no_assist_only_rows" in mut:
            continue
        channel = ORGANIC if camp == ORGANIC else campaigns[camp]
        crows_out.add((camp, channel, day, n, pn, rev, assisted))
        if stats is not None:
            st["max_revenue_per_campaign_day"] = max(st["max_revenue_per_campaign_day"], rev)
            if n == 0:
                st["campaign_rows_assist_only"] += 1
    return urows, crows_out


# ----------------------------------------------------------------------------- helpers


def fmt_day(day):
    y, m, d = civil_from_days(day)
    return f"{y:04d}-{m:02d}-{d:02d}"


def write_csv(urows, crows, out):
    os.makedirs(out, exist_ok=True)
    with open(os.path.join(out, "user_daily.csv"), "wb") as handle:
        handle.write(b"user_id,day,sessions,events,revenue_cents,top_category\n")
        for p, day, n, ev, rev, top in sorted(urows, key=lambda r: (r[0], r[1])):
            handle.write(b"%s,%s,%d,%d,%d,%s\n" % (p, fmt_day(day).encode(), n, ev, rev, top or b""))
    with open(os.path.join(out, "campaign_daily.csv"), "wb") as handle:
        handle.write(b"campaign_id,channel,day,sessions,purchasing_sessions,revenue_cents,assisted_sessions\n")
        for camp, channel, day, n, pn, rev, assisted in sorted(crows, key=lambda r: (r[0], r[2])):
            handle.write(b"%s,%s,%s,%d,%d,%d,%d\n" % (camp, channel, fmt_day(day).encode(), n, pn, rev, assisted))


def read_expected(folder):
    def rows(name, ints, nullable):
        out = set()
        with open(os.path.join(folder, name), "rb") as handle:
            lines = handle.read().split(b"\n")
        header = lines[0].split(b",")
        for line in lines[1:]:
            if not line:
                continue
            f = line.split(b",")
            row = []
            for column, value in zip(header, f):
                column = column.decode()
                if column == "day":
                    y, m, d = (int(x) for x in value.split(b"-"))
                    row.append(days_from_civil(y, m, d))
                elif column in ints:
                    row.append(int(value))
                elif column in nullable and value == b"":
                    row.append(None)
                else:
                    row.append(value)
            out.add(tuple(row))
        return out
    u = rows("user_daily.csv", {"sessions", "events", "revenue_cents"}, {"top_category"})
    c = rows("campaign_daily.csv", {"sessions", "purchasing_sessions", "revenue_cents", "assisted_sessions"}, set())
    return u, c


def fixture_cases(fx):
    return sorted((d for d in os.listdir(fx) if os.path.isdir(os.path.join(fx, d))),
                  key=lambda c: (c[0], int(c[1:])))


def cmd_fixtures(fx):
    bad = 0
    for case in fixture_cases(fx):
        D = load(os.path.join(fx, case, "input"))
        got = pipeline(D)
        want = read_expected(os.path.join(fx, case, "expected"))
        ok = got == want
        bad += not ok
        print(("ok   " if ok else "FAIL ") + case)
        if not ok:
            print("   user only got ", sorted(got[0] - want[0], key=repr))
            print("   user only want", sorted(want[0] - got[0], key=repr))
            print("   camp only got ", sorted(got[1] - want[1], key=repr))
            print("   camp only want", sorted(want[1] - got[1], key=repr))
    print("fixtures failing:", bad)
    return bad


def cmd_mutants(fx, datasets, only=None):
    cases = fixture_cases(fx)
    loaded_fx = {case: load(os.path.join(fx, case, "input")) for case in cases}
    expected_fx = {case: read_expected(os.path.join(fx, case, "expected")) for case in cases}
    names = [m for m in MUTANTS if only is None or m in only]
    table = {m: {} for m in names}
    for m in names:
        failing = [case for case in cases if pipeline(loaded_fx[case], frozenset([m])) != expected_fx[case]]
        table[m]["fixtures"] = failing
    for data_dir in datasets:
        D = load(data_dir)
        base = pipeline(D)
        label = os.path.basename(os.path.normpath(data_dir))
        print(f"# {label}: base rows user_daily={len(base[0])} campaign_daily={len(base[1])}", flush=True)
        for m in names:
            got = pipeline(D, frozenset([m]))
            du = len(got[0] ^ base[0])
            dc = len(got[1] ^ base[1])
            table[m][label] = (du, dc)
            print(f"{label}\t{m}\tuser_rows_diff={du}\tcampaign_rows_diff={dc}\tfixtures_failing={','.join(table[m]['fixtures']) or '-'}", flush=True)
        del D
    return table


def cmd_stats(data_dir):
    D = load(data_dir)
    st = defaultdict(int)
    ws, we = D["ws"], D["we"]
    st["window_start_ms_mod_day"] = ws % DAY
    st["window_days_x1000"] = (we - ws) * 1000 // DAY
    # ---- raw rows
    st["event_rows_wellformed"] = len(D["events"])
    st["event_rows_malformed"] = len(D["events_bad"])
    st["click_rows_wellformed"] = len(D["clicks"])
    st["click_rows_malformed"] = len(D["clicks_bad"])
    st["link_rows_wellformed"] = len(D["links"])
    st["link_rows_malformed"] = len(D["links_bad"])
    st["event_rows_empty_event_id"] = sum(1 for e in D["events"] if e[0] == b"")
    st["click_rows_empty_click_id"] = sum(1 for c in D["clicks"] if c[0] == b"")
    st["event_rows_bad_ingest"] = sum(1 for e in D["events"] if e[3] is None)
    st["event_rows_uppercase_event_id"] = sum(1 for e in D["events"] if e[0][:1].isupper())
    st["event_rows_uppercase_user_id"] = sum(1 for e in D["events"] if e[1][:1].isupper())
    st["click_rows_uppercase_click_id"] = sum(1 for c in D["clicks"] if c[0][:1].isupper())
    st["click_rows_uppercase_user_id"] = sum(1 for c in D["clicks"] if c[1][:1].isupper())
    st["malformed_event_lines_lone_quote_last_field"] = sum(1 for l in D["events_bad"] if l.endswith(b',"'))
    st["malformed_lines_starting_with_quote_then_unquoted"] = sum(
        1 for l in D["events_bad"] + D["clicks_bad"] + D["links_bad"] if l[:1] == b'"' and l[1:2] != b'"' and b'"' not in l.split(b",")[0][1:])
    st["empty_lines"] = sum(1 for l in D["events_bad"] + D["clicks_bad"] + D["links_bad"] if l == b"")
    st["header_like_data_rows"] = (sum(1 for l in D["links"] if l == (b"user_id_a", b"user_id_b")))

    # ---- duplicates
    copies = defaultdict(list)
    for e in D["events"]:
        if e[0] != b"":
            copies[e[0]].append(e)
    bad_ids = defaultdict(int)
    for line in D["events_bad"]:
        f = [x.strip(b'"') for x in line.split(b",")]
        if f and f[0]:
            bad_ids[f[0]] += 1

    def valid(e):
        eid, uid, ts, ing, typ, pid, price, mask, raw = e
        if uid == b"" or ts is None or not ws <= ts < we or typ not in (b"view", b"cart", b"purchase") or pid == b"":
            return False
        if typ == b"purchase" and (not price.isdigit() or not 1 <= int(price) <= 10_000_000):
            return False
        return True

    for eid, cs in copies.items():
        if eid in bad_ids:
            st["event_ids_with_a_malformed_copy_and_a_wellformed_copy"] += 1
            if len(cs) >= 1:
                # was the malformed copy possibly the earliest? unknowable in general; count anyway
                pass
        if len(cs) < 2:
            continue
        st["event_ids_with_copies"] += 1
        cs.sort(key=lambda e: e[3])
        k = cs[0]
        if cs[1][3] == k[3]:
            st["dup_min_ingest_tie"] += 1
            if any(c[:7] != k[:7] for c in cs if c[3] == k[3]):
                st["dup_min_ingest_tie_values_differ"] += 1
        if len({c[3] for c in cs}) < len(cs):
            st["dup_any_ingest_tie"] += 1
        if len({c[1] for c in cs}) > 1:
            st["dup_copies_name_different_users"] += 1
            if k[1] == b"":
                st["dup_kept_copy_empty_user_later_copy_names_a_user"] += 1
            if k[1][:1].isupper():
                st["dup_kept_copy_uppercase_user_later_copy_other_user"] += 1
        if len({c[2] for c in cs}) > 1:
            st["dup_copies_differ_in_ts"] += 1
        if len({(c[4], c[5], c[6]) for c in cs}) > 1:
            st["dup_copies_differ_in_type_product_price"] += 1
        kv = valid(k) and not (k[3] - k[2] > DAY)
        later_ok = [c for c in cs[1:] if valid(c) and not (c[3] - c[2] > DAY)]
        if not kv and later_ok:
            st["dup_kept_invalid_but_later_copy_valid (rescue trap)"] += 1
            if not valid(k):
                if k[1] == b"":
                    st["  rescue trap: kept copy has empty user"] += 1
                elif k[2] is None or not ws <= k[2] < we:
                    st["  rescue trap: kept copy has bad ts"] += 1
                else:
                    st["  rescue trap: kept copy otherwise invalid"] += 1
            else:
                st["  rescue trap: kept copy late"] += 1
            if any(c[1] != k[1] for c in later_ok):
                st["  rescue trap where the rescuing copy names another user"] += 1
        if kv and any(not valid(c) for c in cs[1:]):
            st["dup_kept_valid_but_later_copy_invalid"] += 1
        if valid(k) and not kv and any(valid(c) for c in cs[1:]):
            st["dup_kept_late_and_copies_too"] += 1
        if valid(k) and k[3] - k[2] <= DAY and any(c[3] - c[2] > DAY for c in cs[1:] if c[2] is not None):
            st["dup_kept_on_time_but_a_later_copy_is_late"] += 1

    # ---- late boundaries
    for eid, cs in copies.items():
        k = min(cs, key=lambda e: e[3])
        if not valid(k):
            continue
        d = k[3] - k[2]
        if d == DAY:
            st["late_exactly_24h (kept)"] += 1
        elif d == DAY + 1000:
            st["late_24h_plus_1s (dropped)"] += 1
        elif DAY < d < DAY + 1000:
            st["late_between_24h_and_24h1s (dropped)"] += 1
        elif d > DAY:
            st["late_other (dropped)"] += 1
        elif d < 0:
            st["ingest_before_ts (kept)"] += 1
        if k[2] % 1000:
            st["valid_events_with_millisecond_ts"] += 1

    # ---- links
    parent = {}

    def find(x):
        root = x
        while parent[root] != root:
            root = parent[root]
        while parent[x] != root:
            parent[x], x = root, parent[x]
        return root

    for a, b in D["links"]:
        if a == b"" or b == b"":
            st["link_rows_empty_side"] += 1
            continue
        if a == b:
            st["link_rows_self"] += 1
        if a[:1].isupper() or b[:1].isupper():
            st["link_rows_with_uppercase_side"] += 1
        parent.setdefault(a, a)
        parent.setdefault(b, b)
        ra, rb = find(a), find(b)
        if ra != rb:
            parent[ra] = rb
    members = defaultdict(list)
    for x in parent:
        members[find(x)].append(x)
    event_users = {e[1] for e in D["events"]}
    click_users = {c[1] for c in D["clicks"]}
    sizes = sorted((len(v) for v in members.values()), reverse=True)
    st["link_components"] = len(members)
    st["link_component_sizes_top10"] = sizes[:10]
    for root, ids in members.items():
        smallest = min(ids)
        if smallest not in event_users and any(x in event_users for x in ids):
            st["persons_named_by_id_absent_from_events"] += 1
            if smallest not in click_users:
                st["persons_named_by_id_absent_from_events_and_clicks"] += 1
        if smallest[:1].isupper():
            st["linked_persons_named_by_uppercase_id"] += 1
        if min(ids, key=lambda x: (len(x), x)) != smallest:
            st["persons_where_len_order_name_differs"] += 1
        used = [x for x in ids if x in event_users]
        if used and min(used) != smallest:
            st["persons_where_event_id_min_differs_from_name"] += 1
        only_click = [x for x in ids if x in click_users and x not in event_users]
        if only_click and used:
            st["persons_with_click_only_ids"] += 1
        if any(x not in event_users and x not in click_users for x in ids):
            st["persons_with_link_only_ids"] += 1
    st["event_users_unlinked"] = sum(1 for u in event_users if u not in parent)

    # ---- base run with counters
    pipeline(D, frozenset(), st)
    for k in sorted(st):
        print(f"{k}\t{st[k]}")


def main():
    cmd = sys.argv[1]
    if cmd == "run":
        D = load(sys.argv[2])
        write_csv(*pipeline(D), sys.argv[3])
    elif cmd == "fixtures":
        sys.exit(1 if cmd_fixtures(sys.argv[2]) else 0)
    elif cmd == "mutants":
        only = None
        args = sys.argv[2:]
        if "--only" in args:
            at = args.index("--only")
            only = set(args[at + 1].split(","))
            args = args[:at] + args[at + 2:]
        cmd_mutants(args[0], args[1:], only)
    elif cmd == "stats":
        cmd_stats(sys.argv[2])
    else:
        sys.exit(__doc__)


if __name__ == "__main__":
    main()
