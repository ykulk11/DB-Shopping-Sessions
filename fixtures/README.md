# Fixtures

Each case is an `input/` folder in the layout of `/data` and an `expected/` folder with the two tables as CSV (`day` as `YYYY-MM-DD`, an empty `top_category` for null). The expected rows were written by hand from the rules, not produced by the reference. A correct pipeline matches every case.

All cases share a window of 2026-10-01 to 2026-10-15, the products `p1` (shoes) and `p2` (books), and the campaigns `c_1` (search) and `c_2` (social). `p9` is not in `products.csv`. One user, `u1`, and no links (`links/` holds only the header), unless noted.

## Events and sessions

| Case | Input | Catches |
| --- | --- | --- |
| E1 | Views at 10:00:00 and 10:30:00 | A gap of exactly 30 min stays one session |
| E2 | Views at F3 times 10:00:00.000 and 10:30:00.001 | 30 min + 1 ms splits; millisecond precision |
| E3 | A view Oct 3 23:50, a purchase of p1 for 1000 Oct 4 00:10 | A session crossing midnight belongs to its start day |
| E4 | `e1` twice, ingested 10:01 for u1 and 10:05 for u2 | Earliest copy wins across users |
| E5 | `e2` twice: ingested 10:01 with the bad ts `2026-10-03 10:00:00`, ingested 10:05 valid (listed first) | Dedup before validation |
| E6 | Two events at Oct 3 10:00:00, ingested Oct 4 10:00:00 and 10:00:01 | The late cutoff is strictly over 24 h |
| E7 | ts `2026-10-04T01:30:00+02:00` | The offset converts to Oct 3 23:30 UTC |
| E8 | One session: purchases of p1 for 500 and p2 for 500 | A tie goes to the byte-wise smallest category |
| E9 | Purchases of p9 for 2000 and p1 for 1500 | A missing product is `unknown`, which can win |
| E10 | A purchase priced `19.99`; a view priced `-5` | Price rules apply only to purchases and drop the whole event |
| E11 | Users `U1` and `u1`, one view each | No case folding |
| E12 | ts 2026-09-30T23:59:59Z, 2026-10-15T00:00:00Z, 2026-10-14T23:59:59Z | The event window includes its start and excludes its end |
| E13 | An empty event_id; a line with 6 fields; event_type `Purchase`; ts `1791071890000.0` | Malformed lines; strict type and case checks |
| E14 | Views at 09:00, 09:10, 14:00 Oct 3, then 08:00 Oct 4 | Several sessions a day, counted by start day |
| E15 | One view by `"u1"` (quoted), one by `u1` | Quotes are CSV syntax, not part of the value |
| E16 | `u1`: two views at 10:00:00.000, three events at 10:30:00.001 (one a purchase), one at 11:00:00.001, and a click at 10:30:00.001. `u2`: 10:00, two at 10:30:00.000, then 11:00:00.001 | Equal timestamps on either side of a gap share a session, whatever order they are read in |
| E17 | Offsets `+24:00`, `+05:60`, `+0200` (one user each, all dropped); `+23:59` and `-00:00` (kept) | The offset's hour is 00 to 23 and its minute 00 to 59, with a colon |
| E18 | F3 with leading zeros (kept); F3 `86400000`, in Arabic-Indic digits, and 21 digits long; a year with a full-width digit; purchases priced `２５０` and `٢٥٠` (all dropped); one priced `0250` (kept) | Digits are ASCII only; leading zeros are fine; an F3 value outside the window is bad, however short or long |
| E19 | Users `u1`, ` u1`, `u1 ` and `" u1"`; product ` p1`; event type ` view`; event IDs `e1` and `e1 `; a click by ` u1`, one for campaign ` c_1` | Spaces are part of a value, quoted or not |
| E20 | Lines ending `\r\n`: a view (kept), a purchase whose price becomes `1000\r` (dropped), a click whose `ts` becomes `...Z\r` (dropped); event IDs `e3` and `e3\r` | `\r` is an ordinary character, never a line ending |

## Attribution

One session starting Oct 3 10:00 with no purchase, unless noted.

| Case | Clicks | Catches |
| --- | --- | --- |
| A1 | `k1` for `c_1` at Oct 3 09:00 | Basic credit |
| A2 | `k1` for `c_1` at Sep 26 10:00:00.000, exactly 7 days before; `u2` has the same session and a click 7 days + 1 ms before | The 7-day limit is inclusive; a click before the event window still counts |
| A3 | `k1` for `c_1` at Oct 2 10:00, `k2` for `c_2` at Oct 3 08:00 | The latest click wins |
| A4 | `k1` for `c_1` at exactly 10:00:00; `u2` has a click 1 ms after their session start | A click at the start counts; after it does not |
| A5 | `k2` for `c_2` and `k1` for `c_1`, both at 09:00 | A tie goes to the smallest click_id |
| A6 | `k1` for `c_999`, not in `campaigns.csv` | An unknown campaign drops the click |
| A7 | `k1` by user `U1` | No case folding between clicks and sessions |
| A8 | `k1` for `c_1` at Oct 3 09:00; sessions Oct 3 10:00 (a purchase of p1 for 1000) and Oct 4 08:00 (a view) | One click credits many sessions |
| A9 | Events at 10:00, 10:25 and 11:30; `k1` for `c_1` at 10:20 | A click mid-session credits only later sessions |
| A10 | Sessions at the window's first instant (Oct 1 00:00) and its last second (Oct 14 23:59:59). Clicks at exactly Sep 24 00:00 (window start - 7 days, credits), 1 ms before that (dropped), at the window's end (dropped), and at the last session's start (credits) | The click window's edges: its start is inclusive, its end exclusive |
| A11 | `u1`'s session has a purchase. `k1` for `c_1` exactly 7 days before, `k2` and `k3` for `c_2` at 09:00 and 09:30, `k4` for `c_1` 1 ms after the start; `u2`'s same session has a click 7 days + 1 ms before | Every campaign clicked in the 7 days up to the start assists once, including the crediting one; a campaign with only assists gets a row with 0 credited |
| A12 | `u1`'s sessions Oct 3, 4, 9 at 09:00 and Oct 10 at 09:00:00.001. `k1` for `c_1` Oct 3 09:00; `k2` for `c_2` Oct 4 09:00 by `u2`, linked to `u1` | One click assists every session in the 7 days after it, on each session's own day; a linked ID's click assists |

## Linked IDs

One session per user at Oct 3 10:00, unless noted.

| Case | Links | Catches |
| --- | --- | --- |
| L1 | `u2,u1`. `u1` views at 10:00 and 10:50, `u2` buys p2 for 800 at 10:25; `u3` views at 10:00 | Linked IDs share sessions: the gaps are measured across both. The row takes the smallest ID |
| L2 | `u3,u9` and `u9,u2` in one file, `U7,u9` and `u10,u10` in another; events by `u3`, `u2` and `u10` only | Links chain, in either direction and across files; the person is named by the smallest ID even when no event uses it; a self-link changes nothing |
| L3 | `u1,u2,u3`, `u1,`, `,u3`, `u1,u5"`, an empty line and `u1` (all dropped); `"u4","u1"`, `u4,u1` and `u6,u6` (kept); one event each by `u1` to `u6` | Malformed links and links with an empty side are dropped; quoted, repeated and self-links are fine. A data row equal to the header joins `user_id_a` to `user_id_b`, making an ID with a tab the name of `u4` and `u1` |
| L4 | `u1,u2`. `k1` for `c_1` by `u2` at 09:00, `k2` for `c_2` by `u1` at 08:00 | A linked ID's click credits the session; the latest across all the person's IDs wins, and both assist |
