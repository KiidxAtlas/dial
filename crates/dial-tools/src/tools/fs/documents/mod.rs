//! Read-only rich documents. Parsers never extract archive members to disk or execute notebook cells.
mod analysis;
mod archives;
mod layout;
mod table;
mod workbook;
pub use analysis::analysis_tools;
mod cache;
mod epub;
mod isolate;
use isolate::Job;
pub use isolate::{WORKER_ARG as DOCUMENT_WORKER_ARG, worker_main as document_worker_main};
mod binary_data;
mod certificate;
mod columnar;
mod delimited;
mod executable;
mod json;
mod mail;
mod office;
mod opendocument;
mod pdf;
mod property_list;
mod spreadsheet;
mod sqlite;
mod table_view;
#[cfg(test)]
mod tests;
use super::select::{self, MAX_READ_LINES, MAX_READ_OUTPUT, Selector, plan_spans};
use crate::tools::{Workspace, opt_u64, req_str};
use serde_json::Value;
use std::{
    fs,
    io::Read,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
pub(in crate::tools) const MAX_INPUT: u64 = 64 * 1024 * 1024;
const PARSE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(60);
pub(in crate::tools) const MAX_CONTENT: u64 = 8 * 1024 * 1024;

/// PDF invokes an operator-installed parser; other formats use in-process read-only parsers.
pub fn needs_approval(args: &Value) -> bool {
    args.get("path").and_then(Value::as_str).is_some_and(|s| {
        s.split(':')
            .any(|p| p.to_ascii_lowercase().ends_with(".pdf"))
    })
}
pub(in crate::tools) fn kind(path: &str) -> Option<&'static str> {
    let lower = path.to_ascii_lowercase();
    if lower.ends_with(".pdf") {
        Some("pdf")
    } else if lower.ends_with(".ipynb") {
        Some("notebook")
    } else if lower.ends_with(".docx") {
        Some("docx")
    } else if lower.ends_with(".xlsx") {
        Some("xlsx")
    } else if lower.ends_with(".pptx") {
        Some("pptx")
    } else if lower.ends_with(".epub") {
        Some("epub")
    } else if [".zip", ".jar", ".whl", ".apk"]
        .iter()
        .any(|s| lower.ends_with(s))
    {
        Some("zip")
    } else if [
        ".tar", ".tar.gz", ".tgz", ".tar.xz", ".txz", ".tar.bz2", ".tbz", ".tbz2", ".tar.zst",
        ".tzst",
    ]
    .iter()
    .any(|s| lower.ends_with(s))
    {
        Some("tar")
    } else if lower.ends_with(".gz") {
        Some("gzip")
    } else if [".db", ".sqlite", ".sqlite3"]
        .iter()
        .any(|s| lower.ends_with(s))
    {
        Some("sqlite")
    } else if [
        ".so", ".dylib", ".dll", ".exe", ".o", ".obj", ".a", ".lib", ".node", ".bundle",
    ]
    .iter()
    .any(|s| lower.ends_with(s))
    {
        Some("object")
    } else if let Some(format) = [
        (".parquet", "parquet"),
        (".arrow", "arrow"),
        (".arrows", "arrow"),
        (".feather", "arrow"),
        (".ipc", "arrow"),
        (".xls", "spreadsheet"),
        (".xlsb", "spreadsheet"),
        (".xlsm", "spreadsheet"),
        (".ods", "spreadsheet"),
        (".odt", "opendocument"),
        (".odp", "opendocument"),
        (".eml", "mail"),
        (".mbox", "mbox"),
        (".npy", "npy"),
        (".msgpack", "msgpack"),
        (".mpk", "msgpack"),
        (".ttf", "font"),
        (".otf", "font"),
        (".ttc", "font"),
        (".woff", "font"),
        (".woff2", "font"),
        (".wav", "wav"),
    ]
    .iter()
    .find_map(|(suffix, format)| lower.ends_with(suffix).then_some(*format))
    {
        Some(format)
    } else if [".pem", ".crt", ".cer", ".der", ".csr"]
        .iter()
        .any(|s| lower.ends_with(s))
    {
        Some("x509")
    } else if let Some(format) = [
        (".7z", "7z"),
        (".rar", "rar"),
        (".cab", "cab"),
        (".iso", "iso"),
        (".xz", "xz"),
        (".bz2", "bz2"),
        (".zst", "zst"),
    ]
    .iter()
    .find_map(|(suffix, format)| lower.ends_with(suffix).then_some(*format))
    {
        Some(format)
    } else {
        None
    }
}

/// `name` without a trailing `.gz`, `.zst`, `.xz` or `.bz2` compression suffix.
pub(in crate::tools) fn without_compression(name: &str) -> &str {
    [".gz", ".zst", ".xz", ".bz2"]
        .iter()
        .find_map(|suffix| name.strip_suffix(suffix))
        .unwrap_or(name)
}
/// None leaves ordinary files and hashline snapshots completely unchanged.
pub(in crate::tools) fn read_streaming(
    ws: &Workspace,
    args: &Value,
    cancel: &AtomicBool,
) -> Option<Result<String, String>> {
    read_streaming_impl(ws, args, cancel, None)
}

/// Read a file whose content (not its name) identified `format`, such as an extensionless
/// SQLite database or ELF binary.
pub(in crate::tools) fn read_streaming_as(
    ws: &Workspace,
    args: &Value,
    cancel: &AtomicBool,
    format: &'static str,
) -> Option<Result<String, String>> {
    read_streaming_impl(ws, args, cancel, Some(format))
}

fn read_streaming_impl(
    ws: &Workspace,
    args: &Value,
    cancel: &AtomicBool,
    forced: Option<&'static str>,
) -> Option<Result<String, String>> {
    let input = match req_str(args, "path") {
        Ok(s) => s,
        Err(e) => return Some(Err(e)),
    };
    let expanded = match ws.expand_file_handle(input) {
        Ok(path) => path,
        Err(error) => return Some(Err(error)),
    };
    let input = expanded.as_str();
    let resolved = ws.resolve_read(input);
    if resolved.as_ref().is_ok_and(|p| p.is_dir()) {
        return None;
    }
    let literal = resolved.is_ok_and(|p| p.is_file());
    // Literal filenames containing ':' or '?' always win. Bare JSON stays in
    // the ordinary source/hashline reader; only explicit query suffixes route here.
    if !literal {
        let query_at = input
            .char_indices()
            .filter(|(_, c)| *c == '?')
            .find(|(i, _)| {
                json::is_queryable(&input[..*i])
                    && ws.resolve_read(&input[..*i]).is_ok_and(|p| p.is_file())
            })
            .or_else(|| {
                input
                    .char_indices()
                    .find(|(i, c)| *c == '?' && json::is_queryable(&input[..*i]))
            });
        if let Some((i, _)) = query_at {
            return Some((|| {
                if ["offset", "limit"]
                    .iter()
                    .any(|key| args.get(*key).is_some_and(|v| !v.is_null()))
                {
                    return Err("JSON queries use zero-based ?offset=N and ?limit=N; do not combine them with source-line offset/limit arguments".into());
                }
                let path = ws.resolve_read(&input[..i])?;
                let opened = cache::Opened::open(ws, &path)?;
                let text = isolate::run(
                    "JSON query",
                    &Job::Read {
                        format: "json".into(),
                        name: input[..i].to_owned(),
                        member: Some(input[i + 1..].to_owned()),
                    },
                    &opened.bytes,
                    PARSE_DEADLINE,
                    cancel,
                )?;
                opened.validate(ws)?;
                Ok(text)
            })());
        }
    }
    let (target, selector) = if literal {
        (input, Selector::default())
    } else {
        match select::split_selector(input) {
            Ok(v) => v,
            Err(e) => return Some(Err(e)),
        }
    };
    let (base, member) = if literal || ws.resolve_read(target).is_ok_and(|p| p.is_file()) {
        (target, None)
    } else {
        target
            .char_indices()
            .rev()
            .filter(|(_, c)| *c == ':')
            .find_map(|(i, _)| {
                let base = &target[..i];
                ((kind(base).is_some() || delimited::format_of(base).is_some())
                    && ws.resolve_read(base).is_ok_and(|p| p.is_file()))
                .then_some((base, Some(&target[i + 1..])))
            })
            .unwrap_or_else(|| {
                target
                    .split_once(':')
                    .map_or((target, None), |(a, b)| (a, Some(b)))
            })
    };
    // A bare uncompressed CSV/TSV keeps the editable source view; a `:rows=`/`:table` selector
    // or a compressed file reads it as a table.
    let delimited = forced
        .is_none()
        .then(|| delimited::format_of(base))
        .flatten()
        .filter(|_| member.is_some() || without_compression(base) != base);
    let format = match delimited {
        Some(format) => format,
        None => forced.or_else(|| kind(base))?,
    };
    Some((|| {
        let offset = opt_u64(args, "offset")?.unwrap_or(1);
        let limit = opt_u64(args, "limit")?.unwrap_or(200);
        if offset == 0 || limit == 0 {
            return Err("offset and limit must be >=1".into());
        }
        if args.get("offset").is_some_and(|v| !v.is_null()) && selector.selects_lines() {
            return Err("use offset or a line selector, not both".into());
        }
        let path = ws.resolve_read(base)?;
        if cancel.load(Ordering::Acquire) {
            return Err("document read cancelled".into());
        }
        let opened = cache::Opened::open(ws, &path)?;
        let text: Arc<String> = if format == "sqlite" {
            // SQLite is live database state, not an immutable extraction.
            let text = parse_isolated(
                "sqlite",
                Job::Sqlite {
                    path,
                    member: member.map(str::to_owned),
                },
                cancel,
            )?;
            opened.validate(ws)?;
            let sidecars = crate::tools::fs::binary::sqlite_sidecars(ws, base);
            Arc::new(format!("{sidecars}{text}"))
        } else {
            cache::extracted(&opened, ws, format, member, || {
                if format == "pdf" {
                    return pdf::read(ws, &opened.file()?, member, cancel);
                }
                isolate::run(
                    "document",
                    &Job::Read {
                        format: format.to_owned(),
                        name: base.to_owned(),
                        member: member.map(str::to_owned),
                    },
                    &opened.bytes,
                    PARSE_DEADLINE,
                    cancel,
                )
            })?
        };
        render(
            ws,
            input,
            &text,
            &selector,
            offset.min(usize::MAX as u64) as usize,
            limit.min(MAX_READ_LINES as u64) as usize,
        )
    })())
}
/// Parses in a worker process (see `isolate`), so a crash, hang, or runaway allocation in a parser
/// cannot take down the agent. PDF runs its own `pdftotext` child and is never routed through here.
fn parse_isolated(label: &str, job: Job, cancel: &AtomicBool) -> Result<String, String> {
    isolate::run(label, &job, &[], PARSE_DEADLINE, cancel)
}

/// Whether `format` (as [`kind`] reports it) is an archive or compressed stream.
pub(in crate::tools) fn is_archive(format: &str) -> bool {
    matches!(
        format,
        "zip" | "tar" | "gzip" | "xz" | "bz2" | "zst" | "7z" | "rar" | "cab" | "iso"
    )
}

/// Formats decoded straight from the authorized bytes in the worker. `None` for other formats.
fn read_payload(
    format: &str,
    payload: &[u8],
    member: Option<&str>,
) -> Option<Result<String, String>> {
    let without_member = |read: fn(&[u8]) -> Result<String, String>| {
        Some(match member {
            Some(member) => Err(format!(
                "{format} files have no :{member} selector; read the whole file"
            )),
            None => read(payload),
        })
    };
    match format {
        "x509" => without_member(certificate::read),
        "object" => without_member(executable::read),
        "plist" => without_member(property_list::read),
        "mail" => without_member(mail::message),
        "mbox" => Some(mail::mbox(payload, member)),
        "parquet" => Some(columnar::parquet(payload, member)),
        "arrow" => Some(columnar::arrow(payload, member)),
        "spreadsheet" => Some(spreadsheet::read(payload, member)),
        "opendocument" => without_member(opendocument::read),
        "npy" => without_member(binary_data::npy),
        "msgpack" => without_member(binary_data::msgpack),
        "font" => without_member(binary_data::font),
        "wav" => without_member(binary_data::wav),
        "csv" | "tsv" => Some(delimited::read(format, payload, member)),
        _ => None,
    }
}

/// Read an archive held in memory (a URL download) through the isolated parser. `name` is the URL
/// path, whose suffix selects the tar compression; `label` names the source in errors.
pub(in crate::tools) fn read_archive_bytes(
    label: &str,
    name: &str,
    format: &str,
    member: Option<&str>,
    bytes: &[u8],
    cancel: &AtomicBool,
) -> Result<String, String> {
    let job = Job::ArchiveBytes {
        format: format.to_owned(),
        name: name.to_owned(),
        member: member.map(str::to_owned),
    };
    isolate::run(label, &job, bytes, PARSE_DEADLINE, cancel)
}

pub(in crate::tools) fn bounded(reader: impl Read) -> Result<Vec<u8>, String> {
    let mut data = Vec::new();
    reader
        .take(MAX_CONTENT + 1)
        .read_to_end(&mut data)
        .map_err(|e| format!("reading document content: {e}"))?;
    if data.len() as u64 > MAX_CONTENT {
        return Err("decoded content exceeds 8 MiB; select a smaller member/page/table".into());
    }
    Ok(data)
}
pub(in crate::tools) fn text(data: Vec<u8>) -> Result<String, String> {
    if data.contains(&0) {
        return Err(
            "selected member is binary (NUL bytes); only UTF-8 text members are supported".into(),
        );
    }
    String::from_utf8(data).map_err(|_| "selected content is not UTF-8 text".into())
}
fn notebook(file: fs::File, member: Option<&str>) -> Result<String, String> {
    let bytes = bounded(file)?;
    let value: Value =
        serde_json::from_slice(&bytes).map_err(|e| format!("invalid notebook JSON: {e}"))?;
    if value["nbformat"].as_u64() != Some(4) {
        return Err("only nbformat 4 notebooks are supported".into());
    }
    let cells = value["cells"]
        .as_array()
        .ok_or("notebook cells must be an array")?;
    let selected = member
        .map(|s| {
            s.strip_prefix("cell=")
                .ok_or("notebook selector must be :cell=N (1-based)")?
                .parse::<usize>()
                .map_err(|_| "invalid cell index")
        })
        .transpose()?;
    if selected.is_some_and(|n| n == 0 || n > cells.len()) {
        return Err(format!("cell index outside 1..={}", cells.len()));
    }
    let mut out = String::new();
    for (i, cell) in cells.iter().enumerate() {
        if selected.is_some_and(|n| n != i + 1) {
            continue;
        }
        let source = match &cell["source"] {
            Value::String(s) => s.clone(),
            Value::Array(parts) => parts
                .iter()
                .map(|p| p.as_str().ok_or("cell source array must contain strings"))
                .collect::<Result<Vec<_>, _>>()?
                .concat(),
            _ => return Err(format!("cell {} has invalid source", i + 1)),
        };
        out.push_str(&format!(
            "[cell {}: {}; execution_count={}]\n",
            i + 1,
            cell["cell_type"].as_str().unwrap_or("unknown"),
            cell["execution_count"]
        ));
        out.push_str(&source);
        if !source.ends_with('\n') {
            out.push('\n');
        }
        // Only textual output; attachments, HTML and image payloads are not executed or rendered.
        if let Some(outputs) = cell["outputs"].as_array() {
            for output in outputs {
                let data = output
                    .get("text")
                    .or_else(|| output.pointer("/data/text~1plain"));
                if let Some(data) = data {
                    out.push_str("[text output]\n");
                    match data {
                        Value::String(s) => out.push_str(s),
                        Value::Array(parts) => {
                            for p in parts {
                                if let Some(s) = p.as_str() {
                                    out.push_str(s);
                                }
                            }
                        }
                        _ => {}
                    }
                    out.push('\n');
                }
            }
        }
        if out.len() as u64 > MAX_CONTENT {
            return Err("notebook rendered content exceeds 8 MiB; select :cell=N".into());
        }
    }
    Ok(out)
}
pub(in crate::tools) fn render(
    ws: &Workspace,
    label: &str,
    text: &str,
    sel: &Selector,
    offset: usize,
    limit: usize,
) -> Result<String, String> {
    let line_count = text.lines().count();
    let spans = plan_spans(sel, offset, limit, line_count);
    let mut out = format!("[{label}; extracted text, not editable source anchors]\n");
    let mut shown = 0;
    let mut clipped = false;
    let mut full = false;
    let mut next_offset = offset;
    for span in spans {
        for (index, source) in text.lines().enumerate().take(span.end).skip(span.start - 1) {
            let n = index + 1;
            let end = source
                .char_indices()
                .nth(2000)
                .map_or(source.len(), |(i, _)| i);
            let line = &source[..end];
            let row = if sel.raw {
                format!("{line}\n")
            } else {
                format!("{n:>6}\t{line}\n")
            };
            if out.len() + row.len() > MAX_READ_OUTPUT {
                clipped = true;
                full = true;
                break;
            }
            out.push_str(&row);
            shown += 1;
            next_offset = n + 1;
            clipped |= line.len() < source.len();
        }
        if full || out.len() >= MAX_READ_OUTPUT {
            break;
        }
    }
    out.push_str(&format!(
        "[{shown} extracted lines shown; {} total; clipped={clipped}; next offset={next_offset}]\n",
        line_count
    ));
    if let Some(store) = ws.artifacts() {
        let uri = store
            .retain(text)
            .map_err(|e| format!("retaining document output: {e:#}"))?;
        out.push_str(&format!("[complete extracted text: {uri}]\n"));
    }
    Ok(out)
}

/// Convert an in-memory document body using the same bounded parsers used by `read`.
/// The filename supplies only the format; bytes are never extracted to disk or executed.
pub fn convert_bytes(ws: &Workspace, bytes: &[u8], filename: &str) -> Result<String, String> {
    let format = kind(filename)
        .or_else(|| delimited::format_of(filename))
        .ok_or_else(|| format!("unsupported document type: {filename}"))?;
    convert_format(ws, bytes, format)
}

/// Formats [`convert_format`] reads from memory with their local readers, beyond the
/// markdown conversions (PDF, OOXML, EPUB, notebooks and ZIP).
pub(in crate::tools) const IN_MEMORY_READERS: &[&str] = &[
    "parquet",
    "arrow",
    "spreadsheet",
    "opendocument",
    "mail",
    "mbox",
    "sqlite",
    "npy",
    "msgpack",
    "plist",
    "csv",
    "tsv",
];

/// [`convert_bytes`] for a format already known (as [`kind`] names it), such as a URL download
/// identified by media type or magic bytes.
pub(in crate::tools) fn convert_format(
    ws: &Workspace,
    bytes: &[u8],
    format: &str,
) -> Result<String, String> {
    if bytes.len() as u64 > MAX_INPUT {
        return Err("document exceeds the 64 MiB input limit".into());
    }
    match format {
        "pdf" => {
            use std::io::{Seek, Write};
            let mut file =
                tempfile::tempfile().map_err(|e| format!("creating parser input: {e}"))?;
            file.write_all(bytes)
                .map_err(|e| format!("writing parser input: {e}"))?;
            file.rewind()
                .map_err(|e| format!("rewinding parser input: {e}"))?;
            pdf::read(ws, &file, None, &AtomicBool::new(false))
        }
        "docx" | "xlsx" | "pptx" | "zip" | "tar" | "gzip" | "notebook" | "epub" => isolate::run(
            "document",
            &Job::Convert {
                format: format.to_owned(),
            },
            bytes,
            PARSE_DEADLINE,
            &AtomicBool::new(false),
        ),
        format if IN_MEMORY_READERS.contains(&format) => isolate::run(
            "document",
            &Job::Read {
                format: format.to_owned(),
                name: format!("document.{format}"),
                member: None,
            },
            bytes,
            PARSE_DEADLINE,
            &AtomicBool::new(false),
        ),
        _ => Err(format!("unsupported in-memory document type: {format}")),
    }
}

/// Read structured worksheet cells from authorized bytes in the bounded document worker.
pub(crate) fn table_bytes(
    bytes: &[u8],
    sheet: Option<&str>,
    cancel: &AtomicBool,
) -> Result<serde_json::Value, String> {
    if bytes.len() as u64 > MAX_INPUT {
        return Err("workbook exceeds 64 MiB".into());
    }
    let text = isolate::run(
        "worksheet",
        &Job::Table {
            sheet: sheet.map(str::to_owned),
        },
        bytes,
        PARSE_DEADLINE,
        cancel,
    )?;
    serde_json::from_str(&text).map_err(|e| format!("invalid worksheet worker output: {e}"))
}

/// Inspect worksheet metadata and formula text without evaluating formulas or trusting caches.
pub(crate) fn inspect_workbook_bytes(
    bytes: &[u8],
    cancel: &AtomicBool,
) -> Result<serde_json::Value, String> {
    if bytes.len() as u64 > MAX_INPUT {
        return Err("workbook exceeds 64 MiB".into());
    }
    let text = isolate::run(
        "workbook inspection",
        &Job::Workbook,
        bytes,
        PARSE_DEADLINE,
        cancel,
    )?;
    serde_json::from_str(&text).map_err(|e| format!("invalid workbook inspection output: {e}"))
}

/// Investigate authorized SQLite bytes in a disposable, resource-bounded parser process.
pub(crate) fn inspect_database_bytes(
    bytes: &[u8],
    action: &str,
    sql: Option<&str>,
    limit: usize,
    cancel: &AtomicBool,
) -> Result<serde_json::Value, String> {
    if bytes.len() > 16 * 1024 * 1024 {
        return Err("database snapshot exceeds 16 MiB".into());
    }
    let text = isolate::run(
        "database inspection",
        &Job::Database {
            action: action.to_owned(),
            sql: sql.map(str::to_owned),
            limit,
        },
        bytes,
        std::time::Duration::from_secs(10),
        cancel,
    )?;
    serde_json::from_str(&text).map_err(|e| format!("invalid database worker output: {e}"))
}

/// Worker side of `convert_bytes`: converts the worker-owned copy of the document. Runs only
/// inside the isolated worker process.
fn convert_stream(file: fs::File, format: &str) -> Result<String, String> {
    match format {
        "docx" | "xlsx" | "pptx" => office::read(file, format, None),
        "epub" => epub::read(file, None),
        "notebook" => notebook(file, None),
        "zip" => {
            let mut decoded = Vec::new();
            let mut archive =
                zip::ZipArchive::new(file).map_err(|e| format!("invalid ZIP: {e}"))?;
            for i in 0..archive.len() {
                let entry = archive.by_index(i).map_err(|e| e.to_string())?;
                if !entry.name().is_ok_and(|name| archives::safe(&name)) {
                    continue;
                }
                if entry.is_file() && entry.size() <= MAX_CONTENT {
                    let mut bytes = Vec::new();
                    entry
                        .take(MAX_CONTENT + 1)
                        .read_to_end(&mut bytes)
                        .map_err(|e| e.to_string())?;
                    if let Ok(text) = String::from_utf8(bytes) {
                        decoded.extend_from_slice(text.as_bytes());
                        decoded.push(b'\n');
                        if decoded.len() as u64 > MAX_CONTENT {
                            return Err(
                                "archive text exceeds 8 MiB; select a smaller member".into()
                            );
                        }
                    }
                }
            }
            text(decoded)
        }
        "tar" | "gzip" => Err(
            "in-memory conversion supports OOXML, EPUB and ZIP; use read for path-based archives"
                .into(),
        ),
        _ => Err(format!("unsupported in-memory document type: {format}")),
    }
}
