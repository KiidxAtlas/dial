//! Read-only external-tool checks and explicitly reviewed, ownership-scoped updates.
//!
//! This deliberately does not update Dial, models, project dependencies, or arbitrary PATH tools.
//! Unknown ownership/version formats fail closed. Scheduling and approval belong to the caller.
use super::plan::{Plan, Step};
use super::{Env, Method, RECIPES, Recipe, install_dir, locate, recipe};
use anyhow::{Context, Result, bail, ensure};
use std::cmp::Ordering as Cmp;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

const QUERY_TIMEOUT: Duration = Duration::from_secs(15);
const CHECK_BUDGET: Duration = Duration::from_secs(120);
const PYPROJECT: &str = include_str!("../../../../../python/pyproject.toml");

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Current,
    Available,
    Manual,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub id: String,
    pub status: Status,
    pub detail: String,
}

#[derive(Clone, Debug, Default)]
pub struct Report {
    pub rows: Vec<Row>,
}
impl Report {
    pub fn text(&self) -> String {
        let mut lines =
            vec!["External runtime/tool updates (no installations performed):".to_owned()];
        for row in &self.rows {
            lines.push(format!("  {}: {:?} — {}", row.id, row.status, row.detail));
        }
        if self.rows.is_empty() {
            lines.push("  No installed registered tools found.".into());
        }
        lines.push("Review: /setup update <id>; install only after approval: /setup update <id> yes. Manual entries are not verified current.".into());
        lines.join("\n")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Candidate {
    row: Row,
    plan: Option<Plan>,
    // Canonical target + metadata and manifest state: never silently switch installations.
    stamp: String,
}
#[derive(Debug)]
struct Query {
    code: i32,
    stdout: String,
    stderr: String,
}
type QueryFn<'a> = dyn FnMut(&Step) -> Result<Query> + 'a;
type Reviewed = BTreeMap<(PathBuf, String), Candidate>;
static REVIEWED: OnceLock<Mutex<Reviewed>> = OnceLock::new();

fn reviewed() -> &'static Mutex<Reviewed> {
    REVIEWED.get_or_init(|| Mutex::new(BTreeMap::new()))
}

/// Check only installed recipes. Failures are per-tool and never reported as current.
/// The outer budget bounds a machine with many installed recipes; each subprocess is bounded too.
pub fn check(env: &Env, cancel: &AtomicBool) -> Result<Report> {
    let started = std::time::Instant::now();
    let mut report = Report::default();
    for recipe in RECIPES {
        ensure!(!cancel.load(Ordering::Acquire), "update check cancelled");
        let Some(path) = locate(recipe, env) else {
            continue;
        };
        let result = if started.elapsed() >= CHECK_BUDGET {
            Err(anyhow::anyhow!(
                "check budget exhausted; retry /setup updates"
            ))
        } else {
            inspect(recipe, &path, env, &mut |step| query(step, cancel))
        };
        report.rows.push(match result {
            Ok(candidate) => candidate.row,
            Err(error) => Row {
                id: recipe.id.into(),
                status: Status::Failed,
                detail: format!("{error:#}"),
            },
        });
    }
    if let Some(root) = browser_cache_root().filter(|root| root.is_dir()) {
        report.rows.push(Row { id: "browser-runtime".into(), status: Status::Manual,
            detail: format!("{} contains the immutable, manifest-keyed Playwright/Readability/Chromium runtime. Its owner pins a tested JS/Chromium combination; independently upgrading these packages is unsupported. No browser helpers were changed. Custom Chromium/CDP installations must be updated with their original manager.", root.display()) });
    }
    ensure!(!cancel.load(Ordering::Acquire), "update check cancelled");
    Ok(report)
}

// Discovery only; provisioning and manifest compatibility remain owned by browser/runtime.rs.
fn browser_cache_root() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("XDG_CACHE_HOME").map(PathBuf::from))
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
        .map(|base| base.join("dial/browser-runtime"))
}

/// Review exact commands and destination, remembering this plan only in this process.
pub fn preview(id: &str, env: &Env) -> Result<String> {
    reviewed()
        .lock()
        .map_err(|_| anyhow::anyhow!("update review lock poisoned"))?
        .remove(&(env.user_dir.clone(), id.into()));
    let recipe = recipe(id).context("unknown external tool recipe")?;
    let path = locate(recipe, env).context("tool is not installed; use /setup instead")?;
    let candidate = inspect(recipe, &path, env, &mut |step| {
        query(step, &AtomicBool::new(false))
    })?;
    let mut text = format!(
        "{}: {:?} — {}",
        id, candidate.row.status, candidate.row.detail
    );
    if let Some(plan) = &candidate.plan {
        text.push_str(&format!("\nDestination: {}\n", plan.destination.display()));
        if id == "mlx-runtime" {
            text.push_str(&format!(
                "Write embedded compatibility manifest to {}:\n{}\n",
                env.user_dir.join("python/pyproject.toml").display(),
                PYPROJECT
            ));
        }
        for step in &plan.steps {
            // Debug quoting distinguishes spaces and shell metacharacters; these are argv, not shell.
            text.push_str(&format!(
                "argv: {:?} {:?}\nenv: {:?}\n",
                step.program, step.args, step.env
            ));
        }
        text.push_str(&format!("Only if approved: /setup update {id} yes"));
        reviewed()
            .lock()
            .map_err(|_| anyhow::anyhow!("update review lock poisoned"))?
            .insert((env.user_dir.clone(), id.into()), candidate);
    }
    Ok(text)
}

/// Execute only an explicitly reviewed plan, after fresh ownership/version/plan checks.
/// The caller must obtain explicit approval; this function never interprets a model's consent.
pub fn update(
    id: &str,
    env: &Env,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(&str),
) -> Result<String> {
    ensure!(!cancel.load(Ordering::Acquire), "update cancelled");
    let key = (env.user_dir.clone(), id.to_owned());
    let approved = reviewed()
        .lock()
        .map_err(|_| anyhow::anyhow!("update review lock poisoned"))?
        .remove(&key)
        .with_context(|| {
            format!("review /setup update {id} first, then explicitly confirm with yes")
        })?;
    let recipe = recipe(id).context("unknown external tool recipe")?;
    let path = locate(recipe, env).context("reviewed tool is no longer installed")?;
    let fresh = inspect(recipe, &path, env, &mut |step| query(step, cancel))?;
    ensure!(
        fresh == approved,
        "installation or available update changed; review /setup update {id} again before confirming"
    );
    let plan = fresh.plan.context("no supported update is available")?;
    ensure!(!cancel.load(Ordering::Acquire), "update cancelled");
    if id == "mlx-runtime" {
        crate::config::settings_store::write_private(
            &env.user_dir.join("python/pyproject.toml"),
            PYPROJECT.as_bytes(),
        )?;
    }
    super::run::run_plan(&plan, cancel, progress)?;
    // Re-query instead of claiming a package manager's successful exit means everything is current.
    let path = locate(recipe, env).context("updated tool could not be located")?;
    super::run::health(
        id,
        &path,
        (id == "whisper-cpp")
            .then(|| super::whisper_model_path(&env.user_dir))
            .as_deref(),
    )
    .context("update ran but tool health verification failed")?;
    let verified = inspect(recipe, &path, env, &mut |step| query(step, cancel))?;
    ensure!(
        verified.row.status == Status::Current,
        "update command completed, but verification is not current: {}",
        verified.row.detail
    );
    Ok(format!("{}: {}", id, verified.row.detail))
}

fn query(step: &Step, cancel: &AtomicBool) -> Result<Query> {
    query_with_timeout(step, cancel, QUERY_TIMEOUT)
}
fn query_with_timeout(step: &Step, cancel: &AtomicBool, timeout: Duration) -> Result<Query> {
    let mut command = Command::new(&step.program);
    command
        .args(&step.args)
        .envs(step.env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
    let mut policy = crate::runtime::process::Policy::bounded(timeout, 1024 * 1024);
    policy.may_mutate = false;
    policy.fail_on_output_limit = true;
    let out =
        crate::runtime::process::run_with_policy(command, None, policy, cancel, &mut |_, _| {})
            .map_err(anyhow::Error::msg)?;
    ensure!(!out.cancelled, "update query cancelled");
    ensure!(!out.timed_out, "update query timed out");
    ensure!(
        !out.output_limited && !out.output_incomplete && out.io_error.is_none(),
        "update query output incomplete or over limit"
    );
    Ok(Query {
        code: out.code().unwrap_or(-1),
        stdout: out.stdout_text(),
        stderr: out.stderr_text(2048),
    })
}
fn step(program: &Path, args: &[&str], env: &Env) -> Step {
    let dirs = program
        .parent()
        .into_iter()
        .map(Path::to_path_buf)
        .chain(env.path.iter().cloned());
    Step {
        program: program.display().to_string(),
        args: args.iter().map(|s| (*s).into()).collect(),
        env: vec![
            (
                "PATH".into(),
                std::env::join_paths(dirs)
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned(),
            ),
            ("HOMEBREW_NO_AUTO_UPDATE".into(), "1".into()),
            ("HOMEBREW_NO_ANALYTICS".into(), "1".into()),
            ("UV_NO_PROGRESS".into(), "1".into()),
        ],
    }
}
fn successful(query: Query) -> Result<String> {
    ensure!(
        query.code == 0,
        "query failed ({}): {}",
        query.code,
        query.stderr
    );
    Ok(query.stdout)
}
fn json(query: Query) -> Result<serde_json::Value> {
    serde_json::from_str(&successful(query)?).context("invalid package metadata JSON")
}
fn stamp(path: &Path, env: &Env) -> Result<String> {
    let real = path.canonicalize().context("resolving installed tool")?;
    let meta = std::fs::metadata(&real)?;
    let manifest = std::fs::read(env.user_dir.join("python/pyproject.toml")).ok();
    Ok(format!(
        "{:?}:{:?}:{:?}:{:?}",
        real,
        meta.len(),
        meta.modified()?,
        manifest
    ))
}
fn candidate(
    recipe: &Recipe,
    path: &Path,
    env: &Env,
    status: Status,
    detail: String,
    plan: Option<Plan>,
) -> Result<Candidate> {
    Ok(Candidate {
        row: Row {
            id: recipe.id.into(),
            status,
            detail,
        },
        plan,
        stamp: stamp(path, env)?,
    })
}
fn private_target(path: &Path, dir: &Path) -> bool {
    match (path.canonicalize(), dir.canonicalize()) {
        (Ok(path), Ok(dir)) => path.starts_with(dir),
        _ => false,
    }
}
fn inspect(recipe: &Recipe, path: &Path, env: &Env, query: &mut QueryFn<'_>) -> Result<Candidate> {
    let dir = install_dir(&env.user_dir, recipe.id);
    if recipe.id == "mlx-runtime" {
        return mlx(recipe, path, env, query);
    }
    if private_target(path, &dir) {
        if !private_target(&dir, &env.user_dir) {
            return candidate(recipe, path, env, Status::Manual, "install prefix resolves outside Dial's user directory; refusing to modify a different installation".into(), None);
        }
        if let Some(packages) = recipe.methods.iter().find_map(|m| {
            if let Method::Npm(p) = m {
                Some(*p)
            } else {
                None
            }
        }) && dir.join("node_modules").is_dir()
        {
            return npm(recipe, path, env, packages, query);
        }
        if recipe
            .methods
            .iter()
            .any(|m| matches!(m, Method::Download(_)))
        {
            return upstream(recipe, path, env, query);
        }
        return candidate(recipe, path, env, Status::Manual,
            "Dial-private uv/Go tool: automatic registry upgrades are not supported; review its package compatibility and update the existing private environment manually (not a global install).".into(), None);
    }
    // Homebrew ownership must be proven from its installed keg, not inferred from PATH or method preference.
    if let (Some(brew_program), Some(formula)) = (
        env.find("brew"),
        recipe.methods.iter().find_map(|m| {
            if let Method::Brew(f) = m {
                Some(*f)
            } else {
                None
            }
        }),
    ) {
        return brew(recipe, path, env, &brew_program, formula, query);
    }
    candidate(
        recipe,
        path,
        env,
        Status::Manual,
        format!(
            "{} is externally owned (PATH/rustup/uv/Go/manual); use that installation's manager. Dial will not replace a different install.",
            path.display()
        ),
        None,
    )
}

fn npm(
    recipe: &Recipe,
    path: &Path,
    env: &Env,
    packages: &[&str],
    query: &mut QueryFn<'_>,
) -> Result<Candidate> {
    let dir = install_dir(&env.user_dir, recipe.id);
    let npm = env
        .find("npm")
        .or_else(|| {
            let node = locate(super::recipe("node")?, env)?;
            let npm = node.parent()?.join("npm");
            super::is_executable(&npm).then_some(npm)
        })
        .context("npm unavailable; cannot check private npm install")?;
    let mut changes = Vec::new();
    let mut versions = Vec::new();
    for package in packages {
        let manifest = dir.join("node_modules").join(package).join("package.json");
        ensure!(
            private_target(&manifest, &dir),
            "npm package escapes private installation"
        );
        let installed: serde_json::Value = serde_json::from_slice(&std::fs::read(&manifest)?)?;
        let current = installed["version"]
            .as_str()
            .context("installed npm version missing")?;
        let latest = json(query(&step(
            &npm,
            &[
                "view",
                package,
                "dist-tags.latest",
                "--json",
                "--fetch-retries=0",
                "--fetch-timeout=10000",
            ],
            env,
        ))?)?;
        let latest = latest.as_str().context("npm latest version missing")?;
        match compare_versions(recipe.id, current, latest) {
            Some(Cmp::Less) => {
                changes.push(format!("{package}@{latest}"));
            }
            Some(Cmp::Equal | Cmp::Greater) => {}
            None => {
                return candidate(
                    recipe,
                    path,
                    env,
                    Status::Manual,
                    format!(
                        "cannot safely compare {package} {current} with published {latest}; no downgrade attempted"
                    ),
                    None,
                );
            }
        }
        versions.push(format!("{package}: {current} → {latest}"));
    }
    let plan = if changes.is_empty() {
        None
    } else {
        let mut command = step(
            &npm,
            &[
                "install",
                "--prefix",
                &dir.to_string_lossy(),
                "--no-audit",
                "--no-fund",
                "--ignore-scripts",
                "--save-exact",
            ],
            env,
        );
        // npm's script needs the node beside npm, including private Node installs.
        let dirs = npm
            .parent()
            .into_iter()
            .map(Path::to_path_buf)
            .chain(env.path.iter().cloned());
        command.env.push((
            "PATH".into(),
            std::env::join_paths(dirs)?.to_string_lossy().into_owned(),
        ));
        command.args.extend(changes);
        Some(Plan {
            id: recipe.id,
            method: "private npm pinned package update".into(),
            destination: dir,
            fetch: None,
            steps: vec![command],
            expected: path.to_path_buf(),
        })
    };
    candidate(
        recipe,
        path,
        env,
        if plan.is_some() {
            Status::Available
        } else {
            Status::Current
        },
        versions.join("; "),
        plan,
    )
}

fn brew(
    recipe: &Recipe,
    path: &Path,
    env: &Env,
    brew: &Path,
    formula: &str,
    query: &mut QueryFn<'_>,
) -> Result<Candidate> {
    let value = json(query(&step(brew, &["info", "--json=v2", formula], env))?)?;
    let info = value["formulae"]
        .as_array()
        .and_then(|v| v.first())
        .context("Homebrew formula metadata missing")?;
    let cellar = successful(query(&step(brew, &["--cellar", formula], env))?)?;
    let real = path.canonicalize()?;
    let keg_root = PathBuf::from(cellar.trim());
    let owned = keg_root.is_absolute() && private_target(&real, &keg_root);
    if !owned {
        return candidate(
            recipe,
            path,
            env,
            Status::Manual,
            format!(
                "{} is not owned by Homebrew formula {formula}; update its original installation manually",
                path.display()
            ),
            None,
        );
    }
    // `real` is canonical; `brew --cellar` may return a symlinked prefix (e.g. macOS /var -> /private/var).
    let keg_root_real = keg_root.canonicalize()?;
    let installed = info["installed"]
        .as_array()
        .context("Homebrew installed version metadata missing")?;
    let keg = installed
        .iter()
        .find(|item| {
            item["version"].as_str().is_some_and(|v| {
                let revision = item["revision"].as_u64().unwrap_or(0);
                let folder = if revision == 0
                    || v.rsplit_once('_')
                        .is_some_and(|(_, suffix)| suffix.parse::<u64>().is_ok())
                {
                    v.to_owned()
                } else {
                    format!("{v}_{revision}")
                };
                real.starts_with(keg_root_real.join(folder))
            })
        })
        .context("active binary does not match an installed formula keg")?;
    let current = keg["version"]
        .as_str()
        .context("Homebrew installed version missing")?;
    let (base_version, suffix_revision) = current
        .rsplit_once('_')
        .and_then(|(base, suffix)| suffix.parse::<u64>().ok().map(|revision| (base, revision)))
        .unwrap_or((current, 0));
    let installed_revision = keg["revision"].as_u64().unwrap_or(suffix_revision);
    let latest = info["versions"]["stable"]
        .as_str()
        .context("Homebrew stable version missing")?;
    let curl = env
        .find("curl")
        .context("curl required to check live Homebrew formula metadata")?;
    // Recipes may use Homebrew oldnames/aliases. Only the canonical core name is an API slug.
    if info["tap"]
        .as_str()
        .is_some_and(|tap| tap != "homebrew/core")
    {
        return candidate(recipe, path, env, Status::Manual,
            "Non-core Homebrew tap; check updates with that tap's maintainer. No global update was planned.".into(), None);
    }
    let canonical = info["name"].as_str().unwrap_or(formula);
    ensure!(
        !canonical.is_empty()
            && canonical
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '@' | '+')),
        "invalid canonical Homebrew formula name"
    );
    let url = format!("https://formulae.brew.sh/api/formula/{canonical}.json");
    let published = json(query(&step(
        &curl,
        &[
            "--fail",
            "--silent",
            "--show-error",
            "--proto",
            "=https",
            "--connect-timeout",
            "5",
            "--max-time",
            "10",
            "--max-filesize",
            "1048576",
            &url,
        ],
        env,
    ))?)?;
    let live = published["versions"]["stable"]
        .as_str()
        .context("published Homebrew stable version missing")?;
    let revision = published["revision"].as_u64().unwrap_or(0);
    let local_revision = info["revision"].as_u64().unwrap_or(0);
    if live != latest || revision != local_revision {
        return candidate(
            recipe,
            path,
            env,
            Status::Manual,
            format!(
                "installed {current}; published Homebrew {live} revision {revision}; local formula metadata is stale ({latest} revision {local_revision}). Refresh Homebrew metadata manually, then review again; no upgrade authorized."
            ),
            None,
        );
    }
    if keg["built_as_bottle"] == false
        && keg["version"].as_str().is_some_and(|v| v.contains("HEAD"))
    {
        return candidate(
            recipe,
            path,
            env,
            Status::Manual,
            "development Homebrew build; no downgrade attempted".into(),
            None,
        );
    }
    let ordering = compare_versions(recipe.id, base_version, latest);
    let status = match ordering {
        Some(Cmp::Less) => Status::Available,
        Some(Cmp::Equal) if installed_revision < revision => Status::Available,
        Some(_) => Status::Current,
        None => Status::Manual,
    };
    let plan = (status == Status::Available).then(|| Plan {
        id: recipe.id,
        method: "Homebrew formula upgrade (shared Homebrew installation)".into(),
        destination: keg_root,
        fetch: None,
        steps: vec![step(brew, &["upgrade", formula], env)],
        expected: path.to_path_buf(),
    });
    candidate(
        recipe,
        path,
        env,
        status,
        format!(
            "Homebrew {formula}: installed {current} revision {installed_revision}, published stable {latest} revision {revision}. Unknown/development versions are never downgraded."
        ),
        plan,
    )
}

/// Stable numeric versions only. Unknown/prerelease versions never authorize an install.
/// llama.cpp build numbers are a separate namespace from semantic release versions.
fn compare_versions(id: &str, installed: &str, upstream: &str) -> Option<Cmp> {
    fn build(s: &str) -> Option<u64> {
        let s = s.trim();
        if let Some((_, tail)) = s.split_once("(build ") {
            return tail
                .split(|c: char| !c.is_ascii_digit())
                .next()?
                .parse()
                .ok();
        }
        let s = s
            .strip_prefix('b')
            .or_else(|| s.strip_prefix("version: "))?;
        s.split_whitespace().next()?.parse().ok()
    }
    if id == "llama-cpp" {
        if let (Some(a), Some(b)) = (build(installed), build(upstream)) {
            return Some(a.cmp(&b));
        }
        if build(installed).is_some() || build(upstream).is_some() {
            return None;
        }
    }
    fn numeric(s: &str) -> Option<Vec<u64>> {
        let s = s.trim().strip_prefix('v').unwrap_or(s.trim());
        if s.is_empty() || s.chars().any(|c| !c.is_ascii_digit() && c != '.') {
            return None;
        }
        let mut parts: Vec<u64> = s
            .split('.')
            .map(str::parse)
            .collect::<std::result::Result<_, _>>()
            .ok()?;
        while parts.len() < 3 {
            parts.push(0);
        }
        Some(parts)
    }
    Some(numeric(installed)?.cmp(&numeric(upstream)?))
}

fn upstream(recipe: &Recipe, path: &Path, env: &Env, query: &mut QueryFn<'_>) -> Result<Candidate> {
    let version_query = query(&step(path, &["--version"], env))?;
    ensure!(
        version_query.code == 0,
        "version query failed: {}",
        version_query.stderr
    );
    let current = format!("{}\n{}", version_query.stdout, version_query.stderr);
    let repo = match recipe.id {
        "llama-cpp" => "ggml-org/llama.cpp",
        "bun" => "oven-sh/bun",
        "uv" => "astral-sh/uv",
        "node" => "nodejs/node",
        _ => bail!("no upstream release source mapped"),
    };
    let curl = env
        .find("curl")
        .context("curl required for read-only upstream release checks")?;
    let url = format!("https://api.github.com/repos/{repo}/releases/latest");
    let value = json(query(&step(
        &curl,
        &[
            "--fail",
            "--silent",
            "--show-error",
            "--proto",
            "=https",
            "--connect-timeout",
            "5",
            "--max-time",
            "10",
            "--max-filesize",
            "1048576",
            "-H",
            "Accept: application/vnd.github+json",
            &url,
        ],
        env,
    ))?)?;
    ensure!(
        value["draft"] == false && value["prerelease"] == false,
        "upstream latest is not a stable published release"
    );
    let latest = value["tag_name"].as_str().context("upstream tag missing")?;
    let stable_build = if recipe.id == "llama-cpp" && !latest.starts_with('b') {
        if let Some(asset) = value["assets"]
            .as_array()
            .and_then(|assets| assets.iter().find(|a| a["name"] == "nightly-tag.txt"))
        {
            let url = asset["browser_download_url"]
                .as_str()
                .context("nightly tag asset URL missing")?;
            ensure!(
                url.starts_with("https://github.com/ggml-org/llama.cpp/releases/download/"),
                "unexpected nightly tag asset host/path"
            );
            let digest = asset["digest"]
                .as_str()
                .and_then(|s| s.strip_prefix("sha256:"))
                .context("nightly tag asset has no upstream SHA-256")?;
            let body = successful(query(&step(
                &curl,
                &[
                    "--fail",
                    "--silent",
                    "--show-error",
                    "--location",
                    "--proto",
                    "=https",
                    "--proto-redir",
                    "=https",
                    "--connect-timeout",
                    "5",
                    "--max-time",
                    "10",
                    "--max-filesize",
                    "1024",
                    url,
                ],
                env,
            ))?)?;
            use sha2::Digest;
            ensure!(
                format!("{:x}", sha2::Sha256::digest(body.as_bytes())) == digest,
                "nightly tag digest mismatch"
            );
            let tag = body.trim();
            ensure!(
                tag.strip_prefix('b')
                    .is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit())),
                "invalid nightly build tag"
            );
            Some(tag.to_owned())
        } else {
            None
        }
    } else {
        None
    };
    // llama.cpp --version prints `version: 11868 (...)`, not a semantic release version.
    let installed = if recipe.id == "llama-cpp" {
        current
            .lines()
            .find_map(|s| {
                s.trim()
                    .strip_prefix("version: ")
                    .map(|s| format!("b{}", s.split_whitespace().next().unwrap_or("")))
            })
            .unwrap_or_else(|| current.trim().to_owned())
    } else {
        current
            .split_whitespace()
            .find(|s| {
                s.trim_start_matches('v')
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_digit())
            })
            .unwrap_or(current.trim())
            .to_owned()
    };
    let latest_version = if recipe.id == "bun" {
        latest.strip_prefix("bun-v").unwrap_or(latest)
    } else {
        stable_build.as_deref().unwrap_or(latest)
    };
    let relation = compare_versions(recipe.id, &installed, latest_version);
    let (status, explanation) = match relation {
        Some(Cmp::Equal) => (Status::Current, "matches latest published stable"),
        Some(Cmp::Greater) => (
            Status::Current,
            "newer/development installation retained; no downgrade",
        ),
        Some(Cmp::Less) => (
            Status::Manual,
            "newer published release available; direct archive replacement requires a platform-compatible upstream digest and manual review",
        ),
        None => (
            Status::Manual,
            "versions use incomparable/development namespaces; retained without downgrade, review upstream release manually",
        ),
    };
    candidate(
        recipe,
        path,
        env,
        status,
        format!(
            "installed {installed}, upstream {latest} ({latest_version}): {explanation}. https://github.com/{repo}/releases (no replacement performed)"
        ),
        None,
    )
}

fn mlx(recipe: &Recipe, path: &Path, env: &Env, query: &mut QueryFn<'_>) -> Result<Candidate> {
    let root = env.user_dir.join("python");
    // uv's venv Python is normally a symlink to its interpreter cache outside this directory.
    // Prove ownership of the venv directory, not of the shared interpreter target.
    ensure!(
        private_target(&root, &env.user_dir)
            && path
                .parent()
                .is_some_and(|parent| private_target(parent, &root))
            && root.join(".venv/pyvenv.cfg").is_file(),
        "MLX interpreter is not in Dial's private Python environment"
    );
    let manifest: toml::Value = toml::from_str(PYPROJECT)?;
    let deps = manifest["project"]["dependencies"]
        .as_array()
        .context("embedded Python dependencies missing")?;
    let mut pins = Vec::new();
    for dep in deps {
        let dep = dep.as_str().context("invalid Python dependency")?;
        if let Some((name, version)) = dep.split_once("==") {
            pins.push((name.split('[').next().unwrap_or(name), version));
        }
    }
    let script = "import importlib.metadata as m,json; print(json.dumps({p:m.version(p) for p in ['mlx-lm','mlx-vlm','rich']}))";
    let installed = json(query(&step(path, &["-I", "-B", "-c", script], env))?)?;
    let mut change = false;
    let mut versions = Vec::new();
    for (package, target) in pins {
        let current = installed[package]
            .as_str()
            .context("MLX package version missing")?;
        match compare_versions(package, current, target) {
            Some(Cmp::Less) => change = true,
            Some(Cmp::Equal) => {}
            _ => {
                return candidate(
                    recipe,
                    path,
                    env,
                    Status::Manual,
                    format!(
                        "{package} {current} is newer/development or incomparable with embedded compatibility pin {target}; no downgrade attempted"
                    ),
                    None,
                );
            }
        }
        versions.push(format!("{package} {current} (compatible pin {target})"));
    }
    let rich = installed["rich"].as_str().context("rich version missing")?;
    let rich_ok = compare_versions("rich", rich, "14.0.0").is_some_and(|o| o != Cmp::Less)
        && compare_versions("rich", rich, "15.0.0") == Some(Cmp::Less);
    if !rich_ok {
        return candidate(
            recipe,
            path,
            env,
            Status::Manual,
            format!(
                "rich {rich} outside embedded compatibility range >=14,<15; review private runtime manually"
            ),
            None,
        );
    }
    let plan = if change {
        let uv = locate(super::recipe("uv").context("uv recipe missing")?, env)
            .context("uv unavailable for private runtime sync")?;
        let dir = env.user_dir.join("python");
        ensure!(
            !dir.join("pyproject.toml").is_symlink(),
            "MLX manifest is a symlink; refusing to overwrite"
        );
        let mut sync = step(
            &uv,
            &["sync", "--project", &dir.to_string_lossy(), "--no-dev"],
            env,
        );
        sync.env.push((
            "UV_PROJECT_ENVIRONMENT".into(),
            dir.join(".venv").display().to_string(),
        ));
        sync.env.push(("UV_NO_CONFIG".into(), "1".into()));
        Some(Plan {
            id: recipe.id,
            method: "private MLX embedded compatibility pins".into(),
            destination: dir.clone(),
            fetch: None,
            steps: vec![sync],
            expected: path.into(),
        })
    } else {
        None
    };
    candidate(
        recipe,
        path,
        env,
        if change {
            Status::Available
        } else {
            Status::Current
        },
        format!(
            "{}; rich {rich}. Compared with Dial's embedded compatibility pins, not unbounded PyPI latest.",
            versions.join("; ")
        ),
        plan,
    )
}

#[cfg(test)]
mod tests;
