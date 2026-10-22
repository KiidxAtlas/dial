//! Connect-protocol framing and protobuf wire helpers shared by the native Connect transports.

use anyhow::{Context, Result, bail, ensure};
use std::io::{Read, Write};

pub(super) const FLAG_COMPRESSED: u8 = 0x01;
pub(super) const FLAG_END_STREAM: u8 = 0x02;
pub(super) const MAX_FRAME_PAYLOAD: usize = 16 * 1024 * 1024;

/// Splits a Connect byte stream into `(flags, payload)` frames. Frames are capped before buffering.
#[derive(Default)]
pub(super) struct Frames {
    buf: Vec<u8>,
}

impl Frames {
    pub(super) fn push(&mut self, bytes: &[u8]) -> Result<Vec<(u8, Vec<u8>)>> {
        self.buf.extend_from_slice(bytes);
        let mut out = Vec::new();
        while self.buf.len() >= 5 {
            let len =
                u32::from_be_bytes([self.buf[1], self.buf[2], self.buf[3], self.buf[4]]) as usize;
            ensure!(
                len <= MAX_FRAME_PAYLOAD,
                "Connect frame length {len} exceeds {MAX_FRAME_PAYLOAD}-byte cap"
            );
            if self.buf.len() < 5 + len {
                break;
            }
            let flags = self.buf[0];
            let payload = self.buf[5..5 + len].to_vec();
            self.buf.drain(..5 + len);
            out.push((flags, payload));
        }
        Ok(out)
    }
}

pub(super) fn frame(flags: u8, payload: &[u8]) -> Result<Vec<u8>> {
    let len = u32::try_from(payload.len()).context("Connect frame exceeds 4 GiB")?;
    let mut out = Vec::with_capacity(5 + payload.len());
    out.push(flags);
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(payload);
    Ok(out)
}

pub(super) fn gzip(data: &[u8]) -> Result<Vec<u8>> {
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(data)?;
    Ok(encoder.finish()?)
}

pub(super) fn gunzip(data: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(data).read_to_end(&mut out)?;
    Ok(out)
}

/// Protobuf wire-format encoder for request bodies.
#[derive(Default)]
pub(super) struct Enc(pub(super) Vec<u8>);

impl Enc {
    pub(super) fn varint(&mut self, mut value: u64) {
        while value >= 0x80 {
            self.0.push((value as u8) | 0x80);
            value >>= 7;
        }
        self.0.push(value as u8);
    }

    pub(super) fn key(&mut self, field: u32, wire: u64) {
        self.varint((u64::from(field) << 3) | wire);
    }

    pub(super) fn uint(&mut self, field: u32, value: u64) {
        if value != 0 {
            self.key(field, 0);
            self.varint(value);
        }
    }

    pub(super) fn flag(&mut self, field: u32, value: bool) {
        self.uint(field, u64::from(value));
    }

    pub(super) fn double(&mut self, field: u32, value: f64) {
        if value != 0.0 {
            self.key(field, 1);
            self.0.extend_from_slice(&value.to_le_bytes());
        }
    }

    pub(super) fn string(&mut self, field: u32, value: &str) {
        if !value.is_empty() {
            self.bytes(field, value.as_bytes());
        }
    }

    /// Presence-tracked `optional string`: emitted whenever set, even when empty.
    pub(super) fn optional_string(&mut self, field: u32, value: Option<&str>) {
        if let Some(value) = value {
            self.bytes(field, value.as_bytes());
        }
    }

    pub(super) fn bytes(&mut self, field: u32, value: &[u8]) {
        self.key(field, 2);
        self.varint(value.len() as u64);
        self.0.extend_from_slice(value);
    }

    pub(super) fn message(&mut self, field: u32, value: &Enc) {
        self.bytes(field, &value.0);
    }
}

pub(super) enum Wire<'a> {
    Varint(u64),
    Fixed64(u64),
    Bytes(&'a [u8]),
    Fixed32,
}

/// Decoded protobuf fields of one message, in wire order.
#[derive(Default)]
pub(super) struct Fields<'a>(pub(super) Vec<(u32, Wire<'a>)>);

impl<'a> Fields<'a> {
    pub(super) fn parse(mut data: &'a [u8]) -> Result<Self> {
        let mut out = Vec::new();
        while !data.is_empty() {
            let key = take_varint(&mut data)?;
            let field = u32::try_from(key >> 3).context("protobuf field number out of range")?;
            let wire = match key & 7 {
                0 => Wire::Varint(take_varint(&mut data)?),
                1 => {
                    let raw: [u8; 8] = take(&mut data, 8)?.try_into()?;
                    Wire::Fixed64(u64::from_le_bytes(raw))
                }
                2 => {
                    let len = usize::try_from(take_varint(&mut data)?)
                        .context("protobuf length overflow")?;
                    Wire::Bytes(take(&mut data, len)?)
                }
                5 => {
                    take(&mut data, 4)?;
                    Wire::Fixed32
                }
                other => bail!("unsupported protobuf wire type {other}"),
            };
            out.push((field, wire));
        }
        Ok(Self(out))
    }

    pub(super) fn bytes_all(&self, field: u32) -> impl Iterator<Item = &'a [u8]> + '_ {
        self.0.iter().filter_map(move |(number, wire)| match wire {
            Wire::Bytes(bytes) if *number == field => Some(*bytes),
            _ => None,
        })
    }

    pub(super) fn bytes(&self, field: u32) -> Option<&'a [u8]> {
        self.bytes_all(field).last()
    }

    pub(super) fn message(&self, field: u32) -> Result<Option<Fields<'a>>> {
        self.bytes(field).map(Fields::parse).transpose()
    }

    pub(super) fn messages(&self, field: u32) -> Result<Vec<Fields<'a>>> {
        self.bytes_all(field).map(Fields::parse).collect()
    }

    pub(super) fn string(&self, field: u32) -> String {
        self.bytes(field)
            .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
            .unwrap_or_default()
    }

    pub(super) fn uint(&self, field: u32) -> u64 {
        self.uint_opt(field).unwrap_or(0)
    }

    /// Preserve field presence for partial provider-usage accounting.
    pub(super) fn uint_opt(&self, field: u32) -> Option<u64> {
        self.0
            .iter()
            .filter_map(|(number, wire)| match wire {
                Wire::Varint(value) if *number == field => Some(*value),
                _ => None,
            })
            .next_back()
    }

    /// Last fixed64 value of `field` as a double (`double` in the Devin schema); `0.0` when absent.
    pub(super) fn fixed64_f64(&self, field: u32) -> f64 {
        self.0
            .iter()
            .filter_map(|(number, wire)| match wire {
                Wire::Fixed64(bits) if *number == field => Some(f64::from_bits(*bits)),
                _ => None,
            })
            .next_back()
            .unwrap_or(0.0)
    }
}

pub(super) fn take<'a>(data: &mut &'a [u8], len: usize) -> Result<&'a [u8]> {
    let current: &'a [u8] = data;
    ensure!(current.len() >= len, "truncated protobuf message");
    let (head, tail) = current.split_at(len);
    *data = tail;
    Ok(head)
}

pub(super) fn take_varint(data: &mut &[u8]) -> Result<u64> {
    let mut value = 0u64;
    for shift in (0..64).step_by(7) {
        let current: &[u8] = data;
        let (&byte, rest) = current.split_first().context("truncated protobuf varint")?;
        *data = rest;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    bail!("protobuf varint exceeds 64 bits")
}

#[cfg(test)]
mod tests;
