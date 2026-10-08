//! Dial-owned typed decisions, independent of other coding-agent applications.
use crate::platform::fs::OpenOptions;
use crate::platform::process::CommandExt;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::{Arc, atomic::AtomicBool};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub enabled: bool,
    pub auto_start: bool,
    pub endpoint: String,
    pub timeout_secs: u64,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: false,
            auto_start: true,
            endpoint: "http://127.0.0.1:8002".into(),
            timeout_secs: 3,
        }
    }
}
impl Config {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            (1..=10).contains(&self.timeout_secs),
            "laya.timeout_secs must be 1..10"
        );
        let authority = self
            .endpoint
            .strip_prefix("http://")
            .context("Laya requires a local http endpoint")?;
        let (host, port) = authority
            .split_once(':')
            .context("Laya endpoint requires an explicit port")?;
        ensure!(
            matches!(host, "127.0.0.1" | "localhost") && port.parse::<u16>().is_ok_and(|p| p > 0),
            "Laya endpoint must be http://127.0.0.1:PORT or http://localhost:PORT, without a path"
        );
        Ok(())
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

pub struct Decision {
    pub health: Health,
    pub answers: Value,
    pub elapsed_ms: u128,
}

fn client(config: &Config, cancel: Arc<AtomicBool>) -> ureq::Agent {
    crate::llm::agent_with(config.timeout_secs, false, Some(cancel), None)
}
pub fn health(config: &Config, cancel: Arc<AtomicBool>) -> Result<Health> {
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
    decode_health(&raw)
}

fn decode_health(raw: &str) -> Result<Health> {
    let health: Health = serde_json::from_str(raw).context("invalid Laya health response")?;
    ensure!(
        health.service == "dial-laya" && health.protocol == 1,
        "Endpoint is not a Dial Laya service; select Dial's managed port 8002"
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
        "invalid Laya service identity"
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
            crate::platform::home_var_os().context("HOME is unavailable; set DIAL_HOME")?,
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
) -> Result<()> {
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
    Ok(())
}

fn launch_managed(config: &Config) -> Result<Arc<AtomicBool>> {
    use std::process::{Command, Stdio};
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
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(crate::platform::fs::NOFOLLOW)
        .open(directory.join("server.log"))
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
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log));
    // The managed service survives closing a chat, with its own idle shutdown.
    command.process_group(0);
    let mut child = command
        .spawn()
        .context("could not start Dial Laya service")?;
    let exited = Arc::new(AtomicBool::new(false));
    let completed = exited.clone();
    std::thread::spawn(move || {
        let _ = child.wait();
        completed.store(true, std::sync::atomic::Ordering::SeqCst);
    });
    Ok(exited)
}

/// Launch only after a refused connection; a loading or unhealthy service is never duplicated.
/// Uvicorn reserves the port before loading weights, including across concurrent Dial launches.
pub fn ensure_service(
    config: &Config,
    cancel: Arc<AtomicBool>,
    explicit_start: bool,
) -> Result<Health> {
    ensure_service_with(
        config,
        cancel.clone(),
        explicit_start,
        || health(config, cancel.clone()),
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
    let initial = probe();
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
    let deadline = Instant::now()
        + Duration::from_secs(if explicit_start {
            30
        } else {
            config.timeout_secs
        });
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

pub fn predict(
    config: &Config,
    phase: &str,
    state: &str,
    cancel: Arc<AtomicBool>,
) -> Result<Decision> {
    config.validate()?;
    let start = std::time::Instant::now();
    let health = ensure_service(config, cancel.clone(), false)?;
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
    let requested = questions(phase);
    let prediction_error = |error| {
        if matches!(error, ureq::Error::Timeout(_)) {
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
        .map_err(&prediction_error)?;
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
    for (id, question) in requested.as_object().unwrap() {
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

pub fn summary(phase: &str, answers: &Value) -> String {
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
            .is_err()
        );
    }
    #[test]
    fn foreign_service_is_rejected_instead_of_becoming_a_dependency() {
        let foreign = json!({"service":"another-agent","protocol":1,"status":"ok","checkpoint":"external","device":"cpu"});
        assert!(decode_health(&foreign.to_string()).is_err());
        let mut own = foreign;
        own["service"] = json!("dial-laya");
        assert_eq!(
            decode_health(&own.to_string()).unwrap().service,
            "dial-laya"
        );
        own["protocol"] = json!(2);
        assert!(decode_health(&own.to_string()).is_err());
    }
    fn ready() -> Health {
        Health {
            busy: false,
            status: "ok".into(),
            checkpoint: "test-checkpoint".into(),
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
