//! Skills: reusable procedures stored as `<skills dir>/<name>/SKILL.md` (the agentskills.io layout).
//!
//! Progressive disclosure: only a one-line catalog (`name: description`) goes into the system
//! prompt. The `skill` tool loads a skill's body, and then any bundled file, only when the model
//! asks. `skill_manage` lets the agent write down what it learned as a new or refined skill.
//!
//! Skill files are untrusted text. They are never executed or interpolated into the prompt except
//! as a tool result the model requested, every read refuses symlinks and path traversal, and
//! every size is bounded.
use super::{
    memory::{atomic_write, find_secret, has_hidden_chars},
    paths::Layout,
};
use crate::platform::fs::OpenOptions;
use crate::tools::ToolOutput;
use serde_json::{Value, json};
use std::{
    fs,
    io::Read,
    path::{Component, Path, PathBuf},
};

const SKILL_FILE: &str = "SKILL.md";
/// Largest bundled file or SKILL.md returned by the `skill` tool.
pub const MAX_READ_BYTES: usize = 64 * 1024;
/// Largest skill body `skill_manage` will write.
pub const MAX_BODY_BYTES: usize = 64 * 1024;
pub const MAX_DESCRIPTION_CHARS: usize = 1024;
/// Only this much of a SKILL.md is read during discovery; the frontmatter must fit.
const HEAD_BYTES: usize = 16 * 1024;
const MAX_SKILLS_PER_DIR: usize = 256;
const CATALOG_BUDGET_CHARS: usize = 4000;
const CATALOG_DESCRIPTION_CHARS: usize = 200;
const MAX_LISTED_FILES: usize = 200;
const MAX_LIST_DEPTH: usize = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Project,
    User,
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::User => "user",
        }
    }

    fn root(self, layout: &Layout) -> PathBuf {
        let [project, user] = layout.skills_dirs();
        match self {
            Self::Project => project,
            Self::User => user,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    /// The skill's `SKILL.md`; its parent directory holds the bundled files.
    pub path: PathBuf,
    pub source: Scope,
}

impl Skill {
    fn dir(&self) -> &Path {
        self.path.parent().unwrap_or(&self.path)
    }
}

// ---------------------------------------------------------------------------------------------
// Frontmatter: the tiny YAML subset skills use (top-level `key: value`, quoted strings, block
// scalars, indented continuation lines). Everything else is ignored.
// ---------------------------------------------------------------------------------------------

/// Split `---` delimited frontmatter from the body. `None` when there is no closed frontmatter.
fn split_frontmatter(text: &str) -> Option<(&str, &str)> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut lines = text.split_inclusive('\n');
    let first = lines.next()?;
    if first.trim_end() != "---" {
        return None;
    }
    let mut offset = first.len();
    for line in lines {
        if line.trim_end() == "---" {
            return Some((&text[first.len()..offset], &text[offset + line.len()..]));
        }
        offset += line.len();
    }
    None
}

#[derive(Debug)]
struct Field {
    key: String,
    value: String,
    /// Line range `[start, end)` within the frontmatter occupied by the key and its continuation.
    start: usize,
    end: usize,
}

fn parse_fields(frontmatter: &str) -> Vec<Field> {
    let lines: Vec<&str> = frontmatter.lines().collect();
    let mut fields = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let top_level = !line.starts_with(char::is_whitespace)
            && !line.trim().is_empty()
            && !line.starts_with('#');
        let Some((key, rest)) = line.split_once(':').filter(|_| top_level) else {
            i += 1;
            continue;
        };
        let start = i;
        let mut end = i + 1;
        let mut continuation = Vec::new();
        let mut j = i + 1;
        while j < lines.len()
            && (lines[j].trim().is_empty() || lines[j].starts_with(char::is_whitespace))
        {
            if !lines[j].trim().is_empty() {
                continuation.push(lines[j].trim());
                end = j + 1;
            }
            j += 1;
        }
        let rest = rest.trim();
        let joined = |first: &str| -> String {
            let mut parts = Vec::new();
            if !first.is_empty() {
                parts.push(first);
            }
            parts.extend(continuation.iter().copied());
            parts.join(" ")
        };
        let value = if rest.starts_with('>') || rest.starts_with('|') {
            joined("")
        } else if rest.starts_with('"') || rest.starts_with('\'') {
            unquote(&joined(rest))
        } else {
            let plain = joined(rest);
            match plain.find(" #") {
                Some(pos) => plain[..pos].trim_end().to_string(),
                None => plain,
            }
        };
        fields.push(Field {
            key: key.trim().to_string(),
            value,
            start,
            end,
        });
        i = end.max(i + 1);
    }
    fields
}

/// Decode a YAML quoted scalar; anything after the closing quote is dropped. An unterminated
/// quote yields the text after the opening quote.
fn unquote(text: &str) -> String {
    let mut chars = text.chars().peekable();
    let Some(quote) = chars.next() else {
        return String::new();
    };
    let mut out = String::new();
    while let Some(c) = chars.next() {
        match c {
            '\\' if quote == '"' => match chars.next() {
                Some('n') => out.push(' '),
                Some('t') => out.push(' '),
                Some(other) => out.push(other),
                None => {}
            },
            '\'' if quote == '\'' && chars.peek() == Some(&'\'') => {
                chars.next();
                out.push('\'');
            }
            c if c == quote => break,
            c => out.push(c),
        }
    }
    out
}

fn field<'a>(fields: &'a [Field], key: &str) -> Option<&'a Field> {
    fields.iter().find(|f| f.key == key)
}

/// One line of plain text: control and invisible characters dropped, whitespace collapsed.
fn one_line(text: &str) -> String {
    let flat: String = text
        .chars()
        .filter(|c| c.is_whitespace() || !(c.is_control() || has_hidden_chars(&c.to_string())))
        .collect();
    flat.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    name.len() <= 64
        && chars
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

// ---------------------------------------------------------------------------------------------
// Safe file access
// ---------------------------------------------------------------------------------------------

/// Read up to `max` bytes of a regular file without following a symlink at the final component.
/// Returns the bytes and whether the file was longer.
fn read_head(path: &Path, max: usize) -> std::io::Result<(Vec<u8>, bool)> {
    let file = crate::platform::fs::OpenOptions::new()
        .read(true)
        .custom_flags(
            crate::platform::fs::NOFOLLOW
                | crate::platform::fs::NONBLOCK
                | crate::platform::fs::CLOEXEC,
        )
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::other("not a regular file"));
    }
    let mut bytes = Vec::new();
    file.take(max as u64 + 1).read_to_end(&mut bytes)?;
    let truncated = bytes.len() > max;
    bytes.truncate(max);
    Ok((bytes, truncated))
}

fn is_real_dir(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.is_dir())
}

fn is_real_file(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.is_file())
}

pub mod import;
pub mod management;

/// A project skills directory must not lead outside the project (e.g. via a symlinked `.dial`).
fn root_is_safe(layout: &Layout, scope: Scope, root: &Path) -> bool {
    match scope {
        Scope::User => true,
        Scope::Project => fs::canonicalize(root).is_ok_and(|p| p.starts_with(&layout.project_root)),
    }
}

fn load_skill(dir: &Path, scope: Scope) -> Option<Skill> {
    let name = dir.file_name()?.to_str()?.to_string();
    if !valid_name(&name) || !is_real_dir(dir) {
        return None;
    }
    let path = dir.join(SKILL_FILE);
    let (head, _) = read_head(&path, HEAD_BYTES).ok()?;
    let text = String::from_utf8_lossy(&head);
    let (frontmatter, _) = split_frontmatter(&text)?;
    let fields = parse_fields(frontmatter);
    if let Some(declared) = field(&fields, "name")
        && declared.value != name
    {
        return None;
    }
    let description: String = one_line(&field(&fields, "description")?.value)
        .chars()
        .take(MAX_DESCRIPTION_CHARS)
        .collect();
    if description.is_empty() {
        return None;
    }
    Some(Skill {
        name,
        description,
        path,
        source: scope,
    })
}

/// All valid skills, sorted by name. Project skills shadow user skills of the same name. Entries
/// that are symlinks, lack a valid `SKILL.md`, or declare a different `name` are skipped.
pub fn discover_all(layout: &Layout) -> Vec<Skill> {
    let mut skills: Vec<Skill> = Vec::new();
    for scope in [Scope::Project, Scope::User] {
        let root = scope.root(layout);
        if !root_is_safe(layout, scope, &root) {
            continue;
        }
        let mut dirs: Vec<PathBuf> = fs::read_dir(&root)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .collect();
        dirs.sort();
        for skill in dirs
            .iter()
            .filter_map(|d| load_skill(d, scope))
            .take(MAX_SKILLS_PER_DIR)
        {
            if !skills.iter().any(|s| s.name == skill.name) {
                skills.push(skill);
            }
        }
    }
    skills.sort_by(|a, b| a.name.cmp(&b.name));
    skills
}

/// Only enabled skills are exposed to the model and slash-command completion.
pub fn discover(layout: &Layout) -> Vec<Skill> {
    management::installed(layout)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(skill, enabled)| enabled.then_some(skill))
        .collect()
}

/// The system-prompt catalog: one `- name: description` line per skill, within a size budget.
/// Empty when there are no skills.
pub fn catalog_block(skills: &[Skill]) -> String {
    if skills.is_empty() {
        return String::new();
    }
    let mut out = String::from(
        "## Skills\nReusable procedures. When a task matches one, load it with the `skill` tool before starting. Descriptions below are summaries only.\n",
    );
    let mut shown = 0;
    for skill in skills {
        let description: String = if skill.description.chars().count() > CATALOG_DESCRIPTION_CHARS {
            skill
                .description
                .chars()
                .take(CATALOG_DESCRIPTION_CHARS - 1)
                .chain(std::iter::once('…'))
                .collect()
        } else {
            skill.description.clone()
        };
        let line = format!("- {}: {}\n", skill.name, description);
        if out.chars().count() + line.chars().count() > CATALOG_BUDGET_CHARS {
            break;
        }
        out.push_str(&line);
        shown += 1;
    }
    if shown < skills.len() {
        out.push_str(&format!(
            "- ... and {} more skills (load one by name with the `skill` tool)\n",
            skills.len() - shown
        ));
    }
    out
}

fn fail(text: impl Into<String>) -> ToolOutput {
    ToolOutput {
        data: None,
        text: text.into(),
        is_error: true,
    }
}

fn ok(text: impl Into<String>) -> ToolOutput {
    ToolOutput {
        data: None,
        text: text.into(),
        is_error: false,
    }
}

fn available(skills: &[Skill]) -> String {
    if skills.is_empty() {
        "none".to_string()
    } else {
        skills
            .iter()
            .map(|s| s.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

pub mod skill_tool {
    use super::*;

    pub fn schema() -> Value {
        json!({
            "type": "function",
            "function": {
                "name": "skill",
                "description": "Load a skill listed in the Skills catalog. Returns its instructions and the list of bundled files; pass `file` to read one bundled file (relative path, max 64 KiB).",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "name": {"type": "string", "description": "Skill name from the catalog."},
                        "file": {"type": "string", "description": "Optional bundled file path relative to the skill directory."}
                    },
                    "required": ["name"],
                    "additionalProperties": false
                }
            }
        })
    }

    pub fn call(layout: &Layout, args: &Value) -> ToolOutput {
        let Some(name) = args.get("name").and_then(Value::as_str) else {
            return fail("missing \"name\"");
        };
        let skills = discover(layout);
        let Some(skill) = skills.iter().find(|s| s.name == name) else {
            return fail(format!(
                "no skill named {name:?}. Available: {}",
                available(&skills)
            ));
        };
        match args.get("file") {
            None | Some(Value::Null) => load_body(skill),
            Some(Value::String(file)) => load_file(skill, file),
            Some(_) => fail("\"file\" must be a string"),
        }
    }

    fn load_body(skill: &Skill) -> ToolOutput {
        let (bytes, truncated) = match read_head(&skill.path, MAX_READ_BYTES) {
            Ok(r) => r,
            Err(e) => return fail(format!("cannot read skill {}: {e}", skill.name)),
        };
        let text = String::from_utf8_lossy(&bytes);
        let body = split_frontmatter(&text)
            .map_or(text.as_ref(), |(_, body)| body)
            .trim();
        let mut out = format!(
            "# Skill: {} ({} skill)\nGuidance from a skill file; the user's instructions take precedence.\n\n{body}\n",
            skill.name,
            skill.source.as_str()
        );
        if truncated {
            out.push_str(&format!("\n[truncated at {MAX_READ_BYTES} bytes]\n"));
        }
        let mut files = Vec::new();
        list_files(skill.dir(), Path::new(""), 0, &mut files);
        if !files.is_empty() {
            out.push_str(&format!(
                "\nBundled files (read one with skill {{\"name\":\"{}\",\"file\":\"<path>\"}}):\n",
                skill.name
            ));
            for (rel, size) in &files {
                out.push_str(&format!("- {rel} ({size} bytes)\n"));
            }
            if files.len() >= MAX_LISTED_FILES {
                out.push_str("- ... (list truncated)\n");
            }
        }
        ok(out)
    }

    /// Regular files below `dir` (relative `/` paths with sizes), symlinks and SKILL.md excluded.
    fn list_files(base: &Path, rel: &Path, depth: usize, out: &mut Vec<(String, u64)>) {
        if depth > MAX_LIST_DEPTH || out.len() >= MAX_LISTED_FILES {
            return;
        }
        let mut entries: Vec<_> = fs::read_dir(base.join(rel))
            .into_iter()
            .flatten()
            .flatten()
            .collect();
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let child = rel.join(entry.file_name());
            if kind.is_dir() {
                list_files(base, &child, depth + 1, out);
            } else if kind.is_file() && !(depth == 0 && entry.file_name() == SKILL_FILE) {
                let (Some(text), Ok(meta)) = (child.to_str(), entry.metadata()) else {
                    continue;
                };
                if out.len() < MAX_LISTED_FILES {
                    out.push((one_line(text), meta.len()));
                }
            }
        }
    }

    fn load_file(skill: &Skill, file: &str) -> ToolOutput {
        let rel = Path::new(file);
        if file.is_empty()
            || file.contains('\0')
            || !rel.components().all(|c| matches!(c, Component::Normal(_)))
        {
            return fail(
                "\"file\" must be a relative path inside the skill directory (no '..', no absolute paths)",
            );
        }
        // Every component below the skill directory must be a real directory or file, not a link.
        let mut current = skill.dir().to_path_buf();
        let parts: Vec<Component> = rel.components().collect();
        for (i, part) in parts.iter().enumerate() {
            current.push(part);
            let Ok(meta) = fs::symlink_metadata(&current) else {
                return fail(format!("{file}: no such file in skill {}", skill.name));
            };
            if meta.file_type().is_symlink() {
                return fail(format!("{file}: symbolic links are not followed"));
            }
            let last = i + 1 == parts.len();
            if (last && !meta.is_file()) || (!last && !meta.is_dir()) {
                return fail(format!("{file}: not a regular file"));
            }
        }
        match read_head(&current, MAX_READ_BYTES) {
            Ok((_, true)) => fail(format!(
                "{file}: larger than {MAX_READ_BYTES} bytes; not returned"
            )),
            Ok((bytes, false)) => match String::from_utf8(bytes) {
                Ok(text) => ok(format!("# {}/{file}\n\n{text}", skill.name)),
                Err(_) => fail(format!("{file}: not UTF-8 text")),
            },
            Err(e) => fail(format!("{file}: {e}")),
        }
    }
}

pub mod manage_tool {
    use super::*;

    pub fn schema() -> Value {
        json!({
            "type": "function",
            "function": {
                "name": "skill_manage",
                "description": "Create, update or delete a skill so a procedure you worked out can be reused. A skill is a SKILL.md: a one-line description (when to use it) plus a markdown body of steps, commands and pitfalls. Create after solving something non-obvious; update when a skill proved wrong or incomplete. Scope `project` (default) is shared with this repo; `user` applies everywhere. Never put secrets in a skill.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "action": {"type": "string", "enum": ["create", "update", "delete"]},
                        "name": {"type": "string", "description": "Lowercase letters, digits and hyphens, starting with a letter or digit, max 64 chars."},
                        "description": {"type": "string", "description": "One line: what the skill does and when to use it. Required for create."},
                        "body": {"type": "string", "description": "Markdown instructions without frontmatter, max 64 KB. Required for create."},
                        "scope": {"type": "string", "enum": ["project", "user"]}
                    },
                    "required": ["action", "name"],
                    "additionalProperties": false
                }
            }
        })
    }

    fn parse_scope(args: &Value) -> Result<Option<Scope>, String> {
        match args.get("scope") {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) if s == "project" => Ok(Some(Scope::Project)),
            Some(Value::String(s)) if s == "user" => Ok(Some(Scope::User)),
            Some(_) => Err("\"scope\" must be \"project\" or \"user\"".into()),
        }
    }

    pub fn call(layout: &Layout, args: &Value) -> ToolOutput {
        match run(layout, args) {
            Ok(text) => ok(text),
            Err(text) => fail(text),
        }
    }

    fn run(layout: &Layout, args: &Value) -> Result<String, String> {
        let action = args
            .get("action")
            .and_then(Value::as_str)
            .ok_or("missing \"action\": use create, update or delete")?;
        let name = args
            .get("name")
            .and_then(Value::as_str)
            .ok_or("missing \"name\"")?;
        if !valid_name(name) {
            return Err(format!(
                "invalid skill name {name:?}: use lowercase letters, digits and hyphens, starting with a letter or digit, at most 64 characters"
            ));
        }
        let scope = parse_scope(args)?;
        let text = |key: &str| match args.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) => Ok(Some(s.as_str())),
            Some(_) => Err(format!("\"{key}\" must be a string")),
        };
        let (description, body) = (text("description")?, text("body")?);
        match action {
            "create" => create(
                layout,
                name,
                scope.unwrap_or(Scope::Project),
                description,
                body,
            ),
            "update" => {
                let scope = scope
                    .or_else(|| existing_scope(layout, name))
                    .ok_or_else(|| format!("no skill named {name:?} to update"))?;
                update(layout, name, scope, description, body)
            }
            "delete" => {
                let scope = scope
                    .or_else(|| existing_scope(layout, name))
                    .ok_or_else(|| format!("no skill named {name:?} to delete"))?;
                delete(layout, name, scope)
            }
            other => Err(format!(
                "unknown action {other:?}: use create, update or delete"
            )),
        }
    }

    /// Where a skill of this name lives; project wins as it does for loading.
    fn existing_scope(layout: &Layout, name: &str) -> Option<Scope> {
        [Scope::Project, Scope::User].into_iter().find(|s| {
            is_real_dir(&s.root(layout).join(name))
                && is_real_file(&s.root(layout).join(name).join(SKILL_FILE))
        })
    }

    fn clean_description(raw: &str) -> Result<String, String> {
        let description = one_line(raw);
        if description.is_empty() {
            return Err("\"description\" must not be empty".into());
        }
        if description.chars().count() > MAX_DESCRIPTION_CHARS {
            return Err(format!(
                "description is {} chars; the limit is {MAX_DESCRIPTION_CHARS}",
                description.chars().count()
            ));
        }
        screen("description", &description)?;
        Ok(description)
    }

    fn clean_body(raw: &str) -> Result<String, String> {
        let normalized: String = raw
            .replace("\r\n", "\n")
            .chars()
            .filter(|c| matches!(c, '\n' | '\t') || !c.is_control())
            .collect();
        let body = normalized.trim();
        if body.is_empty() {
            return Err("\"body\" must not be empty".into());
        }
        if body.len() > MAX_BODY_BYTES {
            return Err(format!(
                "body is {} bytes; the limit is {MAX_BODY_BYTES}",
                body.len()
            ));
        }
        if body.lines().next().is_some_and(|l| l.trim_end() == "---") {
            return Err("body must not start with YAML frontmatter; pass name and description as separate arguments".into());
        }
        screen("body", body)?;
        Ok(body.to_string())
    }

    fn screen(what: &str, text: &str) -> Result<(), String> {
        if has_hidden_chars(text) {
            return Err(format!(
                "{what} rejected: contains zero-width or bidirectional control characters"
            ));
        }
        if let Some(kind) = find_secret(text) {
            return Err(format!(
                "{what} rejected: looks like it contains a secret ({kind}). Never store credentials in a skill"
            ));
        }
        Ok(())
    }

    fn quote(description: &str) -> String {
        format!(
            "\"{}\"",
            description.replace('\\', "\\\\").replace('"', "\\\"")
        )
    }

    fn render(name: &str, description: &str, body: &str) -> String {
        format!(
            "---\nname: {name}\ndescription: {}\n---\n\n{body}\n",
            quote(description)
        )
    }

    /// The skills directory for writing, created if needed and verified to stay inside the project.
    fn writable_root(layout: &Layout, scope: Scope) -> Result<PathBuf, String> {
        let root = scope.root(layout);
        fs::create_dir_all(&root).map_err(|e| format!("cannot create {}: {e}", root.display()))?;
        if !root_is_safe(layout, scope, &root) {
            return Err(
                "the project skills directory resolves outside the project; refusing to write"
                    .into(),
            );
        }
        Ok(root)
    }

    fn create(
        layout: &Layout,
        name: &str,
        scope: Scope,
        description: Option<&str>,
        body: Option<&str>,
    ) -> Result<String, String> {
        let description =
            clean_description(description.ok_or("action \"create\" needs \"description\"")?)?;
        let body = clean_body(body.ok_or("action \"create\" needs \"body\"")?)?;
        let root = writable_root(layout, scope)?;
        let dir = root.join(name);
        match fs::create_dir(&dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err(format!(
                    "a {} skill named {name:?} already exists; use action \"update\"",
                    scope.as_str()
                ));
            }
            Err(e) => return Err(format!("cannot create skill directory: {e}")),
        }
        if let Err(e) = atomic_write(
            &dir.join(SKILL_FILE),
            render(name, &description, &body).as_bytes(),
            0o644,
        ) {
            fs::remove_dir(&dir).ok();
            return Err(format!("cannot write skill: {e:#}"));
        }
        Ok(format!("Created {} skill {name:?}.", scope.as_str()))
    }

    fn update(
        layout: &Layout,
        name: &str,
        scope: Scope,
        description: Option<&str>,
        body: Option<&str>,
    ) -> Result<String, String> {
        if description.is_none() && body.is_none() {
            return Err("action \"update\" needs \"description\" and/or \"body\"".into());
        }
        let root = writable_root(layout, scope)?;
        let dir = root.join(name);
        if load_skill(&dir, scope).is_none() {
            return Err(format!(
                "no valid {} skill named {name:?} to update",
                scope.as_str()
            ));
        }
        let (bytes, truncated) = read_head(&dir.join(SKILL_FILE), MAX_BODY_BYTES * 2)
            .map_err(|e| format!("cannot read existing skill: {e}"))?;
        if truncated {
            return Err("existing SKILL.md is too large to update safely".into());
        }
        let existing =
            String::from_utf8(bytes).map_err(|_| "existing SKILL.md is not UTF-8".to_string())?;
        let (old_frontmatter, old_body) =
            split_frontmatter(&existing).ok_or("existing SKILL.md has no frontmatter")?;

        let new_description = description.map(clean_description).transpose()?;
        let new_body = body.map(clean_body).transpose()?;
        let mut lines: Vec<String> = old_frontmatter.lines().map(str::to_string).collect();
        if let Some(description) = &new_description {
            let fields = parse_fields(old_frontmatter);
            let current =
                field(&fields, "description").ok_or("existing SKILL.md has no description")?;
            lines.splice(
                current.start..current.end,
                [format!("description: {}", quote(description))],
            );
        }
        let body = new_body.unwrap_or_else(|| old_body.trim().to_string());
        let out = format!("---\n{}\n---\n\n{body}\n", lines.join("\n"));
        atomic_write(&dir.join(SKILL_FILE), out.as_bytes(), 0o644)
            .map_err(|e| format!("cannot write skill: {e:#}"))?;
        Ok(format!("Updated {} skill {name:?}.", scope.as_str()))
    }

    fn delete(layout: &Layout, name: &str, scope: Scope) -> Result<String, String> {
        let root = scope.root(layout);
        if !root_is_safe(layout, scope, &root) {
            return Err(
                "the project skills directory resolves outside the project; refusing to delete"
                    .into(),
            );
        }
        let dir = root.join(name);
        if !is_real_dir(&dir) || !is_real_file(&dir.join(SKILL_FILE)) {
            return Err(format!(
                "no {} skill named {name:?} to delete (only directories containing a SKILL.md are removed)",
                scope.as_str()
            ));
        }
        fs::remove_dir_all(&dir).map_err(|e| format!("cannot delete skill: {e}"))?;
        Ok(format!("Deleted {} skill {name:?}.", scope.as_str()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::symlink;

    struct Env {
        _home: tempfile::TempDir,
        _proj: tempfile::TempDir,
        layout: Layout,
    }

    fn env() -> Env {
        let home = tempfile::tempdir().unwrap();
        let proj = tempfile::tempdir().unwrap();
        let layout = Layout::with_user_dir(proj.path(), home.path().join("dial")).unwrap();
        Env {
            _home: home,
            _proj: proj,
            layout,
        }
    }

    fn put(root: &Path, name: &str, file: &str, text: &str) {
        let p = root.join(name).join(file);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, text).unwrap();
    }

    fn skill_md(name: &str, description: &str) -> String {
        format!("---\nname: {name}\ndescription: {description}\n---\n\n# {name}\nDo the thing.\n")
    }

    fn manage(layout: &Layout, args: Value) -> ToolOutput {
        manage_tool::call(layout, &args)
    }

    #[test]
    fn frontmatter_subset_parses() {
        let cases: &[(&str, Option<&str>)] = &[
            (
                "---\nname: a\ndescription: Plain text\n---\nbody",
                Some("Plain text"),
            ),
            (
                "---\r\nname: a\r\ndescription: Windows line endings\r\n---\r\nbody",
                Some("Windows line endings"),
            ),
            (
                "\u{feff}---\nname: a\ndescription: With BOM\n---\n",
                Some("With BOM"),
            ),
            (
                "---\nname: a\ndescription: \"Quoted: with colon and \\\"escape\\\"\"\n---\n",
                Some("Quoted: with colon and \"escape\""),
            ),
            (
                "---\nname: a\ndescription: 'It''s single'\n---\n",
                Some("It's single"),
            ),
            (
                "---\nname: a\ndescription: >\n  folded over\n  two lines\n---\n",
                Some("folded over two lines"),
            ),
            (
                "---\nname: a\ndescription: |\n  literal\n  block\n---\n",
                Some("literal block"),
            ),
            (
                "---\nname: a\ndescription: plain with # comment\n---\n",
                Some("plain with"),
            ),
            (
                "---\nname: a\ndescription: has#hash\n---\n",
                Some("has#hash"),
            ),
            (
                "---\n# a comment\nname: a\nmetadata:\n  author: me\n  version: 2\ndescription: After nested map\n---\n",
                Some("After nested map"),
            ),
            (
                "---\nname: a\ndescription: multi\n  line plain\n---\n",
                Some("multi line plain"),
            ),
            ("---\nname: a\n---\nbody", None),
            ("---\nname: a\ndescription:\n---\n", None),
            ("---\nname: a\ndescription: never closed\n", None),
            ("name: a\ndescription: no fence\n", None),
        ];
        for (text, want) in cases {
            let got = split_frontmatter(text)
                .and_then(|(fm, _)| {
                    field(&parse_fields(fm), "description").map(|f| one_line(&f.value))
                })
                .filter(|d| !d.is_empty());
            assert_eq!(got.as_deref(), *want, "{text:?}");
        }
        let (fm, body) = split_frontmatter("---\nname: a\n---\nthe body\n").unwrap();
        assert_eq!((fm, body), ("name: a\n", "the body\n"));
    }

    #[test]
    fn discovery_precedence_and_filtering() {
        let e = env();
        let [project, user] = e.layout.skills_dirs();
        put(
            &project,
            "shared",
            SKILL_FILE,
            &skill_md("shared", "from project"),
        );
        put(
            &user,
            "shared",
            SKILL_FILE,
            &skill_md("shared", "from user"),
        );
        put(
            &user,
            "only-user",
            SKILL_FILE,
            &skill_md("only-user", "user only"),
        );
        put(
            &project,
            "mismatch",
            SKILL_FILE,
            &skill_md("other", "name differs from dir"),
        );
        put(
            &project,
            "No_Good",
            SKILL_FILE,
            &skill_md("No_Good", "bad dir name"),
        );
        put(
            &project,
            "no-desc",
            SKILL_FILE,
            "---\nname: no-desc\n---\nbody",
        );
        put(&project, "no-front", SKILL_FILE, "just text");
        put(
            &project,
            "implicit-name",
            SKILL_FILE,
            "---\ndescription: name taken from directory\n---\nbody",
        );
        fs::create_dir_all(project.join("empty-dir")).unwrap();
        fs::write(project.join("stray-file.md"), "x").unwrap();

        let skills = discover(&e.layout);
        let summary: Vec<_> = skills
            .iter()
            .map(|s| (s.name.as_str(), s.description.as_str(), s.source))
            .collect();
        assert_eq!(
            summary,
            [
                ("implicit-name", "name taken from directory", Scope::Project),
                ("only-user", "user only", Scope::User),
                ("shared", "from project", Scope::Project),
            ]
        );
        assert_eq!(skills[2].path, project.join("shared").join(SKILL_FILE));
    }

    #[test]
    #[cfg(unix)]
    fn discovery_ignores_symlinked_skills_and_escaping_project_dir() {
        let e = env();
        let [project, user] = e.layout.skills_dirs();
        let outside = tempfile::tempdir().unwrap();
        put(
            outside.path(),
            "evil",
            SKILL_FILE,
            &skill_md("evil", "outside"),
        );
        fs::create_dir_all(&project).unwrap();
        symlink(outside.path().join("evil"), project.join("evil")).unwrap();
        put(&project, "linked-file", "other.md", "x");
        symlink(
            outside.path().join("evil").join(SKILL_FILE),
            project.join("linked-file").join(SKILL_FILE),
        )
        .unwrap();
        assert!(discover(&e.layout).is_empty());

        // A whole project skills dir that is a symlink out of the project is ignored too.
        fs::remove_dir_all(&project).unwrap();
        symlink(outside.path(), &project).unwrap();
        assert!(discover(&e.layout).is_empty());
        // ... while a symlinked user dir (dotfiles setups) is the user's own choice.
        fs::create_dir_all(user.parent().unwrap()).unwrap();
        symlink(outside.path(), &user).unwrap();
        assert_eq!(discover(&e.layout).len(), 1);
    }

    #[test]
    fn catalog_lists_and_caps() {
        assert_eq!(catalog_block(&[]), "");
        let mk = |n: usize, d: String| Skill {
            name: format!("s{n}"),
            description: d,
            path: PathBuf::from("/x/SKILL.md"),
            source: Scope::User,
        };
        let few = catalog_block(&[mk(1, "does one".into()), mk(2, "does two".into())]);
        assert!(
            few.contains("- s1: does one\n")
                && few.contains("- s2: does two\n")
                && !few.contains("more skills")
        );
        let long = catalog_block(&[mk(1, "x".repeat(500))]);
        assert!(long.contains('…') && long.chars().count() < 600);
        let many: Vec<_> = (0..100).map(|n| mk(n, "y".repeat(150))).collect();
        let big = catalog_block(&many);
        assert!(
            big.chars().count() <= CATALOG_BUDGET_CHARS + 120,
            "{}",
            big.chars().count()
        );
        assert!(big.contains("more skills") && big.contains("- s0:"));
    }

    #[test]
    fn skill_tool_returns_body_without_frontmatter_and_lists_bundled_files() {
        let e = env();
        let [project, _] = e.layout.skills_dirs();
        put(
            &project,
            "deploy",
            SKILL_FILE,
            "---\nname: deploy\ndescription: Ship it\n---\n\n# Deploy\nRun scripts/ship.sh\n",
        );
        put(&project, "deploy", "scripts/ship.sh", "echo ship\n");
        put(&project, "deploy", "notes.md", "notes");

        let out = skill_tool::call(&e.layout, &json!({"name": "deploy"}));
        assert!(!out.is_error, "{}", out.text);
        assert!(out.text.contains("# Deploy\nRun scripts/ship.sh"));
        assert!(
            !out.text.contains("description: Ship it"),
            "frontmatter must be stripped"
        );
        assert!(
            out.text.contains("- notes.md (5 bytes)")
                && out.text.contains("- scripts/ship.sh (10 bytes)")
        );
        assert!(!out.text.contains("- SKILL.md"));

        let file = skill_tool::call(
            &e.layout,
            &json!({"name": "deploy", "file": "scripts/ship.sh"}),
        );
        assert!(
            !file.is_error && file.text.ends_with("echo ship\n"),
            "{}",
            file.text
        );

        let missing = skill_tool::call(&e.layout, &json!({"name": "nope"}));
        assert!(missing.is_error && missing.text.contains("deploy"));
        assert!(
            skill_tool::call(&e.layout, &json!({"name": "deploy", "file": "absent.txt"})).is_error
        );
        assert!(
            skill_tool::call(&e.layout, &json!({"name": "deploy", "file": "scripts"})).is_error
        );
        assert!(skill_tool::call(&e.layout, &json!({})).is_error);
    }

    #[test]
    #[cfg(unix)]
    fn skill_tool_refuses_traversal_symlinks_and_oversize() {
        let e = env();
        let [project, _] = e.layout.skills_dirs();
        put(&project, "s", SKILL_FILE, &skill_md("s", "d"));
        put(&project, "other", SKILL_FILE, &skill_md("other", "d2"));
        put(&project, "other", "private.txt", "private");
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret.txt"), "top secret").unwrap();
        fs::create_dir_all(project.join("s/data")).unwrap();
        symlink(
            outside.path().join("secret.txt"),
            project.join("s/leak.txt"),
        )
        .unwrap();
        symlink(outside.path(), project.join("s/linkdir")).unwrap();
        fs::write(
            project.join("s/data/big.txt"),
            vec![b'a'; MAX_READ_BYTES + 1],
        )
        .unwrap();
        fs::write(project.join("s/data/ok.txt"), vec![b'a'; MAX_READ_BYTES]).unwrap();
        fs::write(project.join("s/data/bin.dat"), [0xff, 0xfe, 0x00]).unwrap();

        for bad in [
            "../other/private.txt",
            "/etc/passwd",
            "data/../../other/private.txt",
            "leak.txt",
            "linkdir/secret.txt",
            "",
            "./../x",
            "data//..//x",
        ] {
            let out = skill_tool::call(&e.layout, &json!({"name": "s", "file": bad}));
            assert!(out.is_error, "{bad:?} must be refused: {}", out.text);
            assert!(!out.text.contains("top secret") && !out.text.contains("private"));
        }
        let name_escape = skill_tool::call(&e.layout, &json!({"name": "../other"}));
        assert!(name_escape.is_error);
        assert!(
            skill_tool::call(&e.layout, &json!({"name": "s", "file": "data/big.txt"}))
                .text
                .contains("larger")
        );
        assert!(
            !skill_tool::call(&e.layout, &json!({"name": "s", "file": "data/ok.txt"})).is_error
        );
        assert!(
            skill_tool::call(&e.layout, &json!({"name": "s", "file": "data/bin.dat"}))
                .text
                .contains("UTF-8")
        );
        // Symlinks are not even listed as bundled files.
        let body = skill_tool::call(&e.layout, &json!({"name": "s"}));
        assert!(
            !body.text.contains("leak.txt") && !body.text.contains("linkdir"),
            "{}",
            body.text
        );
    }

    #[test]
    fn manage_create_update_delete_round_trip() {
        let e = env();
        let created = manage(
            &e.layout,
            json!({"action": "create", "name": "rust-tests", "description": "Run \"cargo test\": fast\nsecond line", "body": "# Steps\n1. cargo test --lib\n"}),
        );
        assert!(!created.is_error, "{}", created.text);
        let path = e.layout.skills_dirs()[0]
            .join("rust-tests")
            .join(SKILL_FILE);
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("---\nname: rust-tests\ndescription: \"Run \\\"cargo test\\\": fast second line\"\n---\n\n# Steps"), "{text}");
        assert_eq!(
            fs::read_dir(path.parent().unwrap()).unwrap().count(),
            1,
            "no temp files left behind"
        );

        let skills = discover(&e.layout);
        assert_eq!(skills.len(), 1);
        assert_eq!(
            skills[0].description,
            "Run \"cargo test\": fast second line"
        );

        // create refuses to overwrite
        assert!(
            manage(
                &e.layout,
                json!({"action": "create", "name": "rust-tests", "description": "d", "body": "b"})
            )
            .text
            .contains("already exists")
        );

        // update body only keeps description; description only keeps body
        assert!(
            !manage(
                &e.layout,
                json!({"action": "update", "name": "rust-tests", "body": "new body"})
            )
            .is_error
        );
        assert_eq!(
            discover(&e.layout)[0].description,
            "Run \"cargo test\": fast second line"
        );
        assert!(!manage(&e.layout, json!({"action": "update", "name": "rust-tests", "description": "Better description"})).is_error);
        let after = skill_tool::call(&e.layout, &json!({"name": "rust-tests"}));
        assert!(after.text.contains("new body") && !after.text.contains("Steps"));
        assert_eq!(discover(&e.layout)[0].description, "Better description");

        assert!(!manage(&e.layout, json!({"action": "delete", "name": "rust-tests"})).is_error);
        assert!(discover(&e.layout).is_empty());
        assert!(manage(&e.layout, json!({"action": "delete", "name": "rust-tests"})).is_error);
    }

    #[test]
    fn manage_update_preserves_unknown_frontmatter_keys() {
        let e = env();
        let [project, _] = e.layout.skills_dirs();
        put(
            &project,
            "k",
            SKILL_FILE,
            "---\nname: k\nlicense: MIT\ndescription: >\n  old\n  folded\nmetadata:\n  author: me\n---\nold body\n",
        );
        assert!(
            !manage(
                &e.layout,
                json!({"action": "update", "name": "k", "description": "fresh"})
            )
            .is_error
        );
        let text = fs::read_to_string(project.join("k").join(SKILL_FILE)).unwrap();
        assert_eq!(
            text,
            "---\nname: k\nlicense: MIT\ndescription: \"fresh\"\nmetadata:\n  author: me\n---\n\nold body\n"
        );
        assert_eq!(discover(&e.layout)[0].description, "fresh");
    }

    #[test]
    fn manage_scopes_and_update_requires_existing() {
        let e = env();
        let [project, user] = e.layout.skills_dirs();
        assert!(
            manage(
                &e.layout,
                json!({"action": "update", "name": "ghost", "body": "x"})
            )
            .is_error
        );
        assert!(!manage(&e.layout, json!({"action": "create", "name": "u-skill", "description": "d", "body": "b", "scope": "user"})).is_error);
        assert!(
            user.join("u-skill").join(SKILL_FILE).exists() && !project.join("u-skill").exists()
        );
        // update without a scope finds the user skill
        assert!(
            !manage(
                &e.layout,
                json!({"action": "update", "name": "u-skill", "body": "b2"})
            )
            .is_error
        );
        assert!(
            fs::read_to_string(user.join("u-skill").join(SKILL_FILE))
                .unwrap()
                .contains("b2")
        );
        // update in the wrong explicit scope fails
        assert!(
            manage(
                &e.layout,
                json!({"action": "update", "name": "u-skill", "body": "b3", "scope": "project"})
            )
            .is_error
        );
        assert!(manage(&e.layout, json!({"action": "create", "name": "x", "description": "d", "body": "b", "scope": "global"})).is_error);
        assert!(manage(&e.layout, json!({"action": "update", "name": "u-skill"})).is_error);
    }

    #[test]
    fn manage_validates_names_content_and_secrets() {
        let e = env();
        for bad in [
            "",
            "Upper",
            "-lead",
            "has space",
            "a/b",
            "..",
            "a_b",
            &"x".repeat(65),
        ] {
            let out = manage(
                &e.layout,
                json!({"action": "create", "name": bad, "description": "d", "body": "b"}),
            );
            assert!(out.is_error, "{bad:?} must be rejected");
        }
        assert!(!manage(&e.layout, json!({"action": "create", "name": &"x".repeat(64), "description": "d", "body": "b"})).is_error);
        assert!(
            !manage(
                &e.layout,
                json!({"action": "create", "name": "0-start", "description": "d", "body": "b"})
            )
            .is_error
        );

        let base = |d: &str, b: &str| json!({"action": "create", "name": "t", "description": d, "body": b});
        for out in [
            manage(&e.layout, base("", "b")),
            manage(&e.layout, base("   \n ", "b")),
            manage(&e.layout, base(&"d".repeat(MAX_DESCRIPTION_CHARS + 1), "b")),
            manage(&e.layout, base("d", "")),
            manage(&e.layout, base("d", &"z".repeat(MAX_BODY_BYTES + 1))),
            manage(&e.layout, base("d", "---\nname: evil\n---\nbody")),
            manage(&e.layout, base("d", "hidden\u{202E} text")),
            manage(&e.layout, base("d", "-----BEGIN PRIVATE KEY-----\nabc")),
            manage(
                &e.layout,
                json!({"action": "create", "name": "t", "body": "b"}),
            ),
            manage(
                &e.layout,
                json!({"action": "create", "name": "t", "description": "d"}),
            ),
            manage(&e.layout, json!({"action": "explode", "name": "t"})),
        ] {
            assert!(out.is_error, "{}", out.text);
        }
        assert!(
            !e.layout.skills_dirs()[0].join("t").exists(),
            "failed creates leave nothing behind"
        );
        assert!(!manage(&e.layout, base("d", &"z".repeat(MAX_BODY_BYTES))).is_error);
    }

    #[test]
    #[cfg(unix)]
    fn manage_never_follows_symlinks() {
        let e = env();
        let [project, user] = e.layout.skills_dirs();
        let outside = tempfile::tempdir().unwrap();
        put(
            outside.path(),
            "victim",
            SKILL_FILE,
            &skill_md("victim", "outside"),
        );
        fs::write(outside.path().join("victim/keep.txt"), "keep").unwrap();

        fs::create_dir_all(&project).unwrap();
        symlink(outside.path().join("victim"), project.join("victim")).unwrap();
        let del = manage(
            &e.layout,
            json!({"action": "delete", "name": "victim", "scope": "project"}),
        );
        assert!(del.is_error);
        assert!(
            manage(
                &e.layout,
                json!({"action": "update", "name": "victim", "scope": "project", "body": "pwn"})
            )
            .is_error
        );
        assert!(manage(&e.layout, json!({"action": "create", "name": "victim", "scope": "project", "description": "d", "body": "b"})).is_error);
        assert!(outside.path().join("victim/keep.txt").exists());
        assert!(
            fs::read_to_string(outside.path().join("victim").join(SKILL_FILE))
                .unwrap()
                .contains("outside")
        );

        // A symlink inside a managed skill is removed as a link, not followed.
        assert!(!manage(&e.layout, json!({"action": "create", "name": "mine", "description": "d", "body": "b", "scope": "user"})).is_error);
        symlink(outside.path(), user.join("mine").join("link")).unwrap();
        assert!(
            !manage(
                &e.layout,
                json!({"action": "delete", "name": "mine", "scope": "user"})
            )
            .is_error
        );
        assert!(outside.path().join("victim/keep.txt").exists());

        // A project skills dir pointing out of the project is refused for writes.
        fs::remove_file(project.join("victim")).unwrap();
        fs::remove_dir(&project).unwrap();
        symlink(outside.path(), &project).unwrap();
        let out = manage(
            &e.layout,
            json!({"action": "create", "name": "n", "description": "d", "body": "b"}),
        );
        assert!(
            out.is_error && out.text.contains("outside the project"),
            "{}",
            out.text
        );
        assert!(!outside.path().join("n").exists());

        // Delete only removes directories that are skills.
        fs::remove_file(&project).unwrap();
        fs::create_dir_all(project.join("not-a-skill")).unwrap();
        fs::write(project.join("not-a-skill/data.txt"), "x").unwrap();
        assert!(
            manage(
                &e.layout,
                json!({"action": "delete", "name": "not-a-skill"})
            )
            .is_error
        );
        assert!(project.join("not-a-skill/data.txt").exists());
    }

    #[test]
    fn schemas_accept_valid_calls() {
        for (s, args) in [
            (skill_tool::schema(), json!({"name": "a", "file": "b"})),
            (
                manage_tool::schema(),
                json!({"action": "create", "name": "a", "description": "d", "body": "b", "scope": "user"}),
            ),
        ] {
            assert!(crate::agent::calls::validate(&s["function"]["parameters"], &args).is_ok());
        }
        assert!(
            crate::agent::calls::validate(
                &skill_tool::schema()["function"]["parameters"],
                &json!({"file": "b"})
            )
            .is_err()
        );
    }
}
