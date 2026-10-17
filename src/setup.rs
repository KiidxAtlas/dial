//! Local MLX setup shared by the CLI and guided provider UI.
pub mod deps;
pub mod embeddings;
pub mod memory;
use crate::{
    config::types::Config,
    learn::worker,
    storage::{
        paths::Layout,
        settings_store::{self, Scope},
    },
};
use anyhow::{Context, Result, ensure};
use std::{
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

const WORKER: &str = include_str!("../python/dial_worker.py");
const STEERING: &str = include_str!("../python/dial_steering.py");
const PYPROJECT: &str = include_str!("../python/pyproject.toml");

/// Keep the managed worker aligned with the binary without reinstalling Python packages.
/// Explicit custom worker paths are never replaced.
pub(crate) fn refresh_worker(config: &Config) -> Result<()> {
    let managed = config.user_dir.join("python/dial_worker.py");
    if config.worker == managed
        && std::fs::read(&managed).ok().as_deref() != Some(WORKER.as_bytes())
    {
        settings_store::write_private(&managed, WORKER.as_bytes())?;
    }
    if config.worker == managed {
        let path = config.user_dir.join("python/dial_steering.py");
        if std::fs::read(&path).ok().as_deref() != Some(STEERING.as_bytes()) {
            settings_store::write_private(&path, STEERING.as_bytes())?;
        }
    }
    Ok(())
}

/// Install only the private Python runtime. Capture installer output rather than writing into a TUI.
pub fn ensure_runtime(layout: &Layout) -> Result<()> {
    ensure_runtime_with(layout, &AtomicBool::new(false), &|_| {})
}

/// Cancellable installer shared by CLI setup and the interactive background worker.
pub fn ensure_runtime_with(
    layout: &Layout,
    cancel: &AtomicBool,
    progress: &dyn Fn(&str),
) -> Result<()> {
    ensure!(
        !cancel.load(Ordering::SeqCst),
        "runtime installation cancelled"
    );
    ensure!(
        cfg!(target_os = "macos") && cfg!(target_arch = "aarch64"),
        "local MLX requires macOS on Apple Silicon; use /providers to configure a hosted or OpenAI-compatible provider on Intel Macs or Linux/WSL2"
    );
    let dir = layout.user_dir.join("python");
    settings_store::private_dir(&dir)?;
    settings_store::write_private(&dir.join("dial_worker.py"), WORKER.as_bytes())?;
    settings_store::write_private(&dir.join("dial_steering.py"), STEERING.as_bytes())?;
    settings_store::write_private(&dir.join("pyproject.toml"), PYPROJECT.as_bytes())?;
    let mut command = std::process::Command::new("uv");
    command.args(["sync", "--project"]).arg(&dir);
    let timeout = Duration::from_secs(900);
    let output = crate::runtime::process::run(
        command,
        None,
        timeout,
        1024 * 1024,
        cancel,
        &mut |_, bytes| {
            progress(&String::from_utf8_lossy(bytes));
        },
    )
    .map_err(anyhow::Error::msg)
    .context("installing local MLX runtime; uv and network access are required")?;
    ensure!(
        output.success(),
        "{}",
        output.failure("installing local MLX runtime", timeout)
    );
    ensure!(
        dir.join(".venv/bin/python").is_file(),
        "MLX installation did not produce a Python interpreter"
    );
    Ok(())
}

/// Prepare the exact requested candidate before publishing any model settings. A project
/// override must not cause setup to verify a different model than the one being saved.
fn select_model(
    layout: &Layout,
    model: &str,
    prepare: impl FnOnce(&Config) -> Result<serde_json::Value>,
) -> Result<serde_json::Value> {
    let config = settings_store::load_config(layout, &toml::Table::new())?;
    let selection = crate::llm::model_config::ModelSelection {
        provider: "mlx".into(),
        model: model.into(),
        ..Default::default()
    };
    let changes = crate::config::selection::changes(&config, &selection, "main")?;
    let candidate = settings_store::preview_model_config(layout, Scope::Global, &changes, "main")?;
    ensure!(
        candidate.model == model && candidate.endpoint.is_none(),
        "setup did not resolve the requested local model"
    );
    let caps = prepare(&candidate)?;
    ensure!(
        caps["inference"] == true,
        "model is not supported: {}",
        caps["reason"]
    );
    settings_store::save_model_config(layout, Scope::Global, &changes, "main")?;
    Ok(caps)
}

pub fn run(layout: &Layout, model: Option<&str>) -> Result<()> {
    let prepare = |config: &Config| {
        eprintln!(
            "installing mlx-lm into {} (uv)…",
            layout.user_dir.join("python").display()
        );
        ensure_runtime(layout)?;
        eprintln!("verifying {} (first run downloads weights)…", config.model);
        crate::llm::download::ensure_local_model(&config.python, &config.model)?;
        worker::capabilities(config)
    };
    if let Some(model) = model {
        let caps = select_model(layout, model, prepare)?;
        println!("ready: {model}\nlora: {}", caps["lora"]);
    } else {
        let config = settings_store::load_config(layout, &toml::Table::new())?;
        if config.model.trim().is_empty() || !config.runs_dial_mlx() {
            ensure_runtime(layout)?;
            eprintln!(
                "MLX environment ready. Run dial and select a model, or dial setup --model <mlx-model-id>."
            );
        } else {
            let caps = prepare(&config)?;
            ensure!(
                caps["inference"] == true,
                "model is not supported: {}",
                caps["reason"]
            );
            println!("ready: {}\nlora: {}", config.model, caps["lora"]);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn upgrades_managed_worker_without_touching_custom_workers() {
        let root = tempfile::tempdir().unwrap();
        let mut config = Config {
            user_dir: root.path().to_owned(),
            worker: root.path().join("python/dial_worker.py"),
            ..Config::default()
        };
        std::fs::create_dir(root.path().join("python")).unwrap();
        std::fs::write(&config.worker, "old worker").unwrap();
        refresh_worker(&config).unwrap();
        assert_eq!(std::fs::read_to_string(&config.worker).unwrap(), WORKER);
        config.worker = root.path().join("custom.py");
        std::fs::write(&config.worker, "custom worker").unwrap();
        refresh_worker(&config).unwrap();
        assert_eq!(
            std::fs::read_to_string(&config.worker).unwrap(),
            "custom worker"
        );
    }

    #[test]
    fn model_setup_verifies_exact_candidate_before_saving() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let layout = Layout::with_user_dir(project.path(), home.path().join("dial")).unwrap();
        use settings_store::Change;
        settings_store::save_config(
            &layout,
            Scope::Global,
            &[Change::Set {
                path: "model".into(),
                value: "old-global".into(),
            }],
        )
        .unwrap();
        settings_store::save_config(
            &layout,
            Scope::Project,
            &[Change::Set {
                path: "model".into(),
                value: "project-override".into(),
            }],
        )
        .unwrap();
        let before: Vec<_> = layout
            .config_files()
            .iter()
            .map(|p| std::fs::read(p).unwrap())
            .collect();
        for supported in [false, true] {
            let result = select_model(&layout, "org/requested", |candidate| {
                assert_eq!(candidate.model, "org/requested");
                assert!(candidate.endpoint.is_none());
                for (path, contents) in layout.config_files().iter().zip(&before) {
                    assert_eq!(&std::fs::read(path).unwrap(), contents);
                }
                Ok(
                    serde_json::json!({"inference": supported, "reason": "unsupported", "lora": false}),
                )
            });
            assert_eq!(result.is_ok(), supported);
            if !supported {
                for (path, contents) in layout.config_files().iter().zip(&before) {
                    assert_eq!(&std::fs::read(path).unwrap(), contents);
                }
            }
        }
        assert_eq!(
            settings_store::load_config(&layout, &toml::Table::new())
                .unwrap()
                .model,
            "org/requested"
        );
    }

    #[test]
    fn cancelled_runtime_install_does_not_create_files() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let layout = Layout::with_user_dir(project.path(), home.path().join("dial")).unwrap();
        assert!(
            ensure_runtime_with(&layout, &AtomicBool::new(true), &|_| panic!("no installer"))
                .is_err()
        );
        assert!(!layout.user_dir.exists());
    }
}
