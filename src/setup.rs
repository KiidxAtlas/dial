//! Local MLX setup shared by the CLI and guided provider UI.
use crate::{
    config::types::Config,
    learn::worker,
    storage::{
        paths::Layout,
        settings_store::{self, Change, Scope},
    },
};
use anyhow::{Context, Result, bail, ensure};
use std::path::Path;

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
    ensure!(
        cfg!(target_os = "macos") && cfg!(target_arch = "aarch64"),
        "local MLX requires macOS on Apple Silicon; use /providers to configure a hosted or OpenAI-compatible provider on Intel Macs, Windows, or Linux"
    );
    let dir = layout.user_dir.join("python");
    settings_store::private_dir(&dir)?;
    settings_store::write_private(&dir.join("dial_worker.py"), WORKER.as_bytes())?;
    settings_store::write_private(&dir.join("dial_steering.py"), STEERING.as_bytes())?;
    settings_store::write_private(&dir.join("pyproject.toml"), PYPROJECT.as_bytes())?;
    worker::run_command(
        Path::new("uv"),
        &[
            "sync".into(),
            "--project".into(),
            dir.to_string_lossy().into_owned(),
        ],
        "",
        900,
    )
    .context("installing local MLX runtime; uv and network access are required")?;
    ensure!(
        dir.join(".venv/bin/python").is_file(),
        "MLX installation did not produce a Python interpreter"
    );
    Ok(())
}

pub fn run(layout: &Layout, model: Option<&str>) -> Result<()> {
    eprintln!(
        "installing mlx-lm into {} (uv)…",
        layout.user_dir.join("python").display()
    );
    ensure_runtime(layout)?;
    if let Some(model) = model {
        settings_store::save_config(
            layout,
            Scope::Global,
            &[
                Change::Set {
                    path: "model".into(),
                    value: model.into(),
                },
                Change::Set {
                    path: "provider".into(),
                    value: "mlx".into(),
                },
                Change::Remove {
                    path: "endpoint".into(),
                },
            ],
        )?;
    }
    let config = Config::load(layout, None)?;
    if config.model.trim().is_empty() {
        eprintln!(
            "MLX environment ready. Run dial and select a model, or dial setup --model <mlx-model-id>."
        );
        return Ok(());
    }
    if config.endpoint.is_some() {
        bail!(
            "project endpoint overrides the selected local model; choose the model in /models with project scope"
        );
    }
    eprintln!("verifying {} (first run downloads weights)…", config.model);
    let caps = worker::capabilities(&config)?;
    ensure!(
        caps["inference"] == true,
        "model is not supported: {}",
        caps["reason"]
    );
    println!("ready: {}\nlora: {}", config.model, caps["lora"]);
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
}
