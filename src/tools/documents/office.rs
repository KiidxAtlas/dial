//! Bounded read-only OOXML extraction. Package relationships never reach the filesystem or network.
use super::{MAX_CONTENT, bounded, text};
use quick_xml::{Reader, events::Event};
use std::{
    collections::{HashMap, HashSet},
    fs::File,
};

const MAX_ENTRIES: usize = 10_000;
const MAX_DECODED: u64 = 128 * 1024 * 1024;
const MAX_ITEMS: usize = 100_000;
pub(super) struct Package {
    zip: zip::ZipArchive<File>,
    decoded: u64,
}
impl Package {
    pub(super) fn open(file: File) -> Result<Self, String> {
        let mut zip =
            zip::ZipArchive::new(file).map_err(|e| format!("invalid document ZIP: {e}"))?;
        if zip.len() > MAX_ENTRIES {
            return Err("document package exceeds 10000 members".into());
        }
        let mut total = 0u64;
        let mut names = HashSet::new();
        for i in 0..zip.len() {
            let member = zip
                .by_index(i)
                .map_err(|e| format!("reading document directory: {e}"))?;
            let name = member
                .name()
                .map_err(|e| format!("unreadable document member name: {e}"))?;
            if !super::archives::safe(&name) || !names.insert(name.to_string()) {
                return Err(format!("unsafe or duplicate document member {name:?}"));
            }
            if member.unix_mode().is_some_and(|m| m & 0o170000 == 0o120000) {
                return Err("document symlink members are rejected".into());
            }
            total = total
                .checked_add(member.size())
                .ok_or("document declared size overflow")?;
            if total > MAX_DECODED {
                return Err("document declared expansion exceeds 128 MiB".into());
            }
        }
        Ok(Self { zip, decoded: 0 })
    }
    pub(super) fn has(&self, name: &str) -> bool {
        self.zip.file_names().any(|n| n.is_ok_and(|n| n == name))
    }
    pub(super) fn xml(&mut self, name: &str) -> Result<String, String> {
        if !super::archives::safe(name) {
            return Err("unsafe document part path".into());
        }
        let part = self
            .zip
            .by_name(name)
            .map_err(|e| format!("document part {name:?}: {e}"))?;
        if !part.is_file() || part.size() > MAX_CONTENT {
            return Err(format!(
                "document part {name:?} is nonregular or exceeds 8 MiB"
            ));
        }
        let bytes = bounded(part)?;
        self.decoded = self
            .decoded
            .checked_add(bytes.len() as u64)
            .ok_or("document decoded size overflow")?;
        if self.decoded > MAX_CONTENT {
            return Err(
                "document parts exceed 8 MiB decoded budget; select a smaller part, for example :chapter=N, :sheet=N or :slide=N"
                    .into(),
            );
        }
        text(bytes)
    }
}
fn append(out: &mut String, value: &str) -> Result<(), String> {
    if out.len().saturating_add(value.len()) > MAX_CONTENT as usize {
        return Err("extracted text exceeds 8 MiB".into());
    }
    out.push_str(value);
    Ok(())
}
pub(super) enum Xml {
    Start(String, Vec<(String, String)>),
    End(String),
    Text(String),
}
pub(super) fn xml(
    input: &str,
    root: &str,
    mut visit: impl FnMut(Xml) -> Result<(), String>,
) -> Result<(), String> {
    let mut reader = Reader::from_str(input);
    reader.config_mut().expand_empty_elements = true;
    reader.config_mut().enable_all_checks(true);
    let mut depth = 0usize;
    let mut count = 0usize;
    let mut roots = 0usize;
    loop {
        count += 1;
        if count > 1_000_000 {
            return Err("document XML exceeds 1000000 events".into());
        }
        match reader
            .read_event()
            .map_err(|e| format!("malformed document XML: {e}"))?
        {
            Event::Start(start) => {
                let name = std::str::from_utf8(start.local_name().as_ref())
                    .map_err(|_| "non-UTF8 document element")?
                    .to_string();
                if depth == 0 {
                    roots += 1;
                    if roots != 1 || name != root {
                        return Err(format!("document XML expected single {root:?} root"));
                    }
                }
                depth += 1;
                if depth > 128 {
                    return Err("document XML exceeds nesting depth 128".into());
                }
                let mut attributes = Vec::new();
                for attr in start.attributes() {
                    let attr = attr.map_err(|e| format!("malformed document attribute: {e}"))?;
                    let name = std::str::from_utf8(attr.key.as_ref())
                        .map_err(|_| "non-UTF8 document attribute")?
                        .to_string();
                    let value = attr
                        .decoded_and_normalized_value(
                            quick_xml::XmlVersion::Implicit1_0,
                            reader.decoder(),
                        )
                        .map_err(|e| format!("invalid document attribute entity: {e}"))?
                        .into_owned();
                    attributes.push((name, value));
                }
                visit(Xml::Start(name, attributes))?;
            }
            Event::End(end) => {
                depth = depth.checked_sub(1).ok_or("unbalanced document XML")?;
                let name = std::str::from_utf8(end.local_name().as_ref())
                    .map_err(|_| "non-UTF8 document element")?
                    .to_string();
                visit(Xml::End(name))?;
            }
            Event::Text(value) => {
                let decoded = value
                    .decode()
                    .map_err(|e| format!("invalid document text: {e}"))?;
                let value = quick_xml::escape::unescape(&decoded)
                    .map_err(|e| format!("unsupported document entity: {e}"))?
                    .into_owned();
                if depth == 0 && !value.trim().is_empty() {
                    return Err("document text outside root".into());
                }
                visit(Xml::Text(value))?;
            }
            Event::CData(value) => {
                if depth == 0 {
                    return Err("document CDATA outside root".into());
                }
                visit(Xml::Text(
                    value
                        .decode()
                        .map_err(|e| format!("invalid document CDATA: {e}"))?
                        .into_owned(),
                ))?;
            }
            Event::GeneralRef(value) => {
                if depth == 0 {
                    return Err("document entity outside root".into());
                }
                let name = value
                    .decode()
                    .map_err(|e| format!("invalid document entity: {e}"))?;
                let entity = format!("&{name};");
                let decoded = quick_xml::escape::unescape(&entity)
                    .map_err(|e| format!("unsupported document entity: {e}"))?;
                visit(Xml::Text(decoded.into_owned()))?;
            }
            Event::DocType(value) => {
                if value.contains(&b'[') {
                    return Err("document DTDs and external entities are forbidden".into());
                }
            }
            Event::Eof => {
                if roots != 1 || depth != 0 {
                    return Err("incomplete document XML".into());
                }
                break;
            }
            _ => {}
        }
    }
    Ok(())
}
pub(super) fn attr<'a>(attrs: &'a [(String, String)], name: &str) -> Option<&'a str> {
    attrs
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.as_str())
}
fn relationship_id(attrs: &[(String, String)]) -> Result<&str, String> {
    let mut ids = attrs
        .iter()
        .filter(|(key, _)| key.split_once(':').is_some_and(|(_, local)| local == "id"));
    let (_, value) = ids
        .next()
        .ok_or("OOXML missing namespace-qualified relationship id")?;
    if ids.next().is_some() {
        return Err("ambiguous OOXML relationship id".into());
    }
    Ok(value)
}
fn number(selector: Option<&str>, key: &str) -> Result<Option<usize>, String> {
    selector
        .map(|s| {
            let n: usize = s
                .strip_prefix(key)
                .ok_or_else(|| format!("OOXML selector must be :{key}N (1-based)"))?
                .parse()
                .map_err(|_| "invalid OOXML part number")?;
            if !(1..=MAX_ITEMS).contains(&n) {
                return Err("OOXML part number must be 1..100000".into());
            }
            Ok(n)
        })
        .transpose()
}
fn paragraphs(input: &str, root: &str) -> Result<String, String> {
    let mut out = String::new();
    let mut in_text = false;
    xml(input, root, |event| {
        match event {
            Xml::Start(name, _) => match name.as_str() {
                "t" => in_text = true,
                "tab" => append(&mut out, "\t")?,
                "br" | "cr" => append(&mut out, "\n")?,
                _ => {}
            },
            Xml::Text(value) if in_text => append(&mut out, &value)?,
            Xml::End(name) => match name.as_str() {
                "t" => in_text = false,
                "p" => append(&mut out, "\n")?,
                "tc" => append(&mut out, "\t")?,
                "tr" => append(&mut out, "\n")?,
                _ => {}
            },
            _ => {}
        }
        Ok(())
    })?;
    Ok(out)
}
fn relationships(input: &str, base: &str) -> Result<HashMap<String, String>, String> {
    let mut out = HashMap::new();
    xml(input, "Relationships", |event| {
        if let Xml::Start(name, attrs) = event
            && name == "Relationship"
        {
            let id = attr(&attrs, "Id").ok_or("OOXML relationship missing Id")?;
            let target = attr(&attrs, "Target").ok_or("OOXML relationship missing Target")?;
            if attr(&attrs, "TargetMode") == Some("External") {
                return Ok(());
            }
            let target = if let Some(target) = target.strip_prefix('/') {
                target.to_string()
            } else {
                format!("{base}/{target}")
            };
            if !super::archives::safe(&target) {
                return Err("unsafe OOXML relationship target".into());
            }
            if out.insert(id.to_string(), target).is_some() {
                return Err("duplicate OOXML relationship Id".into());
            }
        }
        Ok(())
    })?;
    Ok(out)
}
fn shared_strings(input: &str) -> Result<Vec<String>, String> {
    let mut strings = Vec::new();
    let mut current = String::new();
    let mut in_text = false;
    let mut in_phonetic = false;
    xml(input, "sst", |event| {
        match event {
            Xml::Start(name, _) if name == "si" => current.clear(),
            Xml::Start(name, _) if name == "rPh" => in_phonetic = true,
            Xml::Start(name, _) if name == "t" && !in_phonetic => in_text = true,
            Xml::Text(value) if in_text => append(&mut current, &value)?,
            Xml::End(name) if name == "t" => in_text = false,
            Xml::End(name) if name == "rPh" => in_phonetic = false,
            Xml::End(name) if name == "si" => {
                if strings.len() >= MAX_ITEMS {
                    return Err("OOXML exceeds 100000 shared strings".into());
                }
                strings.push(std::mem::take(&mut current));
            }
            _ => {}
        }
        Ok(())
    })?;
    Ok(strings)
}
fn worksheet(input: &str, strings: &[String]) -> Result<String, String> {
    let mut out = String::new();
    let (mut reference, mut kind, mut value, mut formula) =
        (String::new(), String::new(), String::new(), String::new());
    let (mut in_value, mut in_formula, mut in_inline) = (false, false, false);
    let mut in_phonetic = false;
    let mut cells = 0usize;
    xml(input, "worksheet", |event| {
        match event {
            Xml::Start(name, attrs) if name == "row" => {
                let row = attr(&attrs, "r").unwrap_or("unspecified");
                append(&mut out, &format!("[row {row}]\n"))?;
            }
            Xml::Start(name, attrs) if name == "c" => {
                cells += 1;
                if cells > MAX_ITEMS {
                    return Err("OOXML worksheet exceeds 100000 cells".into());
                }
                reference = attr(&attrs, "r").unwrap_or("unspecified").into();
                kind = attr(&attrs, "t").unwrap_or("n").into();
                value.clear();
                formula.clear();
            }
            Xml::Start(name, _) if name == "v" => in_value = true,
            Xml::Start(name, _) if name == "f" => in_formula = true,
            Xml::Start(name, _) if name == "rPh" => in_phonetic = true,
            Xml::Start(name, _) if name == "t" && kind == "inlineStr" && !in_phonetic => {
                in_inline = true
            }
            Xml::Text(text) if in_formula => append(&mut formula, &text)?,
            Xml::Text(text) if in_value || in_inline => append(&mut value, &text)?,
            Xml::End(name) if name == "v" => in_value = false,
            Xml::End(name) if name == "f" => in_formula = false,
            Xml::End(name) if name == "t" => in_inline = false,
            Xml::End(name) if name == "rPh" => in_phonetic = false,
            Xml::End(name) if name == "c" => {
                let resolved = if kind == "s" {
                    let index: usize = value
                        .parse()
                        .map_err(|_| "invalid OOXML shared string index")?;
                    strings
                        .get(index)
                        .ok_or("OOXML shared string index out of range")?
                        .as_str()
                } else {
                    &value
                };
                append(
                    &mut out,
                    &format!(
                        "{reference}({kind})={}\n",
                        serde_json::to_string(resolved).map_err(|e| e.to_string())?
                    ),
                )?;
                if !formula.is_empty() {
                    append(
                        &mut out,
                        &format!(
                            "[formula {}; cached value only; not executed]\n",
                            serde_json::to_string(&formula).map_err(|e| e.to_string())?
                        ),
                    )?;
                }
            }
            Xml::End(name) if name == "row" => append(&mut out, "\n")?,
            _ => {}
        }
        Ok(())
    })?;
    Ok(out)
}
pub(super) fn read(file: File, format: &str, selector: Option<&str>) -> Result<String, String> {
    let mut package = Package::open(file)?;
    match format {
        "docx" => {
            if selector.is_some() {
                return Err(
                    "DOCX supports extracted-text line selectors only, not a part selector".into(),
                );
            }
            let input = package.xml("word/document.xml")?;
            let mut out = "[DOCX main document; paragraphs and tables; no embedded objects/macros executed]\n".to_string();
            append(&mut out, &paragraphs(&input, "document")?)?;
            Ok(out)
        }
        "xlsx" => {
            let selected = number(selector, "sheet=")?;
            let workbook = package.xml("xl/workbook.xml")?;
            let rels = package.xml("xl/_rels/workbook.xml.rels")?;
            let rels = relationships(&rels, "xl")?;
            let mut sheets = Vec::new();
            xml(&workbook, "workbook", |event| {
                if let Xml::Start(name, attrs) = event
                    && name == "sheet"
                {
                    if sheets.len() >= MAX_ITEMS {
                        return Err("OOXML exceeds 100000 sheets".into());
                    }
                    sheets.push((
                        attr(&attrs, "name")
                            .ok_or("OOXML sheet missing name")?
                            .to_string(),
                        relationship_id(&attrs)?.to_string(),
                    ));
                }
                Ok(())
            })?;
            if selected.is_some_and(|n| n > sheets.len()) {
                return Err(format!("sheet outside 1..={}", sheets.len()));
            }
            let strings = if package.has("xl/sharedStrings.xml") {
                shared_strings(&package.xml("xl/sharedStrings.xml")?)?
            } else {
                Vec::new()
            };
            let mut out = "[XLSX workbook; row/cell addresses preserved; numeric/date values are cached lexical values, not style-rendered]\n".to_string();
            for (i, (name, id)) in sheets.iter().enumerate() {
                if selected.is_some_and(|n| n != i + 1) {
                    continue;
                }
                let target = rels
                    .get(id)
                    .ok_or("missing or external OOXML worksheet relationship")?;
                if !target.starts_with("xl/worksheets/") || !target.ends_with(".xml") {
                    return Err("OOXML worksheet relationship must name xl/worksheets/*.xml".into());
                }
                append(&mut out, &format!("[sheet {}: {name}]\n", i + 1))?;
                append(&mut out, &worksheet(&package.xml(target)?, &strings)?)?;
            }
            Ok(out)
        }
        "pptx" => {
            let selected = number(selector, "slide=")?;
            let presentation = package.xml("ppt/presentation.xml")?;
            let rels = relationships(&package.xml("ppt/_rels/presentation.xml.rels")?, "ppt")?;
            let mut slides = Vec::new();
            xml(&presentation, "presentation", |event| {
                if let Xml::Start(name, attrs) = event
                    && name == "sldId"
                {
                    if slides.len() >= MAX_ITEMS {
                        return Err("OOXML exceeds 100000 slides".into());
                    }
                    slides.push(relationship_id(&attrs)?.to_string());
                }
                Ok(())
            })?;
            if selected.is_some_and(|n| n > slides.len()) {
                return Err(format!("slide outside 1..={}", slides.len()));
            }
            let mut out = "[PPTX presentation; slide order preserved; text and table cells only, no embedded objects executed]\n".to_string();
            for (i, id) in slides.iter().enumerate() {
                if selected.is_some_and(|n| n != i + 1) {
                    continue;
                }
                let target = rels
                    .get(id)
                    .ok_or("missing or external OOXML slide relationship")?;
                if !target.starts_with("ppt/slides/") || !target.ends_with(".xml") {
                    return Err("OOXML slide relationship must name ppt/slides/*.xml".into());
                }
                append(&mut out, &format!("[slide {}]\n", i + 1))?;
                append(&mut out, &paragraphs(&package.xml(target)?, "sld")?)?;
            }
            Ok(out)
        }
        _ => unreachable!(),
    }
}
