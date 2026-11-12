//! Composer (PHP): the version locked in `composer.lock`, read from the project's `vendor/`.
use super::*;

pub(super) fn resolve(root: &Path, name: &str, wanted: Option<&str>) -> Result<Package, String> {
    if name.starts_with('@') || name.split('/').count() != 2 {
        return Err(format!(
            "Composer packages are named vendor/package, not {name:?}"
        ));
    }
    let text = fs::read_to_string(root.join("composer.lock")).map_err(|_| {
        "no composer.lock in the workspace root (`composer install` creates it and vendor/; nothing is fetched here)"
            .to_owned()
    })?;
    let lock: Value = serde_json::from_str(&text)
        .map_err(|error| format!("composer.lock does not parse: {error}"))?;
    let entry = ["packages", "packages-dev"]
        .iter()
        .flat_map(|key| lock[key].as_array().into_iter().flatten())
        .find(|entry| {
            entry["name"]
                .as_str()
                .is_some_and(|locked| locked.eq_ignore_ascii_case(name))
        })
        .ok_or_else(|| format!("{name} is not in composer.lock"))?;
    // Composer names are lowercase; the lock file holds the canonical spelling.
    let name = entry["name"].as_str().unwrap_or(name).to_owned();
    let version = entry["version"].as_str().unwrap_or("unknown").to_owned();
    if let Some(wanted) = wanted
        && !same_version(wanted, &version)
    {
        return Err(format!(
            "{name}@{wanted} is not locked; composer.lock has {version}"
        ));
    }
    let base = root.join("vendor");
    let dir = base.join(&name);
    if !dir.is_dir() {
        return Err(format!(
            "{name} {version} is locked but not installed in vendor/ (`composer install` installs it; nothing is fetched here)"
        ));
    }
    Ok(Package {
        ecosystem: Ecosystem::Composer,
        name,
        version,
        root: dir,
        base,
        how: "composer.lock, vendor/".into(),
        others: Vec::new(),
    })
}

/// Composer versions may carry a leading `v` (`v6.4.1`); both spellings name the same release.
fn same_version(a: &str, b: &str) -> bool {
    a.trim_start_matches('v') == b.trim_start_matches('v')
}
