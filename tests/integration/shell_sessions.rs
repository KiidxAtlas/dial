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
    // Only this suite runs the embedded shell host, so pointing the process-wide override at the
    // built binary affects nothing else.
    unsafe {
        std::env::set_var(
            dial::embedded_shell::HOST_EXE_ENV,
            env!("CARGO_BIN_EXE_dial"),
        )
    };
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
