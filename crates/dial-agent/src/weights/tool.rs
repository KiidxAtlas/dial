//! The `model_weights` agent tool: the same inspections, experiments, edits and evaluations as
//! `dial weights`. Off unless `model_weights` is listed in `integrations`; every call except
//! `status` needs approval. Promotion and revert stay with the user (`dial weights promote`).
use super::{Operation, edits, render, render_status, status};
use crate::config::types::Config;
use crate::runtime::schema::tool_schema;
use crate::runtime::tool_output::ToolOutput;
use crate::tools::integrations::ext::{Tool, ToolContext, ToolEvent};
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::sync::{Arc, atomic::AtomicBool};

pub(super) const TOOL: &str = "model_weights";
/// Per-tensor rows returned to the model. The stored result and the dashboard keep more.
pub(super) const TOOL_TENSOR_ROWS: usize = 64;

const DESCRIPTION: &str = "Inspect and experimentally edit the configured Dial-owned local MLX model's weights. operation: status (stored results, edited copies; no model access); map (layout and parameter counts); stats (per-tensor and per-layer weight norms, optional tensor-name prefix); activations (per-layer contrast between 1-64 matched positive/negative conversations); trace (answer-loss increase with each layer skipped, or each attention head of one layer removed, over 1-16 conversations ending in the assistant answer). Weight edits run inside an experiment with a forward-pass budget: experiment (create `experiment` with `budget` model runs, or show it when budget is omitted); generate (greedy answers to 1-32 cases ending in a user turn: tokens, token-limit hits, repetition, optional response checks; 1 run); direction (unit direction per layer between `positive` wanted and `negative` unwanted answers, saved as `name`; 1 run); edit (new model copy `name` with direction `direction` projected out of the top `columns` MLP down_proj columns of `layers`; quantized models report requantization error; 0 runs); evaluate (base vs edit `edit` on at least 8 held-out cases the experiment never saw; passes when the edit wins more cases than it loses with sign-test p <= 0.1). Never modifies the base model or the project's model choice; the user promotes a passing edit. Results are numbers only; GGUF and hosted models are refused.";

pub fn tools(_config: &Config) -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(ModelWeights)]
}

struct ModelWeights;

impl Tool for ModelWeights {
    fn name(&self) -> &str {
        TOOL
    }

    fn schema(&self) -> Value {
        let messages = json!({"type":"array", "items":{"type":"object",
            "properties":{"role":{"type":"string"}, "content":{"type":"string"}},
            "required":["role", "content"]}});
        tool_schema(
            TOOL,
            DESCRIPTION,
            json!({
                "operation": {"type":"string", "enum":["status", "map", "stats", "activations", "trace", "experiment", "generate", "direction", "edit", "evaluate"]},
                "experiment": {"type":"string", "description":"generate, direction, edit and experiment: experiment name (letters, digits, - or _); its budget is charged before the run."},
                "budget": {"type":"integer", "minimum":1, "maximum":256, "description":"experiment only: create the experiment with this many model runs."},
                "name": {"type":"string", "description":"direction and edit: name of the new direction or edited copy."},
                "direction": {"type":"string", "description":"edit only: a direction measured in the same experiment."},
                "edit": {"type":"string", "description":"evaluate only: the edited copy to evaluate."},
                "prefix": {"type":"string", "description":"stats only: measure tensors whose name starts with this prefix (default: every tensor)."},
                "pairs": {"type":"array", "minItems":1, "maxItems":64, "items":{"type":"object",
                    "properties":{"positive":messages, "negative":messages}, "required":["positive", "negative"]},
                    "description":"activations only: matched conversations with the same prompt; positive ends in the wanted assistant answer, negative in the unwanted one."},
                "positive": {"type":"array", "minItems":1, "maxItems":64, "items":{"type":"object",
                    "properties":{"messages":messages}, "required":["messages"]},
                    "description":"direction only: conversations ending in a wanted assistant answer."},
                "negative": {"type":"array", "minItems":1, "maxItems":64, "items":{"type":"object",
                    "properties":{"messages":messages}, "required":["messages"]},
                    "description":"direction only: conversations ending in an unwanted assistant answer (for example a reasoning loop)."},
                "site": {"type":"string", "enum":["mlp", "residual"], "description":"direction only: measure MLP outputs (default; what edits project out) or the residual stream."},
                "cases": {"type":"array", "minItems":1, "maxItems":32, "items":{"type":"object",
                    "properties":{"messages":messages,
                        "required":{"type":"array", "items":{"type":"string"}, "description":"substrings the answer must contain"},
                        "forbidden":{"type":"array", "items":{"type":"string"}, "description":"substrings the answer must not contain"}},
                    "required":["messages"]},
                    "description":"trace: 1-16 conversations ending in the assistant answer to score. generate and evaluate: conversations ending in the user turn to answer, with optional response checks."},
                "max_tokens": {"type":"integer", "minimum":1, "maximum":4096, "description":"generate and evaluate: token limit per answer (default 512); hitting it counts against the case."},
                "tail": {"type":"integer", "minimum":0, "maximum":400, "description":"generate only: return the last N characters of each answer (default 0: none)."},
                "mode": {"type":"string", "enum":["layers", "heads"], "description":"trace only: skip each layer (default) or remove each attention head of `layer`."},
                "layer": {"type":"integer", "minimum":0, "description":"trace heads mode: zero-based layer whose heads are removed one at a time."},
                "layers": {"type":"array", "items":{"type":"integer", "minimum":0}, "description":"trace layers mode: zero-based layers to skip (default: all). edit: 1-16 distinct layers to edit."},
                "columns": {"type":"integer", "minimum":1, "maximum":4096, "description":"edit only: down_proj output columns edited per layer (default 64)."},
                "alpha": {"type":"number", "exclusiveMinimum":0, "maximum":2, "description":"edit only: fraction of the direction removed (default 1)."}
            }),
            &["operation"],
        )
    }

    fn needs_approval(&self, args: &Value) -> bool {
        args["operation"] != "status"
    }

    fn may_modify_workspace(&self, _args: &Value) -> bool {
        false
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
        match execute(ctx.config, args, cancel) {
            Ok(text) => ToolOutput::ok(text),
            Err(error) => ToolOutput::err(format!("{error:#}")),
        }
    }
}

pub(super) fn execute(config: &Config, args: &Value, cancel: &AtomicBool) -> Result<String> {
    let name = args["operation"]
        .as_str()
        .context("operation is required")?;
    if name == "status" {
        return Ok(render_status(&status(config)));
    }
    // An interactive session already holds foreground compute, so no layout is passed.
    let value = match name {
        "experiment" => {
            let experiment = args["experiment"]
                .as_str()
                .context("experiment needs an `experiment` name")?;
            match args.get("budget").filter(|budget| !budget.is_null()) {
                Some(budget) => edits::create_experiment(
                    config,
                    experiment,
                    budget
                        .as_u64()
                        .context("budget must be a positive integer")?,
                )?,
                None => edits::experiment(config, experiment)?,
            }
        }
        "evaluate" => edits::evaluate(config, None, args, cancel)?,
        _ => super::run(config, None, Operation::parse(name)?, args, None, cancel)?,
    };
    Ok(json!({
        "summary": render(&value),
        "result": compact(value),
        "dashboard": "The user can open /weights open to see this result as charts; only the user can promote a passing edit (/weights promote NAME).",
    })
    .to_string())
}

/// Caps the per-tensor rows for the model's context; per-layer aggregates are kept whole.
pub(super) fn compact(mut value: Value) -> Value {
    let stored = value["tensors_total"].as_u64();
    if let Some(rows) = value["tensors"].as_array_mut()
        && rows.len() > TOOL_TENSOR_ROWS
    {
        let total = stored.map_or_else(|| json!(rows.len()), |n| json!(n));
        rows.truncate(TOOL_TENSOR_ROWS);
        value["tensors_total"] = total;
        value["tensors_truncated"] = json!(true);
    }
    value
}
