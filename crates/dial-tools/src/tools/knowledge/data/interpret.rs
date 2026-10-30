//! Explicit, bounded interpretation declarations; metadata never authenticates human approval.
use super::{Args, MAX_COLUMNS, query, table::Table};
use anyhow::{Context, Result, bail, ensure};
use bigdecimal::BigDecimal;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::str::FromStr;

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Origin {
    CallerDeclared,
    ModelInferred,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Dictionary {
    origin: Origin,
    row_meaning: String,
    columns: BTreeMap<String, Column>,
}
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Column {
    meaning: String,
    unit: Option<String>,
    #[serde(default)]
    missing_values: Vec<Value>,
    nullable: Option<bool>,
    identifier: Option<bool>,
    unique: Option<bool>,
    min: Option<Value>,
    max: Option<Value>,
    allowed_values: Option<Vec<Value>>,
}

pub(super) fn dictionary(table: &Table, dictionary: &Dictionary) -> Result<Value> {
    ensure!(
        dictionary.row_meaning.len() <= 2048 && dictionary.columns.len() <= MAX_COLUMNS,
        "dictionary exceeds 2048-byte row meaning or 128 columns"
    );
    let mut observations = BTreeMap::new();
    for (name, column) in &dictionary.columns {
        ensure!(
            table.columns.contains(name),
            "unknown dictionary column {name}"
        );
        ensure!(
            column.meaning.len() <= 2048 && column.unit.as_ref().is_none_or(|u| u.len() <= 128),
            "dictionary text exceeds bounds"
        );
        ensure!(
            column.missing_values.len() <= 16
                && column
                    .allowed_values
                    .as_ref()
                    .is_none_or(|a| a.len() <= 100),
            "dictionary permits at most 16 missing and 100 allowed values"
        );
        for value in column
            .missing_values
            .iter()
            .chain(column.allowed_values.iter().flatten())
        {
            ensure!(
                !value.is_array() && !value.is_object() && value.to_string().len() <= 2048,
                "dictionary constraints require bounded scalar values"
            );
        }
        let min = column.min.as_ref().map(query::number).transpose()?;
        let max = column.max.as_ref().map(query::number).transpose()?;
        ensure!(
            min.zip(max).is_none_or(|(a, b)| a <= b),
            "dictionary min exceeds max"
        );
        let mut missing = Vec::new();
        let mut violations = Vec::new();
        let mut values = BTreeMap::<String, Vec<usize>>::new();
        for (id, row) in &table.rows {
            let value = row.get(name).unwrap_or(&Value::Null);
            let absent = value.is_null() || column.missing_values.contains(value);
            if absent {
                missing.push(*id);
            }
            if !absent && (column.identifier == Some(true) || column.unique == Some(true)) {
                values
                    .entry(serde_json::to_string(value)?)
                    .or_default()
                    .push(*id);
            }
            let invalid = if absent {
                column.nullable == Some(false) || column.identifier == Some(true)
            } else {
                let range_invalid = if min.is_some() || max.is_some() {
                    match query::number(value) {
                        Ok(n) => min.is_some_and(|m| n < m) || max.is_some_and(|m| n > m),
                        Err(_) => true,
                    }
                } else {
                    false
                };
                range_invalid
                    || column
                        .allowed_values
                        .as_ref()
                        .is_some_and(|allowed| !allowed.contains(value))
            };
            if invalid {
                violations.push(*id);
            }
        }
        let duplicates = values
            .into_values()
            .filter(|ids| ids.len() > 1)
            .collect::<Vec<_>>();
        violations.extend(duplicates.iter().flatten().copied());
        violations.sort_unstable();
        violations.dedup();
        observations.insert(
            name,
            json!({"declared_missing_row_ids":missing,"duplicate_value_row_id_groups":duplicates,"constraint_violation_row_ids":violations,"identifier_semantics":"identifier implies nonmissing and unique; typed lexical JSON equality, no coercion"}),
        );
    }
    Ok(
        json!({"declaration":dictionary,"observations":observations,"scope":"all available source rows before filtering/window; declarations do not recode values or establish truth","human_approval":"not_authenticated","confidence":"not_inferred"}),
    )
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Conversion {
    column: String,
    output: String,
    from: String,
    to: String,
}
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct DateColumn {
    column: String,
    output: String,
    format: DateFormat,
}
#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum DateFormat {
    Date,
    Rfc3339,
}

// Scale and offset to the dimension's base unit. No currency, contextual units or guessed units.
fn unit(name: &str) -> Result<(&'static str, f64, f64)> {
    Ok(match name {
        "m" => ("length", 1.0, 0.0),
        "km" => ("length", 1000.0, 0.0),
        "cm" => ("length", 0.01, 0.0),
        "mm" => ("length", 0.001, 0.0),
        "in" => ("length", 0.0254, 0.0),
        "ft" => ("length", 0.3048, 0.0),
        "mi" => ("length", 1609.344, 0.0),
        "kg" => ("mass", 1.0, 0.0),
        "g" => ("mass", 0.001, 0.0),
        "mg" => ("mass", 0.000001, 0.0),
        "lb" => ("mass", 0.45359237, 0.0),
        "s" => ("duration", 1.0, 0.0),
        "ms" => ("duration", 0.001, 0.0),
        "min" => ("duration", 60.0, 0.0),
        "h" => ("duration", 3600.0, 0.0),
        "K" => ("temperature", 1.0, 0.0),
        "degC" => ("temperature", 1.0, 273.15),
        "degF" => ("temperature", 5.0 / 9.0, 273.15 - 32.0 * 5.0 / 9.0),
        _ => bail!(
            "unsupported unit {name}; supported: m km cm mm in ft mi kg g mg lb s ms min h K degC degF"
        ),
    })
}
fn output_column(table: &Table, input: &str, output: &str) -> Result<()> {
    ensure!(
        table.columns.contains(&input.to_owned()),
        "unknown interpretation column {input}"
    );
    ensure!(
        !output.is_empty() && output.len() <= 256 && !table.columns.contains(&output.to_owned()),
        "interpretation output must be a new nonempty column (maximum 256 bytes)"
    );
    ensure!(
        table.columns.len() < MAX_COLUMNS,
        "derived columns exceed 128-column bound"
    );
    Ok(())
}

pub(super) fn apply(table: &mut Table, args: &Args) -> Result<()> {
    ensure!(
        args.conversions.len() <= 16 && args.dates.len() <= 16,
        "maximum 16 conversions and 16 date declarations"
    );
    for conversion in &args.conversions {
        output_column(table, &conversion.column, &conversion.output)?;
        let (a, scale_a, offset_a) = unit(&conversion.from)?;
        let (b, scale_b, offset_b) = unit(&conversion.to)?;
        ensure!(
            a == b,
            "incompatible unit dimensions: {} and {}",
            conversion.from,
            conversion.to
        );
        for (id, row) in &mut table.rows {
            let value = row.get(&conversion.column).unwrap_or(&Value::Null);
            let converted = if value.is_null() {
                Value::Null
            } else {
                let n =
                    query::number(value).with_context(|| format!("unit conversion row {id}"))?;
                let n = (n * scale_a + offset_a - offset_b) / scale_b;
                ensure!(
                    n.is_finite() && n.abs() <= 9007199254740992.0,
                    "unit conversion overflow at row {id}"
                );
                json!(n)
            };
            row.insert(conversion.output.clone(), converted);
        }
        table.columns.push(conversion.output.clone());
    }
    for date in &args.dates {
        output_column(table, &date.column, &date.output)?;
        let format = time::format_description::parse_borrowed::<2>("[year]-[month]-[day]")?;
        for (id, row) in &mut table.rows {
            let value = row.get(&date.column).unwrap_or(&Value::Null);
            let parsed = if value.is_null() {
                Value::Null
            } else {
                let text = value.as_str().context("date input must be a string")?;
                match date.format {
                    DateFormat::Date => {
                        ensure!(
                            text.len() == 10,
                            "date requires exact YYYY-MM-DD at row {id}"
                        );
                        let date = time::Date::parse(text, &format)
                            .with_context(|| format!("invalid date at row {id}"))?;
                        json!(date.to_string())
                    }
                    DateFormat::Rfc3339 => {
                        // -00:00 explicitly means unknown local offset, not UTC.
                        ensure!(
                            !text.ends_with("-00:00"),
                            "unknown RFC3339 offset -00:00 is not an instant"
                        );
                        ensure!(
                            !text.contains(":60"),
                            "leap seconds are unsupported; no silent normalization"
                        );
                        let instant = time::OffsetDateTime::parse(
                            text,
                            &time::format_description::well_known::Rfc3339,
                        )
                        .with_context(|| {
                            format!("RFC3339 requires explicit Z or numeric offset at row {id}")
                        })?;
                        json!(instant.unix_timestamp_nanos() as f64 / 1_000_000_000.0)
                    }
                }
            };
            row.insert(date.output.clone(), parsed);
        }
        table.columns.push(date.output.clone());
    }
    Ok(())
}

pub(super) fn decimal_stats(values: &[&Value]) -> Result<Value> {
    let mut sum = BigDecimal::from(0);
    let mut count = 0;
    let mut distinct = BTreeSet::new();
    for value in values {
        if value.is_null() {
            continue;
        }
        let text = match value {
            Value::String(text) => text.clone(),
            Value::Number(n) => n.to_string(),
            _ => bail!("exact decimal requires decimal strings or lexical JSON numbers"),
        };
        ensure!(text.len() <= 128, "decimal input exceeds 128 bytes");
        let (mantissa, exponent) = text.split_once(['e', 'E']).unwrap_or((&text, "0"));
        let exponent: i32 = exponent.parse().context("invalid decimal exponent")?;
        ensure!(
            (-100..=100).contains(&exponent),
            "decimal exponent must be -100..=100"
        );
        ensure!(
            mantissa
                .split_once('.')
                .is_none_or(|(_, fraction)| fraction.len() <= 100),
            "decimal scale exceeds 100 places"
        );
        // A lexical grammar guard prevents BigDecimal accepting whitespace or non-decimal spellings.
        let unsigned = mantissa
            .strip_prefix('-')
            .or_else(|| mantissa.strip_prefix('+'))
            .unwrap_or(mantissa);
        ensure!(
            !unsigned.is_empty()
                && unsigned.bytes().all(|b| b.is_ascii_digit() || b == b'.')
                && unsigned.bytes().filter(|b| *b == b'.').count() <= 1
                && unsigned.bytes().any(|b| b.is_ascii_digit()),
            "invalid decimal spelling"
        );
        let number = BigDecimal::from_str(&text).context("invalid exact decimal")?;
        distinct.insert(number.normalized().to_string());
        sum += number;
        count += 1;
    }
    Ok(
        json!({"count":values.len(),"null":values.len()-count,"decimal_count":count,"distinct":distinct.len(),"exact_sum":if count == 0 { None } else { Some(sum.normalized().to_string()) },"arithmetic":"exact bounded base-10 sum; string result; no rounded f64 statistics or mean"}),
    )
}
