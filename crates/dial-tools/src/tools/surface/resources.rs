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
impl Footprint {
    pub fn conflicts(&self, other: &Self) -> bool {
        let overlap = |a: &PathBuf, b: &PathBuf| a.starts_with(b) || b.starts_with(a);
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
}

fn resource(workspace: &Workspace, path: &Path, writing: bool) -> Option<PathBuf> {
    let literal = path.to_str()?;
    let relative = path.strip_prefix(workspace.root()).unwrap_or(path);
    if literal.contains([':', '?', '@']) || relative.components().any(|part| {
        matches!(part, Component::ParentDir)
            || matches!(part, Component::Normal(name) if name.to_string_lossy().starts_with('.'))
    }) {
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
    match std::fs::symlink_metadata(&resolved) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink()
                || (!metadata.is_file() && (writing || !metadata.is_dir()))
            {
                return None;
            }
            if metadata.is_file() {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::MetadataExt;
                    if metadata.nlink() != 1 {
                        return None;
                    }
                }
                #[cfg(not(unix))]
                {
                    return None;
                }
            }
        }
        Err(error) if writing && error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return None,
    }
    Some(resolved)
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
                Some(paths) => paths
                    .as_array()?
                    .iter()
                    .map(|v| v.as_str().map(PathBuf::from))
                    .collect::<Option<Vec<_>>>()?,
                None => vec![PathBuf::from(args["path"].as_str()?)],
            },
            writes: Vec::new(),
        },
        "write" => Access {
            reads: Vec::new(),
            writes: vec![PathBuf::from(args["path"].as_str()?)],
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
            reads: crate::tools::fs::search::scheduling_scopes(workspace, args).ok()?,
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
            resources
        }
    };
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

#[cfg(test)]
mod tests;
