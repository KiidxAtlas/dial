//! Private process metadata shared by model serving and background learning.
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
};

pub struct Record {
    path: PathBuf,
    value: Value,
}

impl Record {
    pub fn new(
        user: &Path,
        pid: u32,
        kind: &str,
        model: &str,
        project: &Path,
        state: &Path,
        phase: &str,
    ) -> Result<Self> {
        let path = user.join("compute-processes").join(format!("{pid}.json"));
        super::settings_store::private_dir(path.parent().context("process record directory")?)?;
        let value = json!({"pid":pid,"parent_pid":std::process::id(),"kind":kind,"model":model,
            "project":project,"state":state,"phase":phase});
        let record = Self { path, value };
        record.save()?;
        Ok(record)
    }

    fn save(&self) -> Result<()> {
        super::settings_store::write_private(&self.path, self.value.to_string().as_bytes())
    }

    pub fn phase(&mut self, phase: &str) -> Result<()> {
        self.value["phase"] = json!(phase);
        self.save()
    }
}

impl Drop for Record {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}
