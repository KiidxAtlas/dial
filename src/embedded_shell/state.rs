//! Bounded named shell state. The parent publishes only a reaped, uncancelled exchange.
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct State {
    pub schema_version: u32,
    #[serde(default)]
    pub completed: bool,
    pub cwd: PathBuf,
    pub exported: BTreeMap<String, String>,
}
impl State {
    pub fn new(cwd: &Path) -> Self {
        Self {
            schema_version: 1,
            completed: false,
            cwd: cwd.to_owned(),
            exported: BTreeMap::new(),
        }
    }
    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != 1
            || self.exported.len() > 128
            || serde_json::to_vec(self)
                .map_err(|error| error.to_string())?
                .len()
                > 256 * 1024
        {
            return Err("embedded shell state version or budget is invalid".into());
        }
        for (name, value) in &self.exported {
            if !allowed(name, value) {
                return Err(format!(
                    "embedded shell state contains a prohibited variable {name:?}"
                ));
            }
        }
        Ok(())
    }
}
pub fn allowed(name: &str, value: &str) -> bool {
    !name.is_empty()
        && name.bytes().enumerate().all(|(index, byte)| {
            byte == b'_' || byte.is_ascii_alphabetic() || (index > 0 && byte.is_ascii_digit())
        })
        && !matches!(
            name,
            "HOME"
                | "TMPDIR"
                | "TMP"
                | "TEMP"
                | "PWD"
                | "OLDPWD"
                | "SHLVL"
                | "_"
                | "GIT_TERMINAL_PROMPT"
                | "GCM_INTERACTIVE"
        )
        && !crate::tools::sandbox::is_secret_env(name, value)
}
pub struct Exchange {
    path: PathBuf,
}
impl Exchange {
    pub fn new(scratch: &Path, seed: &State) -> Result<Self, String> {
        seed.validate()?;
        let mut seed = seed.clone();
        seed.completed = false;
        crate::storage::settings_store::private_dir(scratch).map_err(|error| error.to_string())?;
        let path = scratch.join(format!(
            "dial-shell-state-{:032x}.json",
            rand::random::<u128>()
        ));
        crate::storage::settings_store::write_private(
            &path,
            &serde_json::to_vec(&seed).map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())?;
        Ok(Self { path })
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn read(&self) -> Result<State, String> {
        read(&self.path)
    }
}
impl Drop for Exchange {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}
pub fn read(path: &Path) -> Result<State, String> {
    let bytes = crate::storage::settings_store::read_private(path)
        .map_err(|error| error.to_string())?
        .ok_or("embedded shell state exchange is missing")?;
    if bytes.len() > 256 * 1024 {
        return Err("embedded shell state exchange exceeds its budget".into());
    }
    let state: State = serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
    state.validate()?;
    Ok(state)
}
