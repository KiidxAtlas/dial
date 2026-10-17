//! Evaluation-gated, model-scoped experimental activation steering. No admission from model claims.
use crate::config::types::Config as AppConfig;
use crate::learn::{compute, worker};
use crate::llm::steering::{check, local_unsupported_reason, read, strength, vector_path};
use crate::platform::atomic::write_private;
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
};
/// Imported or candidate vectors live beside the active one until their own evaluation passes.
pub fn pending_path(config: &AppConfig, name: &str) -> Result<PathBuf> {
    ensure!(
        !name.is_empty()
            && name.len() <= 64
            && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'),
        "pending steering name must be 1-64 letters, digits or '-'"
    );
    Ok(config
        .state_dir
        .join("steering")
        .join("pending")
        .join(format!("{name}.json")))
}

/// Hold the vector's mutation lease before reading admission or replacement state.
/// Import, evaluation, automatic creation and promotion share this lock identity.
pub(crate) fn lock_vector(path: &Path) -> Result<fs::File> {
    crate::platform::atomic::private_dir(path.parent().context("steering directory")?)?;
    let lock = crate::learn::auto::lock_file(&path.with_extension("job.lock"))?;
    fs2::FileExt::try_lock_exclusive(&lock)
        .context("another steering job already owns this vector")?;
    Ok(lock)
}

/// Make a pending vector active. Requires its own passing local evaluation; never runs unevaluated.
pub fn promote(config: &AppConfig, name: &str, replace: bool) -> Result<Value> {
    let pending = pending_path(config, name)?;
    // Lock both identities before admission checks: evaluation/import may replace pending bytes.
    // Acquisition is nonblocking, so competing operations fail rather than deadlock.
    let _pending_lock = lock_vector(&pending)?;
    let active = vector_path(config);
    let _active_lock = lock_vector(&active)?;
    let bundle = read(&pending).context("no pending steering vector with that name")?;
    ensure!(
        bundle["model_name"] == config.model.as_str(),
        "pending vector belongs to a different model"
    );
    check(&bundle, strength(config, &bundle)).with_context(|| {
        format!(
            "pending vector is not admitted; run `dial steering evaluate --pending {name}` first"
        )
    })?;
    ensure!(
        replace || !active.exists(),
        "an active steering vector exists at {}; pass --replace to promote over it",
        active.display()
    );
    fs::rename(&pending, &active)?;
    Ok(json!({"promoted": name, "vector": config.steering.vector, "path": active}))
}
fn owned_mlx(config: &AppConfig) -> bool {
    config.endpoint.is_none() && config.provider.as_deref().is_none_or(|p| p == "mlx")
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
                check(bundle, strength(config, bundle))
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
    crate::platform::atomic::private_dir(path.parent().unwrap())?;
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
            let Ok(record) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
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
    let publish = |value: &Value| write_private(&status_path, &serde_json::to_vec(value)?);
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
    let _lock = lock_vector(&path)?;
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
    let mut state = json!({"phase":"extracting candidate; foreground coding has priority","fingerprint":fingerprint,"attempted_at":now,"pairs":pairs.len()});
    publish(&state)?;
    let candidate = path.with_extension("candidate.json");
    let progress = path.with_extension("progress.json");
    let result = (|| -> Result<()> {
        crate::learn::auto::check_stop(config)?;
        // The sweep picks layer and absolute strength by cross-validated pair margin.
        let extracted = call(
            &json!({"op":"steering_extract","model":config.model,"adapter":adapter,"output":candidate,"progress":progress,"strength":config.steering.strength,"layer":null,"pairs":pairs,"sweep":true}),
        )?;
        let strength = extracted["strength"]
            .as_f64()
            .context("sweep returned no strength")?;
        state["sweep"] = json!({"layer":extracted["layer"],"strength":strength,"margin":extracted["margin"],"baseline_margin":extracted["baseline_margin"]});
        state["phase"] = json!("evaluating held-out tool contracts");
        publish(&state)?;
        crate::learn::auto::check_stop(config)?;
        call(
            &json!({"op":"steering_evaluate","model":config.model,"adapter":adapter,"output":candidate,"progress":progress,"strength":strength,"cases":automatic_cases(),"max_tokens":128}),
        )?;
        let mut bundle = read(&candidate)?;
        check(&bundle, strength)?;
        ensure!(
            bundle["model_name"] == config.model && bundle["layer"] == extracted["layer"],
            "candidate model or layer changed"
        );
        if let Ok(incumbent) = read(&path)
            && incumbent["automatic_adapter"] == json!(adapter)
            && let Some(incumbent_strength) = incumbent["evaluation"]["strength"].as_f64()
        {
            // Each vector competes at its own admitted strength.
            let incumbent_path = path.with_extension("incumbent.json");
            write_private(&incumbent_path, &serde_json::to_vec(&incumbent)?)?;
            let evaluation = call(
                &json!({"op":"steering_evaluate","model":config.model,"adapter":adapter,"output":incumbent_path,"progress":path.with_extension("incumbent.progress.json"),"strength":incumbent_strength,"cases":automatic_cases(),"max_tokens":128}),
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
        write_private(&path, &serde_json::to_vec(&bundle)?)?;
        Ok(())
    })();
    // Final state is published below even when the job was cancelled after admission.
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
    run_at(config, kind, input, layer, adapter, &vector_path(config))
}
/// Maintenance job against an explicit vector file (the active one, or a pending import).
pub fn run_at(
    config: &AppConfig,
    kind: &str,
    input: &Path,
    layer: Option<usize>,
    adapter: Option<&Path>,
    path: &Path,
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
    let _lock = lock_vector(path)?;
    let progress_path = path.with_extension("progress.json");
    if !progress_path.exists() && config.compute_locks {
        write_private(
            &progress_path,
            b"{\"phase\":\"queued; waiting for idle compute\"}",
        )?;
    }
    if config.compute_locks {
        eprintln!(
            "Steering {kind}: waiting for idle compute; checkpoints: {}. Foreground coding preempts this job. Ctrl+C stops; rerun resumes.",
            progress_path.display()
        );
    }
    let cancel = crate::platform::interrupt::install_sigint();
    let _lease = compute::Background::acquire_controlled(config, Some(&cancel))?;
    if config.compute_locks {
        compute::register(
            &config.user_dir,
            &format!("steering {kind}"),
            &config.project_root,
            &config.state_dir,
            &config.model,
        )?;
    }
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
    out.push_str(&format!("\n{} → Activation steering: automatic creation, manual vector, strength.\nMaintenance: dial steering extract --pairs FILE --layer N; dial steering evaluate --cases FILE.\nExperimental response contracts; full coding accuracy has not been certified.", crate::learn::improve::SETTINGS_PATH));
    out
}

#[cfg(test)]
mod tests;
