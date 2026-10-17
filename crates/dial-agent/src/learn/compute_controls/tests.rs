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
