//! Opt-in, approved scaling of explicitly selected checkpoint tensors. Works offline on one
//! safetensors file (including a shard); never rewrites a source or reloads an inference server.
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use anyhow::{Context, Result, bail, ensure};
use safetensors::{Dtype, SafeTensors};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::config::types::Config;
use crate::tools::integrations::ext::{Tool, ToolContext, ToolEvent};
use crate::tools::{ToolOutput, tool_schema};

const TOOL: &str = "image_weights";
// Editing holds one source snapshot in RAM; output is streamed to a staged file.
const MAX_BYTES: u64 = 8 * 1024 * 1024 * 1024;
const MAX_SCALES: usize = 128;
const MAX_PAGE: usize = 128;

pub fn tools(_config: &Config) -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(ImageWeights)]
}
struct ImageWeights;
impl Tool for ImageWeights {
    fn name(&self) -> &str {
        TOOL
    }
    fn schema(&self) -> Value {
        tool_schema(
            TOOL,
            "Inspect or scale actual local image-model layer/tensor weights in a safetensors file, not LoRA strengths. Requires image.allow_weight_edits and approval. Inspect first, then supply the returned SHA-256 and exact tensor names for scaling. Supports F16/BF16/F32/F64; packed quantized tensors are refused, never dequantized implicitly. Writes a separate new file, preserving all other bytes and metadata. Does not copy the whole model, change image routing or reload MLX-Serve: install the exported file into a separate compatible model copy and load it explicitly. Source/output must be in authorized roots; add the model directory first. Each file is limited to 8 GiB and must fit in RAM.",
            json!({
                "action": {"type":"string", "enum":["inspect", "scale"]},
                "source": {"type":"string", "description":"Authorized .safetensors checkpoint file or shard. Never modified."},
                "output": {"type":"string", "description":"New .safetensors file for scale. Must not exist; parent directory must exist."},
                "sha256": {"type":"string", "description":"Required for scale: source SHA-256 from inspect, binding edits to the inspected bytes."},
                "scales": {"type":"object", "additionalProperties":{"type":"number"}, "description":"For scale: 1–128 exact tensor names mapped to finite multipliers in [-8, 8]. No wildcard or prefix edits."},
                "prefix": {"type":"string", "description":"Inspect only: filter tensor names by prefix."},
                "offset": {"type":"integer", "minimum":0, "description":"Inspect only: start in the sorted, filtered tensor list."},
                "limit": {"type":"integer", "minimum":1, "maximum":128, "description":"Inspect only: page size, default 128."}
            }),
            &["action", "source"],
        )
    }
    fn needs_approval(&self, _args: &Value) -> bool {
        true
    }
    fn may_modify_workspace(&self, args: &Value) -> bool {
        args["action"] != "inspect"
    }
    fn call(&self, ctx: &ToolContext, args: &Value) -> ToolOutput {
        self.call_streaming(ctx, args, &|_| {}, &AtomicBool::new(false))
    }
    fn call_streaming(
        &self,
        ctx: &ToolContext,
        args: &Value,
        _on_event: &(dyn Fn(ToolEvent) + Sync),
        cancel: &AtomicBool,
    ) -> ToolOutput {
        match execute(ctx, args, cancel) {
            Ok(out) => out,
            Err(error) => ToolOutput::err(format!("{error:#}")),
        }
    }
}

fn check_cancel(cancel: &AtomicBool) -> Result<()> {
    ensure!(
        !cancel.load(Ordering::SeqCst),
        "image weight operation interrupted"
    );
    Ok(())
}
fn text<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args[key]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .with_context(|| format!("image_weights requires {key}"))
}
fn checkpoint_path(ctx: &ToolContext, input: &str) -> Result<std::path::PathBuf> {
    let path = ctx.workspace.resolve(input).map_err(anyhow::Error::msg)?;
    ensure!(
        path.extension().is_some_and(|ext| ext == "safetensors"),
        "checkpoint path must end in .safetensors"
    );
    Ok(path)
}
fn scale_args(args: &Value) -> Result<BTreeMap<String, f64>> {
    let raw = args["scales"]
        .as_object()
        .context("scale requires an exact tensor-name to multiplier map")?;
    ensure!(
        !raw.is_empty() && raw.len() <= MAX_SCALES,
        "scales must select 1..={MAX_SCALES} tensors"
    );
    raw.iter()
        .map(|(name, value)| {
            ensure!(!name.is_empty(), "tensor name cannot be empty");
            let factor = value
                .as_f64()
                .context("tensor multiplier must be a number")?;
            ensure!(
                factor.is_finite() && factor.abs() <= 8.0,
                "tensor multiplier for {name:?} must be finite and in [-8, 8]"
            );
            Ok((name.clone(), factor))
        })
        .collect()
}
fn read_snapshot(
    ctx: &ToolContext,
    path: &std::path::Path,
    cancel: &AtomicBool,
) -> Result<Vec<u8>> {
    let mut file = crate::tools::edit::edit_transaction::open_read(ctx.workspace, path)
        .map_err(anyhow::Error::msg)?;
    let length = file.metadata()?.len();
    ensure!(
        length <= MAX_BYTES,
        "checkpoint file exceeds 8 GiB; use smaller model shards"
    );
    let size = usize::try_from(length).context("checkpoint is too large for this platform")?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(size)
        .context("not enough RAM for checkpoint snapshot")?;
    let mut chunk = [0u8; 64 * 1024];
    loop {
        check_cancel(cancel)?;
        let count = file.read(&mut chunk)?;
        if count == 0 {
            break;
        }
        ensure!(
            count <= size.saturating_sub(bytes.len()),
            "checkpoint grew while reading; inspect it again"
        );
        bytes.extend_from_slice(&chunk[..count]);
    }
    ensure!(
        bytes.len() == size,
        "checkpoint changed length while reading; inspect it again"
    );
    Ok(bytes)
}
fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn execute(ctx: &ToolContext, args: &Value, cancel: &AtomicBool) -> Result<ToolOutput> {
    ensure!(
        ctx.config.image.allow_weight_edits,
        "image weight editing is disabled; enable image.allow_weight_edits in Settings → Image generation first"
    );
    check_cancel(cancel)?;
    let action = text(args, "action")?;
    ensure!(
        matches!(action, "inspect" | "scale"),
        "action must be inspect or scale"
    );
    let source = checkpoint_path(ctx, text(args, "source")?)?;
    let scales = if action == "scale" {
        Some(scale_args(args)?)
    } else {
        None
    };
    let output = if action == "scale" {
        ensure!(
            args.get("prefix").is_none()
                && args.get("offset").is_none()
                && args.get("limit").is_none(),
            "prefix, offset and limit apply only to inspect"
        );
        let hash = text(args, "sha256")?;
        ensure!(
            hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()),
            "sha256 must be the source hash from inspect"
        );
        let output = checkpoint_path(ctx, text(args, "output")?)?;
        ensure!(
            output != source,
            "output must be separate from the source checkpoint"
        );
        ensure!(
            !output.try_exists()?,
            "output already exists; image_weights never overwrites files"
        );
        ctx.authorize_mutation(TOOL, std::slice::from_ref(&output), false)
            .map_err(anyhow::Error::msg)?;
        Some(output)
    } else {
        ensure!(
            args.get("output").is_none()
                && args.get("scales").is_none()
                && args.get("sha256").is_none(),
            "output, scales and sha256 apply only to scale"
        );
        None
    };
    // Reserve destination before allocating a model-sized snapshot. RAII cleans every failure.
    let mut staged = output
        .as_ref()
        .map(|path| {
            crate::tools::edit::edit_transaction::NewBinary::create(ctx.workspace, path)
                .map_err(anyhow::Error::msg)
                .context("preparing weight destination")
        })
        .transpose()?;
    let mut bytes = read_snapshot(ctx, &source, cancel)?;
    let source_hash = digest(&bytes);
    let tensors = SafeTensors::deserialize(&bytes).context("invalid safetensors checkpoint")?;
    if action == "inspect" {
        let prefix = args
            .get("prefix")
            .map(|v| v.as_str().context("prefix must be a string"))
            .transpose()?
            .unwrap_or("");
        let integer = |name: &str, default: usize| -> Result<usize> {
            args.get(name)
                .map(|v| {
                    v.as_u64()
                        .and_then(|n| usize::try_from(n).ok())
                        .with_context(|| format!("{name} must be a nonnegative integer"))
                })
                .unwrap_or(Ok(default))
        };
        let offset = integer("offset", 0)?;
        let limit = integer("limit", MAX_PAGE)?;
        ensure!(
            (1..=MAX_PAGE).contains(&limit),
            "limit must be 1..={MAX_PAGE}"
        );
        let mut names: Vec<_> = tensors
            .names()
            .into_iter()
            .filter(|name| name.starts_with(prefix))
            .collect();
        names.sort();
        let page = names.iter().skip(offset).take(limit).map(|name| {
            let tensor = tensors.tensor(name).context("reading tensor metadata")?;
            Ok(json!({"name":name, "dtype":format!("{:?}", tensor.dtype()), "shape":tensor.shape()}))
        }).collect::<Result<Vec<_>>>()?;
        let next = offset.saturating_add(page.len());
        return Ok(ToolOutput::ok(
            json!({"source":text(args,"source")?, "sha256":source_hash,
            "file_bytes":bytes.len(), "tensors":page, "matching_tensors":names.len(),
            "next_offset":if next < names.len() {Some(next)} else {None}})
            .to_string(),
        ));
    }
    ensure!(
        source_hash.eq_ignore_ascii_case(text(args, "sha256")?),
        "source SHA-256 changed; inspect it again before scaling"
    );
    let scales = scales.context("scale requires tensor selections")?;
    let mut selected = Vec::with_capacity(scales.len());
    for (name, factor) in &scales {
        let tensor = tensors
            .tensor(name)
            .with_context(|| format!("unknown tensor {name:?}"))?;
        ensure!(
            matches!(
                tensor.dtype(),
                Dtype::F16 | Dtype::BF16 | Dtype::F32 | Dtype::F64
            ),
            "tensor {name:?} has unsupported dtype {:?}; packed quantized/integer weights cannot be scaled directly",
            tensor.dtype()
        );
        // TensorView borrows this validated snapshot; no guessed header offsets or custom parser.
        let start = (tensor.data().as_ptr() as usize)
            .checked_sub(bytes.as_ptr() as usize)
            .context("invalid tensor data range")?;
        let end = start
            .checked_add(tensor.data().len())
            .context("tensor data range overflow")?;
        ensure!(end <= bytes.len(), "tensor data range exceeds snapshot");
        selected.push((name.clone(), start..end, tensor.dtype(), *factor));
    }
    drop(tensors);
    for (name, range, dtype, factor) in selected {
        scale_values(&mut bytes[range], dtype, factor, cancel)
            .with_context(|| format!("scaling tensor {name:?}"))?;
    }
    let output = output.context("scale requires output")?;
    let mut staged = staged
        .take()
        .context("weight destination was not prepared")?;
    for chunk in bytes.chunks(1024 * 1024) {
        check_cancel(cancel)?;
        staged
            .file_mut()
            .write_all(chunk)
            .context("writing scaled checkpoint")?;
    }
    let output_hash = digest(&bytes);
    check_cancel(cancel)?;
    staged
        .publish(ctx.workspace, cancel)
        .map_err(anyhow::Error::msg)?;
    Ok(ToolOutput::ok(json!({"output":text(args,"output")?, "source_sha256":source_hash,
        "sha256":output_hash, "scaled_tensors":scales, "file_bytes":bytes.len(),
        "activation":"Not loaded. Install this file into a separate compatible model copy and load that copy in MLX-Serve."}).to_string()).with_changes([output]))
}

fn scale_values(bytes: &mut [u8], dtype: Dtype, factor: f64, cancel: &AtomicBool) -> Result<()> {
    let width = match dtype {
        Dtype::F16 | Dtype::BF16 => 2,
        Dtype::F32 => 4,
        Dtype::F64 => 8,
        _ => bail!("unsupported tensor dtype"),
    };
    ensure!(
        bytes.len().is_multiple_of(width),
        "invalid tensor byte length"
    );
    for (i, chunk) in bytes.chunks_exact_mut(width).enumerate() {
        if i % 8192 == 0 {
            check_cancel(cancel)?;
        }
        let value = match dtype {
            Dtype::F16 => {
                f64::from(half::f16::from_bits(u16::from_le_bytes([chunk[0], chunk[1]])).to_f32())
            }
            Dtype::BF16 => {
                f64::from(half::bf16::from_bits(u16::from_le_bytes([chunk[0], chunk[1]])).to_f32())
            }
            Dtype::F32 => f64::from(f32::from_le_bytes(chunk.try_into()?)),
            Dtype::F64 => f64::from_le_bytes(chunk.try_into()?),
            _ => bail!("unsupported tensor dtype"),
        };
        let scaled = value * factor;
        ensure!(
            value.is_finite() && scaled.is_finite(),
            "nonfinite tensor value or scaling overflow"
        );
        match dtype {
            Dtype::F16 => {
                let v = half::f16::from_f64(scaled);
                ensure!(v.is_finite(), "scaling overflow for F16");
                chunk.copy_from_slice(&v.to_bits().to_le_bytes());
            }
            Dtype::BF16 => {
                let v = half::bf16::from_f64(scaled);
                ensure!(v.is_finite(), "scaling overflow for BF16");
                chunk.copy_from_slice(&v.to_bits().to_le_bytes());
            }
            Dtype::F32 => {
                let v = scaled as f32;
                ensure!(v.is_finite(), "scaling overflow for F32");
                chunk.copy_from_slice(&v.to_le_bytes());
            }
            Dtype::F64 => chunk.copy_from_slice(&scaled.to_le_bytes()),
            _ => bail!("unsupported tensor dtype"),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
