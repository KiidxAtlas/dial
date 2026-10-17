//! Owner-local notebook evidence and revision-checked, observer-backed cell transactions.
use crate::config::types::Config;
use crate::tools::{
    ToolOutput, Workspace,
    integrations::ext::{Tool, ToolContext},
    present,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
#[cfg(unix)]
use std::os::{
    fd::{AsRawFd, FromRawFd},
    unix::ffi::OsStrExt,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::{Read, Write},
    path::{Component, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};
const MAX: usize = 8 * 1024 * 1024;
const PAGE: usize = 16 * 1024;
static NEXT: AtomicU64 = AtomicU64::new(1);
#[derive(Default)]
pub struct Notebook {
    seen: Mutex<BTreeMap<(PathBuf, String, usize), Evidence>>,
}
struct Evidence {
    hash: String,
    coverage: Vec<(usize, usize)>,
    length: usize,
}
struct Loaded {
    path: PathBuf,
    dir: File,
    name: PathBuf,
    file: File,
    identity: crate::platform::FileIdentity,
    revision: String,
    raw: String,
    value: Value,
}
fn identity(file: &File) -> Result<crate::platform::FileIdentity, String> {
    crate::platform::fs::identity(file).map_err(|e| e.to_string())
}
fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn string<'a>(a: &'a Value, k: &str) -> Result<&'a str, String> {
    match a.get(k) {
        None | Some(Value::Null) => Err(format!("missing required argument {k:?}")),
        Some(value) => value
            .as_str()
            .ok_or_else(|| format!("{k} must be a string")),
    }
}
fn index(a: &Value, k: &str) -> Result<usize, String> {
    match a.get(k) {
        None | Some(Value::Null) => Err(format!("missing required argument {k:?}")),
        Some(value) => value
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .ok_or_else(|| format!("{k} must be a nonnegative integer")),
    }
}
fn source(c: &Value) -> Result<std::borrow::Cow<'_, str>, String> {
    match c.get("source") {
        Some(Value::String(s)) => Ok(std::borrow::Cow::Borrowed(s)),
        Some(Value::Array(a)) if a.iter().all(Value::is_string) => Ok(std::borrow::Cow::Owned(
            a.iter().map(|v| v.as_str().unwrap()).collect(),
        )),
        _ => Err("cell source must be a string or string array".into()),
    }
}
fn valid_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}
pub(crate) fn validate(v: &Value) -> Result<(), String> {
    if v.get("nbformat").and_then(Value::as_u64) != Some(4)
        || v.get("nbformat_minor").and_then(Value::as_u64).is_none()
        || !v.get("metadata").is_some_and(Value::is_object)
    {
        return Err("only valid nbformat 4 notebooks are supported".into());
    }
    let cells = v
        .get("cells")
        .and_then(Value::as_array)
        .ok_or("notebook needs cells array")?;
    if cells.len() > 10000 {
        return Err("notebook exceeds 10000 cells".into());
    }
    let mut ids = BTreeSet::new();
    for c in cells {
        let kind = c
            .get("cell_type")
            .and_then(Value::as_str)
            .ok_or("cell needs cell_type")?;
        if !matches!(kind, "code" | "markdown" | "raw")
            || !c.get("metadata").is_some_and(Value::is_object)
        {
            return Err("invalid cell type or metadata".into());
        }
        if !c.get("source").is_some_and(|s| {
            s.is_string() || s.as_array().is_some_and(|a| a.iter().all(Value::is_string))
        }) {
            return Err("cell source must be a string or string array".into());
        }
        if kind == "code"
            && (!c.get("outputs").is_some_and(Value::is_array)
                || !c
                    .get("execution_count")
                    .is_some_and(|n| n.is_null() || n.as_u64().is_some()))
        {
            return Err("invalid code outputs or execution_count".into());
        }
        if let Some(id) = c.get("id") {
            let id = id.as_str().ok_or("cell id must be a string")?;
            if !valid_id(id) || !ids.insert(id) {
                return Err("invalid or duplicate cell id".into());
            }
        } else if v["nbformat_minor"].as_u64().unwrap() >= 5 {
            return Err("nbformat 4.5+ requires cell IDs; no implicit migration".into());
        }
        if c.get("attachments").is_some_and(|a| !a.is_object()) {
            return Err("invalid cell attachments".into());
        }
    }
    Ok(())
}
fn load(ws: &Workspace, input: &str) -> Result<Loaded, String> {
    let path = ws.resolve(input)?;
    if !path
        .extension()
        .is_some_and(|x| x.eq_ignore_ascii_case("ipynb"))
    {
        return Err("notebook path must end in .ipynb".into());
    }
    // Reject lexical symlinks as well as descriptor-level substitutions.
    let lexical = if std::path::Path::new(input).is_absolute() {
        PathBuf::from(input)
    } else {
        ws.root().join(input)
    };
    let mut check = ws.root().to_path_buf();
    for c in lexical
        .strip_prefix(ws.root())
        .map_err(|_| "outside workspace")?
        .components()
    {
        match c {
            Component::Normal(n) => {
                check.push(n);
                if std::fs::symlink_metadata(&check)
                    .map_err(|e| e.to_string())?
                    .file_type()
                    .is_symlink()
                {
                    return Err("notebook symlinks are unsupported".into());
                }
            }
            Component::CurDir => {}
            _ => return Err("notebook path must not contain parent traversal".into()),
        }
    }
    if let Some(crate::tools::exec::sandbox::Policy::Dev { deny_read, .. }) =
        ws.policy.as_ref().filter(|_| !ws.unsandboxed)
    {
        for deny in deny_read {
            if path.starts_with(crate::tools::exec::sandbox::resolve(deny)?) {
                return Err("notebook access denied by workspace policy".into());
            }
        }
    }
    let relative = path
        .strip_prefix(ws.root())
        .map_err(|_| "outside workspace")?;
    let name = PathBuf::from(path.file_name().ok_or("missing filename")?);
    let parent = relative.parent().ok_or("missing parent")?;
    let dir =
        crate::platform::fs::open_beneath(ws.root(), parent, true).map_err(|e| e.to_string())?;
    let mut file =
        crate::platform::fs::open_beneath_handle(&dir, &name).map_err(|e| e.to_string())?;
    let meta = file.metadata().map_err(|e| e.to_string())?;
    if !meta.is_file() || meta.len() > MAX as u64 {
        return Err("notebook must be a regular file no larger than 8 MiB".into());
    }
    let mut bytes = Vec::new();
    (&mut file)
        .take(MAX as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    let file_identity = identity(&file)?;
    let modified = meta.modified().map_err(|e| e.to_string())?;
    if bytes.len() > MAX
        || identity(&file)? != file_identity
        || file.metadata().map_err(|e| e.to_string())?.len() != meta.len()
    {
        return Err("notebook changed during read".into());
    }
    let raw = String::from_utf8(bytes).map_err(|_| "notebook JSON must be UTF-8")?;
    let value = crate::tools::edit::edit_source::parse_notebook(&raw)?;
    validate(&value)?;
    let revision = hash(
        format!(
            "{}:{:?}:{}:{:?}",
            hash(raw.as_bytes()),
            file_identity,
            meta.len(),
            modified
        )
        .as_bytes(),
    );
    Ok(Loaded {
        path,
        dir,
        name,
        file,
        identity: file_identity,
        revision,
        raw,
        value,
    })
}
impl Notebook {
    fn run(&self, ctx: &ToolContext, a: &Value, cancel: &AtomicBool) -> Result<Value, String> {
        let object = a
            .as_object()
            .ok_or("notebook arguments must be an object")?;
        for (key, value) in object.iter().filter(|(_, value)| !value.is_null()) {
            let valid = match key.as_str() {
                "action" | "path" | "revision" | "source" | "old_source" | "cell_type" | "id" => {
                    value.is_string()
                }
                "index" | "offset" | "limit" | "to" => value.as_u64().is_some(),
                "preserve_outputs" | "outputs_are_stale" => value.is_boolean(),
                _ => false,
            };
            if !valid {
                return Err(format!("unknown or invalid notebook argument: {key}"));
            }
        }
        let _serial = loop {
            if cancel.load(Ordering::Relaxed) {
                return Err("notebook operation cancelled while waiting for workspace".into());
            }
            if let Some(guard) = ctx.workspace.try_serial_operation() {
                break guard;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        let action = string(a, "action")?;
        if !matches!(action, "read" | "edit" | "insert" | "delete" | "move") {
            return Err("unknown notebook action".into());
        }
        let mut loaded = load(ctx.workspace, string(a, "path")?)?;
        let mut seen = self.seen.lock().unwrap_or_else(|e| e.into_inner());
        let cells = loaded.value["cells"].as_array().unwrap();
        if action == "read" {
            let offset = if present(a, "offset") {
                index(a, "offset")?
            } else {
                0
            };
            seen.retain(|(p, r, _), _| p != &loaded.path || r == &loaded.revision);
            if !present(a, "index") {
                let limit = if present(a, "limit") {
                    index(a, "limit")?
                } else {
                    64
                };
                if limit == 0 || limit > 256 || offset > cells.len() {
                    return Err(
                        "cell index page requires limit 1..256 and an in-range offset".into(),
                    );
                }
                let end = offset.saturating_add(limit).min(cells.len());
                let mut rows = Vec::with_capacity(end - offset);
                for (i, c) in cells.iter().enumerate().take(end).skip(offset) {
                    let s = source(c)?;
                    let h = hash(s.as_bytes());
                    let length = s.chars().count();
                    let key = (loaded.path.clone(), loaded.revision.clone(), i);
                    seen.entry(key).or_insert(Evidence {
                        hash: h.clone(),
                        coverage: if length == 0 {
                            vec![(0, 0)]
                        } else {
                            Vec::new()
                        },
                        length,
                    });
                    rows.push(json!({"index":i,"id":c.get("id"),"cell_type":c["cell_type"],"source_hash":h,"total_characters":length}));
                }
                let fresh = load(ctx.workspace, string(a, "path")?)?;
                if fresh.identity != loaded.identity
                    || fresh.revision != loaded.revision
                    || cancel.load(Ordering::Relaxed)
                {
                    seen.retain(|(p, _, _), _| p != &loaded.path);
                    return Err("notebook changed or read cancelled".into());
                }
                while seen.len() > 512 {
                    seen.pop_first();
                }
                return Ok(
                    json!({"path":ctx.workspace.rel(&loaded.path),"revision":loaded.revision,"cell_count":cells.len(),"cells":rows,"offset":offset,"end":end,"next_offset":if end<cells.len(){Some(end)}else{None},"witness_capacity":512}),
                );
            }
            let i = index(a, "index")?;
            let c = cells.get(i).ok_or("cell index out of range")?;
            let s = source(c)?;
            let limit = if present(a, "limit") {
                index(a, "limit")?
            } else {
                PAGE / 4
            };
            let length = s.chars().count();
            if limit == 0 || limit > PAGE / 4 || offset > length {
                return Err("offset/limit invalid; limit is 1..4096 Unicode characters".into());
            }
            let text: String = s.chars().skip(offset).take(limit).collect();
            let end = offset + text.chars().count();
            let h = hash(s.as_bytes());
            let fresh = load(ctx.workspace, string(a, "path")?)?;
            if fresh.identity != loaded.identity || fresh.revision != loaded.revision {
                return Err("notebook changed during read".into());
            }
            if cancel.load(Ordering::Relaxed) {
                return Err("notebook read cancelled".into());
            }
            let e = seen
                .entry((loaded.path.clone(), loaded.revision.clone(), i))
                .or_insert(Evidence {
                    hash: h.clone(),
                    coverage: Vec::new(),
                    length,
                });
            e.coverage.push((offset, end));
            e.coverage.sort_unstable();
            let mut merged: Vec<(usize, usize)> = Vec::new();
            for &(start, end) in &e.coverage {
                if let Some(last) = merged.last_mut().filter(|last| start <= last.1) {
                    last.1 = last.1.max(end);
                } else {
                    merged.push((start, end));
                }
            }
            e.coverage = if merged.len() <= 64 {
                merged
            } else {
                vec![(offset, end)]
            };
            // Only genuine returned pages from this fresh revision grant projection evidence.
            // A failed witness does not hide the actual successful source read.
            let projection_witness_error = ctx
                .workspace
                .record_notebook_cell_witnesses(&loaded.path, &loaded.raw, i, &e.coverage)
                .err();
            while seen.len() > 512 {
                seen.pop_first();
            }
            return Ok(
                json!({"path":ctx.workspace.rel(&loaded.path),"revision":loaded.revision,"index":i,"id":c.get("id"),"cell_type":c["cell_type"],"cell_count":cells.len(),"source_hash":h,"source":text,"offset":offset,"end":end,"total_characters":length,"complete":offset==0 && end==length,"next_offset":if end<length {Some(end)}else{None},"projection_witness_error":projection_witness_error}),
            );
        }
        if string(a, "revision")? != loaded.revision {
            return Err("stale notebook revision; reread the cells".into());
        }
        let require_seen = |i: usize| -> Result<(), String> {
            let c = cells.get(i).ok_or("cell index out of range")?;
            let s = source(c)?;
            let e = seen
                .get(&(loaded.path.clone(), loaded.revision.clone(), i))
                .ok_or("cell witness unavailable to this execution owner; read it again")?;
            if e.hash != hash(s.as_bytes()) {
                return Err("cell source changed".into());
            }
            if !(e.coverage.len() == 1 && e.coverage[0] == (0, e.length)) {
                let old = string(a,"old_source").map_err(|_| "partial read: read complete source or supply exact old_source matching observed source_hash")?;
                if hash(old.as_bytes()) != e.hash || old != s.as_ref() {
                    return Err("old_source does not match observed cell".into());
                }
            }
            Ok(())
        };
        let i = index(a, "index")?;
        match action {
            "edit" | "delete" => require_seen(i)?,
            "move" => {
                require_seen(i)?;
                require_seen(index(a, "to")?)?;
            }
            "insert" => {
                if i > cells.len() {
                    return Err("insert index out of range".into());
                }
                if i > 0 {
                    require_seen(i - 1)?;
                }
                if i < cells.len() {
                    require_seen(i)?;
                }
            }
            _ => unreachable!(),
        }
        ctx.authorize_mutation("notebook", std::slice::from_ref(&loaded.path), false)?;
        if cancel.load(Ordering::Relaxed) {
            return Err("notebook operation cancelled".into());
        }
        if (action == "edit" && string(a, "source")? == source(&cells[i])?.as_ref())
            || (action == "move" && i == index(a, "to")?)
        {
            return Ok(
                json!({"path":ctx.workspace.rel(&loaded.path),"action":action,"changed":false,"revision":loaded.revision,"cell_count":cells.len(),"reread_required":false}),
            );
        }
        let minor = loaded.value["nbformat_minor"].as_u64().unwrap();
        let mut updated = std::mem::take(&mut loaded.value);
        let list = updated["cells"].as_array_mut().unwrap();
        let mut origins: Vec<Option<usize>> = (0..list.len()).map(Some).collect();
        let mut preserved_stale = false;
        match action {
            "edit" => {
                let new = string(a, "source")?;
                if new.len() > MAX {
                    return Err("source too large".into());
                }
                list[i]["source"] = Value::String(new.into());
                if list[i]["cell_type"] == "code" {
                    if a.get("preserve_outputs").and_then(Value::as_bool) == Some(true) {
                        if a.get("outputs_are_stale").and_then(Value::as_bool) != Some(true) {
                            return Err("preserve_outputs requires outputs_are_stale:true and explicit operator request".into());
                        }
                        preserved_stale = true;
                    } else {
                        list[i]["outputs"] = json!([]);
                        list[i]["execution_count"] = Value::Null;
                    }
                }
            }
            "delete" => {
                list.remove(i);
                origins.remove(i);
            }
            "move" => {
                let to = index(a, "to")?;
                let c = list.remove(i);
                let origin = origins.remove(i);
                list.insert(to, c);
                origins.insert(to, origin);
            }
            "insert" => {
                let kind = string(a, "cell_type")?;
                if !matches!(kind, "code" | "markdown" | "raw") {
                    return Err("invalid cell_type".into());
                }
                let mut c = json!({"cell_type":kind,"metadata":{},"source":string(a,"source")?});
                if kind == "code" {
                    c["outputs"] = json!([]);
                    c["execution_count"] = Value::Null;
                }
                if present(a, "id") {
                    c["id"] = a["id"].clone();
                } else if minor >= 5 {
                    c["id"] = json!(format!(
                        "dial-{}-{}",
                        std::process::id(),
                        NEXT.fetch_add(1, Ordering::Relaxed)
                    ));
                }
                list.insert(i, c);
                origins.insert(i, None);
            }
            _ => unreachable!(),
        }
        validate(&updated)?;
        if cancel.load(Ordering::Relaxed) {
            return Err("notebook operation cancelled".into());
        }
        let bytes = crate::tools::edit::edit_source::encode_notebook_with_origins(
            &updated,
            &loaded.raw,
            &origins,
        )?
        .into_bytes();
        if bytes.len() > MAX {
            return Err("updated notebook exceeds 8 MiB".into());
        }
        let stage_name = PathBuf::from(format!(
            ".dial-notebook-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let result = (|| -> Result<(), String> {
            #[cfg(windows)]
            let mut staged =
                crate::platform::fs::create_new_beneath(&loaded.dir, &stage_name, true)
                    .map_err(|e| format!("staging notebook: {e}"))?;
            #[cfg(unix)]
            let mut staged = {
                let n = std::ffi::CString::new(stage_name.as_os_str().as_bytes())
                    .map_err(|e| e.to_string())?;
                let fd = unsafe {
                    libc::openat(
                        loaded.dir.as_raw_fd(),
                        n.as_ptr(),
                        libc::O_WRONLY
                            | libc::O_CREAT
                            | libc::O_EXCL
                            | libc::O_NOFOLLOW
                            | libc::O_CLOEXEC,
                        0o600,
                    )
                };
                if fd < 0 {
                    return Err(format!(
                        "staging notebook: {}",
                        std::io::Error::last_os_error()
                    ));
                }
                unsafe { File::from_raw_fd(fd) }
            };
            staged.write_all(&bytes).map_err(|e| e.to_string())?;
            staged
                .set_permissions(
                    loaded
                        .file
                        .metadata()
                        .map_err(|e| e.to_string())?
                        .permissions(),
                )
                .map_err(|e| e.to_string())?;
            staged.sync_all().map_err(|e| e.to_string())?;
            let fresh = load(ctx.workspace, string(a, "path")?)?;
            if fresh.identity != loaded.identity
                || fresh.revision != loaded.revision
                || crate::platform::fs::identity(&fresh.dir).map_err(|e| e.to_string())?
                    != crate::platform::fs::identity(&loaded.dir).map_err(|e| e.to_string())?
            {
                return Err("notebook identity/revision changed during staging".into());
            }
            ctx.workspace.observe_write(&loaded.path)?;
            let fresh = load(ctx.workspace, string(a, "path")?)?;
            if fresh.identity != loaded.identity
                || fresh.revision != loaded.revision
                || crate::platform::fs::identity(&fresh.dir).map_err(|e| e.to_string())?
                    != crate::platform::fs::identity(&loaded.dir).map_err(|e| e.to_string())?
            {
                return Err("notebook changed before commit".into());
            }
            if cancel.load(Ordering::Relaxed) {
                return Err("notebook operation cancelled before commit".into());
            }
            #[cfg(windows)]
            {
                let backup = PathBuf::from(format!(
                    ".dial-notebook-backup-{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ));
                crate::platform::fs::rename_beneath(&loaded.dir, &loaded.name, &backup)
                    .map_err(|e| e.to_string())?;
                let captured = crate::platform::fs::open_beneath_handle(&loaded.dir, &backup)
                    .map_err(|e| e.to_string())?;
                if crate::platform::fs::identity(&captured).map_err(|e| e.to_string())?
                    != loaded.identity
                {
                    if let Err(restore) =
                        crate::platform::fs::rename_beneath(&loaded.dir, &backup, &loaded.name)
                    {
                        return Err(format!(
                            "notebook changed during publication; captured original retained as {}: {restore}",
                            backup.display()
                        ));
                    }
                    return Err("notebook changed during publication".into());
                }
                if let Err(error) =
                    crate::platform::fs::rename_beneath(&loaded.dir, &stage_name, &loaded.name)
                {
                    if let Err(restore) =
                        crate::platform::fs::rename_beneath(&loaded.dir, &backup, &loaded.name)
                    {
                        return Err(format!(
                            "publishing notebook failed: {error}; restoring original failed: {restore}; original retained as {}",
                            backup.display()
                        ));
                    }
                    return Err(error.to_string());
                }
                crate::platform::fs::remove_beneath(&loaded.dir, &backup)
                    .map_err(|e| format!("notebook committed but backup cleanup failed: {e}"))?;
            }
            #[cfg(unix)]
            {
                let stage = std::ffi::CString::new(stage_name.as_os_str().as_bytes())
                    .map_err(|e| e.to_string())?;
                let target = std::ffi::CString::new(loaded.name.as_os_str().as_bytes())
                    .map_err(|e| e.to_string())?;
                if unsafe {
                    libc::renameat(
                        loaded.dir.as_raw_fd(),
                        stage.as_ptr(),
                        loaded.dir.as_raw_fd(),
                        target.as_ptr(),
                    )
                } < 0
                {
                    return Err(std::io::Error::last_os_error().to_string());
                }
            }
            Ok(())
        })();
        #[cfg(windows)]
        if result.is_err() {
            let _ = crate::platform::fs::remove_beneath(&loaded.dir, &stage_name);
        }
        #[cfg(unix)]
        if result.is_err()
            && let Ok(n) = std::ffi::CString::new(stage_name.as_os_str().as_bytes())
        {
            unsafe { libc::unlinkat(loaded.dir.as_raw_fd(), n.as_ptr(), 0) };
        }
        result?;
        ctx.workspace.store().forget(&loaded.path);
        seen.retain(|(p, _, _), _| p != &loaded.path);
        loaded.dir.sync_all().map_err(|error| format!("notebook changes were committed, but directory durability sync failed: {error}; inspect the current notebook before retrying"))?;
        loaded.value = updated;
        Ok(
            json!({"path":ctx.workspace.rel(&loaded.path),"action":action,"changed":true,"content_hash":hash(&bytes),"cell_count":loaded.value["cells"].as_array().unwrap().len(),"outputs_preserved_stale":preserved_stale,"reread_required":true}),
        )
    }
}
impl Tool for Notebook {
    fn name(&self) -> &str {
        "notebook"
    }
    fn schema(&self) -> Value {
        crate::tools::tool_schema(
            "notebook",
            "Read/edit/insert/delete/move nbformat4 cells without execution. Opt-in. All indices are zero-based. Read without index lists cell identities, source hashes and whole-file revision, including empty notebooks; offset/limit page cell rows (max 256). Read with index pages Unicode source characters (max 4096). Mutations require approval, exact revision and owner-observed cell witnesses. Replacement/deletion/move and insertion neighbors require full source coverage or exact old_source matching an observed source_hash. Only 512 witnesses and 64 disjoint page intervals per cell are retained; reread after eviction. Code edits clear outputs/count by default; preserve_outputs requires explicit operator request and outputs_are_stale:true. Unknown metadata and lexical numbers remain intact. Reread after every actual mutation.",
            json!({"action":{"type":"string","enum":["read","edit","insert","delete","move"]},"path":{"type":"string"},"revision":{"type":"string"},"index":{"type":"integer","minimum":0},"offset":{"type":"integer","minimum":0},"limit":{"type":"integer","minimum":1,"maximum":4096},"source":{"type":"string"},"old_source":{"type":"string"},"cell_type":{"type":"string","enum":["code","markdown","raw"]},"id":{"type":"string"},"to":{"type":"integer","minimum":0},"preserve_outputs":{"type":"boolean"},"outputs_are_stale":{"type":"boolean"}}),
            &["action", "path"],
        )
    }
    fn needs_approval(&self, a: &Value) -> bool {
        a.get("action").and_then(Value::as_str) != Some("read")
    }
    fn may_modify_workspace(&self, _: &Value) -> bool {
        false
    }
    fn manages_workspace_serialization(&self) -> bool {
        true
    }
    fn call(&self, c: &ToolContext, a: &Value) -> ToolOutput {
        self.call_streaming(c, a, &|_| {}, &AtomicBool::new(false))
    }
    fn call_streaming(
        &self,
        c: &ToolContext,
        a: &Value,
        _: &(dyn Fn(crate::tools::integrations::ext::ToolEvent) + Sync),
        cancel: &AtomicBool,
    ) -> ToolOutput {
        match self.run(c, a, cancel) {
            Ok(v) => ToolOutput::structured(v),
            Err(e) => ToolOutput::err(e),
        }
    }
    fn fork_session(&self) -> Option<Arc<dyn Tool>> {
        Some(Arc::new(Self::default()))
    }
}
pub fn tools(config: &Config) -> Vec<Arc<dyn Tool>> {
    if config.integrations.iter().any(|n| n == "notebook") {
        vec![Arc::new(Notebook::default())]
    } else {
        Vec::new()
    }
}
