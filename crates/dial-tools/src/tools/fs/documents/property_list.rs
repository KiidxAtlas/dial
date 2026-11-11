//! Binary property lists (`bplist00`) converted to the equivalent XML plist, as
//! `plutil -convert xml1` would print them.

use std::io::Cursor;

#[cfg(test)]
mod tests;

pub(super) fn read(bytes: &[u8]) -> Result<String, String> {
    let value =
        plist::Value::from_reader(Cursor::new(bytes)).map_err(|e| format!("invalid plist: {e}"))?;
    let mut xml = Vec::new();
    value
        .to_writer_xml(&mut xml)
        .map_err(|e| format!("converting plist: {e}"))?;
    let mut text = String::from_utf8(xml).map_err(|_| "plist XML is not UTF-8")?;
    if !text.ends_with('\n') {
        text.push('\n');
    }
    Ok(format!("[binary plist shown as XML]\n{text}"))
}
