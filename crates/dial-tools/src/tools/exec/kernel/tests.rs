use crate::config::tools::kernel::KernelConfig;

use super::*;
#[test]
fn quote_preserves_shell_metacharacters() {
    assert_eq!(shell_quote("a'b;$(x)"), "'a'\\''b;$(x)'");
}
#[test]
fn deadline_is_operator_bounded() {
    assert_eq!(
        deadline(&json!({"timeout":99}), 5).unwrap(),
        Duration::from_secs(5)
    );
    assert_eq!(
        deadline(&json!({"timeout":0}), 5).unwrap(),
        Duration::from_secs(5)
    );
    assert_eq!(deadline(&json!({"timeout":0}), 0).unwrap(), Duration::ZERO);
    assert_eq!(
        deadline(&json!({"timeout":2}), 0).unwrap(),
        Duration::from_secs(2)
    );
}
#[test]
fn zero_timeout_waits_for_a_delayed_response() {
    let mut p = process();
    let result = p
        .request(
            json!({"code":"import time; time.sleep(0.1); print('done')"}),
            Duration::ZERO,
            |_| unreachable!(),
        )
        .unwrap();
    assert_eq!(result["stdout"], "done\n");
}
fn process() -> NativeProcess {
    let mut command = Command::new("python3");
    command.args(["-u", "-c", PYTHON_DRIVER]);
    NativeProcess::start(command, Arc::new(AtomicI32::new(0))).unwrap()
}
#[test]
fn python_state_stdout_tracebacks_and_reset() {
    let mut p = process();
    let call = |p: &mut NativeProcess, code: &str| {
        p.request(
            json!({"code":code}),
            Duration::from_secs(5),
            |_| unreachable!(),
        )
        .unwrap()
    };
    assert_eq!(call(&mut p, "x = 40\nprint('hello')")["stdout"], "hello\n");
    assert_eq!(call(&mut p, "x + 2")["value"], "42");
    assert!(
        call(&mut p, "1 / 0")["error"]
            .as_str()
            .unwrap()
            .contains("ZeroDivisionError")
    );
    assert_eq!(call(&mut p, "x")["value"], "40");
    drop(p);
    let mut fresh = process();
    assert!(
        call(&mut fresh, "x")["error"]
            .as_str()
            .unwrap()
            .contains("NameError")
    );
}
#[test]
fn endless_python_has_a_deadline() {
    let mut p = process();
    let result = p.request(
        json!({"code":"while True: pass"}),
        Duration::from_millis(100),
        |_| unreachable!(),
    );
    assert!(result.unwrap_err().contains("deadline"));
}
#[test]
fn venv_site_packages_passed_as_argv_are_importable() {
    let site = std::env::temp_dir().join(format!("dial-kernel-site-{}", std::process::id()));
    std::fs::create_dir_all(site.join("dialfixture")).unwrap();
    std::fs::write(site.join("dialfixture").join("__init__.py"), "VALUE = 41\n").unwrap();
    let mut command = Command::new("python3");
    command.args(["-I", "-u", "-c", PYTHON_DRIVER]).arg(&site);
    let mut p = NativeProcess::start(command, Arc::new(AtomicI32::new(0))).unwrap();
    let result = p
        .request(
            json!({"code":"import dialfixture\ndialfixture.VALUE + 1"}),
            Duration::from_secs(5),
            |_| unreachable!(),
        )
        .unwrap();
    drop(p);
    let _ = std::fs::remove_dir_all(&site);
    assert_eq!(result["value"], "42");
}
#[cfg(unix)]
#[test]
fn idle_sigint_keeps_the_namespace_and_the_session() {
    let pid = Arc::new(AtomicI32::new(0));
    let mut p = NativeProcess::start(
        {
            let mut command = Command::new("python3");
            command.args(["-u", "-c", PYTHON_DRIVER]);
            command
        },
        pid.clone(),
    )
    .unwrap();
    let call = |p: &mut NativeProcess, code: &str| {
        p.request(
            json!({"code":code}),
            Duration::from_secs(5),
            |_| unreachable!(),
        )
        .unwrap()
    };
    call(&mut p, "kept = 7");
    std::thread::sleep(Duration::from_millis(100));
    // SAFETY: the pid belongs to the child this test started and has not been reaped.
    unsafe { libc::kill(pid.load(Ordering::Acquire), libc::SIGINT) };
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(call(&mut p, "kept")["value"], "7");
}
#[test]
fn noisy_output_is_bounded() {
    let mut p = process();
    let v = p
        .request(
            json!({"code":"print('x' * 100000)"}),
            Duration::from_secs(5),
            |_| unreachable!(),
        )
        .unwrap();
    assert!(v["stdout"].as_str().unwrap().len() < 25000);
    assert!(v["stdout"].as_str().unwrap().contains("truncated"));
}
#[test]
fn native_output_is_captured_without_protocol_corruption() {
    let mut p = process();
    let v = p
        .request(
            json!({"code":"import os\nos.write(1, b'native-output')"}),
            Duration::from_secs(5),
            |_| unreachable!(),
        )
        .unwrap();
    assert!(
        v["process_output"]
            .as_str()
            .unwrap()
            .contains("native-output")
    );
}
#[cfg(unix)]
#[test]
fn drop_kills_and_reaps_the_process_group_leader() {
    let mut command = Command::new("python3");
    command.args(["-u", "-c", PYTHON_DRIVER]);
    let observed_pid = Arc::new(AtomicI32::new(0));
    let p = NativeProcess::start(command, observed_pid.clone()).unwrap();
    let pid = observed_pid.load(Ordering::Acquire) as libc::pid_t;
    drop(p);
    assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH)
    );
}
#[test]
fn unavailable_sandbox_never_runs_model_code() {
    use crate::tools::{Workspace, exec::sandbox::Sandbox};
    let dir = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(dir.path())
        .unwrap()
        .with_sandbox(Sandbox::Unavailable("test-denied".into()));
    let config = Config::default();
    let ctx = ToolContext {
        workspace: &workspace,
        config: &config,
        mutation_authorizer: None,
        confirm: None,
    };
    let tool = tools(&config).pop().unwrap();
    let out = tool.call(
        &ctx,
        &json!({"action":"eval","code":"open('escaped', 'w').write('no')"}),
    );
    assert!(
        out.is_error
            && (out.text.contains("test-denied") || out.text.contains("process-fork containment")),
        "{}",
        out.text
    );
    assert!(!dir.path().join("escaped").exists());
}
#[cfg(target_os = "macos")]
#[test]
fn cancellation_interrupts_an_inflight_kernel_without_session_lock() {
    let dir = tempfile::tempdir().unwrap();
    // The transport/control test opts out explicitly; confinement has a separate fail-closed test.
    let workspace = crate::tools::Workspace::new(dir.path())
        .unwrap()
        .without_sandbox();
    let config = Config::default();
    let tool = Arc::new(KernelTool {
        config: KernelConfig::default(),
        session: Mutex::new(None),
        pid: Arc::new(AtomicI32::new(0)),
        interrupt: AtomicBool::new(false),
        active: AtomicUsize::new(0),
    });
    let running = tool.clone();
    let thread = std::thread::spawn(move || {
        running.call(
            &ToolContext {
                workspace: &workspace,
                config: &config,
                mutation_authorizer: None,
                confirm: None,
            },
            &json!({"action":"eval","code":"while True: pass"}),
        )
    });
    let end = Instant::now() + Duration::from_secs(5);
    while tool.pid.load(Ordering::Acquire) == 0 && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(tool.pid.load(Ordering::Acquire) > 0);
    tool.cancel_operation();
    assert!(thread.join().unwrap().is_error);
    assert_eq!(tool.pid.load(Ordering::Acquire), 0);
    assert!(tool.session.lock().is_none());
}
#[cfg(target_os = "macos")]
#[test]
fn kernel_denies_fork_spawn_and_detached_descendants_even_without_filesystem_sandbox() {
    if !crate::tools::exec::sandbox::require_for_test() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let workspace = crate::tools::Workspace::new(dir.path())
        .unwrap()
        .without_sandbox();
    let config = Config::default();
    let tool = tools(&config).pop().unwrap();
    let ctx = ToolContext {
        workspace: &workspace,
        config: &config,
        mutation_authorizer: None,
        confirm: None,
    };
    for code in [
        "import subprocess, sys\np = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(60)'], start_new_session=True, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)\np.pid",
        "import os, sys\nos.posix_spawn(sys.executable, [sys.executable, '-c', 'import time; time.sleep(60)'], os.environ)",
        "import os\nos.fork()",
    ] {
        let output = tool.call(&ctx, &json!({"action":"eval","code":code}));
        // Clean up a spawned PID even if containment regresses, before failing the test.
        if !output.is_error {
            let value: Value = serde_json::from_str(&output.text).unwrap();
            if let Some(pid) = value["value"]
                .as_str()
                .and_then(|s| s.parse::<i32>().ok())
                .filter(|pid| *pid > 0)
            {
                unsafe {
                    libc::kill(pid, libc::SIGKILL);
                }
            }
        }
        assert!(
            output.is_error && output.text.contains("PermissionError"),
            "fork containment failed: {}",
            output.text
        );
    }
    assert!(!tool.call(&ctx, &json!({"action":"reset"})).is_error);
}
#[test]
fn expired_callback_operation_never_invokes_the_network_handler() {
    let driver = "import sys,json\nr=json.loads(sys.stdin.readline())\nprint(json.dumps({'callback':'http','callback_id':1,'operation_id':0}),flush=True)\nreply=json.loads(sys.stdin.readline())\nprint(json.dumps({'id':r['id'],'result':reply}),flush=True)";
    let mut command = Command::new("python3");
    command.args(["-u", "-c", driver]);
    let mut process = NativeProcess::start(command, Arc::new(AtomicI32::new(0))).unwrap();
    let value = process
        .request(json!({}), Duration::from_secs(5), |_| {
            panic!("expired callback performed networking")
        })
        .unwrap();
    assert!(
        value["error"]
            .as_str()
            .unwrap()
            .contains("expired operation")
    );
}
#[cfg(unix)]
#[test]
fn workspace_python_launcher_is_rejected_before_execution() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let marker = outside.path().join("escaped");
    let launcher = dir.path().join("python3");
    std::fs::write(
        &launcher,
        format!(
            "#!/bin/sh\nprintf escaped > {}\n",
            shell_quote(marker.to_str().unwrap())
        ),
    )
    .unwrap();
    std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o755)).unwrap();
    let root = dir.path().canonicalize().unwrap();
    let error = resolve_python(
        &root,
        launcher.to_str().unwrap(),
        Duration::from_secs(5),
        Arc::new(AtomicI32::new(0)),
        &AtomicBool::new(false),
    )
    .unwrap_err();
    assert!(error.contains("outside the editable workspace"));
    assert!(!marker.exists());
}

#[test]
fn persistent_reply_observation_and_retention_do_not_invent_child_success() {
    use crate::runtime::execution::Completeness;
    let out = code_output(Ok(json!({"stdout":"abc", "error":null,
        "output_integrity":{"original_bytes":9,"retained_bytes":3}})));
    assert!(!out.is_error);
    assert!(out.execution.observation.unwrap().satisfied);
    assert!(
        out.execution.process.is_none(),
        "a persistent interpreter has not exited"
    );
    let integrity = out.execution.output.unwrap();
    assert_eq!(integrity.capture, Completeness::Unknown);
    assert_eq!(integrity.retention, Completeness::Partial);
    assert_eq!(integrity.omitted_bytes, Some(6));
    let failed = code_output(Err("transport disconnected".into()));
    assert!(!failed.execution.observation.unwrap().satisfied);
    assert_eq!(
        failed.execution.output.unwrap().capture,
        Completeness::Unknown
    );
}
