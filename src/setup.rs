//! Local MLX setup shared by the CLI and guided provider UI.
use anyhow::{Context, Result, bail, ensure};
use crate::{
    config::types::Config,
    learn::worker,
    storage::{paths::Layout, settings_store::{self, Change, Scope}},
};
use std::path::Path;

const WORKER: &str = include_str!("../python/dial_worker.py");
const PYPROJECT: &str = include_str!("../python/pyproject.toml");

/// Install only the private Python runtime. Capture installer output rather than writing into a TUI.
pub fn ensure_runtime(layout: &Layout) -> Result<()> {
    let dir = layout.user_dir.join("python");
    settings_store::private_dir(&dir)?;
    settings_store::write_private(&dir.join("dial_worker.py"), WORKER.as_bytes())?;
    settings_store::write_private(&dir.join("pyproject.toml"), PYPROJECT.as_bytes())?;
    worker::run_command(Path::new("uv"), &["sync".into(), "--project".into(), dir.to_string_lossy().into_owned()], "", 900)
        .context("installing local MLX runtime; uv and network access are required")?;
    ensure!(dir.join(".venv/bin/python").is_file(), "MLX installation did not produce a Python interpreter");
    Ok(())
}

pub fn run(layout: &Layout, model: Option<&str>) -> Result<()> {
    eprintln!("installing mlx-lm into {} (uv)…", layout.user_dir.join("python").display());
    ensure_runtime(layout)?;
    if let Some(model) = model {
        settings_store::save_config(layout, Scope::Global, &[
            Change::Set {path:"model".into(),value:model.into()},
            Change::Set {path:"provider".into(),value:"mlx".into()},
            Change::Remove {path:"endpoint".into()},
        ])?;
    }
    let config = Config::load(layout, None)?;
    if config.model.trim().is_empty() {
        eprintln!("MLX environment ready. Run dial and select a model, or dial setup --model <mlx-model-id>.");
        return Ok(());
    }
    if config.endpoint.is_some() { bail!("project endpoint overrides the selected local model; choose the model in /models with project scope"); }
    eprintln!("verifying {} (first run downloads weights)…", config.model);
    let caps = worker::capabilities(&config)?;
    ensure!(caps["inference"] == true, "model is not supported: {}", caps["reason"]);
    println!("ready: {}\nlora: {}", config.model, caps["lora"]);
    Ok(())
}
