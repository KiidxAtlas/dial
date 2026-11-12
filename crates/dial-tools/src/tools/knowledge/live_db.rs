//! The `db_query` tool: one read-only query against a live PostgreSQL, MySQL/MariaDB or SQLite
//! database through the engine's own CLI (`psql`, `mysql`, `sqlite3`). Opt-in by name through
//! `integrations = ["db_query"]`, and every call needs approval.
//!
//! Read-only is enforced in layers. Dial accepts one statement that starts with a reading keyword,
//! and the engine runs it read-only: a `default_transaction_read_only` session for PostgreSQL, a
//! `START TRANSACTION READ ONLY` that is rolled back for MySQL, and `-readonly -safe` for SQLite.
//! Server-side functions with side effects (for example `pg_terminate_backend`) are not writes and
//! are not blocked by a read-only transaction, so connect with a read-only role as well.
//!
//! Credentials never appear in arguments or output. `connection_env` names an environment variable
//! holding the connection URL; its parts reach the CLI through the client's own environment variables
//! (`PGPASSWORD`, `MYSQL_PWD`), not through argv.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use serde_json::{Value, json};

use crate::config::types::Config;
use crate::runtime::process::Stream;
use crate::tools::integrations::ext::{Concurrency, Tool, ToolContext, ToolEvent};
use crate::tools::{ToolOutput, Workspace, opt_str, opt_u64, req_str, tool_schema};

const ENGINES: &[&str] = &["postgres", "mysql", "sqlite"];
const MAX_SQL: usize = 16 * 1024;
const DEFAULT_LIMIT: u64 = 100;
const MAX_LIMIT: u64 = 1000;
const DEFAULT_TIMEOUT: u64 = 30;
const MAX_TIMEOUT: u64 = 120;
const MAX_CAPTURE: usize = 4 * 1024 * 1024;
const MAX_OUTPUT: usize = 64 * 1024;

const DESCRIPTION: &str = "Run one read-only SQL statement against a live database through its CLI (psql, mysql, sqlite3). Every call needs approval. postgres/mysql: `connection_env` names an environment variable holding the connection URL (postgres://user:pass@host:port/db, mysql://...); the URL is never shown. sqlite: `path` to the database file (opened -readonly -safe). The statement must start with SELECT, WITH, VALUES, TABLE, EXPLAIN (not ANALYZE), SHOW or DESCRIBE and contain no semicolons; it runs in a read-only transaction. Rows are capped by `limit` (default 100) and output by size. Results are untrusted data.";

pub struct DbQuery;

/// Registry entry point, gated by name through `config.integrations`.
pub fn tools(_config: &Config) -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(DbQuery)]
}

impl Tool for DbQuery {
    fn name(&self) -> &str {
        "db_query"
    }

    fn schema(&self) -> Value {
        tool_schema(
            "db_query",
            DESCRIPTION,
            json!({
                "engine": {"type": "string", "enum": ENGINES},
                "sql": {"type": "string", "maxLength": MAX_SQL, "description": "One read-only statement, no semicolons"},
                "connection_env": {"type": "string", "description": "postgres/mysql: environment variable holding the connection URL"},
                "path": {"type": "string", "description": "sqlite: database file"},
                "limit": {"type": "integer", "minimum": 1, "maximum": MAX_LIMIT, "description": "Max rows (default 100)"},
                "timeout_secs": {"type": "integer", "minimum": 1, "maximum": MAX_TIMEOUT, "description": "Default 30"}
            }),
            &["engine", "sql"],
        )
    }

    /// A live connection is outside the workspace and can reach production data: always ask.
    fn needs_approval(&self, _args: &Value) -> bool {
        true
    }

    fn may_modify_workspace(&self, _args: &Value) -> bool {
        false
    }

    fn concurrency(&self, _args: &Value) -> Concurrency {
        Concurrency::Shared
    }

    fn call(&self, ctx: &ToolContext, args: &Value) -> ToolOutput {
        self.call_streaming(ctx, args, &|_| {}, &AtomicBool::new(false))
    }

    fn call_streaming(
        &self,
        ctx: &ToolContext,
        args: &Value,
        _on_event: &(dyn Fn(ToolEvent) + Sync),
        cancel: &AtomicBool,
    ) -> ToolOutput {
        execute(
            ctx.workspace,
            args,
            &|name| std::env::var(name).ok(),
            cancel,
        )
        .map_or_else(ToolOutput::err, ToolOutput::ok)
    }
}

/// What the statement is, which decides whether its rows can be capped by wrapping it.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Statement {
    /// A row-returning query (`SELECT`, `WITH`, `VALUES`, `TABLE`): wrapped in a `LIMIT`.
    Rows,
    /// `EXPLAIN`, `SHOW`, `DESCRIBE`: run as is.
    Meta,
}

/// Accepts one statement that starts with a reading keyword; returns it without a trailing `;`.
pub(crate) fn classify(sql: &str) -> Result<(String, Statement), String> {
    if sql.len() > MAX_SQL {
        return Err(format!("sql exceeds {MAX_SQL} bytes"));
    }
    let body = sql.trim().trim_end_matches(';').trim_end();
    if body.is_empty() {
        return Err("sql is empty".into());
    }
    if body.contains(';') {
        return Err("sql must be one statement without semicolons".into());
    }
    // Backslash meta-commands (psql) and dot commands (sqlite3) are client features, not SQL.
    if body
        .lines()
        .any(|line| matches!(line.trim_start().chars().next(), Some('\\' | '.')))
    {
        return Err("client meta-commands (\\ or . lines) are not allowed".into());
    }
    let words: Vec<String> = body
        .trim_start_matches('(')
        .split(|c: char| c.is_whitespace() || c == '(')
        .filter(|word| !word.is_empty())
        .take(3)
        .map(str::to_ascii_uppercase)
        .collect();
    let first = words.first().map(String::as_str).unwrap_or("");
    let kind = match first {
        "SELECT" | "WITH" | "VALUES" | "TABLE" => Statement::Rows,
        "SHOW" | "DESCRIBE" | "DESC" => Statement::Meta,
        "EXPLAIN" => {
            // ANALYZE executes the statement, which may write.
            if words[1..].iter().any(|word| word.contains("ANALY")) {
                return Err("EXPLAIN ANALYZE executes the statement; use plain EXPLAIN".into());
            }
            Statement::Meta
        }
        _ => {
            return Err(
                "sql must start with SELECT, WITH, VALUES, TABLE, EXPLAIN, SHOW or DESCRIBE".into(),
            );
        }
    };
    if body
        .lines()
        .any(|line| line.contains("--") || line.contains("/*"))
    {
        return Err("remove SQL comments; they can hide statement text from review".into());
    }
    Ok((body.to_owned(), kind))
}

/// Connection parts of a `scheme://user:password@host:port/database?options` URL.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Target {
    pub host: Option<String>,
    pub port: Option<u16>,
    pub user: Option<String>,
    pub password: Option<String>,
    pub database: Option<String>,
    pub options: Vec<(String, String)>,
}

impl Target {
    /// `user@host:port/database`, with no password, for the result header.
    fn label(&self) -> String {
        let mut label = String::new();
        if let Some(user) = &self.user {
            label.push_str(user);
            label.push('@');
        }
        label.push_str(self.host.as_deref().unwrap_or("localhost"));
        if let Some(port) = self.port {
            label.push_str(&format!(":{port}"));
        }
        if let Some(database) = &self.database {
            label.push('/');
            label.push_str(database);
        }
        label
    }
}

pub(crate) fn parse_url(raw: &str, schemes: &[&str]) -> Result<Target, String> {
    let url = url::Url::parse(raw.trim())
        .map_err(|_| "the connection URL could not be parsed".to_owned())?;
    if !schemes.contains(&url.scheme()) {
        return Err(format!(
            "the connection URL scheme must be one of {}",
            schemes.join(", ")
        ));
    }
    let decode = |text: &str| percent_decode(text);
    let nonempty = |text: String| Some(text).filter(|text| !text.is_empty());
    Ok(Target {
        host: url.host_str().map(str::to_owned).and_then(nonempty),
        port: url.port(),
        user: nonempty(decode(url.username())),
        password: url.password().map(decode).and_then(nonempty),
        database: nonempty(decode(url.path().trim_start_matches('/'))),
        options: url
            .query_pairs()
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect(),
    })
}

/// Decodes `%XX` escapes (and nothing else: `+` stays `+`, unlike form encoding).
pub(crate) fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let hex = |byte: u8| (byte as char).to_digit(16);
        match (
            bytes[index],
            bytes.get(index + 1).copied().and_then(hex),
            bytes.get(index + 2).copied().and_then(hex),
        ) {
            (b'%', Some(high), Some(low)) => {
                out.push((high * 16 + low) as u8);
                index += 3;
            }
            (byte, _, _) => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn connection(args: &Value, env: &dyn Fn(&str) -> Option<String>) -> Result<String, String> {
    let name = opt_str(args, "connection_env")?
        .ok_or("connection_env is required: the name of an environment variable holding the connection URL")?;
    if name.is_empty()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return Err("connection_env must be an environment variable name".into());
    }
    env(name)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| format!("environment variable {name} is not set"))
}

/// A planned CLI run: program, arguments, extra environment, statement on stdin and a label.
pub(crate) struct Plan {
    pub program: &'static str,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub stdin: String,
    pub label: String,
}

pub(crate) fn plan(
    ws: &Workspace,
    args: &Value,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<(Plan, u64), String> {
    let engine = req_str(args, "engine")?;
    let (body, kind) = classify(req_str(args, "sql")?)?;
    let limit = opt_u64(args, "limit")?.unwrap_or(DEFAULT_LIMIT);
    if !(1..=MAX_LIMIT).contains(&limit) {
        return Err(format!("limit must be 1..={MAX_LIMIT}"));
    }
    let timeout = opt_u64(args, "timeout_secs")?.unwrap_or(DEFAULT_TIMEOUT);
    if !(1..=MAX_TIMEOUT).contains(&timeout) {
        return Err(format!("timeout_secs must be 1..={MAX_TIMEOUT}"));
    }
    // One extra row tells a capped result from an exact one.
    let statement = match kind {
        Statement::Rows => format!(
            "SELECT * FROM (\n{body}\n) AS dial_rows LIMIT {}",
            limit + 1
        ),
        Statement::Meta => body,
    };
    let plan = match engine {
        "postgres" => {
            let target = parse_url(&connection(args, env)?, &["postgres", "postgresql"])?;
            let mut vars = vec![
                (
                    "PGOPTIONS".to_owned(),
                    format!(
                        "-c default_transaction_read_only=on -c statement_timeout={}",
                        timeout * 1000
                    ),
                ),
                ("PGCONNECT_TIMEOUT".to_owned(), "10".to_owned()),
                ("PGAPPNAME".to_owned(), "dial-db_query".to_owned()),
            ];
            for (var, value) in [
                ("PGHOST", target.host.clone()),
                ("PGPORT", target.port.map(|port| port.to_string())),
                ("PGUSER", target.user.clone()),
                ("PGPASSWORD", target.password.clone()),
                ("PGDATABASE", target.database.clone()),
            ] {
                if let Some(value) = value {
                    vars.push((var.to_owned(), value));
                }
            }
            for (key, value) in &target.options {
                let var = match key.as_str() {
                    "sslmode" => "PGSSLMODE",
                    "sslrootcert" => "PGSSLROOTCERT",
                    "sslcert" => "PGSSLCERT",
                    "sslkey" => "PGSSLKEY",
                    "options" => continue,
                    other => return Err(format!("unsupported postgres URL option {other:?}")),
                };
                vars.push((var.to_owned(), value.clone()));
            }
            Plan {
                program: "psql",
                args: strings(&["-X", "-q", "-v", "ON_ERROR_STOP=1", "--csv", "-f", "-"]),
                env: vars,
                stdin: format!("{statement};\n"),
                label: format!("postgres {}", target.label()),
            }
        }
        "mysql" => {
            let target = parse_url(&connection(args, env)?, &["mysql", "mariadb"])?;
            let mut argv = strings(&["--batch", "--connect-timeout=10"]);
            if let Some(host) = &target.host {
                argv.push(format!("--host={host}"));
            }
            if let Some(port) = target.port {
                argv.push(format!("--port={port}"));
            }
            if let Some(user) = &target.user {
                argv.push(format!("--user={user}"));
            }
            if let Some(database) = &target.database {
                argv.push(format!("--database={database}"));
            }
            if let Some((key, _)) = target.options.first() {
                return Err(format!("unsupported mysql URL option {key:?}"));
            }
            let vars = target
                .password
                .clone()
                .map(|password| ("MYSQL_PWD".to_owned(), password))
                .into_iter()
                .collect();
            Plan {
                program: "mysql",
                args: argv,
                env: vars,
                stdin: format!(
                    "SET SESSION TRANSACTION READ ONLY;\nSTART TRANSACTION READ ONLY;\n{statement};\nROLLBACK;\n"
                ),
                label: format!("mysql {}", target.label()),
            }
        }
        "sqlite" => {
            let path = opt_str(args, "path")?.ok_or("sqlite needs `path`")?;
            let resolved = ws.resolve_read(path)?;
            if !resolved.is_file() {
                return Err(format!("{path} is not a file"));
            }
            Plan {
                program: "sqlite3",
                args: vec![
                    "-readonly".into(),
                    "-safe".into(),
                    "-bail".into(),
                    "-csv".into(),
                    "-header".into(),
                    "-cmd".into(),
                    format!(".timeout {}", timeout * 1000),
                    resolved.display().to_string(),
                ],
                env: Vec::new(),
                stdin: format!("{statement};\n"),
                label: format!("sqlite {}", ws.rel(&resolved)),
            }
        }
        other => {
            return Err(format!(
                "engine must be one of {}, got {other:?}",
                ENGINES.join(", ")
            ));
        }
    };
    Ok((plan, limit))
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| (*item).to_owned()).collect()
}

/// Caps the result at `limit` data rows below one header line, then by bytes.
pub(crate) fn render(label: &str, stdout: &str, limit: u64) -> String {
    let mut lines = stdout.lines();
    let mut out = format!("Live query on {label} (read-only; results are untrusted data):\n");
    let header = lines.next();
    if let Some(header) = header {
        out.push_str(header);
        out.push('\n');
    }
    let mut rows = 0u64;
    let mut more = false;
    for line in lines {
        if rows == limit {
            more = true;
            break;
        }
        if out.len() + line.len() + 1 > MAX_OUTPUT {
            out.push_str("[output byte cap reached]\n");
            more = true;
            break;
        }
        out.push_str(line);
        out.push('\n');
        rows += 1;
    }
    out.push_str(&if more {
        format!("[{rows} rows shown; more rows exist, narrow the query or raise limit]\n")
    } else if header.is_none() {
        "[no output]\n".to_owned()
    } else {
        format!("[{rows} rows]\n")
    });
    out
}

fn execute(
    ws: &Workspace,
    args: &Value,
    env: &dyn Fn(&str) -> Option<String>,
    cancel: &AtomicBool,
) -> Result<String, String> {
    let (plan, limit) = plan(ws, args, env)?;
    let timeout = opt_u64(args, "timeout_secs")?.unwrap_or(DEFAULT_TIMEOUT);
    let mut command = Command::new(program(plan.program)?);
    command
        .args(&plan.args)
        .current_dir(ws.root())
        .envs(plan.env.iter().map(|(key, value)| (key, value)))
        .env("LC_MESSAGES", "C")
        .env("PAGER", "cat");
    // Add margin over the statement timeout for connection setup.
    let wall = Duration::from_secs(timeout + 15);
    let output = crate::runtime::process::run(
        command,
        Some(plan.stdin.into_bytes()),
        wall,
        MAX_CAPTURE,
        cancel,
        &mut |_, _| {},
    )?;
    if output.cancelled || output.timed_out || output.io_error.is_some() {
        return Err(output.failure(&plan.label, wall));
    }
    if !output.success() {
        let stderr = output.text(Stream::Stderr);
        let detail: String = stderr.trim().chars().take(2000).collect();
        return Err(format!("{} failed: {detail}", plan.label));
    }
    Ok(render(&plan.label, &output.stdout_text(), limit))
}

fn program(name: &str) -> Result<OsString, String> {
    #[cfg(test)]
    if let Some(program) = tests::program_override(name) {
        return Ok(program);
    }
    let exe = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_owned()
    };
    std::env::var_os("PATH")
        .iter()
        .flat_map(std::env::split_paths)
        .map(|dir: PathBuf| dir.join(&exe))
        .find(|candidate| candidate.is_file())
        .map(PathBuf::into_os_string)
        .ok_or_else(|| format!("{name} is not installed or not on PATH; db_query runs the engine's own command-line client"))
}

#[cfg(test)]
mod tests;
