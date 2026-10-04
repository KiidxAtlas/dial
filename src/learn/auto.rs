//! Automatic background scheduling. Training and admission run outside the foreground session.
use super::improve;
use crate::{agent::Runtime, config::types::Config, storage::{paths::Layout, store::Store}};
use anyhow::{Context, Result, ensure};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{fs::{self, File, OpenOptions}, os::unix::{fs::{OpenOptionsExt, PermissionsExt}, process::CommandExt}, path::{Path, PathBuf}, process::{Command, Stdio}};

#[derive(Serialize, Deserialize)]
struct Request { config: Config, project: PathBuf, state: PathBuf, user: PathBuf }

pub fn lock_file(path: &Path) -> Result<File> {
    if let Some(parent) = path.parent() { fs::create_dir_all(parent)?; }
    Ok(OpenOptions::new().read(true).write(true).create(true).truncate(false).mode(0o600).open(path)?)
}

pub fn compute_lock(layout: &Layout) -> Result<File> {
    let file = lock_file(&layout.user_dir.join("compute.lock"))?;
    if file.try_lock_exclusive().is_err() {
        eprintln!("dial: waiting for local compute (another session or improvement cycle is active)");
        file.lock_exclusive()?;
    }
    Ok(file)
}

pub fn enabled(config: &Config, store: &Store) -> Result<bool> {
    Ok(config.improve.enabled && store.get_meta("auto_enabled")?.as_deref() != Some("false"))
}

pub fn due(config: &Config, store: &Store) -> Result<bool> {
    if !enabled(config, store)? || !improve::due(store, "last_cycle_attempt", config.improve.interval_secs)? { return Ok(false); }
    let watermark = store.get_meta("last_cycle_trajectory")?.map(|v| v.parse::<i64>()).transpose()?.unwrap_or(0);
    Ok(store.live_turns_after(watermark)? >= config.improve.min_new_turns as usize)
}

pub fn maybe_spawn(layout: &Layout, config: &Config, store: &Store) -> Result<bool> {
    if !config.improve.background || !due(config, store)? { return Ok(false); }
    let control = lock_file(&config.state_dir.join("control.lock"))?;
    if control.try_lock_exclusive().is_err() { return Ok(false); }
    let launch = lock_file(&config.state_dir.join("launch.lock"))?;
    if launch.try_lock_exclusive().is_err() || !improve::due(store, "last_background_spawn", 30)? { return Ok(false); }
    let request_path = config.state_dir.join("background-request.json");
    let mut request_config = config.clone();
    request_config.model = store.get_meta("model")?.context("store model binding missing")?;
    let request = Request { config: request_config, project: layout.project_root.clone(), state: config.state_dir.clone(), user: layout.user_dir.clone() };
    let mut file = OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&request_path)?;
    serde_json::to_writer(&mut file, &request)?;
    file.sync_all()?;
    let log = OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(config.state_dir.join("improvement.log"))?;
    let mut child = Command::new(std::env::current_exe()?)
        .arg("-C").arg(&layout.project_root).arg("improve").arg("background").arg(&request_path)
        .env("DIAL_HOME", &layout.user_dir).stdin(Stdio::null()).stdout(log.try_clone()?).stderr(log)
        .process_group(0).spawn().context("starting automatic improvement worker")?;
    store.set_meta("last_background_spawn", &improve::now().to_string())?;
    store.set_meta("auto_status", &json!({"state":"queued","pid":child.id(),"at":improve::now()}).to_string())?;
    std::thread::spawn(move || { let _ = child.wait(); });
    Ok(true)
}

pub fn background(path: &Path) -> Result<()> {
    ensure!(fs::metadata(path)?.len() <= 1024 * 1024, "background request exceeds limit");
    let request: Request = serde_json::from_slice(&fs::read(path)?)?;
    let layout = Layout::with_user_dir(&request.project, request.user)?;
    let mut config = request.config;
    ensure!(config.learning_dir(&layout) == request.state, "background state directory mismatch");
    config.state_dir = request.state;
    config.project_root = layout.project_root.clone();
    config.user_dir = layout.user_dir.clone();
    config.validate()?;
    let store = Store::open(&config.state_dir, &config.model, &config.use_case, &config.harness)?;
    let control = lock_file(&config.state_dir.join("control.lock"))?;
    control.lock_exclusive()?;
    if !due(&config, &store)? { return Ok(()); }
    let _compute = if config.uses_local_compute() { Some(compute_lock(&layout)?) } else { None };
    let current = Config::load(&layout, None)?;
    if !current.improve.enabled || !enabled(&config, &store)? {
        store.set_meta("auto_status", &json!({"state":"disabled","at":improve::now()}).to_string())?;
        return Ok(());
    }
    execute(&mut config, &store, false, &mut |s| eprintln!("{s}"))?;
    Ok(())
}

/// Caller holds control + compute locks. Failures are durable and rate-limited, not swallowed.
pub fn execute(config: &mut Config, store: &Store, force: bool, progress: &mut dyn FnMut(&str)) -> Result<Value> {
    let now = improve::now();
    store.set_meta("last_cycle_attempt", &now.to_string())?;
    store.set_meta("auto_status", &json!({"state":"running","pid":std::process::id(),"at":now}).to_string())?;
    let watermark = store.max_trajectory()?;
    let result = (|| {
        improve::pin_models(config, store, true)?;
        improve::run_cycle(&mut Runtime::new(), config, store, force, progress)
    })();
    match &result {
        Ok(report) => {
            store.set_meta("last_cycle_trajectory", &watermark.to_string())?;
            store.set_meta("auto_status", &json!({"state":"idle","at":improve::now(),"admitted":report["admitted"],"active":report["active"]}).to_string())?;
        }
        Err(error) => store.set_meta("auto_status", &json!({"state":"failed","at":improve::now(),"error":format!("{error:#}")}).to_string())?,
    }
    result
}

pub fn prepare_state(layout: &Layout) -> Result<()> {
    fs::create_dir_all(&layout.state_dir)?;
    fs::set_permissions(&layout.state_dir, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::types::Harness;
    #[test]
    fn only_new_live_turns_trigger_and_disable_and_backoff_win() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::open(root.path(), "model", "project", &Harness::default()).unwrap();
        let mut cfg = Config::default();
        cfg.improve.min_new_turns = 2;
        let active = store.active().unwrap();
        assert!(!due(&cfg, &store).unwrap());
        for domain in ["selfplay:read", "project", "project"] {
            let id = store.begin_trajectory(active.id, domain, "test", "hello").unwrap();
            store.finish_trajectory(id, Some("answer"), None).unwrap();
        }
        assert!(due(&cfg, &store).unwrap());
        store.set_meta("auto_enabled", "false").unwrap();
        assert!(!due(&cfg, &store).unwrap());
        store.set_meta("auto_enabled", "true").unwrap();
        store.set_meta("last_cycle_attempt", &improve::now().to_string()).unwrap();
        assert!(!due(&cfg, &store).unwrap());
        store.set_meta("last_cycle_attempt", "0").unwrap();
        store.set_meta("last_cycle_trajectory", "3").unwrap();
        assert!(!due(&cfg, &store).unwrap());
    }
}
