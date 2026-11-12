//! Ruby gems: the version locked in `Gemfile.lock` (or `gems.locked`), read from where Bundler or
//! RubyGems installed it: the project's bundle path (`vendor/bundle` or `.bundle/config`
//! `BUNDLE_PATH`) or a gem directory (`GEM_HOME`, `GEM_PATH`, user and rbenv installs).
use super::*;

/// One locked spec: version (with any platform suffix) and the lock section it came from.
#[derive(Debug, PartialEq)]
struct Locked {
    version: String,
    section: &'static str,
}

pub(super) fn resolve(
    root: &Path,
    name: &str,
    wanted: Option<&str>,
    caches: &Caches,
) -> Result<Package, String> {
    let (lockfile, text) = ["Gemfile.lock", "gems.locked"]
        .into_iter()
        .find_map(|file| {
            fs::read_to_string(root.join(file))
                .ok()
                .map(|text| (file, text))
        })
        .ok_or("no Gemfile.lock in the workspace root (`bundle lock` creates one; nothing is fetched here)")?;
    let mut locked = locked(&text, name);
    locked.sort_by(|a, b| version_cmp(&b.version, &a.version));
    if locked.is_empty() {
        return Err(format!("{name} is not in {lockfile}"));
    }
    if let Some(wanted) = wanted {
        // `1.15.0` also selects a platform build such as `1.15.0-arm64-darwin`.
        locked.retain(|spec| {
            spec.version == wanted || spec.version.starts_with(&format!("{wanted}-"))
        });
        if locked.is_empty() {
            return Err(format!("{name}@{wanted} is not locked in {lockfile}"));
        }
    }
    if locked.iter().all(|spec| spec.section == "PATH") {
        return Err(format!(
            "{name} is a path gem in {lockfile}; read it with `read`"
        ));
    }
    let homes = gem_homes(root, caches);
    let versions: Vec<String> = locked.iter().map(|spec| spec.version.clone()).collect();
    for spec in &locked {
        let leaf = format!("{name}-{}", spec.version);
        let found = homes.iter().find_map(|home| match spec.section {
            "GIT" => git_gem(&home.join("bundler/gems"), name)
                .map(|dir| (home.join("bundler/gems"), dir)),
            _ => {
                let dir = home.join("gems").join(&leaf);
                dir.is_dir().then(|| (home.join("gems"), dir))
            }
        });
        if let Some((base, dir)) = found {
            return Ok(Package {
                ecosystem: Ecosystem::Ruby,
                name: name.to_owned(),
                version: spec.version.clone(),
                root: dir,
                base,
                how: format!("{lockfile} {}", spec.section),
                others: versions
                    .iter()
                    .filter(|version| **version != spec.version)
                    .cloned()
                    .collect(),
            });
        }
    }
    Err(format!(
        "{name} {} is locked but not installed in the bundle path or any gem directory (`bundle install` installs it; nothing is fetched here)",
        versions.join(", ")
    ))
}

/// Top-level specs named `name` in the GEM, GIT and PATH sections: `    name (version)` lines
/// (four spaces; six-space lines are their dependencies).
fn locked(text: &str, name: &str) -> Vec<Locked> {
    let mut section = "";
    let mut found = Vec::new();
    for line in text.lines() {
        if !line.starts_with(' ') {
            section = match line.trim() {
                "GEM" => "GEM",
                "GIT" => "GIT",
                "PATH" => "PATH",
                _ => "",
            };
            continue;
        }
        let Some(spec) = line
            .strip_prefix("    ")
            .filter(|rest| !rest.starts_with(' '))
        else {
            continue;
        };
        if section.is_empty() {
            continue;
        }
        if let Some((gem, version)) = spec.split_once(" (")
            && gem == name
            && let Some(version) = version.strip_suffix(')')
        {
            found.push(Locked {
                version: version.to_owned(),
                section,
            });
        }
    }
    found
}

/// Directories that hold `gems/` (and `bundler/gems/`): the project's bundle path first.
fn gem_homes(root: &Path, caches: &Caches) -> Vec<PathBuf> {
    let mut bundles = vec![root.join("vendor/bundle")];
    if let Ok(config) = fs::read_to_string(root.join(".bundle/config"))
        && let Some(path) = config.lines().find_map(|line| {
            line.trim()
                .strip_prefix("BUNDLE_PATH:")
                .map(|value| value.trim().trim_matches(['"', '\'']).to_owned())
        })
        && !path.is_empty()
    {
        bundles.insert(0, root.join(path));
    }
    // A bundle path holds `ruby/<abi version>/gems`.
    let mut homes: Vec<PathBuf> = bundles
        .iter()
        .flat_map(|bundle| subdirs(&bundle.join("ruby")))
        .collect();
    homes.extend(caches.gem_paths.iter().cloned());
    homes
}

/// A git gem's checkout: `bundler/gems/<name>-<revision>` holding `<name>.gemspec`.
fn git_gem(dir: &Path, name: &str) -> Option<PathBuf> {
    let prefix = format!("{name}-");
    subdirs(dir).into_iter().find(|checkout| {
        checkout
            .file_name()
            .and_then(|leaf| leaf.to_str())
            .is_some_and(|leaf| leaf.starts_with(&prefix))
            && checkout.join(format!("{name}.gemspec")).is_file()
    })
}
