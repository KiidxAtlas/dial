//! Trusted workspace resource declarations and conservative dependency conflicts.
//! A declaration enables scheduling, never permission. Native mutation callbacks must still
//! validate their computed targets against the resolved writes before committing.
use crate::config::types::EditMode;
use crate::tools::{Workspace, integrations::ext::Registry};
use serde_json::Value;
use std::path::{Component, Path, PathBuf};

/// Tool-owned lexical paths. Directory reads include membership and all descendants.
/// Writers declare individual files; unbounded, external or callback effects stay exclusive.
#[derive(Clone, Debug, Default)]
pub struct Access {
    pub reads: Vec<PathBuf>,
    pub writes: Vec<PathBuf>,
}

/// Resolved resources admitted through the workspace's ordinary confinement boundary.
#[derive(Clone, Debug, Default)]
pub struct Footprint {
    reads: Vec<PathBuf>,
    writes: Vec<PathBuf>,
}
/// Scheduling comparisons may overestimate overlap; authorization (`permits`) must not.
fn overlap(a: &Path, b: &Path) -> bool {
    #[cfg(any(windows, target_os = "macos"))]
    {
        // An absent leaf has no canonical disk spelling yet. Even in a case-sensitive
        // directory, treating two case variants as conflicting only costs concurrency.
        a.components().zip(b.components()).all(|(a, b)| {
            let (a, b) = (a.as_os_str(), b.as_os_str());
            // APFS/HFS can fold or normalize non-ASCII names. Do not try to reproduce
            // the volume's Unicode tables; an ambiguous component cannot prove disjointness.
            a.eq_ignore_ascii_case(b) || !a.is_ascii() || !b.is_ascii()
        })
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        a.starts_with(b) || b.starts_with(a)
    }
}
impl Footprint {
    pub fn conflicts(&self, other: &Self) -> bool {
        self.writes.iter().any(|a| {
            other
                .reads
                .iter()
                .chain(&other.writes)
                .any(|b| overlap(a, b))
        }) || other
            .writes
            .iter()
            .any(|a| self.reads.iter().any(|b| overlap(a, b)))
    }

    /// Only source-read callbacks can use the read set. Effects require declared write files.
    pub fn permits(&self, paths: &[PathBuf], unbounded: bool, source_read: bool) -> bool {
        !unbounded
            && !paths.is_empty()
            && paths.iter().all(|path| {
                self.writes.contains(path)
                    || (source_read && self.reads.iter().any(|root| path.starts_with(root)))
            })
    }
    pub fn writes(&self) -> bool {
        !self.writes.is_empty()
    }
    pub fn write_paths(&self) -> &[PathBuf] {
        &self.writes
    }
    pub fn overlaps_path(&self, path: &Path) -> bool {
        self.writes
            .iter()
            .chain(&self.reads)
            .any(|candidate| overlap(candidate, path))
    }
    /// Whether a recorded change to any of `paths` (absolute, or relative to `root`) may have
    /// reached these resources: the same lexical containment as [`conflicts`](Self::conflicts).
    pub fn touches(&self, root: &Path, paths: &[PathBuf]) -> bool {
        if self.is_empty() {
            return false;
        }
        paths.iter().any(|path| {
            let path = if path.is_absolute() {
                path.to_owned()
            } else {
                root.join(path)
            };
            let path: PathBuf = path
                .components()
                .filter(|part| !matches!(part, Component::CurDir))
                .collect();
            // An unadmitted change may be an alternate name (including a now-removed
            // junction or short name). It cannot prove the recorded read is fresh.
            #[cfg(windows)]
            if !windows::unambiguous(root, &path, false) {
                return true;
            }
            self.reads
                .iter()
                .chain(&self.writes)
                .any(|own| overlap(&path, own))
        })
    }
    /// No workspace resources: bookkeeping or an effect-free external read.
    pub fn is_empty(&self) -> bool {
        self.reads.is_empty() && self.writes.is_empty()
    }
}

/// Version-control and Dial project state, plus ignore rules. Ordinary dotfile directories
/// (`.github/`, `.cargo/`) are source; these hold metadata whose writers (VCS, the harness) are not
/// native tools, or change what searches outside their own directory scope can see.
const METADATA_PATHS: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    ".jj",
    ".dial",
    ".gitignore",
    ".ignore",
    ".rgignore",
    ".fdignore",
    ".gitattributes",
    ".gitmodules",
];

/// `@f7`-style compact file handles are expanded by the controller, never resolved literally.
fn file_handle(segment: &str) -> bool {
    segment.strip_prefix("@f").is_some_and(|rest| {
        rest.split(['#', '('])
            .next()
            .is_some_and(|digits| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
    })
}

/// Lexical admission *after* the read owner has resolved supported selectors. Unresolved
/// selectors, URI- and host-shaped paths, traversal and metadata stay serial barriers.
fn admissible(literal: &str, relative: &Path) -> bool {
    if literal.contains([':', '?']) || literal.contains("//") {
        return false;
    }
    let mut leading = true;
    relative.components().all(|part| match part {
        Component::ParentDir => false,
        Component::Normal(name) => {
            // A SQLite read (including a file identified by magic) may implicitly open its
            // siblings, and a sidecar reader inspects the database. Neither spelling is an
            // independent resource when the sidecar itself is an explicit tool target.
            let name = name.to_string_lossy();
            // `user@host` names a remote in the leading position; scoped package directories
            // (`node_modules/@scope`) and asset names (`icon@2x.png`) below it are paths.
            let host = leading && name.find('@').is_some_and(|at| at > 0);
            leading = false;
            !METADATA_PATHS
                .iter()
                .any(|entry| name.eq_ignore_ascii_case(entry))
                && !file_handle(&name)
                && !crate::tools::fs::binary::possible_sqlite_sidecar(&name)
                && !host
        }
        _ => true,
    })
}

fn resource(workspace: &Workspace, path: &Path, writing: bool) -> Option<PathBuf> {
    if path.is_absolute() && workspace.root_containing(path).is_none() {
        return None;
    }
    let literal = path.to_str()?;
    let relative = path.strip_prefix(workspace.root()).unwrap_or(path);
    // The trusted Windows root is canonical (usually `\\?\C:\...`); only its
    // workspace-relative suffix is user path syntax. Other drive/UNC aliases stay unknown.
    let syntax = if cfg!(windows) {
        relative.to_str()?
    } else {
        literal
    };
    if !admissible(syntax, relative) {
        return None;
    }
    let lexical = if path.is_absolute() {
        path.to_owned()
    } else {
        workspace.root().join(path)
    };
    let resolved = if writing {
        workspace.resolve(literal)
    } else {
        workspace.resolve_read(literal)
    }
    .ok()?;
    // A leading '.' is ordinary search scope syntax, not a path alias.
    let normalized: PathBuf = lexical
        .components()
        .filter(|part| !matches!(part, Component::CurDir))
        .collect();
    if normalized != resolved || !resolved.parent()?.is_dir() {
        return None;
    }
    #[cfg(windows)]
    if !windows::unambiguous(workspace.root(), &resolved, writing) {
        return None;
    }
    #[cfg(not(windows))]
    match std::fs::symlink_metadata(&resolved) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink()
                || (!metadata.is_file() && (writing || !metadata.is_dir()))
            {
                return None;
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                if metadata.is_file() && metadata.nlink() != 1 {
                    return None;
                }
            }
            #[cfg(not(unix))]
            if metadata.is_file() {
                return None;
            }
        }
        Err(error) if writing && error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return None,
    }
    Some(resolved)
}

/// Only projections whose reader has a known local base get a declaration. Do not strip
/// arbitrary suffixes (document members, query strings, remote URIs or a literal `a:1`).
fn read_base(workspace: &Workspace, input: &str) -> Option<PathBuf> {
    use crate::tools::fs::{binary, outline, select};
    let path = Path::new(input);
    let syntax = path
        .strip_prefix(workspace.root())
        .unwrap_or(path)
        .to_str()?;
    if input.is_empty() || syntax.contains("//") || syntax.contains('?') {
        return None;
    }
    if !syntax.contains(':') {
        return Some(path.into());
    }
    // The literal-precedence probes in the selector owners touch the filesystem. Never
    // probe an absolute target outside the declared roots (notably a Windows UNC share).
    if path.is_absolute() && workspace.root_containing(path).is_none() {
        return None;
    }
    let base = if let Some(request) = outline::target(workspace, input) {
        request.path
    } else if let Some((base, spec)) = binary::split_bytes_selector(workspace, input) {
        binary::parse_bytes_spec(spec).ok()?;
        base
    } else {
        select::split_target(workspace, input).ok()?.0
    };
    (!base.is_empty()).then(|| base.into())
}

/// The effect-free external read class: a tool that shares its batch slot and cannot touch the
/// workspace, and (checked by [`resolve`]) declares an empty workspace footprint. It may still need
/// approval (network egress); the class enables concurrent scheduling of authorized calls, never
/// permission.
pub fn external_read(tool: &dyn crate::tools::integrations::ext::Tool, args: &Value) -> bool {
    tool.concurrency(args) == crate::tools::integrations::ext::Concurrency::Shared
        && !tool.may_modify_workspace(args)
        && !tool.supports_tool_dispatch()
        && !tool.manages_workspace_serialization()
}

/// Every root the workspace may write, as directory reads: the conservative footprint of a read
/// whose exact inputs are unknown but which cannot write (a read-only shell command, a working-tree
/// VCS read, a dependency lookup). It conflicts with every native write and with no other read.
pub fn workspace_reads(workspace: &Workspace) -> Access {
    Access {
        reads: workspace.roots().map(Path::to_path_buf).collect(),
        writes: Vec::new(),
    }
}

/// [`workspace_reads`] resolved through the ordinary confinement boundary.
pub fn workspace_read(workspace: &Workspace) -> Option<Footprint> {
    footprint(workspace, &workspace_reads(workspace))
}

/// Reads of exactly these literal workspace paths, admitted through the same rules as `read`.
/// `None` when any path is outside that admission, so the caller keeps its broader read.
pub fn literal_reads(workspace: &Workspace, paths: &[PathBuf]) -> Option<Footprint> {
    footprint(
        workspace,
        &Access {
            reads: paths.to_vec(),
            writes: Vec::new(),
        },
    )
}

/// Resolve a native or explicitly declared extension call. Unknown or ambiguous resources are
/// absent, so callers keep the operation behind a serial barrier.
pub fn resolve(
    workspace: &Workspace,
    mode: EditMode,
    registry: &Registry,
    name: &str,
    args: &Value,
) -> Option<Footprint> {
    let access = match name {
        "read" => Access {
            reads: match args.get("paths").filter(|v| !v.is_null()) {
                Some(paths) if args["path"].is_null() && args.get("format").is_none() => {
                    let paths = paths.as_array()?;
                    if paths.len() > crate::tools::fs::files::MAX_READ_PATHS {
                        return None;
                    }
                    paths
                        .iter()
                        .map(|v| read_base(workspace, v.as_str()?))
                        .collect::<Option<_>>()?
                }
                Some(_) => return None,
                // Structured data mode takes a literal path, not the source reader's selectors.
                None if args.get("format").is_some() => vec![args["path"].as_str()?.into()],
                None => vec![read_base(workspace, args["path"].as_str()?)?],
            },
            writes: Vec::new(),
        },
        "write" => Access {
            reads: Vec::new(),
            writes: crate::tools::declared_write_targets(args)
                .ok()?
                .into_iter()
                .map(PathBuf::from)
                .collect(),
        },
        "edit" => Access {
            reads: Vec::new(),
            writes: crate::tools::editing_targets(mode, args)
                .ok()?
                .into_iter()
                .map(PathBuf::from)
                .collect(),
        },
        "grep" | "glob" => Access {
            reads: crate::tools::fs::search::scheduling_scopes(workspace, name, args).ok()?,
            writes: Vec::new(),
        },
        _ => {
            let tool = registry.get(name)?;
            if tool.supports_tool_dispatch() {
                return None;
            }
            let resources = tool.resources(workspace, args)?;
            if !resources.writes.is_empty() && !tool.manages_workspace_serialization() {
                return None;
            }
            if resources.reads.is_empty() && resources.writes.is_empty() {
                // An effect-free external read (`web_fetch`): explicitly no workspace resources.
                // Approval is still decided per call by the controller before it starts.
                return external_read(tool.as_ref(), args).then(Footprint::default);
            }
            resources
        }
    };
    footprint(workspace, &access)
}

fn footprint(workspace: &Workspace, access: &Access) -> Option<Footprint> {
    if access.reads.len() + access.writes.len() == 0
        || access.reads.len() + access.writes.len() > 128
    {
        return None;
    }
    Some(Footprint {
        reads: access
            .reads
            .iter()
            .map(|p| resource(workspace, p, false))
            .collect::<Option<_>>()?,
        writes: access
            .writes
            .iter()
            .map(|p| resource(workspace, p, true))
            .collect::<Option<_>>()?,
    })
}

#[cfg(windows)]
mod windows;

#[cfg(test)]
mod tests;
