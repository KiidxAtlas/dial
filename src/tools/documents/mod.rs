//! Read-only rich documents. Parsers never extract archive members to disk or execute notebook cells.
mod archives;
mod pdf;
mod sqlite;
#[cfg(test)]
mod tests;
use super::select::{self, MAX_READ_LINES, MAX_READ_OUTPUT, Selector, plan_spans};
use super::{Workspace, opt_u64, req_str};
use serde_json::Value;
use std::{fs, io::Read, path::Path};
const MAX_INPUT: u64 = 64 * 1024 * 1024;
pub(super) const MAX_CONTENT: u64 = 8 * 1024 * 1024;

/// PDF invokes an operator-installed parser; other formats use in-process read-only parsers.
pub fn needs_approval(args: &Value) -> bool {
    args.get("path").and_then(Value::as_str).is_some_and(|s| {
        s.split(':')
            .any(|p| p.to_ascii_lowercase().ends_with(".pdf"))
    })
}
fn kind(path: &str) -> Option<&'static str> {
    let lower = path.to_ascii_lowercase();
    if lower.ends_with(".pdf") {
        Some("pdf")
    } else if lower.ends_with(".ipynb") {
        Some("notebook")
    } else if [".zip", ".jar", ".whl", ".apk"]
        .iter()
        .any(|s| lower.ends_with(s))
    {
        Some("zip")
    } else if [".tar", ".tar.gz", ".tgz"]
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
    } else if [".7z", ".rar", ".xz", ".bz2", ".zst", ".iso", ".cab"]
        .iter()
        .any(|s| lower.ends_with(s))
    {
        Some("unsupported-archive")
    } else {
        None
    }
}
/// None leaves ordinary files and hashline snapshots completely unchanged.
pub(super) fn read(ws: &Workspace, args: &Value) -> Option<Result<String, String>> {
    let input = match req_str(args, "path") {
        Ok(s) => s,
        Err(e) => return Some(Err(e)),
    };
    let literal = ws.resolve(input).is_ok_and(|p| p.is_file());
    let (target, selector) = if literal {
        (input, Selector::default())
    } else {
        match select::split_selector(input) {
            Ok(v) => v,
            Err(e) => return Some(Err(e)),
        }
    };
    let (base, member) = if literal || ws.resolve(target).is_ok_and(|p| p.is_file()) {
        (target, None)
    } else {
        target
            .char_indices()
            .rev()
            .filter(|(_, c)| *c == ':')
            .find_map(|(i, _)| {
                let base = &target[..i];
                (kind(base).is_some() && ws.resolve(base).is_ok_and(|p| p.is_file()))
                    .then_some((base, Some(&target[i + 1..])))
            })
            .unwrap_or_else(|| {
                target
                    .split_once(':')
                    .map_or((target, None), |(a, b)| (a, Some(b)))
            })
    };
    let format = kind(base)?;
    Some((|| {
        let offset = opt_u64(args, "offset")?.unwrap_or(1);
        let limit = opt_u64(args, "limit")?.unwrap_or(200);
        if offset == 0 || limit == 0 {
            return Err("offset and limit must be >=1".into());
        }
        if args.get("offset").is_some_and(|v| !v.is_null()) && selector.selects_lines() {
            return Err("use offset or a line selector, not both".into());
        }
        let path = ws.resolve(base)?;
        let meta = fs::metadata(&path).map_err(|e| format!("reading {base:?}: {e}"))?;
        if !meta.is_file() || meta.len() > MAX_INPUT {
            return Err("rich document must be a regular file no larger than 64 MiB".into());
        }
        let text = match format {
            "notebook" => notebook(&path, member)?,
            "zip" | "tar" | "gzip" => archives::read(&path, format, member)?,
            "sqlite" => sqlite::read(&path, member)?,
            "pdf" => pdf::read(ws, &path, member)?,
            "unsupported-archive" => return Err("unsupported archive type; native rich read supports ZIP/JAR/WHL/APK, TAR/TAR.GZ/TGZ, and gzip only. No external extractor is run automatically".into()),
            _ => unreachable!(),
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
pub(super) fn bounded(reader: impl Read) -> Result<Vec<u8>, String> {
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
pub(super) fn text(data: Vec<u8>) -> Result<String, String> {
    if data.contains(&0) {
        return Err(
            "selected member is binary (NUL bytes); only UTF-8 text members are supported".into(),
        );
    }
    String::from_utf8(data).map_err(|_| "selected content is not UTF-8 text".into())
}
fn notebook(path: &Path, member: Option<&str>) -> Result<String, String> {
    let bytes = bounded(fs::File::open(path).map_err(|e| e.to_string())?)?;
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
fn render(
    ws: &Workspace,
    label: &str,
    text: &str,
    sel: &Selector,
    offset: usize,
    limit: usize,
) -> Result<String, String> {
    let lines: Vec<_> = text.lines().collect();
    let spans = plan_spans(sel, offset, limit, lines.len());
    let mut out = format!("[{label}; extracted text, not editable source anchors]\n");
    let mut shown = 0;
    let mut clipped = false;
    for span in spans {
        for n in span.start..=span.end {
            let line: String = lines[n - 1].chars().take(2000).collect();
            let row = if sel.raw {
                format!("{line}\n")
            } else {
                format!("{n:>6}\t{line}\n")
            };
            if out.len() + row.len() > MAX_READ_OUTPUT {
                clipped = true;
                break;
            }
            out.push_str(&row);
            shown += 1;
            clipped |= line.len() < lines[n - 1].len();
        }
        if out.len() >= MAX_READ_OUTPUT || clipped {
            break;
        }
    }
    out.push_str(&format!(
        "[{shown} extracted lines shown; {} total; clipped={clipped}]\n",
        lines.len()
    ));
    if let Some(store) = ws.artifacts() {
        let uri = store
            .retain(text)
            .map_err(|e| format!("retaining document output: {e:#}"))?;
        out.push_str(&format!("[complete extracted text: {uri}]\n"));
    }
    Ok(out)
}
