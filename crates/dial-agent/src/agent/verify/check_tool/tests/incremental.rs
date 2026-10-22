use super::*;

fn incremental_config(command: &str) -> Config {
    let mut config = Config::default();
    config
        .verification
        .commands
        .push(crate::config::types::VerificationCommand {
            name: "contract".into(),
            command: command.into(),
            file_extensions: vec![],
            timeout_secs: 30,
        });
    config.verification.inputs.insert(
        "contract".into(),
        crate::config::types::VerificationInputs {
            paths: vec!["inputs".into()],
            environment: vec![],
        },
    );
    config
        .verification
        .behavioral_checks
        .push("contract".into());
    config
}

#[test]
fn complete_inputs_reuse_across_unrelated_edits_in_check_and_gate_without_fabricating_runs() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("inputs")).unwrap();
    std::fs::write(root.path().join("inputs/value"), "good").unwrap();
    let command = "test \"$(cat inputs/value)\" = good";
    let config = incremental_config(command);
    let mut agent = hermetic_agent(&config, root.path());
    agent.step_checks.mutations = 1;
    let first = agent.check_tool(&json!({}), &mut NullUi);
    assert!(!first.is_error, "{}", first.text);
    assert_eq!(first.data.as_ref().unwrap()["results"][0]["ran"], true);
    assert_eq!(agent.verifications.len(), 1);

    std::fs::write(root.path().join("README.md"), "unrelated").unwrap();
    agent.step_checks.mutations += 1;
    let reused = agent.check_tool(&json!({}), &mut NullUi);
    assert!(!reused.is_error, "{}", reused.text);
    assert_eq!(reused.data.as_ref().unwrap()["results"][0]["reused"], true);
    assert_eq!(reused.data.as_ref().unwrap()["results"][0]["ran"], false);
    assert_eq!(agent.verifications.len(), 1);
    let evidence = agent.completion_evidence(TurnOutcome::Completed);
    assert_eq!(
        evidence["checks"]["latest"][command]["revision"],
        json!([1, 0, 0])
    );
    assert_eq!(
        evidence["checks"]["latest"][command]["validated_revision"],
        json!([2, 0, 0])
    );
    assert_eq!(evidence["checks"]["passing_behavioral_checks"], 1);

    agent.step_checks.mutations += 1;
    let checks = agent.current_focused_checks();
    assert!(agent.run_checks(&checks, &mut NullUi).failures.is_empty());
    assert_eq!(
        agent.verifications.len(),
        1,
        "the completion gate uses the same witness"
    );
    let explicit = agent.check_tool(&json!({"names":["contract"]}), &mut NullUi);
    assert!(!explicit.is_error, "{}", explicit.text);
    assert_eq!(
        agent.verifications.len(),
        2,
        "explicit requests force real execution"
    );
}

#[test]
fn user_changes_invalidate_reused_evidence_even_without_an_effect_counter_change() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("inputs")).unwrap();
    std::fs::write(root.path().join("inputs/value"), "good").unwrap();
    let command = "test \"$(cat inputs/value)\" = good";
    let config = incremental_config(command);
    let mut agent = hermetic_agent(&config, root.path());
    agent.step_checks.mutations = 1;
    assert!(!agent.check_tool(&json!({}), &mut NullUi).is_error);
    agent.step_checks.record_checked();
    assert!(agent.step_checks.checks_current());
    std::fs::write(root.path().join("inputs/value"), "bad!").unwrap();
    agent.refresh_check_inputs();
    assert!(!agent.step_checks.checks_current());
    assert_eq!(
        agent
            .step_checks
            .evidence
            .current_status(command, agent.step_checks.revision()),
        "stale"
    );
    assert_eq!(
        agent
            .step_checks
            .evidence
            .passing_behavioral(agent.step_checks.revision()),
        0
    );
    let failed = agent.check_tool(&json!({}), &mut NullUi);
    assert!(failed.is_error, "{}", failed.text);
    assert_eq!(agent.verifications.len(), 2);
    assert!(agent.check_tool(&json!({}), &mut NullUi).is_error);
    assert_eq!(agent.verifications.len(), 3, "failures are never reused");
}

#[test]
fn opaque_effects_and_missing_inputs_force_execution() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("inputs")).unwrap();
    let config = incremental_config("test -d inputs");
    let mut agent = hermetic_agent(&config, root.path());
    agent.step_checks.mutations = 1;
    assert!(!agent.check_tool(&json!({}), &mut NullUi).is_error);
    agent.step_checks.uncheckpointed_shell_commands += 1;
    assert!(!agent.check_tool(&json!({}), &mut NullUi).is_error);
    assert_eq!(agent.verifications.len(), 2);
    agent.step_checks.uncheckpointed_extension_calls += 1;
    assert!(!agent.check_tool(&json!({}), &mut NullUi).is_error);
    assert_eq!(agent.verifications.len(), 3);
    std::fs::remove_dir(root.path().join("inputs")).unwrap();
    assert!(agent.check_tool(&json!({}), &mut NullUi).is_error);
    assert_eq!(agent.verifications.len(), 4);
}

#[test]
fn a_check_that_changes_its_own_inputs_cannot_supply_a_reuse_witness() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("inputs")).unwrap();
    let config = incremental_config("echo x >> inputs/output");
    let mut agent = hermetic_agent(&config, root.path());
    agent.step_checks.mutations = 1;
    assert!(!agent.check_tool(&json!({}), &mut NullUi).is_error);
    assert!(!agent.check_tool(&json!({}), &mut NullUi).is_error);
    assert_eq!(agent.verifications.len(), 2);
    assert_eq!(
        std::fs::read_to_string(root.path().join("inputs/output")).unwrap(),
        "x\nx\n"
    );
}

#[test]
fn undeclared_check_execution_and_directory_membership_changes_invalidate_reuse() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("inputs")).unwrap();
    let config = incremental_config("test -d inputs");
    let mut agent = hermetic_agent(&config, root.path());
    agent.step_checks.mutations = 1;
    assert!(!agent.check_tool(&json!({}), &mut NullUi).is_error);
    std::fs::write(root.path().join("inputs/new-fixture"), "added").unwrap();
    let changed = agent.check_tool(&json!({}), &mut NullUi);
    assert_eq!(changed.data.unwrap()["results"][0]["reused"], false);
    assert_eq!(agent.verifications.len(), 2);
    agent.run_focused_check(
        &FocusedCheck {
            name: "opaque".into(),
            command: "true".into(),
            timeout_secs: 30,
        },
        &mut NullUi,
    );
    assert_eq!(agent.verifications.len(), 3);
    let fresh = agent.check_tool(&json!({}), &mut NullUi);
    assert_eq!(fresh.data.unwrap()["results"][0]["reused"], false);
    assert_eq!(agent.verifications.len(), 4);
}

#[test]
fn interruption_does_not_reuse_a_pass_and_reproductions_never_inherit_a_witness() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("inputs")).unwrap();
    let command = "test -d inputs";
    let config = incremental_config(command);
    let mut agent = hermetic_agent(&config, root.path());
    agent.step_checks.mutations = 1;
    assert!(!agent.check_tool(&json!({}), &mut NullUi).is_error);
    agent.interrupt.store(true, Ordering::SeqCst);
    let interrupted = agent.check_tool(&json!({}), &mut NullUi);
    assert!(interrupted.is_error);
    assert_eq!(interrupted.data.unwrap()["results"][0]["reused"], false);
    assert_eq!(agent.verifications.len(), 1);
    agent.interrupt.store(false, Ordering::SeqCst);
    assert!(!agent.check_tool(&json!({}), &mut NullUi).is_error);
    agent
        .step_checks
        .evidence
        .register_repro(FocusedCheck {
            name: "repro: same-command".into(),
            command: command.into(),
            timeout_secs: 30,
        })
        .unwrap();
    assert!(!agent.check_tool(&json!({}), &mut NullUi).is_error);
    assert!(!agent.check_tool(&json!({}), &mut NullUi).is_error);
    assert_eq!(agent.verifications.len(), 4);
}

#[cfg(unix)]
#[test]
fn built_in_python_reuses_without_input_declarations_and_tracks_external_edits() {
    // Cargo injects this loader search path on macOS. Test the supported, injection-free
    // environment in a child rather than mutating the environment of parallel tests.
    if std::env::var_os("DYLD_FALLBACK_LIBRARY_PATH").is_some() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "agent::verify::check_tool::tests::incremental::built_in_python_reuses_without_input_declarations_and_tracks_external_edits", "--nocapture"])
            .env_remove("DYLD_FALLBACK_LIBRARY_PATH")
            .output().unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("app.py"), "value = 1\n").unwrap();
    let output = std::process::Command::new("python3")
        .args(["-I", "-S", "-c", "import sys; print(sys.executable)"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let mut config = Config::default();
    config.python = PathBuf::from(String::from_utf8(output.stdout).unwrap().trim());
    assert!(config.verification.inputs.is_empty());
    let mut agent = hermetic_agent(&config, root.path());
    // Exercise the normal development sandbox: the strict hermetic helper excludes user runtimes.
    let policy = crate::tools::exec::sandbox::Policy::Dev {
        deny_read: Vec::new(),
        extra_write: Vec::new(),
        allow_network: false,
    };
    let sandbox = crate::tools::exec::sandbox::Sandbox::detect();
    sandbox.available().unwrap();
    agent.workspace = crate::tools::Workspace::new(root.path())
        .unwrap()
        .with_policy(policy)
        .with_sandbox(sandbox);
    agent.step_checks.mutations = 1;
    let checks =
        crate::agent::verify::verification::plan(root.path(), &config.python, &["app.py".into()]);
    assert_eq!(checks.len(), 1);
    assert!(
        crate::agent::verify::inputs::python_snapshot(
            &agent.workspace,
            Path::new("app.py"),
            &config.python
        )
        .is_some(),
        "runtime snapshot unavailable"
    );
    let first = agent.run_checks(&checks, &mut NullUi);
    assert!(first.failures.is_empty(), "{:?}", first.failures);
    assert_eq!(agent.verifications.len(), 1);
    std::fs::write(root.path().join("README.md"), "unrelated").unwrap();
    agent.step_checks.mutations += 1;
    let second = agent.run_checks(&checks, &mut NullUi);
    assert!(second.failures.is_empty(), "{:?}", second.failures);
    assert_eq!(
        agent.verifications.len(),
        1,
        "default built-in reuse requires no declarations"
    );
    std::fs::write(root.path().join("app.py"), "invalid syntax here !!!\n").unwrap();
    agent.refresh_check_inputs();
    assert_eq!(
        agent
            .step_checks
            .evidence
            .current_status(&checks[0].command, agent.step_checks.revision()),
        "stale"
    );
    assert!(!agent.run_checks(&checks, &mut NullUi).failures.is_empty());
    assert_eq!(agent.verifications.len(), 2);
}
