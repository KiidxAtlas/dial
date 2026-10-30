//! Workbook inspection observes formulas and cached values without evaluating either.
use super::office::{self, Package, Xml};
use serde_json::{Value, json};
use std::{collections::BTreeSet, fs::File};

const MAX_SHEETS: usize = 100;
const MAX_FORMULAS: usize = 1000;

pub(super) fn inspect(file: File) -> Result<String, String> {
    let mut package = Package::open(file)?;
    let rels = office::relationships(&package.xml("xl/_rels/workbook.xml.rels")?, "xl")?;
    let mut sheets = Vec::new();
    let mut names = BTreeSet::new();
    office::xml(&package.xml("xl/workbook.xml")?, "workbook", |event| {
        if let Xml::Start(name, attrs) = event
            && name == "sheet"
        {
            let name = office::attr(&attrs, "name").ok_or("sheet missing name")?;
            if name.is_empty() || !names.insert(name.to_owned()) || sheets.len() >= MAX_SHEETS {
                return Err(
                    "workbook requires unique nonempty sheet names, at most 100 sheets".into(),
                );
            }
            sheets.push((
                name.to_owned(),
                office::relationship_id(&attrs)?.to_owned(),
                office::attr(&attrs, "state")
                    .unwrap_or("visible")
                    .to_owned(),
            ));
        }
        Ok(())
    })?;
    let mut output = Vec::new();
    let mut formulas = 0;
    for (name, id, state) in sheets {
        let target = rels
            .get(&id)
            .ok_or("missing or external worksheet relationship")?;
        if !target.starts_with("xl/worksheets/") || !target.ends_with(".xml") {
            return Err("inspection supports internal worksheet targets only".into());
        }
        let mut sheet = inspect_sheet(&package.xml(target)?, &mut formulas)?;
        sheet["name"] = json!(name);
        sheet["state"] = json!(state);
        sheet["part"] = json!(target);
        output.push(sheet);
    }
    let result = json!({"sheets":output,"formula_count":formulas,"coverage":"all declared internal worksheets within explicit parser limits; not complete workbook semantics","limitations":["Formula expressions, attributes and cached lexical values are observations, never executed or trusted as calculated results. Shared/array formulas are not expanded.","Row counts count explicit XML row elements, including blank rows, not business records. Headers and units must be established separately.","No macros, external links, chart interpretation, style/date rendering or merged-cell reconstruction. Hidden sheet state is reported; hidden rows are not excluded."]});
    let text = serde_json::to_string(&result).map_err(|e| e.to_string())?;
    if text.len() as u64 > super::MAX_CONTENT {
        return Err("workbook inspection exceeds 8 MiB".into());
    }
    Ok(text)
}

fn inspect_sheet(input: &str, total_formulas: &mut usize) -> Result<Value, String> {
    let mut rows = 0;
    let mut cells = 0;
    let mut row_index = 0usize;
    let mut last_column = 0usize;
    let mut in_row = false;
    let mut cell: Option<(String, String)> = None;
    let mut formula: Option<Value> = None;
    let mut formula_text = String::new();
    let mut cached = String::new();
    let mut capture_formula = false;
    let mut capture_value = false;
    let mut formulas = Vec::new();
    let mut errors = Vec::new();
    let mut types = std::collections::BTreeMap::<String, usize>::new();
    office::xml(input, "worksheet", |event| {
        match event {
            Xml::Start(name, attrs) if name == "row" => {
                let index = office::attr(&attrs, "r")
                    .ok_or("row missing address")?
                    .parse::<usize>()
                    .map_err(|_| "invalid row address")?;
                if in_row || index <= row_index || index > 1_048_576 || rows >= 10_000 {
                    return Err("worksheet requires ordered unique rows, at most 10000".into());
                }
                rows += 1;
                row_index = index;
                last_column = 0;
                in_row = true;
            }
            Xml::Start(name, attrs) if name == "c" => {
                if !in_row || cell.is_some() || cells >= 1_000_000 {
                    return Err("nested/outside cell or more than 1000000 cells".into());
                }
                let address = office::attr(&attrs, "r").ok_or("cell missing address")?;
                let letters = address.bytes().take_while(u8::is_ascii_uppercase).count();
                let (column, suffix) = address.split_at(letters);
                let mut index = 0usize;
                for byte in column.bytes() {
                    index = index
                        .checked_mul(26)
                        .and_then(|n| n.checked_add((byte - b'A' + 1) as usize))
                        .ok_or("column overflow")?;
                }
                if index == 0
                    || index > 128
                    || index <= last_column
                    || suffix.parse::<usize>().ok() != Some(row_index)
                {
                    return Err(
                        "cell address must be ordered, unique, in its row and within 128 columns"
                            .into(),
                    );
                }
                last_column = index;
                cells += 1;
                let kind = office::attr(&attrs, "t").unwrap_or("n").to_owned();
                *types.entry(kind.clone()).or_default() += 1;
                cell = Some((address.to_owned(), kind));
                formula = None;
                formula_text.clear();
                cached.clear();
            }
            Xml::Start(name, attrs) if name == "f" => {
                if cell.is_none() || formula.is_some() || *total_formulas >= MAX_FORMULAS {
                    return Err(
                        "formula outside cell, duplicate formula or more than 1000 formulas".into(),
                    );
                }
                *total_formulas += 1;
                formula = Some(
                    json!({"attributes":attrs.into_iter().collect::<std::collections::BTreeMap<_,_>>()}),
                );
                capture_formula = true;
            }
            Xml::Start(name, _) if name == "v" => {
                if cell.is_none() {
                    return Err("cached value outside cell".into());
                }
                capture_value = true;
            }
            Xml::Text(text) if capture_formula => formula_text.push_str(&text),
            Xml::Text(text) if capture_value => cached.push_str(&text),
            Xml::End(name) if name == "f" => capture_formula = false,
            Xml::End(name) if name == "v" => capture_value = false,
            Xml::End(name) if name == "c" => {
                let (address, kind) = cell.take().ok_or("cell end without start")?;
                if let Some(mut entry) = formula.take() {
                    entry["cell"] = json!(address);
                    entry["expression"] = json!(formula_text);
                    entry["cached_lexical_value"] = json!(cached);
                    entry["cached_value_verified"] = json!(false);
                    formulas.push(entry);
                }
                if kind == "e" {
                    if errors.len() >= 1000 {
                        return Err("more than 1000 error cells".into());
                    }
                    errors.push(json!({"cell":address,"lexical_value":cached}));
                }
            }
            Xml::End(name) if name == "row" => {
                if cell.is_some() || !in_row {
                    return Err("unbalanced worksheet row".into());
                }
                in_row = false;
            }
            _ => {}
        }
        Ok(())
    })?;
    Ok(
        json!({"explicit_rows":rows,"explicit_cells":cells,"cell_types":types,"formulas":formulas,"error_cells":errors}),
    )
}

#[cfg(test)]
mod tests;
