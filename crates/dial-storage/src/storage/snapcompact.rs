//! Layered local conversation archives (`dial_snapcompact` version 2) and their bounded retrieval.
//!
//! Each compaction generation contributes one [`Layer`]: its exact source messages, a readable
//! source text, and the PNG frames rendered from that text. A layer id is a SHA-256 digest of the
//! messages and source text only. Frames are checked structurally (PNG signature, chunk CRCs,
//! IHDR/IEND placement, dimensions) before any view uses them. That proves the bytes are
//! well-formed PNGs, not that they depict the source.
//!
//! Record references are `layer id` plus the local message index inside that layer, never
//! journal entry ids. Views return bounded text (records, a visible `next` cursor, totals and a
//! history caution) and a `data` object with byte spans; `data` is not shown to the model.
use crate::runtime::tool_output::ToolOutput;
use anyhow::{Context, Result, bail, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet},
    fmt::Write as _,
};

#[cfg(test)]
mod tests;

pub const VERSION: u64 = 2;
pub const MAX_LAYERS: usize = 32;
/// Aggregate budget for serialized layer messages plus source text across unique layers.
const MAX_TEXT_BYTES: usize = 16 * 1024 * 1024;
/// Aggregate budget for decoded PNG bytes across unique layers.
const MAX_PNG_BYTES: usize = 64 * 1024 * 1024;
const MAX_FRAMES: usize = 64;
const MAX_EDGE: u32 = 2576;
const MAX_OUTPUT_BYTES: usize = 16 * 1024;
/// Bytes kept free at the end of every view output for the continuation footer.
const FOOTER_BYTES: usize = 1024;
const MAX_QUERY_BYTES: usize = 4096;
const MAX_TOKENS: usize = 8;
const MAX_REASONS: usize = 8;
const DEFAULT_LIMIT: usize = 4;
const MAX_LIMIT: usize = 8;
const LAYER_FIELDS: [&str; 5] = ["id", "color", "source_text", "messages", "frames"];
const VIEW_FIELDS: [&str; 8] = [
    "view", "query", "layer", "record", "offset", "start", "limit", "format",
];
const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
const PNG_PREFIX: &str = "data:image/png;base64,";

/// One compaction generation of a v2 archive.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Layer {
    pub id: String,
    pub color: [u8; 3],
    pub source_text: String,
    pub messages: Vec<Value>,
    pub frames: Vec<Value>,
}

#[derive(Serialize)]
struct Identity<'a> {
    messages: &'a [Value],
    source_text: &'a str,
}

fn identity_bytes(messages: &[Value], source_text: &str) -> Result<Vec<u8>> {
    Ok(serde_json::to_vec(&Identity {
        messages,
        source_text,
    })?)
}

/// SHA-256 (lowercase hex) of the exact serialized messages and source text. Frames are not
/// covered; [`layers`] validates them separately.
pub fn layer_id(messages: &[Value], source_text: &str) -> Result<String> {
    Ok(digest_hex(&identity_bytes(messages, source_text)?))
}

fn digest_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn is_digest(text: &str) -> bool {
    text.len() == 64
        && text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

struct Parsed {
    layer: Layer,
    text_bytes: usize,
    png_bytes: usize,
}

fn parse_layer(value: &Value) -> Result<Parsed> {
    let object = value
        .as_object()
        .context("snapcompact layer must be an object")?;
    if let Some(key) = object
        .keys()
        .find(|key| !LAYER_FIELDS.contains(&key.as_str()))
    {
        bail!("unknown snapcompact layer field {key}");
    }
    let layer = Layer::deserialize(value).context("malformed snapcompact layer")?;
    ensure!(
        is_digest(&layer.id),
        "snapcompact layer id is not a SHA-256 digest"
    );
    let identity = identity_bytes(&layer.messages, &layer.source_text)?;
    ensure!(
        digest_hex(&identity) == layer.id,
        "snapcompact layer {} does not match its source digest",
        layer.id
    );
    for message in &layer.messages {
        let object = message
            .as_object()
            .context("snapcompact layer messages must be objects")?;
        ensure!(
            !object.contains_key("dial_snapcompact"),
            "snapcompact layers cannot contain archive containers"
        );
    }
    ensure!(
        layer.frames.len() <= MAX_FRAMES,
        "snapcompact layer {} has more than {MAX_FRAMES} frames",
        layer.id
    );
    let mut png_bytes = 0usize;
    for frame in &layer.frames {
        png_bytes = png_bytes
            .checked_add(decode_frame(frame)?)
            .context("snapcompact PNG byte count overflow")?;
        ensure!(
            png_bytes <= MAX_PNG_BYTES,
            "snapcompact frames exceed the {MAX_PNG_BYTES}-byte PNG budget"
        );
    }
    Ok(Parsed {
        layer,
        text_bytes: identity.len(),
        png_bytes,
    })
}

/// Validates one `image_url` content part carrying an inline PNG and returns its decoded length.
fn decode_frame(frame: &Value) -> Result<usize> {
    let object = frame
        .as_object()
        .context("snapcompact frame must be an image_url content part")?;
    ensure!(
        object.len() == 2 && object.get("type").and_then(Value::as_str) == Some("image_url"),
        "snapcompact frame must be an image_url content part"
    );
    let image = object
        .get("image_url")
        .and_then(Value::as_object)
        .context("snapcompact frame lacks image_url")?;
    ensure!(
        image.keys().all(|key| key == "url" || key == "detail"),
        "unknown snapcompact frame image field"
    );
    if let Some(detail) = image.get("detail") {
        ensure!(
            detail.is_string(),
            "snapcompact frame detail must be a string"
        );
    }
    let url = image
        .get("url")
        .and_then(Value::as_str)
        .context("snapcompact frame lacks a url")?;
    let encoded = url
        .strip_prefix(PNG_PREFIX)
        .context("snapcompact frame must be an inline PNG data URL")?;
    ensure!(
        encoded.len() / 4 * 3 <= MAX_PNG_BYTES,
        "snapcompact frame exceeds the PNG byte budget"
    );
    let bytes = STANDARD
        .decode(encoded)
        .context("snapcompact frame has invalid base64")?;
    validate_png(&bytes)?;
    Ok(bytes.len())
}

const CRC_TABLE: [u32; 256] = crc_table();

const fn crc_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut n = 0;
    while n < 256 {
        let mut c = n as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        table[n] = c;
        n += 1;
    }
    table
}

fn crc32(parts: &[&[u8]]) -> u32 {
    let mut crc = !0u32;
    for part in parts {
        for &byte in *part {
            crc = CRC_TABLE[((crc ^ u32::from(byte)) & 0xff) as usize] ^ (crc >> 8);
        }
    }
    !crc
}

/// Structural PNG check: signature, chunk lengths and CRCs, IHDR first with bounded nonzero
/// dimensions, at least one IDAT, and IEND last with no trailing bytes.
fn validate_png(bytes: &[u8]) -> Result<()> {
    ensure!(
        bytes.starts_with(&PNG_SIGNATURE),
        "snapcompact frame is not a PNG"
    );
    let mut position = PNG_SIGNATURE.len();
    let mut header: Option<(u32, u32)> = None;
    let mut image_data = false;
    let mut ended = false;
    while position < bytes.len() {
        ensure!(!ended, "PNG data continues after IEND");
        let head = bytes
            .get(position..position + 8)
            .context("truncated PNG chunk header")?;
        let length = u32::from_be_bytes([head[0], head[1], head[2], head[3]]) as usize;
        let kind = &head[4..8];
        let data_start = position + 8;
        let data_end = data_start
            .checked_add(length)
            .context("PNG chunk length overflow")?;
        let data = bytes
            .get(data_start..data_end)
            .context("truncated PNG chunk")?;
        let crc = bytes
            .get(data_end..data_end + 4)
            .context("truncated PNG chunk CRC")?;
        ensure!(
            crc32(&[kind, data]) == u32::from_be_bytes([crc[0], crc[1], crc[2], crc[3]]),
            "PNG chunk CRC mismatch"
        );
        if position == PNG_SIGNATURE.len() {
            ensure!(kind == b"IHDR", "PNG must start with IHDR");
        }
        match kind {
            b"IHDR" => {
                ensure!(header.is_none() && length == 13, "malformed PNG IHDR");
                let width = u32::from_be_bytes(data[0..4].try_into()?);
                let height = u32::from_be_bytes(data[4..8].try_into()?);
                ensure!(
                    width > 0 && height > 0 && width <= MAX_EDGE && height <= MAX_EDGE,
                    "PNG dimensions {width}x{height} outside the snapcompact limit"
                );
                header = Some((width, height));
            }
            b"IDAT" => image_data = true,
            b"IEND" => {
                ensure!(length == 0, "malformed PNG IEND");
                ended = true;
            }
            _ => {}
        }
        position = data_end + 4;
    }
    ensure!(
        header.is_some() && image_data && ended,
        "incomplete PNG (missing IHDR, IDAT or IEND)"
    );
    Ok(())
}

/// Parses every v2 archive in the active projection, deduplicating identical layers by id.
/// Version 1 archives are ignored here; the legacy renderer owns them. Any malformed, conflicting
/// or over-budget v2 layer fails the whole call.
pub fn layers(messages: &[Value]) -> Result<Vec<Layer>> {
    let mut out: Vec<Layer> = Vec::new();
    let mut slots: BTreeMap<String, usize> = BTreeMap::new();
    let mut text_total = 0usize;
    let mut png_total = 0usize;
    for message in messages {
        let Some(archive) = message.get("dial_snapcompact").filter(|v| !v.is_null()) else {
            continue;
        };
        match archive.get("version").and_then(Value::as_u64) {
            Some(1) => continue,
            Some(VERSION) => {}
            _ => bail!("unsupported snapcompact archive version"),
        }
        let items = archive
            .get("layers")
            .and_then(Value::as_array)
            .context("snapcompact v2 archive lacks a layers array")?;
        for item in items {
            let parsed = parse_layer(item)?;
            if let Some(&slot) = slots.get(&parsed.layer.id) {
                ensure!(
                    out[slot] == parsed.layer,
                    "conflicting snapcompact layers share id {}",
                    parsed.layer.id
                );
                continue;
            }
            ensure!(
                out.len() < MAX_LAYERS,
                "snapcompact archive exceeds {MAX_LAYERS} layers"
            );
            text_total += parsed.text_bytes;
            png_total += parsed.png_bytes;
            ensure!(
                text_total <= MAX_TEXT_BYTES,
                "snapcompact text exceeds the 16MiB archive budget"
            );
            ensure!(
                png_total <= MAX_PNG_BYTES,
                "snapcompact PNG frames exceed the 64MiB archive budget"
            );
            slots.insert(parsed.layer.id.clone(), out.len());
            out.push(parsed.layer);
        }
    }
    Ok(out)
}

/// Deterministic transcript of the messages: role headers, plain text content with code line
/// breaks kept, tool calls with id/name/arguments, and every other field as compact JSON.
/// Image data is shown as a placeholder; the record digest identifies the exact source.
pub fn readable(messages: &[Value]) -> Result<String> {
    let mut out = String::new();
    for (index, message) in messages.iter().enumerate() {
        let piece = render_message(index, message)?;
        ensure!(
            out.len() + piece.len() <= MAX_TEXT_BYTES,
            "readable snapcompact transcript exceeds 16MiB"
        );
        out.push_str(&piece);
    }
    Ok(out)
}

fn plain(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

fn render_message(index: usize, message: &Value) -> Result<String> {
    let object = message
        .as_object()
        .context("snapcompact messages must be objects")?;
    let mut out = String::new();
    let role = object.get("role").map_or_else(|| "none".to_owned(), plain);
    writeln!(out, "MESSAGE {index} role={role}")?;
    match object.get("content") {
        None | Some(Value::Null) => {}
        Some(Value::String(text)) => writeln!(out, "{text}")?,
        Some(Value::Array(parts)) => {
            for part in parts {
                render_part(part, &mut out)?;
            }
        }
        Some(other) => writeln!(out, "content: {other}")?,
    }
    if let Some(id) = object.get("tool_call_id") {
        writeln!(out, "TOOL RESULT tool_call_id={}", plain(id))?;
    }
    match object.get("tool_calls") {
        None | Some(Value::Null) => {}
        Some(Value::Array(calls)) => {
            for call in calls {
                render_call(call, &mut out)?;
            }
        }
        Some(other) => writeln!(out, "tool_calls: {other}")?,
    }
    for (key, value) in object {
        if !matches!(
            key.as_str(),
            "role" | "content" | "tool_call_id" | "tool_calls"
        ) {
            writeln!(out, "{key}: {value}")?;
        }
    }
    out.push_str("END MESSAGE\n");
    Ok(out)
}

fn render_part(part: &Value, out: &mut String) -> Result<()> {
    match (
        part.get("type").and_then(Value::as_str),
        part.get("text").and_then(Value::as_str),
    ) {
        (Some("text"), Some(text)) => writeln!(out, "{text}")?,
        (Some("image_url"), _) => out.push_str("[image_url]\n"),
        _ => writeln!(out, "{part}")?,
    }
    Ok(())
}

fn render_call(call: &Value, out: &mut String) -> Result<()> {
    let function = call.get("function");
    let field = |name: &str| {
        call.get(name)
            .or_else(|| function.and_then(|f| f.get(name)))
    };
    let id = call.get("id").map_or_else(|| "none".to_owned(), plain);
    let name = field("name").map_or_else(|| "none".to_owned(), plain);
    writeln!(out, "TOOL CALL id={id} name={name}")?;
    if let Some(arguments) = field("arguments").filter(|v| !v.is_null()) {
        writeln!(out, "{}", plain(arguments))?;
    }
    Ok(())
}

/// Tool-call ids a record carries: its own `tool_call_id`, `tool_calls[].id`, and tool-use parts.
fn tool_ids(message: &Value) -> BTreeSet<String> {
    let mut ids = BTreeSet::new();
    if let Some(id) = message.get("tool_call_id").and_then(Value::as_str) {
        ids.insert(id.to_owned());
    }
    for call in message
        .get("tool_calls")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(id) = call.get("id").and_then(Value::as_str) {
            ids.insert(id.to_owned());
        }
    }
    for part in message
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        for key in ["tool_use_id", "tool_call_id", "call_id"] {
            if let Some(id) = part.get(key).and_then(Value::as_str) {
                ids.insert(id.to_owned());
            }
        }
    }
    ids
}

/// Lexical path/reference tokens: contain `/` or `.`, with a letter, length 3..=256.
fn reference_tokens(text: &str) -> BTreeSet<String> {
    text.split(|c: char| c.is_whitespace() || "\"\\'`(){}[]<>,;=|".contains(c))
        .filter(|token| {
            (3..=256).contains(&token.len())
                && (token.contains('/') || token.contains('.'))
                && token.chars().any(char::is_alphabetic)
        })
        .map(str::to_owned)
        .collect()
}

/// One message (or the whole source text of a message-less legacy layer) in a selected scope.
struct Rec<'a> {
    layer: usize,
    index: usize,
    /// Exact canonical JSON of the message, or the literal source text when there are no messages.
    raw: Cow<'a, str>,
    digest: String,
    tools: BTreeSet<String>,
}

fn records(position: usize, layer: &Layer) -> Result<Vec<Rec<'_>>> {
    if layer.messages.is_empty() {
        return Ok(vec![Rec {
            layer: position,
            index: 0,
            raw: Cow::Borrowed(layer.source_text.as_str()),
            digest: digest_hex(layer.source_text.as_bytes()),
            tools: BTreeSet::new(),
        }]);
    }
    let mut out = Vec::with_capacity(layer.messages.len());
    for (index, message) in layer.messages.iter().enumerate() {
        let raw = serde_json::to_string(message)?;
        let digest = digest_hex(raw.as_bytes());
        out.push(Rec {
            layer: position,
            index,
            raw: Cow::Owned(raw),
            digest,
            tools: tool_ids(message),
        });
    }
    Ok(out)
}

/// A selectable unit: a layer summary, a record, a query match, or a linked related record.
struct Item {
    kind: &'static str,
    layer: usize,
    rec: Option<usize>,
    reasons: Vec<String>,
}

fn match_items(recs: &[Rec<'_>], terms: &[String]) -> Vec<Item> {
    let matched: Vec<bool> = recs
        .iter()
        .map(|rec| {
            let lower = rec.raw.to_lowercase();
            terms.iter().all(|term| lower.contains(term.as_str()))
        })
        .collect();
    let mut tools: BTreeSet<String> = BTreeSet::new();
    let mut tokens: BTreeSet<String> = BTreeSet::new();
    for (rec, _) in recs.iter().zip(&matched).filter(|(_, hit)| **hit) {
        tools.extend(rec.tools.iter().cloned());
        tokens.extend(reference_tokens(&rec.raw));
    }
    let tokens: Vec<String> = tokens.into_iter().take(MAX_TOKENS).collect();
    let mut items = Vec::new();
    for (i, rec) in recs.iter().enumerate() {
        if matched[i] {
            items.push(Item {
                kind: "match",
                layer: rec.layer,
                rec: Some(i),
                reasons: vec![format!("terms {}", terms.join(" "))],
            });
        }
    }
    for (i, rec) in recs.iter().enumerate() {
        if matched[i] {
            continue;
        }
        let mut reasons: Vec<String> = rec
            .tools
            .intersection(&tools)
            .map(|id| format!("tool_call_id {id}"))
            .collect();
        let references = reference_tokens(&rec.raw);
        reasons.extend(
            tokens
                .iter()
                .filter(|token| references.contains(token.as_str()))
                .map(|token| format!("reference {token}")),
        );
        if !reasons.is_empty() {
            reasons.truncate(MAX_REASONS);
            items.push(Item {
                kind: "related",
                layer: rec.layer,
                rec: Some(i),
                reasons,
            });
        }
    }
    items
}

fn layer_summary(layer: &Layer) -> String {
    let [r, g, b] = layer.color;
    let records = layer.messages.len().max(1);
    format!(
        "layer {} color=#{r:02x}{g:02x}{b:02x} records={records} source_bytes={} frames={}",
        layer.id,
        layer.source_text.len(),
        layer.frames.len()
    )
}

struct Body {
    reference: String,
    digest: String,
    record: Option<usize>,
    text: String,
}

fn body(all: &[Layer], recs: &[Rec<'_>], item: &Item) -> Body {
    match item.rec {
        Some(i) => {
            let rec = &recs[i];
            Body {
                reference: format!("{}#{}", all[rec.layer].id, rec.index),
                digest: rec.digest.clone(),
                record: Some(rec.index),
                text: rec.raw.to_string(),
            }
        }
        None => {
            let layer = &all[item.layer];
            Body {
                reference: layer.id.clone(),
                digest: layer.id.clone(),
                record: None,
                text: layer_summary(layer),
            }
        }
    }
}

fn clip(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

fn optional_integer(object: &Map<String, Value>, key: &str) -> Result<Option<usize>> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => {
            let number = value
                .as_u64()
                .with_context(|| format!("{key} must be a non-negative integer"))?;
            let number = usize::try_from(number).with_context(|| format!("{key} is too large"))?;
            Ok(Some(number))
        }
    }
}

fn integer(object: &Map<String, Value>, key: &str, default: usize) -> Result<usize> {
    Ok(optional_integer(object, key)?.unwrap_or(default))
}

struct ViewArgs<'a> {
    query: &'a str,
    layer: Option<&'a str>,
    record: Option<usize>,
    offset: usize,
    start: usize,
    limit: usize,
    image: bool,
}

impl<'a> ViewArgs<'a> {
    fn parse(args: &'a Value) -> Result<Self> {
        let object = args
            .as_object()
            .context("layer view arguments must be an object")?;
        if let Some(key) = object
            .keys()
            .find(|key| !VIEW_FIELDS.contains(&key.as_str()))
        {
            bail!("unknown snapcompact view argument {key}");
        }
        ensure!(
            object.get("view").and_then(Value::as_str) == Some("layers"),
            "snapcompact view must be \"layers\""
        );
        let query = object
            .get("query")
            .context("snapcompact view requires a query string")?
            .as_str()
            .context("query must be a string")?;
        ensure!(
            query.len() <= MAX_QUERY_BYTES,
            "query exceeds {MAX_QUERY_BYTES} bytes"
        );
        let layer = match object.get("layer") {
            None | Some(Value::Null) => None,
            Some(value) => Some(value.as_str().context("layer must be a string id")?),
        };
        let record = optional_integer(object, "record")?;
        if record.is_some() {
            ensure!(layer.is_some(), "record selection requires a layer id");
            ensure!(
                query.trim().is_empty(),
                "record selection requires an empty query"
            );
        }
        let limit = integer(object, "limit", DEFAULT_LIMIT)?;
        ensure!(
            (1..=MAX_LIMIT).contains(&limit),
            "limit must be between 1 and {MAX_LIMIT}"
        );
        let image = match object.get("format") {
            None | Some(Value::Null) => false,
            Some(Value::String(format)) if format == "text" => false,
            Some(Value::String(format)) if format == "image" => true,
            Some(_) => bail!("format must be text or image"),
        };
        Ok(Self {
            query,
            layer,
            record,
            offset: integer(object, "offset", 0)?,
            start: integer(object, "start", 0)?,
            limit,
            image,
        })
    }
}

fn search_terms(query: &str) -> Vec<String> {
    query.split_whitespace().map(str::to_lowercase).collect()
}

/// Bounded layer view. `view` must be `"layers"`. Without a query and without `layer` it lists
/// layers. With `layer` and an empty query it returns that layer's records (or its source text
/// when it has no messages). With `record` (zero-based; requires `layer` and an empty query) it
/// returns exactly that record. With a query it returns records whose canonical JSON contains
/// every term, then records linked to them by a shared tool-call id or a shared path/reference
/// token.
///
/// Message records are returned as their exact canonical JSON; the readable transcript is only
/// produced by [`readable`]. `offset` selects the item index, `start` is a byte offset into the
/// first returned item only (it must be a UTF-8 boundary), and `limit` caps items per call. The
/// text ends with a visible `next:` cursor (or `null`), totals and a history caution; `data`
/// repeats the cursor and item spans. Output text never exceeds 16 KiB; the last
/// [`FOOTER_BYTES`] are reserved for that footer.
pub fn view(messages: &[Value], args: &Value) -> Result<ToolOutput> {
    let args = ViewArgs::parse(args)?;
    let all = layers(messages)?;
    let scope: Vec<usize> = match args.layer {
        Some(id) => {
            let position = all
                .iter()
                .position(|layer| layer.id == id)
                .with_context(|| format!("unknown snapcompact layer {id}"))?;
            vec![position]
        }
        None => (0..all.len()).collect(),
    };
    let terms = search_terms(args.query);
    let mut recs = Vec::new();
    let (mode, items): (&str, Vec<Item>) = if let Some(index) = args.record {
        for &position in &scope {
            recs.extend(records(position, &all[position])?);
        }
        ensure!(
            index < recs.len(),
            "record {index} is beyond layer {} ({} records)",
            args.layer.unwrap_or(""),
            recs.len()
        );
        let item = Item {
            kind: "record",
            layer: recs[index].layer,
            rec: Some(index),
            reasons: Vec::new(),
        };
        ("record", vec![item])
    } else if !terms.is_empty() {
        for &position in &scope {
            recs.extend(records(position, &all[position])?);
        }
        ("query", match_items(&recs, &terms))
    } else if args.layer.is_some() {
        for &position in &scope {
            recs.extend(records(position, &all[position])?);
        }
        let items = (0..recs.len())
            .map(|i| Item {
                kind: "record",
                layer: recs[i].layer,
                rec: Some(i),
                reasons: Vec::new(),
            })
            .collect();
        ("layer", items)
    } else {
        let items = scope
            .iter()
            .map(|&position| Item {
                kind: "layer",
                layer: position,
                rec: None,
                reasons: Vec::new(),
            })
            .collect();
        ("index", items)
    };

    let body_budget = MAX_OUTPUT_BYTES - FOOTER_BYTES;
    let mut text = String::new();
    let mut emitted = Vec::new();
    let mut next: Option<(usize, usize)> = None;
    let mut position = args.offset;
    let mut start = args.start;
    while position < items.len() {
        if emitted.len() == args.limit {
            next = Some((position, 0));
            break;
        }
        let item = &items[position];
        let body = body(&all, &recs, item);
        ensure!(
            start <= body.text.len(),
            "start {start} is beyond the selected item ({} bytes)",
            body.text.len()
        );
        ensure!(
            body.text.is_char_boundary(start),
            "start {start} is not a UTF-8 character boundary"
        );
        let reasons = clip(&item.reasons.join("; "), 512);
        let suffix = if reasons.is_empty() {
            String::new()
        } else {
            format!(" reasons: {reasons}")
        };
        let reserve = 192 + body.reference.len() + body.digest.len() + suffix.len();
        let room = body_budget.saturating_sub(text.len() + reserve);
        let mut end = body.text.len().min(start.saturating_add(room));
        while end > start && !body.text.is_char_boundary(end) {
            end -= 1;
        }
        if end == start && start < body.text.len() {
            next = Some((position, start));
            break;
        }
        writeln!(
            text,
            "[{} {} digest={} bytes {start}..{end} of {}{suffix}]",
            body.reference,
            item.kind,
            body.digest,
            body.text.len()
        )?;
        text.push_str(&body.text[start..end]);
        text.push('\n');
        let more = end < body.text.len();
        emitted.push(json!({
            "kind": item.kind,
            "ref": body.reference,
            "layer": all[item.layer].id,
            "record": body.record,
            "reasons": item.reasons,
            "digest": body.digest,
            "start": start,
            "end": end,
            "total_bytes": body.text.len(),
            "more": more,
        }));
        if more {
            next = Some((position, end));
            break;
        }
        position += 1;
        start = 0;
    }
    if emitted.is_empty() {
        text.push_str("no snapcompact records selected\n");
    }
    let cursor = next.map_or(
        Value::Null,
        |(offset, start)| json!({"offset": offset, "start": start}),
    );
    let more = !cursor.is_null();
    writeln!(text, "next: {cursor}")?;
    writeln!(
        text,
        "shown {} of {} items from {} layers; coverage {}",
        emitted.len(),
        items.len(),
        all.len(),
        if more {
            "partial, continue at next"
        } else {
            "complete"
        }
    )?;
    text.push_str(
        "history: archived compaction history, not current workspace state; verify against current files before acting\nRelated records are lexical links, not proof of a relationship; expansion uses at most 8 reference tokens and 8 reasons per record.\n",
    );
    ensure!(
        text.len() <= MAX_OUTPUT_BYTES,
        "snapcompact view output exceeds {MAX_OUTPUT_BYTES} bytes"
    );
    let format = if args.image { "image" } else { "text" };
    let data = json!({
        "view": "layers",
        "mode": mode,
        "format": format,
        "items": emitted,
        "items_total": items.len(),
        "layers_total": all.len(),
        "more": more,
        "next": cursor,
    });
    Ok(ToolOutput {
        text,
        is_error: false,
        data: Some(data),
        execution: Default::default(),
    })
}

/// Adds the layer-view properties to a tool parameters object (`function.parameters`). Existing
/// properties are kept exactly (for example an existing `limit` default or a session_search
/// `query`), and `required` is never changed.
pub fn schema(base: &mut Value) {
    if !base.is_object() {
        *base = json!({"type": "object", "properties": {}});
    }
    let Some(object) = base.as_object_mut() else {
        return;
    };
    let properties = object.entry("properties").or_insert_with(|| json!({}));
    let Some(properties) = properties.as_object_mut() else {
        return;
    };
    let fields = [
        (
            "view",
            json!({"type": "string", "enum": ["layers"], "description": "Required; must be \"layers\" to read archived compaction layers."}),
        ),
        (
            "query",
            json!({"type": "string", "description": "Terms to find in archived layers; empty with no layer lists the layers."}),
        ),
        (
            "layer",
            json!({"type": "string", "description": "Layer id to read exactly."}),
        ),
        (
            "record",
            json!({"type": "integer", "minimum": 0, "description": "Zero-based record index within `layer`; requires layer and an empty query."}),
        ),
        (
            "offset",
            json!({"type": "integer", "minimum": 0, "description": "Item cursor from the previous next.offset."}),
        ),
        (
            "start",
            json!({"type": "integer", "minimum": 0, "description": "Byte offset into the first returned item; must be a UTF-8 boundary."}),
        ),
        (
            "limit",
            json!({"type": "integer", "minimum": 1, "maximum": MAX_LIMIT, "default": DEFAULT_LIMIT, "description": "Items per call."}),
        ),
        (
            "format",
            json!({"type": "string", "enum": ["text", "image"], "default": "text"}),
        ),
    ];
    for (name, definition) in fields {
        properties.entry(name).or_insert(definition);
    }
}
