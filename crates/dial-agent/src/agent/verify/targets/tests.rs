use super::*;
use crate::runtime::execution::{Observation, ShellLaunch, ShellStdin};

// Existing process-state cases simulate dispatch without a real process. Prepare source identity
// first, just as the controller must before dispatching the tool.
impl Targets {
    fn observe_prepared(
        &mut self,
        workspace: &Workspace,
        tool: &str,
        args: &Value,
        output: &ToolOutput,
        epoch: u64,
    ) {
        let sources = self.prepare(workspace, tool, args);
        self.observe(workspace, tool, args, output, epoch, sources);
    }
}

fn workspace() -> (tempfile::TempDir, Workspace) {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("local testing")).unwrap();
    std::fs::write(root.path().join("local testing/snake.py"), "print(1)\n").unwrap();
    std::fs::create_dir_all(root.path().join("fixtures/snake")).unwrap();
    std::fs::write(root.path().join("fixtures/snake/test_snake.py"), "").unwrap();
    let workspace = Workspace::new(root.path()).unwrap();
    (root, workspace)
}

fn exited(code: i32, terminal: bool) -> ToolOutput {
    ToolOutput::ok(String::new())
        .with_process(ProcessOutcome {
            started: true,
            exit_code: Some(code),
            signal: None,
            timed_out: false,
            cancelled: false,
            output_complete: true,
        })
        .with_shell_launch(ShellLaunch {
            stdin: if terminal {
                ShellStdin::Terminal
            } else {
                ShellStdin::Eof
            },
            terminal_allocated: terminal,
            job: None,
        })
}

fn alive(ready: bool) -> ToolOutput {
    let mut output = ToolOutput::ok(String::new()).with_process(ProcessOutcome {
        started: true,
        exit_code: None,
        signal: None,
        timed_out: false,
        cancelled: false,
        output_complete: false,
    });
    if ready {
        output.execution.observation = Some(Observation {
            kind: ObservationKind::Readiness,
            satisfied: true,
        });
    }
    output
}

fn started(name: &str, terminal: bool) -> ToolOutput {
    let mut output = ToolOutput::ok(format!("started {name}"));
    output.data = Some(json!({"job": {"name": name, "pid": 1, "terminal": terminal}}));
    output.with_shell_launch(ShellLaunch {
        stdin: if terminal {
            ShellStdin::Terminal
        } else {
            ShellStdin::Eof
        },
        terminal_allocated: terminal,
        job: Some(name.to_owned()),
    })
}

#[test]
fn only_a_leading_run_verb_is_an_execution_request() {
    for text in [
        "run the snake game in local testing/",
        "Please run local testing/snake.py",
        "okay, can you launch it",
        "go ahead and play the game",
        "Start the server",
    ] {
        assert!(execution_requested(text), "{text}");
    }
    for text in [
        "okay do these all",
        "how do I run local testing/snake.py?",
        "explain what run.sh does",
        "start by reading local testing/snake.py",
        "fix the crash when you run it",
        "start editing local testing/snake.py",
        "launch a review of local testing/snake.py",
        "",
    ] {
        assert!(!execution_requested(text), "{text}");
    }
}

#[test]
fn named_paths_resolve_across_spaces_quotes_and_absolute_forms() {
    let (root, workspace) = workspace();
    let paths = |text: &str| -> Vec<String> {
        requested_paths(&workspace, text)
            .into_iter()
            .map(|target| target.path)
            .collect()
    };
    assert_eq!(
        paths("run the snake game in local testing/"),
        ["local testing"]
    );
    assert_eq!(
        paths("run `local testing/snake.py`."),
        ["local testing/snake.py"]
    );
    let absolute = root.path().join("local testing").display().to_string();
    assert_eq!(paths(&format!("run {absolute}/")), ["local testing"]);
    // Plain words, missing paths and the root itself are not targets.
    assert!(paths("run the snake game").is_empty());
    assert!(paths("run missing/game.py and ./").is_empty());
    assert!(
        Targets::resolve(&workspace, "okay do these all in local testing/")
            .requested
            .is_empty()
    );
}

#[test]
fn commands_reference_targets_by_path_cwd_or_directory_and_file_name() {
    let (_root, workspace) = workspace();
    let dir = &requested_paths(&workspace, "local testing/")[0];
    let file = &requested_paths(&workspace, "local testing/snake.py")[0];
    assert!(dir.referenced(&workspace, "cd \"local testing\" && python3 snake.py", None));
    assert!(dir.referenced(&workspace, "python3 local\\ testing/snake.py", None));
    assert!(dir.referenced(&workspace, "python3 snake.py", Some("local testing")));
    assert!(file.referenced(&workspace, "cd 'local testing' && python3 snake.py", None));
    assert!(file.referenced(&workspace, "python3 snake.py", Some("local testing")));
    assert!(file.referenced(
        &workspace,
        "chmod +x 'local testing/snake.py' && env TERM=xterm 'local testing/snake.py' 2>&1",
        None
    ));
    assert!(!dir.referenced(&workspace, "python3 -m pytest fixtures/snake", None));
    assert!(!dir.referenced(&workspace, "python3 snake.py", Some("fixtures/snake")));
    assert!(!file.referenced(&workspace, "python3 mysnake.py", Some("local testing")));
    // Writing, editing or inspecting the path does not run it, nor does moving into it.
    for command in [
        "printf 'echo x\\n' >> 'local testing/snake.py'",
        "python3 gen.py > \"local testing/snake.py\"",
        "sed -i '' s/a/b/ 'local testing/snake.py'",
        "cd 'local testing' && ls",
        "cd 'local testing'",
        "cp fixtures/snake/test_snake.py 'local testing/'",
    ] {
        assert!(!dir.referenced(&workspace, command, None), "{command}");
        assert!(!file.referenced(&workspace, command, None), "{command}");
    }
}

#[test]
fn driving_a_tracked_job_does_not_make_its_run_stale() {
    // Both launch surfaces share the same tracked job. Starting is not yet run evidence;
    // input and resize advance effects without changing the code that the job runs.
    let (_root, workspace) = workspace();
    for tool in ["bash", "bg"] {
        let mut targets = Targets::resolve(&workspace, "run local testing/snake.py");
        let mut start = json!({"command": "python3 snake.py", "cwd": "local testing", "pty": true});
        if tool == "bg" {
            start["action"] = json!("start");
        }
        targets.observe_prepared(&workspace, tool, &start, &started("game", true), 1);
        assert!(
            targets
                .blocker(&workspace, 1, true)
                .unwrap()
                .contains("never observed"),
            "{tool}"
        );
        let record = targets.evidence(&workspace, 1);
        assert_eq!(record["targets"][0]["latest"]["tool"], tool);
        assert_eq!(record["targets"][0]["latest"]["terminal"], true);
        let keys = json!({"action": "input", "name": "game", "text": "q"});
        targets.observe_prepared(&workspace, "bg", &keys, &ToolOutput::ok(String::new()), 2);
        let output = json!({"action": "output", "name": "game"});
        targets.observe_prepared(&workspace, "bg", &output, &alive(false), 2);
        assert_eq!(targets.blocker(&workspace, 2, false), None, "{tool}");
        // Input to an unrelated job is a foreign effect.
        let other = json!({"action": "input", "name": "other", "text": "x"});
        targets.observe_prepared(&workspace, "bg", &other, &ToolOutput::ok(String::new()), 3);
        assert!(
            targets
                .blocker(&workspace, 3, false)
                .unwrap()
                .contains("before a later edit"),
            "{tool}"
        );
    }
}

#[test]
fn another_programs_passing_tests_leave_the_requested_path_unrun() {
    // Session 651af3fe: fixture tests elsewhere passed, then the game was reported working.
    let (_root, workspace) = workspace();
    let mut targets = Targets::resolve(&workspace, "run the snake game in local testing/");
    targets.observe_prepared(
        &workspace,
        "bash",
        &json!({"command": "python3 -m pytest fixtures/snake"}),
        &exited(0, false),
        0,
    );
    // Inspection names the path without running it.
    targets.observe_prepared(
        &workspace,
        "bash",
        &json!({"command": "ls \"local testing\""}),
        &exited(0, false),
        0,
    );
    let blocker = targets.blocker(&workspace, 0, true).unwrap();
    assert!(
        blocker.contains("`local testing/`: no run naming this path"),
        "{blocker}"
    );
    assert_eq!(
        targets.evidence(&workspace, 0)["targets"][0]["status"],
        "not_run"
    );
}

#[test]
fn a_failed_run_without_a_terminal_reports_the_untried_terminal_launch() {
    // Session 5307748e: a non-terminal launch failed and the failure was called an environment limit.
    let (_root, workspace) = workspace();
    let mut targets = Targets::resolve(&workspace, "run the snake game in local testing/");
    let run = json!({"command": "cd \"local testing\" && python3 snake.py"});
    targets.observe_prepared(&workspace, "bash", &run, &exited(1, false), 0);
    let blocker = targets.blocker(&workspace, 0, true).unwrap();
    assert!(
        blocker.contains("exited with code 1; launched without a terminal"),
        "{blocker}"
    );
    assert!(
        blocker.contains("`pty: true`) is available and was not attempted"),
        "{blocker}"
    );
    // Without an available terminal launch the refusal states only what was observed.
    assert!(
        !targets
            .blocker(&workspace, 0, false)
            .unwrap()
            .contains("pty")
    );

    // A terminal job that is only started is not yet evidence; observing it running is.
    let start = json!({"action": "start", "command": "python3 snake.py", "cwd": "local testing", "pty": true, "name": "game"});
    targets.observe_prepared(&workspace, "bg", &start, &started("game", true), 0);
    assert!(
        targets
            .blocker(&workspace, 0, true)
            .unwrap()
            .contains("never observed")
    );
    targets.observe_prepared(
        &workspace,
        "bg",
        &json!({"action": "output", "name": "game"}),
        &alive(false),
        0,
    );
    assert_eq!(targets.blocker(&workspace, 0, true), None);
    let evidence = targets.evidence(&workspace, 0);
    assert_eq!(evidence["targets"][0]["status"], "process_succeeded");
    assert_eq!(evidence["targets"][0]["terminal_launches"], 1);
    assert_eq!(evidence["targets"][0]["latest"]["state"], "running");
    assert_eq!(evidence["behavior"], "not_assessed");

    // A later effect makes the run stale. Observing the same job afterwards still describes the
    // launch before that effect, so only a fresh launch can be current again.
    assert!(
        targets
            .blocker(&workspace, 1, true)
            .unwrap()
            .contains("before a later edit or effectful command")
    );
    targets.observe_prepared(
        &workspace,
        "bg",
        &json!({"action": "output", "name": "game"}),
        &alive(false),
        1,
    );
    assert_eq!(
        targets.evidence(&workspace, 1)["targets"][0]["status"],
        "stale"
    );
    targets.observe_prepared(&workspace, "bg", &start, &started("game", true), 1);
    targets.observe_prepared(
        &workspace,
        "bg",
        &json!({"action": "wait", "name": "game"}),
        &exited(1, true),
        1,
    );
    let blocker = targets.blocker(&workspace, 1, true).unwrap();
    assert!(blocker.contains("launched in a terminal"), "{blocker}");
    assert!(!blocker.contains("was not attempted"), "{blocker}");
}

#[test]
fn a_readiness_failure_keeps_the_started_job_observable() {
    // `bg start` with readiness that fails still launched a managed job; its identity is kept.
    let (_root, workspace) = workspace();
    let mut targets = Targets::resolve(&workspace, "run local testing/snake.py");
    let start = json!({"action": "start", "command": "python3 'local testing/snake.py'", "name": "svc", "ready": {"log": "up"}});
    let mut failed = ToolOutput::err("readiness timed out\nstarted svc");
    failed.data = Some(json!({"job": {"name": "svc", "pid": 1, "terminal": false}}));
    targets.observe_prepared(&workspace, "bg", &start, &failed, 0);
    assert_eq!(
        targets.evidence(&workspace, 0)["targets"][0]["status"],
        "started_unobserved"
    );
    targets.observe_prepared(
        &workspace,
        "bg",
        &json!({"action": "output", "name": "svc"}),
        &alive(false),
        0,
    );
    assert_eq!(targets.blocker(&workspace, 0, false), None);
}

#[test]
fn readiness_zero_exits_and_auto_backgrounded_runs_count_and_unknown_jobs_do_not() {
    let (_root, workspace) = workspace();
    let request = "run local testing/snake.py";
    let mut targets = Targets::resolve(&workspace, request);
    targets.observe_prepared(
        &workspace,
        "bg",
        &json!({"action": "wait", "name": "other"}),
        &alive(true),
        0,
    );
    assert!(targets.blocker(&workspace, 0, false).is_some());
    let run = json!({"command": "python3 snake.py", "cwd": "local testing"});
    targets.observe_prepared(&workspace, "bash", &run, &exited(0, false), 0);
    assert_eq!(targets.blocker(&workspace, 0, false), None);

    let mut targets = Targets::resolve(&workspace, request);
    let mut moved = ToolOutput::ok(String::new());
    moved.data = Some(json!({"background_job": "bg1"}));
    targets.observe_prepared(&workspace, "bash", &run, &moved, 0);
    assert!(
        targets
            .blocker(&workspace, 0, false)
            .unwrap()
            .contains("never observed"),
        "an adoption receipt is not an observation of the running program"
    );
    targets.observe_prepared(
        &workspace,
        "bg",
        &json!({"action": "output", "name": "bg1"}),
        &alive(false),
        0,
    );
    assert_eq!(targets.blocker(&workspace, 0, false), None);
    targets.observe_prepared(
        &workspace,
        "bg",
        &json!({"action": "wait", "name": "bg1"}),
        &exited(2, false),
        0,
    );
    assert!(
        targets
            .blocker(&workspace, 0, false)
            .unwrap()
            .contains("exited with code 2")
    );

    let mut targets = Targets::resolve(&workspace, request);
    let mut start = started("svc", false);
    start = start.with_process(alive(true).process_outcome().unwrap());
    start.execution.observation = alive(true).execution.observation;
    targets.observe_prepared(
        &workspace,
        "bg",
        &json!({"action": "start", "command": "python3 'local testing/snake.py'", "name": "svc", "ready": {"log": "up"}}),
        &start,
        0,
    );
    assert_eq!(
        targets.evidence(&workspace, 0)["targets"][0]["latest"]["state"],
        "ready"
    );
    assert_eq!(targets.blocker(&workspace, 0, false), None);
}

#[test]
fn refused_and_unstarted_calls_record_nothing() {
    let (_root, workspace) = workspace();
    let mut targets = Targets::resolve(&workspace, "run local testing/snake.py");
    let run = json!({"command": "python3 'local testing/snake.py'"});
    targets.observe_prepared(&workspace, "bash", &run, &ToolOutput::err("denied"), 0);
    let mut unstarted = exited(0, false);
    unstarted.execution.process.as_mut().unwrap().started = false;
    unstarted.data.as_mut().unwrap()["process"]["started"] = json!(false);
    targets.observe_prepared(&workspace, "bash", &run, &unstarted, 0);
    assert_eq!(targets.evidence(&workspace, 0)["targets"][0]["launches"], 0);
}

#[test]
fn steering_replaces_obligations_only_with_a_new_run_request() {
    let (_root, workspace) = workspace();
    let mut targets = Targets::resolve(&workspace, "run local testing/snake.py");
    let run = json!({"command": "python3 'local testing/snake.py'"});
    targets.observe_prepared(&workspace, "bash", &run, &exited(0, false), 0);
    targets.retarget(&workspace, "now run local testing/snake.py again");
    assert_eq!(targets.blocker(&workspace, 0, false), None);
    // Steering that names nothing to run keeps the open obligation and its evidence.
    targets.retarget(&workspace, "also check the colours");
    assert!(targets.has_obligations());
    assert_eq!(targets.blocker(&workspace, 0, false), None);
    // A run request for another path replaces the obligation and drops unrelated runs.
    targets.retarget(&workspace, "run fixtures/snake/test_snake.py");
    assert_eq!(
        targets.evidence(&workspace, 0)["targets"][0]["path"],
        "fixtures/snake/test_snake.py"
    );
    assert_eq!(
        targets.evidence(&workspace, 0)["targets"][0]["status"],
        "not_run"
    );
}

#[test]
fn compatibility_payload_does_not_supply_terminal_authority() {
    let (_root, workspace) = workspace();
    let mut targets = Targets::resolve(&workspace, "run local testing/snake.py");
    let mut output = exited(1, false);
    output.data.as_mut().unwrap()["shell_launch"]["terminal_allocated"] = json!(true);
    targets.observe_prepared(
        &workspace,
        "bash",
        &json!({"command": "python3 'local testing/snake.py'"}),
        &output,
        0,
    );
    assert_eq!(
        targets.evidence(&workspace, 0)["targets"][0]["latest"]["terminal"],
        false
    );
    output.execution.shell_launch = None;
    let mut targets = Targets::resolve(&workspace, "run local testing/snake.py");
    targets.observe_prepared(
        &workspace,
        "bash",
        &json!({"command": "python3 'local testing/snake.py'"}),
        &output,
        0,
    );
    assert!(targets.evidence(&workspace, 0)["targets"][0]["latest"]["terminal"].is_null());
    assert!(
        !targets
            .blocker(&workspace, 0, true)
            .unwrap()
            .contains("launched in a terminal")
    );
}

#[test]
fn external_edits_invalidate_a_run_without_advancing_the_effect_epoch() {
    // Exercise both edit windows: during dispatch and after its result was recorded.
    for edit_before_result in [true, false] {
        let (root, workspace) = workspace();
        let mut targets = Targets::resolve(&workspace, "run local testing/snake.py");
        let args = json!({"command": "python3 'local testing/snake.py'"});
        let sources = targets.prepare(&workspace, "bash", &args);
        let path = root.path().join("local testing/snake.py");
        if edit_before_result {
            std::fs::write(&path, "print(2)\n").unwrap();
        }
        targets.observe(&workspace, "bash", &args, &exited(0, false), 0, sources);
        if !edit_before_result {
            assert_eq!(targets.blocker(&workspace, 0, false), None);
            std::fs::write(&path, "print(2)\n").unwrap();
        }
        let evidence = targets.evidence(&workspace, 0);
        assert_eq!(evidence["targets"][0]["status"], "source_changed");
        assert_eq!(evidence["targets"][0]["latest"]["current"], false);
        assert!(
            targets
                .blocker(&workspace, 0, false)
                .unwrap()
                .contains("source changed")
        );
    }
}

#[test]
fn observing_a_job_retains_its_original_source_until_a_new_launch() {
    let (root, workspace) = workspace();
    let mut targets = Targets::resolve(&workspace, "run local testing/snake.py");
    let launch =
        json!({"action": "start", "name": "game", "command": "python3 'local testing/snake.py'"});
    targets.observe_prepared(&workspace, "bg", &launch, &started("game", false), 0);
    std::fs::write(root.path().join("local testing/snake.py"), "print(2)\n").unwrap();
    for action in ["output", "wait"] {
        targets.observe_prepared(
            &workspace,
            "bg",
            &json!({"action": action, "name": "game"}),
            &exited(0, false),
            0,
        );
        assert_eq!(
            targets.evidence(&workspace, 0)["targets"][0]["status"],
            "source_changed",
            "{action}"
        );
        assert!(targets.blocker(&workspace, 0, false).is_some());
    }
    targets.observe_prepared(&workspace, "bg", &launch, &started("game", false), 0);
    targets.observe_prepared(
        &workspace,
        "bg",
        &json!({"action": "output", "name": "game"}),
        &alive(false),
        0,
    );
    assert_eq!(targets.blocker(&workspace, 0, false), None);
}

#[test]
fn process_success_is_bound_to_the_original_path_and_never_behavior_verification() {
    for command in [
        "python3 'fixtures/snake/test_snake.py'",
        "python3 'local testing/snake.py'",
    ] {
        let (_root, workspace) = workspace();
        let mut targets = Targets::resolve(&workspace, "run local testing/snake.py");
        targets.observe_prepared(
            &workspace,
            "bash",
            &json!({"command": command}),
            &exited(0, false),
            0,
        );
        let evidence = targets.evidence(&workspace, 0);
        assert_eq!(evidence["behavior_verified"], false);
        if command.contains("fixtures") {
            assert_eq!(evidence["targets"][0]["status"], "not_run");
            assert!(targets.blocker(&workspace, 0, false).is_some());
        } else {
            assert_eq!(evidence["targets"][0]["status"], "process_succeeded");
            assert_eq!(evidence["targets"][0]["latest"]["terminal"], false);
            assert_eq!(evidence["targets"][0]["latest"]["behavior_verified"], false);
            assert_eq!(targets.blocker(&workspace, 0, false), None);
        }
    }
}

#[test]
fn unavailable_source_identities_do_not_become_current_process_evidence() {
    for case in ["missing_at_launch", "missing_at_completion", "no_snapshot"] {
        let (root, workspace) = workspace();
        let mut targets = Targets::resolve(&workspace, "run local testing/snake.py");
        let args = json!({"command": "python3 'local testing/snake.py'"});
        let path = root.path().join("local testing/snake.py");
        if case == "missing_at_launch" {
            std::fs::remove_file(&path).unwrap();
        }
        let sources = if case == "no_snapshot" {
            LaunchSnapshot::default()
        } else {
            targets.prepare(&workspace, "bash", &args)
        };
        if case == "missing_at_launch" {
            std::fs::write(&path, "print(1)\n").unwrap();
        } else if case == "missing_at_completion" {
            std::fs::remove_file(&path).unwrap();
        }
        targets.observe(&workspace, "bash", &args, &exited(0, false), 0, sources);
        let evidence = targets.evidence(&workspace, 0);
        assert_eq!(
            evidence["targets"][0]["status"], "source_identity_unknown",
            "{case}"
        );
        assert_eq!(evidence["targets"][0]["latest"]["current"], false, "{case}");
        assert!(
            targets
                .blocker(&workspace, 0, false)
                .unwrap()
                .contains("source identity is unknown"),
            "{case}"
        );
    }
}

#[cfg(unix)]
#[test]
fn identity_reads_stay_confined_when_a_target_is_replaced_by_an_external_symlink() {
    let (root, workspace) = workspace();
    let outside = tempfile::tempdir().unwrap();
    let outside_file = outside.path().join("snake.py");
    std::fs::write(&outside_file, "print(1)\n").unwrap();
    let mut targets = Targets::resolve(&workspace, "run local testing/snake.py");
    let args = json!({"command": "python3 'local testing/snake.py'"});
    targets.observe_prepared(&workspace, "bash", &args, &exited(0, false), 0);
    let path = root.path().join("local testing/snake.py");
    std::fs::remove_file(&path).unwrap();
    std::os::unix::fs::symlink(&outside_file, &path).unwrap();
    assert_eq!(
        targets.evidence(&workspace, 0)["targets"][0]["status"],
        "source_identity_unknown"
    );
    assert!(targets.blocker(&workspace, 0, false).is_some());
}

#[test]
fn named_directory_runs_remain_current_only_while_the_complete_source_tree_matches() {
    for change in [
        "modify",
        "modify_ignored",
        "add_ignored",
        "remove_ignored",
        "add_directory",
        "remove_directory",
    ] {
        let (root, workspace) = workspace();
        let dir = root.path().join("local testing");
        std::fs::write(dir.join(".gitignore"), ".cache/\n").unwrap();
        std::fs::create_dir(dir.join(".cache")).unwrap();
        std::fs::write(dir.join(".cache/data"), "original").unwrap();
        std::fs::create_dir(dir.join(".cache/empty")).unwrap();
        let mut targets = Targets::resolve(&workspace, "run the snake game in local testing/");
        let args = json!({"command": "python3 snake.py", "cwd": "local testing"});
        targets.observe_prepared(&workspace, "bash", &args, &exited(0, false), 0);
        let evidence = targets.evidence(&workspace, 0);
        assert_eq!(
            evidence["targets"][0]["status"], "process_succeeded",
            "{change}"
        );
        assert_eq!(evidence["targets"][0]["kind"], "directory");
        assert_eq!(evidence["behavior_verified"], false);
        assert_eq!(targets.blocker(&workspace, 0, false), None, "{change}");
        match change {
            "modify" => std::fs::write(dir.join("snake.py"), "print(2)\n").unwrap(),
            "modify_ignored" => std::fs::write(dir.join(".cache/data"), "changed").unwrap(),
            "add_ignored" => std::fs::write(dir.join(".cache/new"), "new").unwrap(),
            "remove_ignored" => std::fs::remove_file(dir.join(".cache/data")).unwrap(),
            "add_directory" => std::fs::create_dir(dir.join(".cache/new_dir")).unwrap(),
            "remove_directory" => std::fs::remove_dir(dir.join(".cache/empty")).unwrap(),
            _ => unreachable!(),
        }
        let evidence = targets.evidence(&workspace, 0);
        assert_eq!(
            evidence["targets"][0]["status"], "source_changed",
            "{change}"
        );
        assert_eq!(
            evidence["targets"][0]["latest"]["current"], false,
            "{change}"
        );
        assert!(targets.blocker(&workspace, 0, false).is_some(), "{change}");
    }
}
