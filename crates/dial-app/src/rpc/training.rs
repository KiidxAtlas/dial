//! Desktop's explicit training workflow. A training target is independent of the chat route.
use super::{Config, ErrorCode, Layout, Session, coded};
use crate::learn::{auto, compute, improve, session, worker};
use crate::llm::{
    catalog,
    local_format::{self, LocalFormat},
    model_config::ModelSelection,
};
use crate::storage::store::Store;
use anyhow::Result;
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Target {
    model: String,
}

fn target(layout: &Layout, saved: &Config, model: &str) -> Result<Config> {
    if model.trim().is_empty() || model.len() > 8192 {
        return Err(coded(ErrorCode::InvalidParams, "Choose a local checkpoint"));
    }
    session::target_config(layout, saved, &ModelSelection::new("mlx", model))
}

/// Disk-only preflight. Even a supported layout must pass the worker's gradient/reload probe.
fn compatibility(config: &Config) -> (Option<LocalFormat>, Option<&'static str>) {
    let resolved = catalog::resolve_local_model(&config.model).ok();
    let path = Path::new(resolved.as_deref().unwrap_or(&config.model));
    let format = local_format::detect(path);
    let apple_silicon = cfg!(all(target_os = "macos", target_arch = "aarch64"));
    let reason = match format {
        Some(LocalFormat::Gguf) => Some(
            "GGUF weights cannot be trained in place. Use Prepare GGUF to create a trainable FP16 safetensors copy, then train and export a GGUF LoRA (--lora) or fused GGUF.",
        ),
        Some(LocalFormat::Exl3) if cfg!(target_os = "macos") => Some(
            "EXL3 checkpoints require an NVIDIA CUDA GPU (Linux/Windows) with ExLlamaV3/TabbyAPI. Use MLX or GGUF on macOS.",
        ),
        Some(LocalFormat::Exl3) => Some(
            "EXL3 quantized checkpoints serve PEFT LoRA adapters via ExLlamaV3/TabbyAPI, but training requires an unquantized HuggingFace safetensors base checkpoint (then export PEFT or EXL3).",
        ),
        Some(LocalFormat::Splash) => Some(
            "Splash bundles run in LM Studio. Choose the original model’s compatible MLX checkpoint to train it.",
        ),
        None => Some(
            "No complete local checkpoint was found. Download a compatible MLX or safetensors language model in the model library.",
        ),
        Some(LocalFormat::Mlx) if resolved.is_none() => {
            Some("This checkpoint is incomplete. Finish downloading its weights first.")
        }
        Some(LocalFormat::Mlx) if !apple_silicon && config.training.backend == "mlx" => {
            Some("Dial’s MLX trainer requires an Apple Silicon Mac. Set training.backend to auto or peft for PyTorch/PEFT.")
        }
        Some(LocalFormat::Mlx)
            if !config.runs_dial_mlx()
                || !catalog::local_model_metadata(path).kind.supports_chat() =>
        {
            Some("Choose a local language model, not an endpoint or a non-chat checkpoint.")
        }
        Some(LocalFormat::Mlx)
            if apple_silicon
                && config.training.backend != "peft"
                && catalog::local_lora_support(&config.user_dir, path) == Some(false) =>
        {
            Some(
                "The installed MLX runtime does not recognize this architecture. Update the runtime or choose another checkpoint.",
            )
        }
        Some(LocalFormat::Mlx) => None,
    };
    (format, reason)
}

fn resolved_backend(config: &Config) -> &'static str {
    match config.training.backend.as_str() {
        "peft" => "peft",
        "mlx" => "mlx",
        _ => {
            if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
                "mlx"
            } else {
                "peft"
            }
        }
    }
}

pub(super) fn status(layout: &Layout, saved: &Config, params: &Value) -> Result<Value> {
    let request: Target =
        serde_json::from_value(params.clone()).map_err(|e| coded(ErrorCode::InvalidParams, e))?;
    let config = target(layout, saved, &request.model)?;
    let (format, reason) = compatibility(&config);
    let t = &config.training;
    let mut result = json!({
        "format": format,
        "unavailable": reason,
        "enabled": config.improve.enabled && config.training.enabled,
        "backend": resolved_backend(&config),
        "compute_paused": compute::paused(&config.user_dir),
        "paused": false,
        "minimum": improve::min_examples(&config),
        "validation_minimum": t.batch_size,
        "verified_turns": 0,
        "approved_snapshots": 0,
        "job": null,
        "version": null,
        "has_adapter": false,
        "adapter_path": null,
        "config": {
            "backend": t.backend,
            "fine_tune_type": t.fine_tune_type,
            "target_modules": t.target_modules,
            "iters": t.iters,
            "batch_size": t.batch_size,
            "grad_accumulation_steps": t.grad_accumulation_steps,
            "num_layers": t.num_layers,
            "rank": t.rank,
            "scale": t.scale,
            "dropout": t.dropout,
            "learning_rate": t.learning_rate,
            "lr_schedule": t.lr_schedule,
            "warmup_ratio": t.warmup_ratio,
            "optimizer": t.optimizer,
            "weight_decay": t.weight_decay,
            "grad_checkpoint": t.grad_checkpoint,
            "checkpoint_every": t.checkpoint_every,
            "max_seq_length": t.max_seq_length,
            "max_train_examples": t.max_train_examples,
            "max_replay_examples": t.max_replay_examples,
            "max_valid_examples": t.max_valid_examples,
            "gate_mode": config.improve.gate_mode,
        },
        "capabilities": {
            "peft_export": true,
            "gguf_lora_export": true,
            "gguf_fused_export": local_format::convert_hf_to_gguf().is_some(),
            "llama_quantize": local_format::llama_quantize().is_some(),
            "exl3_server": !cfg!(target_os = "macos") && local_format::exl3_server().is_some(),
            "exl3_export": !cfg!(target_os = "macos"),
        },
    });
    if format == Some(LocalFormat::Gguf) {
        let resolved =
            catalog::resolve_local_model(&config.model).unwrap_or_else(|_| config.model.clone());
        result["gguf_file"] = json!(local_format::gguf_file(Path::new(&resolved)));
    }
    if config.state_dir.join("state.sqlite3").try_exists()? {
        let store = Store::open_read_only(&config.state_dir)?;
        let version = store.active()?;
        result["version"] = json!({"id":version.id,"kind":version.kind});
        result["has_adapter"] = json!(version.adapter.is_some());
        result["adapter_path"] = json!(version.adapter);
        result["verified_turns"] = json!(store.verified_live_turns()?);
        result["approved_snapshots"] = json!(store.session_example_count()?);
        result["job"] = auto::model_job(&config.state_dir, &store)?;
        result["paused"] = json!(
            result["job"]["stop_requested"] == true
                || store.get_meta("auto_enabled")?.as_deref() == Some("false")
        );
    }
    Ok(result)
}

#[derive(Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
enum Operation {
    Approve,
    Start,
    Stop,
    Resume,
    Fuse,
    ExportPeft,
    ExportGgufLora,
    ExportGguf,
    ExportExl3,
    PrepareGguf,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Action {
    model: String,
    action: Operation,
    confirm: bool,
    session_id: Option<String>,
    output: Option<String>,
    version: Option<i64>,
    dequantize: Option<bool>,
    dtype: Option<String>,
    quant: Option<String>,
    bpw: Option<f64>,
}

fn require_output(raw: Option<&str>) -> Result<PathBuf> {
    let text = raw
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| coded(ErrorCode::InvalidParams, "Specify a new output path"))?;
    let path = PathBuf::from(text);
    if !path.is_absolute() {
        return Err(coded(
            ErrorCode::InvalidParams,
            "Output path must be absolute",
        ));
    }
    if path.exists() {
        return Err(coded(
            ErrorCode::Conflict,
            "Output path already exists; choose a new path",
        ));
    }
    Ok(path)
}

pub(super) fn action(
    layout: &Layout,
    saved: &Config,
    source: &Session,
    params: &Value,
) -> Result<Value> {
    let request: Action =
        serde_json::from_value(params.clone()).map_err(|e| coded(ErrorCode::InvalidParams, e))?;
    if !request.confirm || (request.action == Operation::Approve) != request.session_id.is_some() {
        return Err(coded(
            ErrorCode::InvalidParams,
            "Confirm the operation and identify the source session only for approval",
        ));
    }
    if request
        .session_id
        .as_deref()
        .is_some_and(|id| id != source.id())
    {
        return Err(coded(
            ErrorCode::Conflict,
            "The session changed. Review the examples again.",
        ));
    }
    let config = target(layout, saved, &request.model)?;
    if request.action == Operation::PrepareGguf {
        let output = require_output(request.output.as_deref())?;
        let resolved =
            catalog::resolve_local_model(&config.model).unwrap_or_else(|_| config.model.clone());
        let gguf = local_format::gguf_file(Path::new(&resolved)).ok_or_else(|| {
            coded(
                ErrorCode::Refused,
                "Selected model is not a local GGUF checkpoint",
            )
        })?;
        return worker::prepare_gguf(&config, &gguf, &output)
            .map_err(|e| coded(ErrorCode::Refused, e));
    }
    if matches!(request.action, Operation::Start | Operation::Approve)
        && let Some(reason) = compatibility(&config).1
    {
        return Err(coded(ErrorCode::Refused, reason));
    }
    if request.action == Operation::Start && (!config.improve.enabled || !config.training.enabled) {
        return Err(coded(
            ErrorCode::Refused,
            "Enable improvement and training in Learning settings first.",
        ));
    }
    let checked_output = if matches!(
        request.action,
        Operation::Fuse
            | Operation::ExportPeft
            | Operation::ExportGgufLora
            | Operation::ExportGguf
            | Operation::ExportExl3
    ) {
        Some(require_output(request.output.as_deref())?)
    } else {
        None
    };
    let state = &config.state_dir;
    let needs_existing_store = matches!(
        request.action,
        Operation::Resume
            | Operation::Stop
            | Operation::Fuse
            | Operation::ExportPeft
            | Operation::ExportGgufLora
            | Operation::ExportGguf
    );
    if needs_existing_store && !state.join("state.sqlite3").try_exists()? {
        return Err(coded(
            ErrorCode::NotFound,
            "No learning state exists for this model",
        ));
    }
    let store = Store::open(state, &config.model, &config.use_case, &config.harness)?;
    let job = auto::model_job(state, &store)?;
    match request.action {
        Operation::Stop if job["active"] == true => auto::request_stop(
            layout,
            state.file_name().and_then(|s| s.to_str()).unwrap_or(""),
        ),
        Operation::Stop => Err(coded(
            ErrorCode::Conflict,
            "This model no longer has an active job",
        )),
        _ if job["active"] == true => Err(coded(
            ErrorCode::Busy,
            "Wait for this model’s active job to finish",
        )),
        Operation::Resume => {
            auto::resume(state)?;
            store.set_meta("auto_enabled", "true")?;
            Ok(json!({"message":"Scheduling resumed for this model."}))
        }
        Operation::Approve => {
            let r = session::approve(&config, &store, source)?;
            Ok(
                json!({"approved":r.approved,"already":r.already,"judged":r.judged,"unusable":r.unusable}),
            )
        }
        Operation::Start => {
            auto::start_training(layout, &config, &store)
                .map_err(|e| coded(ErrorCode::Refused, e))?
                .ensure_started(state)
                .map_err(|e| coded(ErrorCode::Busy, e))?;
            Ok(json!({"message":"Training and evaluation queued."}))
        }
        Operation::Fuse
        | Operation::ExportPeft
        | Operation::ExportGgufLora
        | Operation::ExportGguf
        | Operation::ExportExl3 => {
            let output = checked_output.expect("checked above");
            let version = match request.version {
                Some(id) => store
                    .version(id)
                    .map_err(|e| coded(ErrorCode::NotFound, e))?,
                None => store.active()?,
            };
            if request.action != Operation::ExportExl3 {
                if version.kind != "lora" || version.adapter.is_none() {
                    return Err(coded(
                        ErrorCode::Refused,
                        "Selected version has no trained LoRA/DoRA adapter to export",
                    ));
                }
                improve::verify_adapter(&store, &version)
                    .map_err(|e| coded(ErrorCode::Refused, e))?;
            } else if version.adapter.is_some() {
                improve::verify_adapter(&store, &version)
                    .map_err(|e| coded(ErrorCode::Refused, e))?;
            }
            match request.action {
                Operation::Fuse => {
                    let adapter = version.adapter.as_deref().expect("checked");
                    let fused = worker::fuse(
                        &config,
                        &config.model,
                        adapter,
                        &output,
                        request.dequantize.unwrap_or(false),
                    )
                    .map_err(|e| coded(ErrorCode::Refused, e))?;
                    Ok(json!({"version": version.id, "fused": fused}))
                }
                Operation::ExportPeft => {
                    let adapter = version.adapter.as_deref().expect("checked");
                    let exported = worker::export_peft(&config, &config.model, adapter, &output)
                        .map_err(|e| coded(ErrorCode::Refused, e))?;
                    Ok(json!({"version": version.id, "exported": exported}))
                }
                Operation::ExportGgufLora => {
                    let adapter = version.adapter.as_deref().expect("checked");
                    let dtype = request.dtype.as_deref().unwrap_or("f32");
                    let exported =
                        worker::export_gguf_lora(&config, &config.model, adapter, &output, dtype)
                            .map_err(|e| coded(ErrorCode::Refused, e))?;
                    Ok(json!({"version": version.id, "exported": exported}))
                }
                Operation::ExportGguf => {
                    let adapter = version.adapter.as_deref().expect("checked");
                    let quant = request.quant.as_deref().unwrap_or("f16");
                    let exported =
                        worker::export_gguf(&config, &config.model, adapter, &output, quant)
                            .map_err(|e| coded(ErrorCode::Refused, e))?;
                    Ok(json!({"version": version.id, "exported": exported}))
                }
                Operation::ExportExl3 => {
                    let bpw = request.bpw.unwrap_or(4.0);
                    let exported = worker::export_exl3(
                        &config,
                        &config.model,
                        version.adapter.as_deref(),
                        &output,
                        bpw,
                    )
                    .map_err(|e| coded(ErrorCode::Refused, e))?;
                    Ok(json!({"version": version.id, "exported": exported}))
                }
                _ => unreachable!(),
            }
        }
        Operation::PrepareGguf => unreachable!(),
    }
}

#[cfg(test)]
mod tests;
