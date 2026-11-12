//! NuGet (.NET): the version restored for the project (`packages.lock.json`, else
//! `obj/project.assets.json`), read from the NuGet global packages folder.
use super::*;

const PROJECT_EXTENSIONS: &[&str] = &["csproj", "fsproj", "vbproj", "sln", "slnx"];

/// Whether `root` holds a .NET project or solution file.
pub(super) fn has_project(root: &Path) -> bool {
    fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .any(|entry| {
            entry
                .path()
                .extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| PROJECT_EXTENSIONS.contains(&extension))
        })
}

pub(super) fn resolve(
    root: &Path,
    name: &str,
    wanted: Option<&str>,
    caches: &Caches,
) -> Result<Package, String> {
    if name.contains('/') || name.starts_with('@') {
        return Err(format!("{name:?} is not a NuGet package id"));
    }
    let mut versions = restored(root, name);
    versions.sort_by(|a, b| version_cmp(&b.0, &a.0));
    versions.dedup_by(|a, b| a.0.eq_ignore_ascii_case(&b.0));
    if versions.is_empty() {
        return Err(format!(
            "{name} is not in packages.lock.json or obj/project.assets.json (`dotnet restore` writes them; nothing is fetched here)"
        ));
    }
    let index = match wanted {
        Some(wanted) => versions
            .iter()
            .position(|(version, _)| version.eq_ignore_ascii_case(wanted))
            .ok_or_else(|| {
                let known: Vec<&str> = versions.iter().map(|(v, _)| v.as_str()).collect();
                format!(
                    "{name}@{wanted} is not restored; restored versions: {}",
                    known.join(", ")
                )
            })?,
        None => 0,
    };
    let (version, how) = versions.remove(index);
    let base = caches
        .nuget_packages
        .clone()
        .ok_or("NUGET_PACKAGES and HOME are unset")?;
    // The global packages folder uses lowercase ids and versions.
    let dir = base
        .join(name.to_ascii_lowercase())
        .join(version.to_ascii_lowercase());
    if !dir.is_dir() {
        return Err(format!(
            "{name} {version} is restored but not in the NuGet packages folder {} (`dotnet restore` downloads it; nothing is fetched here)",
            base.display()
        ));
    }
    Ok(Package {
        ecosystem: Ecosystem::NuGet,
        name: name.to_owned(),
        version,
        root: dir,
        base,
        how,
        others: versions.into_iter().map(|(version, _)| version).collect(),
    })
}

/// `(version, source file)` for every restore record of `name` in the root project and the
/// projects one directory below it.
fn restored(root: &Path, name: &str) -> Vec<(String, String)> {
    let mut projects = vec![root.to_path_buf()];
    projects.extend(subdirs(root));
    let mut found = Vec::new();
    for project in &projects {
        let shown = |file: &str| match project.strip_prefix(root) {
            Ok(rest) if !rest.as_os_str().is_empty() => format!("{}/{file}", rest.display()),
            _ => file.to_owned(),
        };
        if let Some(lock) = json(&project.join("packages.lock.json")) {
            // {"dependencies": {"net8.0": {"Id": {"resolved": "1.2.3"}}}}
            for framework in lock["dependencies"].as_object().into_iter().flatten() {
                for (id, entry) in framework.1.as_object().into_iter().flatten() {
                    if id.eq_ignore_ascii_case(name)
                        && let Some(version) = entry["resolved"].as_str()
                    {
                        found.push((version.to_owned(), shown("packages.lock.json")));
                    }
                }
            }
        }
        if let Some(assets) = json(&project.join("obj/project.assets.json")) {
            // {"libraries": {"Id/1.2.3": {"type": "package"}}}
            for (key, entry) in assets["libraries"].as_object().into_iter().flatten() {
                if entry["type"] == "package"
                    && let Some((id, version)) = key.split_once('/')
                    && id.eq_ignore_ascii_case(name)
                {
                    found.push((version.to_owned(), shown("obj/project.assets.json")));
                }
            }
        }
    }
    found
}

/// A restore record, bounded: these files are generated and normally a few MiB at most.
fn json(path: &Path) -> Option<Value> {
    let metadata = fs::metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > 64 * 1024 * 1024 {
        return None;
    }
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}
