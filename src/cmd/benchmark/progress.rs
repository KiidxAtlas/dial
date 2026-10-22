//! Durable evidence for interrupted machine runs. This journal is not a scored report or resume file.
use super::*;

pub(super) struct Progress {
    file: File,
    completed: usize,
    expected: usize,
}

impl Progress {
    pub(super) fn open(report: &Path, suite: &str, expected: usize) -> Result<Self> {
        let parent = report.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)?;
        let prefix = format!(
            "{}.progress-",
            report
                .file_name()
                .context("report path needs a filename")?
                .to_string_lossy()
        );
        // Unique owner-only files preserve earlier and concurrently running measurements.
        let (file, path) = tempfile::Builder::new()
            .prefix(&prefix)
            .suffix(".jsonl")
            .tempfile_in(parent)?
            .keep()?;
        let mut progress = Self {
            file,
            completed: 0,
            expected,
        };
        progress.append(&json!({
            "kind": "machine_benchmark_progress", "schema_version": 1,
            "suite": suite, "expected_trials": expected
        }))?;
        eprintln!("benchmark progress: {}", path.display());
        Ok(progress)
    }

    fn append(&mut self, value: &serde_json::Value) -> Result<()> {
        // Serialize first so a serialization error cannot leave a half-written record.
        let mut bytes = serde_json::to_vec(value)?;
        bytes.push(b'\n');
        self.file.write_all(&bytes)?;
        self.file.sync_all()?;
        Ok(())
    }

    pub(super) fn configuration(
        &mut self,
        name: &str,
        contract: &dial::benchmark::records::Contract,
    ) -> Result<()> {
        self.append(&json!({"kind": "configuration", "name": name, "contract": contract}))
    }

    pub(super) fn case(
        &mut self,
        configuration: &str,
        repetition: usize,
        result: &dial::benchmark::CaseResult,
    ) -> Result<()> {
        ensure!(
            self.completed < self.expected,
            "too many benchmark progress trials"
        );
        self.append(&json!({
            "kind": "case", "configuration": configuration, "repetition": repetition,
            "result": result
        }))?;
        self.completed += 1;
        Ok(())
    }

    pub(super) fn finish(&mut self) -> Result<()> {
        ensure!(
            self.completed == self.expected,
            "benchmark progress trials are incomplete"
        );
        self.append(&json!({"kind": "finished", "completed_trials": self.completed}))
    }
}

#[cfg(test)]
mod tests {
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
}
