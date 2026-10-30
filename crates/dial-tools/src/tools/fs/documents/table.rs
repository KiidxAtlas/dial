//! Structured worksheet values, never a round trip through display text.
use super::office::{self, Package, Xml};
use serde_json::{Value, json};
use std::fs::File;

pub(super) fn read(file: File, selected: Option<&str>) -> Result<String, String> {
    let mut package = Package::open(file)?;
    let rels = office::relationships(&package.xml("xl/_rels/workbook.xml.rels")?, "xl")?;
    let mut sheets = Vec::new();
    office::xml(&package.xml("xl/workbook.xml")?, "workbook", |event| {
        if let Xml::Start(name, attrs) = event
            && name == "sheet"
        {
            sheets.push((
                office::attr(&attrs, "name")
                    .ok_or("sheet missing name")?
                    .to_owned(),
                office::relationship_id(&attrs)?.to_owned(),
            ));
        }
        Ok(())
    })?;
    let (name, id) = match selected {
        Some(name) => sheets
            .iter()
            .find(|(n, _)| n == name)
            .ok_or("worksheet name not found")?,
        None if sheets.len() == 1 => &sheets[0],
        None => {
            return Err(
                "sheet must explicitly name a worksheet when the workbook has multiple sheets"
                    .into(),
            );
        }
    };
    let target = rels
        .get(id)
        .ok_or("missing or external worksheet relationship")?;
    if !target.starts_with("xl/worksheets/") || !target.ends_with(".xml") {
        return Err("worksheet target must be xl/worksheets/*.xml".into());
    }
    let strings = if package.has("xl/sharedStrings.xml") {
        office::shared_strings(&package.xml("xl/sharedStrings.xml")?)?
    } else {
        Vec::new()
    };
    let rows = worksheet(&package.xml(target)?, &strings)?;
    let out = serde_json::to_string(&json!({"sheet":name,"rows":rows,"limitations":["No formula evaluation, date/style rendering, merged-cell expansion or hidden-row filtering; explicit lexical cell values only."]})).map_err(|e| e.to_string())?;
    if out.len() as u64 > super::MAX_CONTENT {
        return Err("structured worksheet exceeds 8 MiB".into());
    }
    Ok(out)
}

fn worksheet(input: &str, strings: &[String]) -> Result<Vec<Value>, String> {
    let mut rows = Vec::new();
    let mut cells = Vec::new();
    let mut row = 0usize;
    let mut column = None;
    let mut kind = String::new();
    let mut value = String::new();
    let mut capture = false;
    let mut phonetic = false;
    let mut total = 0usize;
    let mut row_count = 0usize;
    let mut in_row = false;
    office::xml(input, "worksheet", |event| {
        match event {
            Xml::Start(name, attrs) if name == "row" => {
                row_count += 1;
                if in_row || row_count > 10_000 { return Err("worksheet exceeds 10000 rows or has nested rows".into()); }
                in_row = true;
                let next = office::attr(&attrs, "r").ok_or("worksheet row missing explicit index")?.parse::<usize>().map_err(|_| "invalid worksheet row index")?;
                if next <= row || next > 1_048_576 || rows.len() >= 10_000 { return Err("worksheet rows must be ordered, unique and bounded to 10000 rows".into()); }
                row = next;
                cells.clear();
            }
            Xml::Start(name, attrs) if name == "c" => {
                if !in_row || column.is_some() { return Err("cell outside row or nested cell".into()); }
                total += 1;
                if total > 1_000_000 { return Err("worksheet exceeds 1000000 cells".into()); }
                let reference = office::attr(&attrs,"r").ok_or("cell missing explicit address")?;
                let letters = reference.bytes().take_while(u8::is_ascii_uppercase).count();
                let (col, suffix) = reference.split_at(letters);
                let mut index = 0usize;
                for b in col.bytes() { index = index.checked_mul(26).and_then(|n| n.checked_add((b-b'A'+1) as usize)).ok_or("cell column overflow")?; }
                if index == 0 || index > 128 || suffix.parse::<usize>().ok() != Some(row) || index <= cells.len() { return Err("cells must have ordered unique addresses in their row, with at most 128 columns".into()); }
                column = Some(index);
                kind = office::attr(&attrs,"t").unwrap_or("n").into();
                value.clear();
            }
            Xml::Start(name, _) if name == "f" => return Err("formula cells are unsupported; formulas and cached results are not reproducible source values".into()),
            Xml::Start(name, _) if name == "rPh" => phonetic = true,
            Xml::End(name) if name == "rPh" => phonetic = false,
            Xml::Start(name, _) if name == "v" || (name == "t" && kind == "inlineStr" && !phonetic) => capture = true,
            Xml::Text(text) if capture => value.push_str(&text),
            Xml::End(name) if name == "v" || name == "t" => capture = false,
            Xml::End(name) if name == "c" => {
                let index = column.take().ok_or("cell end without address")?;
                let parsed = if value.is_empty() { Value::Null } else { match kind.as_str() {
                    "s" => Value::String(strings.get(value.parse::<usize>().map_err(|_| "invalid shared string index")?).ok_or("shared string index out of range")?.clone()),
                    "inlineStr" | "str" | "d" => Value::String(value.clone()),
                    "b" => match value.as_str() { "0" => Value::Bool(false), "1" => Value::Bool(true), _ => return Err("invalid boolean cell".into()) },
                    "n" => { let n: Value = serde_json::from_str(&value).map_err(|_| "invalid numeric cell")?; if !n.is_number() { return Err("numeric cell is not a number".into()); } n },
                    "e" => return Err(format!("worksheet contains error cell: {value}")),
                    _ => return Err(format!("unsupported cell type {kind}")),
                }};
                cells.resize(index-1, Value::Null);
                cells.push(parsed);
            }
            Xml::End(name) if name == "row" => {
                in_row = false;
                if cells.iter().any(|v| !v.is_null()) { rows.push(json!({"row":row,"cells":cells})); }
            }
            _ => {}
        }
        Ok(())
    })?;
    Ok(rows)
}

#[cfg(test)]
mod tests;
