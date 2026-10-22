//! Bounded related-evidence bundles for `find`: the enclosing owner, lexical call-sites/mentions,
//! signature type definitions and config references around the top hits.
//!
//! Provenance is stated per row. `syntax` rows are tree-sitter declaration spans; `lexical` rows are
//! whole-word text matches in the searched scope. Neither is a resolved symbol or call graph (only the
//! optional language-server section of `find` is resolved), and no row is an edit witness.
use super::{MAX_FILE, Workspace, declarations};
use crate::tools::fs::walk::Walk;
use sha2::{Digest, Sha256};
use std::{fs, io::Read, path::Path};

const MAX_FILES: usize = 3000;
const MAX_BYTES: u64 = 24 * 1024 * 1024;
const MAX_SPAN: usize = 60;
const MAX_REFS: usize = 4;
const MAX_CONFIG: usize = 2;
const MAX_TYPES: usize = 2;
pub(super) const MAX_TARGETS: usize = 3;
const GENERIC_NAMES: &[&str] = &[
    "main", "test", "tests", "new", "default", "from", "into", "run", "get", "set", "init", "name",
    "build", "load", "save", "read", "write", "call", "next", "drop", "fmt", "clone",
];
const COMMON_TYPES: &[&str] = &[
    "Self", "Result", "Option", "Vec", "String", "Value", "Box", "Arc", "Some", "None", "Ok",
    "Err", "Path", "PathBuf", "Mutex", "BTreeMap", "BTreeSet", "HashMap", "HashSet", "Error",
    "Promise", "Array", "Map", "Set", "Object", "Number", "Boolean",
];

pub(super) struct Target {
    pub path: String,
    pub start: usize,
    pub end: usize,
    pub sha256: String,
}

struct Row {
    path: String,
    start: usize,
    end: usize,
    label: &'static str,
    provenance: &'static str,
    sha256: String,
    text: String,
    call: bool,
}
impl Row {
    fn render(&self) -> String {
        // Copied text follows the existing secret screening; a flagged line is withheld.
        let text = if crate::storage::memory::screen(&self.text).is_err() {
            "[line withheld: matches secret screening]"
        } else {
            self.text.as_str()
        };
        format!(
            "  {} {}:{}-{} provenance={} source_sha256={} {}\n",
            self.label, self.path, self.start, self.end, self.provenance, self.sha256, text
        )
    }
}

#[derive(Default)]
struct Plan {
    stale: bool,
    owner: Option<Row>,
    name: Option<String>,
    types: Vec<String>,
    refs: Vec<Row>,
    config: Vec<Row>,
    type_defs: Vec<(usize, Row)>,
}

fn read_source(ws: &Workspace, path: &Path) -> Option<(String, String)> {
    let meta = fs::symlink_metadata(path).ok()?;
    if !meta.is_file() || meta.file_type().is_symlink() || meta.len() > MAX_FILE {
        return None;
    }
    if !ws
        .resolve(&ws.rel(path))
        .is_ok_and(|resolved| resolved == path)
    {
        return None;
    }
    let mut data = Vec::with_capacity(meta.len() as usize);
    fs::File::open(path)
        .ok()?
        .take(MAX_FILE + 1)
        .read_to_end(&mut data)
        .ok()?;
    if data.len() as u64 > MAX_FILE || data.contains(&0) {
        return None;
    }
    let sha256 = format!("{:x}", Sha256::digest(&data));
    Some((String::from_utf8(data).ok()?, sha256))
}

fn ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}
fn find_word(line: &str, word: &str) -> Option<usize> {
    line.match_indices(word).map(|(at, _)| at).find(|at| {
        !line[..*at].chars().next_back().is_some_and(ident)
            && !line[at + word.len()..].chars().next().is_some_and(ident)
    })
}
fn clip(line: &str) -> String {
    let line = line.trim();
    let mut out: String = line.chars().take(160).collect();
    if out.len() < line.len() {
        out.push('…');
    }
    out
}
/// Declared name from a declaration's first line, using keywords common to the supported languages.
fn declared_name(line: &str) -> Option<String> {
    let words: Vec<&str> = line
        .split(|c: char| !ident(c))
        .filter(|w| !w.is_empty())
        .collect();
    let named = words
        .windows(2)
        .find(|pair| {
            matches!(
                pair[0],
                "fn" | "struct"
                    | "enum"
                    | "trait"
                    | "class"
                    | "def"
                    | "function"
                    | "interface"
                    | "type"
                    | "func"
            )
        })
        .map(|pair| pair[1]);
    let name = named.or_else(|| {
        // Methods and arrow functions: the identifier directly before the first '('.
        let open = line.find('(')?;
        let before = line[..open].trim_end();
        let start = before.rfind(|c: char| !ident(c)).map_or(0, |at| {
            at + before[at..].chars().next().map_or(1, char::len_utf8)
        });
        let word = &before[start..];
        (!matches!(word, "if" | "for" | "while" | "switch" | "match" | "return")).then_some(word)
    })?;
    (name.len() >= 4 && !GENERIC_NAMES.contains(&name)).then(|| name.to_owned())
}
fn signature_types(lines: &[&str], start: usize) -> Vec<String> {
    let mut types: Vec<String> = Vec::new();
    for line in lines.iter().skip(start - 1).take(4) {
        for word in line.split(|c: char| !ident(c)) {
            if word.len() > 2
                && word.chars().next().is_some_and(char::is_uppercase)
                && word.chars().any(char::is_lowercase)
                && !COMMON_TYPES.contains(&word)
                && !types.iter().any(|t| t == word)
            {
                types.push(word.to_owned());
            }
        }
        if line.contains('{') || line.trim_end().ends_with(':') {
            break;
        }
    }
    types.truncate(3);
    types
}
fn config_file(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    matches!(
        name.rsplit('.').next().filter(|_| name.contains('.')),
        Some("toml" | "json" | "yaml" | "yml" | "ini" | "cfg" | "properties")
    ) || matches!(name, "Dockerfile" | "Makefile")
}
/// Environment, credential and key material is never expanded automatically, even when a symbol
/// name matches; an explicit search or `read` remains the only way to look at such files.
fn sensitive_path(path: &str) -> bool {
    path.split('/').any(|part| {
        let lower = part.to_ascii_lowercase();
        lower == ".env"
            || lower.starts_with(".env.")
            || lower.ends_with(".env")
            || matches!(
                lower.rsplit('.').next().filter(|_| lower.contains('.')),
                Some(
                    "pem"
                        | "key"
                        | "p12"
                        | "pfx"
                        | "crt"
                        | "cer"
                        | "jks"
                        | "keystore"
                        | "asc"
                        | "gpg"
                        | "tfvars"
                )
            )
            || [
                "secret",
                "credential",
                "password",
                "passwd",
                "token",
                "id_rsa",
                "id_ed25519",
                ".npmrc",
                ".netrc",
                ".pypirc",
                "auth.json",
                "private",
            ]
            .iter()
            .any(|marker| lower.contains(marker))
    })
}
fn type_declaration(kind: &str) -> bool {
    [
        "struct",
        "enum",
        "trait",
        "class",
        "interface",
        "type_declaration",
    ]
    .iter()
    .any(|k| kind.contains(k))
}
fn span_row(
    path: &str,
    sha256: &str,
    decls: &[(usize, usize, String)],
    line: usize,
    lines: &[&str],
    label: &'static str,
    call: bool,
) -> Row {
    let enclosing = decls
        .iter()
        .filter(|(s, e, _)| *s <= line && line <= *e && e - s < MAX_SPAN)
        .min_by_key(|(s, e, _)| e - s);
    let (start, end, provenance) = match enclosing {
        Some((s, e, _)) => (*s, *e, "lexical"),
        None => (
            line.saturating_sub(2).max(1),
            (line + 2).min(lines.len().max(1)),
            "lexical",
        ),
    };
    Row {
        path: path.to_owned(),
        start,
        end,
        label,
        provenance,
        sha256: sha256.to_owned(),
        text: clip(lines.get(line - 1).copied().unwrap_or("")),
        call,
    }
}

/// Build the bundle for at most [`MAX_TARGETS`] hits. `scope` bounds the lexical search like `find`.
pub(super) fn render(ws: &Workspace, scope: &Path, targets: &[Target], max_bytes: usize) -> String {
    let targets = &targets[..targets.len().min(MAX_TARGETS)];
    if targets.is_empty() || max_bytes < 512 {
        return String::new();
    }
    let mut plans: Vec<Plan> = targets.iter().map(|_| Plan::default()).collect();
    for (plan, target) in plans.iter_mut().zip(targets) {
        let Ok(abs) = ws.resolve(&target.path) else {
            plan.stale = true;
            continue;
        };
        let Some((text, sha256)) = read_source(ws, &abs).filter(|(_, sha)| *sha == target.sha256)
        else {
            plan.stale = true;
            continue;
        };
        let lines: Vec<&str> = text.lines().collect();
        let decls = declarations(&abs, &text);
        let mut chain: Vec<_> = decls
            .iter()
            .filter(|(s, e, _)| *s <= target.start && target.end <= *e)
            .collect();
        chain.sort_by_key(|(s, e, _)| (e - s, *s));
        let same = chain
            .iter()
            .find(|(s, e, _)| (*s, *e) == (target.start, target.end));
        let owner = chain
            .iter()
            .find(|(s, e, _)| (*s, *e) != (target.start, target.end));
        if let Some((s, e, kind)) = owner {
            plan.owner = Some(Row {
                path: target.path.clone(),
                start: *s,
                end: *e,
                label: "owner",
                provenance: "syntax",
                sha256: sha256.clone(),
                text: format!("[{kind}] {}", clip(lines.get(s - 1).copied().unwrap_or(""))),
                call: false,
            });
        }
        if let Some((s, _, _)) = same.or(owner) {
            plan.name = lines.get(s - 1).and_then(|line| declared_name(line));
            plan.types = signature_types(&lines, *s);
        }
    }
    let wanted: Vec<&Plan> = plans.iter().filter(|p| p.name.is_some()).collect();
    let mut scanned = 0usize;
    let mut bytes = 0u64;
    let mut truncated = false;
    if !wanted.is_empty() {
        let mut walk = Walk::new(ws);
        walk.run(scope, &mut |path| {
            if scanned >= MAX_FILES || bytes >= MAX_BYTES {
                truncated = true;
                return false;
            }
            let Some((text, sha256)) = read_source(ws, path) else {
                return true;
            };
            scanned += 1;
            bytes += text.len() as u64;
            let rel = ws.rel(path);
            if sensitive_path(&rel) {
                return true;
            }
            let is_config = config_file(&rel);
            let mut lines: Option<Vec<&str>> = None;
            let mut decls: Option<Vec<(usize, usize, String)>> = None;
            for (plan, target) in plans.iter_mut().zip(targets) {
                if plan.stale {
                    continue;
                }
                let mut hits = Vec::new();
                if let Some(name) = plan.name.as_deref().filter(|n| text.contains(*n)) {
                    let all = lines.get_or_insert_with(|| text.lines().collect());
                    for (index, line) in all.iter().enumerate() {
                        let number = index + 1;
                        let own =
                            rel == target.path && target.start <= number && number <= target.end;
                        if own {
                            continue;
                        }
                        if let Some(at) = find_word(line, name) {
                            let call = line[at + name.len()..].trim_start().starts_with('(');
                            hits.push((number, call));
                        }
                    }
                }
                for (number, call) in hits.into_iter().take(12) {
                    let all = lines.as_ref().expect("lines loaded for hits");
                    let spans = decls.get_or_insert_with(|| declarations(path, &text));
                    if is_config {
                        if plan.config.len() < 8 {
                            plan.config.push(span_row(
                                &rel,
                                &sha256,
                                &[],
                                number,
                                all,
                                "config-reference",
                                false,
                            ));
                        }
                    } else if plan.refs.len() < 40 {
                        let label = if call { "call-site" } else { "mention" };
                        let row = span_row(&rel, &sha256, spans, number, all, label, call);
                        if !plan
                            .refs
                            .iter()
                            .any(|r| (&r.path, r.start, r.end) == (&row.path, row.start, row.end))
                        {
                            plan.refs.push(row);
                        }
                    }
                }
                for (index, name) in plan.types.clone().iter().enumerate() {
                    if is_config
                        || plan.type_defs.iter().any(|(i, _)| *i == index)
                        || !text.contains(name.as_str())
                    {
                        continue;
                    }
                    let all = lines.get_or_insert_with(|| text.lines().collect());
                    let spans = decls.get_or_insert_with(|| declarations(path, &text));
                    if let Some((s, e, kind)) = spans.iter().find(|(s, _, kind)| {
                        type_declaration(kind)
                            && all
                                .get(s - 1)
                                .is_some_and(|line| find_word(line, name).is_some())
                    }) && !(rel == target.path && (*s, *e) == (target.start, target.end))
                    {
                        plan.type_defs.push((
                            index,
                            Row {
                                path: rel.clone(),
                                start: *s,
                                end: *e,
                                label: "type-definition",
                                provenance: "syntax",
                                sha256: sha256.clone(),
                                text: format!(
                                    "[{kind}] {}",
                                    clip(all.get(s - 1).copied().unwrap_or(""))
                                ),
                                call: false,
                            },
                        ));
                    }
                }
            }
            true
        });
        truncated |= walk.truncated;
    }
    let mut out = String::from(
        "Related evidence (bounded navigation aids, NOT edit witnesses: read the lines before editing). provenance=syntax is a tree-sitter declaration span; provenance=lexical is a whole-word text match in the searched scope, not a resolved symbol or call graph. Only the language-server section, when requested, is resolved.\n",
    );
    let mut clipped = false;
    let mut push = |out: &mut String, row: String| {
        if out.len() + row.len() > max_bytes {
            clipped = true;
            return;
        }
        out.push_str(&row);
    };
    for (plan, target) in plans.iter_mut().zip(targets) {
        // A hit that is not a named declaration (a config line, a text window) has no owner to relate.
        if plan.name.is_none() && !plan.stale {
            continue;
        }
        push(
            &mut out,
            format!(
                "Related to {}:{}-{}:\n",
                target.path, target.start, target.end
            ),
        );
        if plan.stale {
            push(
                &mut out,
                "  source changed or unreadable since ranking; related evidence omitted\n".into(),
            );
            continue;
        }
        let mut rows = Vec::new();
        rows.extend(plan.owner.take());
        rows.extend(plan.type_defs.drain(..).take(MAX_TYPES).map(|(_, row)| row));
        plan.refs.sort_by_key(|row| !row.call);
        rows.extend(plan.refs.drain(..).take(MAX_REFS));
        rows.extend(plan.config.drain(..).take(MAX_CONFIG));
        if rows.is_empty() {
            push(
                &mut out,
                "  none found in scope (lexical search; absence is not proof)\n".into(),
            );
        }
        for row in rows {
            push(&mut out, row.render());
        }
    }
    out.push_str(&format!(
        "[related scan: {scanned} files, {bytes} bytes; scan truncated={truncated}; output clipped={clipped}; caps: {MAX_REFS} references, {MAX_TYPES} type definitions, {MAX_CONFIG} config rows per hit]\n"
    ));
    out
}
