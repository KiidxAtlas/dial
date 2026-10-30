//! Operator-trusted Python plugins use an install-time venv and Dial's bundled SDK.
use super::{MANIFEST_FILE, RuntimeSpec, manifest_env, resolve_inside};
use crate::runtime::process;
use anyhow::{Context, Result, anyhow, ensure};
use serde::Deserialize;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::AtomicBool,
    time::Duration,
};

const RUNTIME: &str = ".dial-python";
const TIMEOUT: Duration = Duration::from_secs(300);
pub(crate) const SDK: &[(&str, &str)] = &[
    (
        "__init__.py",
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../python/dial_extensions/__init__.py"
        )),
    ),
    (
        "__main__.py",
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../python/dial_extensions/__main__.py"
        )),
    ),
    (
        "_launcher.py",
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../python/dial_extensions/_launcher.py"
        )),
    ),
    (
        "host.py",
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../python/dial_extensions/host.py"
        )),
    ),
    (
        "contributions.py",
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../python/dial_extensions/contributions.py"
        )),
    ),
    (
        "scheduler.py",
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../python/dial_extensions/scheduler.py"
        )),
    ),
    (
        "testing.py",
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../python/dial_extensions/testing.py"
        )),
    ),
];

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Python {
    entry: String,
    #[serde(default = "default_interpreter")]
    interpreter: String,
    #[serde(default)]
    dependencies: Vec<String>,
    #[serde(default)]
    lockfile: Option<String>,
    #[serde(default)]
    runtime: PythonRuntime,
}

fn default_interpreter() -> String {
    "python3".into()
}
#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PythonRuntime {
    #[default]
    Request,
    Worker,
}

fn declaration(map: &Map<String, Value>, base: &Path) -> Result<Python> {
    ensure!(
        !map.contains_key("program") && !map.contains_key("args"),
        "python is mutually exclusive with program and args"
    );
    let python: Python = serde_json::from_value(
        map.get("python")
            .context("missing python declaration")?
            .clone(),
    )
    .with_context(|| {
        format!("{MANIFEST_FILE} field \"python\" must declare entry, interpreter and dependencies")
    })?;
    ensure!(
        !python.entry.is_empty()
            && python.entry.len() <= 4096
            && !python.entry.contains('\\')
            && python.entry.ends_with(".py"),
        "python entry must be a bounded relative path"
    );
    resolve_inside(base, &python.entry).context("invalid python entry")?;
    ensure!(
        !python.interpreter.is_empty()
            && python.interpreter.len() <= 4096
            && !python.interpreter.starts_with('-')
            && !python.interpreter.contains('\0'),
        "python interpreter must name an installed interpreter"
    );
    ensure!(
        python.dependencies.len() <= 256,
        "python dependencies exceeds 256 entries"
    );
    for dependency in &python.dependencies {
        ensure!(
            !dependency.trim().is_empty()
                && dependency.len() <= 4096
                && !dependency.trim_start().starts_with('-')
                && !dependency.contains(['\0', '\n', '\r']),
            "invalid python dependency {dependency:?}"
        );
    }
    if let Some(lockfile) = &python.lockfile {
        ensure!(
            python.dependencies.is_empty(),
            "python lockfile and dependencies are mutually exclusive"
        );
        let path = resolve_inside(base, lockfile).context("invalid Python lockfile")?;
        ensure!(
            fs::metadata(&path)?.len() <= 1 << 20,
            "Python lockfile exceeds 1 MiB"
        );
        let text = fs::read_to_string(path).context("reading Python lockfile")?;
        ensure!(
            !text.trim().is_empty() && !text.contains('\0'),
            "Python lockfile must be nonempty text"
        );
        // pip's --require-hashes enforces every requirement and transitive dependency at install.
        // Disallow option/recursive-file directives, which can redirect the staged lock boundary.
        for line in text
            .lines()
            .map(str::trim)
            .filter(|s| !s.is_empty() && !s.starts_with('#'))
        {
            ensure!(
                !line.starts_with('-') || line.starts_with("--hash=sha256:"),
                "Python lockfile may contain requirements and SHA256 hashes only"
            );
        }
    }
    ensure!(
        map.get("settings").is_none_or(Value::is_object),
        "extension settings must be a JSON object"
    );
    let env = manifest_env(map)?;
    ensure!(
        !env.keys().any(|key| key.starts_with("DIAL_EXTENSION_")),
        "extension env cannot override reserved DIAL_EXTENSION_ variables"
    );
    Ok(python)
}

pub(super) fn validate(map: &Map<String, Value>, base: &Path) -> Result<()> {
    declaration(map, base)?;
    ensure!(
        !base.join(RUNTIME).exists(),
        "{RUNTIME} is reserved for Dial's managed Python runtime"
    );
    Ok(())
}

pub(super) fn prepare(
    map: &Map<String, Value>,
    base: &Path,
    store: &Path,
) -> Result<Option<tempfile::TempDir>> {
    if !map.contains_key("python") {
        return Ok(None);
    }
    let base = fs::canonicalize(base).context("resolving staged Python extension")?;
    let python = declaration(map, &base)?;
    let runtime = base.join(RUNTIME);
    fs::create_dir_all(store.join(".python-envs"))
        .context("creating extension environment root")?;
    let environments = environment_root(store)?;
    let environment = tempfile::Builder::new()
        .prefix("env-")
        .tempdir_in(&environments)
        .context("allocating stable extension Python environment")?;
    let venv = environment.path().join("venv");
    fs::create_dir_all(&runtime)?;
    let mut command = Command::new(&python.interpreter);
    command.args([
        "-I", "-c",
        "import sys; sys.exit('Dial Python extensions require Python 3.11 or newer') if sys.version_info < (3, 11) else None",
    ]);
    run(
        command,
        "checking extension Python version (requires 3.11+)",
    )?;
    let mut command = Command::new(&python.interpreter);
    command.args(["-I", "-m", "venv"]).arg(&venv);
    // Avoid bootstrapping pip when no dependencies were explicitly requested.
    if python.dependencies.is_empty() && python.lockfile.is_none() {
        command.arg("--without-pip");
    }
    run(command, "creating extension Python environment")?;
    if !python.dependencies.is_empty() {
        let mut command = Command::new(venv_python(environment.path()));
        command
            .args([
                "-I",
                "-m",
                "pip",
                "--isolated",
                "install",
                "--disable-pip-version-check",
                "--no-input",
                "--",
            ])
            .args(&python.dependencies)
            .current_dir(&base);
        run(command, "installing extension Python dependencies")?;
    }
    if let Some(lockfile) = &python.lockfile {
        let mut command = Command::new(venv_python(environment.path()));
        command
            .args([
                "-I",
                "-m",
                "pip",
                "--isolated",
                "install",
                "--disable-pip-version-check",
                "--no-input",
                "--require-hashes",
                "-r",
            ])
            .arg(resolve_inside(&base, lockfile)?)
            .current_dir(&base);
        run(
            command,
            "installing hash-locked extension Python dependencies",
        )?;
    }
    let sdk = runtime.join("sdk/dial_extensions");
    fs::create_dir_all(&sdk)?;
    for (name, source) in SDK {
        fs::write(sdk.join(name), source)?;
    }
    fs::write(runtime.join("launcher.py"), SDK[2].1)?;
    fs::write(
        runtime.join("environment"),
        environment
            .path()
            .file_name()
            .context("environment has no name")?
            .as_encoded_bytes(),
    )?;
    fs::write(runtime.join("ready"), sdk_identity())?;
    Ok(Some(environment))
}

fn sdk_identity() -> String {
    let mut digest = Sha256::new();
    for (name, source) in SDK {
        digest.update(name.as_bytes());
        digest.update([0]);
        digest.update(source.as_bytes());
    }
    crate::hex(&digest.finalize())
}

fn run(command: Command, what: &str) -> Result<()> {
    let never = AtomicBool::new(false);
    let finished =
        process::run(command, None, TIMEOUT, 1 << 20, &never, &mut |_, _| {}).map_err(|error| {
            anyhow!("{what}; install the configured interpreter locally first: {error}")
        })?;
    ensure!(finished.success(), "{}", finished.failure(what, TIMEOUT));
    Ok(())
}

/// Only direct, non-symlink children of the store-owned environment root are accepted.
fn environment_root(store: &Path) -> Result<PathBuf> {
    let store = fs::canonicalize(store).context("resolving extension store")?;
    let root = store.join(".python-envs");
    ensure!(
        !fs::symlink_metadata(&root)?.file_type().is_symlink(),
        "extension environment root must not be a symlink"
    );
    let root = fs::canonicalize(root)?;
    ensure!(
        root.parent() == Some(store.as_path()),
        "extension environment root escapes store"
    );
    Ok(root)
}

pub(super) fn environment(base: &Path, store: &Path) -> Result<Option<PathBuf>> {
    let pointer = base.join(RUNTIME).join("environment");
    if !pointer.exists() {
        return Ok(None);
    }
    let name = fs::read_to_string(&pointer).context("reading extension environment pointer")?;
    let path = Path::new(&name);
    ensure!(
        name.starts_with("env-")
            && name.len() <= 128
            && path.components().count() == 1
            && matches!(
                path.components().next(),
                Some(std::path::Component::Normal(_))
            )
            && !name.contains(['/', '\\']),
        "invalid extension environment pointer"
    );
    let root = environment_root(store)?;
    let environment = root.join(path);
    // A missing environment is incomplete, but still a safe cleanup target.
    if environment.exists() {
        ensure!(
            !fs::symlink_metadata(&environment)?.file_type().is_symlink()
                && fs::canonicalize(&environment)?.parent() == Some(root.as_path()),
            "extension environment pointer escapes owned environment root"
        );
    }
    Ok(Some(environment))
}

pub(super) fn cleanup(environment: Option<PathBuf>) -> Result<()> {
    if let Some(path) = environment {
        match fs::remove_dir_all(&path) {
            Ok(()) => {},
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {},
            Err(error) => return Err(error).with_context(|| format!("plugin change committed, but previous Python environment could not be removed (it may still be in use): {}", path.display())),
        }
    }
    Ok(())
}

fn venv_python(runtime: &Path) -> PathBuf {
    runtime.join(if cfg!(windows) {
        "venv/Scripts/python.exe"
    } else {
        "venv/bin/python"
    })
}

pub(super) fn runtime(map: &Map<String, Value>, base: &Path) -> Result<RuntimeSpec> {
    let python = declaration(map, base)?;
    let base = fs::canonicalize(base).context("resolving extension root")?;
    let runtime = base.join(RUNTIME);
    let environment = environment(&base, base.parent().context("extension root has no store")?)?
        .context(
            "extension Python environment is missing; explicitly reinstall or update the extension",
        )?;
    let program = venv_python(&environment);
    ensure!(
        program.is_file()
            && environment.join("venv/pyvenv.cfg").is_file()
            && fs::read_to_string(runtime.join("ready"))
                .is_ok_and(|identity| identity == sdk_identity())
            && runtime.join("launcher.py").is_file()
            && SDK
                .iter()
                .all(|(name, _)| runtime.join("sdk/dial_extensions").join(name).is_file()),
        "extension Python environment is incomplete or uses an incompatible SDK; explicitly reinstall or update the extension"
    );
    let name = base.file_name().context("extension root has no name")?;
    let state = base
        .parent()
        .context("extension root has no store")?
        .join(".state")
        .join(name);
    fs::create_dir_all(&state).context("creating extension state directory")?;
    let mut env = manifest_env(map)?;
    env.insert(
        "DIAL_EXTENSION_RUNTIME".into(),
        match python.runtime {
            PythonRuntime::Request => "request",
            PythonRuntime::Worker => "worker",
        }
        .into(),
    );
    env.insert(
        "DIAL_EXTENSION_NAME".into(),
        name.to_string_lossy().into_owned(),
    );
    env.insert(
        "DIAL_EXTENSION_ROOT".into(),
        base.to_string_lossy().into_owned(),
    );
    env.insert(
        "DIAL_EXTENSION_STATE_DIR".into(),
        state.to_string_lossy().into_owned(),
    );
    env.insert(
        "DIAL_EXTENSION_SETTINGS".into(),
        serde_json::to_string(map.get("settings").unwrap_or(&Value::Object(Map::new())))?,
    );
    Ok((
        program,
        vec![
            "-I".into(),
            runtime.join("launcher.py").to_string_lossy().into_owned(),
            resolve_inside(&base, &python.entry)?
                .to_string_lossy()
                .into_owned(),
        ],
        env,
    ))
}

#[cfg(test)]
mod tests;
