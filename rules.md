# Shopping Sessions: the rules

## The rules

### Input files

Mounted read-only at `/data`. Every CSV file has a header row.

| Path                                        | Columns                                                                                                       |
| ------------------------------------------- | ------------------------------------------------------------------------------------------------------------- |
| `events/*.csv`                              | `event_id,user_id,ts,ingest_ts,event_type,product_id,price_cents`                                             |
| `clicks/*.csv`                              | `click_id,user_id,campaign_id,ts`                                                                             |
| `links/*.csv`                               | `user_id_a,user_id_b`: two IDs of one person. See [Linked IDs](#linked-ids)                                   |
| `products.csv`                              | `product_id,category`, clean, `product_id` unique                                                             |
| `campaigns.csv`                             | `campaign_id,channel`, clean, `campaign_id` unique, IDs like `c_123`                                          |
| `manifest.json`                             | `{"window_start": "2026-10-01T00:00:00Z", "window_end": "2026-10-15T00:00:00Z"}`, different for every dataset |

Read every `*.csv` file in `events/`, `clicks/` and `links/`. How many there are changes with the dataset, and so does their size: a few large files, many small ones, and some that hold only the header. The fixtures have one or two each.

### CSV dialect

Comma-separated, UTF-8, `\n` line endings. Only `\n` ends a line: a `\r` is an ordinary character of the field it is in, so a line ending `\r\n` has a `\r` at the end of its last field (a purchase priced `1000\r` has a bad price, a click whose `ts` is `...Z\r` has a bad `ts`). The generator never writes a `\r`. Any field may be wrapped in double quotes, and the quotes are syntax, not part of the value: `"u1"` is `u1`, and `""` is an empty field. No value ever holds a comma, a quote or a newline, quoted or not: every comma separates fields. An empty field means null. A line with the wrong number of fields, or with a quote anywhere except around a whole field, is malformed and is dropped. An empty line is malformed too. Spaces are part of a value, quoted or not: ` u1` and `" u1"` are one user, `u1 ` is another, and neither is `u1`. Wherever these rules say digits, they mean the ASCII characters `0` to `9`; `٢٥٠` and `２５０` are not digits.

### Timestamps

`ts` in events and clicks takes three forms. Convert each to integer milliseconds since the Unix epoch, UTC.

| Form | Example                     | Meaning                                                                                                                 |
| ---- | --------------------------- | ----------------------------------------------------------------------------------------------------------------------- |
| F1   | `2026-10-03T23:58:10Z`      | UTC, whole seconds                                                                                                      |
| F2   | `2026-10-03T19:58:10-04:00` | Offset `+HH:MM` or `-HH:MM` (hours 00 to 23, minutes 00 to 59); subtract it to get UTC. This one equals the F1 example. |
| F3   | `1791071890000`             | Digits only: epoch milliseconds. Leading zeros are allowed (`0001791071890000` is the same instant)                    |

Anything else is a bad timestamp: empty, fractional seconds, a lowercase `z` or `t`, a space instead of `T`, no zone, any non-digit in F3 (`1791071890000.0`, `1.79e12`, `-5`), or an impossible calendar value (month 13, February 30, February 29 outside a leap year, hour 24, second 60), and an offset outside its range (`+24:00`, `+05:60`) or without its colon (`+0200`). `-00:00` is UTC. An F3 value is just a number, so one that is very short (`86400000` is 1970-01-02) or too long for a 64-bit integer is not malformed, only outside the window. `ingest_ts` is always valid F1.

An event's `ts` must satisfy `window_start <= ts < window_end`. A click's `ts` must satisfy `window_start - 7 days <= ts < window_end`, because a click up to 7 days before the window can still credit a session inside it. A timestamp outside its window is bad. A click at exactly `window_start - 7 days` is in its window; one at `window_end` is not.

### Linked IDs

One person can use many user IDs (a cleared cookie, a second device, logging in), and `links/` says which IDs belong together. Every rule below that says "person" means this, not a single `user_id`.

1. **Parse.** Drop malformed lines (the CSV dialect above) and lines where either field is empty.
2. **Join.** Every remaining line joins its two IDs into one person, and joining is transitive: if `a,b` and `b,c` are lines, then `a`, `b` and `c` are one person, however many links apart and in whichever files and order the lines come. A line may repeat another, either way round, and may link an ID to itself; neither changes anything. IDs compare exactly, as everywhere: `U1` is linked only to what a line links `U1` to.
3. **Name each person** by the byte-wise smallest of their IDs. An ID that appears in no remaining line is a person on its own, named by itself. An ID can be linked without appearing in any event or click, and can still be the smallest, and so the name.

Links come before everything else: an event or click is the person's as soon as it is parsed, whatever its own ID. Links have no time; they hold for the whole window and for the 7 days before it.

### Events, in this order

1. **Parse.** Drop malformed lines and lines whose `event_id` is empty.
2. **Dedup on `event_id`.** Keep only the copy with the smallest `ingest_ts`. Copies with equal `ingest_ts` are identical. Dedup comes before validation: if the kept copy is invalid the event is dropped, and a later valid copy does not rescue it. A malformed copy was already dropped in step 1, so it is never the kept copy.
3. **Validate the kept copy.** Drop the event unless `user_id` is non-empty, `ts` is valid and in the event window, `event_type` is exactly `view`, `cart` or `purchase` (case-sensitive), and `product_id` is non-empty. A `purchase` also needs `price_cents` to be digits only with a value from 1 to 10,000,000 (`0250` is 250). For `view` and `cart`, `price_cents` is ignored whatever it holds.
4. **Drop late events:** drop the event if `ingest_ts - ts` is over 24 hours. Exactly 24 hours is kept, and so is an `ingest_ts` before `ts`.
5. **Sessionize** each person's surviving events, all of their IDs together, sorted by `ts`. A new session starts when the gap from the previous event is over 30 minutes (1,800,000 ms). Equal timestamps share a session: the gap is measured between distinct timestamps, so when several events share a `ts` they all join the session the first of them joins, in whatever order they were read. A session starts at its first event's `ts`.
6. **One day per session:** the UTC date of its start. All its events and purchases count on that day, even those after midnight.
7. **Categories** come from `products.csv`. A product not in the file has the category `unknown`, a name no category in the file uses.

### Clicks

1. **Parse.** Drop malformed lines and lines whose `click_id` is empty.
2. **Dedup on `click_id`.** Copies of a click have identical values, so keep any one.
3. **Validate.** Drop the click unless `user_id` is non-empty, `ts` is valid and in the click window, and `campaign_id` is in `campaigns.csv`.
4. **Credit each session to one click.** Among the same person's valid clicks, from any of their IDs, with `click.ts <= session_start` and `session_start - click.ts <= 7 days` (604,800,000 ms), take the one with the latest `ts`; on a tie in `ts`, the smallest `click_id`, compared byte by byte. A click at exactly the session start counts; one even 1 ms after it does not.
5. **No qualifying click** credits the session to the campaign `organic` with the channel `organic`. That ID never appears in `campaigns.csv`.
6. One click can credit many sessions. Clicks never create, split or merge sessions.
7. **Assists.** A session is assisted by every campaign with at least one of the person's valid clicks with `session_start - 7 days <= click.ts <= session_start`: the same clicks step 4 chooses from, including the one it chose. Each such campaign assists the session once, however many of its clicks qualify. `organic` never assists.

Strings are never normalized: IDs compare exactly, case-sensitive, untrimmed. `u1` and `U1` are two IDs, and two people unless a link joins them.

### Outputs

Two tables, each one or more `*.parquet` files directly in its folder. Other files, symlinks, and files whose names start with `.` or `_` (`_SUCCESS`, `.part-0.parquet.crc`) are ignored. Row order and file count do not matter. Exactly these columns, in any order, no extras. Any integer width and any Parquet string encoding is accepted; `day` must be a Parquet `DATE`.

`/output/user_daily/`: one row per person and day with at least one session.

| Column          | Type             | Definition                                                                                                                                                                         |
| --------------- | ---------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `user_id`       | string           | The person's name: the byte-wise smallest of their IDs (see [Linked IDs](#linked-ids))                                                                                            |
| `day`           | DATE             | UTC date of the sessions' start                                                                                                                                                    |
| `sessions`      | integer          | Sessions starting that day                                                                                                                                                         |
| `events`        | integer          | Surviving events in those sessions, all types                                                                                                                                      |
| `revenue_cents` | integer          | Sum of `price_cents` over purchases in those sessions; 0 if none                                                                                                                   |
| `top_category`  | string, nullable | The category with the highest summed purchase `price_cents` in those sessions; a tie goes to the byte-wise smallest; null with no purchases. `unknown` competes like any category. |

`/output/campaign_daily/`: one row per `(campaign_id, day)` with at least one session credited to or assisted by the campaign. A campaign that only assisted that day has a row with 0 in the three credited columns.

| Column                | Type    | Definition                                                     |
| --------------------- | ------- | -------------------------------------------------------------- |
| `campaign_id`         | string  | The campaign, or `organic`                                     |
| `channel`             | string  | From `campaigns.csv`; `organic` for `organic`                  |
| `day`                 | DATE    | UTC date of the sessions' start                                |
| `sessions`            | integer | Sessions credited to this campaign that started that day       |
| `purchasing_sessions` | integer | Of those, sessions with at least one purchase                  |
| `revenue_cents`       | integer | Sum of purchase `price_cents` in those sessions                |
| `assisted_sessions`   | integer | Sessions starting that day that this campaign assisted; 0 for `organic` |

A quick self-check: summed over all rows, `sessions` and `revenue_cents` are equal in the two tables, and every session counted in `assisted_sessions` was credited to some campaign other than `organic`, so summed `assisted_sessions` is at least the credited, non-`organic` `sessions`.

## The generator

`generate --seed N --events E --clicks C --output DIR` writes every input file above. The whale users, whale campaigns, window, ID keys and dirt rates all change with the seed, so nothing about a dataset can be hard-coded. One in eight distinct `user_id`, `event_id` and `click_id` values carries a 32-character hexadecimal suffix (the rest use short lowercase base 36), and every ID passes through a per-seed keyed bijection, so pipelines must treat IDs as opaque variable-width strings rather than fixed-width or invertible generator numbers.

The clean data:

- About `E / 30` people with Zipf-like activity, and 5 whale people with 1 to 2% of all events each and many clicks.
- Each person has 1 to 8 IDs (45% one, 30% two, 13% three), whales 3 to 6. Half of a person's rows use their first ID and the rest any of them. Each ID is linked to an earlier one, so a person's links form a tree.
- 3 people, and 1 in 20,000 of the rest, have 50 to 400 IDs used evenly and linked one after the next in a random order: chains hundreds of links long.
- 0.4% of people also used one of 3 to 6 shared devices, whose ID links them all into one person: thousands of people, interleaved sessions, at full size.
- 5% of people have an extra ID linked but never used in a row.
- 50,000 products in 20 categories with Zipf popularity, priced 100 to 50,000 cents. About 3% of events are purchases.
- 2,000 campaigns in 5 channels. 3 to 5 whale campaigns take 30% of all clicks.
- Sessions seconds to minutes between events and hours apart, about 2% crossing midnight, and about 0.1% of gaps exactly 30:00.000 or 30:00.001. About 60% of sessions have a qualifying click, often several in the 7 days before.
- Boundary clicks: exactly 7 days and 7 days + 1 ms before a session start, exactly at it, 1 ms after it, and pairs at the same millisecond.
- A 14-day window starting at a random midnight from 2025 to 2028, so some windows hold February 29, 2028.
- Rows shuffled across the part files: 24 to 160 event files, 8 to 48 click files and 4 to 24 link files, drawn per seed, with Zipf sizes in a shuffled order (the largest holds about a fifth of the rows) and 1 to 3 holding only the header.

The dirt. A rate given as a range is drawn per seed from it; a single rate is the same for every seed:

| Dirt             | Rate                               | Details                                                                                                              |
| ---------------- | ---------------------------------- | -------------------------------------------------------------------------------------------------------------------- |
| Event duplicates | 2 to 4% of events                  | 1 to 3 extra copies, ingested 1 s to 10 min later. In 2% of duplicated events only the earliest copy has a bad `ts`. For about 1 in 64 duplicated events the later copies name a different user. |
| Click duplicates | 1 to 3% of clicks                  | Identical values, quoted differently                                                                                 |
| Timestamp forms  | F1 about 58%, F2 10%, F3 about 32% | Offsets from -10:00 to +12:00, some changing the date. A `ts` with milliseconds can only be F3.                      |
| Bad timestamps   | 0.3 to 0.7%                        | Every bad form above, including just outside the window                                                              |
| Late events      | 0.5 to 1.5%                        | Ingested 24 h to 5 days after `ts`, some at exactly 24 h and 24 h + 1 s                                              |
| Bad user         | 0.2% of events and clicks          | Empty `user_id`, plus about 1 in 200,000 rows with an upper-cased ID                                                 |
| Link dirt        | Per link                           | 8% repeated, either way round; 1% of people link an ID to itself; a side empty at the bad-user rate, which drops the line; 1 in 2,000 sides upper-cased: a different ID, which sorts before every lower-case one. Either can split a person in two |
| Bad type         | 0.1%                               | Unknown values and case variants (`Purchase`, `cart `)                                                               |
| Bad price        | 5% of purchases                    | Decimal, signed, zero, empty, over 10,000,000                                                                        |
| Unknown product  | 0.5%                               | Not in `products.csv`, including case variants; a tenth of these are empty                                           |
| Unknown campaign | 0.5% of clicks                     | Not in `campaigns.csv`, including case variants                                                                      |
| Quoted fields    | 20% of fields                      | Random fields wrapped in double quotes                                                                               |
| Malformed lines  | 0.05%, links too                   | Wrong field count, unbalanced quotes, an empty ID, empty lines                                                       |

