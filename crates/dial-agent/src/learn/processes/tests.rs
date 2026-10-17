use super::*;
use crate::llm::{
    self,
    server::{ServerProfile, ensure_profile_tracked, fixtures::fake_python},
};
use serde_json::json;
#[cfg(target_os = "macos")]
#[test]
fn macos_reads_real_memory_footprint_and_handles_an_exited_pid() {
    assert!(footprint(std::process::id() as i32).unwrap() > 0);
    assert_eq!(footprint(i32::MAX), None);
}

#[test]
fn inventory_finds_old_orphaned_models_services_and_dial_children_only() {
    let home = tempfile::tempdir().unwrap();
    let text = format!(
        "42 1 42 0.0 35651584 Tue Oct 6 08:00:00 2026 {}/python/.venv/bin/python -m mlx_lm.server --model=big\n43 1 43 1.0 100 Tue Oct 6 08:00:00 2026 {}/laya/venv/bin/python -m uvicorn dial_laya_server:app\n44 1 44 2.0 100 Tue Oct 6 08:00:00 2026 /other/python -m uvicorn other:app\n45 1 45 0.0 100 Tue Oct 6 08:00:00 2026 /bin/dial\n46 45 46 8.0 100 Tue Oct 6 08:00:00 2026 /bin/python -c tool\n",
        home.path().display(),
        home.path().display()
    );
    let all = parse(&text);
    let rows = inventory(home.path(), &all);
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0]["model"], "big");
    assert_eq!(rows[0]["memory_bytes"], 34_u64 * 1024 * 1024 * 1024);
    assert_eq!(rows[0]["orphaned"], true);
    assert!(rows.iter().any(|row| row["kind"] == "services"));
    assert!(
        rows.iter()
            .any(|row| row["pid"] == 46 && row["kind"] == "tools")
    );
    assert!(!rows.iter().any(|row| row["pid"] == 44));
}
#[cfg(unix)]
#[test]
fn registration_exposes_loading_for_custom_python_and_cleans_up() {
    if live_inspection_unavailable() {
        return;
    }
    let home = tempfile::tempdir().unwrap();
    let mut child = Command::new("/bin/sleep").arg("30").spawn().unwrap();
    let pid = child.id();
    let mut record = Record::new(
        home.path(),
        pid,
        "models",
        "test",
        Path::new("project"),
        Path::new("state"),
        "loading",
    )
    .unwrap();
    let phase = |home: &Path| {
        snapshot(home)
            .unwrap()
            .into_iter()
            .find(|row| row["pid"] == pid)
            .unwrap()["phase"]
            .clone()
    };
    assert_eq!(phase(home.path()), "loading");
    record.phase("loaded").unwrap();
    assert_eq!(phase(home.path()), "loaded");
    let path = home
        .path()
        .join("compute-processes")
        .join(format!("{pid}.json"));
    drop(record);
    assert!(!path.exists());
    child.kill().unwrap();
    child.wait().unwrap();
}

#[test]
fn identity_changes_when_a_pid_is_reused_or_command_changes() {
    let first = parse("42 1 42 0 100 Tue Oct 6 08:00:00 2026 /bin/python server");
    let later = parse("42 1 42 0 100 Tue Oct 6 08:01:00 2026 /bin/python server");
    assert_ne!(first[0].identity(), later[0].identity());
}
#[test]
fn process_without_a_verifiable_identity_is_never_selected() {
    let home = tempfile::tempdir().unwrap();
    let all = [Process {
        pid: 42,
        parent: 1,
        #[cfg(unix)]
        group: 42,
        cpu: None,
        memory: None,
        started: "Tue Oct 6 08:00:00 2026".into(),
        command: "/bin/python server".into(),
        id: None,
    }];
    let identity = all[0].identity();
    let error = select(home.path(), &all, 42, &identity).unwrap_err();
    assert!(error.to_string().contains("identity is unavailable"));
}

#[cfg(unix)]
#[test]
fn stop_rechecks_identity_and_terminates_only_the_selected_child() {
    if live_inspection_unavailable() {
        return;
    }
    use std::os::unix::process::CommandExt;
    let home = tempfile::tempdir().unwrap();
    let mut child = Command::new("/bin/sleep")
        .arg("30")
        .process_group(0)
        .spawn()
        .unwrap();
    let _record = Record::new(
        home.path(),
        child.id(),
        "models",
        "test-model",
        Path::new("project"),
        Path::new("state"),
        "loading",
    )
    .unwrap();
    let row = snapshot(home.path())
        .unwrap()
        .into_iter()
        .find(|row| row["pid"] == child.id())
        .unwrap();
    assert!(
        stop(home.path(), child.id() as i32, "stale")
            .unwrap_err()
            .to_string()
            .contains("process changed")
    );
    assert!(child.try_wait().unwrap().is_none());
    stop(
        home.path(),
        child.id() as i32,
        row["identity"].as_str().unwrap(),
    )
    .unwrap();
    assert!(!child.wait().unwrap().success());
    assert!(
        !snapshot(home.path())
            .unwrap()
            .iter()
            .any(|row| row["pid"] == child.id())
    );
}

#[test]
fn tracked_server_exposes_model_phase_and_cleans_up_on_stop() {
    if crate::learn::processes::live_inspection_unavailable() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let python = fake_python(root.path());
    let model_path = root.path().join("model-one");
    let model = model_path.to_str().unwrap();
    let config = crate::config::types::Config {
        user_dir: root.path().join("user"),
        state_dir: root.path().join("state"),
        project_root: root.path().join("project"),
        ..Default::default()
    };
    let mut slot = None;
    let server = ensure_profile_tracked(
        &mut slot,
        ServerProfile {
            python: &python,
            model,
            adapter: None,
            steering: None,
            context: Some(&config),
            runtime: &llm::local_runtime::LocalRuntime::default(),
        },
        30,
        None,
    )
    .unwrap();
    server.phase("generating").unwrap();
    let rows = crate::learn::processes::snapshot(&config.user_dir).unwrap();
    let row = rows.iter().find(|row| row["model"] == model).unwrap();
    assert_eq!(row["phase"], "generating");
    assert_eq!(row["project"], json!(config.project_root));
    let pid = row["pid"].clone();
    server.stop();
    assert!(
        !crate::learn::processes::snapshot(&config.user_dir)
            .unwrap()
            .iter()
            .any(|row| row["pid"] == pid)
    );
}
