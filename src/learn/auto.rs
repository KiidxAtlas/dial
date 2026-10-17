//! Automatic background scheduling. Training and admission run outside the foreground session.
use super::improve;
use crate::tools::background::procinfo::ProcId;
use crate::{
    agent::Runtime,
    config::types::Config,
    storage::{paths::Layout, store::Store},
};
use anyhow::{Context, Result, ensure};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

#[derive(Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
enum Operation {
    #[default]
    Cycle,
    Train,
}

#[derive(Serialize, Deserialize)]
struct Request {
    config: Config,
    project: PathBuf,
    state: PathBuf,
    user: PathBuf,
    #[serde(default)]
    force: bool,
    #[serde(default)]
    operation: Operation,
}

pub fn lock_file(path: &Path) -> Result<File> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    Ok(crate::platform::fs::open_owner_only(&options, path)?)
}

/// Foreground sessions hold `compute.lock` shared, so any number of local-model and Laya sessions
/// run together. Only background training/maintenance takes it exclusively, and it yields first.
pub fn compute_lock(layout: &Layout) -> Result<File> {
    let _request = super::compute::Request::foreground(&layout.user_dir)?;
    let file = lock_file(&layout.user_dir.join("compute.lock"))?;
    if fs2::FileExt::try_lock_shared(&file).is_err() {
        eprintln!("dial: waiting for local compute (a background improvement job is yielding)");
        fs2::FileExt::lock_shared(&file)?;
    }
    super::compute::register(
        &layout.user_dir,
        "session",
        &layout.project_root,
        &layout.state_dir,
        "",
    )?;
    Ok(file)
}

/// Persistent operator stop: checked between cases and inside training subprocess polling.
pub fn check_stop(config: &Config) -> Result<()> {
    ensure!(
        !config.state_dir.join("improvement.stop").try_exists()?,
        "improvement stopped by operator"
    );
    // A running background job also stops when today's compute budget is spent.
    if super::compute::background_active() {
        let used = super::resources::used_today_checked(&config.user_dir)?
            + super::compute::lease_elapsed_secs();
        if super::resources::budget_left(&config.improve, used) == Some(0) {
            return Err(BudgetReached.into());
        }
    }
    Ok(())
}

/// The daily background-compute budget ran out; the cycle ends and resumes when it is next due.
#[derive(Debug)]
pub struct BudgetReached;
impl std::fmt::Display for BudgetReached {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("daily background budget reached (improve.daily_compute_minutes for local compute, or improve.max_daily_usd for endpoint spend); the cycle pauses until tomorrow")
    }
}
impl std::error::Error for BudgetReached {}

pub fn resume(state: &Path) -> Result<()> {
    for name in ["improvement.stop", "compute.hidden"] {
        match fs::remove_file(state.join(name)) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

pub fn request_stop(layout: &Layout, id: &str) -> Result<Value> {
    ensure!(
        !id.is_empty() && id.bytes().all(|b| b.is_ascii_hexdigit()),
        "use a model state ID from improve jobs"
    );
    let root = fs::canonicalize(layout.state_dir.join("models"))?;
    let state = fs::canonicalize(root.join(id))?;
    ensure!(
        state.parent() == Some(root.as_path()),
        "model state is outside this project"
    );
    let store = Store::open_read_only(&state)?;
    let status: Value = store
        .get_meta("auto_status")?
        .and_then(|v| serde_json::from_str(&v).ok())
        .unwrap_or(Value::Null);
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    crate::platform::fs::open_owner_only(&options, &state.join("improvement.stop"))?.sync_all()?;
    Ok(
        json!({"state_id":id,"stop_requested":true,"automatic_paused":true,
        "running_worker_supports_stop":status["stop_supported"] == true,
        "message":if status["stop_supported"] == true { "Stop requested. Training subprocesses cancel promptly; inference/evaluation stops after the current case. Run improve jobs to confirm the control lock is released." }
        else { "Future cycles are paused in updated Dial. This older worker cannot acknowledge stop requests; it must finish or be stopped separately." }}),
    )
}

/// Project-wide observational status. A busy control lock proves work is still active;
/// persisted scheduler state alone does not. The compute lock is shared across projects.
pub fn jobs(layout: &Layout) -> Result<Value> {
    fn busy(path: &Path) -> Result<bool> {
        let file = match OpenOptions::new().read(true).write(true).open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        match file.try_lock_exclusive() {
            Ok(()) => Ok(false),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Ok(true),
            Err(error) => Err(error.into()),
        }
    }
    fn log_tail(path: &Path) -> Option<String> {
        use std::io::{Read, Seek, SeekFrom};
        let mut file = File::open(path).ok()?;
        let len = file.metadata().ok()?.len();
        file.seek(SeekFrom::Start(len.saturating_sub(4096))).ok()?;
        let mut bytes = Vec::new();
        file.take(4096).read_to_end(&mut bytes).ok()?;
        let text = String::from_utf8_lossy(&bytes);
        let lines: Vec<_> = text.lines().collect();
        Some(lines[lines.len().saturating_sub(5)..].join("\n"))
    }
    let mut entries = Vec::new();
    let models = layout.state_dir.join("models");
    if models.is_dir() {
        for entry in fs::read_dir(models)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let state = entry.path();
            let record = (|| -> Result<Value> {
                let store = Store::open_read_only(&state)?;
                let meta = |key| -> Result<Value> {
                    Ok(store
                        .get_meta(key)?
                        .and_then(|raw| serde_json::from_str(&raw).ok())
                        .unwrap_or(Value::Null))
                };
                Ok(
                    json!({"state_id":entry.file_name().to_string_lossy(),"model":store.get_meta("model")?,
                    "control_busy":busy(&state.join("control.lock"))?,"scheduler":meta("auto_status")?,
                    "progress":meta("cycle_progress")?,"training":meta("training_progress")?,"lora_phase":store.get_meta("lora_phase")?,
                    "stop_requested":state.join("improvement.stop").try_exists()?,"log":state.join("improvement.log"),
                    "recent_log":log_tail(&state.join("improvement.log"))}),
                )
            })();
            entries.push(record.unwrap_or_else(|error| json!({"state_id":entry.file_name().to_string_lossy(),"error":error.to_string()})));
        }
    }
    entries.sort_by_key(|entry| entry["state_id"].as_str().unwrap_or_default().to_owned());
    Ok(
        json!({"shared_compute_busy":busy(&layout.user_dir.join("compute.lock"))?,"project_jobs":entries}),
    )
}

pub fn jobs_summary(layout: &Layout) -> Result<String> {
    let status = jobs(layout)?;
    let mut text = format!(
        "Shared local compute: {}",
        if status["shared_compute_busy"] == true {
            "busy (all sessions and projects)"
        } else {
            "available"
        }
    );
    let mut active = 0;
    for job in status["project_jobs"].as_array().into_iter().flatten() {
        if let Some(error) = job["error"].as_str() {
            text.push_str(&format!(
                "\n{}: status unavailable ({error})",
                job["state_id"].as_str().unwrap_or("unknown")
            ));
            continue;
        }
        if job["control_busy"] != true {
            continue;
        }
        active += 1;
        let stage = job["progress"]["stage"]
            .as_str()
            .or_else(|| {
                job["recent_log"]
                    .as_str()
                    .and_then(|log| log.lines().last())
            })
            .unwrap_or("stage unavailable");
        text.push_str(&format!(
            "\nActive work: {} · PID {}\nStage: {stage}\nStop: /improve stop {}{}",
            job["model"].as_str().unwrap_or("unknown"),
            job["scheduler"]["pid"],
            job["state_id"].as_str().unwrap_or("unknown"),
            if job["stop_requested"] == true {
                " (stop already requested)"
            } else {
                ""
            }
        ));
        if job["scheduler"]["stop_supported"] != true {
            text.push_str("\nOlder worker: cannot acknowledge the new stop command.");
        }
    }
    if active == 0 {
        text.push_str("\nNo active improvement job found in this project. Other sessions or verification may hold compute.");
    }
    Ok(text)
}

pub fn enabled(config: &Config, store: &Store) -> Result<bool> {
    Ok(config.improve.enabled && runtime_enabled(config, store)?)
}

/// Operator stop and the stored automatic switch. Unlike `enabled`, ignores `config.improve.enabled`
/// so a forced background run is not refused by a stale configuration snapshot.
fn runtime_enabled(config: &Config, store: &Store) -> Result<bool> {
    Ok(!config.state_dir.join("improvement.stop").try_exists()?
        && store.get_meta("auto_enabled")?.as_deref() != Some("false"))
}

pub fn due(config: &Config, store: &Store) -> Result<bool> {
    if !enabled(config, store)?
        || !improve::due(store, "last_cycle_attempt", config.improve.interval_secs)?
    {
        return Ok(false);
    }
    let watermark = store
        .get_meta("last_cycle_trajectory")?
        .map(|v| v.parse::<i64>())
        .transpose()?
        .unwrap_or(0);
    Ok(store.live_turns_after(watermark)? >= config.improve.min_new_turns as usize)
}

pub fn remaining(store: &Store, key: &str, interval: u64, now: u64) -> Result<u64> {
    let last = store
        .get_meta(key)?
        .map(|value| value.parse::<u64>())
        .transpose()?
        .unwrap_or(0);
    Ok(interval.saturating_sub(now.saturating_sub(last)))
}

pub fn duration(seconds: u64) -> String {
    format!(
        "{}h {:02}m {:02}s",
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60
    )
}

pub fn readiness(config: &Config, store: &Store) -> Result<String> {
    if !enabled(config, store)? {
        return Ok("automatic improvement paused".into());
    }
    let mut gates = Vec::new();
    if super::compute::paused(&config.user_dir) {
        gates.push("background work paused by /compute (resume it to continue)".into());
    }
    if !config.improve.background {
        gates.push(format!(
            "background improvement is off (turn it on in {})",
            improve::SETTINGS_PATH
        ));
    }
    if !config.uses_local_compute() && config.improve.max_daily_usd <= 0.0 {
        gates.push(format!(
            "a cloud model needs a daily spend limit for background cycles (set it in {})",
            improve::SETTINGS_PATH
        ));
    }
    let wait = remaining(
        store,
        "last_cycle_attempt",
        config.improve.interval_secs,
        improve::now(),
    )?;
    let watermark = store
        .get_meta("last_cycle_trajectory")?
        .map(|v| v.parse::<i64>())
        .transpose()?
        .unwrap_or(0);
    let turns = store.live_turns_after(watermark)?;
    if wait > 0 {
        gates.push(format!("cooldown {} remaining", duration(wait)));
    }
    if turns < config.improve.min_new_turns as usize {
        gates.push(format!(
            "{turns}/{} new turns recorded",
            config.improve.min_new_turns
        ));
    }
    if let Some(why) = super::resources::describe(&config.improve, &config.user_dir) {
        gates.push(format!("waiting for a free machine: {why}"));
    }
    if gates.is_empty() {
        Ok("eligible at the next scheduling check".into())
    } else {
        Ok(gates.join("; "))
    }
}

pub fn maybe_spawn(layout: &Layout, config: &Config, store: &Store) -> Result<Spawn> {
    spawn_job(layout, config, store, false, Operation::Cycle)
}
pub fn start_cycle(layout: &Layout, config: &Config, store: &Store) -> Result<Spawn> {
    spawn_job(layout, config, store, true, Operation::Cycle)
}
pub fn start_training(layout: &Layout, config: &Config, store: &Store) -> Result<Spawn> {
    ensure!(
        config.training.enabled,
        "training is disabled; enable it in settings"
    );
    ensure!(
        config.runs_dial_mlx() && !config.model.is_empty(),
        "training requires Dial's local model runtime and a local MLX checkpoint"
    );
    if let Ok(resolved) = crate::llm::catalog::resolve_local_model(&config.model) {
        let format = crate::llm::local_format::detect(std::path::Path::new(&resolved));
        ensure!(
            format.is_none() || format == Some(crate::llm::local_format::LocalFormat::Mlx),
            "training needs MLX weights; {} models can be served and evaluated but not LoRA-trained",
            format.map_or("this", |f| f.label())
        );
    }
    ensure!(
        runtime_enabled(config, store)?,
        "Improvement is paused; use /improve enable first"
    );
    spawn_job(layout, config, store, true, Operation::Train)
}

/// Why a background job was not started. The operator sees the cause, not a generic "busy".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// `/compute` paused background work.
    Paused,
    /// The control lock is held: a worker is running a cycle or training.
    Running,
    /// The launch lock is held, or a worker was launched in the last 30 seconds.
    Launching,
    /// A scheduled start found the cycle not due, the machine busy, or background work off.
    NotDue,
}

impl Refusal {
    /// Operator-facing explanation. `state` is the model's state directory holding the lock files.
    pub fn message(self, state: &Path) -> String {
        match self {
            Self::Paused => "background work is paused by /compute; resume it first".into(),
            Self::Running => "an improvement cycle or training run is already running (it holds the control lock). Check /improve jobs; /improve stop <id> ends it safely.".into(),
            Self::Launching => format!(
                "a worker was just launched and has not taken its lock yet; try again in about 30 seconds. If /improve jobs shows nothing active for a minute, no worker holds {}, so the lock is stale and retrying is safe.",
                state.join("launch.lock").display()
            ),
            Self::NotDue => "no cycle is due yet (cooldown, too few new turns, background work off, or the machine is busy). /improve status shows which.".into(),
        }
    }
}

/// Outcome of asking for a background job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Spawn {
    Started,
    Refused(Refusal),
}

impl Spawn {
    /// Turns a refusal into an error that names its cause, for operator-initiated starts.
    pub fn ensure_started(self, state: &Path) -> Result<()> {
        match self {
            Self::Started => Ok(()),
            Self::Refused(reason) => anyhow::bail!("{}", reason.message(state)),
        }
    }
}

fn spawn_job(
    layout: &Layout,
    config: &Config,
    store: &Store,
    force: bool,
    operation: Operation,
) -> Result<Spawn> {
    if super::compute::paused(&layout.user_dir) {
        return Ok(Spawn::Refused(Refusal::Paused));
    }
    if !force
        && (!config.improve.background
            || (!config.uses_local_compute() && config.improve.max_daily_usd <= 0.0)
            || !due(config, store)?
            || super::resources::describe(&config.improve, &layout.user_dir).is_some())
    {
        return Ok(Spawn::Refused(Refusal::NotDue));
    }
    let control = lock_file(&config.state_dir.join("control.lock"))?;
    if control.try_lock_exclusive().is_err() {
        return Ok(Spawn::Refused(Refusal::Running));
    }
    let launch = lock_file(&config.state_dir.join("launch.lock"))?;
    if launch.try_lock_exclusive().is_err()
        || (!force && !improve::due(store, "last_background_spawn", 30)?)
    {
        return Ok(Spawn::Refused(Refusal::Launching));
    }
    let request_path = config.state_dir.join("background-request.json");
    let mut request_config = config.clone();
    request_config.model = store
        .get_meta("model")?
        .context("store model binding missing")?;
    let request = Request {
        config: request_config,
        project: layout.project_root.clone(),
        state: config.state_dir.clone(),
        user: layout.user_dir.clone(),
        force,
        operation,
    };
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    let mut file = crate::platform::fs::open_owner_only(&options, &request_path)?;
    serde_json::to_writer(&mut file, &request)?;
    file.sync_all()?;
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    let log =
        crate::platform::fs::open_owner_only(&options, &config.state_dir.join("improvement.log"))?;
    let mut command = Command::new(std::env::current_exe()?);
    command
        .arg("-C")
        .arg(&layout.project_root)
        .arg("improve")
        .arg("background")
        .arg(&request_path)
        .env("DIAL_HOME", &layout.user_dir)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    let mut child = crate::platform::process::spawn_group(&mut command)
        .context("starting automatic improvement worker")?;
    let pid = child.id();
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    store.set_meta("last_background_spawn", &improve::now().to_string())?;
    store.set_meta(
        "auto_status",
        &json!({"state":"queued","stop_supported":true,"pid":pid,"proc":ProcId::of(pid as i32),"at":improve::now()})
            .to_string(),
    )?;
    Ok(Spawn::Started)
}

pub fn background(path: &Path) -> Result<()> {
    ensure!(
        fs::metadata(path)?.len() <= 1024 * 1024,
        "background request exceeds limit"
    );
    let request: Request = serde_json::from_slice(&fs::read(path)?)?;
    let layout = Layout::with_user_dir(&request.project, request.user)?;
    let mut config = request.config;
    ensure!(
        config.learning_dir(&layout) == request.state,
        "background state directory mismatch"
    );
    config.state_dir = request.state;
    config.project_root = layout.project_root.clone();
    config.user_dir = layout.user_dir.clone();
    config.validate()?;
    let store = Store::open(
        &config.state_dir,
        &config.model,
        &config.use_case,
        &config.harness,
    )?;
    let control = lock_file(&config.state_dir.join("control.lock"))?;
    control.lock_exclusive()?;
    let current = Config::load(&layout, None)?;
    config.compute_locks = current.compute_locks;
    if !request.force {
        // The operator's current setting decides; the request snapshot may predate a change to it.
        config.improve.enabled = current.improve.enabled;
    }
    if !request.force && !due(&config, &store)? {
        return Ok(());
    }
    let _compute = if config.uses_local_compute() {
        match super::compute::Background::acquire(&config) {
            Ok(lease) => Some(lease),
            Err(_) if check_stop(&config).is_err() => {
                store.set_meta(
                    "auto_status",
                    &json!({"state":"stopped","at":improve::now()}).to_string(),
                )?;
                return Ok(());
            }
            Err(error) => return Err(error),
        }
    } else {
        None
    };
    if (!request.force && !current.improve.enabled) || !runtime_enabled(&config, &store)? {
        store.set_meta(
            "auto_status",
            &json!({"state":"disabled","at":improve::now()}).to_string(),
        )?;
        return Ok(());
    }
    execute_job(
        &mut config,
        &store,
        request.force,
        request.operation,
        &mut |s| eprintln!("{s}"),
    )?;
    Ok(())
}

/// Caller holds control + compute locks. Failures are durable and rate-limited, not swallowed.
pub fn execute(
    config: &mut Config,
    store: &Store,
    force: bool,
    progress: &mut dyn FnMut(&str),
) -> Result<Value> {
    execute_job(config, store, force, Operation::Cycle, progress)
}

fn execute_job(
    config: &mut Config,
    store: &Store,
    force: bool,
    operation: Operation,
    progress: &mut dyn FnMut(&str),
) -> Result<Value> {
    check_stop(config)?;
    let now = improve::now();
    if operation == Operation::Cycle {
        store.set_meta("last_cycle_attempt", &now.to_string())?;
    }
    store.set_meta(
        "auto_status",
        &json!({"state":"running","stop_supported":true,"pid":std::process::id(),"proc":ProcId::current(),"at":now})
            .to_string(),
    )?;
    let watermark = store.max_trajectory()?;
    store.set_meta(
        "cycle_progress",
        &json!({"stage":"preparing models","at":now,"started":now}).to_string(),
    )?;
    let timing = std::cell::RefCell::new(super::resources::Timing::default());
    let mut tracked_progress = |stage: &str| {
        timing.borrow_mut().enter(stage);
        let _ = store.set_meta(
            "cycle_progress",
            &json!({"stage":stage,"at":improve::now(),"started":now}).to_string(),
        );
        progress(stage);
    };
    let result = (|| {
        improve::pin_models(config, store, operation == Operation::Cycle)?;
        let mut runtime = Runtime::new();
        match operation {
            Operation::Cycle => {
                improve::run_cycle(&mut runtime, config, store, force, &mut tracked_progress)
            }
            Operation::Train => {
                let proposal = crate::config::types::Proposal {
                    diagnosis: "Operator-requested training from verified experience".into(),
                    operation: "lora".into(),
                    harness: None,
                };
                let admission = improve::apply(
                    &mut runtime,
                    config,
                    store,
                    &proposal,
                    &mut tracked_progress,
                )?;
                Ok(
                    json!({"admitted": admission.passed, "active": store.active()?.id,
                    "admission": admission}),
                )
            }
        }
    })();
    let _ = store.set_meta(
        "last_cycle_timing",
        &timing.into_inner().finish().to_string(),
    );
    match &result {
        Ok(report) => {
            if operation == Operation::Cycle {
                store.set_meta("last_cycle_trajectory", &watermark.to_string())?;
            }
            store.set_meta("auto_status", &json!({"state":"idle","at":improve::now(),"admitted":report["admitted"],"active":report["active"]}).to_string())?;
        }
        Err(error) if error.downcast_ref::<BudgetReached>().is_some() => store.set_meta("auto_status", &json!({"state":"paused","at":improve::now(),"error":error.to_string()}).to_string())?,
        Err(error) => store.set_meta("auto_status", &json!({"state":if config.state_dir.join("improvement.stop").exists() { "stopped" } else { "failed" },"at":improve::now(),"error":format!("{error:#}")}).to_string())?,
    }
    result
}

pub fn prepare_state(layout: &Layout) -> Result<()> {
    fs::create_dir_all(&layout.state_dir)?;
    crate::platform::fs::set_owner_only(&layout.state_dir)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::types::Harness;

    #[test]
    fn background_request_preserves_training_and_defaults_legacy_to_cycle() {
        let request = Request {
            config: Config::default(),
            project: PathBuf::from("project"),
            state: PathBuf::from("state"),
            user: PathBuf::from("user"),
            force: true,
            operation: Operation::Train,
        };
        let mut value = serde_json::to_value(&request).unwrap();
        let restored: Request = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(restored.operation, Operation::Train);
        assert!(restored.force);
        value.as_object_mut().unwrap().remove("operation");
        let legacy: Request = serde_json::from_value(value).unwrap();
        assert_eq!(legacy.operation, Operation::Cycle);
    }

    #[test]
    fn training_dispatch_uses_lora_gate_and_records_failure_without_cycle_watermark() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), "model", "project", &Harness::default()).unwrap();
        let mut config = Config {
            state_dir: dir.path().into(),
            model: "model".into(),
            ..Config::default()
        };
        // No inference is needed: the manual LoRA gate rejects disabled training first.
        config.endpoint =
            Some(serde_json::from_value(json!({"base_url": "http://localhost:1234/v1"})).unwrap());
        config.training.enabled = false;
        store.set_meta("last_cycle_trajectory", "7").unwrap();
        let active = store.active().unwrap().id;
        let error =
            execute_job(&mut config, &store, true, Operation::Train, &mut |_| {}).unwrap_err();
        assert!(error.to_string().contains("LoRA training is disabled"));
        let status: Value =
            serde_json::from_str(&store.get_meta("auto_status").unwrap().unwrap()).unwrap();
        assert_eq!(status["state"], "failed");
        assert!(
            status["error"]
                .as_str()
                .unwrap()
                .contains("LoRA training is disabled")
        );
        assert_eq!(
            store.get_meta("last_cycle_trajectory").unwrap().as_deref(),
            Some("7")
        );
        assert!(store.get_meta("last_cycle_attempt").unwrap().is_none());
        assert_eq!(store.active().unwrap().id, active);
    }

    #[test]
    fn training_rejects_ineligible_models_and_respects_pause_and_control_lock() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        fs::create_dir(&project).unwrap();
        let layout = Layout::with_user_dir(&project, root.path().join("user")).unwrap();
        let state = layout.state_dir.join("models/abcd");
        let store = Store::open(&state, "model", "project", &Harness::default()).unwrap();
        let mut config = Config {
            state_dir: state.clone(),
            model: "model".into(),
            ..Config::default()
        };
        config.training.enabled = false;
        assert!(
            start_training(&layout, &config, &store)
                .unwrap_err()
                .to_string()
                .contains("disabled")
        );
        config.training.enabled = true;
        config.endpoint = Some(
            serde_json::from_value(json!({
                "base_url": "http://localhost:1234/v1"
            }))
            .unwrap(),
        );
        assert!(
            start_training(&layout, &config, &store)
                .unwrap_err()
                .to_string()
                .contains("local MLX checkpoint")
        );
        config.endpoint = None;
        store.set_meta("auto_enabled", "false").unwrap();
        assert!(
            start_training(&layout, &config, &store)
                .unwrap_err()
                .to_string()
                .contains("paused")
        );
        store.set_meta("auto_enabled", "true").unwrap();
        let control = lock_file(&state.join("control.lock")).unwrap();
        control.lock_exclusive().unwrap();
        assert_eq!(
            start_training(&layout, &config, &store).unwrap(),
            Spawn::Refused(Refusal::Running)
        );
        assert!(Refusal::Running.message(&state).contains("/improve jobs"));
        assert!(Refusal::Launching.message(&state).contains("launch.lock"));
        assert!(!state.join("background-request.json").exists());
        drop(control);
        fs::write(state.join("improvement.stop"), b"stop").unwrap();
        assert!(
            execute_job(
                &mut config,
                &store,
                true,
                Operation::Train,
                &mut |_| panic!("stopped training started")
            )
            .is_err()
        );
        assert!(store.get_meta("last_cycle_attempt").unwrap().is_none());
    }
    #[test]
    fn foreground_sessions_share_compute_but_exclude_background_work() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        fs::create_dir(&project).unwrap();
        let layout = Layout::with_user_dir(&project, root.path().join("user")).unwrap();
        let first = compute_lock(&layout).unwrap();
        let second = compute_lock(&layout).unwrap();
        let probe = lock_file(&layout.user_dir.join("compute.lock")).unwrap();
        assert_eq!(
            probe.try_lock_exclusive().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock,
            "background work cannot start while sessions hold compute"
        );
        drop(first);
        assert!(probe.try_lock_exclusive().is_err());
        drop(second);
        // A process forked concurrently by another test can hold an inherited copy of the
        // descriptor until its exec closes it, so wait briefly instead of asserting at once.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while probe.try_lock_exclusive().is_err() {
            assert!(
                std::time::Instant::now() < deadline,
                "sessions released compute"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
    #[test]
    fn project_jobs_distinguish_locks_from_stale_status_and_stop_pauses() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        fs::create_dir(&project).unwrap();
        let layout = Layout::with_user_dir(&project, root.path().join("user")).unwrap();
        let state = layout.state_dir.join("models/abcd");
        let store = Store::open(&state, "model", "project", &Harness::default()).unwrap();
        let mut config = Config {
            state_dir: state.clone(),
            ..Config::default()
        };
        store
            .set_meta(
                "auto_status",
                r#"{"state":"running","pid":123,"stop_supported":true}"#,
            )
            .unwrap();
        store.set_meta("lora_phase", "paused").unwrap();
        let control = lock_file(&state.join("control.lock")).unwrap();
        control.lock_exclusive().unwrap();
        let compute = compute_lock(&layout).unwrap();
        let status = jobs(&layout).unwrap();
        assert_eq!(status["shared_compute_busy"], true);
        assert_eq!(status["project_jobs"][0]["control_busy"], true);
        assert_eq!(status["project_jobs"][0]["lora_phase"], "paused");
        assert!(request_stop(&layout, "../abcd").is_err());
        assert_eq!(
            request_stop(&layout, "abcd").unwrap()["running_worker_supports_stop"],
            true
        );
        assert!(!enabled(&config, &store).unwrap());
        assert!(check_stop(&config).is_err());
        assert!(
            execute(&mut config, &store, true, &mut |_| panic!(
                "stopped cycle started"
            ))
            .is_err()
        );
        drop(control);
        drop(compute);
        let status = jobs(&layout).unwrap();
        assert_eq!(status["shared_compute_busy"], false);
        assert_eq!(status["project_jobs"][0]["control_busy"], false);
        assert_eq!(status["project_jobs"][0]["scheduler"]["state"], "running");
        resume(&state).unwrap();
        assert!(enabled(&config, &store).unwrap());
        check_stop(&config).unwrap();
        store
            .set_meta("auto_status", r#"{"state":"running","pid":123}"#)
            .unwrap();
        assert_eq!(
            request_stop(&layout, "abcd").unwrap()["running_worker_supports_stop"],
            false
        );
    }

    #[test]
    fn only_new_live_turns_trigger_and_disable_and_backoff_win() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::open(root.path(), "model", "project", &Harness::default()).unwrap();
        let mut cfg = Config::default();
        cfg.improve.min_new_turns = 2;
        let active = store.active().unwrap();
        assert!(!due(&cfg, &store).unwrap());
        for domain in ["selfplay:read", "project", "project"] {
            let id = store
                .begin_trajectory(active.id, domain, "test", "hello")
                .unwrap();
            store.finish_trajectory(id, Some("answer"), None).unwrap();
        }
        assert!(due(&cfg, &store).unwrap());
        store.set_meta("auto_enabled", "false").unwrap();
        assert!(!due(&cfg, &store).unwrap());
        store.set_meta("auto_enabled", "true").unwrap();
        store
            .set_meta("last_cycle_attempt", &improve::now().to_string())
            .unwrap();
        assert!(!due(&cfg, &store).unwrap());
        store.set_meta("last_cycle_attempt", "0").unwrap();
        store.set_meta("last_cycle_trajectory", "3").unwrap();
        assert!(!due(&cfg, &store).unwrap());
    }

    #[test]
    fn countdown_and_readiness_report_each_real_gate() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::open(root.path(), "model", "project", &Harness::default()).unwrap();
        let mut cfg = Config::default();
        // Keep the user dir inside the tempdir so a real ~/.dial pause file cannot leak into the test.
        cfg.user_dir = root.path().join("user");
        // Cloud spend is allowed here, so only the turn-count and cooldown gates apply below.
        cfg.improve.max_daily_usd = 1.0;
        cfg.improve.min_new_turns = 1;
        assert_eq!(readiness(&cfg, &store).unwrap(), "0/1 new turns recorded");
        store.set_meta("last_cycle_attempt", "1000").unwrap();
        assert_eq!(
            remaining(&store, "last_cycle_attempt", 21600, 1001).unwrap(),
            21599
        );
        assert_eq!(
            remaining(&store, "last_cycle_attempt", 21600, 22600).unwrap(),
            0
        );
        assert_eq!(duration(21599), "5h 59m 59s");
        store
            .set_meta("last_cycle_attempt", &improve::now().to_string())
            .unwrap();
        let status = readiness(&cfg, &store).unwrap();
        assert!(status.contains("cooldown") && status.contains("0/1 new turns"));
        let id = store
            .begin_trajectory(store.active().unwrap().id, "project", "test", "hello")
            .unwrap();
        store.finish_trajectory(id, Some("done"), None).unwrap();
        assert!(!readiness(&cfg, &store).unwrap().contains("new turns"));
        store.set_meta("last_cycle_attempt", "0").unwrap();
        assert!(readiness(&cfg, &store).unwrap().contains("eligible"));
        std::fs::create_dir_all(&cfg.user_dir).unwrap();
        std::fs::write(cfg.user_dir.join("background.pause"), b"paused").unwrap();
        assert!(
            readiness(&cfg, &store)
                .unwrap()
                .contains("paused by /compute")
        );
        std::fs::remove_file(cfg.user_dir.join("background.pause")).unwrap();
        cfg.improve.background = false;
        assert!(
            readiness(&cfg, &store)
                .unwrap()
                .contains("background improvement is off")
        );
        cfg.improve.background = true;
        // A cloud main model (no local model string) with no daily budget must say why it will not learn.
        cfg.model = String::new();
        cfg.improve.max_daily_usd = 0.0;
        assert!(
            readiness(&cfg, &store)
                .unwrap()
                .contains("daily spend limit")
        );
        store.set_meta("auto_enabled", "false").unwrap();
        assert_eq!(
            readiness(&cfg, &store).unwrap(),
            "automatic improvement paused"
        );
    }
}
