//! refimpl: a second, independent reference for the Shopping Sessions rules (rules.md).
//!
//! Usage: refimpl DATA_DIR OUT_DIR
//! Writes OUT_DIR/user_daily.csv and OUT_DIR/campaign_daily.csv, rows sorted by (id bytes, day).
//!
//! Single-threaded, std only, raw bytes throughout. Lines end only at b'\n', fields are split
//! only at b','. One file is read at a time and only compact records are kept, so that tens of
//! millions of events fit in a few GB.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process;

const SESSION_GAP: i64 = 30 * 60 * 1000; // a gap over this starts a new session
const DAY: i64 = 24 * 60 * 60 * 1000;
const WEEK: i64 = 7 * DAY;
const MAX_PRICE: u64 = 10_000_000;
const NONE: u32 = u32::MAX;

fn fail(message: &str) -> ! {
    eprintln!("refimpl: {message}");
    process::exit(1);
}

// ------------------------------------------------------------------------------------ interner

/// Gives every distinct byte string a dense u32 id. The bytes live in one arena and the lookup
/// table is open addressing with linear probing, which keeps tens of millions of IDs compact.
struct Interner {
    arena: Vec<u8>,
    ends: Vec<usize>, // string i is arena[ends[i - 1]..ends[i]], from 0 for the first
    table: Vec<u32>,  // 0 = empty slot, otherwise id + 1; the length is a power of two
}

impl Interner {
    fn new() -> Self {
        Interner { arena: Vec::new(), ends: Vec::new(), table: vec![0; 1024] }
    }

    fn len(&self) -> usize {
        self.ends.len()
    }

    fn get(&self, id: u32) -> &[u8] {
        let i = id as usize;
        let start = if i == 0 { 0 } else { self.ends[i - 1] };
        &self.arena[start..self.ends[i]]
    }

    /// FNV-1a, then a mixing step so that the low bits used for the slot depend on every byte.
    fn hash(s: &[u8]) -> usize {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for &b in s {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        h ^= h >> 32;
        h = h.wrapping_mul(0x9e37_79b9_7f4a_7c15);
        h ^= h >> 32;
        h as usize
    }

    /// The id of `s`, if it was interned.
    fn find(&self, s: &[u8]) -> Option<u32> {
        let mask = self.table.len() - 1;
        let mut slot = Self::hash(s) & mask;
        loop {
            let entry = self.table[slot];
            if entry == 0 {
                return None;
            }
            if self.get(entry - 1) == s {
                return Some(entry - 1);
            }
            slot = (slot + 1) & mask;
        }
    }

    /// The id of `s`, adding it when it is new; the flag says whether it was added.
    fn intern(&mut self, s: &[u8]) -> (u32, bool) {
        let mask = self.table.len() - 1;
        let mut slot = Self::hash(s) & mask;
        loop {
            let entry = self.table[slot];
            if entry == 0 {
                break;
            }
            if self.get(entry - 1) == s {
                return (entry - 1, false);
            }
            slot = (slot + 1) & mask;
        }
        let id = self.ends.len();
        if id >= (NONE - 1) as usize {
            fail("too many distinct IDs for a u32");
        }
        self.arena.extend_from_slice(s);
        self.ends.push(self.arena.len());
        self.table[slot] = id as u32 + 1;
        if self.ends.len() * 2 > self.table.len() {
            self.grow();
        }
        (id as u32, true)
    }

    fn grow(&mut self) {
        let mask = self.table.len() * 2 - 1;
        let mut table = vec![0u32; self.table.len() * 2];
        for id in 0..self.ends.len() as u32 {
            let mut slot = Self::hash(self.get(id)) & mask;
            while table[slot] != 0 {
                slot = (slot + 1) & mask;
            }
            table[slot] = id + 1;
        }
        self.table = table;
    }
}

// ----------------------------------------------------------------------------------------- CSV

/// The value of one raw field: quotes are syntax and may only wrap the whole field.
fn unquote(raw: &[u8]) -> Option<&[u8]> {
    if !raw.contains(&b'"') {
        return Some(raw);
    }
    let n = raw.len();
    if n < 2 || raw[0] != b'"' || raw[n - 1] != b'"' {
        return None;
    }
    let inner = &raw[1..n - 1];
    if inner.contains(&b'"') {
        return None;
    }
    Some(inner)
}

/// The N values of one line, or None when the line is malformed: empty, the wrong number of
/// fields, or a quote anywhere except around a whole field. Every comma separates fields.
fn split_line<const N: usize>(line: &[u8]) -> Option<[&[u8]; N]> {
    if line.is_empty() {
        return None;
    }
    let mut fields: [&[u8]; N] = [&[]; N];
    let mut count = 0;
    for raw in line.split(|&b| b == b',') {
        if count == N {
            return None;
        }
        fields[count] = unquote(raw)?;
        count += 1;
    }
    if count != N {
        return None;
    }
    Some(fields)
}

/// Every `*.csv` file directly in `dir`, in name order.
fn csv_files(dir: &Path) -> Vec<PathBuf> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) => {
            eprintln!("refimpl: warning: {}: {error}; treated as holding no files", dir.display());
            return Vec::new();
        }
    };
    let mut files = Vec::new();
    for entry in entries {
        let entry = entry.unwrap_or_else(|e| fail(&format!("{}: {e}", dir.display())));
        let name = entry.file_name();
        let name = name.as_encoded_bytes();
        if name.ends_with(b".csv") && !name.starts_with(b".") && entry.path().is_file() {
            files.push(entry.path());
        }
    }
    files.sort();
    files
}

/// Calls `row` for every line of the file after its header. Only b'\n' ends a line; a last
/// line without one still counts, and a `\r` stays in the line.
fn for_each_row(path: &Path, mut row: impl FnMut(&[u8])) {
    let file = File::open(path).unwrap_or_else(|e| fail(&format!("{}: {e}", path.display())));
    let mut reader = BufReader::with_capacity(1 << 20, file);
    let mut line: Vec<u8> = Vec::new();
    let mut header = true;
    loop {
        line.clear();
        let read = reader
            .read_until(b'\n', &mut line)
            .unwrap_or_else(|e| fail(&format!("{}: {e}", path.display())));
        if read == 0 {
            break;
        }
        if line.last() == Some(&b'\n') {
            line.pop();
        }
        if header {
            header = false; // the first line is the header, whatever it holds
            continue;
        }
        row(&line);
    }
}

// ---------------------------------------------------------------------------------- timestamps

fn two_digits(f: &[u8], at: usize) -> Option<i64> {
    let (a, b) = (f[at], f[at + 1]);
    if a.is_ascii_digit() && b.is_ascii_digit() {
        Some(i64::from(a - b'0') * 10 + i64::from(b - b'0'))
    } else {
        None
    }
}

fn is_leap(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ => {
            if is_leap(year) {
                29
            } else {
                28
            }
        }
    }
}

/// Days from 1970-01-01 to January 1 of `year` in the proleptic Gregorian calendar:
/// 365 a year plus one for every leap year before it; 0001-01-01 is day -719162.
fn days_to_year(year: i64) -> i64 {
    let before = year - 1;
    365 * before + before.div_euclid(4) - before.div_euclid(100) + before.div_euclid(400) - 719_162
}

fn days_to_date(year: i64, month: i64, day: i64) -> i64 {
    let mut days = days_to_year(year);
    for earlier in 1..month {
        days += days_in_month(year, earlier);
    }
    days + day - 1
}

/// (year, month, day) of a day number counted from 1970-01-01.
fn date_of_day(days: i64) -> (i64, i64, i64) {
    let mut year = 1970 + days.div_euclid(366);
    while days_to_year(year + 1) <= days {
        year += 1;
    }
    while days_to_year(year) > days {
        year -= 1;
    }
    let mut left = days - days_to_year(year);
    let mut month = 1;
    while left >= days_in_month(year, month) {
        left -= days_in_month(year, month);
        month += 1;
    }
    (year, month, left + 1)
}

/// Milliseconds since the epoch of a valid timestamp in form F1, F2 or F3; None for a bad one.
/// The window is not checked here. An F3 number too large for an i64 becomes i64::MAX, which
/// is outside every window.
fn parse_ts(f: &[u8]) -> Option<i64> {
    if f.is_empty() {
        return None;
    }
    if f.iter().all(|b| b.is_ascii_digit()) {
        let mut value: i64 = 0;
        for &b in f {
            value = value.saturating_mul(10).saturating_add(i64::from(b - b'0'));
        }
        return Some(value);
    }
    // 0123456789012345678901234
    // YYYY-MM-DDTHH:MM:SSZ        F1, 20 bytes
    // YYYY-MM-DDTHH:MM:SS+HH:MM   F2, 25 bytes
    if f.len() != 20 && f.len() != 25 {
        return None;
    }
    if f[4] != b'-' || f[7] != b'-' || f[10] != b'T' || f[13] != b':' || f[16] != b':' {
        return None;
    }
    let year = two_digits(f, 0)? * 100 + two_digits(f, 2)?;
    let month = two_digits(f, 5)?;
    let day = two_digits(f, 8)?;
    let hour = two_digits(f, 11)?;
    let minute = two_digits(f, 14)?;
    let second = two_digits(f, 17)?;
    if !(1..=12).contains(&month) || day < 1 || day > days_in_month(year, month) {
        return None;
    }
    if hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    let local_seconds = ((days_to_date(year, month, day) * 24 + hour) * 60 + minute) * 60 + second;
    let offset_seconds = if f.len() == 20 {
        if f[19] != b'Z' {
            return None;
        }
        0
    } else {
        let sign = match f[19] {
            b'+' => 1,
            b'-' => -1,
            _ => return None,
        };
        if f[22] != b':' {
            return None;
        }
        let offset_hours = two_digits(f, 20)?;
        let offset_minutes = two_digits(f, 23)?;
        if offset_hours > 23 || offset_minutes > 59 {
            return None;
        }
        sign * (offset_hours * 3600 + offset_minutes * 60)
    };
    Some((local_seconds - offset_seconds) * 1000)
}

/// A purchase's price: digits only, value 1 to 10,000,000. Leading zeros are fine.
fn parse_price(f: &[u8]) -> Option<u32> {
    if f.is_empty() || !f.iter().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let mut value: u64 = 0;
    for &b in f {
        value = value.saturating_mul(10).saturating_add(u64::from(b - b'0'));
    }
    if (1..=MAX_PRICE).contains(&value) {
        Some(value as u32)
    } else {
        None
    }
}

/// The string value of `"key": "value"` in manifest.json, parsed as a timestamp.
fn manifest_time(text: &[u8], key: &str) -> i64 {
    let quoted = format!("\"{key}\"");
    let quoted = quoted.as_bytes();
    let at = text
        .windows(quoted.len())
        .position(|w| w == quoted)
        .unwrap_or_else(|| fail(&format!("manifest.json: no {key}")));
    let mut rest = text[at + quoted.len()..].iter().copied().skip_while(|b| b.is_ascii_whitespace());
    if rest.next() != Some(b':') {
        fail(&format!("manifest.json: no ':' after {key}"));
    }
    let mut rest = rest.skip_while(|b| b.is_ascii_whitespace());
    if rest.next() != Some(b'"') {
        fail(&format!("manifest.json: {key} is not a string"));
    }
    let value: Vec<u8> = rest.take_while(|&b| b != b'"').collect();
    parse_ts(&value).unwrap_or_else(|| fail(&format!("manifest.json: bad {key}")))
}

// -------------------------------------------------------------------------------------- people

/// All user IDs seen in links, events and clicks, and which of them are one person.
struct People {
    ids: Interner,
    parent: Vec<u32>, // union-find forest over the IDs
    size: Vec<u32>,   // by root: the number of IDs in its tree
}

impl People {
    fn new() -> Self {
        People { ids: Interner::new(), parent: Vec::new(), size: Vec::new() }
    }

    /// The number of a user ID; a new one starts as a person on its own.
    fn id(&mut self, user_id: &[u8]) -> u32 {
        let (id, new) = self.ids.intern(user_id);
        if new {
            self.parent.push(id);
            self.size.push(1);
        }
        id
    }

    /// The root standing for the person `id` belongs to.
    fn person(&mut self, mut id: u32) -> u32 {
        while self.parent[id as usize] != id {
            let up = self.parent[id as usize];
            self.parent[id as usize] = self.parent[up as usize]; // path halving
            id = up;
        }
        id
    }

    fn join(&mut self, a: u32, b: u32) {
        let (a, b) = (self.person(a), self.person(b));
        if a == b {
            return;
        }
        let (big, small) = if self.size[a as usize] >= self.size[b as usize] { (a, b) } else { (b, a) };
        self.parent[small as usize] = big;
        self.size[big as usize] += self.size[small as usize];
    }
}

// ------------------------------------------------------------------------------------- records

/// The copy of an event with the smallest ingest_ts read so far. Only what the later steps
/// need; when `valid` is false the copy failed validation or is late, and the event is dropped.
#[derive(Clone, Copy)]
struct Kept {
    ingest: i64,
    ts: i64,
    user: u32,
    category: u32,
    price: u32, // 0 unless a purchase
    purchase: bool,
    valid: bool,
}

/// A surviving event.
#[derive(Clone, Copy)]
struct Event {
    person: u32,
    category: u32,
    ts: i64,
    price: u32, // 0 unless a purchase
    purchase: bool,
}

/// A valid click.
#[derive(Clone, Copy)]
struct Click {
    person: u32,
    campaign: u32,
    ts: i64,
    id: u32,
}

struct UserRow {
    name: u32,    // the user ID naming the person
    name_key: u64, // the first 8 bytes of that name, big-endian, zero padded
    day: i64,
    sessions: u64,
    events: u64,
    revenue: u64,
    top_category: u32, // NONE for null
}

#[derive(Default)]
struct CampaignDay {
    sessions: u64,
    purchasing: u64,
    revenue: u64,
    assisted: u64,
}

// ---------------------------------------------------------------------------------- the tables

/// Sessionizes one person at a time and adds them to both tables.
struct Tally {
    user_rows: Vec<UserRow>,
    campaign_days: HashMap<(u32, i64), CampaignDay>,
    organic: u32,
    // Scratch; everything is zero or empty between two people.
    in_window: Vec<u32>,    // by campaign: the person's clicks in the session's 7 days
    day_assists: Vec<u64>,  // by campaign: sessions assisted on the day being counted
    campaigns: Vec<u32>,    // the distinct campaigns the person clicked
    category_cents: Vec<u64>, // by category: purchase cents on the day being counted
    categories: Vec<u32>,   // the categories with a purchase on that day
}

impl Tally {
    /// `events`: all surviving events of one person, sorted by ts.
    /// `clicks`: all valid clicks of that person, sorted by ts and then by click_id descending,
    /// so that of the clicks up to an instant the last is the latest and, on a tie in ts, the
    /// one with the smallest click_id.
    fn person(&mut self, name: u32, name_key: u64, category_names: &Interner, events: &[Event], clicks: &[Click]) {
        self.campaigns.clear();
        self.campaigns.extend(clicks.iter().map(|c| c.campaign));
        self.campaigns.sort_unstable();
        self.campaigns.dedup();

        // clicks[lo..hi] are the clicks with start - 7 days <= ts <= start for the current
        // session; both ends only move forward because the starts grow.
        let (mut lo, mut hi) = (0, 0);
        let mut day = events[0].ts.div_euclid(DAY);
        let (mut sessions, mut event_count, mut revenue) = (0u64, 0u64, 0u64);

        let mut first = 0;
        while first < events.len() {
            // The session is events[first..end]: it grows while the gap is at most 30 minutes.
            let mut end = first + 1;
            while end < events.len() && events[end].ts - events[end - 1].ts <= SESSION_GAP {
                end += 1;
            }
            let start = events[first].ts;
            let start_day = start.div_euclid(DAY);
            if start_day != day {
                self.close_day(name, name_key, category_names, day, sessions, event_count, revenue);
                day = start_day;
                (sessions, event_count, revenue) = (0, 0, 0);
            }

            let mut session_revenue = 0u64;
            let mut purchased = false;
            for event in &events[first..end] {
                if event.purchase {
                    purchased = true;
                    session_revenue += u64::from(event.price);
                    let cents = &mut self.category_cents[event.category as usize];
                    if *cents == 0 {
                        self.categories.push(event.category);
                    }
                    *cents += u64::from(event.price);
                }
            }
            sessions += 1;
            event_count += (end - first) as u64;
            revenue += session_revenue;

            while hi < clicks.len() && clicks[hi].ts <= start {
                self.in_window[clicks[hi].campaign as usize] += 1;
                hi += 1;
            }
            while lo < hi && clicks[lo].ts < start - WEEK {
                self.in_window[clicks[lo].campaign as usize] -= 1;
                lo += 1;
            }
            let credited = if lo < hi { clicks[hi - 1].campaign } else { self.organic };
            let credit = self.campaign_days.entry((credited, start_day)).or_default();
            credit.sessions += 1;
            credit.purchasing += u64::from(purchased);
            credit.revenue += session_revenue;
            for &campaign in &self.campaigns {
                if self.in_window[campaign as usize] > 0 {
                    self.day_assists[campaign as usize] += 1;
                }
            }
            first = end;
        }
        self.close_day(name, name_key, category_names, day, sessions, event_count, revenue);
        for &campaign in &self.campaigns {
            self.in_window[campaign as usize] = 0;
        }
    }

    /// Writes down one person's day and clears the per-day scratch.
    #[allow(clippy::too_many_arguments)]
    fn close_day(&mut self, name: u32, name_key: u64, category_names: &Interner, day: i64, sessions: u64, events: u64, revenue: u64) {
        let mut top = NONE;
        for &category in &self.categories {
            let cents = self.category_cents[category as usize];
            let better = top == NONE
                || cents > self.category_cents[top as usize]
                || (cents == self.category_cents[top as usize]
                    && category_names.get(category) < category_names.get(top));
            if better {
                top = category;
            }
        }
        for &category in &self.categories {
            self.category_cents[category as usize] = 0;
        }
        self.categories.clear();
        self.user_rows.push(UserRow { name, name_key, day, sessions, events, revenue, top_category: top });

        for &campaign in &self.campaigns {
            let assisted = self.day_assists[campaign as usize];
            if assisted > 0 {
                self.campaign_days.entry((campaign, day)).or_default().assisted += assisted;
                self.day_assists[campaign as usize] = 0;
            }
        }
    }
}

/// The first 8 bytes of `s` as a big-endian number, zero padded. If two keys differ, the
/// strings compare byte-wise the same way as the keys; if they are equal, compare the strings.
fn prefix_key(s: &[u8]) -> u64 {
    let mut bytes = [0u8; 8];
    let n = s.len().min(8);
    bytes[..n].copy_from_slice(&s[..n]);
    u64::from_be_bytes(bytes)
}

fn write_day(out: &mut impl Write, day: i64) -> std::io::Result<()> {
    let (year, month, date) = date_of_day(day);
    write!(out, "{year:04}-{month:02}-{date:02}")
}

fn create(path: &Path) -> BufWriter<File> {
    BufWriter::with_capacity(1 << 20, File::create(path).unwrap_or_else(|e| fail(&format!("{}: {e}", path.display()))))
}

// ---------------------------------------------------------------------------------------- main

fn main() {
    let args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    if args.len() != 3 {
        eprintln!("usage: refimpl DATA_DIR OUT_DIR");
        process::exit(2);
    }
    let data = Path::new(&args[1]);
    let out_dir = Path::new(&args[2]);

    // The window.
    let manifest = fs::read(data.join("manifest.json")).unwrap_or_else(|e| fail(&format!("manifest.json: {e}")));
    let window_start = manifest_time(&manifest, "window_start");
    let window_end = manifest_time(&manifest, "window_end");

    // products.csv: product -> category. `unknown` is the category of every other product.
    let mut products = Interner::new();
    let mut product_category: Vec<u32> = Vec::new();
    let mut categories = Interner::new();
    for_each_row(&data.join("products.csv"), |line| {
        if let Some([product, category]) = split_line::<2>(line) {
            let (category, _) = categories.intern(category);
            let (product, new) = products.intern(product);
            if new {
                product_category.push(category);
            } else {
                product_category[product as usize] = category;
            }
        }
    });
    let (unknown_category, _) = categories.intern(b"unknown");

    // campaigns.csv: campaign -> channel. `organic` gets the number after the last real one
    // and is not in the lookup, so a click naming it is a click for an unknown campaign.
    let mut campaigns = Interner::new();
    let mut channels: Vec<Vec<u8>> = Vec::new();
    for_each_row(&data.join("campaigns.csv"), |line| {
        if let Some([campaign, channel]) = split_line::<2>(line) {
            let (campaign, new) = campaigns.intern(campaign);
            if new {
                channels.push(channel.to_vec());
            } else {
                channels[campaign as usize] = channel.to_vec();
            }
        }
    });
    let organic = campaigns.len() as u32;
    channels.push(b"organic".to_vec());

    // Links: drop malformed lines and lines with an empty side; every other line joins two IDs.
    let mut people = People::new();
    let (mut link_lines, mut links_kept) = (0u64, 0u64);
    for path in csv_files(&data.join("links")) {
        for_each_row(&path, |line| {
            link_lines += 1;
            let Some([a, b]) = split_line::<2>(line) else { return };
            if a.is_empty() || b.is_empty() {
                return;
            }
            links_kept += 1;
            let (a, b) = (people.id(a), people.id(b));
            people.join(a, b);
        });
    }
    eprintln!("links: {link_lines} lines, {links_kept} kept, {} IDs", people.ids.len());

    // Events: parse, then dedup on event_id keeping the copy with the smallest ingest_ts.
    // Whether a copy passes validation and is not late depends on that copy alone, so it is
    // worked out as the copy is read and stored with it; it only counts if the copy is kept.
    let mut event_ids = Interner::new();
    let mut kept: Vec<Kept> = Vec::new();
    let (mut event_lines, mut event_rows, mut bad_ingest) = (0u64, 0u64, 0u64);
    for path in csv_files(&data.join("events")) {
        for_each_row(&path, |line| {
            event_lines += 1;
            let Some([event_id, user, ts, ingest, kind, product, price]) = split_line::<7>(line) else { return };
            if event_id.is_empty() {
                return;
            }
            event_rows += 1;
            // The rules promise a valid ingest_ts. If one is not, the copy is invalid and
            // loses the dedup to every copy that has one.
            let ingest_ms = parse_ts(ingest);
            if ingest_ms.is_none() {
                bad_ingest += 1;
            }
            let mut copy = Kept {
                ingest: ingest_ms.unwrap_or(i64::MAX),
                ts: 0,
                user: 0,
                category: 0,
                price: 0,
                purchase: false,
                valid: false,
            };
            'validate: {
                let Some(ingest_ms) = ingest_ms else { break 'validate };
                if user.is_empty() || product.is_empty() {
                    break 'validate;
                }
                let Some(ts) = parse_ts(ts) else { break 'validate };
                if ts < window_start || ts >= window_end {
                    break 'validate;
                }
                match kind {
                    b"view" | b"cart" => {}
                    b"purchase" => {
                        let Some(cents) = parse_price(price) else { break 'validate };
                        copy.price = cents;
                        copy.purchase = true;
                    }
                    _ => break 'validate,
                }
                if ingest_ms.saturating_sub(ts) > DAY {
                    break 'validate; // late
                }
                copy.ts = ts;
                copy.user = people.id(user);
                copy.category = match products.find(product) {
                    Some(product) => product_category[product as usize],
                    None => unknown_category,
                };
                copy.valid = true;
            }
            let (id, new) = event_ids.intern(event_id);
            if new {
                kept.push(copy);
            } else if copy.ingest < kept[id as usize].ingest {
                kept[id as usize] = copy;
            }
        });
    }
    drop(event_ids);
    drop(products);
    if bad_ingest > 0 {
        eprintln!("refimpl: warning: {bad_ingest} event rows with an invalid ingest_ts");
    }

    // The surviving events, each with its person, sorted by person and then by ts.
    let mut events: Vec<Event> = Vec::new();
    for copy in &kept {
        if copy.valid {
            events.push(Event {
                person: people.person(copy.user),
                category: copy.category,
                ts: copy.ts,
                price: copy.price,
                purchase: copy.purchase,
            });
        }
    }
    eprintln!(
        "events: {event_lines} lines, {event_rows} parsed, {} distinct, {} surviving",
        kept.len(),
        events.len()
    );
    drop(kept);
    events.sort_unstable_by_key(|e| (e.person, e.ts));

    // Clicks: parse, dedup on click_id (copies are identical, the first read is kept), validate.
    let mut click_ids = Interner::new();
    let mut clicks: Vec<Click> = Vec::new();
    let (mut click_lines, mut click_rows) = (0u64, 0u64);
    for path in csv_files(&data.join("clicks")) {
        for_each_row(&path, |line| {
            click_lines += 1;
            let Some([click_id, user, campaign, ts]) = split_line::<4>(line) else { return };
            if click_id.is_empty() {
                return;
            }
            click_rows += 1;
            let (id, new) = click_ids.intern(click_id);
            if !new || user.is_empty() {
                return;
            }
            let Some(ts) = parse_ts(ts) else { return };
            if ts < window_start - WEEK || ts >= window_end {
                return;
            }
            let Some(campaign) = campaigns.find(campaign) else { return };
            let user = people.id(user);
            clicks.push(Click { person: people.person(user), campaign, ts, id });
        });
    }
    eprintln!(
        "clicks: {click_lines} lines, {click_rows} parsed, {} distinct, {} valid",
        click_ids.len(),
        clicks.len()
    );
    clicks.sort_unstable_by(|a, b| {
        (a.person, a.ts).cmp(&(b.person, b.ts)).then_with(|| click_ids.get(b.id).cmp(click_ids.get(a.id)))
    });
    drop(click_ids);

    // Name each person by the byte-wise smallest of their IDs.
    let id_count = people.ids.len() as u32;
    let mut name: Vec<u32> = vec![NONE; id_count as usize];
    for id in 0..id_count {
        let root = people.person(id) as usize;
        if name[root] == NONE || people.ids.get(id) < people.ids.get(name[root]) {
            name[root] = id;
        }
    }

    // Walk the people: the events and the clicks are both grouped by person, in the same order.
    let mut tally = Tally {
        user_rows: Vec::new(),
        campaign_days: HashMap::new(),
        organic,
        in_window: vec![0; organic as usize],
        day_assists: vec![0; organic as usize],
        campaigns: Vec::new(),
        category_cents: vec![0; categories.len()],
        categories: Vec::new(),
    };
    let (mut first, mut click_at) = (0, 0);
    while first < events.len() {
        let person = events[first].person;
        let mut end = first;
        while end < events.len() && events[end].person == person {
            end += 1;
        }
        while click_at < clicks.len() && clicks[click_at].person < person {
            click_at += 1;
        }
        let click_first = click_at;
        while click_at < clicks.len() && clicks[click_at].person == person {
            click_at += 1;
        }
        let person_name = name[person as usize];
        let name_key = prefix_key(people.ids.get(person_name));
        tally.person(person_name, name_key, &categories, &events[first..end], &clicks[click_first..click_at]);
        first = end;
    }
    drop(events);
    drop(clicks);

    // user_daily.csv, sorted by (user_id bytes, day).
    let ids = &people.ids;
    let mut user_rows = tally.user_rows;
    user_rows.sort_unstable_by(|a, b| {
        if a.name == b.name {
            a.day.cmp(&b.day)
        } else {
            a.name_key.cmp(&b.name_key).then_with(|| ids.get(a.name).cmp(ids.get(b.name)))
        }
    });

    // campaign_daily.csv, sorted by (campaign_id bytes, day).
    let mut campaign_rows: Vec<(&[u8], i64, u32, &CampaignDay)> = Vec::new();
    for (&(campaign, day), counts) in &tally.campaign_days {
        let campaign_id: &[u8] = if campaign == organic { b"organic" } else { campaigns.get(campaign) };
        campaign_rows.push((campaign_id, day, campaign, counts));
    }
    campaign_rows.sort_unstable_by(|a, b| (a.0, a.1, a.2).cmp(&(b.0, b.1, b.2)));

    // The rules' self-check: both tables count the same sessions and the same revenue, and the
    // assisted sessions are at least the sessions credited to a real campaign.
    let user_total = user_rows.iter().fold((0u64, 0u64), |t, r| (t.0 + r.sessions, t.1 + r.revenue));
    let (mut campaign_total, mut credited, mut assisted) = ((0u64, 0u64), 0u64, 0u64);
    for &(_, _, campaign, counts) in &campaign_rows {
        campaign_total = (campaign_total.0 + counts.sessions, campaign_total.1 + counts.revenue);
        assisted += counts.assisted;
        if campaign != organic {
            credited += counts.sessions;
        }
    }
    eprintln!(
        "totals: sessions {}, revenue_cents {}, credited to a campaign {credited}, assisted {assisted}",
        user_total.0, user_total.1
    );
    if user_total != campaign_total || assisted < credited {
        fail(&format!("self-check failed: user_daily {user_total:?}, campaign_daily {campaign_total:?}"));
    }

    fs::create_dir_all(out_dir).unwrap_or_else(|e| fail(&format!("{}: {e}", out_dir.display())));
    let write_users = || -> std::io::Result<()> {
        let mut out = create(&out_dir.join("user_daily.csv"));
        out.write_all(b"user_id,day,sessions,events,revenue_cents,top_category\n")?;
        for row in &user_rows {
            out.write_all(ids.get(row.name))?;
            out.write_all(b",")?;
            write_day(&mut out, row.day)?;
            write!(out, ",{},{},{},", row.sessions, row.events, row.revenue)?;
            if row.top_category != NONE {
                out.write_all(categories.get(row.top_category))?;
            }
            out.write_all(b"\n")?;
        }
        out.flush()
    };
    write_users().unwrap_or_else(|e| fail(&format!("user_daily.csv: {e}")));

    let write_campaigns = || -> std::io::Result<()> {
        let mut out = create(&out_dir.join("campaign_daily.csv"));
        out.write_all(b"campaign_id,channel,day,sessions,purchasing_sessions,revenue_cents,assisted_sessions\n")?;
        for &(campaign_id, day, campaign, counts) in &campaign_rows {
            out.write_all(campaign_id)?;
            out.write_all(b",")?;
            out.write_all(&channels[campaign as usize])?;
            out.write_all(b",")?;
            write_day(&mut out, day)?;
            writeln!(out, ",{},{},{},{}", counts.sessions, counts.purchasing, counts.revenue, counts.assisted)?;
        }
        out.flush()
    };
    write_campaigns().unwrap_or_else(|e| fail(&format!("campaign_daily.csv: {e}")));
    eprintln!("rows: user_daily {}, campaign_daily {}", user_rows.len(), campaign_rows.len());
}
