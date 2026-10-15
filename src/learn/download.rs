//! Explicit model preparation; catalog probes and inference remain offline.
use anyhow::{Context, Result, ensure};
use std::path::Path;

const DOWNLOAD: &str = r#"
import json, sys
from huggingface_hub import snapshot_download
print(json.dumps(snapshot_download(repo_id=sys.argv[1])))
"#;

/// Reuse complete local weights, otherwise download a Hugging Face repository.
/// Local paths are never interpreted as repository ids.
pub fn ensure_local_model(python: &Path, model: &str) -> Result<String> {
    prepare(model, crate::llm::catalog::resolve_local_model, |repo| {
        let output = crate::learn::worker::run_command(
            python,
            &["-c".into(), DOWNLOAD.into(), repo.into()],
            "",
            3600,
        )
        .with_context(|| format!("downloading local model {repo:?}; check network access, Hugging Face access, and the local Python runtime"))?;
        serde_json::from_str(output.trim()).context("reading downloaded model snapshot path")
    })
}

fn prepare(
    model: &str,
    resolve: impl Fn(&str) -> Result<String>,
    download: impl FnOnce(&str) -> Result<String>,
) -> Result<String> {
    match resolve(model) {
        Ok(path) => Ok(path),
        Err(error) => {
            let parts: Vec<_> = model.split('/').collect();
            if Path::new(model).exists()
                || parts.len() != 2
                || parts.iter().any(|part| {
                    part.is_empty()
                        || *part == "."
                        || *part == ".."
                        || !part
                            .chars()
                            .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
                })
            {
                return Err(error);
            }
            let snapshot = download(model)?;
            ensure!(
                Path::new(&snapshot).is_absolute(),
                "download returned a non-absolute snapshot path"
            );
            resolve(&snapshot).context("downloaded model has no complete usable weights")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::bail;
    use std::cell::Cell;

    #[test]
    fn complete_models_never_download() {
        assert_eq!(
            prepare(
                "org/model",
                |_| Ok("/cached/model".into()),
                |_| panic!("unexpected download")
            )
            .unwrap(),
            "/cached/model"
        );
    }

    #[test]
    fn missing_and_partial_repositories_download_and_validate() {
        let downloaded = Cell::new(false);
        let path = prepare(
            "org/model",
            |value| {
                if value == "/snapshot" {
                    Ok(value.into())
                } else {
                    bail!("missing or partial")
                }
            },
            |repo| {
                assert_eq!(repo, "org/model");
                downloaded.set(true);
                Ok("/snapshot".into())
            },
        )
        .unwrap();
        assert!(downloaded.get());
        assert_eq!(path, "/snapshot");
    }

    #[test]
    fn invalid_paths_do_not_download() {
        for model in [
            "/missing/model",
            "./model",
            "../model",
            "model",
            "org/model/subdir",
            "",
        ] {
            assert!(
                prepare(
                    model,
                    |_| bail!("missing"),
                    |_| panic!("unexpected download")
                )
                .is_err()
            );
        }
    }

    #[test]
    fn download_failure_and_incomplete_snapshots_are_errors() {
        assert!(
            prepare(
                "org/model",
                |_| bail!("missing"),
                |_| bail!("network failure")
            )
            .unwrap_err()
            .to_string()
            .contains("network failure")
        );
        assert!(
            prepare("org/model", |_| bail!("missing"), |_| Ok("/partial".into()))
                .unwrap_err()
                .to_string()
                .contains("no complete usable weights")
        );
    }

    #[cfg(unix)]
    #[test]
    fn subprocess_snapshot_is_validated_and_cached_paths_skip_python() {
        use std::{fs, os::unix::fs::PermissionsExt};
        let temp = tempfile::tempdir().unwrap();
        let snapshot = temp.path().join("snapshot");
        fs::create_dir(&snapshot).unwrap();
        fs::write(snapshot.join("config.json"), "{}").unwrap();
        fs::write(snapshot.join("model.safetensors"), "weights").unwrap();
        let python = temp.path().join("python");
        let json = serde_json::to_string(&snapshot.to_string_lossy()).unwrap();
        fs::write(&python, format!(
            "#!/bin/sh\n[ \"$1\" = '-c' ] || exit 2\n[ \"$3\" = 'dial-download-regression-test/not-a-real-model' ] || exit 3\nprintf '%s\\n' '{json}'\n"
        )).unwrap();
        fs::set_permissions(&python, fs::Permissions::from_mode(0o700)).unwrap();
        let resolved =
            ensure_local_model(&python, "dial-download-regression-test/not-a-real-model").unwrap();
        assert_eq!(Path::new(&resolved), fs::canonicalize(&snapshot).unwrap());
        fs::remove_file(&python).unwrap();
        assert_eq!(ensure_local_model(&python, &resolved).unwrap(), resolved);
        fs::write(
            snapshot.join("model.safetensors.index.json"),
            r#"{"weight_map":{"w":"missing.safetensors"}}"#,
        )
        .unwrap();
        assert!(ensure_local_model(&python, &resolved).is_err());
    }
}
