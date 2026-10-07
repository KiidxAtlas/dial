//! Evaluation-gated, model-scoped experimental activation steering. No admission from model claims.
use crate::{
    config::types::Config as AppConfig,
    learn::{compute, worker},
    storage::settings_store,
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
};
const MAX_BYTES: usize = 4 * 1024 * 1024;
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub enabled: bool,
    /// Build and admit vectors during automatic improvement.
    pub automatic: bool,
    pub vector: String,
    pub strength: f64,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: false,
            automatic: true,
            vector: "coding".into(),
            strength: 0.5,
        }
    }
}
impl Config {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.vector.is_empty()
                && self.vector.len() <= 64
                && self
                    .vector
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_')),
            "steering vector name must use 1–64 letters, digits, - or _"
        );
        ensure!(
            self.strength.is_finite() && (0.0..=2.0).contains(&self.strength),
            "steering strength must be 0..2"
        );
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Specification {
    pub path: PathBuf,
    pub id: String,
    pub strength: f64,
    pub fingerprint: String,
}
pub fn vector_path(config: &AppConfig) -> PathBuf {
    config
        .state_dir
        .join("steering")
        .join(format!("{}.json", config.steering.vector))
}
fn read(path: &Path) -> Result<Value> {
    let mut data = Vec::new();
    fs::File::open(path)?
        .take((MAX_BYTES + 1) as u64)
        .read_to_end(&mut data)?;
    ensure!(data.len() <= MAX_BYTES, "steering JSON exceeds 4 MiB");
    Ok(serde_json::from_slice(&data)?)
}
fn check(bundle: &Value, strength: f64) -> Result<()> {
    ensure!(bundle["version"] == 1, "unsupported steering format");
    let size = bundle["dimension"]
        .as_u64()
        .context("missing hidden dimension")?;
    ensure!(
        (1..=32768).contains(&size),
        "hidden dimension exceeds budget"
    );
    let layers = bundle["layer_count"]
        .as_u64()
        .context("missing layer count")?;
    ensure!(
        (1..=256).contains(&layers) && bundle["layer"].as_u64().is_some_and(|n| n < layers),
        "invalid layer selection"
    );
    let vector = bundle["vector"].as_array().context("missing vector")?;
    ensure!(vector.len() == size as usize, "vector dimension mismatch");
    let mut norm = 0.;
    for x in vector {
        let x = x.as_f64().context("non-numeric vector")?;
        ensure!(x.is_finite(), "nonfinite vector");
        norm += x * x;
    }
    ensure!(
        (0.99..=1.01).contains(&norm),
        "vector must have unit L2 norm"
    );
    ensure!(
        bundle["pairs"]
            .as_u64()
            .is_some_and(|n| (8..=64).contains(&n)),
        "insufficient contrast pairs"
    );
    ensure!(
        bundle["id"]
            .as_str()
            .is_some_and(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())),
        "invalid artifact ID"
    );
    let e = &bundle["evaluation"];
    let cases = e["cases"].as_u64().unwrap_or(0);
    let base = e["baseline_passes"].as_u64().unwrap_or(33);
    let steered = e["steered_passes"].as_u64().unwrap_or(0);
    ensure!(
        e["passed"] == true
            && e["scope"] == "held-out response contracts"
            && e["artifact"] == bundle["id"]
            && e["strength"].as_f64() == Some(strength),
        "awaiting passing held-out evaluation at this exact strength"
    );
    ensure!(
        (8..=32).contains(&cases)
            && steered <= cases
            && base <= cases
            && steered > base
            && steered as f64 / cases as f64 >= 0.8,
        "evaluation did not improve response contracts"
    );
    ensure!(
        e["tool_nonregression"] == true
            && e["repetition_nonregression"] == true
            && e["latency_nonregression"] == true,
        "evaluation regression gate failed"
    );
    Ok(())
}
pub fn specification(config: &AppConfig, model: &str) -> Result<Option<Specification>> {
    if (!config.steering.enabled && !config.steering.automatic) || config.steering.strength == 0.0 {
        return Ok(None);
    }
    if let Some(reason) = local_unsupported_reason(model) {
        anyhow::bail!(reason);
    }
    config.steering.validate()?;
    let path = vector_path(config);
    if config.steering.automatic && !path.exists() {
        return Ok(None);
    }
    let bundle = read(&path).map_err(|error| {
        if error.downcast_ref::<std::io::Error>().is_some_and(|error| {
            error.kind() == std::io::ErrorKind::NotFound
        }) {
            anyhow::anyhow!(
                "selected vector '{}' is missing at {}. Extract and evaluate a candidate before enabling steering, or disable it in Settings > Model > Activation steering. Coding continues without steering",
                config.steering.vector,
                path.display()
            )
        } else {
            error.context(format!("cannot read steering vector at {}", path.display()))
        }
    })?;
    if !config.steering.enabled && bundle["origin"] != "automatic verified repairs" {
        return Ok(None);
    }
    ensure!(
        bundle["model_name"] == model,
        "vector belongs to a different model"
    );
    check(&bundle, config.steering.strength)?;
    let fingerprint = format!("{:x}", Sha256::digest(serde_json::to_vec(&bundle)?));
    Ok(Some(Specification {
        path,
        id: bundle["id"].as_str().unwrap().into(),
        strength: config.steering.strength,
        fingerprint,
    }))
}
fn owned_mlx(config: &AppConfig) -> bool {
    config.endpoint.is_none() && config.provider.as_deref().is_none_or(|p| p == "mlx")
}
/// Match the supervisor's runner choice without starting Python or loading weights.
pub(crate) fn local_unsupported_reason(_model: &str) -> Option<&'static str> {
    // Both managed engines have activation hooks. Python probes decoder layers and dimensions
    // before applying an evaluated vector; hosted/external endpoints remain unsupported.
    None
}
fn unsupported_reason(config: &AppConfig) -> Option<&'static str> {
    if !owned_mlx(config) {
        return Some("hosted/external endpoints expose no Dial-owned activation hook");
    }
    let model = crate::llm::catalog::resolve_local_model(&config.model)
        .unwrap_or_else(|_| config.model.clone());
    local_unsupported_reason(&model)
}
pub fn status(config: &AppConfig) -> Value {
    let path = vector_path(config);
    let progress = read(&path.with_extension("progress.json")).ok();
    let automatic = read(&path.with_extension("automatic.json")).ok();
    let candidate = read(&path);
    let (state, reason) = if let Some(reason) = unsupported_reason(config) {
        ("unsupported", reason.to_owned())
    } else {
        match &candidate {
            Err(error) => {
                if path.exists() {
                    ("invalid vector", error.to_string())
                } else {
                    (
                        "no vector",
                        if config.steering.automatic {
                            "automatic improvement is collecting verified repair contrasts".into()
                        } else {
                            "extract a contrastive candidate first".into()
                        },
                    )
                }
            }
            Ok(bundle) => match (|| {
                ensure!(
                    bundle["model_name"] == config.model,
                    "vector belongs to a different model"
                );
                check(bundle, config.steering.strength)
            })() {
                Err(error) => ("awaiting evaluation", error.to_string()),
                Ok(()) => {
                    if (config.steering.enabled
                        || (config.steering.automatic
                            && bundle["origin"] == "automatic verified repairs"))
                        && config.steering.strength > 0.0
                    {
                        (
                            "configured",
                            "runtime confirmation is required; configured is not used".into(),
                        )
                    } else {
                        ("evaluated", "steering is disabled".into())
                    }
                }
            },
        }
    };
    json!({"automatic":automatic,"automatic_enabled":config.steering.automatic,"state":state,"reason":reason,"enabled":config.steering.enabled,"vector":config.steering.vector,"strength":config.steering.strength,"path":path,"progress":progress,"candidate":candidate.ok().map(|b|json!({"id":b["id"],"model":b["model_name"],"layer":b["layer"],"dimension":b["dimension"],"pairs":b["pairs"],"evaluation":b["evaluation"]})),"scope":"experimental held-out response contracts; no full coding accuracy certification"})
}
/// A bounded scheduled job using the improvement worker's existing compute lease.
/// Only harvested, verified training repairs contribute contrast pairs.
pub fn automatic(
    runtime: &mut crate::agent::Runtime,
    config: &AppConfig,
    store: &crate::storage::store::Store,
    adapter: Option<&Path>,
) -> Result<Value> {
    automatic_with(config, store, adapter, &mut |request| {
        // Release the cycle's resident inference weights before loading the extraction worker.
        runtime.release();
        worker::call_controlled(config, request, None)
    })
}
fn automatic_with(
    config: &AppConfig,
    store: &crate::storage::store::Store,
    adapter: Option<&Path>,
    call: &mut dyn FnMut(&Value) -> Result<Value>,
) -> Result<Value> {
    if !config.steering.automatic || !owned_mlx(config) || config.steering.strength == 0.0 {
        return Ok(json!({"phase":"disabled or unsupported"}));
    }
    if let Some(reason) = unsupported_reason(config) {
        return Ok(json!({"phase":"unsupported","reason":reason}));
    }
    let path = vector_path(config);
    settings_store::private_dir(path.parent().unwrap())?;
    let status_path = path.with_extension("automatic.json");
    let allowed = crate::learn::harvest::steering_pairs(store)?;
    let mut pairs = Vec::new();
    let mut seen = std::collections::HashSet::new();
    if let Ok(mut file) = fs::File::open(&config.training.train_data) {
        use std::io::{BufRead, BufReader, Seek, SeekFrom};
        let offset = file.metadata()?.len().saturating_sub(16 * 1024 * 1024);
        file.seek(SeekFrom::Start(offset))?;
        let mut reader = BufReader::new(file.take(16 * 1024 * 1024));
        if offset > 0 {
            let mut partial = String::new();
            reader.read_line(&mut partial)?;
        }
        let mut recent = std::collections::VecDeque::new();
        for line in reader.lines() {
            recent.push_back(line?);
            if recent.len() > 4000 {
                recent.pop_front();
            }
        }
        // New verified experience must be able to replace older contrast prompts.
        for line in recent.into_iter().rev() {
            let record: Value = serde_json::from_str(&line)?;
            for pair in record["preference_pairs"].as_array().into_iter().flatten() {
                if !allowed.contains(&serde_json::to_string(pair)?) {
                    continue;
                }
                if let Some(pair) = contrast_pair(pair) {
                    let key = serde_json::to_string(
                        &pair["positive"].as_array().unwrap()
                            [..pair["positive"].as_array().unwrap().len() - 1],
                    )?;
                    if seen.insert(key) {
                        pairs.push(pair);
                    }
                }
                if pairs.len() == 32 {
                    break;
                }
            }
            if pairs.len() == 32 {
                break;
            }
        }
    }
    let publish =
        |value: &Value| settings_store::write_private(&status_path, &serde_json::to_vec(value)?);
    if pairs.len() < 8 {
        let state = json!({"phase":format!("collecting verified repairs: {}/8 distinct prompts",pairs.len()),"pairs":pairs.len()});
        publish(&state)?;
        return Ok(state);
    }
    let fingerprint = format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&json!([
            pairs,
            config.model,
            adapter,
            config.steering.strength
        ]))?)
    );
    if path.exists() {
        let bundle =
            read(&path).context("existing vector is unreadable; preserved without replacement")?;
        if bundle["origin"] != "automatic verified repairs" {
            let state = json!({"phase":"manual vector preserved; automatic replacement disabled for this vector name"});
            publish(&state)?;
            return Ok(state);
        }
    }
    let previous = read(&status_path).unwrap_or(Value::Null);
    let now = crate::learn::improve::now();
    if previous["finished"] == true
        && previous["fingerprint"] == fingerprint
        && now.saturating_sub(previous["attempted_at"].as_u64().unwrap_or(0)) < 86400
    {
        return Ok(previous);
    }
    let lock = crate::learn::auto::lock_file(&path.with_extension("job.lock"))?;
    fs2::FileExt::try_lock_exclusive(&lock).context("another steering job owns this vector")?;
    let mut state = json!({"phase":"extracting candidate; foreground coding has priority","fingerprint":fingerprint,"attempted_at":now,"pairs":pairs.len()});
    publish(&state)?;
    let candidate = path.with_extension("candidate.json");
    let progress = path.with_extension("progress.json");
    let result = (|| -> Result<()> {
        crate::learn::auto::check_stop(config)?;
        call(
            &json!({"op":"steering_extract","model":config.model,"adapter":adapter,"output":candidate,"progress":progress,"strength":config.steering.strength,"layer":null,"pairs":pairs}),
        )?;
        state["phase"] = json!("evaluating held-out tool contracts");
        publish(&state)?;
        crate::learn::auto::check_stop(config)?;
        call(
            &json!({"op":"steering_evaluate","model":config.model,"adapter":adapter,"output":candidate,"progress":progress,"strength":config.steering.strength,"cases":automatic_cases(),"max_tokens":128}),
        )?;
        let mut bundle = read(&candidate)?;
        check(&bundle, config.steering.strength)?;
        ensure!(
            bundle["model_name"] == config.model,
            "candidate model changed"
        );
        if let Ok(incumbent) = read(&path)
            && incumbent["automatic_adapter"] == json!(adapter)
            && incumbent["evaluation"]["strength"] == config.steering.strength
        {
            let incumbent_path = path.with_extension("incumbent.json");
            settings_store::write_private(&incumbent_path, &serde_json::to_vec(&incumbent)?)?;
            let evaluation = call(
                &json!({"op":"steering_evaluate","model":config.model,"adapter":adapter,"output":incumbent_path,"progress":path.with_extension("incumbent.progress.json"),"strength":config.steering.strength,"cases":automatic_cases(),"max_tokens":128}),
            )?;
            ensure!(
                bundle["evaluation"]["steered_passes"].as_u64().unwrap_or(0)
                    >= evaluation["steered_passes"]
                        .as_u64()
                        .context("incumbent evaluation missing score")?,
                "candidate regressed from the active vector"
            );
        }
        bundle["automatic_adapter"] = json!(adapter);
        bundle["origin"] = json!("automatic verified repairs");
        crate::learn::auto::check_stop(config)?;
        // Atomic publication keeps the last admitted vector intact on failure/cancellation.
        settings_store::write_private(&path, &serde_json::to_vec(&bundle)?)?;
        Ok(())
    })();
    crate::learn::auto::check_stop(config)?;
    state["finished"] = json!(true);
    state["phase"] = json!(match result {
        Ok(()) => "admitted; runtime confirmation pending".to_owned(),
        Err(error) => format!("candidate not activated: {error:#}"),
    });
    publish(&state)?;
    Ok(state)
}
fn contrast_pair(pair: &Value) -> Option<Value> {
    let context = pair["context"].as_array()?;
    if context.is_empty() || context.len() >= 8 || !context.iter().any(|m| m["role"] == "user") {
        return None;
    }
    if context
        .iter()
        .filter(|m| m["role"] == "user")
        .any(|message| {
            automatic_cases()
                .iter()
                .any(|case| case["messages"][0]["content"] == message["content"])
        })
    {
        return None;
    }
    if pair["chosen"] == pair["rejected"] {
        return None;
    }
    let mut positive = context.clone();
    positive.push(pair["chosen"].clone());
    let mut negative = context.clone();
    negative.push(pair["rejected"].clone());
    if serde_json::to_vec(&json!([positive, negative])).ok()?.len() > 16000 {
        return None;
    }
    Some(json!({"positive":positive,"negative":negative,"tools":pair["tools"]}))
}
fn automatic_cases() -> Vec<Value> {
    (0..8).map(|n| {
        let path = format!("src/steering_holdout_{n}.rs");
        let (name, properties, arguments, instruction) = match n % 4 {
            0 => ("read", json!({"path":{"type":"string"}}), json!({"path":path}), format!("Read {path}")),
            1 => ("write", json!({"path":{"type":"string"},"content":{"type":"string"}}), json!({"path":path,"content":"// checked"}), format!("Write exactly // checked to {path}")),
            2 => ("grep", json!({"pattern":{"type":"string"}}), json!({"pattern":format!("heldout_symbol_{n}")}), format!("Search for heldout_symbol_{n}")),
            _ => ("glob", json!({"pattern":{"type":"string"}}), json!({"pattern":format!("src/heldout_{n}/*.rs")}), format!("Find files matching src/heldout_{n}/*.rs")),
        };
        let required = arguments.as_object().unwrap().keys().cloned().collect::<Vec<_>>();
        json!({"messages":[{"role":"user","content":format!("{instruction}. Use the {name} tool; return one tool call with the exact requested arguments.")}],"tools":[{"type":"function","function":{"name":name,"description":instruction,"parameters":{"type":"object","properties":properties,"required":required}}}],"expected_tool":name,"required_arguments":required,"expected_arguments":arguments})
    }).collect()
}

/// Explicit maintenance job. Progress/checkpoints are outside source; interrupted jobs resume.
pub fn run(
    config: &AppConfig,
    kind: &str,
    input: &Path,
    layer: Option<usize>,
    adapter: Option<&Path>,
) -> Result<Value> {
    ensure!(
        owned_mlx(config),
        "activation steering currently supports Dial-owned MLX only"
    );
    if let Some(reason) = unsupported_reason(config) {
        anyhow::bail!(reason);
    }
    config.steering.validate()?;
    let data = read(input)?;
    let rows = data
        .as_array()
        .context("steering dataset must be a JSON array")?;
    match kind {
        "extract" => {
            ensure!(
                (8..=64).contains(&rows.len()),
                "extraction requires 8–64 matched pairs"
            );
            ensure!(
                layer.is_some_and(|n| n < 256),
                "extraction needs a zero-based --layer below 256"
            );
        }
        "evaluate" => {
            ensure!(
                (8..=32).contains(&rows.len()),
                "evaluation requires 8–32 held-out cases"
            );
            ensure!(
                config.steering.strength > 0.,
                "evaluation needs a positive steering strength"
            );
        }
        _ => anyhow::bail!("unknown steering operation"),
    }
    let adapter = adapter
        .map(fs::canonicalize)
        .transpose()
        .context("adapter must exist locally")?;
    ensure!(
        adapter.as_ref().is_none_or(|p| p.is_dir()),
        "adapter must be a directory"
    );
    let path = vector_path(config);
    settings_store::private_dir(path.parent().unwrap())?;
    let lock = crate::learn::auto::lock_file(&path.with_extension("job.lock"))?;
    fs2::FileExt::try_lock_exclusive(&lock)
        .context("another steering job already owns this vector")?;
    let progress_path = path.with_extension("progress.json");
    if !progress_path.exists() {
        settings_store::write_private(
            &progress_path,
            b"{\"phase\":\"queued; waiting for idle compute\"}",
        )?;
    }
    eprintln!(
        "Steering {kind}: waiting for idle compute; checkpoints: {}. Foreground coding preempts this job. Ctrl+C stops; rerun resumes.",
        progress_path.display()
    );
    let cancel = crate::repl::install_sigint();
    let _lease = compute::Background::acquire_controlled(config, Some(&cancel))?;
    compute::register(
        &config.user_dir,
        &format!("steering {kind}"),
        &config.project_root,
        &config.state_dir,
        &config.model,
    )?;
    let mut request = json!({"op":format!("steering_{kind}"),"model":config.model,"adapter":adapter,"output":path,"progress":progress_path,"strength":config.steering.strength});
    if kind == "extract" {
        request["pairs"] = data;
        request["layer"] = json!(layer.context("extraction needs --layer")?);
    } else {
        ensure!(kind == "evaluate", "unknown steering operation");
        request["cases"] = data;
        request["max_tokens"] = json!(128);
    }
    worker::call_controlled(config, &request, Some(&cancel))
}
pub fn render(value: &Value) -> String {
    let text = |key: &str| value[key].as_str().unwrap_or("unconfirmed");
    let mut out = format!(
        "{} · {} · strength {}\n{}",
        text("state"),
        text("vector"),
        value["strength"],
        text("reason")
    );
    if let Some(candidate) = value["candidate"].as_object() {
        out.push_str(&format!(
            "\nModel: {} · layer {} · hidden {} · {} contrast pairs",
            candidate["model"].as_str().unwrap_or("unknown"),
            candidate["layer"],
            candidate["dimension"],
            candidate["pairs"]
        ));
        let evaluation = &candidate["evaluation"];
        if evaluation.is_object() {
            out.push_str(&format!(
                "\nHeld-out contracts: baseline {}/{} → steered {}/{} · admitted {}",
                evaluation["baseline_passes"],
                evaluation["cases"],
                evaluation["steered_passes"],
                evaluation["cases"],
                evaluation["passed"]
            ));
        }
    }
    if let Some(auto) = value["automatic"].as_object() {
        out.push_str(&format!(
            "\nAutomatic: {}",
            auto.get("phase")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
        ));
    }
    let progress = &value["progress"];
    if progress.is_object() {
        out.push_str(&format!(
            "\nJob: {} · {}/{} units checkpointed",
            progress["phase"].as_str().unwrap_or("unknown"),
            progress["completed"].as_u64().unwrap_or(0),
            progress["total"].as_u64().unwrap_or(0)
        ));
    }
    let runtime = &value["runtime"];
    out.push_str(&format!(
        "\nThis session: {} · {} steered forward passes",
        runtime["state"]
            .as_str()
            .unwrap_or("no runtime confirmation"),
        runtime["uses"].as_u64().unwrap_or(0)
    ));
    if let Some(reason) = runtime["reason"].as_str() {
        out.push_str(&format!("\n{reason}"));
    }
    out.push_str("\n/settings → Model → Activation steering: automatic creation, manual vector, strength.\nMaintenance: dial steering extract --pairs FILE --layer N; dial steering evaluate --cases FILE.\nExperimental response contracts; full coding accuracy has not been certified.");
    out
}
pub fn runtime(base_url: &str) -> Result<Value> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(1)))
        .build()
        .into();
    let mut response = agent.get(&format!("{base_url}/dial/steering")).call()?;
    let raw = response
        .body_mut()
        .with_config()
        .limit(65536)
        .read_to_string()?;
    Ok(serde_json::from_str(&raw)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn candidate() -> Value {
        json!({"version":1,"id":"a".repeat(64),"model_name":"test","dimension":2,"layer":0,"layer_count":2,"vector":[1.0,0.0],"pairs":8,"evaluation":{"passed":true,"artifact":"a".repeat(64),"strength":0.5,"cases":8,"baseline_passes":4,"steered_passes":7,"scope":"held-out response contracts","tool_nonregression":true,"repetition_nonregression":true,"latency_nonregression":true}})
    }
    fn auto_fixture() -> (tempfile::TempDir, AppConfig, crate::storage::store::Store) {
        let dir = tempfile::tempdir().unwrap();
        let config = AppConfig {
            model: "test".into(),
            state_dir: dir.path().join("state"),
            ..Default::default()
        };
        let store = crate::storage::store::Store::open(
            &config.state_dir,
            "test",
            "case",
            &Default::default(),
        )
        .unwrap();
        (dir, config, store)
    }
    fn seed_pairs(config: &mut AppConfig, store: &crate::storage::store::Store) {
        seed_count(config, store, 8);
    }
    fn seed_count(config: &mut AppConfig, store: &crate::storage::store::Store, count: usize) {
        config.training.train_data = config.state_dir.join("train.jsonl");
        let version = store.active().unwrap().id;
        let mut lines = String::new();
        for n in 0..count {
            let context = json!([{"role":"user","content":format!("repair prompt {n}")}]);
            let tools = json!([{"type":"function","function":{"name":"read","parameters":{"type":"object"}}}]);
            let chosen = json!({"role":"assistant","content":format!("corrected {n}")});
            let rejected = json!({"role":"assistant","content":format!("incorrect {n}")});
            let id = store
                .begin_trajectory(version, "selfplay:files", "task", "prompt")
                .unwrap();
            store.tool_recovery(id,version,&json!({"context":context,"tools":tools,"corrected":chosen,"rejected":[rejected]})).unwrap();
            lines.push_str(&json!({"preference_pairs":[{"context":context,"tools":tools,"chosen":chosen,"rejected":rejected}]}).to_string());
            lines.push('\n');
        }
        fs::write(&config.training.train_data, lines).unwrap();
    }
    #[test]
    fn automatic_prefers_newest_verified_repairs_with_a_bounded_pair_budget() {
        let (_dir, mut config, store) = auto_fixture();
        seed_count(&mut config, &store, 40);
        let mut observed = false;
        automatic_with(&config, &store, None, &mut |request| {
            assert_eq!(request["op"], "steering_extract");
            let pairs = request["pairs"].as_array().unwrap();
            assert_eq!(pairs.len(), 32);
            assert_eq!(pairs[0]["positive"][0]["content"], "repair prompt 39");
            assert_eq!(pairs[31]["positive"][0]["content"], "repair prompt 8");
            observed = true;
            anyhow::bail!("stop fixture before inference")
        })
        .unwrap();
        assert!(observed);
    }
    #[test]
    fn automatic_waits_for_provenance_and_never_loads_model_without_data() {
        let (_dir, mut config, store) = auto_fixture();
        config.training.train_data = config.state_dir.join("unverified.jsonl");
        fs::write(&config.training.train_data, json!({"preference_pairs":[{"context":[{"role":"user","content":"unverified"}],"chosen":{"role":"assistant","content":"good"},"rejected":{"role":"assistant","content":"bad"}}]}).to_string()).unwrap();
        let result = automatic_with(&config, &store, None, &mut |_| {
            panic!("no compute should start")
        })
        .unwrap();
        assert_eq!(result["pairs"], 0);
        assert!(!vector_path(&config).exists());
        config.steering.automatic = false;
        assert_eq!(
            automatic_with(&config, &store, None, &mut |_| panic!("disabled")).unwrap()["phase"],
            "disabled or unsupported"
        );
    }
    #[test]
    fn vision_models_keep_the_evaluation_gate_and_normal_dataset_validation() {
        let (dir, mut config, _store) = auto_fixture();
        let model = dir.path().join("vision-model");
        fs::create_dir(&model).unwrap();
        fs::write(model.join("config.json"), r#"{"vision_config":{}}"#).unwrap();
        config.model = model.to_string_lossy().into_owned();
        config.steering.enabled = true;
        config.steering.automatic = false;
        let path = vector_path(&config);
        settings_store::private_dir(path.parent().unwrap()).unwrap();
        let mut vector = candidate();
        vector["model_name"] = json!(config.model);
        vector["origin"] = json!("automatic verified repairs");
        settings_store::write_private(&path, &serde_json::to_vec(&vector).unwrap()).unwrap();
        assert_eq!(status(&config)["state"], "configured");
        assert!(specification(&config, &config.model).unwrap().is_some());
        // Eligible vision models still reject missing datasets before starting any worker.
        assert!(
            run(
                &config,
                "extract",
                &dir.path().join("missing.json"),
                None,
                None
            )
            .is_err()
        );
        vector["evaluation"]["passed"] = json!(false);
        settings_store::write_private(&path, &serde_json::to_vec(&vector).unwrap()).unwrap();
        assert!(specification(&config, &config.model).is_err());
    }
    #[test]
    fn automatic_admits_only_passing_candidates_and_preserves_last_good_on_failure() {
        let (_dir, mut config, store) = auto_fixture();
        seed_pairs(&mut config, &store);
        let mut calls = 0;
        let result = automatic_with(&config, &store, None, &mut |request| {
            calls += 1;
            if request["op"] == "steering_extract" {
                assert!(request["layer"].is_null());
                assert_eq!(request["pairs"].as_array().unwrap().len(), 8);
            } else {
                assert_eq!(request["cases"].as_array().unwrap().len(), 8);
            }
            settings_store::write_private(
                Path::new(request["output"].as_str().unwrap()),
                &serde_json::to_vec(&candidate())?,
            )?;
            Ok(candidate()["evaluation"].clone())
        })
        .unwrap();
        assert_eq!(calls, 2);
        assert!(result["phase"].as_str().unwrap().starts_with("admitted"));
        assert!(specification(&config, "test").unwrap().is_some());
        automatic_with(&config, &store, None, &mut |_| {
            panic!("cooldown should skip")
        })
        .unwrap();
        let active = fs::read(vector_path(&config)).unwrap();
        fs::remove_file(vector_path(&config).with_extension("automatic.json")).unwrap();
        let result = automatic_with(&config, &store, None, &mut |request| {
            let mut bad = candidate();
            bad["evaluation"]["passed"] = json!(false);
            settings_store::write_private(
                Path::new(request["output"].as_str().unwrap()),
                &serde_json::to_vec(&bad)?,
            )?;
            Ok(bad["evaluation"].clone())
        })
        .unwrap();
        assert!(result["phase"].as_str().unwrap().contains("not activated"));
        assert_eq!(fs::read(vector_path(&config)).unwrap(), active);
        config.steering.automatic = false;
        assert!(specification(&config, "test").unwrap().is_none());
    }
    #[test]
    fn passing_candidate_cannot_reduce_incumbent_score() {
        let (_dir, mut config, store) = auto_fixture();
        seed_pairs(&mut config, &store);
        let path = vector_path(&config);
        settings_store::private_dir(path.parent().unwrap()).unwrap();
        let mut incumbent = candidate();
        incumbent["origin"] = json!("automatic verified repairs");
        incumbent["automatic_adapter"] = Value::Null;
        incumbent["evaluation"]["steered_passes"] = json!(8);
        settings_store::write_private(&path, &serde_json::to_vec(&incumbent).unwrap()).unwrap();
        let before = fs::read(&path).unwrap();
        let result = automatic_with(&config, &store, None, &mut |request| {
            let output = Path::new(request["output"].as_str().unwrap());
            if output == path.with_extension("incumbent.json") {
                return Ok(incumbent["evaluation"].clone());
            }
            settings_store::write_private(output, &serde_json::to_vec(&candidate())?)?;
            Ok(candidate()["evaluation"].clone())
        })
        .unwrap();
        assert!(result["phase"].as_str().unwrap().contains("regressed"));
        assert_eq!(fs::read(&path).unwrap(), before);
    }
    #[test]
    fn manual_vectors_are_preserved_and_heldout_cases_are_varied_exact_contracts() {
        let (_dir, mut config, store) = auto_fixture();
        seed_pairs(&mut config, &store);
        settings_store::private_dir(vector_path(&config).parent().unwrap()).unwrap();
        settings_store::write_private(
            &vector_path(&config),
            &serde_json::to_vec(&candidate()).unwrap(),
        )
        .unwrap();
        let result = automatic_with(&config, &store, None, &mut |_| {
            panic!("manual vector must be preserved")
        })
        .unwrap();
        assert!(
            result["phase"]
                .as_str()
                .unwrap()
                .contains("manual vector preserved")
        );
        assert!(specification(&config, "test").unwrap().is_none());
        let cases = automatic_cases();
        assert_eq!(cases.len(), 8);
        assert_eq!(
            cases
                .iter()
                .map(|c| c["expected_tool"].as_str().unwrap())
                .collect::<std::collections::HashSet<_>>()
                .len(),
            4
        );
        assert!(
            cases
                .iter()
                .all(|c| !c["expected_arguments"].as_object().unwrap().is_empty())
        );
    }
    #[test]
    fn heldout_prompt_is_excluded_even_with_added_system_context() {
        let cases = automatic_cases();
        let pair = json!({"context":[{"role":"system","content":"follow rules"},cases[0]["messages"][0]],"chosen":{"role":"assistant","content":"good"},"rejected":{"role":"assistant","content":"bad"}});
        assert!(contrast_pair(&pair).is_none());
        let config = AppConfig::default();
        for role in ["architecture", "compaction", "feedback"] {
            assert!(!config.for_role(role).steering.automatic);
        }
    }
    #[test]
    fn disabled_never_requires_a_vector_and_names_cannot_escape_state() {
        let config = AppConfig::default();
        assert!(specification(&config, "test").unwrap().is_none());
        for name in ["../x", "", "x/y"] {
            let config = Config {
                vector: name.into(),
                ..Default::default()
            };
            assert!(config.validate().is_err());
        }
        assert!(
            Config {
                strength: f64::NAN,
                ..Default::default()
            }
            .validate()
            .is_err()
        );
    }
    #[test]
    fn evaluation_must_improve_and_pass_all_gates_at_exact_strength() {
        let original = candidate();
        check(&original, 0.5).unwrap();
        assert!(check(&original, 1.).is_err());
        for (key, value) in [
            ("steered_passes", json!(4)),
            ("cases", json!(7)),
            ("tool_nonregression", json!(false)),
            ("latency_nonregression", json!(false)),
            ("repetition_nonregression", json!(false)),
        ] {
            let mut item = original.clone();
            item["evaluation"][key] = value;
            assert!(check(&item, 0.5).is_err(), "{key}");
        }
    }
    #[test]
    fn missing_vector_explains_path_and_recovery_without_creating_it() {
        let root = tempfile::tempdir().unwrap();
        let config = AppConfig {
            state_dir: root.path().into(),
            steering: Config {
                enabled: true,
                automatic: false,
                ..Default::default()
            },
            ..Default::default()
        };
        let path = vector_path(&config);
        let reason = specification(&config, "test").unwrap_err().to_string();
        assert!(reason.contains("selected vector 'coding' is missing"));
        assert!(reason.contains(&path.display().to_string()));
        assert!(reason.contains("Settings > Model > Activation steering"));
        assert!(!path.exists());
    }
    #[test]
    fn admission_is_model_scoped_and_content_change_restarts_profile() {
        let root = tempfile::tempdir().unwrap();
        let config = AppConfig {
            model: "test".into(),
            state_dir: root.path().into(),
            steering: Config {
                enabled: true,
                ..Default::default()
            },
            ..Default::default()
        };
        let path = vector_path(&config);
        settings_store::private_dir(path.parent().unwrap()).unwrap();
        let mut item = candidate();
        settings_store::write_private(&path, &serde_json::to_vec(&item).unwrap()).unwrap();
        let first = specification(&config, "test").unwrap().unwrap();
        assert!(specification(&config, "another").is_err());
        item["evaluation"]["baseline_passes"] = json!(3);
        settings_store::write_private(&path, &serde_json::to_vec(&item).unwrap()).unwrap();
        assert_ne!(first, specification(&config, "test").unwrap().unwrap());
    }
}
