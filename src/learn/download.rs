//! Model preparation; catalog probes remain offline while explicit selection and backend setup may fetch weights.
//!
//! A Hugging Face repository is listed first and then fetched by one of two plans:
//! - MLX (any `.safetensors` present): a full snapshot, minus GGUF files a mixed repo may carry.
//! - GGUF-only: exactly one model file (or one complete split-shard set) plus at most one
//!   multimodal projector. Several quantizations are never chosen silently; the user must pick one.
use crate::llm::local_format::GgufShard as Shard;
use anyhow::{Context, Result, bail, ensure};
use std::path::Path;

const LIST: &str = r#"
import json, sys
from huggingface_hub import list_repo_files
print(json.dumps(list_repo_files(sys.argv[1])))
"#;

const SNAPSHOT: &str = r#"
import json, sys
from huggingface_hub import snapshot_download
print(json.dumps(snapshot_download(repo_id=sys.argv[1], ignore_patterns=["*.gguf"])))
"#;

const FILES: &str = r#"
import json, sys
from huggingface_hub import hf_hub_download
from huggingface_hub.utils import LocalEntryNotFoundError
repo, names = sys.argv[1], sys.argv[2:]
paths = []
for name in names:
    try:
        paths.append(hf_hub_download(repo_id=repo, filename=name, local_files_only=True))
    except LocalEntryNotFoundError:
        paths.append(hf_hub_download(repo_id=repo, filename=name))
print(json.dumps(paths))
"#;

/// What an explicit download fetches, decided from the repository listing before any weights move.
#[derive(Debug, PartialEq, Eq)]
enum Fetch {
    /// MLX repository: the whole snapshot except GGUF files.
    Snapshot,
    /// Exactly these repository-relative files; the first is the model.
    Files(Vec<String>),
}

/// Reuse complete local weights, otherwise download a Hugging Face repository.
/// Local paths are never interpreted as repository ids.
pub fn ensure_local_model(python: &Path, model: &str) -> Result<String> {
    prepare(
        model,
        crate::llm::catalog::resolve_local_model,
        |repo| {
            let output = run_python(python, LIST, &[repo.to_owned()], 600).with_context(|| {
                format!("listing files of {repo:?}; check the repository id, network access, and Hugging Face access")
            })?;
            serde_json::from_str(output.trim()).context("reading the repository file list")
        },
        |repo, fetch| {
            let (script, args) = match fetch {
                Fetch::Snapshot => (SNAPSHOT, vec![repo.to_owned()]),
                Fetch::Files(files) => {
                    let mut args = vec![repo.to_owned()];
                    args.extend(files.iter().cloned());
                    (FILES, args)
                }
            };
            let output = run_python(python, script, &args, 3600).with_context(|| {
                format!("downloading local model {repo:?}; check network access, Hugging Face access, and the local Python runtime")
            })?;
            let paths: Vec<String> = match fetch {
                Fetch::Snapshot => vec![serde_json::from_str(output.trim())?],
                Fetch::Files(_) => serde_json::from_str(output.trim())?,
            };
            Ok(paths)
        },
    )
}

fn run_python(python: &Path, script: &str, args: &[String], timeout: u64) -> Result<String> {
    let mut argv = vec!["-c".to_owned(), script.to_owned()];
    argv.extend(args.iter().cloned());
    crate::learn::worker::run_command(python, &argv, "", timeout)
}

fn prepare(
    model: &str,
    resolve: impl Fn(&str) -> Result<String>,
    list: impl FnOnce(&str) -> Result<Vec<String>>,
    fetch: impl FnOnce(&str, &Fetch) -> Result<Vec<String>>,
) -> Result<String> {
    match resolve(model) {
        Ok(path) => Ok(path),
        Err(error) => {
            if Path::new(model).exists() {
                return Err(error);
            }
            if !is_repo_id(model) {
                bail!(
                    "{model:?} is neither an existing local model path nor a Hugging Face repository id (owner/name); pass a complete local MLX or GGUF model, or a repository id"
                );
            }
            let plan = plan(model, &list(model)?)?;
            let paths = fetch(model, &plan)?;
            let snapshot = paths.first().context("download returned no model path")?;
            ensure!(
                Path::new(snapshot).is_absolute(),
                "download returned a non-absolute snapshot path"
            );
            resolve(snapshot).context("downloaded model has no complete usable weights")
        }
    }
}

fn is_repo_id(model: &str) -> bool {
    let parts: Vec<_> = model.split('/').collect();
    parts.len() == 2
        && parts.iter().all(|part| {
            !part.is_empty()
                && *part != "."
                && *part != ".."
                && part
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
        })
}

/// Choose the files to fetch for `repo` from its listing. Never picks between GGUF quantizations.
fn plan(repo: &str, files: &[String]) -> Result<Fetch> {
    if files.iter().any(|file| file.ends_with(".safetensors")) {
        return Ok(Fetch::Snapshot);
    }
    let gguf: Vec<&String> = files
        .iter()
        .filter(|file| file.to_ascii_lowercase().ends_with(".gguf"))
        .collect();
    let (projectors, mut models): (Vec<&String>, Vec<&String>) = gguf
        .into_iter()
        .partition(|file| file_name(file).to_ascii_lowercase().contains("mmproj"));
    if models.is_empty() {
        bail!(
            "{repo:?} has no MLX safetensors or GGUF weights; Dial downloads only those formats (see docs/model-downloads.md)"
        );
    }
    models.sort();
    let mut selected: Vec<String> = if models.len() == 1 && Shard::parse(models[0]).is_none() {
        vec![models[0].clone()]
    } else {
        let shards: Vec<Option<Shard>> = models.iter().map(|file| Shard::parse(file)).collect();
        match shards.first().and_then(Option::as_ref) {
            Some(first)
                if shards
                    .iter()
                    .all(|shard| shard.as_ref().is_some_and(|s| s.set == first.set)) =>
            {
                let total = first.set.1;
                let mut indices: Vec<u32> = shards.iter().flatten().map(|s| s.index).collect();
                indices.sort_unstable();
                let complete = indices.len() == total as usize
                    && indices
                        .iter()
                        .zip(1..)
                        .all(|(index, expected)| *index == expected);
                ensure!(
                    complete,
                    "{repo:?} has an incomplete GGUF split set (expected {total} shards); download the missing shards manually"
                );
                models.iter().map(|file| (*file).clone()).collect()
            }
            _ => bail!(
                "{repo:?} has {} GGUF quantizations ({}); Dial will not choose one. Download the one you want with `huggingface-cli download {repo} <file>` and set the model to its local path",
                models.len(),
                models
                    .iter()
                    .map(|file| file.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    };
    if projectors.len() > 1 {
        bail!(
            "{repo:?} has {} multimodal projectors ({}); Dial will not choose one. Download the wanted projector manually next to the model",
            projectors.len(),
            projectors
                .iter()
                .map(|file| file.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    selected.extend(projectors.into_iter().cloned());
    Ok(Fetch::Files(selected))
}

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn files(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    #[test]
    fn complete_models_never_download() {
        assert_eq!(
            prepare(
                "org/model",
                |_| Ok("/cached/model".into()),
                |_| panic!("unexpected listing"),
                |_, _| panic!("unexpected download")
            )
            .unwrap(),
            "/cached/model"
        );
    }

    #[test]
    fn missing_mlx_repository_takes_a_snapshot_and_validates() {
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
            |_| {
                Ok(files(&[
                    "config.json",
                    "model.safetensors",
                    "model-q4.gguf",
                ]))
            },
            |repo, fetch| {
                assert_eq!(repo, "org/model");
                assert_eq!(fetch, &Fetch::Snapshot);
                downloaded.set(true);
                Ok(vec!["/snapshot".into()])
            },
        )
        .unwrap();
        assert!(downloaded.get());
        assert_eq!(path, "/snapshot");
    }

    #[test]
    fn invalid_paths_do_not_list_or_download() {
        for model in [
            "/missing/model",
            "./model",
            "../model",
            "model",
            "org/model/subdir",
            "org/model/file.gguf",
            "",
        ] {
            assert!(
                prepare(
                    model,
                    |_| bail!("missing"),
                    |_| panic!("unexpected listing"),
                    |_, _| panic!("unexpected download")
                )
                .is_err(),
                "{model:?} must be rejected"
            );
        }
    }

    #[test]
    fn download_failure_and_incomplete_snapshots_are_errors() {
        assert!(
            prepare(
                "org/model",
                |_| bail!("missing"),
                |_| Ok(files(&["model.safetensors"])),
                |_, _| bail!("network failure")
            )
            .unwrap_err()
            .to_string()
            .contains("network failure")
        );
        assert!(
            prepare(
                "org/model",
                |_| bail!("missing"),
                |_| Ok(files(&["model.safetensors"])),
                |_, _| Ok(vec!["/partial".into()])
            )
            .unwrap_err()
            .to_string()
            .contains("no complete usable weights")
        );
    }

    #[test]
    fn single_gguf_downloads_only_that_file_and_its_projector() {
        let names = files(&["README.md", "m-Q4_K_M.gguf", "mmproj-f16.gguf"]);
        assert_eq!(
            plan("org/gguf", &names).unwrap(),
            Fetch::Files(files(&["m-Q4_K_M.gguf", "mmproj-f16.gguf"]))
        );
        assert_eq!(
            plan("org/gguf", &files(&["m-Q4_K_M.gguf"])).unwrap(),
            Fetch::Files(files(&["m-Q4_K_M.gguf"]))
        );
    }

    #[test]
    fn multiple_quantizations_are_never_chosen_or_downloaded() {
        let names = files(&["m-Q4_K_M.gguf", "m-Q8_0.gguf", "mmproj-f16.gguf"]);
        let error = plan("org/gguf", &names).unwrap_err().to_string();
        assert!(error.contains("2 GGUF quantizations"), "{error}");
        assert!(error.contains("m-Q8_0.gguf"), "{error}");
        assert!(
            error.contains("huggingface-cli download org/gguf <file>"),
            "{error}"
        );
        let downloaded = Cell::new(false);
        assert!(
            prepare(
                "org/gguf",
                |_| bail!("missing"),
                |_| Ok(names.clone()),
                |_, _| {
                    downloaded.set(true);
                    Ok(vec![])
                }
            )
            .is_err()
        );
        assert!(!downloaded.get());
    }

    #[test]
    fn complete_split_sets_download_together_and_partial_sets_fail() {
        assert_eq!(
            plan(
                "org/big",
                &files(&["q/m-00002-of-00002.gguf", "q/m-00001-of-00002.gguf"])
            )
            .unwrap(),
            Fetch::Files(files(&[
                "q/m-00001-of-00002.gguf",
                "q/m-00002-of-00002.gguf"
            ]))
        );
        let error = plan(
            "org/big",
            &files(&["m-00001-of-00003.gguf", "m-00002-of-00003.gguf"]),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("incomplete GGUF split set"), "{error}");
        assert!(plan("org/big", &files(&["m-00001-of-00002.gguf"])).is_err());
        assert!(plan("org/big", &files(&["m-00002-of-00002.gguf"])).is_err());
        // Two different split sets are two quantizations, not one model.
        assert!(
            plan(
                "org/big",
                &files(&["a-00001-of-00002.gguf", "b-00001-of-00001.gguf"])
            )
            .is_err()
        );
    }

    #[test]
    fn ambiguous_projectors_and_missing_weights_are_explained() {
        let error = plan(
            "org/vl",
            &files(&["m.gguf", "mmproj-f16.gguf", "mmproj-f32.gguf"]),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("2 multimodal projectors"), "{error}");
        let error = plan("org/none", &files(&["model.bin", "README.md"]))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("no MLX safetensors or GGUF weights"),
            "{error}"
        );
        assert!(error.contains("docs/model-downloads.md"), "{error}");
    }

    #[test]
    fn mixed_mlx_repository_ignores_gguf_by_snapshot() {
        assert_eq!(
            plan(
                "org/mix",
                &files(&["model.safetensors", "a-Q4.gguf", "a-Q8.gguf"])
            )
            .unwrap(),
            Fetch::Snapshot
        );
    }

    #[cfg(unix)]
    #[test]
    fn subprocess_plans_gguf_files_and_validates_them_before_caching() {
        use std::{fs, os::unix::fs::PermissionsExt};
        let temp = tempfile::tempdir().unwrap();
        let snapshot = temp.path().join("snapshot");
        fs::create_dir(&snapshot).unwrap();
        let model = snapshot.join("m-Q4_K_M.gguf");
        fs::write(&model, b"GGUFweights").unwrap();
        let marker = temp.path().join("fetched");
        let python = temp.path().join("python");
        let json = serde_json::to_string(&model.to_string_lossy()).unwrap();
        // The fake interpreter answers the listing script and records any file fetch.
        fs::write(
            &python,
            format!(
                "#!/bin/sh\n[ \"$1\" = '-c' ] || exit 2\ncase \"$2\" in\n*list_repo_files*) printf '%s\\n' '[\"m-Q4_K_M.gguf\",\"README.md\"]' ;;\n*hf_hub_download*) touch '{marker}'; printf '[%s]\\n' '{json}' ;;\n*) exit 4 ;;\nesac\n",
                marker = marker.display(),
            ),
        )
        .unwrap();
        fs::set_permissions(&python, fs::Permissions::from_mode(0o700)).unwrap();
        let fetch = |repo: &str, fetch: &Fetch| -> Result<Vec<String>> {
            let output = match fetch {
                Fetch::Snapshot => bail!("unexpected snapshot"),
                Fetch::Files(files) => {
                    let mut args = vec![repo.to_owned()];
                    args.extend(files.iter().cloned());
                    run_python(&python, FILES, &args, 10)?
                }
            };
            Ok(serde_json::from_str(output.trim())?)
        };
        let resolved = prepare(
            "org/gguf",
            |value| {
                if Path::new(value).is_absolute() && value.ends_with(".gguf") {
                    crate::llm::local_format::gguf_file(Path::new(value))
                        .map(|path| path.to_string_lossy().into_owned())
                        .context("not a GGUF")
                } else {
                    bail!("missing")
                }
            },
            |repo| {
                let output = run_python(&python, LIST, &[repo.to_owned()], 10)?;
                Ok(serde_json::from_str(output.trim())?)
            },
            fetch,
        )
        .unwrap();
        assert_eq!(Path::new(&resolved), fs::canonicalize(&model).unwrap());
        assert!(marker.exists());

        // Multi-quant repositories are rejected by the listing alone: no fetch marker is created.
        fs::remove_file(&marker).unwrap();
        fs::write(
            &python,
            "#!/bin/sh\necho '[\"m-Q4.gguf\",\"m-Q8.gguf\"]'\nexit 0\n",
        )
        .unwrap();
        fs::set_permissions(&python, fs::Permissions::from_mode(0o700)).unwrap();
        let error = ensure_local_model(&python, "org/multi-quant").unwrap_err();
        assert!(
            format!("{error:#}").contains("2 GGUF quantizations"),
            "{error:#}"
        );
        assert!(!marker.exists());
    }
}
