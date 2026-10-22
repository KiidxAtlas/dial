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
mod tests;
