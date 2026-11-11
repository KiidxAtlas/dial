//! Apache Parquet and Arrow IPC (Feather v2) files: schema, file metadata and a page of rows.
//! Rows are selected with `:rows=O+N` (zero-based offset, default `0+100`).

use arrow_array::{Array, RecordBatch, RecordBatchReader};
use arrow_cast::display::{ArrayFormatter, FormatOptions};
use arrow_schema::{DataType, Schema};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use serde_json::{Map, Value};
use std::{
    fmt::Write as _,
    io::{Seek, Write},
};

#[cfg(test)]
mod tests;

const DEFAULT_ROWS: usize = 100;
const MAX_ROWS: usize = 10_000;
/// Characters kept from one cell value.
const MAX_CELL: usize = 2_000;

fn rows(member: Option<&str>) -> Result<(usize, usize), String> {
    let Some(member) = member else {
        return Ok((0, DEFAULT_ROWS));
    };
    let usage = || format!("columnar selector must be :rows=O or :rows=O+N (N <= {MAX_ROWS})");
    let spec = member.strip_prefix("rows=").ok_or_else(usage)?;
    let (offset, count) = spec.split_once('+').unwrap_or((spec, "100"));
    let offset = offset.parse().map_err(|_| usage())?;
    let count: usize = count.parse().map_err(|_| usage())?;
    if count == 0 || count > MAX_ROWS {
        return Err(usage());
    }
    Ok((offset, count))
}

fn spool(bytes: &[u8]) -> Result<std::fs::File, String> {
    let mut file = tempfile::tempfile().map_err(|e| format!("creating parser input: {e}"))?;
    file.write_all(bytes)
        .and_then(|()| file.rewind())
        .map_err(|e| format!("writing parser input: {e}"))?;
    Ok(file)
}

pub(super) fn parquet(bytes: &[u8], member: Option<&str>) -> Result<String, String> {
    let (offset, count) = rows(member)?;
    let builder = ParquetRecordBatchReaderBuilder::try_new(spool(bytes)?)
        .map_err(|e| format!("invalid Parquet file: {e}"))?;
    let metadata = builder.metadata().clone();
    let file = metadata.file_metadata();
    let total = usize::try_from(file.num_rows()).unwrap_or(0);
    let mut out = format!(
        "Parquet: {total} rows; {} row groups; format version {}",
        metadata.num_row_groups(),
        file.version()
    );
    if let Some(by) = file.created_by() {
        let _ = write!(out, "; created by {by}");
    }
    out.push('\n');
    if let Some(group) = metadata.row_groups().first()
        && let Some(column) = group.columns().first()
    {
        let _ = writeln!(
            out,
            "Compression (first column chunk): {:?}",
            column.compression()
        );
    }
    schema(&mut out, builder.schema());
    if let Some(pairs) = file.key_value_metadata() {
        let keys: Vec<&str> = pairs.iter().map(|kv| kv.key.as_str()).collect();
        let _ = writeln!(out, "Key-value metadata keys: {}", keys.join(", "));
    }
    let reader = builder
        .with_offset(offset)
        .with_limit(count)
        .build()
        .map_err(|e| format!("reading Parquet rows: {e}"))?;
    let batches = reader
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("reading Parquet rows: {e}"))?;
    page(&mut out, &batches, 0, count, offset, total)?;
    Ok(out)
}

pub(super) fn arrow(bytes: &[u8], member: Option<&str>) -> Result<String, String> {
    let (offset, count) = rows(member)?;
    let (label, reader): (&str, Box<dyn RecordBatchReader>) =
        match arrow_ipc::reader::FileReader::try_new(spool(bytes)?, None) {
            Ok(reader) => ("Arrow IPC file", Box::new(reader)),
            Err(_) => (
                "Arrow IPC stream",
                Box::new(
                    arrow_ipc::reader::StreamReader::try_new(spool(bytes)?, None)
                        .map_err(|e| format!("invalid Arrow IPC data: {e}"))?,
                ),
            ),
        };
    let schema_ref = reader.schema();
    let batches = reader
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("reading Arrow batches: {e}"))?;
    let total: usize = batches.iter().map(RecordBatch::num_rows).sum();
    let mut out = format!("{label}: {total} rows; {} record batches\n", batches.len());
    schema(&mut out, &schema_ref);
    page(&mut out, &batches, offset, count, offset, total)?;
    Ok(out)
}

fn schema(out: &mut String, schema: &Schema) {
    let _ = writeln!(out, "Schema ({} columns):", schema.fields().len());
    for field in schema.fields() {
        let _ = writeln!(
            out,
            "  {}: {}{}",
            field.name(),
            field.data_type(),
            if field.is_nullable() {
                ""
            } else {
                " (not null)"
            }
        );
    }
}

/// Renders rows `skip..skip+count` of `batches` as one JSON object per line. `first` is the
/// absolute index of the first rendered row.
fn page(
    out: &mut String,
    batches: &[RecordBatch],
    mut skip: usize,
    count: usize,
    first: usize,
    total: usize,
) -> Result<(), String> {
    let options = FormatOptions::default().with_null("null");
    let mut shown = 0;
    let mut lines = String::new();
    for batch in batches {
        if shown == count {
            break;
        }
        if skip >= batch.num_rows() {
            skip -= batch.num_rows();
            continue;
        }
        let schema = batch.schema();
        let formatters = batch
            .columns()
            .iter()
            .map(|column| ArrayFormatter::try_new(column.as_ref(), &options))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("formatting rows: {e}"))?;
        for row in skip..batch.num_rows() {
            if shown == count {
                break;
            }
            let mut object = Map::new();
            for (index, field) in schema.fields().iter().enumerate() {
                let column = batch.column(index);
                let value = if column.is_null(row) {
                    Value::Null
                } else {
                    let text = formatters[index].value(row).to_string();
                    let numeric = field.data_type().is_numeric()
                        || matches!(field.data_type(), DataType::Boolean);
                    match numeric
                        .then(|| serde_json::from_str::<Value>(&text).ok())
                        .flatten()
                    {
                        Some(value @ (Value::Number(_) | Value::Bool(_))) => value,
                        _ => Value::String(text.chars().take(MAX_CELL).collect()),
                    }
                };
                object.insert(field.name().clone(), value);
            }
            lines.push_str(&Value::Object(object).to_string());
            lines.push('\n');
            shown += 1;
        }
        skip = 0;
    }
    let end = first + shown;
    let _ = writeln!(out, "[rows {first}..{end} of {total}]");
    out.push_str(&lines);
    if end < total {
        let _ = writeln!(out, "[more rows: :rows={end}+{count}]");
    }
    Ok(())
}
