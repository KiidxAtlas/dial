use super::{Args, table::Table};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Filter {
    column: String,
    op: String,
    value: Option<Value>,
}

/// Compare the arbitrary-precision decimal spelling before f64 can round away integer units.
fn within_safe_magnitude(text: &str) -> bool {
    const BOUND: &str = "9007199254740992";
    let unsigned = text.strip_prefix('-').unwrap_or(text);
    let (mantissa, exponent) = unsigned.split_once(['e', 'E']).unwrap_or((unsigned, "0"));
    let fractional = mantissa
        .split_once('.')
        .map_or(0, |(_, fraction)| fraction.len());
    let digits = mantissa.bytes().filter(|b| *b != b'.').collect::<Vec<_>>();
    let first_nonzero = digits.iter().position(|b| *b != b'0');
    let Some(first_nonzero) = first_nonzero else {
        return true;
    };
    let digits = &digits[first_nonzero..];
    // Exponent saturation is sufficient for a magnitude comparison, without expanding zeros.
    let exponent = exponent.parse::<i64>().unwrap_or_else(|_| {
        if exponent.starts_with('-') {
            i64::MIN
        } else {
            i64::MAX
        }
    });
    let integer_digits = digits.len() as i128 + i128::from(exponent) - fractional as i128;
    match integer_digits.cmp(&(BOUND.len() as i128)) {
        std::cmp::Ordering::Less => true,
        std::cmp::Ordering::Greater => false,
        std::cmp::Ordering::Equal => {
            let prefix = digits
                .iter()
                .copied()
                .chain(std::iter::repeat(b'0'))
                .take(BOUND.len());
            match prefix.cmp(BOUND.bytes()) {
                std::cmp::Ordering::Less => true,
                std::cmp::Ordering::Greater => false,
                std::cmp::Ordering::Equal => digits.iter().skip(BOUND.len()).all(|b| *b == b'0'),
            }
        }
    }
}

pub(super) fn number(value: &Value) -> Result<f64> {
    let n = value
        .as_number()
        .context("numeric comparison requires numeric values on both sides")?;
    ensure!(
        within_safe_magnitude(&n.to_string()),
        "numeric magnitude exceeds exact f64 range (+/-2^53)"
    );
    let f = n
        .as_f64()
        .context("number cannot be represented as finite f64")?;
    ensure!(f.is_finite(), "non-finite numeric value");
    Ok(f)
}

fn matches(row: &Map<String, Value>, filter: &Filter) -> Result<bool> {
    let value = row.get(&filter.column).unwrap_or(&Value::Null);
    if filter.op == "is_null" {
        return Ok(value.is_null());
    }
    if filter.op == "not_null" {
        return Ok(!value.is_null());
    }
    let right = filter
        .value
        .as_ref()
        .context("filter comparison requires a non-null value; use is_null/not_null for null")?;
    if value.is_null() {
        return Ok(false);
    }
    match filter.op.as_str() {
        "eq" | "ne" => {
            let equal = if value.is_number() && right.is_number() {
                number(value)? == number(right)?
            } else {
                value == right
            };
            Ok(if filter.op == "eq" { equal } else { !equal })
        }
        "lt" | "le" | "gt" | "ge" => {
            let (left, right) = (number(value)?, number(right)?);
            Ok(match filter.op.as_str() {
                "lt" => left < right,
                "le" => left <= right,
                "gt" => left > right,
                _ => left >= right,
            })
        }
        _ => bail!("unknown filter operator {}", filter.op),
    }
}

fn validate_columns(names: &[String], table: &Table) -> Result<()> {
    ensure!(
        names.iter().collect::<BTreeSet<_>>().len() == names.len(),
        "duplicate column selection"
    );
    for name in names {
        ensure!(table.columns.contains(name), "unknown column {name}");
    }
    Ok(())
}

fn stats(values: &[&Value]) -> Result<Value> {
    let non_null = values
        .iter()
        .copied()
        .filter(|v| !v.is_null())
        .collect::<Vec<_>>();
    let distinct = non_null
        .iter()
        .map(|value| -> Result<String> {
            if value.is_number() {
                let n = number(value)?;
                Ok(format!("number:{}", if n == 0.0 { 0.0 } else { n }))
            } else {
                Ok(serde_json::to_string(value)?)
            }
        })
        .collect::<Result<BTreeSet<_>>>()?
        .len();
    let mut result = json!({"count":values.len(),"null":values.len()-non_null.len(),"distinct":distinct,"numeric_count":non_null.iter().filter(|v| v.is_number()).count(),"min":null,"max":null,"sum":null,"mean":null});
    if !non_null.is_empty() && non_null.iter().all(|v| v.is_number()) {
        let mut sum = 0.0;
        let mut min = f64::INFINITY;
        let mut max = f64::NEG_INFINITY;
        for value in &non_null {
            let n = number(value)?;
            sum += n;
            ensure!(sum.is_finite(), "numeric sum overflow");
            min = min.min(n);
            max = max.max(n);
        }
        result["min"] = json!(min);
        result["max"] = json!(max);
        result["sum"] = json!(sum);
        result["mean"] = json!(sum / non_null.len() as f64);
    }
    Ok(result)
}

pub(super) fn calculate(table: &Table, args: &Args) -> Result<Value> {
    let action = args.action.as_deref().unwrap_or("profile");
    ensure!(
        [
            "profile",
            "select",
            "aggregate",
            "chart",
            "histogram",
            "trend",
            "correlation"
        ]
        .contains(&action),
        "unknown data action"
    );
    let columns = args.columns.as_ref().unwrap_or(&table.columns);
    validate_columns(columns, table)?;
    validate_columns(&args.group_by, table)?;
    ensure!(
        args.filters.len() <= 16 && args.group_by.len() <= 4,
        "maximum 16 filters and 4 group columns"
    );
    ensure!(
        args.group_by.is_empty() || matches!(action, "aggregate" | "chart"),
        "group_by requires aggregate or chart"
    );
    for filter in &args.filters {
        ensure!(
            table.columns.contains(&filter.column),
            "unknown filter column {}",
            filter.column
        );
        ensure!(
            ["eq", "ne", "lt", "le", "gt", "ge", "is_null", "not_null"]
                .contains(&filter.op.as_str()),
            "unknown filter operator {}",
            filter.op
        );
        ensure!(
            matches!(filter.op.as_str(), "is_null" | "not_null")
                || filter.value.as_ref().is_some_and(|v| !v.is_null()),
            "comparison requires non-null value; use is_null/not_null"
        );
        if let Some(v) = &filter.value {
            ensure!(
                !v.is_array() && !v.is_object(),
                "filter values must be scalar"
            );
        }
    }
    super::investigate::validate(table, args)?;
    ensure!(
        !args.whole_source || (args.offset == 0 && args.limit.is_none()),
        "whole_source conflicts with offset/limit"
    );
    let limit = if args.whole_source {
        super::MAX_ROWS
    } else {
        args.limit.unwrap_or(100)
    };
    ensure!(
        args.whole_source || (1..=1000).contains(&limit),
        "limit must be 1..=1000"
    );
    validate_columns(&args.decimal_columns, table)?;
    ensure!(
        args.decimal_columns.is_empty() || matches!(action, "profile" | "aggregate" | "select"),
        "decimal_columns requires profile, aggregate or select; plots use f64"
    );
    let mut matched = Vec::new();
    for row in &table.rows {
        let mut include = super::investigate::includes(row, args)?;
        for filter in &args.filters {
            if !matches(&row.1, filter)? {
                include = false;
            }
        }
        if include {
            matched.push(row);
        }
    }
    let selected = matched
        .iter()
        .skip(args.offset)
        .take(limit)
        .copied()
        .collect::<Vec<_>>();
    let mut output = json!({
        "action":action,"columns":columns,"group_by":args.group_by,
        "coverage":{"source_rows":table.rows.len(),"matched_rows":matched.len(),"used_rows":selected.len(),"not_used_rows":table.rows.len()-selected.len(),"row_ids":"CSV data record; JSON array index+1; JSONL physical line; XLSX worksheet row; join output pair index+1 (see join.row_pairs)","window":{"offset":args.offset,"limit":limit,"whole_source":args.whole_source}},
        "used_row_ids":selected.iter().map(|r|r.0).collect::<Vec<_>>(),
        "used_rows":selected.iter().map(|(id,row)|json!({"row":id,"values":row})).collect::<Vec<_>>(),
        "replay":"used_rows retain all selected scalar values including filter/group dependencies. Rerun calculations over used_rows without the original offset/filter; proving which excluded rows matched requires the original source snapshot."
    });
    if matches!(action, "histogram" | "trend" | "correlation") {
        output["investigation"] = super::investigate::calculate(&selected, args)?;
    } else if action == "select" {
        output["rows"] = Value::Array(selected.iter().map(|(id,row)| json!({"row":id,"values":columns.iter().map(|c| (c.clone(),row.get(c).cloned().unwrap_or(Value::Null))).collect::<Map<_,_>>()})).collect());
    } else {
        struct Group<'a> {
            key: Vec<Value>,
            rows: Vec<&'a Map<String, Value>>,
            ids: Vec<usize>,
        }
        let mut groups: BTreeMap<String, Group<'_>> = BTreeMap::new();
        if args.group_by.is_empty() {
            groups.insert(
                "[]".into(),
                Group {
                    key: Vec::new(),
                    rows: Vec::new(),
                    ids: Vec::new(),
                },
            );
        }
        for (id, row) in &selected {
            let key = args
                .group_by
                .iter()
                .map(|c| row.get(c).cloned().unwrap_or(Value::Null))
                .collect::<Vec<_>>();
            let key_text = serde_json::to_string(&key)?;
            let entry = groups.entry(key_text).or_insert_with(|| Group {
                key,
                rows: Vec::new(),
                ids: Vec::new(),
            });
            entry.rows.push(row);
            entry.ids.push(*id);
            ensure!(
                groups.len() <= 100,
                "result exceeds 100 groups; refine filters/window"
            );
        }
        let mut results = Vec::new();
        for (_, Group { key, rows, ids }) in groups {
            let mut calculated = Map::new();
            for column in columns {
                let values = rows
                    .iter()
                    .map(|r| r.get(column).unwrap_or(&Value::Null))
                    .collect::<Vec<_>>();
                calculated.insert(
                    column.clone(),
                    if args.decimal_columns.contains(column) {
                        super::interpret::decimal_stats(&values)
                    } else {
                        stats(&values)
                    }
                    .with_context(|| format!("column {column}"))?,
                );
            }
            results
                .push(json!({"key":key,"rows":rows.len(),"row_ids":ids,"statistics":calculated}));
        }
        output["groups"] = json!(results);
        if action == "chart" {
            ensure!(
                columns.len() == 1 && args.group_by.len() <= 1,
                "chart requires exactly one column and at most one group_by column"
            );
            for group in &results {
                let stat = &group["statistics"][&columns[0]];
                ensure!(
                    stat["numeric_count"].as_u64()
                        == Some(
                            group["rows"].as_u64().unwrap_or(0)
                                - stat["null"].as_u64().unwrap_or(0)
                        ),
                    "chart requires a numeric column (nulls allowed)"
                );
            }
        }
    }
    Ok(output)
}

pub(super) fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

pub(super) fn chart(calculation: &Value) -> Result<String> {
    use std::fmt::Write;
    let column = calculation["columns"][0]
        .as_str()
        .context("chart column missing")?;
    let groups = calculation["groups"]
        .as_array()
        .context("chart groups missing")?;
    let max = groups
        .iter()
        .filter_map(|g| g["statistics"][column]["sum"].as_f64())
        .map(f64::abs)
        .fold(0.0_f64, f64::max);
    let height = 60 + groups.len() * 28;
    let mut svg = format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"800\" height=\"{height}\" viewBox=\"0 0 800 {height}\"><rect width=\"100%\" height=\"100%\" fill=\"white\"/><text x=\"12\" y=\"22\" fill=\"black\">Selected-row sums: {}</text><path d=\"M480 35V{height}\" stroke=\"black\"/>",
        escape(column)
    );
    for (i, group) in groups.iter().enumerate() {
        let value = group["statistics"][column]["sum"].as_f64();
        let label = serde_json::to_string(&group["key"])?;
        let width = value
            .map(|v| {
                if max == 0.0 {
                    0.0
                } else {
                    v.abs() / max * 240.0
                }
            })
            .unwrap_or(0.0);
        let x = if value.is_some_and(|v| v < 0.0) {
            480.0 - width
        } else {
            480.0
        };
        let y = 42 + i * 28;
        let ids = group["row_ids"]
            .as_array()
            .context("group row IDs missing")?
            .iter()
            .filter(|id| {
                calculation["used_rows"].as_array().is_some_and(|rows| {
                    rows.iter()
                        .any(|row| row["row"] == **id && row["values"][column].is_number())
                })
            })
            .cloned()
            .collect::<Vec<_>>();
        let ids = serde_json::to_string(&ids)?;
        let reference = calculation["calculation_reference"].as_str().unwrap_or("");
        write!(
            svg,
            "<a href=\"{}\" data-row-ids=\"{}\"><title>Source rows: {}</title>",
            escape(reference),
            escape(&ids),
            escape(&ids)
        )?;
        write!(
            svg,
            "<text x=\"12\" y=\"{}\" fill=\"black\">{}</text><rect x=\"{x}\" y=\"{y}\" width=\"{width}\" height=\"18\" fill=\"steelblue\"/><text x=\"730\" y=\"{}\" fill=\"black\">{}</text>",
            y + 14,
            escape(&label),
            y + 14,
            value
                .map(|v| v.to_string())
                .unwrap_or_else(|| "null".into())
        )?;
        svg.push_str("</a>");
    }
    svg.push_str("</svg>");
    Ok(svg)
}
