//! Ordered file resource edits prepared entirely in memory before one native commit.
use super::{
    Change, Client, Edit, MAX_FILE_BYTES, MAX_TRANSACTION_BYTES, Plan, Positions, Snapshot,
};
use crate::tools::Workspace;
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
};

struct Document {
    original: Option<String>,
    text: Option<String>,
    edits: usize,
    origin: Option<PathBuf>,
}
struct Preparation<'a> {
    workspace: &'a Workspace,
    snapshot: &'a Snapshot,
    documents: BTreeMap<PathBuf, Document>,
}
impl Preparation<'_> {
    fn path(&self, uri: &str) -> Result<PathBuf, String> {
        let path =
            super::path_from_uri(uri).ok_or("resource operation requires a local file URI")?;
        let path = self
            .workspace
            .resolve(path.to_str().ok_or("resource path must be UTF-8")?)?;
        if path
            .strip_prefix(self.workspace.root())
            .map_err(|_| "resource is outside workspace")?
            .components()
            .any(|part| part.as_os_str() == ".git")
        {
            return Err("LSP resource operations on Git internals are refused".into());
        }
        if path.is_dir() {
            return Err(
                "directory resource operations require the explicit rename_file workflow".into(),
            );
        }
        Ok(path)
    }
    fn load(&mut self, path: &PathBuf) -> Result<(), String> {
        if self.documents.contains_key(path) {
            return Ok(());
        }
        let original = if path.exists() {
            self.snapshot.validate(self.workspace, path)?;
            let source = super::read_source(path).map_err(|error| error.to_string())?;
            self.snapshot.validate(self.workspace, path)?;
            Some(source)
        } else {
            self.snapshot.validate_absent(self.workspace, path)?;
            None
        };
        self.documents.insert(
            path.clone(),
            Document {
                origin: original.as_ref().map(|_| path.clone()),
                text: original.clone(),
                original,
                edits: 0,
            },
        );
        Ok(())
    }
    fn annotation(source: &Value, operation: &Value) -> Result<(), String> {
        if let Some(id) = operation.get("annotationId") {
            let id = id.as_str().ok_or("annotationId must be a string")?;
            let annotation = source["changeAnnotations"]
                .get(id)
                .ok_or("unknown change annotation")?;
            if annotation["needsConfirmation"].as_bool() == Some(true) {
                return Err(
                    "a change annotation requires individual confirmation; nothing was written"
                        .into(),
                );
            }
        }
        Ok(())
    }
    fn option(operation: &Value, key: &str) -> Result<bool, String> {
        match operation["options"].get(key) {
            None | Some(Value::Null) => Ok(false),
            Some(value) => value
                .as_bool()
                .ok_or_else(|| format!("resource option {key} must be boolean")),
        }
    }
    fn text(&mut self, client: &Client, source: &Value, operation: &Value) -> Result<(), String> {
        let descriptor = operation
            .get("textDocument")
            .ok_or("text edit needs textDocument")?;
        let path = self.path(descriptor["uri"].as_str().ok_or("textDocument needs uri")?)?;
        self.load(&path)?;
        let doc = self.documents.get_mut(&path).unwrap();
        let text = doc
            .text
            .as_ref()
            .ok_or("text edit targets an absent or deleted resource")?;
        let version = match descriptor.get("version") {
            None | Some(Value::Null) => None,
            Some(value) => Some(
                value
                    .as_i64()
                    .ok_or("document version must be integer or null")?,
            ),
        };
        if let Some(origin) = &doc.origin {
            let original = super::read_source(origin).map_err(|error| error.to_string())?;
            client
                .validate_document(origin, &original, version)
                .map_err(|error| format!("{error:#}"))?;
        } else if version.is_some() {
            return Err("new resources cannot claim an open document version".into());
        }
        let positions = Positions::new(text);
        let mut edits = Vec::new();
        for edit in operation["edits"]
            .as_array()
            .ok_or("text edit needs edits array")?
        {
            Self::annotation(source, edit)?;
            let start = positions.offset(&edit["range"]["start"])?;
            let end = positions.offset(&edit["range"]["end"])?;
            if start > end {
                return Err("text edit range is reversed".into());
            }
            edits.push(Edit {
                start,
                end,
                text: edit["newText"].as_str().ok_or("text edit needs newText")?,
            });
        }
        if !super::sort_nonoverlapping(&mut edits) {
            return Err("overlapping or ambiguous text edits; nothing was written".into());
        }
        let mut updated = String::new();
        let mut previous = 0;
        for edit in &edits {
            updated.push_str(&text[previous..edit.start]);
            updated.push_str(edit.text);
            previous = edit.end;
            if updated.len() > MAX_FILE_BYTES {
                return Err("updated resource exceeds 4 MiB".into());
            }
        }
        updated.push_str(&text[previous..]);
        if updated.len() > MAX_FILE_BYTES {
            return Err("updated resource exceeds 4 MiB".into());
        }
        doc.text = Some(updated);
        doc.edits += edits.len();
        Ok(())
    }
    fn resource(&mut self, source: &Value, operation: &Value) -> Result<(), String> {
        Self::annotation(source, operation)?;
        if operation
            .get("options")
            .is_some_and(|value| !value.is_null() && !value.is_object())
        {
            return Err("resource options must be an object".into());
        }
        match operation["kind"]
            .as_str()
            .ok_or("resource kind must be a string")?
        {
            "create" => {
                let path = self.path(operation["uri"].as_str().ok_or("create needs uri")?)?;
                self.load(&path)?;
                let doc = self.documents.get_mut(&path).unwrap();
                if doc.text.is_some() && !Self::option(operation, "overwrite")? {
                    if Self::option(operation, "ignoreIfExists")? {
                        return Ok(());
                    }
                    return Err("create target already exists".into());
                }
                doc.text = Some(String::new());
                doc.origin = None;
                doc.edits += 1;
            }
            "rename" => {
                let from = self.path(operation["oldUri"].as_str().ok_or("rename needs oldUri")?)?;
                let to = self.path(operation["newUri"].as_str().ok_or("rename needs newUri")?)?;
                self.load(&from)?;
                self.load(&to)?;
                if from == to {
                    return Ok(());
                }
                let source = self.documents.get(&from).unwrap();
                let text = source.text.clone().ok_or("rename source does not exist")?;
                let origin = source.origin.clone();
                let destination = self.documents.get_mut(&to).unwrap();
                if destination.text.is_some() && !Self::option(operation, "overwrite")? {
                    if Self::option(operation, "ignoreIfExists")? {
                        return Ok(());
                    }
                    return Err("rename destination already exists".into());
                }
                destination.text = Some(text);
                destination.origin = origin;
                destination.edits += 1;
                let source = self.documents.get_mut(&from).unwrap();
                source.text = None;
                source.edits += 1;
            }
            "delete" => {
                let path = self.path(operation["uri"].as_str().ok_or("delete needs uri")?)?;
                self.load(&path)?;
                let doc = self.documents.get_mut(&path).unwrap();
                if doc.text.is_none() && !Self::option(operation, "ignoreIfNotExists")? {
                    return Err("delete resource does not exist".into());
                }
                doc.text = None;
                doc.edits += 1;
            }
            kind => return Err(format!("unknown resource operation {kind:?}")),
        }
        Ok(())
    }
}

pub(super) fn prepare(
    ws: &Workspace,
    sources: &[(&Client, &Value)],
    snapshot: &Snapshot,
) -> Result<Plan, String> {
    let mut preparation = Preparation {
        workspace: ws,
        snapshot,
        documents: BTreeMap::new(),
    };
    let mut prior_targets = BTreeSet::new();
    for &(client, source) in sources {
        if source.get("changes").is_some() && source.get("documentChanges").is_some() {
            return Err("WorkspaceEdit mixing changes and documentChanges is refused".into());
        }
        let before: BTreeMap<_, _> = preparation
            .documents
            .iter()
            .map(|(path, document)| (path.clone(), document.edits))
            .collect();
        if let Some(changes) = source.get("documentChanges") {
            for operation in changes
                .as_array()
                .ok_or("documentChanges must be an array")?
            {
                if operation.get("kind").is_some() {
                    preparation.resource(source, operation)?;
                } else {
                    preparation.text(client, source, operation)?;
                }
            }
        } else if let Some(changes) = source.get("changes") {
            for (uri, edits) in changes.as_object().ok_or("changes must be an object")? {
                preparation.text(
                    client,
                    source,
                    &serde_json::json!({"textDocument":{"uri":uri},"edits":edits}),
                )?;
            }
        }
        // Separate servers compute against the same initial snapshot, not each other's operations.
        let touched: BTreeSet<_> = preparation
            .documents
            .iter()
            .filter(|(path, document)| before.get(*path) != Some(&document.edits))
            .map(|(path, _)| path.clone())
            .collect();
        if !touched.is_disjoint(&prior_targets) {
            // Existing text-only multi-server merging remains handled by the original preparation.
            return Err("multiple resource-edit sources require disjoint targets; select one language server".into());
        }
        prior_targets.extend(touched);
    }
    let targets = preparation.documents.keys().cloned().collect();
    let mut bytes = 0usize;
    let mut changes = Vec::new();
    for (abs, document) in preparation.documents {
        bytes = bytes
            .checked_add(document.original.as_ref().map_or(0, String::len))
            .and_then(|n| n.checked_add(document.text.as_ref().map_or(0, String::len)))
            .ok_or("resource transaction size overflow")?;
        if bytes > MAX_TRANSACTION_BYTES {
            return Err("resource transaction exceeds 32 MiB".into());
        }
        if document.original != document.text {
            let native_original = crate::tools::edit::edit_transaction::read(ws, &abs)?;
            let permissions_from = document.origin.filter(|origin| *origin != abs);
            changes.push(Change {
                abs,
                original: document.original,
                updated: document.text,
                edits: document.edits,
                permissions_from,
                native_original,
            });
        }
    }
    Ok(Plan { changes, targets })
}
