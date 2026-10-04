use rusqlite::{Connection, OpenFlags, types::ValueRef};
use serde_json::{Value, json};
use std::{path::Path, time::Duration};
fn identifier(name: &str) -> String { format!("\"{}\"", name.replace('"', "\"\"")) }
pub(super) fn read(path: &Path, selector: Option<&str>) -> Result<String, String> {
    let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX).map_err(|e| format!("opening read-only SQLite: {e}"))?;
    db.busy_timeout(Duration::from_millis(500)).map_err(|e| e.to_string())?;
    db.execute_batch("PRAGMA query_only=ON; PRAGMA trusted_schema=OFF;").map_err(|e| e.to_string())?;
    let mut tables = db.prepare("SELECT name, sql FROM sqlite_schema WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name LIMIT 1001").map_err(|e| e.to_string())?;
    let schema = tables.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))).map_err(|e| e.to_string())?.collect::<Result<Vec<_>, _>>().map_err(|e| e.to_string())?;
    if schema.len() > 1000 { return Err("SQLite exceeds 1000 table limit".into()); }
    let Some(selector) = selector else { return Ok(schema.iter().map(|(name,sql)| format!("{name}\t{}\n", sql.as_deref().unwrap_or(""))).collect()); };
    let (table, options) = selector.split_once('?').unwrap_or((selector, ""));
    let (table, key) = table.split_once(":key=").map_or((table, None), |(a,b)| (a, Some(b)));
    if !schema.iter().any(|(name,_)| name == table) { return Err(format!("SQLite table {table:?} not found (views and arbitrary SQL are not supported)")); }
    let (mut offset, mut limit) = (0usize, 200usize);
    for option in options.split('&').filter(|s| !s.is_empty()) {
        let (name, value) = option.split_once('=').ok_or("SQLite options must be offset=N or limit=N")?;
        let n = value.parse::<usize>().map_err(|_| "invalid SQLite numeric option")?;
        match name { "offset" if n <= 1_000_000 => offset = n, "limit" if (1..=1000).contains(&n) => limit = n, _ => return Err("SQLite supports offset=0..1000000 and limit=1..1000 only".into()) }
    }
    let mut columns = db.prepare(&format!("PRAGMA table_info({})", identifier(table))).map_err(|e| e.to_string())?;
    let info = columns.query_map([], |r| Ok((r.get::<_, String>(1)?, r.get::<_, i64>(5)?))).map_err(|e| e.to_string())?.collect::<Result<Vec<_>, _>>().map_err(|e| e.to_string())?;
    if info.len() > 256 { return Err("SQLite table exceeds 256-column limit".into()); }
    let primary: Vec<_> = info.iter().filter(|(_,pk)| *pk > 0).collect();
    if key.is_some() && primary.len() != 1 { return Err("key selector requires a single-column primary key".into()); }
    // Projection bounds each value before conversion; blobs are represented by size, never dumped.
    let projection = info.iter().map(|(name,_)| {
        let col = identifier(name);
        format!("CASE WHEN typeof({col})='blob' THEN '[blob ' || length({col}) || ' bytes]' WHEN typeof({col})='text' THEN substr({col},1,2000) ELSE {col} END")
    }).collect::<Vec<_>>().join(",");
    let filter = if key.is_some() { format!(" WHERE {} = ?1", identifier(&primary[0].0)) } else { String::new() };
    let order = if primary.is_empty() { String::new() } else {
        let mut sorted = primary.clone(); sorted.sort_by_key(|(_,pk)| *pk);
        format!(" ORDER BY {}", sorted.iter().map(|(name,_)| identifier(name)).collect::<Vec<_>>().join(","))
    };
    let sql = format!("SELECT {projection} FROM {}{filter}{order} LIMIT {limit} OFFSET {offset}", identifier(table));
    let mut statement = db.prepare(&sql).map_err(|e| e.to_string())?;
    let mut rows = if let Some(key) = key { statement.query([key]) } else { statement.query([]) }.map_err(|e| e.to_string())?;
    let mut out = format!("[table {table}; row offset {offset}; limit {limit}; values bounded to 2000 characters; blobs summarized]\n{}
", json!(info.iter().map(|(name,_)| name).collect::<Vec<_>>()));
    while let Some(row) = rows.next().map_err(|e| e.to_string())? {
        let mut values = Vec::with_capacity(info.len());
        for i in 0..info.len() {
            values.push(match row.get_ref(i).map_err(|e| e.to_string())? {
                ValueRef::Null => Value::Null,
                ValueRef::Integer(n) => json!(n),
                ValueRef::Real(n) => json!(n),
                ValueRef::Text(s) => json!(String::from_utf8_lossy(s)),
                ValueRef::Blob(s) => json!(format!("[blob {} bytes]", s.len())),
            });
        }
        out.push_str(&format!("{}\n", json!(values)));
        if out.len() > super::MAX_CONTENT as usize { return Err("SQLite output exceeds 8 MiB; use a smaller limit".into()); }
    }
    Ok(out)
}
