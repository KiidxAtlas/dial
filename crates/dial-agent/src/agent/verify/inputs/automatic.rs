//! Automatic inputs for the exact built-in `python -I -S` AST check, not arbitrary Python code.
//! Unknown interpreter layouts, wrappers and oversized installations fall back to real execution.
use super::*;
use std::path::PathBuf;

pub(in crate::agent::verify) fn python_snapshot(
    workspace: &Workspace,
    source: &Path,
    python: &Path,
) -> Option<Snapshot> {
    // Shell startup and loader injection can run arbitrary code even for an isolated interpreter.
    if std::env::vars_os().any(|(key, _)| {
        let key = key.to_string_lossy();
        matches!(key.as_ref(), "BASH_ENV" | "ENV")
            || key.starts_with("LD_")
            || key.starts_with("DYLD_")
    }) {
        return None;
    }
    let executable = executable(python)?;
    let (runtime, inputs, excluded) = runtime_inputs(&executable)?;
    let project = capture(
        workspace,
        &VerificationInputs {
            paths: vec![source.to_str()?.to_owned()],
            environment: vec![],
        },
    )?;
    let runtime = capture_excluding(&runtime, &inputs, excluded, IdentityKind::InstalledRuntime)?;
    let mut hash = Sha256::new();
    hash.update(b"dial-automatic-python-v1");
    hash.update(project.0);
    hash.update(runtime.0);
    // Bind resolution and all inherited environment, without storing values in evidence.
    let mut environment = std::env::vars_os().collect::<Vec<_>>();
    environment.sort();
    for (key, value) in environment {
        for bytes in [key.as_encoded_bytes(), value.as_encoded_bytes()] {
            hash.update((bytes.len() as u64).to_le_bytes());
            hash.update(bytes);
        }
    }
    hash.update(executable.as_os_str().as_encoded_bytes());
    Some(Snapshot(hash.finalize().into()))
}

fn executable(python: &Path) -> Option<PathBuf> {
    let path = if python.is_absolute() {
        python.to_owned()
    } else if python.components().count() == 1 {
        std::env::split_paths(&std::env::var_os("PATH")?)
            .filter(|dir| dir.is_absolute())
            .map(|dir| dir.join(python))
            .find(|path| path.is_file())?
    } else {
        return None;
    };
    path.canonicalize().ok()
}

// Only conventional Unix CPython installations are recognized. The complete stdlib is included,
// including bytecode and native modules. `-S` makes site/dist-packages irrelevant. OS libraries are
// part of the stable host runtime, not workspace dependencies, just as for every check execution.
fn runtime_inputs(executable: &Path) -> Option<(Workspace, VerificationInputs, BTreeSet<String>)> {
    if !cfg!(unix) {
        return None;
    }
    let name = executable.file_name()?.to_str()?;
    let version = name.strip_prefix("python3.")?;
    if version.is_empty() || !version.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let bin = executable.parent()?;
    if bin.file_name()? != "bin" {
        return None;
    }
    let root = bin.parent()?;
    let stdlib = format!("lib/python3.{version}");
    if !root.join(&stdlib).join("encodings/__init__.py").is_file()
        || root.join("pyvenv.cfg").exists()
        || bin.join(format!("{name}._pth")).exists()
    {
        return None;
    }
    let runtime = Workspace::new(root).ok()?;
    let mut paths = vec![format!("bin/{name}"), stdlib.clone()];
    // Include the interpreter's shared library and optional stdlib zip, including their absence:
    // discovering a new matching file on a later snapshot changes the path set and its digest.
    let started = Instant::now();
    for (count, entry) in fs::read_dir(root.join("lib")).ok()?.enumerate() {
        if count >= MAX_ENTRIES || started.elapsed() > MAX_TIME {
            return None;
        }
        let entry = entry.ok()?;
        let name = entry.file_name();
        let name = name.to_str()?;
        if name.starts_with(&format!("libpython3.{version}"))
            || name == format!("python3{version}.zip")
        {
            let path = entry.path().canonicalize().ok()?;
            let relative = path.strip_prefix(root).ok()?.to_str()?.to_owned();
            if !paths.contains(&relative) {
                paths.push(relative);
            }
        }
    }
    let excluded = [
        format!("{stdlib}/site-packages"),
        format!("{stdlib}/dist-packages"),
    ]
    .into_iter()
    .collect();
    Some((
        runtime,
        VerificationInputs {
            paths,
            environment: vec![],
        },
        excluded,
    ))
}
