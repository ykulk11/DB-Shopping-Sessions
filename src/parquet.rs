//! A minimal Parquet writer: one row group, one uncompressed PLAIN data page
//! per column, Thrift compact metadata written by hand.

use std::fs::File;
use std::io::Write;
use std::path::Path;

#[derive(Clone, Copy, PartialEq)]
pub enum Kind {
    /// BYTE_ARRAY annotated as a UTF-8 string; data is `u32 length + bytes` per value.
    Str,
    /// INT32 annotated as DATE; data is little-endian i32 days since the epoch.
    Date,
    /// INT32; data is little-endian i32.
    I32,
    /// INT64; data is little-endian i64.
    I64,
}

pub struct Column<'a> {
    pub name: &'static str,
    pub kind: Kind,
    /// PLAIN-encoded values (non-null ones only for a nullable column).
    pub data: &'a [u8],
    /// For a nullable column: one bit per row, set when the value is present.
    pub validity: Option<&'a [u8]>,
}

struct Thrift {
    buf: Vec<u8>,
    last: i16,
    stack: Vec<i16>,
}

const T_I32: u8 = 5;
const T_I64: u8 = 6;
const T_BINARY: u8 = 8;
const T_LIST: u8 = 9;
const T_STRUCT: u8 = 12;

impl Thrift {
    fn new() -> Self {
        Thrift { buf: Vec::with_capacity(1024), last: 0, stack: Vec::new() }
    }
    fn varint(&mut self, mut v: u64) {
        while v >= 0x80 {
            self.buf.push((v as u8) | 0x80);
            v >>= 7;
        }
        self.buf.push(v as u8);
    }
    fn zigzag(&mut self, v: i64) {
        self.varint(((v << 1) ^ (v >> 63)) as u64);
    }
    fn field(&mut self, id: i16, ty: u8) {
        let delta = id - self.last;
        if delta > 0 && delta <= 15 {
            self.buf.push(((delta as u8) << 4) | ty);
        } else {
            self.buf.push(ty);
            self.zigzag(id as i64);
        }
        self.last = id;
    }
    fn i32(&mut self, id: i16, v: i32) {
        self.field(id, T_I32);
        self.zigzag(v as i64);
    }
    fn i64(&mut self, id: i16, v: i64) {
        self.field(id, T_I64);
        self.zigzag(v);
    }
    fn string(&mut self, id: i16, s: &[u8]) {
        self.field(id, T_BINARY);
        self.varint(s.len() as u64);
        self.buf.extend_from_slice(s);
    }
    fn begin_struct(&mut self, id: i16) {
        self.field(id, T_STRUCT);
        self.stack.push(self.last);
        self.last = 0;
    }
    /// A struct that is a list element: no field header.
    fn begin_element(&mut self) {
        self.stack.push(self.last);
        self.last = 0;
    }
    fn end_struct(&mut self) {
        self.buf.push(0);
        self.last = self.stack.pop().unwrap();
    }
    fn list(&mut self, id: i16, elem: u8, n: usize) {
        self.field(id, T_LIST);
        if n < 15 {
            self.buf.push(((n as u8) << 4) | elem);
        } else {
            self.buf.push(0xf0 | elem);
            self.varint(n as u64);
        }
    }
}

fn physical(kind: Kind) -> i32 {
    match kind {
        Kind::Str => 6,
        Kind::Date | Kind::I32 => 1,
        Kind::I64 => 2,
    }
}

fn page_header(rows: usize, size: usize) -> Vec<u8> {
    let mut t = Thrift::new();
    t.i32(1, 0); // DATA_PAGE
    t.i32(2, size as i32);
    t.i32(3, size as i32);
    t.begin_struct(5);
    t.i32(1, rows as i32);
    t.i32(2, 0); // PLAIN
    t.i32(3, 3); // definition levels: RLE
    t.i32(4, 3); // repetition levels: RLE
    t.end_struct();
    t.buf.push(0);
    t.buf
}

/// Definition levels for a nullable column: one bit-packed run, 4-byte length first.
fn definition_levels(rows: usize, validity: &[u8]) -> Vec<u8> {
    let groups = (rows + 7) / 8;
    let mut run = Vec::with_capacity(groups + 8);
    let mut header = ((groups as u64) << 1) | 1;
    while header >= 0x80 {
        run.push((header as u8) | 0x80);
        header >>= 7;
    }
    run.push(header as u8);
    run.extend_from_slice(&validity[..groups]);
    let mut out = Vec::with_capacity(run.len() + 4);
    out.extend_from_slice(&(run.len() as u32).to_le_bytes());
    out.extend_from_slice(&run);
    out
}

/// Writes one Parquet file with `rows` rows. With zero rows the file has a
/// schema and no row group.
pub fn write_file(path: &Path, rows: usize, columns: &[Column]) -> std::io::Result<()> {
    let mut parts: Vec<Vec<u8>> = Vec::new();
    let mut order: Vec<(usize, Option<usize>)> = Vec::new(); // (small part, column data index)
    let mut offset = 4u64;
    let mut chunks: Vec<(u64, u64)> = Vec::new(); // (page offset, total size)

    if rows > 0 {
        for (ci, c) in columns.iter().enumerate() {
            let levels = c.validity.map(|v| definition_levels(rows, v)).unwrap_or_default();
            let size = levels.len() + c.data.len();
            let mut head = page_header(rows, size);
            head.extend_from_slice(&levels);
            let total = (head.len() + c.data.len()) as u64;
            chunks.push((offset, total));
            offset += total;
            parts.push(head);
            order.push((parts.len() - 1, Some(ci)));
        }
    }

    let mut t = Thrift::new();
    t.i32(1, 1); // version
    t.list(2, T_STRUCT, columns.len() + 1);
    t.begin_element();
    t.string(4, b"schema");
    t.i32(5, columns.len() as i32);
    t.end_struct();
    for c in columns {
        t.begin_element();
        t.i32(1, physical(c.kind));
        t.i32(3, if c.validity.is_some() { 1 } else { 0 });
        t.string(4, c.name.as_bytes());
        match c.kind {
            Kind::Str => {
                t.i32(6, 0); // UTF8
                t.begin_struct(10);
                t.begin_struct(1); // STRING
                t.end_struct();
                t.end_struct();
            }
            Kind::Date => {
                t.i32(6, 6); // DATE
                t.begin_struct(10);
                t.begin_struct(6); // DATE
                t.end_struct();
                t.end_struct();
            }
            _ => {}
        }
        t.end_struct();
    }
    t.i64(3, rows as i64);
    if rows > 0 {
        t.list(4, T_STRUCT, 1);
        t.begin_element();
        t.list(1, T_STRUCT, columns.len());
        let mut total_bytes = 0u64;
        for (c, &(page_offset, total)) in columns.iter().zip(&chunks) {
            total_bytes += total;
            t.begin_element();
            t.i64(2, page_offset as i64);
            t.begin_struct(3);
            t.i32(1, physical(c.kind));
            t.list(2, T_I32, 2);
            t.zigzag(0); // PLAIN
            t.zigzag(3); // RLE
            t.list(3, T_BINARY, 1);
            t.varint(c.name.len() as u64);
            t.buf.extend_from_slice(c.name.as_bytes());
            t.i32(4, 0); // UNCOMPRESSED
            t.i64(5, rows as i64);
            t.i64(6, total as i64);
            t.i64(7, total as i64);
            t.i64(9, page_offset as i64);
            t.end_struct();
            t.end_struct();
        }
        t.i64(2, total_bytes as i64);
        t.i64(3, rows as i64);
        t.end_struct();
    } else {
        t.list(4, T_STRUCT, 0);
    }
    t.string(6, b"shopping-sessions pipeline");
    t.buf.push(0);
    let mut tail = t.buf;
    let footer_len = tail.len() as u32;
    tail.extend_from_slice(&footer_len.to_le_bytes());
    tail.extend_from_slice(b"PAR1");

    let mut file = File::create(path)?;
    let mut small: Vec<u8> = Vec::with_capacity(4096);
    small.extend_from_slice(b"PAR1");
    for &(pi, ci) in &order {
        small.extend_from_slice(&parts[pi]);
        let data = columns[ci.unwrap()].data;
        if data.len() < 1 << 16 {
            small.extend_from_slice(data);
        } else {
            file.write_all(&small)?;
            small.clear();
            file.write_all(data)?;
        }
    }
    small.extend_from_slice(&tail);
    file.write_all(&small)?;
    Ok(())
}
