//! Deterministic descriptive statistics and row-addressable marks over the explicit query window.
use super::{
    Args,
    query::{escape, number},
    table::Table,
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::{collections::BTreeSet, fmt::Write};

type Row = (usize, Map<String, Value>);

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Brush {
    column: String,
    min: Option<Value>,
    max: Option<Value>,
}

pub(super) fn validate(table: &Table, args: &Args) -> Result<()> {
    if let Some(ids) = &args.row_ids {
        ensure!(
            ids.len() <= 1000 && ids.iter().all(|id| *id > 0),
            "row_ids requires at most 1000 positive IDs"
        );
        ensure!(
            ids.iter().collect::<BTreeSet<_>>().len() == ids.len(),
            "duplicate row IDs"
        );
        ensure!(
            ids.iter().all(|id| table.rows.iter().any(|r| r.0 == *id)),
            "row ID absent from available source/retained window"
        );
    }
    if let Some(brush) = &args.brush {
        ensure!(
            table.columns.contains(&brush.column),
            "unknown brush column"
        );
        ensure!(
            brush.min.is_some() || brush.max.is_some(),
            "brush requires min or max"
        );
        let min = brush.min.as_ref().map(number).transpose()?;
        let max = brush.max.as_ref().map(number).transpose()?;
        ensure!(
            min.zip(max).is_none_or(|(a, b)| a <= b),
            "brush min exceeds max"
        );
    }
    if let Some(bins) = args.bins {
        ensure!((1..=100).contains(&bins), "bins must be 1..=100");
        ensure!(
            args.action.as_deref() == Some("histogram"),
            "bins requires histogram"
        );
    }
    if let Some(order) = &args.order_by {
        ensure!(table.columns.contains(order), "unknown order_by column");
        ensure!(
            args.action.as_deref() == Some("trend"),
            "order_by requires trend"
        );
    }
    Ok(())
}

pub(super) fn includes(row: &Row, args: &Args) -> Result<bool> {
    if args
        .row_ids
        .as_ref()
        .is_some_and(|ids| !ids.contains(&row.0))
    {
        return Ok(false);
    }
    if let Some(brush) = &args.brush {
        let value = row.1.get(&brush.column).unwrap_or(&Value::Null);
        if value.is_null() {
            return Ok(false);
        }
        let value = number(value)?;
        if brush
            .min
            .as_ref()
            .map(number)
            .transpose()?
            .is_some_and(|min| value < min)
            || brush
                .max
                .as_ref()
                .map(number)
                .transpose()?
                .is_some_and(|max| value > max)
        {
            return Ok(false);
        }
    }
    Ok(true)
}

pub(super) fn replay(value: &Value) -> Result<Table> {
    let used = value["used_rows"]
        .as_array()
        .context("reference has no retained rows")?;
    ensure!(used.len() <= super::MAX_ROWS, "retained row bound exceeded");
    let mut rows = Vec::new();
    let mut columns = BTreeSet::new();
    for row in used {
        let id = row["row"]
            .as_u64()
            .context("retained row ID missing")?
            .try_into()?;
        let values = row["values"]
            .as_object()
            .context("retained values missing")?
            .clone();
        ensure!(
            values.values().all(|v| !v.is_object() && !v.is_array()),
            "invalid retained scalar"
        );
        columns.extend(values.keys().cloned());
        rows.push((id, values));
    }
    // Empty selections still retain the declared column names.
    if let Some(names) = value["columns"].as_array() {
        columns.extend(names.iter().filter_map(Value::as_str).map(str::to_owned));
    }
    ensure!(
        columns.len() <= super::MAX_COLUMNS,
        "retained column bound exceeded"
    );
    Ok(Table {
        rows,
        columns: columns.into_iter().collect(),
        sheet: None,
    })
}

fn finite(value: f64) -> Result<f64> {
    ensure!(value.is_finite(), "numeric calculation overflow");
    Ok(value)
}

// Hyndman-Fan type 7: linear interpolation at (n-1)*p. Input is sorted.
fn quantile(values: &[(usize, f64)], p: f64) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let index = (values.len() - 1) as f64 * p;
    let lo = index.floor() as usize;
    let hi = index.ceil() as usize;
    Some(values[lo].1 + (values[hi].1 - values[lo].1) * (index - lo as f64))
}

pub(super) fn calculate(rows: &[&Row], args: &Args) -> Result<Value> {
    let action = args
        .action
        .as_deref()
        .context("investigation action missing")?;
    let columns = args
        .columns
        .as_ref()
        .context("investigation requires explicit numeric columns")?;
    let paired = matches!(action, "trend" | "correlation");
    ensure!(
        columns.len() == if action == "correlation" { 2 } else { 1 },
        "histogram/trend require one numeric column; correlation requires two"
    );
    let x_column = if action == "trend" {
        args.order_by.as_ref().context(
            "trend requires explicit numeric order_by (time coordinate; units supplied by source)",
        )?
    } else {
        &columns[0]
    };
    let y_column = if action == "trend" {
        &columns[0]
    } else {
        columns.last().context("numeric column missing")?
    };
    let mut points = Vec::new();
    let mut excluded = Vec::new();
    let mut values = Vec::new();
    for row in rows {
        let x = row.1.get(x_column).unwrap_or(&Value::Null);
        let y = row.1.get(y_column).unwrap_or(&Value::Null);
        // Validate non-null values even if their partner is missing.
        let x = (!x.is_null()).then(|| number(x)).transpose()?;
        let y = (!y.is_null()).then(|| number(y)).transpose()?;
        if let (Some(x), Some(y)) = (x, y) {
            points.push(json!({"x":x,"y":y,"row_ids":[row.0]}));
            values.push((row.0, x));
        } else {
            excluded.push(row.0);
        }
    }
    let mut out = json!({"x_column":x_column,"y_column":y_column,"excluded_null_row_ids":excluded,"contributing_row_ids":values.iter().map(|v|v.0).collect::<Vec<_>>(),"methodology":"Descriptive only; no significance, confidence or causal claims. Nulls excluded, never zero. Numeric f64 with lexical magnitude and finite arithmetic guards."});
    if !paired {
        values.sort_by(|a, b| a.1.total_cmp(&b.1).then(a.0.cmp(&b.0)));
        let q1 = quantile(&values, 0.25);
        let q3 = quantile(&values, 0.75);
        out["quantiles"] = json!({"method":"Hyndman-Fan type 7, linear interpolation at (n-1)*p","min":quantile(&values,0.0),"q1":q1,"median":quantile(&values,0.5),"q3":q3,"max":quantile(&values,1.0)});
        let fences = q1
            .zip(q3)
            .map(|(a, b)| (a - 1.5 * (b - a), b + 1.5 * (b - a)));
        out["outliers"] = json!({"method":"Tukey: strictly outside Q1-1.5*IQR, Q3+1.5*IQR; not an error/anomaly claim","lower_fence":fences.map(|f|f.0),"upper_fence":fences.map(|f|f.1),"row_ids":values.iter().filter(|v|fences.is_some_and(|(lo,hi)|v.1<lo || v.1>hi)).map(|v|v.0).collect::<Vec<_>>()});
        let mut bins = Vec::new();
        if let (Some(min), Some(max)) = (values.first().map(|v| v.1), values.last().map(|v| v.1)) {
            let count = if min == max {
                1
            } else {
                args.bins.unwrap_or(10)
            };
            let range = max - min;
            let edges = (0..=count)
                .map(|i| {
                    if i == count {
                        max
                    } else {
                        min + range * (i as f64 / count as f64)
                    }
                })
                .collect::<Vec<_>>();
            ensure!(
                min == max || edges.windows(2).all(|w| w[0] < w[1]),
                "histogram bin edges collapse at f64 precision; reduce bins or rescale source units"
            );
            let mut ids = vec![Vec::new(); count];
            for (id, value) in &values {
                let bin = edges
                    .partition_point(|edge| edge <= value)
                    .saturating_sub(1)
                    .min(count - 1);
                ids[bin].push(*id);
            }
            for (i, ids) in ids.into_iter().enumerate() {
                bins.push(json!({"lower":edges[i],"upper":edges[i+1],"upper_inclusive":i+1==count,"count":ids.len(),"row_ids":ids}));
            }
        }
        out["bins"] = json!(bins);
        out["bin_method"] = json!(
            "Equal-width [lower,upper), final upper inclusive; constant column one closed bin; empty column no bins."
        );
    } else {
        if action == "trend" {
            points.sort_by(|a, b| {
                a["x"]
                    .as_f64()
                    .unwrap_or(0.0)
                    .total_cmp(&b["x"].as_f64().unwrap_or(0.0))
            });
        }
        let n = points.len() as f64;
        let mut mx = 0.0;
        let mut my = 0.0;
        for p in &points {
            mx = finite(mx + p["x"].as_f64().context("point x")?)?;
            my = finite(my + p["y"].as_f64().context("point y")?)?;
        }
        if n > 0.0 {
            mx /= n;
            my /= n;
        }
        let mut xx = 0.0;
        let mut yy = 0.0;
        let mut xy = 0.0;
        for p in &points {
            let dx = p["x"].as_f64().context("point x")? - mx;
            let dy = p["y"].as_f64().context("point y")? - my;
            xx = finite(xx + finite(dx * dx)?)?;
            yy = finite(yy + finite(dy * dy)?)?;
            xy = finite(xy + finite(dx * dy)?)?;
        }
        let r = if n >= 2.0 && xx > 0.0 && yy > 0.0 {
            Some(finite((xy / xx.sqrt()) / yy.sqrt())?.clamp(-1.0, 1.0))
        } else {
            None
        };
        let slope = if n >= 2.0 && xx > 0.0 {
            Some(finite(xy / xx)?)
        } else {
            None
        };
        let intercept = slope.map(|s| finite(my - s * mx)).transpose()?;
        out["fit"] = json!({"method":"Paired complete cases; centered sums; Pearson r and ordinary least squares y = intercept + slope*x. Null when fewer than two pairs or zero x variance; r also null for zero y variance. Floating-point underflow may collapse extremely small variation to zero; rescale units for such data. Equal x ties retain source order. Numeric time units are not inferred.","paired_count":points.len(),"pearson_r":r,"slope":slope,"intercept":intercept});
        out["points"] = json!(points);
    }
    Ok(out)
}

pub(super) fn chart(calculation: &Value) -> Result<String> {
    let stats = &calculation["investigation"];
    let histogram = calculation["action"] == "histogram";
    let marks = stats[if histogram { "bins" } else { "points" }]
        .as_array()
        .context("plot marks missing")?;
    let reference = escape(
        calculation["calculation_reference"]
            .as_str()
            .context("plot requires retained calculation reference")?,
    );
    let label = escape(&format!(
        "{}: {} / {} (selected rows only)",
        calculation["action"].as_str().unwrap_or(""),
        stats["x_column"].as_str().unwrap_or(""),
        stats["y_column"].as_str().unwrap_or("")
    ));
    let mut svg = format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"800\" height=\"440\" viewBox=\"0 0 800 440\"><rect width=\"800\" height=\"440\" fill=\"white\"/><text x=\"12\" y=\"24\">{label}</text><path d=\"M60 50V390H760\" stroke=\"black\" fill=\"none\"/>"
    );
    let coords = marks
        .iter()
        .map(|p| {
            if histogram {
                (
                    p["lower"].as_f64().unwrap_or(0.0),
                    p["count"].as_f64().unwrap_or(0.0),
                )
            } else {
                (
                    p["x"].as_f64().unwrap_or(0.0),
                    p["y"].as_f64().unwrap_or(0.0),
                )
            }
        })
        .collect::<Vec<_>>();
    let bounds = |axis: usize| {
        let mut min = f64::INFINITY;
        let mut max = f64::NEG_INFINITY;
        for p in &coords {
            let v = if axis == 0 { p.0 } else { p.1 };
            min = min.min(v);
            max = max.max(v);
        }
        (min, max)
    };
    let (xmin, mut xmax) = bounds(0);
    let (mut ymin, ymax) = bounds(1);
    if histogram {
        ymin = 0.0;
        xmax = marks
            .last()
            .and_then(|p| p["upper"].as_f64())
            .unwrap_or(xmax);
    }
    let scale = |v: f64, min: f64, max: f64| {
        if min == max {
            0.5
        } else {
            (v - min) / (max - min)
        }
    };
    for (i, mark) in marks.iter().enumerate() {
        let ids = escape(&serde_json::to_string(&mark["row_ids"])?);
        let tip = escape(&serde_json::to_string(mark)?);
        write!(
            svg,
            "<a href=\"{reference}\" data-row-ids=\"{ids}\"><title>{tip}</title>"
        )?;
        if histogram {
            let width = 700.0 / marks.len().max(1) as f64;
            let height = if ymax == 0.0 {
                0.0
            } else {
                coords[i].1 / ymax * 330.0
            };
            write!(
                svg,
                "<rect x=\"{}\" y=\"{}\" width=\"{}\" height=\"{height}\" fill=\"steelblue\"/>",
                60.0 + i as f64 * width,
                390.0 - height,
                (width - 2.0).max(0.0)
            )?;
        } else {
            let x = 60.0 + scale(coords[i].0, xmin, xmax) * 700.0;
            let y = 390.0 - scale(coords[i].1, ymin, ymax) * 330.0;
            write!(
                svg,
                "<circle cx=\"{x}\" cy=\"{y}\" r=\"4\" fill=\"steelblue\"/>"
            )?;
        }
        svg.push_str("</a>");
    }
    if histogram {
        svg.push_str("<text x=\"60\" y=\"45\">y: count of contributing rows</text>");
    }
    if !marks.is_empty() {
        write!(
            svg,
            "<text x=\"60\" y=\"415\">x: {xmin} .. {xmax}; y: {ymin} .. {ymax}</text>"
        )?;
    }
    svg.push_str("</svg>");
    Ok(svg)
}
