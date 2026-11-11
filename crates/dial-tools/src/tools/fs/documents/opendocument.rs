//! OpenDocument text and presentations (`.odt`, `.odp`): paragraph and heading text from
//! `content.xml`. Embedded objects, macros and images are ignored.

use super::office::Xml;
use std::io::{Cursor, Read};

#[cfg(test)]
pub(super) mod tests;

/// Largest decompressed `content.xml` accepted.
const MAX_XML: u64 = 64 * 1024 * 1024;

pub(super) fn read(bytes: &[u8]) -> Result<String, String> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes))
        .map_err(|e| format!("invalid OpenDocument: {e}"))?;
    let mut xml = Vec::new();
    archive
        .by_name("content.xml")
        .map_err(|_| "OpenDocument has no content.xml")?
        .take(MAX_XML + 1)
        .read_to_end(&mut xml)
        .map_err(|e| format!("reading content.xml: {e}"))?;
    if xml.len() as u64 > MAX_XML {
        return Err("OpenDocument content.xml exceeds 64 MiB".into());
    }
    text(&xml)
}

/// Text of `text:p`/`text:h` elements, one per line; `text:s`, `text:tab` and `text:line-break`
/// become spaces, tabs and newlines. Slides (`draw:page`) are labeled. Parsing goes through the
/// shared Office XML reader, which forbids DTDs and bounds depth and event count.
fn text(xml: &[u8]) -> Result<String, String> {
    let xml = std::str::from_utf8(xml).map_err(|_| "content.xml is not UTF-8")?;
    let mut out = String::new();
    let mut depth = 0usize;
    let mut slide = 0usize;
    super::office::xml(xml, "document-content", |event| {
        match event {
            Xml::Start(name, _) => match name.as_str() {
                "p" | "h" => depth += 1,
                "page" => {
                    slide += 1;
                    out.push_str(&format!("\n[slide {slide}]\n"));
                }
                "s" if depth > 0 => out.push(' '),
                "tab" if depth > 0 => out.push('\t'),
                "line-break" if depth > 0 => out.push('\n'),
                _ => {}
            },
            Xml::Text(text) if depth > 0 => out.push_str(&text),
            Xml::End(name) if matches!(name.as_str(), "p" | "h") && depth > 0 => {
                depth -= 1;
                if depth == 0 {
                    out.push('\n');
                }
            }
            _ => {}
        }
        Ok(())
    })?;
    Ok(out)
}
