//! Local MLX setup shared by the CLI and guided provider UI.
pub mod deps;
pub mod embeddings;
pub mod memory;
use crate::config::paths::Layout;
use crate::config::settings_store::{self, Scope};
use crate::config::types::Config;
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

/// `capabilities` probes a model's capabilities once its runtime is installed; the caller supplies
/// the learn-layer probe so this tools-layer module does not depend on `learn`.
pub fn run(
    layout: &Layout,
    model: Option<&str>,
    capabilities: impl Fn(&Config) -> Result<serde_json::Value>,
) -> Result<()> {
    let prepare = |config: &Config| {
        eprintln!(
            "installing mlx-lm into {} (uv)…",
            layout.user_dir.join("python").display()
        );
        ensure_runtime(layout)?;
        eprintln!("verifying {} (first run downloads weights)…", config.model);
        crate::llm::download::ensure_local_model(&config.python, &config.model)?;
        capabilities(config)
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
mod tests;
