//! Read-only decoders for binary data files that have no text form: NumPy arrays (`.npy`),
//! MessagePack values, TrueType/OpenType/WOFF fonts and WAV audio. Each reports the file's
//! structure and a bounded sample of its values; nothing is executed or written.

use serde_json::{Map, Value, json};
use std::fmt::Write as _;

#[cfg(test)]
mod tests;

/// Values shown from a flat array; a 2-D array shows at most `ROWS` x `COLUMNS`.
const VALUES: usize = 200;
const ROWS: usize = 20;
const COLUMNS: usize = 12;

fn u16_le(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?))
}
fn u32_le(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}
fn u16_be(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes(b.get(at..at + 2)?.try_into().ok()?))
}
fn u32_be(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

// ---------------------------------------------------------------------------------------------
// NumPy .npy

/// The `.npy` header: element type, memory order and shape.
struct NpyHeader {
    descr: String,
    fortran: bool,
    shape: Vec<usize>,
}

/// Parses the Python dict literal of an `.npy` header without evaluating it.
fn npy_header(text: &str) -> Result<NpyHeader, String> {
    let field = |key: &str| -> Result<&str, String> {
        let at = text
            .find(&format!("'{key}'"))
            .ok_or_else(|| format!("NumPy header has no '{key}'"))?;
        let rest = text[at + key.len() + 2..].trim_start();
        Ok(rest.strip_prefix(':').unwrap_or(rest).trim_start())
    };
    let descr = field("descr")?;
    let descr = descr
        .strip_prefix('\'')
        .and_then(|s| s.split_once('\''))
        .map(|(d, _)| d.to_owned())
        .ok_or("NumPy structured dtypes are not decoded; only scalar dtypes are")?;
    let fortran = field("fortran_order")?.starts_with("True");
    let shape = field("shape")?;
    let shape = shape
        .strip_prefix('(')
        .and_then(|s| s.split_once(')'))
        .ok_or("NumPy header shape is not a tuple")?
        .0
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| {
            s.parse::<usize>()
                .map_err(|_| format!("bad NumPy dimension {s:?}"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(NpyHeader {
        descr,
        fortran,
        shape,
    })
}

/// Renders element `i` of a scalar dtype such as `<f8`, `|u1` or `>i4`.
fn npy_value(descr: &str, data: &[u8], i: usize) -> Option<String> {
    let (order, code) = descr.split_at(1);
    let big = order == ">";
    let (kind, size) = code.split_at(1);
    let size: usize = size.parse().ok()?;
    let raw = data.get(i * size..(i + 1) * size)?;
    let mut bytes = raw.to_vec();
    if big {
        bytes.reverse();
    }
    let int = |signed: bool| -> Option<String> {
        let mut buf = [0u8; 16];
        buf[..size].copy_from_slice(&bytes);
        if signed && bytes[size - 1] & 0x80 != 0 {
            buf[size..].fill(0xff);
        }
        Some(if signed {
            i128::from_le_bytes(buf).to_string()
        } else {
            u128::from_le_bytes(buf).to_string()
        })
    };
    match (kind, size) {
        ("b", 1) => Some((bytes[0] != 0).to_string()),
        ("i", 1 | 2 | 4 | 8) => int(true),
        ("u", 1 | 2 | 4 | 8) => int(false),
        ("f", 2) => Some(half::f16::from_le_bytes([bytes[0], bytes[1]]).to_string()),
        ("f", 4) => Some(f32::from_le_bytes(bytes.try_into().ok()?).to_string()),
        ("f", 8) => Some(f64::from_le_bytes(bytes.try_into().ok()?).to_string()),
        _ => None,
    }
}

pub(super) fn npy(payload: &[u8]) -> Result<String, String> {
    if !payload.starts_with(b"\x93NUMPY") {
        return Err("not a NumPy .npy file".into());
    }
    let major = *payload.get(6).ok_or("truncated NumPy header")?;
    let (len, start) = match major {
        1 => (u16_le(payload, 8).map(usize::from), 10),
        2 | 3 => (u32_le(payload, 8).map(|n| n as usize), 12),
        v => return Err(format!("unsupported NumPy format version {v}")),
    };
    let len = len.ok_or("truncated NumPy header")?;
    let header = payload
        .get(start..start + len)
        .ok_or("truncated NumPy header")?;
    let header = npy_header(&String::from_utf8_lossy(header))?;
    let data = &payload[start + len..];
    let count: usize = header.shape.iter().product();
    let shape = header
        .shape
        .iter()
        .map(usize::to_string)
        .collect::<Vec<_>>()
        .join(" x ");
    let mut out = format!(
        "NumPy array: dtype {}, shape ({}), {} element(s), {} order, {} data byte(s)\n",
        header.descr,
        if shape.is_empty() {
            "scalar".into()
        } else {
            shape
        },
        count,
        if header.fortran { "Fortran" } else { "C" },
        data.len()
    );
    if header.descr.starts_with('|') && header.descr[1..].starts_with('O') {
        out.push_str("Object arrays hold pickled Python objects; they are not decoded.\n");
        return Ok(out);
    }
    if npy_value(&header.descr, &[0; 16], 0).is_none() {
        out.push_str(
            "Values are not shown for this dtype (only bool, int, uint and float are decoded).\n",
        );
        return Ok(out);
    }
    let value = |i: usize| npy_value(&header.descr, data, i).unwrap_or_else(|| "?".into());
    if let [rows, cols] = header.shape[..]
        && !header.fortran
    {
        let shown = rows.min(ROWS);
        let _ = writeln!(
            out,
            "rows 0-{} of {rows}, columns 0-{} of {cols}:",
            shown.saturating_sub(1),
            cols.min(COLUMNS).saturating_sub(1)
        );
        for r in 0..shown {
            let mut line: Vec<String> = (0..cols.min(COLUMNS))
                .map(|c| value(r * cols + c))
                .collect();
            if cols > COLUMNS {
                line.push("…".into());
            }
            let _ = writeln!(out, "  [{r}] {}", line.join("  "));
        }
        if rows > ROWS {
            let _ = writeln!(out, "  … {} more row(s)", rows - ROWS);
        }
    } else {
        let shown = count.min(VALUES);
        let _ = writeln!(
            out,
            "first {shown} value(s) in {} order:",
            if header.fortran {
                "storage (column-major)"
            } else {
                "row-major"
            }
        );
        let values: Vec<String> = (0..shown).map(value).collect();
        for chunk in values.chunks(10) {
            let _ = writeln!(out, "  {}", chunk.join("  "));
        }
        if count > shown {
            let _ = writeln!(out, "  … {} more", count - shown);
        }
    }
    if data.len() < count * header.descr[2..].parse::<usize>().unwrap_or(0) {
        out.push_str("warning: the data section is shorter than the shape requires\n");
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// MessagePack

/// Values decoded across all top-level objects before the rest is summarized.
const MSGPACK_ITEMS: usize = 200_000;
const MSGPACK_DEPTH: usize = 64;
const MSGPACK_OBJECTS: usize = 10_000;

struct Unpacker<'a> {
    b: &'a [u8],
    at: usize,
    items: usize,
}

impl Unpacker<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8], String> {
        let end = self.at.checked_add(n).filter(|&e| e <= self.b.len());
        let end = end.ok_or_else(|| format!("truncated MessagePack value at byte {}", self.at))?;
        let s = &self.b[self.at..end];
        self.at = end;
        Ok(s)
    }
    fn uint(&mut self, n: usize) -> Result<u64, String> {
        Ok(self
            .take(n)?
            .iter()
            .fold(0u64, |acc, &b| (acc << 8) | u64::from(b)))
    }
    fn text(&mut self, n: usize) -> Result<Value, String> {
        Ok(json!(String::from_utf8_lossy(self.take(n)?)))
    }
    fn bin(&mut self, n: usize) -> Result<Value, String> {
        self.take(n)?;
        Ok(json!(format!("[binary {n} bytes]")))
    }
    fn ext(&mut self, n: usize) -> Result<Value, String> {
        let kind = self.take(1)?[0] as i8;
        let data = self.take(n)?;
        if kind == -1 && matches!(n, 4 | 8) {
            let secs = if n == 4 {
                u64::from(u32_be(data, 0).unwrap_or(0))
            } else {
                u64::from_be_bytes(data.try_into().unwrap_or([0; 8])) & 0x3_ffff_ffff
            };
            return Ok(json!(format!("[timestamp {secs} s since the Unix epoch]")));
        }
        Ok(json!(format!("[extension type {kind}, {n} bytes]")))
    }
    fn array(&mut self, n: usize, depth: usize) -> Result<Value, String> {
        (0..n)
            .map(|_| self.value(depth + 1))
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array)
    }
    fn map(&mut self, n: usize, depth: usize) -> Result<Value, String> {
        let mut map = Map::new();
        for _ in 0..n {
            let key = match self.value(depth + 1)? {
                Value::String(s) => s,
                other => other.to_string(),
            };
            let value = self.value(depth + 1)?;
            map.insert(key, value);
        }
        Ok(Value::Object(map))
    }
    fn value(&mut self, depth: usize) -> Result<Value, String> {
        if depth > MSGPACK_DEPTH {
            return Err(format!(
                "MessagePack nesting exceeds {MSGPACK_DEPTH} levels"
            ));
        }
        self.items += 1;
        if self.items > MSGPACK_ITEMS {
            return Err(format!(
                "MessagePack holds more than {MSGPACK_ITEMS} values"
            ));
        }
        let tag = self.take(1)?[0];
        Ok(match tag {
            0x00..=0x7f => json!(tag),
            0x80..=0x8f => self.map(usize::from(tag & 0x0f), depth)?,
            0x90..=0x9f => self.array(usize::from(tag & 0x0f), depth)?,
            0xa0..=0xbf => self.text(usize::from(tag & 0x1f))?,
            0xc0 => Value::Null,
            0xc2 => json!(false),
            0xc3 => json!(true),
            0xc4..=0xc6 => {
                let n = self.uint(1 << (tag - 0xc4))? as usize;
                self.bin(n)?
            }
            0xc7..=0xc9 => {
                let n = self.uint(1 << (tag - 0xc7))? as usize;
                self.ext(n)?
            }
            0xca => json!(f32::from_bits(self.uint(4)? as u32)),
            0xcb => json!(f64::from_bits(self.uint(8)?)),
            0xcc..=0xcf => json!(self.uint(1 << (tag - 0xcc))?),
            0xd0 => json!(self.uint(1)? as u8 as i8),
            0xd1 => json!(self.uint(2)? as u16 as i16),
            0xd2 => json!(self.uint(4)? as u32 as i32),
            0xd3 => json!(self.uint(8)? as i64),
            0xd4..=0xd8 => self.ext(1 << (tag - 0xd4))?,
            0xd9..=0xdb => {
                let n = self.uint(1 << (tag - 0xd9))? as usize;
                self.text(n)?
            }
            0xdc | 0xdd => {
                let n = self.uint(2 << (tag - 0xdc))? as usize;
                self.array(n, depth)?
            }
            0xde | 0xdf => {
                let n = self.uint(2 << (tag - 0xde))? as usize;
                self.map(n, depth)?
            }
            0xe0..=0xff => json!(tag as i8),
            0xc1 => return Err(format!("invalid MessagePack byte 0xc1 at {}", self.at - 1)),
        })
    }
}

/// One object renders as indented JSON; a stream of concatenated objects renders one per line.
pub(super) fn msgpack(payload: &[u8]) -> Result<String, String> {
    let mut unpacker = Unpacker {
        b: payload,
        at: 0,
        items: 0,
    };
    let mut objects = Vec::new();
    while unpacker.at < payload.len() && objects.len() < MSGPACK_OBJECTS {
        objects.push(unpacker.value(0)?);
    }
    let rest = payload.len() - unpacker.at;
    let mut out = match objects.as_slice() {
        [] => return Err("empty MessagePack file".into()),
        [one] => format!(
            "MessagePack value (shown as JSON; binary and extension values are summarized):\n{}\n",
            serde_json::to_string_pretty(one).map_err(|e| e.to_string())?
        ),
        many => {
            let mut out = format!(
                "MessagePack stream: {} object(s), one JSON line each (binary and extension values are summarized):\n",
                many.len()
            );
            for object in many {
                out.push_str(&object.to_string());
                out.push('\n');
            }
            out
        }
    };
    if rest > 0 {
        let _ = writeln!(
            out,
            "[{rest} more byte(s) after the first {MSGPACK_OBJECTS} objects are not shown]"
        );
    }
    if out.len() > super::MAX_CONTENT as usize {
        return Err("decoded MessagePack exceeds 8 MiB".into());
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// Fonts

/// Table directory entry: tag, offset into the (decompressed) font, length.
struct Table {
    tag: String,
    offset: usize,
    length: usize,
    compressed: Option<usize>,
}

/// `name` table IDs worth showing, with their labels.
const NAMES: [(u16, &str); 9] = [
    (1, "family"),
    (2, "style"),
    (4, "full name"),
    (6, "PostScript name"),
    (5, "version"),
    (8, "manufacturer"),
    (9, "designer"),
    (0, "copyright"),
    (13, "license"),
];

fn sfnt_tables(font: &[u8], base: usize) -> Result<Vec<Table>, String> {
    let count = u16_be(font, base + 4).ok_or("truncated font header")?;
    (0..usize::from(count))
        .map(|i| {
            let at = base + 12 + i * 16;
            let record = font
                .get(at..at + 16)
                .ok_or("truncated font table directory")?;
            Ok(Table {
                tag: String::from_utf8_lossy(&record[..4]).into_owned(),
                offset: u32_be(record, 8).unwrap_or(0) as usize,
                length: u32_be(record, 12).unwrap_or(0) as usize,
                compressed: None,
            })
        })
        .collect()
}

fn woff_tables(font: &[u8]) -> Result<Vec<Table>, String> {
    let count = u16_be(font, 12).ok_or("truncated WOFF header")?;
    (0..usize::from(count))
        .map(|i| {
            let at = 44 + i * 20;
            let record = font
                .get(at..at + 20)
                .ok_or("truncated WOFF table directory")?;
            let stored = u32_be(record, 8).unwrap_or(0) as usize;
            let length = u32_be(record, 12).unwrap_or(0) as usize;
            Ok(Table {
                tag: String::from_utf8_lossy(&record[..4]).into_owned(),
                offset: u32_be(record, 4).unwrap_or(0) as usize,
                length,
                compressed: (stored < length).then_some(stored),
            })
        })
        .collect()
}

/// The bytes of one table, inflating a WOFF zlib table (bounded by its declared length).
fn table_bytes(font: &[u8], table: &Table) -> Option<Vec<u8>> {
    let stored = table.compressed.unwrap_or(table.length);
    let raw = font.get(table.offset..table.offset.checked_add(stored)?)?;
    if table.compressed.is_none() {
        return Some(raw.to_vec());
    }
    use std::io::Read as _;
    let mut out = Vec::new();
    flate2::read::ZlibDecoder::new(raw)
        .take(table.length as u64)
        .read_to_end(&mut out)
        .ok()?;
    Some(out)
}

/// Best English-ish record for each wanted name ID: Windows Unicode first, then Mac Roman.
fn font_names(name: &[u8]) -> Vec<(&'static str, String)> {
    let (Some(count), Some(strings)) = (u16_be(name, 2), u16_be(name, 4)) else {
        return Vec::new();
    };
    let mut found: Vec<(u16, u8, String)> = Vec::new();
    for i in 0..usize::from(count) {
        let at = 6 + i * 12;
        let Some(r) = name.get(at..at + 12) else {
            break;
        };
        let (platform, language, id) = (
            u16_be(r, 0).unwrap_or(0),
            u16_be(r, 4).unwrap_or(0),
            u16_be(r, 6).unwrap_or(0),
        );
        let (len, off) = (
            usize::from(u16_be(r, 8).unwrap_or(0)),
            usize::from(u16_be(r, 10).unwrap_or(0)),
        );
        let start = usize::from(strings) + off;
        let Some(raw) = name.get(start..start + len) else {
            continue;
        };
        let rank = match (platform, language) {
            (3, 0x0409) => 0,
            (3, _) | (0, _) => 1,
            (1, 0) => 2,
            _ => continue,
        };
        let text = if platform == 1 {
            raw.iter().map(|&b| char::from(b)).collect()
        } else {
            let units: Vec<u16> = raw
                .as_chunks::<2>()
                .0
                .iter()
                .map(|c| u16::from_be_bytes(*c))
                .collect();
            String::from_utf16_lossy(&units)
        };
        match found.iter_mut().find(|(n, _, _)| *n == id) {
            Some(slot) if slot.1 > rank => *slot = (id, rank, text),
            Some(_) => {}
            None => found.push((id, rank, text)),
        }
    }
    NAMES
        .iter()
        .filter_map(|(id, label)| {
            let (_, _, text) = found.iter().find(|(n, _, _)| n == id)?;
            let text: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
            let text = if text.chars().count() > 300 {
                format!("{}…", text.chars().take(300).collect::<String>())
            } else {
                text
            };
            Some((*label, text))
        })
        .collect()
}

pub(super) fn font(payload: &[u8]) -> Result<String, String> {
    let magic = payload.get(..4).ok_or("truncated font")?;
    let (container, tables, extra) = match magic {
        b"ttcf" => {
            let fonts = u32_be(payload, 8).ok_or("truncated font collection")?;
            let first = u32_be(payload, 12).ok_or("truncated font collection")? as usize;
            (
                "TrueType/OpenType collection",
                sfnt_tables(payload, first)?,
                format!("{fonts} font(s) in the collection; the first is described\n"),
            )
        }
        b"wOFF" => ("WOFF web font", woff_tables(payload)?, String::new()),
        b"wOF2" => {
            let flavor = payload
                .get(4..8)
                .map(String::from_utf8_lossy)
                .unwrap_or_default();
            let count = u16_be(payload, 12).unwrap_or(0);
            return Ok(format!(
                "WOFF2 web font: flavor {flavor:?}, {count} table(s), {} byte(s) uncompressed\nTables are Brotli-compressed with transforms; names and metrics are not decoded.\n",
                u32_be(payload, 16).unwrap_or(0)
            ));
        }
        [0, 1, 0, 0] | b"true" => ("TrueType font", sfnt_tables(payload, 0)?, String::new()),
        b"OTTO" => (
            "OpenType font (CFF outlines)",
            sfnt_tables(payload, 0)?,
            String::new(),
        ),
        _ => return Err("not a TrueType, OpenType or WOFF font".into()),
    };
    let find = |tag: &str| {
        tables
            .iter()
            .find(|t| t.tag == tag)
            .and_then(|t| table_bytes(payload, t))
    };
    let mut out = format!("{container}\n{extra}");
    for (label, text) in find("name").map(|n| font_names(&n)).unwrap_or_default() {
        let _ = writeln!(out, "{label}: {text}");
    }
    if let Some(units) = find("head").and_then(|h| u16_be(&h, 18)) {
        let _ = writeln!(out, "units per em: {units}");
    }
    if let Some(glyphs) = find("maxp").and_then(|m| u16_be(&m, 4)) {
        let _ = writeln!(out, "glyphs: {glyphs}");
    }
    if let Some(weight) = find("OS/2").and_then(|o| u16_be(&o, 4)) {
        let _ = writeln!(out, "weight class: {weight}");
    }
    if tables.iter().any(|t| t.tag == "fvar") {
        out.push_str("variable font (fvar axes present)\n");
    }
    let _ = writeln!(
        out,
        "tables ({}): {}",
        tables.len(),
        tables
            .iter()
            .map(|t| t.tag.trim_end())
            .collect::<Vec<_>>()
            .join(" ")
    );
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// WAV

const WAVE_FORMATS: [(u16, &str); 5] = [
    (1, "PCM"),
    (3, "IEEE float"),
    (6, "A-law"),
    (7, "µ-law"),
    (0x55, "MPEG Layer 3"),
];

/// Peak and RMS level in dBFS over all channels; `None` for encodings this does not decode.
fn levels(format: u16, bits: u16, data: &[u8]) -> Option<(f64, f64, usize)> {
    let width = usize::from(bits / 8);
    if width == 0 || !bits.is_multiple_of(8) {
        return None;
    }
    let sample = |s: &[u8]| -> Option<f64> {
        Some(match (format, bits) {
            (1, 8) => (f64::from(s[0]) - 128.0) / 128.0,
            (1, 16) => f64::from(i16::from_le_bytes([s[0], s[1]])) / 32768.0,
            (1, 24) => f64::from(i32::from_le_bytes([0, s[0], s[1], s[2]]) >> 8) / 8_388_608.0,
            (1, 32) => f64::from(i32::from_le_bytes([s[0], s[1], s[2], s[3]])) / 2_147_483_648.0,
            (3, 32) => f64::from(f32::from_le_bytes([s[0], s[1], s[2], s[3]])),
            (3, 64) => f64::from_le_bytes(s.try_into().ok()?),
            _ => return None,
        })
    };
    let (mut peak, mut sum, mut n) = (0f64, 0f64, 0usize);
    for s in data.chunks_exact(width) {
        let v = sample(s)?;
        peak = peak.max(v.abs());
        sum += v * v;
        n += 1;
    }
    let db = |x: f64| {
        if x > 0.0 {
            20.0 * x.log10()
        } else {
            f64::NEG_INFINITY
        }
    };
    Some((db(peak), db((sum / n.max(1) as f64).sqrt()), n))
}

pub(super) fn wav(payload: &[u8]) -> Result<String, String> {
    if !(payload.starts_with(b"RIFF") && payload.get(8..12) == Some(b"WAVE")) {
        return Err("not a RIFF/WAVE file".into());
    }
    let (mut at, mut fmt, mut data, mut info) = (12usize, None, None, Vec::new());
    while let (Some(id), Some(len)) = (payload.get(at..at + 4), u32_le(payload, at + 4)) {
        let len = len as usize;
        let body =
            &payload[(at + 8).min(payload.len())..(at + 8).saturating_add(len).min(payload.len())];
        match id {
            b"fmt " => fmt = Some(body),
            b"data" => data = Some((body, len)),
            b"LIST" if body.starts_with(b"INFO") => {
                let mut p = 4;
                while let (Some(tag), Some(n)) = (body.get(p..p + 4), u32_le(body, p + 4)) {
                    let n = n as usize;
                    if let Some(text) = body.get(p + 8..p + 8 + n) {
                        let text = String::from_utf8_lossy(text)
                            .trim_end_matches('\0')
                            .trim()
                            .to_owned();
                        if !text.is_empty() {
                            info.push(format!("{}: {text}", String::from_utf8_lossy(tag)));
                        }
                    }
                    p += 8 + n + n % 2;
                }
            }
            _ => {}
        }
        at = at.saturating_add(8 + len + len % 2);
    }
    let fmt = fmt.ok_or("WAV file has no fmt chunk")?;
    let mut format = u16_le(fmt, 0).ok_or("truncated WAV fmt chunk")?;
    if format == 0xfffe {
        format = u16_le(fmt, 24).unwrap_or(format);
    }
    let channels = u16_le(fmt, 2).unwrap_or(0);
    let rate = u32_le(fmt, 4).unwrap_or(0);
    let bytes_per_sec = u32_le(fmt, 8).unwrap_or(0);
    let bits = u16_le(fmt, 14).unwrap_or(0);
    let name = WAVE_FORMATS.iter().find(|(n, _)| *n == format).map_or_else(
        || format!("format 0x{format:04x}"),
        |(_, n)| (*n).to_owned(),
    );
    let mut out = format!("WAV audio: {name}, {channels} channel(s), {rate} Hz, {bits}-bit\n");
    match data {
        Some((body, declared)) => {
            if bytes_per_sec > 0 {
                let _ = writeln!(
                    out,
                    "duration: {:.3} s ({declared} data bytes)",
                    declared as f64 / f64::from(bytes_per_sec)
                );
            }
            if body.len() < declared {
                let _ = writeln!(
                    out,
                    "warning: data chunk declares {declared} bytes but only {} are present",
                    body.len()
                );
            }
            if let Some((peak, rms, samples)) = levels(format, bits, body) {
                if peak.is_finite() {
                    let _ = writeln!(
                        out,
                        "level: peak {peak:.1} dBFS, RMS {rms:.1} dBFS over {samples} sample(s)"
                    );
                } else {
                    let _ = writeln!(out, "level: digital silence over {samples} sample(s)");
                }
            }
        }
        None => out.push_str("no data chunk\n"),
    }
    for line in info {
        let _ = writeln!(out, "{line}");
    }
    out.push_str("For transcription or windowed levels, use the audio tool.\n");
    Ok(out)
}
