//! Immutable run contracts and durable case checkpoints. No source or credentials in manifests.
use super::*;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Contract {
    pub schema_version: u32,
    pub suite_sha256: String,
    pub source_sha256: String,
    pub executable_sha256: String,
    pub configuration_sha256: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Checkpoints {
    contract: Contract,
    cases: BTreeMap<String, CaseResult>,
}
pub struct Recorder {
    _lock: fs::File,
    path: PathBuf,
    data: Checkpoints,
}

pub use crate::runtime::fingerprint::sha256_file as digest_file;
/// Hash the exact isolated copy, after the existing isolation code has excluded private state.
pub fn source_digest(root: &Path) -> Result<String> {
    fn visit(root: &Path, path: &Path, digest: &mut Sha256, count: &mut usize) -> Result<()> {
        let mut entries = fs::read_dir(path)?.collect::<std::io::Result<Vec<_>>>()?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            *count += 1;
            ensure!(
                *count <= 100_000,
                "benchmark source manifest exceeds 100000 entries"
            );
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path)?;
            ensure!(
                !metadata.file_type().is_symlink(),
                "benchmark manifest requires an isolated source copy"
            );
            digest.update(path.strip_prefix(root)?.to_string_lossy().as_bytes());
            digest.update([0]);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                digest.update((metadata.permissions().mode() & 0o777).to_le_bytes());
            }
            #[cfg(windows)]
            digest.update([u8::from(metadata.permissions().readonly())]);
            if metadata.is_dir() {
                digest.update(b"directory\0");
                visit(root, &path, digest, count)?;
            } else if metadata.is_file() {
                digest.update(b"file\0");
                digest.update(digest_file(&path)?.as_bytes());
            } else {
                bail!("unsupported benchmark source entry");
            }
        }
        Ok(())
    }
    let mut digest = Sha256::new();
    visit(root, root, &mut digest, &mut 0)?;
    Ok(format!("{:x}", digest.finalize()))
}
impl Contract {
    pub fn new(
        suite: &BenchmarkSuite,
        isolated_source: &Path,
        configuration: &serde_json::Value,
    ) -> Result<Self> {
        // Omit credentials entirely, but keep every outcome-affecting limit (for example
        // `max_tokens` or `history_token_cap`) so that changing it changes the contract.
        fn credential_key(key: &str) -> bool {
            let key = key.to_ascii_lowercase();
            let segments: Vec<&str> = key
                .split(|c: char| !c.is_ascii_alphanumeric())
                .filter(|segment| !segment.is_empty())
                .collect();
            const CREDENTIAL_WORDS: [&str; 12] = [
                "secret",
                "secrets",
                "password",
                "passwd",
                "credential",
                "credentials",
                "authorization",
                "headers",
                "env",
                "environment",
                "apikey",
                "bearer",
            ];
            const BUDGET_WORDS: [&str; 8] = [
                "cap", "budget", "limit", "max", "min", "count", "size", "window",
            ];
            if segments
                .iter()
                .any(|segment| CREDENTIAL_WORDS.contains(segment))
            {
                return true;
            }
            if segments
                .windows(2)
                .any(|pair| matches!(pair, ["api" | "access" | "private", "key"]))
            {
                return true;
            }
            // A singular `token` names a credential unless it is a budget (`max_token`, `token_cap`).
            segments.contains(&"token") && !segments.iter().any(|s| BUDGET_WORDS.contains(s))
        }
        fn scrub(value: &mut serde_json::Value) {
            match value {
                serde_json::Value::Object(map) => {
                    map.retain(|key, _| !credential_key(key));
                    for value in map.values_mut() {
                        scrub(value);
                    }
                }
                serde_json::Value::Array(values) => {
                    for value in values {
                        scrub(value);
                    }
                }
                _ => {}
            }
        }
        let mut configuration = configuration.clone();
        scrub(&mut configuration);
        Ok(Self {
            schema_version: 1,
            suite_sha256: format!("{:x}", Sha256::digest(serde_json::to_vec(suite)?)),
            source_sha256: source_digest(isolated_source)?,
            executable_sha256: digest_file(&std::env::current_exe()?)?,
            configuration_sha256: format!(
                "{:x}",
                Sha256::digest(serde_json::to_vec(&configuration)?)
            ),
        })
    }
    /// Matched quality comparisons can change executable/model configuration, but not task/source.
    pub fn matched_tasks(&self, other: &Self) -> bool {
        self.schema_version == other.schema_version
            && self.suite_sha256 == other.suite_sha256
            && self.source_sha256 == other.source_sha256
    }
}
impl Recorder {
    pub fn open(path: &Path, contract: Contract, resume: bool) -> Result<Self> {
        ensure!(
            contract.schema_version == 1,
            "unsupported run contract version"
        );
        let lock = crate::storage::settings_store::try_locked(path)?;
        let data =
            if resume && let Some(bytes) = crate::storage::settings_store::read_private(path)? {
                let data: Checkpoints =
                    serde_json::from_slice(&bytes).context("invalid benchmark checkpoint")?;
                ensure!(
                    data.contract == contract,
                    "resume refused: suite, source, executable or configuration changed"
                );
                data
            } else {
                ensure!(
                    !path.exists(),
                    "run checkpoint already exists; select resume or a new path"
                );
                Checkpoints {
                    contract,
                    cases: BTreeMap::new(),
                }
            };
        let recorder = Self {
            _lock: lock,
            path: path.to_owned(),
            data,
        };
        recorder.flush()?;
        Ok(recorder)
    }
    pub fn completed(&self, case: &BenchmarkCase) -> Option<CaseResult> {
        self.data
            .cases
            .get(&case.id)
            .filter(|result| {
                !result.metrics.agent_error
                    && !result.metrics.token_usage_incomplete
                    && result.metrics.token_usage_observed
                    && result.evidence.as_ref().is_some_and(|evidence| {
                        serde_json::to_vec(case).is_ok_and(|bytes| {
                            evidence.contract_sha256 == format!("{:x}", Sha256::digest(bytes))
                        })
                    })
            })
            .cloned()
    }
    pub fn record(&mut self, result: &CaseResult) -> Result<()> {
        self.data.cases.insert(result.id.clone(), result.clone());
        self.flush()
    }
    pub fn contract(&self) -> &Contract {
        &self.data.contract
    }
    fn flush(&self) -> Result<()> {
        crate::storage::atomic::write_private(&self.path, &serde_json::to_vec(&self.data)?)
    }
}

#[cfg(test)]
mod tests;
