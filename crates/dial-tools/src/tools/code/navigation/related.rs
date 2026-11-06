//! Bounded related-evidence bundles for `find`: the enclosing owner, lexical call-sites/mentions,
//! signature type definitions and config references around the top hits.
//!
//! Provenance is stated per row. `syntax` rows are tree-sitter declaration spans; `lexical` rows are
//! whole-word text matches in the searched scope. Neither is a resolved symbol or call graph (only the
//! optional language-server section of `find` is resolved), and no row is an edit witness.
use super::{MAX_FILE, Workspace, declarations};
use crate::tools::fs::walk::Walk;
use ast_grep_core::{AstGrep, Language};
use sha2::{Digest, Sha256};
use std::{fs, io::Read, path::Path};

const MAX_FILES: usize = 3000;
const MAX_BYTES: u64 = 24 * 1024 * 1024;
const MAX_SPAN: usize = 60;
const MAX_REFS: usize = 4;
const MAX_CONFIG: usize = 2;
const MAX_TYPES: usize = 2;
const MAX_CONTRACT_REFS: usize = 12;
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

#[derive(Default)]
pub(super) struct Bundle {
    pub text: String,
    pub previews: Vec<Target>,
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
    contracts: Vec<String>,
    contract_refs: Vec<Row>,
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
        .resolve_read(&ws.rel(path))
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

/// Literal public names in name methods and string-array constants inside a ranked
/// span. These are lexical search seeds, not resolved symbols or computed values.
pub(super) fn contract_names(path: &Path, text: &str, start: usize, end: usize) -> Vec<String> {
    let Some(lang) = super::SupportLang::from_path(path) else {
        return Vec::new();
    };
    let Ok(tree) = AstGrep::try_new(text, lang) else {
        return Vec::new();
    };
    let mut names = Vec::new();
    for node in tree.root().dfs() {
        if node.start_pos().line() + 1 < start || node.end_pos().line() + 1 > end {
            continue;
        }
        let Some(name) = node.field("name") else {
            continue;
        };
        let registry = node.kind() == "const_item";
        let value = if registry {
            node.field("value")
        } else if name.text() == "name" {
            node.field("body")
        } else {
            None
        };
        let Some(value) = value else { continue };
        let value_text = value.text();
        let expression = value_text
            .trim()
            .trim_start_matches('{')
            .trim_end_matches('}')
            .trim();
        let expression = expression
            .strip_prefix("return ")
            .unwrap_or(expression)
            .trim_end_matches(';')
            .trim();
        for literal in value.dfs().filter(|node| node.kind() == "string_literal") {
            let direct = if registry {
                literal
                    .parent()
                    .is_some_and(|parent| parent.kind() == "array_expression")
            } else {
                expression == literal.text()
            };
            if direct
                && let Ok(name) = serde_json::from_str::<String>(&literal.text())
                && (3..=64).contains(&name.len())
                && name.chars().all(ident)
                && crate::storage::memory::screen(&name).is_ok()
                && !names.contains(&name)
            {
                names.push(name);
                if names.len() == 8 {
                    return names;
                }
            }
        }
    }
    names
}

/// Coarse source groups keep one crate/module's many directories from dominating.
fn contract_group(path: &str) -> &str {
    let start = path.find("/src/").map_or(0, |at| at + 5);
    let end = path[start..].find('/').map_or(path.len(), |at| start + at);
    &path[..end]
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
pub(super) fn sensitive_path(path: &str) -> bool {
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
pub(super) fn render(ws: &Workspace, scope: &Path, targets: &[Target], max_bytes: usize) -> Bundle {
    let targets = &targets[..targets.len().min(MAX_TARGETS)];
    if targets.is_empty() || max_bytes < 512 {
        return Bundle::default();
    }
    let mut plans: Vec<Plan> = targets.iter().map(|_| Plan::default()).collect();
    for (plan, target) in plans.iter_mut().zip(targets) {
        let Ok(abs) = ws.resolve_read(&target.path) else {
            plan.stale = true;
            continue;
        };
        let Some((text, sha256)) = read_source(ws, &abs).filter(|(_, sha)| *sha == target.sha256)
        else {
            plan.stale = true;
            continue;
        };
        let lines: Vec<&str> = text.lines().collect();
        plan.contracts = contract_names(&abs, &text, target.start, target.end);
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
    let wanted = plans
        .iter()
        .any(|p| p.name.is_some() || !p.contracts.is_empty());
    let mut scanned = 0usize;
    let mut bytes = 0u64;
    let mut truncated = false;
    if wanted {
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
                // Keep one contract reference per directory so a dense group of local
                // callers cannot crowd out config, persistence, dispatch or history.
                if !plan.contracts.is_empty()
                    && (is_config || super::SupportLang::from_path(path).is_some())
                    && plan.contract_refs.len() < 40
                    && plan
                        .contract_refs
                        .iter()
                        .filter(|row| contract_group(&row.path) == contract_group(&rel))
                        .count()
                        < 4
                    && !plan
                        .contract_refs
                        .iter()
                        .any(|row| Path::new(&row.path).parent() == Path::new(&rel).parent())
                {
                    let all = lines.get_or_insert_with(|| text.lines().collect());
                    if let Some((index, _)) = all.iter().enumerate().find(|(index, line)| {
                        !(rel == target.path && target.start <= index + 1 && *index < target.end)
                            && plan.contracts.iter().any(|name| {
                                line.contains(&format!("\"{name}\""))
                                    || line.contains(&format!("'{name}'"))
                            })
                    }) {
                        let spans = decls.get_or_insert_with(|| declarations(path, &text));
                        plan.contract_refs.push(span_row(
                            &rel,
                            &sha256,
                            spans,
                            index + 1,
                            all,
                            "contract-reference",
                            false,
                        ));
                    }
                }
                let mut hits = Vec::new();
                let has_room = if is_config {
                    plan.config.len() < 8
                } else {
                    plan.refs.len() < 40
                };
                if has_room && let Some(name) = plan.name.as_deref().filter(|n| text.contains(*n)) {
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
                        let spans = decls.get_or_insert_with(|| declarations(path, &text));
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
    // Reuse the bounded discovery pool, before summary quotas drain it. No extra walk or
    // relaxed format eligibility: unsupported config formats remain summary-only.
    let eligible = |row: &&Row| super::SupportLang::from_path(Path::new(&row.path)).is_some();
    let test = plans
        .iter()
        .flat_map(|plan| &plan.refs)
        .filter(eligible)
        .find(|row| crate::tools::fs::search::conventional_test(Path::new(&row.path)));
    let config = plans.iter().flat_map(|plan| &plan.config).find(eligible);
    let previews = test
        .into_iter()
        .chain(config)
        .map(|row| Target {
            path: row.path.clone(),
            start: row.start,
            end: row.end,
            sha256: row.sha256.clone(),
        })
        .collect();
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
    let mut shown_contracts = std::collections::BTreeSet::new();
    for (plan, target) in plans.iter_mut().zip(targets) {
        // Literal public contracts can be mapped even when the declaration name is generic.
        if plan.name.is_none() && plan.contracts.is_empty() && !plan.stale {
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
        if !plan.contracts.is_empty() {
            push(
                &mut out,
                format!(
                    "  contract names: {} (literal declarations; lexical references, not confirmed owners)\n",
                    plan.contracts.join(", ")
                ),
            );
        }
        let mut rows = Vec::new();
        rows.extend(plan.owner.take());
        let mut groups = std::collections::BTreeMap::new();
        let mut contracts: Vec<_> = plan
            .contract_refs
            .drain(..)
            .enumerate()
            .map(|(index, row)| {
                let count = groups
                    .entry(contract_group(&row.path).to_owned())
                    .or_insert(0);
                let order = *count;
                *count += 1;
                (order, index, row)
            })
            .collect();
        contracts.sort_by_key(|(order, index, _)| (*order, *index));
        rows.extend(
            contracts
                .into_iter()
                .map(|(_, _, row)| row)
                .filter(|row| shown_contracts.insert((row.path.clone(), row.start, row.end)))
                .take(MAX_CONTRACT_REFS),
        );
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
        "[related scan: {scanned} files, {bytes} bytes; scan truncated={truncated}; output clipped={clipped}; caps: {MAX_REFS} references, {MAX_TYPES} type definitions, {MAX_CONFIG} config rows, {MAX_CONTRACT_REFS} directory-diverse contract references per hit; contract discovery bounded to 8 literal names and 40 directories per hit, at most 4 directories per source group; source groups prioritized before extra directories; absence is not proof]\n"
    ));
    Bundle {
        text: out,
        previews,
    }
}
