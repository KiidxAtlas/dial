use super::*;

#[test]
fn unsupported_action_is_refused_before_tooling_lookup() {
    let error = handle(Path::new("/nonexistent-benchmark-project"), "run").unwrap_err();
    assert!(error.to_string().contains("expected open, status or stop"));
}

#[test]
fn checkout_discovers_tooling_from_nested_project() {
    if std::env::var_os("DIAL_BENCHMARK_ROOT").is_some() {
        return;
    }
    let temporary = tempfile::tempdir().unwrap();
    let tools = temporary.path().join("examples/benchmarks");
    std::fs::create_dir_all(&tools).unwrap();
    std::fs::write(tools.join("benchmark_dashboard.py"), "# fixture").unwrap();
    let nested = temporary.path().join("nested/project");
    std::fs::create_dir_all(&nested).unwrap();
    assert_eq!(
        checkout(&nested).unwrap(),
        temporary.path().canonicalize().unwrap()
    );
    assert_eq!(handle(&nested, "status").unwrap()["status"], "closed");
}

#[cfg(unix)]
#[test]
fn stop_kills_an_unresponsive_workspace_and_forgets_it() {
    if std::env::var_os("DIAL_BENCHMARK_ROOT").is_some() {
        return;
    }
    let temporary = tempfile::tempdir().unwrap();
    let tools = temporary.path().join("examples/benchmarks");
    std::fs::create_dir_all(&tools).unwrap();
    std::fs::write(tools.join("benchmark_dashboard.py"), "# fixture").unwrap();
    let root = checkout(temporary.path()).unwrap();
    assert_eq!(handle(&root, "stop").unwrap()["status"], "closed");
    let child = Command::new("sleep").arg("30").spawn().unwrap();
    let pid = child.id();
    WORKSPACES
        .get_or_init(Mutex::default)
        .lock()
        .unwrap()
        .insert(
            root.clone(),
            Workspace {
                child,
                // Port 1 refuses connections, like a runner that stopped answering.
                url: "http://127.0.0.1:1/token/".into(),
            },
        );
    assert_eq!(handle(&root, "stop").unwrap()["status"], "stopped");
    assert_eq!(handle(&root, "status").unwrap()["status"], "closed");
    let alive = Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .unwrap()
        .success();
    assert!(
        !alive,
        "unresponsive workspace process {pid} was left running"
    );
}
