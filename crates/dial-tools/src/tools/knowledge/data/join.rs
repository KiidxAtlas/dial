//! Bounded equijoins preserve both source revisions and every output row's source pair.
use super::{
    MAX_BYTES, MAX_COLUMNS,
    table::{self, Table},
};
use crate::tools::Workspace;
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::AtomicBool;

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Join {
    path: String,
    format: Option<String>,
    sheet: Option<String>,
    left_on: Vec<String>,
    right_on: Vec<String>,
    kind: Kind,
    max_rows: Option<usize>,
}
#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum Kind {
    Inner,
    Left,
    Full,
}

fn key(row: &Map<String, Value>, columns: &[String]) -> Result<Option<String>> {
    let values = columns
        .iter()
        .map(|c| row.get(c).unwrap_or(&Value::Null))
        .collect::<Vec<_>>();
    if values.iter().any(|v| v.is_null()) {
        return Ok(None);
    }
    Ok(Some(serde_json::to_string(&values)?))
}
fn index(table: &Table, columns: &[String]) -> Result<BTreeMap<String, Vec<usize>>> {
    let mut result = BTreeMap::<_, Vec<_>>::new();
    for (i, (_, row)) in table.rows.iter().enumerate() {
        if let Some(key) = key(row, columns)? {
            result.entry(key).or_default().push(i);
        }
    }
    Ok(result)
}
fn duplicates(table: &Table, index: &BTreeMap<String, Vec<usize>>) -> Vec<Value> {
    index.iter().filter(|(_, ids)| ids.len() > 1).map(|(key, ids)| json!({"key_lexical_json":key,"row_ids":ids.iter().map(|i|table.rows[*i].0).collect::<Vec<_>>()})).collect()
}

pub(super) fn load(
    ws: &Workspace,
    left: &mut Table,
    join: &Join,
    pointers: bool,
    retain: bool,
    cancel: &AtomicBool,
) -> Result<Value> {
    use super::super::evidence::source;
    ensure!(
        !join.left_on.is_empty()
            && join.left_on.len() <= 4
            && join.left_on.len() == join.right_on.len(),
        "join requires equal 1..=4 left_on/right_on keys"
    );
    let max = join.max_rows.unwrap_or(1000);
    ensure!((1..=1000).contains(&max), "join max_rows must be 1..=1000");
    let bytes = source::read(ws, &join.path)?;
    ensure!(bytes.len() <= MAX_BYTES, "join source exceeds 8 MiB");
    let format = super::source_format(ws, &join.path, join.format.as_deref())?;
    let right = table::parse_with_paths(&bytes, &format, join.sheet.as_deref(), pointers, cancel)?;
    for (columns, table) in [(&join.left_on, &*left), (&join.right_on, &right)] {
        ensure!(
            columns.iter().collect::<BTreeSet<_>>().len() == columns.len(),
            "duplicate join key column"
        );
        ensure!(
            columns.iter().all(|c| table.columns.contains(c)),
            "unknown join key column"
        );
    }
    ensure!(
        left.columns.len() + right.columns.len() <= MAX_COLUMNS,
        "joined table exceeds 128 columns"
    );
    let li = index(left, &join.left_on)?;
    let ri = index(&right, &join.right_on)?;
    let mut pairs = Vec::new();
    let mut unmatched_left = Vec::new();
    let mut matched_right = BTreeSet::new();
    for (i, (id, row)) in left.rows.iter().enumerate() {
        super::check_cancel(cancel)?;
        let matches = key(row, &join.left_on)?.and_then(|key| ri.get(&key));
        if let Some(matches) = matches {
            for j in matches {
                ensure!(
                    pairs.len() < max,
                    "join exceeds max_rows {max}; no partial join returned"
                );
                pairs.push((Some(i), Some(*j)));
                matched_right.insert(*j);
            }
        } else {
            unmatched_left.push(*id);
            if matches!(join.kind, Kind::Left | Kind::Full) {
                ensure!(
                    pairs.len() < max,
                    "join exceeds max_rows {max}; no partial join returned"
                );
                pairs.push((Some(i), None));
            }
        }
    }
    let unmatched_right = right
        .rows
        .iter()
        .enumerate()
        .filter(|(i, _)| !matched_right.contains(i))
        .map(|(_, r)| r.0)
        .collect::<Vec<_>>();
    if matches!(join.kind, Kind::Full) {
        for j in 0..right.rows.len() {
            if !matched_right.contains(&j) {
                ensure!(
                    pairs.len() < max,
                    "join exceeds max_rows {max}; no partial join returned"
                );
                pairs.push((None, Some(j)));
            }
        }
    }
    let mut rows = Vec::new();
    let mut identities = Vec::new();
    for (i, (l, r)) in pairs.into_iter().enumerate() {
        let mut values = Map::new();
        for (prefix, table, index) in [("left", &*left, l), ("right", &right, r)] {
            for column in &table.columns {
                let value = index
                    .and_then(|index| table.rows[index].1.get(column))
                    .cloned()
                    .unwrap_or(Value::Null);
                values.insert(format!("{prefix}:{column}"), value);
            }
        }
        identities.push(json!({"row":i+1,"left_row":l.map(|l|left.rows[l].0),"right_row":r.map(|r|right.rows[r].0)}));
        rows.push((i + 1, values));
    }
    let mut snapshot = source::snapshot(&bytes)?;
    if retain {
        snapshot.artifact = Some(
            ws.artifacts()
                .ok_or_else(|| anyhow::anyhow!("retain_source requires artifact storage"))?
                .retain_exact_bytes(&bytes)?,
        );
    }
    let report = json!({"right_source":{"path":join.path,"format":format,"sheet":right.sheet,"snapshot":snapshot},"left_source":"see top-level source snapshot","left_rows":left.rows.len(),"right_rows":right.rows.len(),"joined_rows":rows.len(),"duplicate_left_keys":duplicates(left,&li),"duplicate_right_keys":duplicates(&right,&ri),"unmatched_left_row_ids":unmatched_left,"unmatched_right_row_ids":unmatched_right,"row_pairs":identities,"semantics":"typed lexical JSON equality, no coercion (1 and 1.0 may differ); any null key unmatched; duplicate keys produce all pairs, never silently deduplicated. Report covers full sources before query filters/window. Columns prefixed left: and right:."});
    left.columns = left
        .columns
        .iter()
        .map(|c| format!("left:{c}"))
        .chain(right.columns.iter().map(|c| format!("right:{c}")))
        .collect();
    left.rows = rows;
    left.sheet = None;
    Ok(report)
}
