use super::*;

fn trial() -> dial::benchmark::CaseResult {
    let suite = BenchmarkSuite::parse_toml("version=1\nname='test'\n[[cases]]\nid='one'\nprompt='test'\nchecks=[{kind='file_exists',path='missing'}]").unwrap();
    let root = tempfile::tempdir().unwrap();
    suite
        .run(
            root.path(),
            |_, _| anyhow::bail!("provider unavailable"),
            |_, _| unreachable!(),
        )
        .unwrap()
        .cases
        .remove(0)
}

#[test]
fn interrupted_journals_preserve_failed_trials_and_do_not_claim_completion() {
    let root = tempfile::tempdir().unwrap();
    let output = root.path().join("report.json");
    let mut progress = Progress::open(&output, "test", 2).unwrap();
    let result = trial();
    progress.case("baseline", 0, &result).unwrap();
    assert!(progress.finish().is_err());
    drop(progress);
    assert!(!output.exists());
    let path = fs::read_dir(root.path())
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let values: Vec<serde_json::Value> = fs::read_to_string(&path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(values.len(), 2);
    assert_eq!(values[0]["expected_trials"], 2);
    assert_eq!(values[1]["result"], serde_json::to_value(result).unwrap());
    assert_eq!(values[1]["configuration"], "baseline");
    assert_eq!(values[1]["repetition"], 0);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

#[test]
fn repeated_outputs_get_distinct_journals_and_finish_only_after_all_trials() {
    let root = tempfile::tempdir().unwrap();
    let output = root.path().join("report.json");
    let mut first = Progress::open(&output, "test", 1).unwrap();
    let _second = Progress::open(&output, "test", 1).unwrap();
    assert_eq!(fs::read_dir(root.path()).unwrap().count(), 2);
    let result = trial();
    first.case("baseline", 0, &result).unwrap();
    assert!(first.case("baseline", 1, &result).is_err());
    first.finish().unwrap();
    let mut contents = String::new();
    for entry in fs::read_dir(root.path()).unwrap() {
        contents.push_str(&fs::read_to_string(entry.unwrap().path()).unwrap());
    }
    assert_eq!(contents.matches("\"kind\":\"finished\"").count(), 1);
}
