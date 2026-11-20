//! `readMemory` and `disassemble`: argument validation and a readable rendering of raw memory.
//!
//! DAP returns memory as base64; the tool replaces it with an addressed hex/ASCII dump so the
//! model can read bytes without decoding them itself.
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Map, Value, json};

/// Most bytes one `readMemory` call requests. A dump is about four characters per byte, so this
/// keeps one answer inside the tool's inline output bound.
pub(super) const MAX_MEMORY_BYTES: u64 = 4096;
/// Most instructions one `disassemble` call requests.
pub(super) const MAX_INSTRUCTIONS: u64 = 256;
/// Bytes per dump line.
const LINE: usize = 16;
/// Longest accepted memory reference; adapters issue short opaque strings or addresses.
const MAX_REFERENCE: usize = 256;

fn memory_reference(args: &Value) -> Result<String, String> {
    let reference = args
        .get("memoryReference")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|reference| !reference.is_empty())
        .ok_or("memoryReference is required: use a memoryReference from evaluate, variables, stackTrace (instructionPointerReference) or disassemble")?;
    if reference.len() > MAX_REFERENCE {
        return Err(format!(
            "memoryReference is longer than {MAX_REFERENCE} bytes"
        ));
    }
    Ok(reference.to_owned())
}

fn integer(args: &Value, key: &str) -> Result<Option<i64>, String> {
    match args.get(key).filter(|value| !value.is_null()) {
        None => Ok(None),
        Some(value) => value
            .as_i64()
            .map(Some)
            .ok_or_else(|| format!("{key} must be an integer")),
    }
}

fn bounded_count(args: &Value, key: &str, max: u64) -> Result<u64, String> {
    let count = integer(args, key)?.ok_or_else(|| format!("{key} is required"))?;
    u64::try_from(count)
        .ok()
        .filter(|count| (1..=max).contains(count))
        .ok_or_else(|| format!("{key} must be between 1 and {max}"))
}

/// Validated `readMemory` arguments: `memoryReference`, `count` (1..=4096) and optional `offset`.
pub(super) fn read_memory_arguments(args: &Value) -> Result<Value, String> {
    let mut result = Map::new();
    result.insert("memoryReference".into(), json!(memory_reference(args)?));
    result.insert(
        "count".into(),
        json!(bounded_count(args, "count", MAX_MEMORY_BYTES)?),
    );
    if let Some(offset) = integer(args, "offset")? {
        result.insert("offset".into(), json!(offset));
    }
    Ok(Value::Object(result))
}

/// Validated `disassemble` arguments: `memoryReference`, `instructionCount` (1..=256) and optional
/// `offset`, `instructionOffset` and `resolveSymbols`.
pub(super) fn disassemble_arguments(args: &Value) -> Result<Value, String> {
    let mut result = Map::new();
    result.insert("memoryReference".into(), json!(memory_reference(args)?));
    result.insert(
        "instructionCount".into(),
        json!(bounded_count(args, "instructionCount", MAX_INSTRUCTIONS)?),
    );
    for key in ["offset", "instructionOffset"] {
        if let Some(value) = integer(args, key)? {
            result.insert(key.into(), json!(value));
        }
    }
    if let Some(flag) = args.get("resolveSymbols").filter(|flag| !flag.is_null()) {
        if !flag.is_boolean() {
            return Err("resolveSymbols must be boolean".into());
        }
        result.insert("resolveSymbols".into(), flag.clone());
    }
    Ok(Value::Object(result))
}

/// Replaces the base64 `data` of a `readMemory` response with `bytes` (the count read) and `hex`
/// (dump lines addressed from the response `address`). A body without `data` (nothing readable)
/// is returned unchanged.
pub(super) fn render_memory(mut body: Value) -> Result<Value, String> {
    let Some(object) = body.as_object_mut() else {
        return Ok(body);
    };
    let Some(data) = object.get("data").and_then(Value::as_str) else {
        return Ok(body);
    };
    let bytes = STANDARD
        .decode(data)
        .map_err(|error| format!("adapter returned invalid base64 memory data: {error}"))?;
    let base = object
        .get("address")
        .and_then(Value::as_str)
        .and_then(parse_address);
    object.remove("data");
    object.insert("bytes".into(), json!(bytes.len()));
    object.insert("hex".into(), json!(dump(base, &bytes)));
    Ok(body)
}

/// Parses a DAP address: hexadecimal with a `0x` prefix, otherwise decimal.
fn parse_address(text: &str) -> Option<u64> {
    let text = text.trim();
    match text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        Some(hex) => u64::from_str_radix(hex, 16).ok(),
        None => text.parse().ok(),
    }
}

/// One line per 16 bytes: address (or `+offset` when the adapter gave no parseable address),
/// hex bytes, and printable ASCII with `.` for other bytes.
fn dump(base: Option<u64>, bytes: &[u8]) -> Vec<String> {
    bytes
        .chunks(LINE)
        .enumerate()
        .map(|(index, chunk)| {
            let offset = (index * LINE) as u64;
            let address = match base {
                Some(base) => format!("0x{:016x}", base.wrapping_add(offset)),
                None => format!("+0x{offset:04x}"),
            };
            let hex: Vec<String> = chunk.iter().map(|byte| format!("{byte:02x}")).collect();
            let ascii: String = chunk
                .iter()
                .map(|&byte| {
                    if byte.is_ascii_graphic() || byte == b' ' {
                        byte as char
                    } else {
                        '.'
                    }
                })
                .collect();
            format!("{address}  {:<47}  |{ascii}|", hex.join(" "))
        })
        .collect()
}

#[cfg(test)]
mod tests;
