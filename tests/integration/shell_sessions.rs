//! Named `bash` sessions moved to a background job, run through the real embedded shell host.
//! The host is the `dial` binary itself, so this behavior is only observable end to end.
use dial::config::types::{Config, ShellBackend};
use dial::tools::Workspace;
use dial::tools::exec::background::AdoptedJob;
use dial::tools::exec::shell::{ForegroundJob, bash_tool_streaming_with_adopter};
use dial::tools::integrations::ext::ToolContext;
use serde_json::json;
use std::sync::atomic::AtomicBool;

#[cfg(unix)]
#[test]
fn named_session_restores_cwd_and_env_after_a_job_and_refuses_while_busy() {
    // Set the embedded-host override before the child test process starts, never by
    // mutating the environment of this multithreaded integration-test process.
    const CHILD: &str = "DIAL_TEST_NAMED_SESSION_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "shell_sessions::named_session_restores_cwd_and_env_after_a_job_and_refuses_while_busy", "--nocapture"])
            .env(CHILD, "1")
            .env(dial::embedded_shell::HOST_EXE_ENV, env!("CARGO_BIN_EXE_dial"))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "isolated named-session test: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("src")).unwrap();
    let ws = Workspace::new(dir.path())
        .unwrap()
        .without_sandbox()
        .with_shell_backend(ShellBackend::Embedded);
    let config = Config::default();
    let bg = dial::tools::exec::background::tools(&config)
        .into_iter()
        .next()
        .expect("the background tool is registered");
    let ctx = ToolContext {
        workspace: &ws,
        config: &config,
        mutation_authorizer: None,
        confirm: None,
    };
    let cancel = AtomicBool::new(false);
    let adopt = |job: ForegroundJob| bg.adopt_job(AdoptedJob::Shell(Box::new(job)));
    let run = |args: serde_json::Value| {
        bash_tool_streaming_with_adopter(&ws, &args, 0, 0, &|_: &[u8]| {}, &cancel, &adopt)
    };

    let started = run(json!({
        "command": "cd src && export NAMED_VAR=kept && sleep 2",
        "session": "s1",
        "background": true
    }))
    .unwrap();
    let job = started
        .text
        .split("background job ")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .expect("the command reports its background job")
        .trim_end_matches(|c: char| !c.is_alphanumeric())
        .to_owned();

    let busy =
        run(json!({"command": "pwd", "session": "s1"})).expect_err("a busy session is refused");
    assert!(
        busy.contains(&format!("session busy in background job {job}")),
        "{busy}"
    );

    let waited = bg.call(&ctx, &json!({"action": "wait", "name": job, "timeout": 30}));
    assert!(!waited.is_error, "{}", waited.text);

    let restored = run(json!({"command": "pwd; echo $NAMED_VAR", "session": "s1"})).unwrap();
    assert!(restored.text.contains("/src"), "{}", restored.text);
    assert!(restored.text.contains("kept"), "{}", restored.text);

    let failed = run(json!({
        "command": "cd .. && export NAMED_VAR=retained_after_failure && false",
        "session": "s1",
        "background": true
    }))
    .unwrap();
    let failed_job = failed
        .text
        .split("background job ")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .expect("the failed command reports its background job")
        .trim_end_matches(|c: char| !c.is_alphanumeric());
    let waited = bg.call(
        &ctx,
        &json!({"action": "wait", "name": failed_job, "timeout": 30}),
    );
    assert!(!waited.is_error, "{}", waited.text);
    assert!(waited.text.contains("exit code 1"), "{}", waited.text);

    let restored = run(json!({"command": "pwd; echo $NAMED_VAR", "session": "s1"})).unwrap();
    assert!(
        restored.text.contains(dir.path().to_str().unwrap()),
        "{}",
        restored.text
    );
    assert!(!restored.text.contains("/src"), "{}", restored.text);
    assert!(
        restored.text.contains("retained_after_failure"),
        "{}",
        restored.text
    );
}

#[cfg(unix)]
#[test]
fn named_state_saves_only_changed_exports_with_a_large_inherited_environment() {
    use dial::embedded_shell::state::{Exchange, State};
    use std::process::Command;

    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().canonicalize().unwrap();
    let mut seed = State::new(&cwd);
    seed.exported.insert("SEEDED_VAR".into(), "retained".into());
    let exchange = Exchange::new(dir.path(), &seed).unwrap();
    let script = dir.path().join("state.sh");
    let inherited: Vec<_> = (0..300)
        .map(|i| (format!("CI_FILLER_{i}"), format!("value-{i}")))
        .collect();
    let run = || {
        let output = Command::new(env!("CARGO_BIN_EXE_dial"))
            .arg(dial::embedded_shell::HOST_ARG)
            .arg(&script)
            .arg(exchange.path())
            .current_dir(&cwd)
            .envs(inherited.iter().cloned())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "host failed: {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
        let state = exchange.read().unwrap();
        assert!(state.completed);
        assert_eq!(state.exported["SEEDED_VAR"], "retained");
        assert_eq!(state.exported["NAMED_VAR"], "kept");
        assert_eq!(state.exported["CI_FILLER_2"], "changed");
        assert!(
            !state.exported.contains_key("CI_FILLER_1"),
            "an unchanged inherited variable consumed the state budget"
        );
    };

    std::fs::write(&script, "export NAMED_VAR=kept CI_FILLER_2=changed\n").unwrap();
    run();
    // The next command must retain the previous command's delta, not just new changes.
    std::fs::write(&script, "test \"$NAMED_VAR\" = kept\n").unwrap();
    run();
}

/// Unit fixtures re-execute a test helper; this checks the shipping binary's private entry point.
#[cfg(unix)]
#[test]
fn durable_owner_entry_point_runs_once_and_publishes_a_terminal_observation() {
    use std::{
        io::Write,
        os::unix::fs::PermissionsExt,
        process::{Command, Stdio},
        time::{Duration, Instant},
    };
    let temp = tempfile::Builder::new()
        .prefix("dial-owner-")
        .tempdir_in("/tmp")
        .unwrap();
    let root = temp.path().canonicalize().unwrap();
    let directory = root.join("owner");
    std::fs::create_dir(&directory).unwrap();
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
    let launch = json!({
        "directory":directory, "root":root, "roots":[], "strict":false, "deny":[],
        "writes":[], "network":false, "unsandboxed":true, "backend":ShellBackend::System,
        "cargo_session":null, "artifacts":null, "lifetime":3,
        "args":{"name":"job","command":"printf x >> once; printf ready; printf done"}
    });
    let mut child = Command::new(env!("CARGO_BIN_EXE_dial"))
        .arg(dial::tools::exec::background::durable::OWNER_ARG)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(&serde_json::to_vec(&launch).unwrap())
        .unwrap();
    let end = Instant::now() + Duration::from_secs(8);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= end {
            let _ = child.kill();
            let _ = child.wait();
            panic!("production durable owner did not terminate within the fixture deadline");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    assert!(
        status.success(),
        "production durable owner failed: {status}"
    );
    let observation: serde_json::Value =
        serde_json::from_slice(&std::fs::read(directory.join("finished.json")).unwrap()).unwrap();
    assert_eq!(observation["is_error"], false, "{observation}");
    assert_eq!(
        observation["execution"]["process"]["exit_code"], 0,
        "{observation}"
    );
    assert!(
        observation["text"].as_str().unwrap().contains("readydone"),
        "{observation}"
    );
    assert_eq!(std::fs::read(root.join("once")).unwrap(), b"x");
}
