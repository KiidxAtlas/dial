//! Source parsing. Diagnostics are advisory, except that small/lean local tiers refuse edits that
//! add parser errors to a high-confidence grammar (see [`refusal`]). Never a type or behavior check.
use ast_grep_core::{AstGrep, Language};
use ast_grep_language::SupportLang;
use serde_json::{Value, json};
use std::path::Path;

/// ast-grep maps `.zsh` (and zsh rc names) to the Bash grammar, which reports false parse errors on
/// zsh-only syntax. No zsh grammar is linked, so zsh is reported unsupported and never blocks anything.
fn language(path: &Path) -> Option<SupportLang> {
    if is_zsh(path) {
        return None;
    }
    SupportLang::from_path(path)
}
fn is_zsh(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()) == Some("zsh")
        || matches!(
            path.file_name().and_then(|n| n.to_str()),
            Some(".zshrc" | "zshrc" | ".zprofile" | ".zlogin")
        )
}
pub fn report(path: &Path, content: &str) -> Value {
    if content.len() > 4 * 1024 * 1024 {
        return json!({"supported":false,"reason":"source exceeds parser budget"});
    }
    let Some(lang) = language(path) else {
        return json!({"supported":false,"reason":"no syntax parser for this extension"});
    };
    match AstGrep::try_new(content, lang) {
        Ok(tree) => {
            let errors: Vec<_> = tree.root().dfs().filter(|node|node.is_error()||node.is_missing()).take(33)
                .map(|node| {let pos=node.start_pos();json!({"line":pos.line()+1,"column":pos.column(&node)+1,"missing":node.is_missing()})}).collect();
            json!({"supported":true,"language":lang.to_string(),"errors":errors,"valid":errors.is_empty()})
        }
        Err(_) => json!({"supported":false,"reason":"parser unavailable"}),
    }
}

pub fn advisory(path: &Path, before: &str, proposed: &str) -> Option<String> {
    let after = report(path, proposed);
    let new = after["errors"].as_array().map_or(0, Vec::len);
    if new == 0 {
        return None;
    }
    let old = report(path, before)["errors"]
        .as_array()
        .map_or(0, Vec::len);
    Some(format!(
        "Syntax advisory for {}: {} parser errors (previously {}). First error: {}. The change was not blocked; the parser may not support this dialect. Use the language's own checks. This is not a type or behavioral check.",
        path.display(),
        new,
        old,
        after["errors"][0]
    ))
}
/// Grammars whose tree-sitter parsers are trusted enough to refuse an edit that adds errors.
fn high_confidence(lang: SupportLang) -> bool {
    matches!(
        lang,
        SupportLang::Rust
            | SupportLang::Python
            | SupportLang::TypeScript
            | SupportLang::Tsx
            | SupportLang::JavaScript
            | SupportLang::Go
    )
}
/// Error/missing node positions (1-based line, column, missing), bounded so huge broken files stay cheap.
fn error_positions(lang: SupportLang, content: &str) -> Option<Vec<(usize, usize, bool)>> {
    let tree = AstGrep::try_new(content, lang).ok()?;
    Some(
        tree.root()
            .dfs()
            .filter(|node| node.is_error() || node.is_missing())
            .take(4096)
            .map(|node| {
                let pos = node.start_pos();
                (pos.line() + 1, pos.column(&node) + 1, node.is_missing())
            })
            .collect(),
    )
}
/// The refusal for an edit that raises the parser error count of a high-confidence grammar, naming
/// the first new error with a few lines of the would-be result. `None` lets the edit proceed.
pub(crate) fn refusal(path: &Path, rel: &str, before: &str, after: &str) -> Option<String> {
    if after.len() > 4 * 1024 * 1024 || before.len() > 4 * 1024 * 1024 {
        return None;
    }
    let lang = language(path).filter(|lang| high_confidence(*lang))?;
    let new = error_positions(lang, after)?;
    if new.is_empty() {
        return None;
    }
    let old = error_positions(lang, before)?;
    if new.len() <= old.len() {
        return None;
    }
    let old_lines: Vec<&str> = before.lines().collect();
    let new_lines: Vec<&str> = after.lines().collect();
    let prefix = old_lines
        .iter()
        .zip(&new_lines)
        .take_while(|(a, b)| a == b)
        .count();
    let suffix = old_lines[prefix..]
        .iter()
        .rev()
        .zip(new_lines[prefix..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let changed = prefix + 1..=new_lines.len().saturating_sub(suffix).max(prefix + 1);
    let &(line, column, missing) = new
        .iter()
        .find(|(line, ..)| changed.contains(line))
        .or_else(|| new.iter().find(|error| !old.contains(error)))
        .unwrap_or(&new[0]);
    let last = new_lines.len().max(1);
    let window = (line.saturating_sub(2).max(1)..=(line + 2).min(last))
        .map(|n| format!("{n}:{}", new_lines.get(n - 1).copied().unwrap_or("")))
        .collect::<Vec<_>>()
        .join("\n");
    Some(format!(
        "Edit refused: it would add {lang} syntax errors to {rel} ({} before, {} after); nothing was written. First new error at {line}:{column} ({}). Would-be result:\n{window}\nFix the edit text and retry; earlier reads and tags stay valid. If the code is valid, retry with allow_syntax_errors: true.",
        old.len(),
        new.len(),
        if missing {
            "missing token"
        } else {
            "unexpected text"
        },
    ))
}
/// Block resolution, adapted to Dial's already-linked tree-sitter grammars.
/// Parse once for all requested anchors; never swallow a statement-sequence container or
/// an error-recovery subtree. Unsupported grammars return None to the explicit fallback.
pub(crate) fn block_ends(
    path: &Path,
    source: &str,
    requested: &[usize],
) -> Option<std::collections::BTreeMap<usize, Result<usize, String>>> {
    if is_zsh(path) {
        return None;
    }
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    let lang = SupportLang::from_path(path).or_else(|| {
        matches!(
            name,
            ".bashrc"
                | ".zshrc"
                | ".bash_profile"
                | ".bash_logout"
                | ".zprofile"
                | ".zlogin"
                | ".profile"
                | "bashrc"
                | "zshrc"
        )
        .then(|| SupportLang::from_path(Path::new("source.sh")))
        .flatten()
    })?;
    let tree = AstGrep::try_new(source, lang).ok()?;
    let root = tree.root().get_inner_node();
    let wanted: std::collections::BTreeSet<_> = requested.iter().copied().collect();
    let mut output = std::collections::BTreeMap::new();
    let mut offset = 0;
    for (row, text) in source.split_inclusive('\n').enumerate() {
        let line = row + 1;
        if wanted.contains(&line) {
            let resolved = (|| {
                let column = text
                    .bytes()
                    .position(|byte| {
                        byte != b' ' && byte != b'\t' && byte != b'\r' && byte != b'\n'
                    })
                    .ok_or_else(|| format!("line {line} is blank; anchor a construct opener"))?;
                let mut node = root
                    .named_descendant_for_byte_range(offset + column, offset + column + 1)
                    .ok_or_else(|| format!("line {line} has no syntax block"))?;
                if node.id() == root.id() {
                    return Err(format!(
                        "line {line} does not open a syntax block; use an explicit range"
                    ));
                }
                if node.start_position().row != row {
                    return Err(format!(
                        "line {line} is a continuation or closing delimiter, not a block opener"
                    ));
                }
                while let Some(parent) = node.parent() {
                    if parent.id() == root.id() || parent.start_position().row != row {
                        break;
                    }
                    if matches!(
                        parent.kind(),
                        "statement_list"
                            | "block"
                            | "body_statement"
                            | "statements"
                            | "body"
                            | "indented_block"
                            | "block_mapping"
                            | "block_sequence"
                            | "import_list"
                            | "hash_literal_body"
                    ) && parent.start_byte() == node.start_byte()
                    {
                        let mut next = node.next_named_sibling();
                        let mut skipped = false;
                        while next.is_some_and(|node| node.is_extra()) {
                            skipped = true;
                            next = next.unwrap().next_named_sibling();
                        }
                        if next.is_some_and(|next| {
                            next.start_position().row > node.end_position().row
                                && next.start_position().column == node.start_position().column
                        }) || next.is_none() && skipped
                        {
                            break;
                        }
                    }
                    node = parent;
                }
                if node.has_error() {
                    return Err(format!(
                        "line {line} starts a block with syntax errors; use an explicit range"
                    ));
                }
                let end = node.end_position();
                let last = end.row + usize::from(end.column > 0);
                if last <= line {
                    return Err(format!(
                        "line {line} is a single-line construct; use the explicit range form `{line}.={line}` instead of `{line}*`"
                    ));
                }
                Ok(last)
            })();
            output.insert(line, resolved);
        }
        offset += text.len();
    }
    for line in wanted {
        output
            .entry(line)
            .or_insert_with(|| Err(format!("line {line} is outside the file")));
    }
    Some(output)
}

pub(crate) type RepairCallback<'a> =
    &'a dyn Fn(&crate::tools::EditRepairRequest) -> Result<String, String>;
pub(crate) struct PreparedSource {
    pub text: String,
    pub warnings: Vec<String>,
    pub blackbox: Option<Value>,
}
/// Repair is a real controller/model callback; it cannot request tools or expand mutation targets.
/// Failed/invalid repairs retain the requested edit with an explicit warning.
/// Repair is a real controller/model callback; it cannot request tools or expand mutation targets.
/// Failed/invalid repairs retain the requested edit with an explicit warning. With `refuse`, a
/// result that still adds parser errors is an error before anything is committed.
pub(crate) fn prepare_edit(
    ws: &crate::tools::Workspace,
    path: &Path,
    before: &str,
    proposed: &str,
    proposed_seen: Option<&[bool]>,
    repair: Option<RepairCallback<'_>>,
    refuse: bool,
) -> Result<PreparedSource, String> {
    let settings = &ws.edit_settings;
    if settings.auto_repair.enabled && repair.is_none() && ws.edit_repair.0.is_none() {
        return Err(
            "edit.auto_repair.enabled requires a current-owner model repair callback".into(),
        );
    }
    if settings.blackbox.enabled && ws.edit_blackbox_dir.is_none() {
        return Err("edit.blackbox.enabled requires a private session recording directory".into());
    }
    let mut output = PreparedSource {
        text: proposed.to_owned(),
        warnings: Vec::new(),
        blackbox: None,
    };
    if !settings.auto_repair.enabled
        && !settings.blackbox.enabled
        && !ws.syntax_guard_enabled()
        && !refuse
    {
        return Ok(output);
    }
    let prior = report(path, before);
    let after = report(path, proposed);
    let regression =
        prior["valid"] == true && after["supported"] == true && after["valid"] == false;
    if regression && settings.blackbox.enabled {
        output.blackbox =
            Some(json!({"path":ws.rel(path),"before":before,"after":proposed,"diagnostics":after}));
    }
    if regression && settings.auto_repair.enabled {
        let allowed = settings.enforce_seen_lines.then(|| {
            proposed_seen.map_or_else(
                || {
                    let old = super::hashline::normalize_body(before);
                    let prior = ws.store().witness_flags(path, &old);
                    super::hashline::transition_flags(
                        &old,
                        &super::hashline::normalize_body(proposed),
                        &prior,
                    )
                },
                <[bool]>::to_vec,
            )
        });
        let mut diagnostics = after.clone();
        if let Some(flags) = &allowed {
            diagnostics["repair_allowed_lines"] = json!(super::hashline::ranges_from_flags(flags));
        }
        let request = crate::tools::EditRepairRequest {
            path: ws.rel(path),
            before: before.to_owned(),
            proposed: proposed.to_owned(),
            diagnostics,
        };
        let result = match repair {
            Some(callback) => callback(&request),
            None => ws.edit_repair.0.as_ref().unwrap().repair(&request),
        };
        match result {
            Ok(fixed) if fixed.len()<=super::edit_transaction::MAX_FILE && report(path,&fixed)["valid"]==true && bounded_repair(proposed,&fixed,&after)&&allowed.as_ref().is_none_or(|flags|repair_seen(proposed,&fixed,flags))=>{
                output.text=fixed;output.warnings.push(format!("Auto-repair fixed the parse regression in {} (validated by reparse; not a type or behavior check)",ws.rel(path)));
            },
            Ok(_)=>output.warnings.push(format!("Auto-repair for {} was rejected: source was oversized, still invalid, changed unseen source, or changed outside the bounded parser-error region; requested edit retained",ws.rel(path))),
            Err(error)=>output.warnings.push(format!("Auto-repair for {} failed: {error}; requested edit retained",ws.rel(path))),
        }
    }
    if refuse && let Some(refused) = refusal(path, &ws.rel(path), before, &output.text) {
        return Err(refused);
    }
    if let Some(note) = advisory(path, before, &output.text) {
        output.warnings.push(note);
    }
    Ok(output)
}
fn bounded_repair(proposed: &str, fixed: &str, diagnostics: &Value) -> bool {
    let old: Vec<_> = proposed.lines().collect();
    let new: Vec<_> = fixed.lines().collect();
    let prefix = old.iter().zip(&new).take_while(|(a, b)| a == b).count();
    let suffix = old[prefix..]
        .iter()
        .rev()
        .zip(new[prefix..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let end = old.len().saturating_sub(suffix).max(prefix + 1);
    if end.saturating_sub(prefix) > 64 || new.len().saturating_sub(prefix + suffix) > 64 {
        return false;
    }
    let errors: Vec<usize> = diagnostics["errors"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|e| e["line"].as_u64().map(|n| n as usize))
        .collect();
    (prefix + 1..=end).all(|line| errors.iter().any(|error| line.abs_diff(*error) <= 8))
}
fn repair_seen(proposed: &str, fixed: &str, flags: &[bool]) -> bool {
    let old: Vec<_> = proposed.lines().collect();
    let new: Vec<_> = fixed.lines().collect();
    let prefix = old.iter().zip(&new).take_while(|(a, b)| a == b).count();
    let suffix = old[prefix..]
        .iter()
        .rev()
        .zip(new[prefix..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let end = old.len().saturating_sub(suffix);
    if prefix == end {
        return old.is_empty()
            || (prefix == 0 || flags.get(prefix - 1) == Some(&true))
                && (prefix == old.len() || flags.get(prefix) == Some(&true));
    }
    (prefix..end).all(|line| flags.get(line) == Some(&true))
}
/// Opt-in full-source records: bounded count/bytes, private modes, no redaction or secrecy claim.
/// Called only after owner authorization and workspace commit.
#[cfg(unix)]
pub(crate) fn record_blackbox(ws: &crate::tools::Workspace, record: &Value) -> Result<(), String> {
    use std::{
        fs::{self, OpenOptions},
        io::Write,
        os::unix::fs::{OpenOptionsExt, PermissionsExt},
        sync::atomic::{AtomicU64, Ordering},
    };
    static RECORD: AtomicU64 = AtomicU64::new(0);
    let dir = ws
        .edit_blackbox_dir
        .as_ref()
        .ok_or("missing edit blackbox directory")?;
    fs::create_dir_all(dir).map_err(|e| format!("creating edit blackbox: {e}"))?;
    let meta = fs::symlink_metadata(dir).map_err(|e| e.to_string())?;
    if !meta.is_dir() || meta.file_type().is_symlink() {
        return Err("edit blackbox must be a real private directory".into());
    }
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).map_err(|e| e.to_string())?;
    use std::os::fd::AsRawFd;
    let lock = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(dir)
        .map_err(|e| e.to_string())?;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } < 0 {
        return Err(format!(
            "locking private edit records: {}",
            std::io::Error::last_os_error()
        ));
    }
    let bytes = serde_json::to_vec(record).map_err(|e| e.to_string())?;
    if bytes.len() > 8 * 1024 * 1024 {
        return Err("parse-regression record exceeds 8 MiB; not recorded".into());
    }
    prune_records(dir, bytes.len())?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_nanos();
    let path = dir.join(format!(
        "edit-regression-{stamp}-{}-{}.json",
        std::process::id(),
        RECORD.fetch_add(1, Ordering::Relaxed)
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|e| e.to_string())?;
    file.write_all(&bytes)
        .and_then(|_| file.sync_all())
        .map_err(|e| e.to_string())
}
#[cfg(windows)]
pub(crate) fn record_blackbox(ws: &crate::tools::Workspace, record: &Value) -> Result<(), String> {
    use std::{
        io::Write,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
    };
    static RECORD: AtomicU64 = AtomicU64::new(0);
    let dir = ws
        .edit_blackbox_dir
        .as_ref()
        .ok_or("missing edit blackbox directory")?;
    std::fs::create_dir_all(dir).map_err(|e| format!("creating edit blackbox: {e}"))?;
    let root = crate::platform::fs::open_dir_nofollow_write(dir).map_err(|e| e.to_string())?;
    crate::platform::fs::set_owner_only_file(&root).map_err(|e| e.to_string())?;
    if !crate::platform::fs::is_owner_only(&root).map_err(|e| e.to_string())? {
        return Err("edit blackbox must be a private directory".into());
    }
    let bytes = serde_json::to_vec(record).map_err(|e| e.to_string())?;
    if bytes.len() > 8 * 1024 * 1024 {
        return Err("parse-regression record exceeds 8 MiB; not recorded".into());
    }
    prune_records(dir, bytes.len())?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_nanos();
    let name = PathBuf::from(format!(
        "edit-regression-{stamp}-{}-{}.json",
        std::process::id(),
        RECORD.fetch_add(1, Ordering::Relaxed)
    ));
    let mut file =
        crate::platform::fs::create_new_beneath(&root, &name, true).map_err(|e| e.to_string())?;
    file.write_all(&bytes)
        .and_then(|_| file.sync_all())
        .map_err(|e| e.to_string())
}

/// Enforces the private record budget (at most 31 existing records and 16 MiB in total, pruning
/// oldest first) before one more record of `incoming` bytes is written.
fn prune_records(dir: &Path, incoming: usize) -> Result<(), String> {
    let mut records = Vec::new();
    let mut total = incoming;
    for entry in std::fs::read_dir(dir).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with("edit-regression-") || !name.ends_with(".json") {
            continue;
        }
        let metadata = std::fs::symlink_metadata(entry.path()).map_err(|e| e.to_string())?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err("unexpected entry in edit blackbox".into());
        }
        total = total.saturating_add(metadata.len() as usize);
        records.push((
            metadata.modified().ok(),
            entry.path(),
            metadata.len() as usize,
        ));
        if records.len() > 64 {
            return Err("edit blackbox contains too many records to prune safely".into());
        }
    }
    records.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    while records.len() >= 32 || total > 16 * 1024 * 1024 {
        if records.is_empty() {
            return Err("edit blackbox byte budget exceeded".into());
        }
        let (_, path, len) = records.remove(0);
        std::fs::remove_file(path).map_err(|e| e.to_string())?;
        total = total.saturating_sub(len);
    }
    Ok(())
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod writes;
