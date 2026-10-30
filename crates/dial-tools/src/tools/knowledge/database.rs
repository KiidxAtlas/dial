//! Bounded investigation of offline SQLite main-file snapshots, never live database connections.
use super::evidence::{publish, source};
use crate::config::types::Config;
use crate::runtime::evidence::{
    Coverage, CoverageExtent, Evidence, EvidenceContext, EvidenceKind, Location, Method,
    Representation,
};
use crate::tools::integrations::ext::{Tool, ToolContext, ToolEvent};
use crate::tools::{ToolOutput, Workspace};
use anyhow::{Context, Result, ensure};
use rusqlite::{
    Connection, OpenFlags,
    hooks::{AuthAction, Authorization},
    limits::Limit,
    types::ValueRef,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    io::Write,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

#[cfg(test)]
mod tests;

const MAX_DB: usize = 16 * 1024 * 1024;
const MAX_SQL: usize = 16 * 1024;
const MAX_CELL: usize = 4096;
const MAX_OUTPUT: usize = 256 * 1024;
const MAX_ROWS: usize = 200;
const MAX_COLUMNS: usize = 64;
const MAX_CALLBACKS: usize = 2000;
const DEADLINE: Duration = Duration::from_secs(2);
/// Aggregate SQLite allocator cap, installed only in a disposable worker process.
const SQLITE_HEAP_BYTES: i64 = 64 * 1024 * 1024;

/// Register the local database investigation tool.
pub fn tools(_: &Config) -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(Database)]
}
struct Database;
impl Tool for Database {
    fn name(&self) -> &str {
        "database"
    }
    fn schema(&self) -> Value {
        crate::tools::tool_schema(
            "database",
            "Inspect an authorized offline SQLite main-file snapshot (16 MiB), not a live connection. schema returns declared columns, keys and indexes; query executes one read-only SELECT/WITH; plan returns EXPLAIN QUERY PLAN, not measured runtime bottlenecks. WAL-mode files and existing WAL/SHM/journal companions are refused. No virtual tables, extensions, PRAGMAs, attachments or arbitrary functions. SQL must contain no semicolons. Bounded work/time/rows/columns/cells/output; integer and float values are strings, blobs report byte length only. Retains observations and source identity, never the database bytes; no verification/causality credit.",
            json!({
                "action":{"type":"string","enum":["schema","query","plan"]},
                "path":{"type":"string","maxLength":4096},
                "sql":{"type":"string","maxLength":16384},
                "limit":{"type":"integer","minimum":1,"maximum":200}
            }),
            &["action", "path"],
        )
    }
    fn needs_approval(&self, _: &Value) -> bool {
        false
    }
    fn read_only_hint(&self) -> bool {
        true
    }
    fn may_modify_workspace(&self, _: &Value) -> bool {
        false
    }
    fn call(&self, ctx: &ToolContext, args: &Value) -> ToolOutput {
        run(ctx.workspace, args).unwrap_or_else(|e| ToolOutput::err(format!("{e:#}")))
    }
    fn call_streaming(
        &self,
        ctx: &ToolContext,
        args: &Value,
        _: &(dyn Fn(ToolEvent) + Sync),
        cancel: &AtomicBool,
    ) -> ToolOutput {
        run_with_cancel(ctx.workspace, args, cancel)
            .unwrap_or_else(|e| ToolOutput::err(format!("{e:#}")))
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    action: String,
    path: String,
    sql: Option<String>,
    limit: Option<usize>,
}

fn no_companions(ws: &Workspace, path: &str) -> Result<()> {
    for suffix in ["-wal", "-shm", "-journal"] {
        let companion = ws
            .resolve(&format!("{path}{suffix}"))
            .map_err(anyhow::Error::msg)?;
        match std::fs::symlink_metadata(companion) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(e).context("checking SQLite companions"),
            Ok(_) => anyhow::bail!(
                "SQLite companion {suffix} exists; export a closed, checkpointed offline database first"
            ),
        }
    }
    Ok(())
}
fn capture(ws: &Workspace, path: &str) -> Result<Vec<u8>> {
    no_companions(ws, path)?;
    let bytes = source::read(ws, path)?;
    ensure!(
        bytes.len() >= 100 && bytes.len() <= MAX_DB && bytes.starts_with(b"SQLite format 3\0"),
        "expected SQLite database of 100 bytes..=16 MiB"
    );
    ensure!(
        bytes[18] == 1 && bytes[19] == 1,
        "WAL-mode or unsupported SQLite format; export an offline rollback-mode database"
    );
    no_companions(ws, path)?;
    ensure!(
        bytes == source::read(ws, path)?,
        "database changed during capture; use an offline export"
    );
    no_companions(ws, path)?;
    Ok(bytes)
}
fn controls(conn: &Connection) -> Result<()> {
    conn.busy_timeout(Duration::ZERO)?;
    conn.execute_batch(
        "PRAGMA query_only=ON; PRAGMA trusted_schema=OFF; PRAGMA temp_store=MEMORY;",
    )?;
    for (limit, value) in [
        (Limit::SQLITE_LIMIT_LENGTH, 1024 * 1024),
        (Limit::SQLITE_LIMIT_SQL_LENGTH, MAX_SQL as i32),
        (Limit::SQLITE_LIMIT_COLUMN, MAX_COLUMNS as i32),
        (Limit::SQLITE_LIMIT_EXPR_DEPTH, 64),
        (Limit::SQLITE_LIMIT_COMPOUND_SELECT, 16),
        (Limit::SQLITE_LIMIT_VDBE_OP, 25000),
        (Limit::SQLITE_LIMIT_FUNCTION_ARG, 32),
        (Limit::SQLITE_LIMIT_ATTACHED, 0),
        (Limit::SQLITE_LIMIT_VARIABLE_NUMBER, 128),
        (Limit::SQLITE_LIMIT_WORKER_THREADS, 0),
    ] {
        conn.set_limit(limit, value)?;
    }
    let start = Instant::now();
    let mut callbacks = 0;
    conn.progress_handler(
        1000,
        Some(move || {
            callbacks += 1;
            callbacks > MAX_CALLBACKS || start.elapsed() >= DEADLINE
        }),
    )?;
    Ok(())
}
fn restrict(conn: &Connection, tables: Vec<String>) -> Result<()> {
    conn.authorizer(Some(
        move |ctx: rusqlite::hooks::AuthContext<'_>| match ctx.action {
            AuthAction::Select | AuthAction::Recursive => Authorization::Allow,
            AuthAction::Read { table_name, .. }
                if tables
                    .iter()
                    .any(|name| name.eq_ignore_ascii_case(table_name)) =>
            {
                Authorization::Allow
            }
            AuthAction::Function { function_name }
                if [
                    "count",
                    "sum",
                    "avg",
                    "min",
                    "max",
                    "total",
                    "length",
                    "typeof",
                    "coalesce",
                    "ifnull",
                    "nullif",
                    "abs",
                    "round",
                    "lower",
                    "upper",
                    "substr",
                    "substring",
                    "trim",
                    "ltrim",
                    "rtrim",
                    "like",
                    "glob",
                ]
                .contains(&function_name.to_ascii_lowercase().as_str()) =>
            {
                Authorization::Allow
            }
            _ => Authorization::Deny,
        },
    ))?;
    Ok(())
}
fn sql_text(sql: &str) -> Result<()> {
    ensure!(
        !sql.is_empty() && sql.len() <= MAX_SQL && !sql.contains(';') && !sql.contains('\0'),
        "SQL must be 1..=16384 bytes, with no semicolons or NULs"
    );
    let keyword = sql
        .trim_start()
        .split(|c: char| !c.is_ascii_alphabetic())
        .next()
        .unwrap_or("");
    ensure!(
        keyword.eq_ignore_ascii_case("select") || keyword.eq_ignore_ascii_case("with"),
        "only SELECT or WITH SELECT is accepted (no leading comments)"
    );
    Ok(())
}
fn cell(value: ValueRef<'_>) -> Value {
    match value {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(n) => json!({"integer":n.to_string()}),
        ValueRef::Real(n) => json!({"real":n.to_string()}),
        ValueRef::Blob(b) => json!({"blob_bytes":b.len(),"content_omitted":true}),
        ValueRef::Text(b) => {
            let mut end = b.len().min(MAX_CELL);
            while end > 0 && std::str::from_utf8(&b[..end]).is_err() {
                end -= 1;
            }
            json!({"text":String::from_utf8_lossy(&b[..end]),"bytes":b.len(),"truncated":end < b.len()})
        }
    }
}
fn rows(conn: &Connection, sql: &str, limit: usize) -> Result<Value> {
    let mut stmt = conn
        .prepare(sql)
        .context("preparing bounded read-only SQL")?;
    ensure!(stmt.readonly(), "statement is not SQLite read-only");
    let count = stmt.column_count();
    ensure!(
        count > 0 && count <= MAX_COLUMNS,
        "result must have 1..=64 columns"
    );
    let columns: Vec<_> = stmt.column_names().iter().map(|s| s.to_string()).collect();
    let mut cursor = stmt.query([])?;
    let mut output = Vec::new();
    let mut size = serde_json::to_vec(&columns)?.len();
    let mut truncated = false;
    let mut truncated_cells = 0usize;
    while let Some(row) = cursor
        .next()
        .context("SQLite read failed (possibly work/time/cancellation limit)")?
    {
        if output.len() >= limit {
            truncated = true;
            break;
        }
        let values: Vec<_> = (0..count)
            .map(|i| row.get_ref(i).map(cell))
            .collect::<rusqlite::Result<_>>()?;
        let row_truncated_cells = values
            .iter()
            .filter(|value| value["truncated"] == true)
            .count();
        let n = serde_json::to_vec(&values)?.len();
        if size + n > MAX_OUTPUT {
            truncated = true;
            break;
        }
        size += n;
        output.push(values);
        truncated_cells += row_truncated_cells;
    }
    Ok(
        json!({"columns":columns,"rows":output,"truncated":truncated || truncated_cells > 0,"truncated_cells":truncated_cells,"returned_rows":output.len()}),
    )
}
fn quote(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}
fn schema(conn: &Connection, limit: usize) -> Result<Value> {
    let mut stmt = conn.prepare("SELECT name FROM sqlite_schema WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name")?;
    let mut cursor = stmt.query([])?;
    let mut tables = Vec::new();
    let mut truncated = false;
    let mut size = 0;
    while let Some(row) = cursor.next()? {
        if tables.len() >= limit {
            truncated = true;
            break;
        }
        let name: String = row.get(0)?;
        let table = json!({"name":name,
            "columns":rows(conn, &format!("PRAGMA table_xinfo({})",quote(&name)), MAX_ROWS)?,
            "foreign_keys":rows(conn, &format!("PRAGMA foreign_key_list({})",quote(&name)), MAX_ROWS)?,
            "indexes":rows(conn, &format!("PRAGMA index_list({})",quote(&name)), MAX_ROWS)?,
            "index_definitions":rows(conn, &format!("SELECT name,sql FROM sqlite_schema WHERE type='index' AND tbl_name='{}' ORDER BY name", name.replace('\'', "''")), MAX_ROWS)?});
        truncated |= ["columns", "foreign_keys", "indexes", "index_definitions"]
            .iter()
            .any(|key| table[key]["truncated"] == true);
        size += serde_json::to_vec(&table)?.len();
        if size > MAX_OUTPUT {
            truncated = true;
            break;
        }
        tables.push(table);
    }
    Ok(
        json!({"tables":tables,"truncated":truncated,"semantics":"SQLite declared type, not enforced value type; notnull is declared NOT NULL, pk is primary-key ordinal; foreign keys are declared links, not measured cardinality or enforcement status. Index list includes uniqueness/origin/partial flags; no runtime performance claim."}),
    )
}
/// Inspect SQLite bytes inside the disposable document worker ONLY.
///
/// This deliberately sets SQLite's process-global heap limit. Calling it in the agent process
/// would constrain unrelated SQLite connections; callers must use `inspect_database_bytes`.
pub(crate) fn inspect_snapshot(
    payload: &[u8],
    action: &str,
    sql: Option<&str>,
    limit: usize,
) -> Result<Value> {
    ensure!(
        (100..=MAX_DB).contains(&payload.len())
            && payload.starts_with(b"SQLite format 3\0")
            && payload[18] == 1
            && payload[19] == 1,
        "invalid offline SQLite snapshot"
    );
    ensure!((1..=MAX_ROWS).contains(&limit), "limit must be 1..=200");
    ensure!(
        ["schema", "query", "plan"].contains(&action),
        "unknown database action"
    );
    if action == "schema" {
        ensure!(sql.is_none(), "schema does not accept sql");
    } else {
        sql_text(sql.context("query/plan requires sql")?)?;
    }

    // The process has exactly one job. Set the aggregate allocator ceiling before SQLite sees
    // hostile schema/pages, not merely before stepping: preparation also allocates. The cap is
    // on all SQLite heap allocations (including in-memory sorters), not Rust's bounded payload.
    let bootstrap = Connection::open_in_memory()?;
    let heap_limit: i64 =
        bootstrap.query_row("PRAGMA hard_heap_limit=67108864", [], |r| r.get(0))?;
    ensure!(
        heap_limit > 0 && heap_limit <= SQLITE_HEAP_BYTES,
        "SQLite heap ceiling was not installed"
    );
    drop(bootstrap);

    let mut temporary = tempfile::NamedTempFile::new()?;
    temporary.write_all(payload)?;
    temporary.flush()?;
    let conn = Connection::open_with_flags(
        temporary.path(),
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    controls(&conn)?;
    let virtuals: i64 = conn.query_row(
        "SELECT count(*) FROM sqlite_schema WHERE upper(sql) LIKE '%CREATE VIRTUAL TABLE%'",
        [],
        |r| r.get(0),
    )?;
    ensure!(virtuals == 0, "virtual-table databases are not supported");
    if action == "schema" {
        return schema(&conn, limit);
    }
    let mut names =
        conn.prepare("SELECT name FROM sqlite_schema WHERE type IN ('table','view') LIMIT 1001")?;
    let tables = names
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    ensure!(tables.len() <= 1000, "database schema exceeds 1000 objects");
    drop(names);
    restrict(&conn, tables)?;
    let sql = sql.context("sql required")?;
    // EXPLAIN must not mask the original statement's read-only status.
    ensure!(conn.prepare(sql)?.readonly(), "statement is not read-only");
    rows(
        &conn,
        &if action == "plan" {
            format!("EXPLAIN QUERY PLAN {sql}")
        } else {
            sql.into()
        },
        limit,
    )
}
/// Investigate authorized SQLite bytes, retaining only bounded observations and their source identity.
pub fn run(ws: &Workspace, value: &Value) -> Result<ToolOutput> {
    run_with_cancel(ws, value, &AtomicBool::new(false))
}
fn run_with_cancel(ws: &Workspace, value: &Value, cancel: &AtomicBool) -> Result<ToolOutput> {
    let args: Args = serde_json::from_value(value.clone())?;
    ensure!(
        !cancel.load(Ordering::Relaxed),
        "database investigation cancelled"
    );
    ensure!(
        !args.path.is_empty() && args.path.len() <= 4096,
        "path must contain 1..=4096 bytes"
    );
    ensure!(
        ["schema", "query", "plan"].contains(&args.action.as_str()),
        "unknown database action"
    );
    let limit = args.limit.unwrap_or(50);
    ensure!((1..=MAX_ROWS).contains(&limit), "limit must be 1..=200");
    if args.action == "schema" {
        ensure!(args.sql.is_none(), "schema does not accept sql");
    } else {
        sql_text(args.sql.as_deref().context("query/plan requires sql")?)?;
    }
    let bytes = capture(ws, &args.path)?;
    ensure!(
        !cancel.load(Ordering::Relaxed),
        "database investigation cancelled"
    );
    let snapshot = source::snapshot(&bytes)?;
    let observed = crate::tools::fs::documents::inspect_database_bytes(
        &bytes,
        &args.action,
        args.sql.as_deref(),
        limit,
        cancel,
    )
    .map_err(anyhow::Error::msg)?;
    ensure!(
        !cancel.load(Ordering::Relaxed),
        "database investigation cancelled"
    );
    let truncated = observed["truncated"].as_bool().unwrap_or(false);
    let result = json!({"type":"sqlite_observation","action":args.action,"path":args.path,"sql":args.sql,"observation":observed,
        "limits":{"source_bytes":MAX_DB,"sql_bytes":MAX_SQL,"rows_or_tables":limit,"columns":MAX_COLUMNS,"cell_text_bytes":MAX_CELL,"output_bytes":MAX_OUTPUT,"vm_steps_approx":MAX_CALLBACKS*1000,"sqlite_time_ms":DEADLINE.as_millis(),"sqlite_heap_bytes":SQLITE_HEAP_BYTES,"execution":"disposable isolated worker"},
        "snapshot_semantics":"Main-file bytes only; WAL-mode and existing companions rejected before/after capture. Not a transactional live backup or guarantee of latest commits. Use an offline export.",
        "verification_credit":false,"plan_semantics":"SCAN/SEARCH are SQLite planner descriptions, not executed timings or proof of a bottleneck"});
    let representation = serde_json::to_vec(&result)?;
    let item = Evidence { source:args.path, location:Location::Whole, method:Method::Calculation,
        producer:"dial/sqlite-v1".into(), kind:EvidenceKind::Observation,
        summary:"Bounded offline SQLite schema/query/plan observation".into(),
        representation:Representation::of_bytes("application/json", &representation),
        limitations:vec!["Only isolated main-file bytes; not a consistent live backup or latest-commit guarantee. No original database bytes retained.".into(),"Declared links and query plans are not cardinality, causality, enforcement or runtime performance measurements. Row/cell/output caps may omit data.".into()],
        context:EvidenceContext { snapshot:Some(snapshot), coverage:Some(Coverage { extent:CoverageExtent::Partial, inspected:vec![Location::Whole], omitted:vec![], truncated, failures:vec![] }), ..Default::default() } };
    publish(ws, ToolOutput::structured(result), item, &representation)
}
