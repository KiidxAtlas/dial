//! Inventory and operator controls for shared compute across all Dial projects.
use crate::{
    config::types::Config,
    storage::{paths::Layout, settings_store, store::Store},
};
use anyhow::{Context, Result, ensure};
use fs2::FileExt;
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

pub fn snapshot(user: &Path) -> Result<Value> {
    let lock = super::auto::lock_file(&user.join("compute.lock"))?;
    let busy = match lock.try_lock_exclusive() {
        Ok(()) => {
            FileExt::unlock(&lock)?;
            false
        }
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => true,
        Err(error) => return Err(error.into()),
    };
    let owner = if busy {
        fs::read(user.join("compute-owner.json"))
            .ok()
            .and_then(|raw| serde_json::from_slice::<Value>(&raw).ok())
            .filter(|value| {
                value["pid"]
                    .as_i64()
                    .and_then(|pid| i32::try_from(pid).ok())
                    .is_some_and(|pid| pid > 1 && unsafe { libc::kill(pid, 0) } == 0)
            })
            .unwrap_or(Value::Null)
    } else {
        Value::Null
    };
    let mut jobs = Vec::new();
    if user.join("projects").is_dir() {
        for entry in fs::read_dir(user.join("projects"))? {
            let project = entry?.path();
            if !project.is_dir() {
                continue;
            }
            let layout = Layout {
                project_root: PathBuf::new(),
                project_dir: PathBuf::new(),
                state_dir: project.clone(),
                user_dir: user.into(),
            };
            let status = super::auto::jobs(&layout)?;
            for mut job in status["project_jobs"]
                .as_array()
                .into_iter()
                .flatten()
                .cloned()
            {
                let id = job["state_id"].as_str().unwrap_or("").to_owned();
                job["state"] = json!(project.join("models").join(&id));
                job["project"] = json!(project.file_name().unwrap_or_default().to_string_lossy());
                if project
                    .join("models")
                    .join(&id)
                    .join("compute.hidden")
                    .exists()
                    && job["control_busy"] != true
                {
                    continue;
                }
                job["active"] = json!(
                    (busy && owner["state"] == job["state"])
                        || (job["control_busy"] == true && job["scheduler"]["state"] == "running")
                );
                jobs.push(job);
            }
        }
    }
    jobs.sort_by_key(|job| job["control_busy"] != true);
    let (processes, session_memory_bytes, session_count, process_error) =
        match super::processes::snapshot_with_usage(user) {
            Ok(usage) => (
                usage.processes,
                usage.session_memory_bytes,
                usage.session_count,
                None,
            ),
            Err(error) => (Vec::new(), 0, 0, Some(error.to_string())),
        };
    Ok(
        json!({"busy":busy,"owner":owner,"background_paused":super::compute::paused(user),"jobs":jobs,
        "processes":processes,"session_memory_bytes":session_memory_bytes,
        "session_count":session_count,"process_error":process_error}),
    )
}

fn state_path(user: &Path, state: &str) -> Result<PathBuf> {
    let root = fs::canonicalize(user.join("projects"))?;
    let state = fs::canonicalize(state)?;
    ensure!(
        state.starts_with(&root)
            && state
                .parent()
                .and_then(Path::file_name)
                .is_some_and(|name| name == "models")
            && state.parent().and_then(Path::parent).and_then(Path::parent) == Some(root.as_path()),
        "job is outside Dial's model state"
    );
    Ok(state)
}

pub fn action(user: &Path, state: Option<&str>, action: &str) -> Result<String> {
    match action {
        "pause" => {
            settings_store::write_private(&user.join("background.pause"), b"paused")?;
            return Ok("Background work paused; coding keeps priority.".into());
        }
        "resume" => {
            match fs::remove_file(user.join("background.pause")) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            };
            return Ok("Background work resumed; coding still has priority.".into());
        }
        _ => {}
    }
    let state = state_path(user, state.context("select a job")?)?;
    let store = Store::open_read_only(&state)?;
    if action == "remove" {
        settings_store::write_private(&state.join("improvement.stop"), b"stop")?;
        // Disable future scheduling, but retain sessions, checkpoints and admitted adapters.
        let lock = super::auto::lock_file(&state.join("control.lock"))?;
        match lock.try_lock_exclusive() {
            Ok(()) => {
                FileExt::unlock(&lock)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                stop_legacy(user, state.to_str().context("invalid state path")?)?;
            }
            Err(error) => return Err(error.into()),
        }
        settings_store::write_private(&state.join("compute.hidden"), b"removed")?;
        return Ok("Job removed and automatic work disabled. Saved models, adapters and history are preserved.".into());
    }
    if action == "stop" {
        settings_store::write_private(&state.join("improvement.stop"), b"stop")?;
        return Ok("Stop requested; this model stays paused until Start is selected.".into());
    }
    ensure!(action == "start", "unknown compute action");
    ensure!(
        !super::compute::paused(user),
        "Resume background work first"
    );
    let request: Value = serde_json::from_slice(
        &fs::read(state.join("background-request.json"))
            .context("No saved job configuration; use Start current model")?,
    )?;
    let mut config: Config = serde_json::from_value(request["config"].clone())?;
    config.state_dir = state.clone();
    config.user_dir = user.into();
    config.validate()?;
    let project = PathBuf::from(request["project"].as_str().context("missing job project")?);
    let layout = Layout::with_user_dir(&project, user.into())?;
    ensure!(
        state.parent().and_then(Path::parent) == Some(layout.state_dir.as_path()),
        "job project does not match its state"
    );
    super::auto::resume(&state)?;
    let _ = store; // The existing worker's control lock is the authority for whether it is running.
    let writable = Store::open(&state, &config.model, &config.use_case, &config.harness)?;
    writable.set_meta("auto_enabled", "true")?;
    config.improve.enabled = true;
    ensure!(
        super::auto::start_cycle(&layout, &config, &writable)?,
        "Job already running or queued"
    );
    Ok("Improvement cycle started; it will yield to coding requests.".into())
}

/// Old workers have no preemption protocol. Stop only a positively identified detached Dial
/// background job, never a foreground chat. Collect children before terminating their parent.
pub fn stop_legacy(user: &Path, state: &str) -> Result<String> {
    let state = state_path(user, state)?;
    let lock = super::auto::lock_file(&state.join("control.lock"))?;
    match lock.try_lock_exclusive() {
        Ok(()) => anyhow::bail!("job is no longer running"),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
        Err(error) => return Err(error.into()),
    }
    let store = Store::open_read_only(&state)?;
    let status: Value =
        serde_json::from_str(&store.get_meta("auto_status")?.context("missing job PID")?)?;
    let pid = status["pid"]
        .as_i64()
        .and_then(|p| i32::try_from(p).ok())
        .context("invalid job PID")?;
    ensure!(
        pid > 1 && pid != std::process::id() as i32 && unsafe { libc::getpgid(pid) } == pid,
        "job is not a detached background process"
    );
    let output = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "args="])
        .output()?;
    let args = String::from_utf8_lossy(&output.stdout);
    let tokens = shlex::split(args.trim()).context("cannot verify background process arguments")?;
    let expected = state.join("background-request.json");
    ensure!(
        tokens.windows(3).any(|part| part[0] == "improve"
            && part[1] == "background"
            && Path::new(&part[2]) == expected),
        "PID no longer belongs to this background job"
    );
    action(
        user,
        Some(state.to_str().context("invalid state path")?),
        "stop",
    )?;
    fn children(pid: i32, depth: usize, found: &mut Vec<i32>) {
        if depth >= 8 {
            return;
        }
        if let Ok(output) = Command::new("pgrep")
            .args(["-P", &pid.to_string()])
            .stderr(Stdio::null())
            .output()
        {
            for child in String::from_utf8_lossy(&output.stdout)
                .split_whitespace()
                .filter_map(|v| v.parse::<i32>().ok())
            {
                children(child, depth + 1, found);
                found.push(child);
            }
        }
    }
    let mut descendants = Vec::new();
    children(pid, 0, &mut descendants);
    for child in descendants {
        unsafe {
            if libc::getpgid(child) == child {
                libc::killpg(child, libc::SIGTERM);
            } else {
                libc::kill(child, libc::SIGTERM);
            }
        }
    }
    ensure!(
        unsafe { libc::killpg(pid, libc::SIGTERM) } == 0,
        "could not stop background worker: {}",
        std::io::Error::last_os_error()
    );
    Ok("Termination requested for the background worker and its model processes; the approved adapter is preserved.".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn global_pause_and_resume_do_not_stop_foreground_sessions() {
        let home = tempfile::tempdir().unwrap();
        action(home.path(), None, "pause").unwrap();
        assert!(super::super::compute::paused(home.path()));
        action(home.path(), None, "resume").unwrap();
        assert!(!super::super::compute::paused(home.path()));
    }
    #[test]
    fn stop_is_scoped_to_a_dial_model_and_keeps_admission() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let layout = Layout::with_user_dir(project.path(), home.path().into()).unwrap();
        let state = layout.state_dir.join("models/test");
        let store = Store::open(
            &state,
            "model",
            "test",
            &crate::config::types::Harness::default(),
        )
        .unwrap();
        let version = store.active().unwrap().id;
        action(home.path(), Some(state.to_str().unwrap()), "stop").unwrap();
        assert!(state.join("improvement.stop").exists());
        assert_eq!(store.active().unwrap().id, version);
        assert!(action(home.path(), Some(project.path().to_str().unwrap()), "stop").is_err());
        assert!(!project.path().join("improvement.stop").exists());
        let status = snapshot(home.path()).unwrap();
        assert_eq!(status["jobs"][0]["stop_requested"], true);
        assert_eq!(status["busy"], false);
    }
    #[test]
    fn inventory_includes_jobs_from_other_projects() {
        let home = tempfile::tempdir().unwrap();
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        for (project, model) in [(first.path(), "one"), (second.path(), "two")] {
            let layout = Layout::with_user_dir(project, home.path().into()).unwrap();
            Store::open(
                &layout.state_dir.join("models/test"),
                model,
                "test",
                &crate::config::types::Harness::default(),
            )
            .unwrap();
        }
        let status = snapshot(home.path()).unwrap();
        let jobs = status["jobs"].as_array().unwrap();
        assert_eq!(jobs.len(), 2);
        assert!(jobs.iter().any(|job| job["model"] == "one"));
        assert!(jobs.iter().any(|job| job["model"] == "two"));
        assert_ne!(jobs[0]["project"], jobs[1]["project"]);
    }

    #[test]
    fn immediate_stop_rejects_a_foreground_process() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let layout = Layout::with_user_dir(project.path(), home.path().into()).unwrap();
        let state = layout.state_dir.join("models/test");
        let store = Store::open(
            &state,
            "model",
            "test",
            &crate::config::types::Harness::default(),
        )
        .unwrap();
        store
            .set_meta(
                "auto_status",
                &json!({"pid":std::process::id()}).to_string(),
            )
            .unwrap();
        let lock = super::super::auto::lock_file(&state.join("control.lock")).unwrap();
        lock.lock_exclusive().unwrap();
        let error = stop_legacy(home.path(), state.to_str().unwrap()).unwrap_err();
        assert!(error.to_string().contains("not a detached background"));
        assert!(!state.join("improvement.stop").exists());
    }

    #[test]
    fn stale_owner_is_hidden_when_compute_is_free() {
        let home = tempfile::tempdir().unwrap();
        super::super::compute::register(
            home.path(),
            "session",
            Path::new("project"),
            Path::new("state"),
            "model",
        )
        .unwrap();
        assert_eq!(snapshot(home.path()).unwrap()["owner"], Value::Null);
    }

    #[test]
    fn removing_a_job_disables_scheduling_and_preserves_its_model_store() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let layout = Layout::with_user_dir(project.path(), home.path().into()).unwrap();
        let state = layout.state_dir.join("models/test");
        let store = Store::open(
            &state,
            "model",
            "test",
            &crate::config::types::Harness::default(),
        )
        .unwrap();
        let active = store.active().unwrap().id;
        action(home.path(), Some(state.to_str().unwrap()), "remove").unwrap();
        assert!(state.join("improvement.stop").exists());
        assert!(state.join("compute.hidden").exists());
        assert_eq!(store.active().unwrap().id, active);
        assert!(
            snapshot(home.path()).unwrap()["jobs"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        let mut config = Config::load(&layout, None).unwrap();
        config.state_dir = state.clone();
        config.improve.enabled = true;
        assert!(!super::super::auto::enabled(&config, &store).unwrap());
        super::super::auto::resume(&state).unwrap();
        assert!(!state.join("compute.hidden").exists());
        assert_eq!(
            snapshot(home.path()).unwrap()["jobs"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }
}
