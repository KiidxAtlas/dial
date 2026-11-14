use super::*;

fn suite() -> BenchmarkSuite {
    BenchmarkSuite::parse_toml("version=1\nname='observed'\n[[cases]]\nid='one'\nprompt='test'\nchecks=[{kind='file_exists',path='answer'}]\n[[cases]]\nid='two'\nprompt='test'\nchecks=[{kind='file_exists',path='answer'}]\n").unwrap()
}

#[test]
fn observers_receive_success_and_failure_after_checks() {
    let root = tempfile::tempdir().unwrap();
    let mut observed = Vec::new();
    let report = suite()
        .run_observed(
            root.path(),
            |case, workspace| {
                if case.id == "one" {
                    fs::write(workspace.join("answer"), "done")?;
                    Ok(RunMetrics {
                        agent_succeeded: true,
                        ..Default::default()
                    })
                } else {
                    bail!("inference unavailable")
                }
            },
            |_, _| unreachable!(),
            None,
            |result| {
                observed.push(result.clone());
                Ok(())
            },
        )
        .unwrap();
    assert_eq!(
        serde_json::to_value(&observed).unwrap(),
        serde_json::to_value(&report.cases).unwrap()
    );
    assert!(observed[0].passed);
    assert!(!observed[1].passed);
    assert!(observed[1].metrics.agent_error);
}

#[test]
fn observer_write_failure_stops_before_next_agent_call() {
    let root = tempfile::tempdir().unwrap();
    let mut calls = 0;
    let error = suite()
        .run_observed(
            root.path(),
            |_, _| {
                calls += 1;
                Ok(RunMetrics::default())
            },
            |_, _| unreachable!(),
            None,
            |_| bail!("progress disk full"),
        )
        .unwrap_err();
    assert_eq!(calls, 1);
    assert!(error.to_string().contains("progress disk full"));
}

/// Gitignored local data (benchmark runs, virtualenvs) is not source: freezing the suite source
/// leaves it out, so an escaping symlink inside it no longer aborts the run.
#[cfg(unix)]
#[test]
fn frozen_source_skips_gitignored_local_data() {
    let outer = tempfile::tempdir().unwrap();
    let root = outer.path().join("repo");
    fs::create_dir_all(root.join("local/env/bin")).unwrap();
    fs::write(root.join(".gitignore"), "/local/\n").unwrap();
    fs::write(outer.path().join("python3"), "outside").unwrap();
    std::os::unix::fs::symlink(
        outer.path().join("python3"),
        root.join("local/env/bin/python3"),
    )
    .unwrap();
    let git = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(&root)
        .status()
        .unwrap();
    assert!(git.success());
    let mut saw_local = None;
    let report = suite()
        .run(
            &root,
            |_, workspace| {
                saw_local = Some(workspace.join("local").exists());
                fs::write(workspace.join("answer"), "done")?;
                Ok(RunMetrics {
                    agent_succeeded: true,
                    ..Default::default()
                })
            },
            |_, _| unreachable!(),
        )
        .unwrap();
    assert!(report.cases.iter().all(|case| case.passed));
    assert_eq!(saw_local, Some(false));
}

#[test]
fn observers_also_receive_resumed_cases_without_new_inference() {
    let root = tempfile::tempdir().unwrap();
    let checkpoint = tempfile::tempdir().unwrap();
    let suite = suite();
    let contract = records::Contract::new(&suite, root.path(), &serde_json::json!({})).unwrap();
    let path = checkpoint.path().join("cases.json");
    let mut recorder = records::Recorder::open(&path, contract.clone(), false).unwrap();
    let original = suite
        .run_recorded(
            root.path(),
            |_, workspace| {
                fs::write(workspace.join("answer"), "done")?;
                Ok(RunMetrics {
                    agent_succeeded: true,
                    token_usage_observed: true,
                    ..Default::default()
                })
            },
            |_, _| unreachable!(),
            Some(&mut recorder),
        )
        .unwrap();
    drop(recorder);
    let mut recorder = records::Recorder::open(&path, contract, true).unwrap();
    let mut observed = Vec::new();
    let resumed = suite
        .run_observed(
            root.path(),
            |_, _| panic!("completed trials must not make inference requests"),
            |_, _| unreachable!(),
            Some(&mut recorder),
            |result| {
                observed.push(result.clone());
                Ok(())
            },
        )
        .unwrap();
    assert_eq!(
        serde_json::to_value(&observed).unwrap(),
        serde_json::to_value(&original.cases).unwrap()
    );
    assert_eq!(
        serde_json::to_value(&resumed.cases).unwrap(),
        serde_json::to_value(&original.cases).unwrap()
    );
}
