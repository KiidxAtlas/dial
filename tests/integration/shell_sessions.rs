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
