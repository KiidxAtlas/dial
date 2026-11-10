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

const DESCRIPTION: &str = "Inspect and experimentally edit the configured Dial-owned local MLX model's weights. operation: status (stored results, edited copies; no model access); map (layout and parameter counts); analyze (no arguments, 0 experiment runs: probes the model with labelled built-in text (facts, code, maths, story, other languages, instructions, reasoning, repetition) and returns an atlas: per-layer importance overall and per category, a logit lens, each attention head's role such as induction/previous-token/attention-sink/current-token/same-token/local/long-range, the most active MLP neurons with the tokens they promote and the snippets that fire them, expert routing with experts that favour a category, weight-matrix effective rank, and plain-English `findings`; labels are leads, not proof; call it once before choosing what to edit); stats (per-tensor and per-layer weight norms, optional tensor-name prefix); activations (per-layer contrast between 1-64 matched positive/negative conversations); trace (answer-loss increase with each layer skipped, or each attention head of one layer removed, over 1-16 conversations ending in the assistant answer). Weight edits run inside an experiment with a forward-pass budget: experiment (create `experiment` with `budget` model runs, or show it when budget is omitted); generate (greedy answers to 1-32 cases ending in a user turn: tokens, token-limit hits, repetition, optional response checks; 1 run); direction (unit direction per layer between `positive` wanted and `negative` unwanted answers, saved as `name`; with `span` only the last tokens before the answer end or end-of-thinking marker are measured, and without `negative` the span is contrasted with the rest of each positive answer; 1 run); edit (new model copy `name`, 0 runs; method project_out, the default: direction `direction` projected out of the top `columns` columns of every MLP output projection of `layers` (dense, shared expert and each routed expert); method add: the direction written into those same writers at `alpha` (default 0.1) times their mean output size, as a steering vector compiled into the weights; method scale: you choose the weights yourself, `changes` multiplies exact tensors (names from map/stats) by a factor, optionally only on chosen `rows`/`columns`, e.g. scale 0.5 on one down_proj or on rows of an attention o_proj; quantized models report requantization error, which can exceed a small edit); attribute (1 run; ranks attention heads and MLP neurons by estimated loss rise if removed, over 1-16 `cases` ending in the unwanted answer; with `wanted` answers to the same prompts it ranks by the difference, so components that drive the unwanted answer without being needed for the wanted one come first; an estimate to shortlist candidates); try (1 run; scores an edit in memory at each of `doses` (1 = as specified) and discards it: loss change on `cases` and drift from the intact model on `neutral` answers, so you can test many candidates and strengths before paying for a copy; takes the same edit fields as edit and also accepts allow_noisy). A scale change may name a `layer` with `heads` (that layer's attention o_proj columns) or `neurons` (its down_proj columns) instead of a tensor, using indices from attribute. Primer: down_proj column n is what MLP neuron n writes to the residual stream and o_proj columns h*size..(h+1)*size are what attention head h writes; scale 0 silences them, 1 leaves them, and a negative scale flips their contribution. When the user states a goal in plain words, carry it out yourself: turn it into prompts that show the failure, generate to reproduce it (stop if it does not reproduce), attribute (focus repeats for loops; give wanted and neutral answers; read agreement, promotes/suppresses and induction to say why a component is chosen) to shortlist components, try them at several doses with neutral answers, keep only a candidate that lowers loss on cases while neutral drift stays near 0, then edit and evaluate. Edits on 4-bit models are refused when requantization error exceeds half the edit (use an 8-bit or full-precision copy, or allow_noisy). status lists earlier edits with their parameters, so do not repeat one that failed. evaluate (base vs edit `edit` on at least 8 held-out cases the experiment never saw; passes when the edit wins more cases than it loses with sign-test p <= 0.1 and, for project_out and add edits, also beats a norm-matched edit along a random orthogonal direction built and removed during the evaluation; reports answer and thinking token changes with paired bootstrap 95% intervals). Never modifies the base model or the project's model choice; the user promotes a passing edit. Results are numbers only; GGUF and hosted models are refused.";

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
                "operation": {"type":"string", "enum":["status", "map", "analyze", "stats", "activations", "trace", "experiment", "generate", "direction", "edit", "try", "attribute", "evaluate"]},
                "experiment": {"type":"string", "description":"generate, direction, edit and experiment: experiment name (letters, digits, - or _); its budget is charged before the run."},
                "budget": {"type":"integer", "minimum":1, "maximum":256, "description":"experiment only: create the experiment with this many model runs."},
                "name": {"type":"string", "description":"direction and edit: name of the new direction or edited copy."},
                "direction": {"type":"string", "description":"edit and try, method project_out or add: a direction measured in the same experiment."},
                "method": {"type":"string", "enum":["project_out", "scale", "add"], "description":"edit and try: project_out (default; needs direction and layers), add (writes the direction into every MLP writer of `layers` at `alpha` times its mean output size: steering compiled into the weights; needs a direction measured at site mlp by this version) or scale (needs changes)."},
                "changes": {"type":"array", "minItems":1, "maxItems":32, "items":{"type":"object",
                    "properties":{"tensor":{"type":"string", "description":"exact tensor name, e.g. model.layers.14.mlp.down_proj.weight"},
                        "layer":{"type":"integer", "minimum":0, "description":"instead of tensor: the layer whose heads or neurons are scaled"},
                        "heads":{"type":"array", "items":{"type":"integer", "minimum":0}, "description":"with layer: attention heads to scale (their o_proj columns)"},
                        "neurons":{"type":"array", "items":{"type":"integer", "minimum":0}, "description":"with layer: MLP neurons to scale (their down_proj columns)"},
                        "scale":{"type":"number", "minimum":-4, "maximum":4, "description":"multiplier for the selected elements (1 = unchanged, 0 = zeroed)"},
                        "rows":{"type":"array", "items":{"type":"integer", "minimum":0}, "description":"first-axis indices to scale (default: all)"},
                        "columns":{"type":"array", "items":{"type":"integer", "minimum":0}, "description":"last-axis indices to scale (default: all)"}},
                    "required":["scale"]},
                    "description":"edit, method scale: exact tensors and factors; every other element is left as stored."},
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
                    "description":"direction only: conversations ending in an unwanted assistant answer (for example a reasoning loop). May be omitted with `span`: the span is then contrasted with the rest of each positive answer."},
                "site": {"type":"string", "enum":["mlp", "residual"], "description":"direction only: measure MLP outputs (default; what edits project out) or the residual stream."},
                "span": {"type":"object", "properties":{
                        "last":{"type":"integer", "minimum":1, "maximum":1024, "description":"tokens measured"},
                        "before":{"type":"string", "enum":["answer_end", "think_end"], "description":"anchor: the end of the answer (default) or the end-of-thinking marker"}},
                    "required":["last"],
                    "description":"direction only: measure only the last `last` answer tokens before the anchor instead of every answer token, e.g. {last: 64, before: think_end} for the stretch where a model decides to stop thinking."},
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
                "columns": {"type":"integer", "minimum":1, "maximum":4096, "description":"edit and try, method project_out: output-projection columns edited per writer and expert (default 64)."},
                "alpha": {"type":"number", "exclusiveMinimum":0, "maximum":2, "description":"edit and try: project_out, the fraction of the direction removed (default 1); add, the strength relative to each writer's mean output size (default 0.1)."},
                "neutral": {"type":"array", "minItems":1, "maxItems":8, "items":{"type":"object",
                    "properties":{"messages":messages}, "required":["messages"]},
                    "description":"try and attribute: ordinary conversations ending in an assistant answer the edit must not disturb; try reports drift (KL) from the intact model, attribute subtracts the components' support for them so generally important components rank low."},
                "focus": {"type":"string", "enum":["answer", "repeats"], "description":"attribute only: score every answer token (default) or only tokens that repeat an earlier 3-token sequence, so a loop is not diluted by ordinary text."},
                "explain": {"type":"boolean", "description":"attribute only: add the tokens each top neuron promotes/suppresses and each top head's induction (copy) attention (default true)."},
                "wanted": {"type":"array", "minItems":1, "maxItems":16, "items":{"type":"object",
                    "properties":{"messages":messages}, "required":["messages"]},
                    "description":"attribute only: conversations ending in the wanted answer to the same kind of prompt; components are then ranked by support for `cases` minus support for these."},
                "top": {"type":"integer", "minimum":1, "maximum":100, "description":"attribute only: components listed per kind (default 20)."},
                "doses": {"type":"array", "minItems":1, "maxItems":5, "items":{"type":"number", "exclusiveMinimum":0, "maximum":2},
                    "description":"try only: edit strengths to compare (1 = the edit as specified; scale changes move that fraction of the way from 1 to their factor)."},
                "allow_noisy": {"type":"boolean", "description":"edit and try: accept a quantized edit whose requantization error exceeds half the edit itself (refused by default)."}
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
