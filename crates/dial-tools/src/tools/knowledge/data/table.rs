use super::{MAX_COLUMNS, MAX_ROWS};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Map, Value};
use std::collections::BTreeSet;
use std::sync::atomic::AtomicBool;

pub(super) struct Table {
    pub columns: Vec<String>,
    pub rows: Vec<(usize, Map<String, Value>)>,
    pub sheet: Option<String>,
}

pub(super) fn parse_with_paths(
    bytes: &[u8],
    format: &str,
    sheet: Option<&str>,
    pointers: bool,
    cancel: &AtomicBool,
) -> Result<Table> {
    if format != "jsonl" && !(format == "json" && pointers) {
        return parse(bytes, format, sheet, cancel);
    }
    super::check_cancel(cancel)?;
    ensure!(sheet.is_none(), "sheet is only valid for XLSX");
    let records: Vec<(usize, Value)> = if format == "jsonl" {
        let text = std::str::from_utf8(bytes).context("JSONL must be UTF-8")?;
        let mut records = Vec::new();
        for (line, text) in text.lines().enumerate() {
            super::check_cancel(cancel)?;
            if text.trim().is_empty() {
                continue;
            }
            ensure!(records.len() < MAX_ROWS, "table exceeds {MAX_ROWS} rows");
            records.push((
                line + 1,
                serde_json::from_str(text).with_context(|| format!("JSONL line {}", line + 1))?,
            ));
        }
        records
    } else {
        let rows: Vec<Value> =
            serde_json::from_slice(bytes).context("JSON table must be an array")?;
        ensure!(rows.len() <= MAX_ROWS, "table exceeds {MAX_ROWS} rows");
        rows.into_iter()
            .enumerate()
            .map(|(i, v)| (i + 1, v))
            .collect()
    };
    let mut columns = BTreeSet::new();
    let mut rows = Vec::new();
    for (id, value) in records {
        super::check_cancel(cancel)?;
        let object = value
            .as_object()
            .context("every JSON/JSONL row must be an object")?;
        let mut flat = Map::new();
        if pointers {
            flatten(&value, "", 0, &mut flat)?;
        } else {
            for (key, value) in object {
                ensure!(
                    !key.is_empty() && !value.is_object() && !value.is_array(),
                    "nested JSON requires json_pointers:true; column names must be nonempty"
                );
                flat.insert(key.clone(), value.clone());
            }
        }
        columns.extend(flat.keys().cloned());
        ensure!(
            columns.len() <= MAX_COLUMNS,
            "table exceeds {MAX_COLUMNS} columns"
        );
        rows.push((id, flat));
    }
    Ok(Table {
        columns: columns.into_iter().collect(),
        rows,
        sheet: None,
    })
}

fn flatten(value: &Value, path: &str, depth: usize, out: &mut Map<String, Value>) -> Result<()> {
    ensure!(depth <= 32, "nested JSON exceeds depth 32");
    match value {
        Value::Object(object) => {
            ensure!(
                path.is_empty() || !object.is_empty(),
                "empty nested objects have no scalar columns"
            );
            for (key, value) in object {
                let escaped = key.replace('~', "~0").replace('/', "~1");
                flatten(value, &format!("{path}/{escaped}"), depth + 1, out)?;
            }
        }
        Value::Array(array) => {
            ensure!(
                !array.is_empty(),
                "empty nested arrays have no scalar columns"
            );
            for (index, value) in array.iter().enumerate() {
                flatten(value, &format!("{path}/{index}"), depth + 1, out)?;
            }
        }
        _ => {
            out.insert(path.to_owned(), value.clone());
        }
    }
    ensure!(
        out.len() <= MAX_COLUMNS,
        "table exceeds {MAX_COLUMNS} columns"
    );
    Ok(())
}

pub(super) fn parse(
    bytes: &[u8],
    format: &str,
    sheet: Option<&str>,
    cancel: &AtomicBool,
) -> Result<Table> {
    super::check_cancel(cancel)?;
    match format {
        "json" => {
            ensure!(sheet.is_none(), "sheet is only valid for XLSX");
            let value: Value = serde_json::from_slice(bytes).context("parsing JSON table")?;
            let rows = value
                .as_array()
                .context("JSON table must be an array of flat objects")?;
            ensure!(rows.len() <= MAX_ROWS, "table exceeds {MAX_ROWS} rows");
            let mut columns = BTreeSet::new();
            let mut result = Vec::new();
            for (i, row) in rows.iter().enumerate() {
                super::check_cancel(cancel)?;
                let row = row
                    .as_object()
                    .context("every JSON row must be an object")?;
                for (key, value) in row {
                    ensure!(!key.is_empty(), "empty column name");
                    ensure!(
                        !value.is_array() && !value.is_object(),
                        "nested JSON values are unsupported (row {}, column {key})",
                        i + 1
                    );
                    columns.insert(key.clone());
                    ensure!(
                        columns.len() <= MAX_COLUMNS,
                        "table exceeds {MAX_COLUMNS} columns"
                    );
                }
                result.push((i + 1, row.clone()));
            }
            Ok(Table {
                columns: columns.into_iter().collect(),
                rows: result,
                sheet: None,
            })
        }
        "csv" => {
            ensure!(sheet.is_none(), "sheet is only valid for XLSX");
            let mut reader = csv::ReaderBuilder::new().flexible(false).from_reader(bytes);
            let headers = reader
                .headers()
                .context("CSV header")?
                .iter()
                .map(str::to_owned)
                .collect::<Vec<_>>();
            validate_headers(&headers)?;
            let mut rows = Vec::new();
            for record in reader.records() {
                super::check_cancel(cancel)?;
                ensure!(rows.len() < MAX_ROWS, "table exceeds {MAX_ROWS} rows");
                let record =
                    record.context("CSV record (quoted and multiline fields supported)")?;
                let row = headers
                    .iter()
                    .cloned()
                    .zip(record.iter().map(csv_value))
                    .collect();
                rows.push((rows.len() + 1, row));
            }
            Ok(Table {
                columns: headers,
                rows,
                sheet: None,
            })
        }
        "xlsx" => {
            let value = crate::tools::fs::documents::table_bytes(bytes, sheet, cancel)
                .map_err(anyhow::Error::msg)?;
            let sheet = value["sheet"]
                .as_str()
                .context("XLSX missing sheet identity")?
                .to_owned();
            let rows = value["rows"].as_array().context("XLSX missing rows")?;
            let header = rows.first().context("XLSX worksheet has no header")?;
            let headers = header["cells"]
                .as_array()
                .context("XLSX missing header cells")?
                .iter()
                .map(|v| {
                    v.as_str()
                        .context("XLSX headers must be nonempty strings")
                        .map(str::to_owned)
                })
                .collect::<Result<Vec<_>>>()?;
            validate_headers(&headers)?;
            ensure!(
                rows.len() - 1 <= MAX_ROWS,
                "table exceeds {MAX_ROWS} data rows"
            );
            let mut result = Vec::new();
            for row in rows.iter().skip(1) {
                super::check_cancel(cancel)?;
                let id: usize = row["row"]
                    .as_u64()
                    .context("XLSX missing worksheet row number")?
                    .try_into()?;
                let cells = row["cells"].as_array().context("XLSX missing cells")?;
                ensure!(
                    cells.len() <= headers.len(),
                    "XLSX row {id} extends beyond header columns"
                );
                let values = headers
                    .iter()
                    .enumerate()
                    .map(|(i, name)| (name.clone(), cells.get(i).cloned().unwrap_or(Value::Null)))
                    .collect();
                result.push((id, values));
            }
            Ok(Table {
                columns: headers,
                rows: result,
                sheet: Some(sheet),
            })
        }
        _ => bail!("format must be json, jsonl, csv or xlsx (XLS/ODS are unsupported; export CSV)"),
    }
}

fn validate_headers(headers: &[String]) -> Result<()> {
    ensure!(
        !headers.is_empty() && headers.len() <= MAX_COLUMNS,
        "table requires 1..={MAX_COLUMNS} columns"
    );
    let unique = headers.iter().collect::<BTreeSet<_>>();
    ensure!(
        unique.len() == headers.len() && headers.iter().all(|s| !s.is_empty()),
        "column headers must be nonempty and unique"
    );
    Ok(())
}

fn csv_value(s: &str) -> Value {
    if s.is_empty() {
        Value::Null
    } else {
        // Preserve ordinary labels (including booleans and leading-zero identifiers) as strings.
        match serde_json::from_str::<Value>(s) {
            Ok(Value::Number(n)) => Value::Number(n),
            _ => Value::String(s.into()),
        }
    }
}
