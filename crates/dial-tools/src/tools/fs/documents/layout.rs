//! Bounded structural layout observations; no document rendering or inferred visual semantics.
use super::office::{self, Package, Xml};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    io::{Seek, Write},
};

const MAX_NODES: usize = 20_000;
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Node {
    pub order: usize,
    pub part: String,
    pub kind: String,
    pub attributes: BTreeMap<String, String>,
    pub text: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Layout {
    pub version: u32,
    pub format: String,
    pub reading_order: String,
    pub nodes: Vec<Node>,
    pub limitations: Vec<String>,
}
impl Layout {
    fn new(format: &str) -> Self {
        Self { version: 1, format: format.into(), reading_order: "package document order (not guaranteed visual or accessibility reading order)".into(), nodes: vec![], limitations: vec![
            "Only explicit main-document/slide XML is inspected; no rendering, OCR, embedded objects, headers/footers, notes, inherited styles or linked resources. No macros execute and no relationships are fetched.".into(),
            "Layout comparison concerns extracted structure/attributes, not pixel geometry or all formatting. Content comparison is exact extracted text in document order, not semantic equivalence.".into(),
        ] }
    }
    fn push(
        &mut self,
        part: &str,
        kind: &str,
        attributes: BTreeMap<String, String>,
    ) -> Result<usize, String> {
        if self.nodes.len() >= MAX_NODES {
            return Err("layout exceeds 20000 nodes; document is too complex".into());
        }
        let index = self.nodes.len();
        self.nodes.push(Node {
            order: index + 1,
            part: part.into(),
            kind: kind.into(),
            attributes,
            text: String::new(),
        });
        Ok(index)
    }
    pub fn content(&self) -> Vec<&str> {
        self.nodes
            .iter()
            .filter(|n| n.kind == "paragraph" || n.kind == "line")
            .map(|n| n.text.as_str())
            .collect()
    }
    pub fn structure(&self) -> Vec<(&str, &str, &BTreeMap<String, String>)> {
        self.nodes
            .iter()
            .map(|n| (n.part.as_str(), n.kind.as_str(), &n.attributes))
            .collect()
    }
}

pub(super) fn read(bytes: &[u8], format: &str) -> Result<String, String> {
    let mut file = tempfile::tempfile().map_err(|e| e.to_string())?;
    file.write_all(bytes).map_err(|e| e.to_string())?;
    file.rewind().map_err(|e| e.to_string())?;
    let mut package = Package::open(file)?;
    let mut out = Layout::new(format);
    match format {
        "docx" => {
            out.limitations.push("DOCX pagination is unavailable without rendering. pgSz/pgMar and explicit breaks are section declarations, not observed page boundaries. Heading/caption style-name hints are labelled heuristic; outlineLvl is explicit. Custom style inheritance is not resolved.".into());
            parse(
                &mut out,
                "word/document.xml",
                &package.xml("word/document.xml")?,
                "document",
            )?;
        }
        "pptx" => {
            out.limitations.push("PPTX coordinates are raw EMU shape-local transforms, not normalized page rectangles; group transforms and inherited slide layouts are not resolved. Shape order is XML/z-order, not guaranteed reading order. Placeholder type is explicit, caption semantics unavailable.".into());
            let rels =
                office::relationships(&package.xml("ppt/_rels/presentation.xml.rels")?, "ppt")?;
            let mut ids = Vec::new();
            office::xml(
                &package.xml("ppt/presentation.xml")?,
                "presentation",
                |event| {
                    if let Xml::Start(name, attrs) = event {
                        if name == "sldId" {
                            ids.push(office::relationship_id(&attrs)?.to_owned());
                        }
                        if name == "sldSz" {
                            out.push(
                                "ppt/presentation.xml",
                                "slide_size_emu",
                                attrs.into_iter().collect(),
                            )?;
                        }
                    }
                    Ok(())
                },
            )?;
            for (i, id) in ids.iter().enumerate() {
                let target = rels
                    .get(id)
                    .ok_or("missing or external slide relationship")?;
                if !target.starts_with("ppt/slides/") || !target.ends_with(".xml") {
                    return Err("invalid slide relationship target".into());
                }
                out.push(
                    target,
                    "slide",
                    BTreeMap::from([("number".into(), (i + 1).to_string())]),
                )?;
                parse(&mut out, target, &package.xml(target)?, "sld")?;
            }
        }
        _ => return Err("native layout supports DOCX and PPTX only".into()),
    }
    let json = serde_json::to_string(&out).map_err(|e| e.to_string())?;
    if json.len() as u64 > super::MAX_CONTENT {
        return Err("layout output exceeds 8 MiB".into());
    }
    Ok(json)
}

fn parse(out: &mut Layout, part: &str, input: &str, root: &str) -> Result<(), String> {
    let mut paragraph = None;
    let mut in_text = false;
    office::xml(input, root, |event| {
        match event {
            Xml::Start(name, attrs) => {
                match name.as_str() {
                    "p" => {
                        paragraph = Some(out.push(part, "paragraph", BTreeMap::new())?);
                    }
                    "t" => in_text = true,
                    "pStyle" | "outlineLvl" => {
                        if let Some(i) = paragraph {
                            let value = attrs
                                .iter()
                                .find(|(k, _)| k.rsplit(':').next() == Some("val"))
                                .map(|(_, v)| v.as_str())
                                .unwrap_or("unspecified");
                            out.nodes[i].attributes.insert(name.clone(), value.into());
                            let lower = value.to_ascii_lowercase();
                            if name == "outlineLvl" {
                                out.nodes[i].attributes.insert(
                                    "heading_basis".into(),
                                    "explicit outline level".into(),
                                );
                            } else if lower.starts_with("heading")
                                || lower == "title"
                                || lower == "caption"
                            {
                                out.nodes[i].attributes.insert("role_hint".into(), lower);
                                out.nodes[i].attributes.insert("role_basis".into(),"heuristic conventional style name; not resolved style semantics".into());
                            }
                        }
                    }
                    "tbl" | "tr" | "tc" | "gridSpan" | "vMerge" | "tblGrid" | "gridCol"
                    | "sectPr" | "pgSz" | "pgMar" | "sp" | "graphicFrame" | "grpSp" | "xfrm"
                    | "off" | "ext" | "chOff" | "chExt" | "ph" | "cNvPr" | "br" => {
                        out.push(part, &name, attrs.into_iter().collect())?;
                        if name == "br"
                            && let Some(i) = paragraph
                        {
                            out.nodes[i].text.push('\n');
                        }
                    }
                    "tab" => {
                        if let Some(i) = paragraph {
                            out.nodes[i].text.push('\t');
                        }
                    }
                    _ => {}
                }
            }
            Xml::Text(text) if in_text => {
                if let Some(i) = paragraph {
                    out.nodes[i].text.push_str(&text);
                }
            }
            Xml::End(name) => match name.as_str() {
                "t" => in_text = false,
                "p" => paragraph = None,
                "tbl" | "tr" | "tc" | "sectPr" | "sp" | "graphicFrame" | "grpSp" => {
                    out.push(part, &format!("end_{name}"), BTreeMap::new())?;
                }
                _ => {}
            },
            _ => {}
        }
        Ok(())
    })
}

pub(super) fn pdf_text(text: &str) -> Result<Layout, String> {
    let mut out = Layout::new("pdf");
    out.reading_order = "heuristic Poppler -layout text order".into();
    out.limitations.push("PDF page separators and whitespace-preserving lines are converter heuristics; no headings, tables, captions, bounding boxes, tagged reading order or images are recognized. Scanned pages without text may be empty; no OCR.".into());
    let pages: Vec<_> = text.split('\x0c').collect();
    for (page, body) in pages.iter().enumerate() {
        if page + 1 == pages.len() && body.is_empty() {
            break;
        }
        let part = format!("page={}", page + 1);
        out.push(&part, "page_heuristic", BTreeMap::new())?;
        for (line, text) in body.lines().enumerate() {
            let i = out.push(
                &part,
                "line",
                BTreeMap::from([("line".into(), (line + 1).to_string())]),
            )?;
            out.nodes[i].text = (*text).into();
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests;
