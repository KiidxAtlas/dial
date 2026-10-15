//! Validated, observer-backed LSP text transactions.
//!
//! Ordered file resource operations and text edits share the native transaction. Original contents are staged as
//! independent same-directory backups until every replacement succeeds; commit failures restore
//! already replaced files. This is rollback atomicity, not a filesystem-wide atomic rename or
//! crash journal. An external process changing directory permissions can prevent rollback; in
//! that case the error preserves and names the original backup instead of claiming success.

use super::client::Client;
use super::text::{path_from_uri, read_source};
use crate::tools::Workspace;
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
mod resource_ops;

const MAX_FILES: usize = 100_000;
const MAX_TRANSACTION_BYTES: usize = crate::tools::edit_transaction::MAX_BYTES;
const MAX_FILE_BYTES: usize = crate::tools::edit_transaction::MAX_FILE;

#[derive(Clone, PartialEq, Eq)]
struct Identity {
    id: crate::platform::FileIdentity,
    len: u64,
    modified: std::time::SystemTime,
    mode: u32,
    #[cfg(unix)]
    ctime: (i64, i64),
}

impl Identity {
    fn of(path: &Path, meta: &fs::Metadata) -> Result<Self, String> {
        Ok(Self {
            id: crate::platform::fs::path_identity(path).map_err(|e| e.to_string())?,
            len: meta.len(),
            modified: meta.modified().map_err(|e| e.to_string())?,
            mode: crate::platform::fs::mode(meta),
            #[cfg(unix)]
            ctime: (meta.ctime(), meta.ctime_nsec()),
        })
    }
}

/// Capture identities before asking the server, including unopened cross-file edit targets.
/// Symlinks are never followed by the traversal; canonical in-root targets are captured through
/// their ordinary paths. Git internals are not refactoring targets.
pub(super) struct Snapshot {
    root: PathBuf,
    files: BTreeMap<PathBuf, Identity>,
}

impl Snapshot {
    pub fn capture(ws: &Workspace) -> Result<Self, String> {
        let mut files = BTreeMap::new();
        let mut dirs = vec![ws.root().to_owned()];
        let mut entries = 0usize;
        while let Some(dir) = dirs.pop() {
            let iter = fs::read_dir(&dir)
                .map_err(|e| format!("cannot snapshot {}: {e}", dir.display()))?;
            for entry in iter {
                let entry = entry.map_err(|e| format!("cannot snapshot {}: {e}", dir.display()))?;
                entries += 1;
                if entries > MAX_FILES {
                    return Err(format!(
                        "refactoring snapshot exceeds {MAX_FILES} entries; nothing was written"
                    ));
                }
                if entry.file_name() == ".git" {
                    continue;
                }
                let path = entry.path();
                if ws.is_protected(&path) {
                    continue;
                }
                let meta = fs::symlink_metadata(&path)
                    .map_err(|e| format!("cannot snapshot {}: {e}", path.display()))?;
                if meta.is_dir() {
                    dirs.push(path);
                } else if meta.is_file() {
                    files.insert(path.clone(), Identity::of(&path, &meta)?);
                }
            }
        }
        Ok(Self {
            root: ws.root().to_owned(),
            files,
        })
    }

    pub fn validate(&self, ws: &Workspace, path: &Path) -> Result<(), String> {
        if self.root != ws.root() {
            return Err("refactoring belongs to a different workspace root".into());
        }
        let expected = self.files.get(path).ok_or_else(|| {
            format!(
                "{} was not a regular refactoring target before the server request",
                path.display()
            )
        })?;
        let current = fs::symlink_metadata(path)
            .map_err(|e| format!("{} changed since the server request: {e}", path.display()))?;
        if !current.is_file() || &Identity::of(path, &current)? != expected {
            return Err(format!(
                "{} changed since the server request; request a fresh refactoring",
                path.display()
            ));
        }
        Ok(())
    }

    pub fn validate_source(&self, ws: &Workspace, path: &Path, source: &str) -> Result<(), String> {
        self.validate(ws, path)?;
        let current =
            read_source(path).map_err(|e| format!("cannot re-read {}: {e}", path.display()))?;
        if current != source {
            return Err(format!(
                "{} changed before the server request; nothing was written",
                path.display()
            ));
        }
        self.validate(ws, path)
    }
}

pub(super) struct Change {
    abs: PathBuf,
    original: Option<String>,
    updated: Option<String>,
    edits: usize,
    permissions_from: Option<PathBuf>,
    native_original: Option<super::super::edit_transaction::Original>,
}

pub(super) struct Plan {
    changes: Vec<Change>,
    targets: Vec<PathBuf>,
}

struct Edit<'a> {
    start: usize,
    end: usize,
    text: &'a str,
}

/// Index line boundaries once per target, then convert each UTF-16 column strictly. Display
/// helpers intentionally clamp positions; application rejects surrogate interiors and overruns.
struct Positions<'a> {
    text: &'a str,
    lines: Vec<(usize, usize)>,
}

impl<'a> Positions<'a> {
    fn new(text: &'a str) -> Self {
        let mut lines = Vec::new();
        let bytes = text.as_bytes();
        let mut start = 0usize;
        let mut byte = 0usize;
        while byte < bytes.len() {
            if bytes[byte] == b'\r' || bytes[byte] == b'\n' {
                lines.push((start, byte));
                if bytes[byte] == b'\r' && bytes.get(byte + 1) == Some(&b'\n') {
                    byte += 1;
                }
                start = byte + 1;
            }
            byte += 1;
        }
        lines.push((start, bytes.len()));
        Self { text, lines }
    }

    fn offset(&self, position: &Value) -> Result<usize, String> {
        let line = position
            .get("line")
            .and_then(Value::as_u64)
            .ok_or("edit position needs an unsigned line")?;
        let character = position
            .get("character")
            .and_then(Value::as_u64)
            .ok_or("edit position needs an unsigned UTF-16 character")?;
        let index = usize::try_from(line).map_err(|_| "edit line is too large")?;
        let &(start, end) = self
            .lines
            .get(index)
            .ok_or_else(|| format!("edit line {line} is beyond the document"))?;
        let mut units = 0u64;
        for (byte, ch) in self.text[start..end].char_indices() {
            if units == character {
                return Ok(start + byte);
            }
            units += ch.len_utf16() as u64;
            if units > character {
                return Err("edit position splits a UTF-16 surrogate pair".into());
            }
        }
        if units == character {
            Ok(end)
        } else {
            Err(format!(
                "edit UTF-16 character {character} is beyond line {line}"
            ))
        }
    }
}

fn target(ws: &Workspace, uri: &str) -> Result<PathBuf, String> {
    let path =
        path_from_uri(uri).ok_or_else(|| format!("unsupported or invalid file URI {uri:?}"))?;
    let path = path.to_str().ok_or("refactoring paths must be UTF-8")?;
    let abs = ws.resolve(path)?;
    if !abs.is_file() {
        return Err(format!("{uri:?} is not a regular file in the workspace"));
    }
    if abs
        .strip_prefix(ws.root())
        .map_err(|_| "edit target is outside the workspace")?
        .components()
        .any(|part| part.as_os_str() == ".git")
    {
        return Err("LSP edits to Git internals are refused".into());
    }
    Ok(abs)
}

/// One document touched by one or more servers: each contributing client must still hold the exact
/// text its edits were computed against.
struct Pending<'a> {
    checks: Vec<(&'a Client, Option<i64>)>,
    edits: Vec<(&'a Value, &'a Value)>,
}

/// Parses one `WorkspaceEdit` into per-document edit lists. `null` means no edits.
fn collect_documents<'v>(
    ws: &Workspace,
    value: &'v Value,
) -> Result<BTreeMap<PathBuf, (Option<i64>, Vec<&'v Value>)>, String> {
    let mut documents: BTreeMap<PathBuf, (Option<i64>, Vec<&'v Value>)> = BTreeMap::new();
    if value.is_null() {
        return Ok(documents);
    }
    if !value.is_object() {
        return Err("WorkspaceEdit must be an object".into());
    }
    if value.get("changes").is_some() && value.get("documentChanges").is_some() {
        return Err(
            "WorkspaceEdit mixing changes and documentChanges is ambiguous and refused".into(),
        );
    }
    if let Some(changes) = value.get("changes") {
        let changes = changes
            .as_object()
            .ok_or("WorkspaceEdit.changes must be an object")?;
        for (uri, edits) in changes {
            let edits = edits.as_array().ok_or("text edits must be an array")?;
            let entry = documents
                .entry(target(ws, uri)?)
                .or_insert_with(|| (None, Vec::new()));
            entry.1.extend(edits);
        }
    }
    if let Some(changes) = value.get("documentChanges") {
        let changes = changes
            .as_array()
            .ok_or("WorkspaceEdit.documentChanges must be an array")?;
        // Inspect every operation before loading or staging any target.
        for change in changes {
            let doc = change
                .get("textDocument")
                .ok_or("document change needs textDocument")?;
            let uri = doc
                .get("uri")
                .and_then(Value::as_str)
                .ok_or("textDocument needs a file URI")?;
            let version = match doc.get("version") {
                Some(Value::Null) | None => None,
                Some(version) => Some(
                    version
                        .as_i64()
                        .ok_or("document version must be an integer or null")?,
                ),
            };
            let edits = change
                .get("edits")
                .and_then(Value::as_array)
                .ok_or("document change needs edits")?;
            let entry = documents
                .entry(target(ws, uri)?)
                .or_insert_with(|| (version, Vec::new()));
            if entry.0 != version {
                return Err("multiple edit batches disagree on document version".into());
            }
            entry.1.extend(edits);
        }
    }
    Ok(documents)
}

impl Plan {
    /// Prepares one server's `WorkspaceEdit` as a transaction (see [`Self::prepare_sources`]).
    pub fn prepare(
        ws: &Workspace,
        client: &Client,
        snapshot: &Snapshot,
        value: &Value,
    ) -> Result<Self, String> {
        Self::prepare_sources(ws, &[(client, value)], snapshot)
    }

    /// Prepares the `WorkspaceEdit`s of one or more servers as a single transaction. Every document
    /// is validated against each client that edited it, and each edit's change annotation is looked
    /// up in the `WorkspaceEdit` it came from.
    pub fn prepare_sources(
        ws: &Workspace,
        sources: &[(&Client, &Value)],
        snapshot: &Snapshot,
    ) -> Result<Self, String> {
        if sources.iter().any(|(_, value)| {
            value["documentChanges"]
                .as_array()
                .is_some_and(|changes| changes.iter().any(|change| change.get("kind").is_some()))
        }) {
            return resource_ops::prepare(ws, sources, snapshot);
        }
        let mut documents: BTreeMap<PathBuf, Pending<'_>> = BTreeMap::new();
        for &(client, value) in sources {
            for (abs, (version, edits)) in collect_documents(ws, value)? {
                let entry = documents.entry(abs).or_insert_with(|| Pending {
                    checks: Vec::new(),
                    edits: Vec::new(),
                });
                entry.checks.push((client, version));
                entry
                    .edits
                    .extend(edits.into_iter().map(|edit| (value, edit)));
            }
        }
        let targets = documents.keys().cloned().collect();
        let mut changes = Vec::with_capacity(documents.len());
        let mut bytes = 0usize;
        for (abs, pending) in documents {
            snapshot.validate(ws, &abs)?;
            let original =
                read_source(&abs).map_err(|e| format!("cannot read {}: {e}", abs.display()))?;
            snapshot.validate(ws, &abs)?;
            for (client, version) in &pending.checks {
                client
                    .validate_document(&abs, &original, *version)
                    .map_err(|e| format!("{e:#}"))?;
            }
            bytes = bytes
                .checked_add(original.len())
                .ok_or("transaction size overflow")?;
            if bytes > MAX_TRANSACTION_BYTES {
                return Err(
                    "WorkspaceEdit exceeds the 32 MiB transaction limit; nothing was written"
                        .into(),
                );
            }
            let positions = Positions::new(&original);
            let mut edits = Vec::with_capacity(pending.edits.len());
            for (source, edit) in pending.edits {
                if let Some(id) = edit.get("annotationId") {
                    let id = id.as_str().ok_or("annotationId must be a string")?;
                    let annotation = source
                        .get("changeAnnotations")
                        .and_then(|items| items.get(id))
                        .ok_or("text edit refers to an unknown change annotation")?;
                    if annotation.get("needsConfirmation").and_then(Value::as_bool) == Some(true) {
                        return Err("a change annotation requires individual confirmation, which this tool does not support; nothing was written".into());
                    }
                }
                let range = edit.get("range").ok_or("text edit needs range")?;
                let start = positions.offset(&range["start"])?;
                let end = positions.offset(&range["end"])?;
                if start > end {
                    return Err("text edit range is reversed".into());
                }
                let text = edit
                    .get("newText")
                    .and_then(Value::as_str)
                    .ok_or("text edit needs newText")?;
                bytes = bytes
                    .checked_add(text.len())
                    .ok_or("transaction size overflow")?;
                if bytes > MAX_TRANSACTION_BYTES {
                    return Err(
                        "WorkspaceEdit exceeds the 32 MiB transaction limit; nothing was written"
                            .into(),
                    );
                }
                edits.push(Edit { start, end, text });
            }
            edits.sort_by_key(|edit| (edit.start, edit.end));
            if edits
                .windows(2)
                .any(|pair| pair[0].end > pair[1].start || pair[0].start == pair[1].start)
            {
                return Err(format!(
                    "overlapping or ambiguous edits for {}; nothing was written",
                    abs.display()
                ));
            }
            let count = edits.len();
            let mut updated_len = original.len();
            for edit in &edits {
                updated_len = updated_len
                    .checked_sub(edit.end - edit.start)
                    .and_then(|len| len.checked_add(edit.text.len()))
                    .ok_or("updated file size overflow")?;
            }
            if updated_len > MAX_FILE_BYTES {
                return Err(format!(
                    "updated {} exceeds the 4 MiB source limit; nothing was written",
                    abs.display()
                ));
            }
            let mut updated = String::with_capacity(updated_len);
            let mut previous = 0usize;
            for edit in &edits {
                updated.push_str(&original[previous..edit.start]);
                updated.push_str(edit.text);
                previous = edit.end;
            }
            updated.push_str(&original[previous..]);
            if updated != original {
                changes.push(Change {
                    native_original: super::super::edit_transaction::read(ws, &abs)?,
                    abs,
                    original: Some(original),
                    updated: Some(updated),
                    edits: count,
                    permissions_from: None,
                });
            }
        }
        Ok(Self { changes, targets })
    }

    /// The full canonical server-provided target set, including edits that produce no change.
    /// Owner path authorization must judge this set before any observer or staging mutation.
    pub fn targets(&self) -> &[PathBuf] {
        &self.targets
    }

    pub fn summary(&self, ws: &Workspace) -> String {
        if self.changes.is_empty() {
            return "no text changes".into();
        }
        self.changes
            .iter()
            .map(|change| format!("{}: {} edit(s)", ws.rel(&change.abs), change.edits))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The plan for a request no server answered: nothing to stage or write.
    pub fn empty() -> Self {
        Self {
            changes: Vec::new(),
            targets: Vec::new(),
        }
    }

    /// Writes each original text back. Used when a step after the text commit fails, so the
    /// workspace returns to the state the snapshot validated.
    pub fn revert(&self, ws: &Workspace) -> Result<Vec<String>, String> {
        let mut changes = Vec::with_capacity(self.changes.len());
        for change in &self.changes {
            let mut restored = super::super::edit_transaction::Change::load(
                ws,
                change.abs.clone(),
                change.original.clone(),
            )?;
            if restored.before.as_ref().map(|original| &original.text) != change.updated.as_ref() {
                super::super::execution::unsettled();
                return Err(
                    "resource changed after commit; rollback refused to overwrite external work"
                        .into(),
                );
            }
            if let Some(original) = &change.native_original {
                super::super::edit_transaction::inherit_permissions(&mut restored, original);
            }
            changes.push(restored);
        }
        let warnings = super::super::edit_transaction::commit(ws, &changes)?;
        let mut store = ws.store();
        for change in &self.changes {
            store.forget(&change.abs);
        }
        Ok(warnings)
    }

    pub fn commit(&self, ws: &Workspace, snapshot: &Snapshot) -> Result<Vec<String>, String> {
        let mut changes = Vec::with_capacity(self.changes.len());
        for change in &self.changes {
            if change.original.is_some() {
                snapshot.validate(ws, &change.abs)?;
            } else if snapshot.files.contains_key(&change.abs) {
                return Err("new resource existed before the server request".into());
            }
            if ws.resolve(change.abs.to_str().ok_or("path is not UTF-8")?)? != change.abs {
                return Err(
                    "edit target changed its symlink resolution; nothing was written".into(),
                );
            }
            let mut transaction = super::super::edit_transaction::Change::load(
                ws,
                change.abs.clone(),
                change.updated.clone(),
            )?;
            if transaction.before.as_ref().map(|original| &original.text)
                != change.original.as_ref()
            {
                return Err(format!(
                    "{} changed before commit; nothing was written",
                    ws.rel(&change.abs)
                ));
            }
            if let Some(source) = &change.permissions_from {
                snapshot.validate(ws, source)?;
                let original = super::super::edit_transaction::read(ws, source)?
                    .ok_or("rename source disappeared before commit")?;
                super::super::edit_transaction::inherit_permissions(&mut transaction, &original);
            }
            changes.push(transaction);
        }
        let warnings = super::super::edit_transaction::commit(ws, &changes)?;
        let mut store = ws.store();
        for change in &self.changes {
            store.forget(&change.abs);
        }
        Ok(warnings)
    }

    pub fn paths(&self) -> impl Iterator<Item = &Path> {
        self.changes.iter().map(|change| change.abs.as_path())
    }
    pub fn removed(&self, path: &Path) -> bool {
        self.changes
            .iter()
            .any(|change| change.abs == path && change.updated.is_none())
    }
}
