//! JVM (Maven and Gradle) dependencies, named `group:artifact`. The version comes from
//! `gradle.lockfile` or the root `pom.xml` (literal versions and `${property}` references), and
//! the source from the artifact's `-sources.jar` in the local Maven repository or the Gradle
//! module cache. The jar is read in place: never extracted, built or run.
use super::*;
use std::collections::{BTreeSet, HashMap};
use std::io::Read as _;

/// The Gradle module cache, relative to the Gradle user home.
pub(super) const GRADLE_FILES: &str = "caches/modules-2/files-2.1";
/// Members of a sources jar; larger archives are refused rather than partly listed.
const MAX_ENTRIES: usize = 50_000;

/// One version of the artifact found in a local cache.
struct Installed {
    version: String,
    sources: Option<PathBuf>,
    base: PathBuf,
    cache: &'static str,
}

pub(super) fn resolve(
    root: &Path,
    name: &str,
    wanted: Option<&str>,
    caches: &Caches,
) -> Result<Package, String> {
    let (group, artifact) = name.split_once(':').ok_or_else(|| {
        format!(
            "JVM dependencies are named group:artifact (for example com.google.guava:guava), not {name:?}"
        )
    })?;
    let mut declared = declared(root, group, artifact);
    declared.sort_by(|a, b| version_cmp(&b.0, &a.0));
    let installed = installed(caches, group, artifact);
    let version = match (wanted, declared.first()) {
        (Some(wanted), _) => wanted.to_owned(),
        (None, Some((version, _))) => version.clone(),
        (None, None) => {
            let with_sources: BTreeSet<&str> = installed
                .iter()
                .filter(|found| found.sources.is_some())
                .map(|found| found.version.as_str())
                .collect();
            match with_sources.len() {
                1 => with_sources
                    .into_iter()
                    .next()
                    .unwrap_or_default()
                    .to_owned(),
                0 => {
                    return Err(format!(
                        "{name} is not declared in gradle.lockfile or pom.xml and has no -sources.jar in the local Maven repository or Gradle cache"
                    ));
                }
                _ => {
                    let versions: Vec<&str> = with_sources.into_iter().collect();
                    return Err(format!(
                        "{name} is not declared with a literal version; pick one with {name}@<version>: {}",
                        versions.join(", ")
                    ));
                }
            }
        }
    };
    let Some(found) = installed
        .iter()
        .find(|found| found.version == version && found.sources.is_some())
    else {
        return Err(if installed.iter().any(|found| found.version == version) {
            format!(
                "{name} {version} is in the local caches without a -sources.jar (`mvn dependency:sources` or the IDE's \"download sources\" fetches it; nothing is fetched here)"
            )
        } else {
            format!(
                "{name} {version} is not in the local Maven repository or Gradle cache (a build downloads it; nothing is fetched here)"
            )
        });
    };
    let declared_by = declared
        .iter()
        .find(|(declared, _)| *declared == version)
        .map_or("requested version", |(_, how)| how.as_str());
    let mut others: Vec<String> = declared
        .iter()
        .map(|(version, _)| version.clone())
        .chain(installed.iter().map(|found| found.version.clone()))
        .filter(|other| *other != version)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    others.sort_by(|a, b| version_cmp(b, a));
    Ok(Package {
        ecosystem: Ecosystem::Jvm,
        name: name.to_owned(),
        version,
        root: found.sources.clone().unwrap_or_default(),
        base: found.base.clone(),
        how: format!("{declared_by}, {}", found.cache),
        others,
    })
}

/// `(version, source)` for literal declarations of `group:artifact`.
fn declared(root: &Path, group: &str, artifact: &str) -> Vec<(String, String)> {
    let mut found = Vec::new();
    if let Ok(lock) = fs::read_to_string(root.join("gradle.lockfile")) {
        for line in lock.lines().filter(|line| !line.starts_with('#')) {
            let coordinates = line.split_once('=').map_or(line, |(left, _)| left).trim();
            let mut parts = coordinates.split(':');
            if let (Some(g), Some(a), Some(v), None) =
                (parts.next(), parts.next(), parts.next(), parts.next())
                && g == group
                && a == artifact
            {
                found.push((v.to_owned(), "gradle.lockfile".to_owned()));
            }
        }
    }
    if let Ok(pom) = fs::read_to_string(root.join("pom.xml")) {
        found.extend(
            pom_versions(&pom, group, artifact)
                .into_iter()
                .map(|version| (version, "pom.xml".to_owned())),
        );
    }
    found
}

/// Versions the POM gives `group:artifact` in `<dependencies>` or `<dependencyManagement>`,
/// with `${property}` references resolved from `<properties>` and the project's own version.
/// Unresolvable versions (BOM imports, parent properties) are skipped.
pub(super) fn pom_versions(pom: &str, group: &str, artifact: &str) -> Vec<String> {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_str(pom);
    reader.config_mut().trim_text(true);
    let mut path: Vec<String> = Vec::new();
    let mut properties: HashMap<String, String> = HashMap::new();
    let mut dependencies: Vec<[String; 3]> = Vec::new();
    let mut current: Option<[String; 3]> = None;
    for _ in 0..1_000_000 {
        match reader.read_event() {
            Ok(Event::Start(start)) => {
                path.push(start.local_name().as_ref().to_string());
                if path.len() > 64 {
                    break;
                }
                if path.last().is_some_and(|name| name == "dependency") {
                    current = Some(Default::default());
                }
            }
            Ok(Event::End(_)) => {
                if path.last().is_some_and(|name| name == "dependency")
                    && let Some(dependency) = current.take()
                {
                    dependencies.push(dependency);
                }
                path.pop();
            }
            Ok(Event::Text(text)) => {
                let Ok(value) = quick_xml::escape::unescape(&text) else {
                    continue;
                };
                let value = value.trim().to_owned();
                let names: Vec<&str> = path.iter().map(String::as_str).collect();
                match names.as_slice() {
                    ["project", "properties", key] => {
                        properties.insert((*key).to_owned(), value);
                    }
                    ["project", "version"] => {
                        properties.insert("project.version".into(), value);
                    }
                    [.., "dependency", field] => {
                        if let Some(dependency) = current.as_mut() {
                            match *field {
                                "groupId" => dependency[0] = value,
                                "artifactId" => dependency[1] = value,
                                "version" => dependency[2] = value,
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    let resolve = |text: &str| -> Option<String> {
        match text
            .strip_prefix("${")
            .and_then(|rest| rest.strip_suffix('}'))
        {
            Some(key) => properties
                .get(key)
                .filter(|value| !value.contains("${"))
                .cloned(),
            None => Some(text.to_owned()),
        }
    };
    let mut versions: Vec<String> = dependencies
        .iter()
        .filter(|[g, a, v]| g == group && a == artifact && !v.is_empty())
        .filter_map(|[_, _, version]| resolve(version))
        // A range such as `[1.0,2.0)` is not one locked version.
        .filter(|version| !version.starts_with(['[', '(']))
        .collect();
    versions.dedup();
    versions
}

/// Every cached version of `group:artifact`, with its sources jar when one was downloaded.
fn installed(caches: &Caches, group: &str, artifact: &str) -> Vec<Installed> {
    let mut found = Vec::new();
    let sources = |dir: &Path, version: &str| {
        let jar = dir.join(format!("{artifact}-{version}-sources.jar"));
        jar.is_file().then_some(jar)
    };
    let leaf = |dir: &Path| {
        dir.file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()
    };
    if let Some(repo) = &caches.maven_repo {
        let mut dir = repo.clone();
        dir.extend(group.split('.'));
        for version_dir in subdirs(&dir.join(artifact)) {
            let version = leaf(&version_dir);
            found.push(Installed {
                sources: sources(&version_dir, &version),
                version,
                base: repo.clone(),
                cache: "local Maven repository",
            });
        }
    }
    if let Some(home) = &caches.gradle_home {
        let base = home.join(GRADLE_FILES);
        for version_dir in subdirs(&base.join(group).join(artifact)) {
            let version = leaf(&version_dir);
            // Each file sits in a directory named after its SHA-1.
            let jar = subdirs(&version_dir)
                .into_iter()
                .find_map(|hash| sources(&hash, &version));
            found.push(Installed {
                sources: jar,
                version,
                base: base.clone(),
                cache: "Gradle module cache",
            });
        }
    }
    found
}

/// Lists, searches or excerpts inside the sources jar, the way directory packages are browsed.
pub(super) fn browse(
    jar: &Path,
    path: Option<&str>,
    range: Option<LineRange>,
    query: Option<&str>,
    out: &mut String,
) -> Result<(), String> {
    let file = fs::File::open(jar).map_err(|error| format!("{}: {error}", jar.display()))?;
    let mut archive =
        zip::ZipArchive::new(file).map_err(|error| format!("invalid sources jar: {error}"))?;
    if archive.len() > MAX_ENTRIES {
        return Err(format!(
            "sources jar has more than {MAX_ENTRIES} members; refusing to read it"
        ));
    }
    let mut files: Vec<String> = Vec::new();
    for index in 0..archive.len() {
        let entry = archive
            .by_index(index)
            .map_err(|error| format!("reading sources jar member: {error}"))?;
        let name = entry
            .name()
            .map_err(|error| format!("unreadable sources jar member name: {error}"))?;
        if entry.is_file() && safe_member(&name) {
            files.push(name.to_string());
        }
    }
    files.sort();
    let prefix = match path.map(|path| path.trim_start_matches("./").trim_end_matches('/')) {
        None | Some("" | ".") => String::new(),
        Some(path) if !safe_member(path) => {
            return Err(format!(
                "path {path:?} must be relative to the package root, without `..`"
            ));
        }
        Some(path) if files.iter().any(|file| file == path) => {
            if query.is_some() {
                let text = member_text(&mut archive, path, MAX_SEARCH_FILE)?;
                let mut tally = SearchTally::default();
                tally.admit_file();
                tally.lines(path, &text, query.unwrap_or_default(), out);
                tally.finish(query.unwrap_or_default(), out);
                return Ok(());
            }
            let text = member_text(&mut archive, path, MAX_READ_FILE)?;
            return excerpt_text(path, &text, range, out);
        }
        Some(path) => format!("{path}/"),
    };
    let under: Vec<&String> = files
        .iter()
        .filter(|file| file.starts_with(&prefix))
        .collect();
    if under.is_empty() {
        return Err(format!(
            "{}: no such file or directory in the package",
            path.unwrap_or(".")
        ));
    }
    match query {
        Some(query) => {
            let mut tally = SearchTally::default();
            for name in under {
                if !tally.admit_file() {
                    break;
                }
                let Ok(text) = member_text(&mut archive, name, MAX_SEARCH_FILE) else {
                    continue;
                };
                if tally.lines(name, &text, query, out) {
                    break;
                }
            }
            tally.finish(query, out);
        }
        None => {
            let entries: BTreeSet<String> = under
                .iter()
                .filter_map(|name| {
                    let rest = &name[prefix.len()..];
                    match rest.split_once('/') {
                        Some((dir, _)) => Some(format!("{dir}/")),
                        None => Some(rest.to_owned()).filter(|rest| !rest.is_empty()),
                    }
                })
                .collect();
            let shown = if prefix.is_empty() {
                ".".to_owned()
            } else {
                prefix.trim_end_matches('/').to_owned()
            };
            let _ = writeln!(out, "{shown}: {} entries", entries.len());
            for entry in entries.iter().take(MAX_LISTING) {
                let _ = writeln!(out, "  {entry}");
            }
            if entries.len() > MAX_LISTING {
                let _ = writeln!(out, "  ... {} more", entries.len() - MAX_LISTING);
            }
        }
    }
    Ok(())
}

/// A relative member path with no `..`, absolute or drive component.
fn safe_member(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('/')
        && !name.contains('\\')
        && !name.contains(':')
        && name.split('/').all(|part| part != "..")
}

/// A member's text, read through a size bound (declared sizes are not trusted).
fn member_text(
    archive: &mut zip::ZipArchive<fs::File>,
    name: &str,
    limit: u64,
) -> Result<String, String> {
    let entry = archive
        .by_name(name)
        .map_err(|error| format!("{name}: {error}"))?;
    let mut bytes = Vec::new();
    entry
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("{name}: {error}"))?;
    if bytes.len() as u64 > limit || bytes.iter().take(8192).any(|byte| *byte == 0) {
        return Err(format!("{name} is binary or larger than {limit} bytes"));
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}
