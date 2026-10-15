//! Dial-owned typed decisions, independent of other coding-agent applications.
//!
//! A decision engine is selected by `[laya] engine`. Laya is Dial's managed local service
//! (installed with `dial laya install`). Callers use the free functions below, which dispatch
//! through the selected engine's [`DecisionEngine`] implementation.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::{Arc, atomic::AtomicBool};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionEngineType {
    #[default]
    Laya,
    Kev,
}

/// Provider name that marks `model_roles.decision` as the Laya checkpoint.
pub const PROVIDER: &str = "laya";
/// Model id of a `judge` role served by Laya. The judge shares the service, and so the checkpoint, of the
/// `decision` role (else the installed default), so judging never restarts the service with another one.
pub const JUDGE_MODEL: &str = "decision";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub engine: DecisionEngineType,
    pub enabled: bool,
    pub auto_start: bool,
    pub endpoint: String,
    pub timeout_secs: u64,
    /// Checkpoint the service loads: the assigned `decision` role. Set from the model roles, never read from TOML.
    #[serde(skip)]
    pub checkpoint: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            engine: DecisionEngineType::Laya,
            enabled: false,
            auto_start: true,
            endpoint: "http://127.0.0.1:8002".into(),
            timeout_secs: 0,
            checkpoint: None,
        }
    }
}

impl Config {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.engine == DecisionEngineType::Laya,
            "decision engine `kev` has no implementation in this build; set [laya] engine = \"laya\""
        );
        let authority = self
            .endpoint
            .strip_prefix("http://")
            .context("Decision engine requires a local http endpoint")?;
        let (host, port) = authority
            .split_once(':')
            .context("Decision engine endpoint requires an explicit port")?;
        ensure!(
            matches!(host, "127.0.0.1" | "localhost") && port.parse::<u16>().is_ok_and(|p| p > 0),
            "Decision engine endpoint must be http://127.0.0.1:PORT or http://localhost:PORT, without a path"
        );
        Ok(())
    }
}

/// Pid file the managed service writes so a stale checkpoint can be replaced.
const PID_FILE: &str = "server.pid";

/// The checkpoint the service must load: the assigned `decision` role, else the installed default.
pub fn expected_checkpoint(config: &Config) -> Option<String> {
    config.checkpoint.clone().or_else(|| {
        let model = runtime_dir().ok()?.join("model");
        model
            .join("rl_agent_config.json")
            .is_file()
            .then(|| model.to_string_lossy().into_owned())
    })
}

/// Stops the managed service recorded in the pid file. Returns `false` when this Dial did not launch one.
fn stop_managed() -> Result<bool> {
    let path = runtime_dir()?.join(PID_FILE);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(false);
    };
    let pid: u32 = text
        .trim()
        .parse()
        .context("Dial Laya pid file is corrupt")?;
    crate::platform::process::terminate_group(pid)
        .context("could not stop the Dial Laya service")?;
    let _ = std::fs::remove_file(&path);
    Ok(true)
}

fn wait_for_stop(probe: &mut impl FnMut() -> Result<Health>, cancel: &AtomicBool) -> Result<()> {
    use std::time::{Duration, Instant};
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if cancel.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(crate::llm::Interrupted.into());
        }
        match probe() {
            Err(error) if connection_refused(&error) => return Ok(()),
            _ if Instant::now() >= deadline => {
                anyhow::bail!("Dial Laya service did not stop; stop it manually and retry")
            }
            _ => std::thread::sleep(Duration::from_millis(100)),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Health {
    pub status: String,
    #[serde(default)]
    pub busy: bool,
    pub checkpoint: String,
    pub device: String,
    pub service: String,
    pub protocol: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Decision {
    pub health: Health,
    pub answers: Value,
    pub elapsed_ms: u128,
}

/// A typed decision provider. `predict` answers typed questions (as built by [`questions`]) for the evidence.
pub trait DecisionEngine: Send + Sync {
    fn health(&self, cancel: Arc<AtomicBool>) -> Result<Health>;
    /// Wait for a ready service, starting a managed one when allowed.
    fn ensure_service(&self, cancel: Arc<AtomicBool>, explicit_start: bool) -> Result<Health>;
    fn predict(&self, questions: &Value, state: &str, cancel: Arc<AtomicBool>) -> Result<Decision>;
}

/// The engine selected by `config.engine`.
pub fn engine(config: &Config) -> Result<Box<dyn DecisionEngine>> {
    config.validate()?;
    match config.engine {
        DecisionEngineType::Laya => Ok(Box::new(LayaEngine {
            config: config.clone(),
        })),
        DecisionEngineType::Kev => {
            unreachable!("validate rejects engines without an implementation")
        }
    }
}

pub fn health(config: &Config, cancel: Arc<AtomicBool>) -> Result<Health> {
    engine(config)?.health(cancel)
}

/// Launch only after a refused connection; a loading or unhealthy service is never duplicated.
pub fn ensure_service(
    config: &Config,
    cancel: Arc<AtomicBool>,
    explicit_start: bool,
) -> Result<Health> {
    engine(config)?.ensure_service(cancel, explicit_start)
}

pub fn predict(
    config: &Config,
    phase: &str,
    state: &str,
    cancel: Arc<AtomicBool>,
) -> Result<Decision> {
    engine(config)?.predict(&questions(phase), state, cancel)
}

/// Pick one of `criteria` (choice name, what it means) for `state` with one typed `choice` question.
pub fn choose(
    config: &Config,
    instructions: &str,
    criteria: &[(&str, &str)],
    state: &str,
    cancel: Arc<AtomicBool>,
) -> Result<String> {
    let criteria: serde_json::Map<String, Value> = criteria
        .iter()
        .map(|(choice, meaning)| ((*choice).to_owned(), json!(meaning)))
        .collect();
    let questions =
        json!({"choice":{"type":"choice","instructions":instructions,"criteria":criteria}});
    let decision = engine(config)?.predict(&questions, state, cancel)?;
    decision.answers["choice"]["choice"]
        .as_str()
        .map(str::to_owned)
        .context("missing Laya choice")
}

/// Human summary of a Laya answer set (the default engine's wording).
pub fn summary(phase: &str, answers: &Value) -> String {
    laya_summary(phase, answers)
}

const LAYA_SERVICE: &str = "dial-laya";
/// Endpoints whose advice is paused until the given instant after a prediction timeout.
static ADVICE_PAUSES: std::sync::Mutex<std::collections::BTreeMap<String, std::time::Instant>> =
    std::sync::Mutex::new(std::collections::BTreeMap::new());

pub struct LayaEngine {
    pub config: Config,
}

impl DecisionEngine for LayaEngine {
    fn health(&self, cancel: Arc<AtomicBool>) -> Result<Health> {
        laya_health(&self.config, cancel)
    }
    fn ensure_service(&self, cancel: Arc<AtomicBool>, explicit_start: bool) -> Result<Health> {
        laya_ensure(&self.config, cancel, explicit_start)
    }
    fn predict(&self, questions: &Value, state: &str, cancel: Arc<AtomicBool>) -> Result<Decision> {
        laya_predict(&self.config, questions, state, cancel)
    }
}

fn client(config: &Config, cancel: Arc<AtomicBool>) -> ureq::Agent {
    crate::llm::agent_with(config.timeout_secs, false, Some(cancel), None)
}

fn laya_health(config: &Config, cancel: Arc<AtomicBool>) -> Result<Health> {
    config.validate()?;
    let mut response = client(config, cancel)
        .get(format!("{}/health", config.endpoint))
        .call()
        .context("Laya service is unavailable")?;
    ensure!(
        response.status().is_success(),
        "Laya health request failed: HTTP {}",
        response.status()
    );
    let raw = response
        .body_mut()
        .with_config()
        .limit(8192)
        .read_to_string()?;
    decode_health(&raw, LAYA_SERVICE)
}

pub fn decode_health(raw: &str, expected_service: &str) -> Result<Health> {
    let health: Health =
        serde_json::from_str(raw).context("invalid decision engine health response")?;
    ensure!(
        health.service == expected_service && health.protocol == 1,
        "Endpoint is not the expected service; expected {} with protocol 1 (Dial's managed Laya port is 8002)",
        expected_service
    );
    ensure!(
        matches!(health.status.as_str(), "ok" | "starting")
            && health.checkpoint.len() <= 2048
            && health.device.len() <= 80
            && !health
                .checkpoint
                .chars()
                .chain(health.device.chars())
                .any(char::is_control),
        "invalid decision engine service identity"
    );
    Ok(health)
}

fn connection_refused(error: &anyhow::Error) -> bool {
    error.chain().any(|cause|cause.downcast_ref::<std::io::Error>().is_some_and(|e|e.kind()==std::io::ErrorKind::ConnectionRefused)
        || cause.downcast_ref::<ureq::Error>().is_some_and(|e|matches!(e,ureq::Error::Io(io) if io.kind()==std::io::ErrorKind::ConnectionRefused)))
}

fn request_timed_out(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<ureq::Error>()
            .is_some_and(|e| matches!(e, ureq::Error::Timeout(_)))
    })
}

pub fn runtime_dir() -> Result<std::path::PathBuf> {
    let root = match std::env::var_os("DIAL_HOME").filter(|s| !s.is_empty()) {
        Some(root) => std::path::PathBuf::from(root),
        None => std::path::PathBuf::from(
            std::env::var_os("HOME").context("HOME is unavailable; set DIAL_HOME")?,
        )
        .join(".dial"),
    };
    let root = if root.is_absolute() {
        root
    } else {
        std::env::current_dir()?.join(root)
    };
    Ok(root.join("laya"))
}

const SERVER: &str = include_str!("../../python/dial_laya_server.py");
const REQUIREMENTS: &str = include_str!("../../python/laya-requirements.txt");

pub fn install(
    layout: &crate::storage::paths::Layout,
    checkpoint: Option<&std::path::Path>,
) -> Result<Option<std::path::PathBuf>> {
    use crate::{learn::worker, storage::settings_store};
    let directory = layout.user_dir.join("laya");
    settings_store::private_dir(&directory)?;
    settings_store::write_private(&directory.join("dial_laya_server.py"), SERVER.as_bytes())?;
    settings_store::write_private(&directory.join("requirements.txt"), REQUIREMENTS.as_bytes())?;
    let venv = directory.join("venv");
    let python = venv.join("bin/python");
    if !python.is_file() {
        worker::run_command(
            std::path::Path::new("uv"),
            &[
                "venv".into(),
                "--python".into(),
                "3.14".into(),
                venv.to_string_lossy().into_owned(),
            ],
            "",
            300,
        )
        .context("creating Dial's Laya runtime")?;
    }
    worker::run_command(
        std::path::Path::new("uv"),
        &[
            "pip".into(),
            "install".into(),
            "--python".into(),
            python.to_string_lossy().into_owned(),
            "-r".into(),
            directory
                .join("requirements.txt")
                .to_string_lossy()
                .into_owned(),
        ],
        "",
        900,
    )
    .context("installing Dial's Laya dependencies")?;
    if let Some(source) = checkpoint {
        let source = std::fs::canonicalize(source).context("checkpoint source does not exist")?;
        ensure!(
            !directory.join("model").exists(),
            "Dial already has a model; refusing to replace it implicitly"
        );
        for file in [
            "model.safetensors",
            "rl_agent_config.json",
            "encoder/config.json",
            "tokenizer/tokenizer.json",
            "tokenizer/tokenizer_config.json",
        ] {
            ensure!(source.join(file).is_file(), "checkpoint is missing {file}");
        }
        let staging = tempfile::tempdir_in(&directory)?;
        let model = staging.path().join("model");
        settings_store::private_dir(&model)?;
        for file in [
            "model.safetensors",
            "rl_agent_config.json",
            "encoder/config.json",
            "tokenizer/tokenizer.json",
            "tokenizer/tokenizer_config.json",
        ] {
            let input = source.join(file);
            ensure!(
                !std::fs::symlink_metadata(&input)?.file_type().is_symlink(),
                "checkpoint asset must be a regular file"
            );
            let output = model.join(file);
            settings_store::private_dir(output.parent().unwrap())?;
            std::fs::copy(&input, &output)?;
            crate::platform::fs::set_mode(&output, 0o600)?;
        }
        std::fs::rename(model, directory.join("model"))?;
    }
    ensure!(
        directory.join("model/rl_agent_config.json").is_file(),
        "Runtime installed; import a local checkpoint with dial laya install --checkpoint PATH"
    );
    Ok(checkpoint.map(|_| directory.join("model")))
}

fn launch_managed(config: &Config) -> Result<Arc<AtomicBool>> {
    use std::{
        fs::OpenOptions,
        process::{Command, Stdio},
    };
    let directory = runtime_dir()?;
    let python = directory.join("venv/bin/python");
    ensure!(
        python.is_file() && directory.join("model/rl_agent_config.json").is_file(),
        "Dial Laya is not installed; run dial laya install --checkpoint PATH"
    );
    crate::storage::settings_store::write_private(
        &directory.join("dial_laya_server.py"),
        SERVER.as_bytes(),
    )?;
    let log = crate::platform::fs::open_owner_only(
        &{
            let mut options = OpenOptions::new();
            options.create(true).append(true);
            options
        },
        &directory.join("server.log"),
    )
    .context("cannot open Dial Laya service log")?;
    let mut command = Command::new(python);
    let port = config
        .endpoint
        .rsplit(':')
        .next()
        .context("missing Laya port")?;
    command
        .args([
            "-m",
            "uvicorn",
            "dial_laya_server:app",
            "--host",
            "127.0.0.1",
            "--port",
            port,
            "--log-level",
            "info",
        ])
        .current_dir(&directory)
        .env("DIAL_LAYA_HOME", &directory)
        .envs(
            config
                .checkpoint
                .as_ref()
                .map(|checkpoint| ("DIAL_LAYA_CHECKPOINT", checkpoint.as_str())),
        )
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log));
    let mut child = crate::platform::process::spawn_group(&mut command)
        .context("could not start Dial Laya service")?;
    crate::storage::settings_store::write_private(
        &directory.join(PID_FILE),
        child.id().to_string().as_bytes(),
    )?;

    let exited = Arc::new(AtomicBool::new(false));
    let completed = exited.clone();
    std::thread::spawn(move || {
        let _ = child.wait();
        completed.store(true, std::sync::atomic::Ordering::SeqCst);
    });
    Ok(exited)
}

fn laya_ensure(config: &Config, cancel: Arc<AtomicBool>, explicit_start: bool) -> Result<Health> {
    ensure_service_with(
        config,
        cancel.clone(),
        explicit_start,
        || laya_health(config, cancel.clone()),
        || launch_managed(config),
    )
}

fn ensure_service_with(
    config: &Config,
    cancel: Arc<AtomicBool>,
    explicit_start: bool,
    mut probe: impl FnMut() -> Result<Health>,
    mut launch: impl FnMut() -> Result<Arc<AtomicBool>>,
) -> Result<Health> {
    use std::time::{Duration, Instant};
    config.validate()?;
    let expected = expected_checkpoint(config);
    let initial = match probe() {
        Ok(health)
            if expected
                .as_ref()
                .is_some_and(|expected| *expected != health.checkpoint) =>
        {
            ensure!(
                stop_managed()?,
                "Dial Laya at {} serves {:?} but the decision role selects {:?}; stop that service so Dial can start the selected checkpoint",
                config.endpoint,
                health.checkpoint,
                expected
            );
            wait_for_stop(&mut probe, &cancel)?;
            probe()
        }
        other => other,
    };
    let mut child_exited = None;
    let mut current = match initial {
        Ok(health) => health,
        Err(error) if connection_refused(&error) && (config.auto_start || explicit_start) => {
            if cancel.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(crate::llm::Interrupted.into());
            }
            child_exited = Some(launch()?);
            Health {
                busy: false,
                status: "starting".into(),
                checkpoint: "Dial configured checkpoint".into(),
                device: "loading".into(),
                service: "dial-laya".into(),
                protocol: 1,
            }
        }
        Err(error) => return Err(error),
    };
    let timeout_secs = if explicit_start || config.timeout_secs == 0 {
        30
    } else {
        config.timeout_secs
    };
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    while current.status != "ok" && Instant::now() < deadline {
        if cancel.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(crate::llm::Interrupted.into());
        }
        std::thread::sleep(Duration::from_millis(100));
        match probe() {
            Ok(health) => current = health,
            Err(error) if request_timed_out(&error) => {}
            Err(error) if connection_refused(&error) => {
                ensure!(
                    !child_exited
                        .as_ref()
                        .is_some_and(|exited| exited.load(std::sync::atomic::Ordering::SeqCst)),
                    "Dial Laya service exited during startup; inspect server.log in Dial's Laya directory"
                );
            }
            Err(error) => return Err(error),
        }
    }
    Ok(current)
}

pub fn questions(phase: &str) -> Value {
    if phase == "investigation" {
        json!({
            "retrieval":{"type":"choice","instructions":"How much repository discovery is required before answering this request?","criteria":{"none":"No repository lookup is needed; answer directly.","targeted":"Inspect named files or symbols; broad discovery is unnecessary.","explore":"Required files or facts are unknown; broader discovery is necessary."}}
        })
    } else {
        json!({
            "ran":{"type":"noul","instructions":"Does the supplied evidence show that verification commands actually ran after the code changes?"},
            "passed":{"type":"noul","instructions":"Does the supplied command output show that the relevant verification checks passed? Missing or failed checks are false."},
            "relevant":{"type":"noul","instructions":"Do the observed checks cover the changed behavior and the explicit task requirements? Build or syntax checks alone do not verify behavior."},
            "claim":{"type":"noul","instructions":"Does the proposed completion claim exceed the observed source and verification evidence? Answer true for unsupported claims."}
        })
    }
}

fn laya_predict(
    config: &Config,
    requested: &Value,
    state: &str,
    cancel: Arc<AtomicBool>,
) -> Result<Decision> {
    config.validate()?;
    let paused = ADVICE_PAUSES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&config.endpoint)
        .copied();
    ensure!(
        !paused.is_some_and(|until| std::time::Instant::now() < until),
        "Laya advice is paused after a timeout; coding continues without advice"
    );
    let start = std::time::Instant::now();
    let health = laya_ensure(config, cancel.clone(), false)?;
    ensure!(
        health.status == "ok",
        "Laya is loading its checkpoint; coding continues without advice"
    );
    ensure!(
        !health.busy,
        "Laya is busy with another prediction; coding continues without advice"
    );
    let filtered = state
        .lines()
        .filter(|line| crate::storage::memory::screen(line).is_ok())
        .collect::<Vec<_>>()
        .join("\n");
    let state = crate::agent::prompt::truncate_chars(&filtered, 10000);
    let requested = requested
        .as_object()
        .context("Laya questions must be a JSON object")?;
    let prediction_error = |error| {
        if matches!(error, ureq::Error::Timeout(_)) {
            ADVICE_PAUSES
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(
                    config.endpoint.clone(),
                    std::time::Instant::now() + std::time::Duration::from_secs(60),
                );
            anyhow::anyhow!(
                "Laya advice exceeded its {}s budget; pausing advice for 60s while coding continues",
                config.timeout_secs
            )
        } else {
            anyhow::Error::new(error).context("Laya prediction failed")
        }
    };
    let mut response = client(config, cancel)
        .post(format!("{}/v1/predict", config.endpoint))
        .send_json(json!({"state":{"input":state},"questions":requested}))
        .map_err(prediction_error)?;
    ensure!(
        response.status().as_u16() != 503,
        "Laya is busy or loading; coding continues without advice"
    );
    ensure!(
        response.status().is_success(),
        "Laya prediction request failed: HTTP {}",
        response.status()
    );
    let raw = response
        .body_mut()
        .with_config()
        .limit(32768)
        .read_to_string()
        .map_err(prediction_error)?;
    let value: Value = serde_json::from_str(&raw).context("invalid Laya prediction JSON")?;
    let answers = value["answers"]
        .as_object()
        .context("missing typed Laya answers")?;
    for (id, question) in requested {
        let answer = answers.get(id).context("missing requested Laya answer")?;
        if question["type"] == "choice" {
            ensure!(
                answer["choice"]
                    .as_str()
                    .is_some_and(|choice| question["criteria"].get(choice).is_some()),
                "invalid Laya choice"
            );
        } else {
            ensure!(
                answer["noul"]
                    .as_f64()
                    .is_some_and(|p| p.is_finite() && (0.0..=1.0).contains(&p)),
                "invalid Laya probability"
            );
        }
        if let Some(confidence) = answer.get("confidence") {
            ensure!(
                confidence
                    .as_f64()
                    .is_some_and(|p| p.is_finite() && (0.0..=1.0).contains(&p)),
                "invalid Laya confidence"
            );
        }
    }
    Ok(Decision {
        health,
        answers: Value::Object(answers.clone()),
        elapsed_ms: start.elapsed().as_millis(),
    })
}

fn laya_summary(phase: &str, answers: &Value) -> String {
    if phase == "investigation" {
        format!(
            "Suggested retrieval: {}. Advisory; inspect the evidence needed for the task.",
            answers["retrieval"]["choice"].as_str().unwrap_or("unknown")
        )
    } else {
        format!(
            "Post-edit evidence review: ran {:.2}, passed {:.2}, relevant {:.2}, unsupported claim {:.2}. Raw probabilities; test results remain authoritative.",
            answers["ran"]["noul"].as_f64().unwrap_or(0.0),
            answers["passed"]["noul"].as_f64().unwrap_or(0.0),
            answers["relevant"]["noul"].as_f64().unwrap_or(0.0),
            answers["claim"]["noul"].as_f64().unwrap_or(0.0)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn local_endpoint_and_budgets_are_enforced() {
        assert!(Config::default().validate().is_ok());
        for endpoint in [
            "http://example.com:8001",
            "http://127.0.0.1:8001/path",
            "https://127.0.0.1:8001",
            "http://127.0.0.1:0",
            "http://user@localhost:8001",
        ] {
            assert!(
                Config {
                    endpoint: endpoint.into(),
                    ..Config::default()
                }
                .validate()
                .is_err()
            );
        }
        assert!(
            Config {
                timeout_secs: 0,
                ..Config::default()
            }
            .validate()
            .is_ok()
        );
    }
    #[test]
    fn an_engine_without_an_implementation_is_refused_before_any_request() {
        let config = Config {
            engine: DecisionEngineType::Kev,
            ..Config::default()
        };
        let error = engine(&config).err().expect("kev has no implementation");
        assert!(error.to_string().contains("`kev`"), "{error:#}");
        assert!(
            predict(
                &config,
                "investigation",
                "hello",
                Arc::new(AtomicBool::new(false))
            )
            .is_err()
        );
        assert!(engine(&Config::default()).is_ok());
    }
    #[test]
    fn foreign_service_is_rejected_instead_of_becoming_a_dependency() {
        let foreign = json!({"service":"another-agent","protocol":1,"status":"ok","checkpoint":"external","device":"cpu"});
        assert!(decode_health(&foreign.to_string(), LAYA_SERVICE).is_err());
        let mut own = foreign;
        own["service"] = json!("dial-laya");
        assert_eq!(
            decode_health(&own.to_string(), LAYA_SERVICE)
                .unwrap()
                .service,
            "dial-laya"
        );
        own["protocol"] = json!(2);
        assert!(decode_health(&own.to_string(), LAYA_SERVICE).is_err());
    }
    fn ready() -> Health {
        Health {
            busy: false,
            status: "ok".into(),
            checkpoint: expected_checkpoint(&Config::default())
                .unwrap_or_else(|| "test-checkpoint".into()),
            device: "cpu".into(),
            service: "dial-laya".into(),
            protocol: 1,
        }
    }
    fn refused() -> anyhow::Error {
        std::io::Error::from(std::io::ErrorKind::ConnectionRefused).into()
    }

    #[test]
    fn busy_service_does_not_submit_another_prediction() {
        let mut health = ready();
        health.busy = true;
        let server = crate::llm::fake::serve(vec![crate::llm::fake::json(
            200,
            &serde_json::to_value(health).unwrap(),
        )]);
        let config = Config {
            endpoint: server.url.clone(),
            ..Default::default()
        };
        let error = predict(
            &config,
            "investigation",
            "hello",
            Arc::new(AtomicBool::new(false)),
        )
        .err()
        .unwrap();
        assert!(error.to_string().contains("Laya is busy"));
        let requests = server.requests();
        assert_eq!(requests.len(), 1);
        assert!(crate::llm::fake::request_line(&requests[0]).starts_with("GET /health"));
    }

    #[test]
    fn prediction_body_timeout_is_an_advisory_delay() {
        let server = crate::llm::fake::serve(vec![
            crate::llm::fake::json(200, &serde_json::to_value(ready()).unwrap()),
            crate::llm::fake::Script::ColdStart {
                delay: std::time::Duration::from_millis(1200),
                body: String::new(),
            },
        ]);
        let config = Config {
            endpoint: server.url.clone(),
            timeout_secs: 1,
            ..Default::default()
        };
        let error = predict(
            &config,
            "investigation",
            "hello",
            Arc::new(AtomicBool::new(false)),
        )
        .err()
        .unwrap();
        assert!(
            error.to_string().contains("exceeded its 1s budget"),
            "{error:#}"
        );
        assert!(error.to_string().contains("coding continues"));
        assert_eq!(server.requests().len(), 2);
    }

    #[test]
    fn refused_service_is_started_once_and_existing_service_is_reused() {
        let config = Config::default();
        let cancel = Arc::new(AtomicBool::new(false));
        let mut probes = 0;
        let mut launches = 0;
        let health = ensure_service_with(
            &config,
            cancel.clone(),
            false,
            || {
                probes += 1;
                if probes == 1 {
                    Err(refused())
                } else {
                    Ok(ready())
                }
            },
            || {
                launches += 1;
                Ok(Arc::new(AtomicBool::new(false)))
            },
        )
        .unwrap();
        assert_eq!(health.status, "ok");
        assert_eq!(launches, 1);
        ensure_service_with(
            &config,
            cancel,
            false,
            || Ok(ready()),
            || panic!("must reuse running service"),
        )
        .unwrap();
    }
    #[test]
    fn loading_service_is_not_duplicated_and_errors_do_not_trigger_startup() {
        let config = Config::default();
        let cancel = Arc::new(AtomicBool::new(false));
        let mut probes = 0;
        ensure_service_with(
            &config,
            cancel.clone(),
            false,
            || {
                probes += 1;
                let mut health = ready();
                if probes == 1 {
                    health.status = "starting".into();
                }
                Ok(health)
            },
            || panic!("must reuse loading service"),
        )
        .unwrap();
        assert!(
            ensure_service_with(
                &config,
                cancel,
                false,
                || Err(anyhow::anyhow!("invalid response")),
                || panic!("must not replace unhealthy service")
            )
            .is_err()
        );
    }
    #[test]
    fn disabled_startup_cancel_and_early_process_exit_are_observable() {
        let config = Config {
            auto_start: false,
            ..Config::default()
        };
        let cancel = Arc::new(AtomicBool::new(false));
        assert!(
            ensure_service_with(
                &config,
                cancel.clone(),
                false,
                || Err(refused()),
                || panic!("startup disabled")
            )
            .is_err()
        );
        assert!(
            ensure_service_with(
                &Config::default(),
                Arc::new(AtomicBool::new(true)),
                false,
                || Err(refused()),
                || panic!("cancelled")
            )
            .is_err()
        );
        let error = ensure_service_with(
            &config,
            cancel,
            true,
            || Err(refused()),
            || Ok(Arc::new(AtomicBool::new(true))),
        )
        .unwrap_err();
        assert!(error.to_string().contains("exited during startup"));
    }
    #[test]
    #[ignore = "requires the installed Dial Laya service"]
    fn installed_service_accepts_dials_native_typed_request() {
        let config = Config {
            enabled: true,
            auto_start: false,
            ..Config::default()
        };
        let decision = predict(
            &config,
            "investigation",
            "The user says hello. No code change or repository lookup is requested.",
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        assert_eq!(decision.health.status, "ok");
        assert!(!decision.health.checkpoint.is_empty());
        assert!(matches!(
            decision.answers["retrieval"]["choice"].as_str(),
            Some("none" | "targeted" | "explore")
        ));
    }
}
