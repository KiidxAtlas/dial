"""MLX weight and activation inspection and budgeted weight edits for Dial's weights surface.

Read-only operations: `weights_map` (checkpoint headers only), `weights_stats` (per-tensor
moments), `weights_activations` (per-layer contrast of matched response pairs),
`weights_trace` (layer or attention-head ablation against scored answers), `weights_generate`
(greedy answers with length/loop metrics) and `weights_direction` (per-layer mean difference of
answer activations, saved for an edit). `weights_edit` never touches the base model: it writes a
new model directory whose unedited files are hard links to the base. Results are bounded JSON
and never echo prompt text.
`weights_combine` sums the changes of 2-4 edited copies into one copy, and `weights_edit` method `steering`
folds a steering bundle's rank-1 delta into a copy (no adapter is left behind).
"""
import contextlib
import json
import math
import os
import re
import shutil
import struct
import tempfile
from pathlib import Path

import dial_steering as steering

VERSION = 1
MAX_CONFIG = 4 * 1024 * 1024
MAX_HEADER = 100 * 1024 * 1024
MAX_SHARDS = 256
MAX_TENSORS = 16384
MAX_PAIRS = 64
MAX_CASES = 16
MAX_HEADS = 256
MAX_TOKENS = 2048
MAX_GENERATE_CASES = 32
MAX_NEW_TOKENS = 4096
MAX_TAIL = 400
MAX_EDIT_LAYERS = 16
MAX_EDIT_COLUMNS = 4096
MAX_EDIT_CHANGES = 32
MAX_EDIT_INDICES = 4096
MAX_EDIT_SCALE = 4.0
MAX_DOSES = 5
MAX_NEUTRAL_CASES = 8
NEUTRAL_TOKENS = 64
MAX_SPAN = 1024
# Default `add` strength: the writer's mean output size times this, written along the direction.
ADD_ALPHA = 0.1
MAX_ATTRIBUTION_ROWS = 100
# Requantization error relative to the edit itself. A rounded-away edit has a ratio of 1, so above
# half the edit the stored weights are already too far from the intended ones to trust.
MAX_REQUANTIZATION = 0.5
TOP_LOGITS = 64
# Ridge for steering edits, relative to the trace of the projection inputs' Gram matrix (as dial_steering.bake).
STEERING_RIDGE = 1e-3
# Dense MLP output projections: their columns are the vectors each hidden unit writes to the residual.
OUTPUT_PROJECTIONS = ("down_proj", "w2", "fc2", "dense_4h_to_h")
DIRECTION_FORMAT = "dial-weights-direction"
EDIT_RECORD = "weights-edit.json"
# Elements dequantized and reduced per step: bounds the float32 working set to 64 MiB.
CHUNK_ELEMENTS = 1 << 24
LAYER = re.compile(r"(?:^|\.)(?:layers|h|blocks)\.(\d+)\.")
UNSUPPORTED_FORMAT = ("weights inspection needs a local MLX safetensors model directory with config.json; "
                      "GGUF files and hosted endpoints are not supported")


class WeightsError(ValueError):
    """Operator-facing contract failure; messages name fields and indices, never content."""


def require(condition, message):
    if not condition:
        raise WeightsError(message)


def report(request, phase, completed, total):
    if request.get("progress"):
        steering.atomic_json(request["progress"], {"phase": phase, "completed": completed, "total": total})


def rounded(value):
    """Six significant digits keep results compact; nonfinite numbers become null."""
    if isinstance(value, float):
        return float(f"{value:.6g}") if math.isfinite(value) else None
    if isinstance(value, dict):
        return {key: rounded(item) for key, item in value.items()}
    if isinstance(value, list):
        return [rounded(item) for item in value]
    return value


# ---------------------------------------------------------------------------
# Checkpoint layout (no model load)
# ---------------------------------------------------------------------------

def read_config(root):
    with (root / "config.json").open("rb") as source:
        raw = source.read(MAX_CONFIG + 1)
    require(len(raw) <= MAX_CONFIG, "config.json exceeds 4 MiB")
    config = json.loads(raw)
    require(isinstance(config, dict), "config.json must be an object")
    return config


def shards(root):
    files = sorted(path for path in root.glob("*.safetensors") if path.is_file())
    require(0 < len(files) <= MAX_SHARDS, f"model must have 1–{MAX_SHARDS} safetensors files")
    return files


def header(path):
    with path.open("rb") as source:
        prefix = source.read(8)
        require(len(prefix) == 8, f"{path.name} is not a safetensors file")
        size = struct.unpack("<Q", prefix)[0]
        require(2 <= size <= MAX_HEADER, f"{path.name} has an invalid safetensors header size")
        raw = source.read(size)
    require(len(raw) == size, f"{path.name} has a truncated safetensors header")
    try:
        data = json.loads(raw)
    except (ValueError, UnicodeError):
        raise WeightsError(f"{path.name} has an invalid safetensors header") from None
    require(isinstance(data, dict), f"{path.name} has an invalid safetensors header")
    return {name: info for name, info in data.items() if name != "__metadata__"}


def architecture(config):
    text = config.get("text_config") if isinstance(config.get("text_config"), dict) else {}

    def field(key):
        for source in (text, config):
            value = source.get(key)
            if type(value) in (int, str):
                return value
        return None
    fields = (("layers", "num_hidden_layers"), ("hidden_size", "hidden_size"),
              ("heads", "num_attention_heads"), ("kv_heads", "num_key_value_heads"),
              ("intermediate_size", "intermediate_size"), ("vocab_size", "vocab_size"),
              ("experts", "num_experts"), ("full_attention_interval", "full_attention_interval"))
    result = {name: field(key) for name, key in fields}
    result["model_type"] = config.get("model_type") if isinstance(config.get("model_type"), str) else field("model_type")
    result["vision"] = isinstance(config.get("vision_config"), dict)
    return result


def quantization(config, module):
    """MLX quantization of `module`: global settings with MLX-LM's per-module overrides."""
    settings = config.get("quantization") or config.get("quantization_config")
    if not isinstance(settings, dict):
        return None
    override = settings.get(module)
    if override is False:
        return None
    if isinstance(override, dict):
        settings = dict(settings, **override)
    group, bits = settings.get("group_size"), settings.get("bits")
    if type(group) is not int or type(bits) is not int or group <= 0 or not 1 <= bits <= 8:
        return None
    mode = settings.get("mode", "affine")
    return {"group_size": group, "bits": bits, "mode": mode if isinstance(mode, str) else "affine"}


def component(name):
    lower = name.lower()
    if any(part in lower for part in ("vision", "visual", "multi_modal_projector", "mm_projector")):
        return "vision"
    if "lm_head" in lower:
        return "head"
    if "embed" in lower or lower.startswith("wte.") or ".wte." in lower:
        return "embedding"
    if "norm" in lower or ".ln_" in lower:
        return "norm"
    if "linear_attn" in lower or ".mixer." in lower:
        return "linear_attention"
    if "attn" in lower or "attention" in lower:
        return "attention"
    if any(part in lower for part in ("expert", "switch_mlp", "router")) or lower.endswith(".mlp.gate.weight"):
        return "moe"
    if any(part in lower for part in ("mlp", "feed_forward", "ffn")):
        return "mlp"
    return "other"


def tensors(root, config):
    """Every tensor in the checkpoint headers. Quantization scales/biases fold into their weight."""
    entries = {}
    for path in shards(root):
        for name, info in header(path).items():
            require(isinstance(info, dict) and isinstance(info.get("dtype"), str)
                    and isinstance(info.get("shape"), list)
                    and all(type(n) is int and n >= 0 for n in info["shape"]),
                    f"{path.name} has an invalid tensor entry")
            require(name not in entries, "tensor names repeat across shards")
            offsets = info.get("data_offsets")
            size = offsets[1] - offsets[0] if (isinstance(offsets, list) and len(offsets) == 2
                                               and all(type(n) is int for n in offsets)) else 0
            entries[name] = (path.name, info["shape"], info["dtype"], size)
            require(len(entries) <= MAX_TENSORS, f"checkpoint exceeds {MAX_TENSORS} tensors")
    rows = []
    for name in sorted(entries):
        module, _, leaf = name.rpartition(".")
        if leaf in ("scales", "biases") and f"{module}.weight" in entries:
            continue
        shard, shape, dtype, size = entries[name]
        packed = leaf == "weight" and f"{module}.scales" in entries
        logical = list(shape)
        settings = quantization(config, module) if packed else None
        if packed:
            size += sum(entries[f"{module}.{part}"][3] for part in ("scales", "biases")
                        if f"{module}.{part}" in entries)
            if settings and logical:
                logical[-1] = logical[-1] * 32 // settings["bits"]
        kind = component(name)
        match = LAYER.search(name)
        rows.append({
            "name": name, "shard": shard, "shape": logical, "dtype": dtype, "component": kind,
            "layer": int(match.group(1)) if match and kind != "vision" else None,
            "params": math.prod(logical), "bytes": size, "quantized": packed, "quantization": settings,
        })
    return rows


# These are structural interpretations, not measured semantic specializations.
ROLES = {
    "embedding": ("Token IDs to feature vectors; may also supply tied output weights.",
                  "A row edit changes a token representation and possibly its output score."),
    "head": ("Features to vocabulary scores.",
             "A row edit changes a token score conditional on the final hidden state."),
    "query": ("Features to attention queries.",
              "Changes query features and downstream attention scores, not a fixed instruction preference."),
    "key": ("Features to attention keys.", "Changes how positions match queries."),
    "value": ("Features to the information aggregated by attention.",
              "Changes retrieved information, not necessarily which positions receive attention."),
    "attention_output": ("Attention features back to the residual representation.",
                         "Changes how retrieved features influence subsequent layers."),
    "router": ("Features to expert-routing scores.",
               "Can change selected experts near routing boundaries; no expert topic is established."),
    "gate": ("Features to feed-forward gating inputs.",
             "Changes nonlinear gating of intermediate features."),
    "up": ("Features to feed-forward intermediate values.",
           "Changes the intermediate values subsequently gated and projected."),
    "down": ("Intermediate feed-forward features back to the residual representation.",
             "Changes what an active intermediate feature writes to the residual stream."),
    "norm": ("Feature scaling in a normalization operation.",
             "Changes relative feature gains; exact offset convention requires runtime inspection."),
    "vision": ("Visual encoding or image-to-language projection.",
               "May change image representations; exact role requires runtime inspection."),
    "unknown": ("No specific role established by the supported naming rules.",
                "Unknown; inspect the runtime computation before proposing an intervention."),
}


def tensor_role(row):
    """Conservative name-based role, explicitly separate from observed behavior."""
    name, kind = row["name"].lower(), row["component"]
    if kind in ("vision", "embedding", "head", "norm"):
        return kind
    if ".router.proj.weight" in name or name.endswith(".mlp.gate.weight"):
        return "router"
    if kind == "attention":
        for suffix, role in (("q_proj.weight", "query"), ("k_proj.weight", "key"),
                             ("v_proj.weight", "value"), ("o_proj.weight", "attention_output")):
            if name.endswith("." + suffix):
                return role
    if kind in ("mlp", "moe"):
        for suffix, role in (("gate_proj.weight", "gate"), ("up_proj.weight", "up"),
                             ("down_proj.weight", "down")):
            if name.endswith("." + suffix):
                return role
    return "unknown"


def weight_map(request, root, config):
    rows = tensors(root, config)
    for row in rows:
        row["role"] = tensor_role(row)
    layers = {}
    for row in rows:
        if row["layer"] is not None:
            entry = layers.setdefault(row["layer"], {})
            entry[row["component"]] = entry.get(row["component"], 0) + row["params"]
    dtypes = {}
    for row in rows:
        dtypes[row["dtype"]] = dtypes.get(row["dtype"], 0) + 1
    return {
        "architecture": architecture(config),
        "knowledge": {
            "evidence": "config and safetensors headers; roles inferred from tensor names",
            "roles": {role: {"function": ROLES[role][0], "change_effect": ROLES[role][1],
                             "evidence_kind": "structural_inference"}
                      for role in sorted({row["role"] for row in rows})},
            "linear_effect": "For y = W x with fixed x, changing W[i,j] by delta changes y[i] by delta*x[j], before subsequent nonlinear operations.",
            "quantization": "Packed integers are not individual floating-point weights. For affine groups w=s*q+b, scale and bias edits affect a whole group; rounding can erase small edits.",
            "limits": "No activations, causal interventions, numerical weight scan or semantic specializations were measured. Behavioral effects require controlled experiments.",
        },
        "tensors": rows,
        "layers": [{"layer": layer, "params": sum(parts.values()), "components": parts}
                   for layer, parts in sorted(layers.items())],
        "totals": {"tensors": len(rows), "params": sum(row["params"] for row in rows),
                   "bytes": sum(row["bytes"] for row in rows), "dtypes": dtypes,
                   "quantized": any(row["quantized"] for row in rows)},
    }


# ---------------------------------------------------------------------------
# Tensor statistics
# ---------------------------------------------------------------------------

def measure(arrays, row):
    """Moments of one tensor, dequantized and reduced in bounded chunks."""
    import mlx.core as mx
    module = row["name"].rpartition(".")[0]
    weight = arrays.pop(row["name"])
    scales = arrays.pop(f"{module}.scales", None) if row["quantized"] else None
    biases = arrays.pop(f"{module}.biases", None) if row["quantized"] else None
    if row["quantized"] and row["quantization"] is None:
        return {"skipped": "quantization parameters are missing from config.json"}
    if not row["quantized"] and not mx.issubdtype(weight.dtype, mx.floating):
        return {"skipped": "not a floating-point tensor"}
    if weight.size == 0:
        return {"skipped": "empty tensor"}
    width = weight.shape[-1] if weight.ndim else 1
    flat = weight.reshape(-1, width)
    if scales is not None:
        scales = scales.reshape(-1, scales.shape[-1])
        biases = None if biases is None else biases.reshape(-1, biases.shape[-1])
    step = max(1, CHUNK_ELEMENTS // max(1, row["shape"][-1] if row["shape"] else 1))
    count, total, squares, peak = 0, 0.0, 0.0, 0.0
    for start in range(0, flat.shape[0], step):
        part = flat[start:start + step]
        if scales is not None:
            settings = row["quantization"]
            part = mx.dequantize(part, scales[start:start + step],
                                 None if biases is None else biases[start:start + step],
                                 group_size=settings["group_size"], bits=settings["bits"],
                                 mode=settings["mode"])
        part = part.astype(mx.float32)
        values = mx.stack([mx.sum(part), mx.sum(part * part), mx.max(mx.abs(part))])
        mx.eval(values)
        part_sum, part_squares, part_peak = values.tolist()
        count += part.size
        total += part_sum
        squares += part_squares
        peak = max(peak, part_peak)
    if not all(math.isfinite(value) for value in (total, squares, peak)):
        return {"nonfinite": True}
    mean, rms = total / count, math.sqrt(squares / count)
    return {"norm": math.sqrt(squares), "rms": rms, "mean": mean,
            "std": math.sqrt(max(rms * rms - mean * mean, 0.0)), "absmax": peak,
            "outlier_ratio": peak / rms if rms > 0 else 0.0}


def layer_summary(rows):
    layers = {}
    for row in rows:
        if row["layer"] is None or "norm" not in row:
            continue
        entry = layers.setdefault(row["layer"], {})
        squares, count = entry.get(row["component"], (0.0, 0))
        entry[row["component"]] = (squares + row["norm"] ** 2, count + row["params"])
    return [{
        "layer": layer,
        "norm": math.sqrt(sum(squares for squares, _ in parts.values())),
        "components": {name: {"norm": math.sqrt(squares), "rms": math.sqrt(squares / count) if count else 0.0}
                       for name, (squares, count) in parts.items()},
    } for layer, parts in sorted(layers.items())]


def stats(request, root, config):
    import mlx.core as mx
    prefix = request.get("prefix") or ""
    require(isinstance(prefix, str) and len(prefix) <= 512, "prefix must be a string of at most 512 characters")
    selected = [row for row in tensors(root, config) if row["name"].startswith(prefix)]
    require(bool(selected), "no tensor name starts with prefix")
    loaded, results = {}, []
    for index, row in enumerate(selected):
        report(request, "measuring tensors", index, len(selected))
        if row["shard"] not in loaded:
            # Lazy: each tensor is read on first evaluation and released once popped.
            loaded[row["shard"]] = mx.load(str(root / row["shard"]))
        result = {key: row[key] for key in ("name", "layer", "component", "shape", "dtype", "params", "quantized")}
        result.update(measure(loaded[row["shard"]], row))
        results.append(result)
        mx.clear_cache()
    report(request, "measured", len(selected), len(selected))
    return {"prefix": prefix, "tensors": results, "layers": layer_summary(results)}


# ---------------------------------------------------------------------------
# Activations and ablation (model load)
# ---------------------------------------------------------------------------

def tool_list(value, label):
    require(value is None or (isinstance(value, list) and len(value) <= 16
                              and all(isinstance(item, dict) for item in value)),
            f"{label} tools must be a list of at most 16 objects")
    return value


def contrast_pairs(request):
    pairs = request.get("pairs")
    require(isinstance(pairs, list) and 1 <= len(pairs) <= MAX_PAIRS,
            f"activations require 1–{MAX_PAIRS} contrast pairs")
    for number, pair in enumerate(pairs):
        require(isinstance(pair, dict), f"pair {number} must be an object")
        positive = steering.messages(pair.get("positive"), True)
        negative = steering.messages(pair.get("negative"), True)
        require(positive[:-1] == negative[:-1] and positive[-1] != negative[-1],
                f"pair {number} must share a prompt and differ only in the final assistant response")
        tool_list(pair.get("tools"), f"pair {number}")
    return pairs


def activations(request, root, config):
    """Per-layer separation of matched responses at the last token of each conversation."""
    import mlx.core as mx
    from dial_worker import reasoning_renderer
    pairs = contrast_pairs(request)
    model, tokenizer = steering.load(request)
    render = reasoning_renderer(tokenizer)
    count = len(steering.layer_list(model))
    total = magnitude = positive_norm = negative_norm = None
    for number, pair in enumerate(pairs):
        report(request, "capturing activations", number, len(pairs))
        states = {}
        for key in ("positive", "negative"):
            sinks = [[] for _ in range(count)]
            with contextlib.ExitStack() as stack:
                for index in range(count):
                    stack.enter_context(steering.hook(
                        model, index, lambda output, sink=sinks[index]: steering.capture(sink, output)))
                tokens = steering.prompt(tokenizer, render(pair[key]), True, tools=pair.get("tools"))
                logits = model(mx.array([tokens], dtype=mx.int32))
                mx.eval(logits, *[item for sink in sinks for item in sink])
            require(all(len(sink) == 1 for sink in sinks), "every layer must run exactly once per response")
            require(len({sink[0].shape[-1] for sink in sinks}) == 1, "layers have different hidden sizes")
            states[key] = mx.stack([sink[0] for sink in sinks])
        difference = states["positive"] - states["negative"]
        norms = [mx.sqrt(mx.sum(value * value, axis=1)) for value in (difference, states["positive"], states["negative"])]
        if total is None:
            total, magnitude, positive_norm, negative_norm = difference, *norms
        else:
            total, magnitude = total + difference, magnitude + norms[0]
            positive_norm, negative_norm = positive_norm + norms[1], negative_norm + norms[2]
        mx.eval(total, magnitude, positive_norm, negative_norm)
        mx.clear_cache()
    contrast = (mx.sqrt(mx.sum(total * total, axis=1)) / len(pairs)).tolist()
    magnitude = (magnitude / len(pairs)).tolist()
    scale = ((positive_norm + negative_norm) / (2 * len(pairs))).tolist()
    rows = []
    for index in range(count):
        values = (contrast[index], magnitude[index], scale[index])
        require(all(math.isfinite(value) for value in values), "activations produced nonfinite values")
        relative = contrast[index] / scale[index] if scale[index] > 0 else 0.0
        # 1 when every pair moves the residual stream the same way; near 0 when they cancel.
        alignment = contrast[index] / magnitude[index] if magnitude[index] > 0 else 0.0
        rows.append({"layer": index, "contrast": contrast[index], "residual_norm": scale[index],
                     "relative_contrast": relative, "alignment": alignment, "score": relative * alignment})
    report(request, "captured", len(pairs), len(pairs))
    best = max(rows, key=lambda row: row["score"])
    return {"pairs": len(pairs), "dimension": int(total.shape[-1]), "layers": rows,
            "suggested_layer": best["layer"]}


def loss(model, encoded):
    """Mean negative log-likelihood per answer token, and the answer token count."""
    from dial_worker import target_logprobs
    results = target_logprobs(model, encoded)
    tokens = sum(count for _, count in results)
    require(tokens > 0, "trace cases have no answer tokens")
    return -sum(total for total, _ in results) / tokens, tokens


def probe_encoded(tokenizer):
    """The built-in probe texts as (tokens, 1) pairs: every answer position after the first is scored."""
    encoded = []
    for number, text in enumerate(PROBES):
        tokens = [int(token) for token in tokenizer.encode(text)]
        require(2 <= len(tokens) <= MAX_TOKENS, f"probe {number} does not tokenize to 2-{MAX_TOKENS} tokens")
        encoded.append((tokens, 1))
    return encoded


@contextlib.contextmanager
def skipped(model, index):
    """Run with layer `index` replaced by the identity on its residual input."""
    layers = steering.layer_list(model)
    original = layers[index]
    inputs = []

    def observe(*args, **kwargs):
        inputs.append(args[0] if args else kwargs.get("x"))

    def bypass(output):
        steering.hidden(output)
        return inputs.pop()
    layers[index] = steering.wrap(original, bypass, observe)
    try:
        yield
    finally:
        layers[index] = original


def attention_heads(model, index):
    layer = steering.layer_list(model)[index]
    attention = getattr(layer, "self_attn", None)
    if attention is None:
        attention = getattr(layer, "attention", None)
    heads = None
    if attention is not None:
        heads = next((getattr(attention, key) for key in ("n_heads", "num_attention_heads", "num_heads")
                      if type(getattr(attention, key, None)) is int), None)
    require(heads is not None and hasattr(attention, "o_proj"),
            f"layer {index} has no standard multi-head attention with an output projection; "
            "linear-attention layers cannot be traced per head")
    require(1 <= heads <= MAX_HEADS, f"layer {index} has more than {MAX_HEADS} heads")
    return attention, heads


@contextlib.contextmanager
def head_removed(model, index, head):
    """Zero one head's slice of the attention output before its output projection."""
    import mlx.core as mx
    from mlx import nn
    attention, heads = attention_heads(model, index)
    projection = attention.o_proj

    class Masked(nn.Module):
        def __init__(self):
            super().__init__()
            self["inner"] = projection

        def __call__(self, x):
            width = x.shape[-1]
            require(width % heads == 0, f"layer {index} attention width does not split into {heads} heads")
            size = width // heads
            positions = mx.arange(width)
            keep = mx.logical_or(positions < head * size, positions >= (head + 1) * size)
            return self.inner(x * keep.astype(x.dtype))
    attention.o_proj = Masked()
    try:
        yield
    finally:
        attention.o_proj = projection


def trace_cases(request, key="cases", limit=MAX_CASES):
    cases = request.get(key)
    require(isinstance(cases, list) and 1 <= len(cases) <= limit, f"{key} requires 1–{limit} conversations")
    for number, case in enumerate(cases):
        require(isinstance(case, dict), f"{key} {number} must be an object")
        raw = case.get("messages")
        require(isinstance(raw, list) and bool(raw) and isinstance(raw[-1], dict) and raw[-1].get("role") == "assistant",
                f"{key} {number} must end with the assistant answer to score")
        steering.messages(raw, True)
        tool_list(case.get("tools"), f"{key} {number}")
    return cases


def trace(request, root, config):
    """Answer loss with each layer (or each head of one layer) removed, against the intact model."""
    from dial_worker import encode, reasoning_renderer
    cases = trace_cases(request)
    mode = request.get("mode", "layers")
    require(mode in ("layers", "heads"), "mode must be layers or heads")
    if mode == "heads":
        require(type(request.get("layer")) is int, "heads mode needs an integer layer")
    model, tokenizer = steering.load(request)
    render = reasoning_renderer(tokenizer)
    encoded = []
    for number, case in enumerate(cases):
        tokens, offset = encode(tokenizer, render(case["messages"]), case.get("tools"))
        require(0 < offset < len(tokens) <= MAX_TOKENS,
                f"case {number} has no answer tokens or exceeds {MAX_TOKENS} tokens")
        encoded.append((tokens, offset))
    count = len(steering.layer_list(model))
    baseline, answer_tokens = loss(model, encoded)
    result = {"mode": mode, "cases": len(cases), "answer_tokens": answer_tokens, "baseline_loss": baseline,
              "layer_count": count}
    if mode == "heads":
        index = request["layer"]
        require(0 <= index < count, "layer is out of range")
        _, heads = attention_heads(model, index)
        rows = []
        for head in range(heads):
            report(request, "ablating heads", head, heads)
            with head_removed(model, index, head):
                value, _ = loss(model, encoded)
            rows.append({"head": head, "loss": value, "delta": value - baseline})
        report(request, "traced", heads, heads)
        return dict(result, layer=index, heads=rows)
    selected = request.get("layers")
    if selected is None:
        selected = list(range(count))
    require(isinstance(selected, list) and 1 <= len(selected) <= count
            and all(type(index) is int and 0 <= index < count for index in selected)
            and len(set(selected)) == len(selected), "layers must be distinct indices below the layer count")
    rows = []
    for position, index in enumerate(selected):
        report(request, "ablating layers", position, len(selected))
        with skipped(model, index):
            value, _ = loss(model, encoded)
        rows.append({"layer": index, "loss": value, "delta": value - baseline})
    report(request, "traced", len(selected), len(selected))
    return dict(result, layers=rows)


# Deliberately small diagnostics, not a benchmark or claims about a layer's specialization.
INVESTIGATION_PROBES = (
    ("arithmetic", "What is 7 + 5? Reply with only the number.", "12"),
    ("copy", "Repeat exactly: amber violet amber", "amber violet amber"),
    ("json", 'Return only a JSON object with key "ok" and boolean value true.', '{"ok": true}'),
    ("code", "In Python, what does len([3, 1, 4]) return? Reply with only the number.", "3"),
)


def investigation_plan(count, case_count, selected, budget):
    """Bound all model calls, including one intact repeat after interventions."""
    require(type(budget) is int and 3 <= budget <= 256, "investigate budget must be 3–256 forward passes")
    require(type(count) is int and 1 <= count <= 256, "unsupported investigation layer count")
    if selected is None:
        size = min(4, count, budget // case_count - 2)
        require(size >= 1, "investigate budget cannot cover baseline, one layer and restoration")
        selected = [i * (count - 1) // (size - 1) for i in range(size)] if size > 1 else [count // 2]
    require(isinstance(selected, list) and 1 <= len(selected) <= 16
            and all(type(i) is int and 0 <= i < count for i in selected)
            and len(set(selected)) == len(selected), "investigate layers must be 1–16 distinct indices below the layer count")
    passes = case_count * (len(selected) + 2)
    require(passes <= budget, "investigate budget cannot cover selected layers and restoration")
    return selected, passes


def investigate(request, root, config):
    """Measure case-specific residual-output interventions, not individual-weight meanings."""
    from dial_worker import encode, reasoning_renderer
    if "cases" in request:
        cases = trace_cases(request)
        labels = ["custom"] * len(cases)
        source = "user-supplied reference answers"
    else:
        cases = [{"messages": [{"role": "user", "content": prompt},
                               {"role": "assistant", "content": answer}]}
                 for _, prompt, answer in INVESTIGATION_PROBES]
        labels = [label for label, _, _ in INVESTIGATION_PROBES]
        source = "dial-investigation-probes-v1"
    budget = request.get("budget", 24)
    # Reject static budget/index errors before allocating the model when metadata permits.
    declared = architecture(config)["layers"]
    investigation_plan(declared if type(declared) is int else 256, len(cases), request.get("layers"), budget)
    model, tokenizer = steering.load(request)
    model.eval()
    count = len(steering.layer_list(model))
    selected, passes = investigation_plan(count, len(cases), request.get("layers"), budget)
    render = reasoning_renderer(tokenizer)
    encoded = []
    for number, case in enumerate(cases):
        tokens, offset = encode(tokenizer, render(case["messages"]), case.get("tools"))
        require(0 < offset < len(tokens) <= MAX_TOKENS,
                f"case {number} has no answer tokens or exceeds {MAX_TOKENS} tokens")
        encoded.append((tokens, offset))
    completed = 0

    def score():
        nonlocal completed
        values = []
        for item in encoded:
            require(completed < budget, "investigation forward-pass budget exhausted")
            report(request, "investigating", completed, passes)
            value, tokens = loss(model, [item])
            require(math.isfinite(value), "investigation produced nonfinite loss")
            values.append((value, tokens))
            completed += 1
        return values

    baseline = score()
    rows = []
    total_tokens = sum(tokens for _, tokens in baseline)
    for index in selected:
        with skipped(model, index):
            measured = score()
        per_case = []
        for number, ((base, tokens), (value, measured_tokens)) in enumerate(zip(baseline, measured)):
            require(tokens == measured_tokens, "intervention changed the scored token count")
            per_case.append({"case": number, "probe": labels[number], "loss": value, "delta": value - base})
        delta = sum(row["delta"] * baseline[i][1] for i, row in enumerate(per_case)) / total_tokens
        rows.append({"layer": index, "delta": delta, "cases": per_case})
    restored = score()
    for (base, tokens), (value, restored_tokens) in zip(baseline, restored):
        require(tokens == restored_tokens and math.isclose(base, value, rel_tol=1e-5, abs_tol=1e-5),
                "intact baseline was not reproduced after interventions; results not retained")
    report(request, "investigated", completed, passes)
    return {
        "evidence_kind": "measured_residual_output_intervention",
        "intervention": "execute layer, then replace its residual output with its input",
        "case_source": source, "case_count": len(cases), "layer_count": count,
        "budget": budget, "forward_passes": completed, "restoration_checked": True,
        "cases": [{"case": i, "probe": labels[i], "fingerprint": steering.digest(case),
                   "baseline_loss": baseline[i][0], "answer_tokens": baseline[i][1],
                   "restored_loss": restored[i][0]} for i, case in enumerate(cases)],
        "layers": rows,
        "limits": "Effects apply only to these reference answers under this intervention. Positive loss delta means less likelihood, not lower general quality. Layer computation still executes; side outputs such as shared KV are not ablated. No individual-weight semantics, expert specializations, confidence intervals or beneficial weight edits are established. Checkpoint files are unchanged.",
    }


# ---------------------------------------------------------------------------
# Generation, directions and project-out edits
# ---------------------------------------------------------------------------

def generate_cases(request):
    cases = request.get("cases")
    require(isinstance(cases, list) and 1 <= len(cases) <= MAX_GENERATE_CASES,
            f"generate requires 1–{MAX_GENERATE_CASES} cases")
    for number, case in enumerate(cases):
        require(isinstance(case, dict), f"case {number} must be an object")
        raw = case.get("messages")
        require(isinstance(raw, list) and bool(raw) and isinstance(raw[-1], dict) and raw[-1].get("role") == "user",
                f"case {number} must end with the user turn to answer")
        steering.messages(raw)
        steering.contract(case)
    return cases


def generate(request, root, config):
    """Greedy answers per case: tokens, whether the token limit cut them off, and loop ratio."""
    import mlx.core as mx
    cases = generate_cases(request)
    limit = request.get("max_tokens", 512)
    require(type(limit) is int and 1 <= limit <= MAX_NEW_TOKENS, f"max_tokens must be 1–{MAX_NEW_TOKENS}")
    tail = request.get("tail", 0)
    require(type(tail) is int and 0 <= tail <= MAX_TAIL, f"tail must be 0–{MAX_TAIL} characters")
    probe = request.get("probe_loss", False)
    require(type(probe) is bool, "probe_loss must be true or false")
    model, tokenizer = steering.load(request)
    rows = []
    for number, case in enumerate(cases):
        report(request, "generating", number, len(cases))
        result = steering.generate_case(model, tokenizer, case, limit, tail)
        mx.clear_cache()
        row = {"case": number, "tokens": result["tokens"], "finish_reason": result["finish_reason"],
               "hit_limit": result["finish_reason"] == "length", "repeat_ratio": result["repeat_ratio"],
               "passed": result["passed"] if steering.contract(case) else None, "seconds": result["seconds"],
               "thinking_tokens": result.get("thinking_tokens")}
        if tail:
            row["tail"] = result["tail"]
        rows.append(row)
    report(request, "generated", len(cases), len(cases))
    checked = [row["passed"] for row in rows if row["passed"] is not None]
    probes = {}
    if probe:
        value, count = loss(model, probe_encoded(tokenizer))
        probes = {"probe_loss": value, "probe_tokens": count}
    thinking = [row["thinking_tokens"] for row in rows if row["thinking_tokens"] is not None]
    return {"cases": rows, "max_tokens": limit, "summary": {
        "cases": len(rows), "hit_limit": sum(row["hit_limit"] for row in rows),
        "mean_tokens": sum(row["tokens"] for row in rows) / len(rows),
        "mean_thinking_tokens": sum(thinking) / len(thinking) if thinking else None,
        "mean_repeat_ratio": sum(row["repeat_ratio"] for row in rows) / len(rows),
        "checked": len(checked), "passed": sum(checked)}, **probes}


def conversations(request, key):
    value = request.get(key)
    require(isinstance(value, list) and 1 <= len(value) <= MAX_PAIRS,
            f"direction requires 1–{MAX_PAIRS} {key} conversations")
    for number, item in enumerate(value):
        require(isinstance(item, dict), f"{key} {number} must be an object")
        raw = item.get("messages")
        require(isinstance(raw, list) and bool(raw) and isinstance(raw[-1], dict) and raw[-1].get("role") == "assistant",
                f"{key} {number} must end with the assistant answer to measure")
        steering.messages(raw, True)
        tool_list(item.get("tools"), f"{key} {number}")
    return value


@contextlib.contextmanager
def module_hook(model, index, key, callback):
    """Observe the output of layer `index`'s `key` module (its MLP or linear attention)."""
    layer = steering.layer_list(model)[index]
    original = getattr(layer, key, None)
    require(original is not None, f"layer {index} has no {key} module")
    setattr(layer, key, steering.wrap(original, callback))
    try:
        yield
    finally:
        setattr(layer, key, original)


@contextlib.contextmanager
def mlp_hook(model, index, callback):
    """Observe layer `index`'s MLP output: what the MLP writes into the residual stream."""
    require(getattr(steering.layer_list(model)[index], "mlp", None) is not None,
            f"layer {index} has no mlp module; use site residual")
    with module_hook(model, index, "mlp", callback):
        yield


def answer_mean(sink, offset):
    import mlx.core as mx

    def capture(output):
        steering.hidden(output)
        sink.append(mx.mean(output[0, offset:, :].astype(mx.float32), axis=0))
        return output
    return capture


def region_means(sink, regions):
    """Capture the mean output over each region, in order. A region is a list of (start, end)
    position ranges averaged together."""
    import mlx.core as mx

    def capture(output):
        steering.hidden(output)
        values = output[0].astype(mx.float32)
        sink.append([mx.mean(mx.concatenate([values[start:end] for start, end in region]), axis=0)
                     for region in regions])
        return output
    return capture


def find_tokens(tokens, pattern, start=0):
    """Index of the first occurrence of `pattern` in tokens[start:], or None."""
    pattern = tuple(pattern)
    width = len(pattern)
    if not width:
        return None
    for index in range(start, len(tokens) - width + 1):
        if tuple(tokens[index:index + width]) == pattern:
            return index
    return None


def direction_span(value):
    """A validated `span` ({last, before}) or None for every answer token."""
    if value is None:
        return None
    require(isinstance(value, dict) and set(value) <= {"last", "before"},
            "span must be {last, before?}")
    last, before = value.get("last"), value.get("before", "answer_end")
    require(type(last) is int and 1 <= last <= MAX_SPAN, f"span last must be 1-{MAX_SPAN} tokens")
    require(before in ("answer_end", "think_end"), "span before must be answer_end or think_end")
    return {"last": last, "before": before}


def span_regions(tokens, offset, span, think_end, what):
    """(selected, rest) answer regions, each a list of position ranges. Without a span every answer
    token is selected and there is no rest. With one, the `last` tokens before the end of the answer
    or the end-of-thinking marker are selected, and every other answer token, before or after them,
    is the rest."""
    if span is None:
        return [(offset, len(tokens))], None
    anchor = len(tokens)
    if span["before"] == "think_end":
        require(think_end, "this model's tokenizer has no end-of-thinking marker; use span before answer_end")
        anchor = find_tokens(tokens, think_end, offset)
        require(anchor is not None, f"{what} has no end-of-thinking marker in its answer")
    start = max(offset, anchor - span["last"])
    require(start < anchor, f"{what} has no answer tokens before the span's end")
    rest = [(low, high) for low, high in ((offset, start), (anchor, len(tokens))) if low < high]
    return [(start, anchor)], rest


def mlp_writers(rows, layer):
    """Every tensor of `layer` that writes MLP output into the residual stream: the dense down
    projection, a shared expert's, and the stack of routed experts' (one slice per expert). Each is
    {kind, owner, row}; `owner` is the module path inside the layer that holds the projection."""
    found = []
    for row in rows:
        name = row["name"]
        if row["layer"] != layer or row["component"] not in ("mlp", "moe") or not name.endswith(".weight"):
            continue
        path = name[LAYER.search(name).end():-len(".weight")]
        owner, _, leaf = path.rpartition(".")
        if leaf not in OUTPUT_PROJECTIONS or not owner:
            continue
        if len(row["shape"]) == 3:
            kind = "experts"
        elif "shared_expert" in path and len(row["shape"]) == 2:
            kind = "shared"
        elif row["component"] == "mlp" and len(row["shape"]) == 2:
            kind = "dense"
        else:
            continue
        found.append({"kind": kind, "owner": owner, "leaf": leaf, "row": row})
    kinds = [writer["kind"] for writer in found]
    require(found, f"layer {layer} has no MLP output projection to edit")
    require(len(set(kinds)) == len(kinds), f"layer {layer} has more than one MLP output projection of a kind")
    return found


def submodule(module, path):
    for part in path.split("."):
        module = getattr(module, part, None)
        require(module is not None, f"module {path} is missing from the loaded model")
    return module


@contextlib.contextmanager
def swapped(module, path, replacement):
    """Replace the submodule at dotted `path` for the duration of the block."""
    parent, _, leaf = path.rpartition(".")
    owner = submodule(module, parent) if parent else module
    original = getattr(owner, leaf)
    setattr(owner, leaf, replacement)
    try:
        yield
    finally:
        setattr(owner, leaf, original)


def writer_recorder(writer, layer_module, regions, sink):
    """A (path, replacement) that records, over `regions`, what one MLP writer reads and writes: the
    summed input, the summed output norm and the number of positions, per expert for a stack.
    None when the writer's module is not one this can observe (only SwitchGLU expert stacks are)."""
    import mlx.core as mx
    owner = submodule(layer_module, writer["owner"])
    if writer["kind"] != "experts":
        inner = getattr(owner, writer["leaf"])

        def observe(x, *rest, **kwargs):
            parts = [x[0, start:end] for start, end in regions]
            hidden = mx.concatenate(parts).astype(mx.float32)
            out = inner(hidden[None].astype(x.dtype))[0].astype(mx.float32)
            sink.append((mx.sum(hidden, axis=0)[None], mx.sum(mx.sqrt(mx.sum(out * out, axis=-1)))[None],
                         mx.array([float(hidden.shape[0])])))
        return f"{writer['owner']}.{writer['leaf']}", steering.wrap(inner, lambda output: output, observe)
    if not all(hasattr(owner, key) for key in ("up_proj", "gate_proj", "down_proj", "activation")):
        return None
    experts = writer["row"]["shape"][0]

    def observe(x, indices, *rest, **kwargs):
        xs = mx.concatenate([x[:, start:end] for start, end in regions], axis=1)
        chosen = mx.concatenate([indices[:, start:end] for start, end in regions], axis=1)
        expanded = mx.expand_dims(xs, (-2, -3))
        hidden = owner.activation(owner.up_proj(expanded, chosen), owner.gate_proj(expanded, chosen))
        out = owner.down_proj(hidden, chosen)
        flat = chosen.reshape(-1)
        hidden = hidden.reshape(flat.shape[0], -1).astype(mx.float32)
        out = out.reshape(flat.shape[0], -1).astype(mx.float32)
        norms = mx.sqrt(mx.sum(out * out, axis=-1))
        sink.append((mx.zeros((experts, hidden.shape[-1])).at[flat].add(hidden),
                     mx.zeros((experts,)).at[flat].add(norms),
                     mx.zeros((experts,)).at[flat].add(mx.ones_like(norms))))
    return writer["owner"], steering.wrap(owner, lambda output: output, observe)


def direction(request, root, config):
    """Unit mean difference (positive minus negative) of answer-token activations, per layer.

    `positive` conversations show the behaviour the direction points toward; `negative` ones show
    the other behaviour. Site `mlp` reads each layer's MLP output, `residual` the layer output.
    `span` ({last, before}) keeps only the last tokens before the end of the answer or before the
    end-of-thinking marker; without `negative`, the span is contrasted with the answer tokens before
    it in the same positive conversations. Each layer's MLP writers also get their mean input and
    mean output norm over every answer token measured, which the `add` edit needs. The directions
    are saved to `output` for `weights_edit`.
    """
    import mlx.core as mx
    from dial_worker import encode, reasoning_renderer
    site = request.get("site", "mlp")
    require(site in ("mlp", "residual"), "site must be mlp or residual")
    output = request.get("output")
    require(isinstance(output, str) and output.endswith(".safetensors") and not Path(output).exists()
            and Path(output).parent.is_dir(), "output must be a new .safetensors path in an existing directory")
    span = direction_span(request.get("span"))
    within = request.get("negative") is None
    require(not within or span is not None,
            "direction needs `negative` conversations, or a `span` to contrast with the rest of the positive answers")
    groups = {key: conversations(request, key) for key in (("positive",) if within else ("positive", "negative"))}
    model, tokenizer = steering.load(request)
    render = reasoning_renderer(tokenizer)
    think_end = tuple(getattr(tokenizer, "think_end_tokens", None) or ())
    layers = steering.layer_list(model)
    count = len(layers)
    rows_by_layer = {}
    checkpoint = tensors(root, config)
    for index in range(count):
        try:
            rows_by_layer[index] = mlp_writers(checkpoint, index)
        except WeightsError:
            rows_by_layer[index] = []
    writer_sums = {}
    total, done, samples = sum(len(items) for items in groups.values()), 0, {"positive": [], "negative": []}
    for key, items in groups.items():
        for number, item in enumerate(items):
            report(request, "capturing answers", done, total)
            tokens, offset = encode(tokenizer, render(item["messages"]), item.get("tools"))
            require(0 < offset < len(tokens) <= MAX_TOKENS,
                    f"{key} {number} has no answer tokens or exceeds {MAX_TOKENS} tokens")
            selected, rest = span_regions(tokens, offset, span, think_end, f"{key} {number}")
            require(not within or rest,
                    f"{key} {number} has no answer tokens outside the span to contrast it with; shorten the span")
            regions = [selected] + ([rest] if within else [])
            sinks = [[] for _ in range(count)]
            observed = {}
            with contextlib.ExitStack() as stack:
                # Writers are swapped inside the original modules before any site hook wraps them.
                for index in range(count):
                    for writer in rows_by_layer[index]:
                        sink = observed.setdefault((index, writer["kind"]), [])
                        swap = writer_recorder(writer, layers[index], [(offset, len(tokens))], sink)
                        if swap is not None:
                            stack.enter_context(swapped(layers[index], *swap))
                for index in range(count):
                    callback = region_means(sinks[index], regions)
                    stack.enter_context(mlp_hook(model, index, callback) if site == "mlp"
                                        else steering.hook(model, index, callback))
                model(mx.array([tokens], dtype=mx.int32))
                mx.eval(*[value for sink in sinks for means in sink for value in means],
                        *[value for sink in observed.values() for entry in sink for value in entry])
            require(all(len(sink) == 1 for sink in sinks), "every layer must run exactly once per conversation")
            samples[key].append(mx.stack([sink[0][0] for sink in sinks]))
            if within:
                samples["negative"].append(mx.stack([sink[0][1] for sink in sinks]))
            for place, entries in observed.items():
                for entry in entries:
                    before = writer_sums.get(place)
                    writer_sums[place] = entry if before is None else tuple(a + b for a, b in zip(before, entry))
            mx.eval(*[value for entry in writer_sums.values() for value in entry])
            done += 1
            mx.clear_cache()
    positive, negative = mx.stack(samples["positive"]), mx.stack(samples["negative"])
    samples = {"positive": positive, "negative": negative}
    gap = mx.mean(positive, axis=0) - mx.mean(negative, axis=0)
    norm = mx.sqrt(mx.sum(gap * gap, axis=-1))
    unit = gap / mx.maximum(norm, 1e-12)[:, None]
    projected = {key: mx.sum(value * unit[None], axis=-1) for key, value in samples.items()}
    scale = mx.mean(mx.sqrt(mx.sum(mx.concatenate([positive, negative]) ** 2, axis=-1)), axis=0)
    mx.eval(unit, norm, scale, *projected.values())
    norms, scales = norm.tolist(), scale.tolist()
    require(all(math.isfinite(value) and value > 0 for value in norms),
            "positive and negative answers produced no finite difference at some layer")
    spread = {key: mx.var(value, axis=0).tolist() for key, value in projected.items()}
    rows = []
    for index in range(count):
        pooled = math.sqrt((spread["positive"][index] + spread["negative"][index]) / 2)
        rows.append({"layer": index, "gap": norms[index],
                     "relative_gap": norms[index] / scales[index] if scales[index] > 0 else 0.0,
                     # Projected gap in pooled standard deviations; null when every sample agrees.
                     "separation": norms[index] / pooled if pooled > 0 else None})
    saved = {"directions": unit}
    for (index, kind), (inputs, out_norms, counts) in writer_sums.items():
        seen = mx.maximum(counts, 1.0)
        saved[f"inputs.{index}.{kind}"] = inputs / seen[:, None]
        saved[f"norms.{index}.{kind}"] = out_norms / seen
        saved[f"counts.{index}.{kind}"] = counts
    staged = Path(output).with_name(f".{Path(output).stem}.partial.safetensors")
    mx.save_safetensors(str(staged), saved, metadata={
        "format": DIRECTION_FORMAT, "site": site, "model_id": steering.identity(root),
        "layers": str(count), "dimension": str(unit.shape[-1]), "writers": "1",
        "contrast": "within" if within else "groups", "span": json.dumps(span)})
    os.replace(staged, output)
    report(request, "measured", total, total)
    ranked = sorted(rows, key=lambda row: -(row["separation"] if row["separation"] is not None else row["relative_gap"]))
    writers = sorted({kind for _, kind in writer_sums})
    return {"site": site, "positive": len(groups["positive"]),
            "negative": None if within else len(groups["negative"]),
            "contrast": "within" if within else "groups", "span": span,
            "dimension": int(unit.shape[-1]), "layer_count": count, "layers": rows, "writers": writers,
            "suggested_layers": sorted(row["layer"] for row in ranked[:2]), "output": output}


def output_projection(rows, layer):
    found = [row for row in rows if row["layer"] == layer and row["component"] == "mlp"
             and row["name"].endswith(".weight") and row["name"].rsplit(".", 2)[-2] in OUTPUT_PROJECTIONS]
    require(len(found) == 1, f"layer {layer} has no single dense MLP output projection to edit")
    return found[0]


def dense_weight(arrays, row):
    """The tensor as float32, plus what `store_edit` needs to write it back in its stored form."""
    import mlx.core as mx
    name = row["name"]
    weight = arrays[name]
    if row["quantized"]:
        module = name.rpartition(".")[0]
        settings = row["quantization"]
        require(settings is not None, f"{name}: quantization parameters are missing from config.json")
        scales, biases = arrays[f"{module}.scales"], arrays.get(f"{module}.biases")
        dense = mx.dequantize(weight, scales, biases, group_size=settings["group_size"],
                              bits=settings["bits"], mode=settings["mode"])
        return dense.astype(mx.float32), (dense.dtype, scales, biases, settings)
    require(mx.issubdtype(weight.dtype, mx.floating), f"{name} is not a floating-point tensor")
    return weight.astype(mx.float32), (weight.dtype, None, None, None)


def store_edit(arrays, row, stored_form, edited, touched, count):
    """Write `edited` back as the tensor's stored form: (updates, stored float32, rewritten elements).

    Quantized weights are requantized; `touched` (rows x quantization groups) marks the groups that
    hold an edited element, and every other group keeps its original bytes. Float tensors are
    rewritten whole, and `count` is the number of elements the edit changed.
    """
    import mlx.core as mx
    name = row["name"]
    module = name.rpartition(".")[0]
    weight = arrays[name]
    stored_dtype, scales, biases, settings = stored_form
    if not row["quantized"]:
        updates = {name: edited.astype(stored_dtype)}
        return updates, updates[name].astype(mx.float32), count
    width, group = edited.shape[-1], settings["group_size"]
    require(width % group == 0 and weight.shape[-1] % (width // group) == 0,
            f"{name}: packed layout does not split into quantization groups")
    packed = mx.quantize(edited.astype(stored_dtype), group_size=group, bits=settings["bits"],
                         mode=settings["mode"])
    groups = mx.array(touched)
    words = mx.repeat(groups, weight.shape[-1] // (width // group), axis=-1)
    new_weight = mx.where(words, packed[0], weight)
    new_scales = mx.where(groups, packed[1], scales)
    updates = {name: new_weight, f"{module}.scales": new_scales}
    new_biases = None
    if biases is not None:
        require(len(packed) > 2, f"{name}: requantization returned no biases")
        new_biases = mx.where(groups, packed[2], biases)
        updates[f"{module}.biases"] = new_biases
    stored = mx.dequantize(new_weight, new_scales, new_biases, group_size=group,
                           bits=settings["bits"], mode=settings["mode"]).astype(mx.float32)
    return updates, stored, int(touched.sum()) * group


def unit_vector(vector):
    import mlx.core as mx
    vector = vector.astype(mx.float32)
    return vector / mx.maximum(mx.sqrt(mx.sum(vector * vector)), 1e-12)


def control_direction(direction, seed, layer):
    """A random unit vector orthogonal to `direction`, fixed by (`seed`, `layer`): what a
    norm-matched control edit writes instead of the measured direction."""
    import mlx.core as mx
    import numpy as np
    d = np.array(unit_vector(direction), dtype=np.float64)
    sample = np.random.default_rng([seed, layer]).standard_normal(d.shape[0])
    sample -= (sample @ d) * d
    sample /= np.linalg.norm(sample)
    return mx.array(sample.astype(np.float32))


def stacked(dense, vector, name):
    """`dense` as a stack of (out x in) matrices, one per expert (a single matrix is a stack of one)."""
    require(dense.ndim in (2, 3) and dense.shape[-2] == vector.shape[0],
            f"{name} does not write the direction's hidden size")
    return dense if dense.ndim == 3 else dense[None]


def store_delta(arrays, row, form, matrices, delta, sign, changed, count):
    """Write matrices + sign*delta back in the stored form; `changed` is an (experts x in) mask of
    edited input columns. Returns (updates, stored stack, rewritten elements, metrics)."""
    import mlx.core as mx
    import numpy as np
    name = row["name"]
    edited = matrices + sign * delta
    experts, rows_out, width = matrices.shape
    touched = None
    if row["quantized"]:
        group = row["quantization"]["group_size"]
        require(width % group == 0, f"{name}: packed layout does not split into quantization groups")
        groups = changed.reshape(experts, width // group, group).max(axis=2) > 0
        touched = np.ascontiguousarray(np.broadcast_to(groups[:, None, :], (experts, rows_out, width // group)))
        if len(row["shape"]) == 2:
            touched = touched[0]
    flat = edited if len(row["shape"]) == 3 else edited[0]
    updates, stored, rewritten = store_edit(arrays, row, form, flat, touched, count)
    stored = stored if stored.ndim == 3 else stored[None]
    values = mx.stack([mx.sqrt(mx.sum(delta * delta)), mx.sqrt(mx.sum((stored - edited) ** 2))])
    mx.eval(values, *updates.values())
    size, error = values.tolist()
    require(math.isfinite(size) and math.isfinite(error), f"{name}: edit produced nonfinite values")
    return updates, stored, rewritten, {"edit_norm": size, "requantization_error": error / size if size > 0 else 0.0}


def project_out(arrays, row, vector, columns, alpha, control=None):
    """Remove `alpha` of direction d from the `columns` output-projection columns most aligned with it.

    Column j of W is what hidden unit j writes to the residual stream; W[:, j] -= alpha*d*(d.W[:, j]).
    A stack of experts is edited expert by expert, each in its own most aligned columns. With
    `control` (a unit vector orthogonal to d) the same columns get alpha*r*(r.W[:, j]) instead,
    rescaled per matrix to the norm of the real edit. Quantized weights are dequantized, edited and
    requantized; groups holding no edited column keep their original bytes. The requantization
    error is relative to the size of the intended edit.
    """
    import mlx.core as mx
    import numpy as np
    name = row["name"]
    dense, form = dense_weight(arrays, row)
    d = unit_vector(vector)
    matrices = stacked(dense, d, name)
    experts, rows_out, width = matrices.shape
    require(columns <= width, f"{name} has only {width} columns")
    coefficients = (d[None, None, :] @ matrices)[:, 0, :]
    mx.eval(coefficients)
    raw = np.array(coefficients)
    chosen = np.sort(np.argsort(-np.abs(raw), axis=1, kind="stable")[:, :columns], axis=1)
    mask = np.zeros((experts, width), dtype=np.float32)
    np.put_along_axis(mask, chosen, 1.0, axis=1)
    selected = mx.array(mask)
    delta = alpha * d[None, :, None] * (coefficients * selected)[:, None, :]
    if control is not None:
        other = (control[None, None, :] @ matrices)[:, 0, :]
        replacement = alpha * control[None, :, None] * (other * selected)[:, None, :]
        size = mx.sqrt(mx.sum(delta * delta, axis=(1, 2)))
        replaced = mx.sqrt(mx.sum(replacement * replacement, axis=(1, 2)))
        delta = replacement * (size / mx.maximum(replaced, 1e-12))[:, None, None]
    updates, stored, rewritten, metrics = store_delta(arrays, row, form, matrices, delta, -1.0, mask,
                                                      experts * rows_out * columns)
    values = mx.stack([mx.sqrt(mx.sum(coefficients * coefficients)),
                       mx.sqrt(mx.sum(((d[None, None, :] @ stored)[:, 0, :]) ** 2))])
    before, after = values.tolist()
    result = {"tensor": name, "projection_before": before, "projection_after": after, **metrics,
              "edited_params": experts * rows_out * columns, "rewritten_params": rewritten}
    if experts == 1 and len(row["shape"]) == 2:
        result["columns"] = chosen[0].tolist()
    else:
        result.update(experts=experts, columns_per_expert=columns)
    return result, updates


def add_along(arrays, row, vector, inputs, norms, alpha, control=None):
    """Write `alpha` times the writer's mean output size along d: W += alpha*n*d*u^T/|u|^2.

    u is the writer's mean input and n its mean output norm over the answer tokens the direction
    was measured on, so on a typical input the edit adds about alpha*n along d: steering compiled
    into the weights. A stack of experts gets one such term per expert from that expert's own u and
    n; experts never routed there are left alone. With `control` (a unit vector orthogonal to d)
    the same term writes along it instead, which has exactly the same norm.
    """
    import mlx.core as mx
    import numpy as np
    name = row["name"]
    dense, form = dense_weight(arrays, row)
    d = unit_vector(vector)
    matrices = stacked(dense, d, name)
    experts, rows_out, width = matrices.shape
    u = inputs.astype(mx.float32).reshape(experts, -1)
    n = norms.astype(mx.float32).reshape(-1)
    require(u.shape[-1] == width and n.shape[0] == experts,
            f"{name} does not match the writer statistics saved with the direction; measure it again")
    square = mx.sum(u * u, axis=-1)
    weight = mx.where(square > 0, alpha * n / mx.maximum(square, 1e-12), 0.0)
    along = d if control is None else control
    delta = weight[:, None, None] * along[None, :, None] * u[:, None, :]
    mx.eval(weight)
    active = np.array(weight) > 0
    require(active.any(), f"{name}: the direction's answers never reached this writer")
    mask = np.repeat(active[:, None], width, axis=1).astype(np.float32)
    count = int(active.sum()) * rows_out * width
    updates, _, rewritten, metrics = store_delta(arrays, row, form, matrices, delta, 1.0, mask, count)
    typical = float(np.array(n)[active].mean()) * alpha
    result = {"tensor": name, "alpha": alpha, "write_norm": typical, **metrics,
              "edited_params": count, "rewritten_params": rewritten}
    if len(row["shape"]) == 3:
        result["experts"] = int(active.sum())
    return result, updates


def indices(value, limit, what):
    """A sorted, distinct index list below `limit`, or None for "all"."""
    import numpy as np
    if value is None:
        return None
    require(isinstance(value, list) and 1 <= len(value) <= MAX_EDIT_INDICES
            and all(type(n) is int and 0 <= n < limit for n in value),
            f"{what} must be 1-{MAX_EDIT_INDICES} integers below {limit}")
    return np.unique(np.array(value, dtype=np.int64))


def scale_tensor(arrays, row, factor, rows, columns, experts=None):
    """Multiply the selected elements of one tensor by `factor`, leaving every other element as stored.

    `rows` index the first axis and `columns` the last axis of a matrix (None selects all of it).
    A three-dimensional tensor is a stack of experts (switch_mlp), selected by `experts` along its
    first axis. Quantized tensors are requantized only in the groups holding a selected element.
    """
    import mlx.core as mx
    import numpy as np
    name = row["name"]
    dense, form = dense_weight(arrays, row)
    require(dense.ndim in (1, 2, 3),
            f"{name} has {dense.ndim} dimensions; only vectors, matrices and expert stacks can be scaled")
    shape = tuple(dense.shape)
    mask = np.zeros(shape, dtype=bool)
    chosen_rows = chosen_columns = chosen_experts = None
    if dense.ndim == 3:
        require(rows is None and columns is None,
                f"{name} is a stack of experts; select `experts` instead of rows or columns")
        chosen_experts = indices(experts, shape[0], f"{name} experts")
        mask[slice(None) if chosen_experts is None else chosen_experts] = True
    else:
        require(experts is None, f"{name} is not a stack of experts; `experts` applies only to switch (MoE) tensors")
        chosen_rows = indices(rows, shape[0], f"{name} rows")
        require(dense.ndim == 2 or columns is None, f"{name} is a vector and has no columns")
        chosen_columns = indices(columns, shape[-1], f"{name} columns") if dense.ndim == 2 else None
        if dense.ndim == 1:
            mask[slice(None) if chosen_rows is None else chosen_rows] = True
        else:
            mask[np.ix_(np.arange(shape[0]) if chosen_rows is None else chosen_rows,
                        np.arange(shape[1]) if chosen_columns is None else chosen_columns)] = True
    selected = mx.array(mask)
    edited = mx.where(selected, dense * factor, dense)
    touched = None
    if row["quantized"]:
        group = row["quantization"]["group_size"]
        require(dense.ndim >= 2 and shape[-1] % group == 0,
                f"{name}: packed layout does not split into quantization groups")
        touched = mask.reshape(*shape[:-1], shape[-1] // group, group).any(axis=-1)
    updates, stored, rewritten = store_edit(arrays, row, form, edited, touched, int(mask.sum()))
    zero = mx.zeros_like(dense)
    values = mx.stack([mx.sqrt(mx.sum(mx.where(selected, dense, zero) ** 2)),
                       mx.sqrt(mx.sum(mx.where(selected, stored, zero) ** 2)),
                       mx.sqrt(mx.sum((edited - dense) ** 2)), mx.sqrt(mx.sum((stored - edited) ** 2))])
    mx.eval(values, *updates.values())
    before, after, size, error = values.tolist()
    require(all(math.isfinite(value) for value in (before, after, size, error)), f"{name}: edit produced nonfinite values")
    result = {"tensor": name, "scale": factor}
    if dense.ndim == 3:
        result["experts"] = "all" if chosen_experts is None else len(chosen_experts)
    else:
        result["rows"] = "all" if chosen_rows is None else len(chosen_rows)
        result["columns"] = "all" if chosen_columns is None else len(chosen_columns)
    return dict(result, norm_before=before, norm_after=after, edit_norm=size,
                requantization_error=error / size if size > 0 else 0.0,
                quantized=row["quantized"], edited_params=int(mask.sum()), rewritten_params=rewritten), updates


def link_tree(root, stage, rewritten):
    """Hard-link (copy when linking fails) every base file except rewritten shards and edit records."""
    linked = copied = 0
    for path in sorted(root.rglob("*")):
        relative = path.relative_to(root)
        if str(relative) in rewritten or str(relative) == EDIT_RECORD or not path.is_file():
            continue
        target = stage / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        try:
            os.link(path, target, follow_symlinks=True)
            linked += 1
        except OSError:
            shutil.copy2(path, target)
            copied += 1
    return linked, copied


def planned_projection(request, root, rows, method="project_out"):
    """`project_out` or `add` with a measured direction, on every MLP writer of the chosen layers
    (a dense down projection, a shared expert's and a routed expert stack's). `control_seed` makes
    the norm-matched control instead: the same edit along a random direction orthogonal to the
    measured one, per layer."""
    import mlx.core as mx
    source = request.get("direction")
    require(isinstance(source, str) and Path(source).is_file(), "direction must be a saved direction file")
    loaded, metadata = mx.load(source, return_metadata=True)
    require(metadata.get("format") == DIRECTION_FORMAT and "directions" in loaded,
            "direction file is not a Dial weights direction")
    require(metadata.get("model_id") == steering.identity(root),
            "direction was measured on a different model; measure it again on this model")
    directions = loaded["directions"].astype(mx.float32)
    layers = request.get("layers")
    require(isinstance(layers, list) and 1 <= len(layers) <= MAX_EDIT_LAYERS
            and all(type(layer) is int and 0 <= layer < directions.shape[0] for layer in layers)
            and len(set(layers)) == len(layers),
            f"layers must be 1–{MAX_EDIT_LAYERS} distinct layers below {directions.shape[0]} (the measured layer count)")
    seed = request.get("control_seed")
    require(seed is None or (type(seed) is int and 0 <= seed < 2 ** 32), "control_seed must be an integer in [0, 2^32)")
    columns = None
    if method == "add":
        require(request.get("columns") is None, "columns applies only to project_out")
        require(metadata.get("site") == "mlp", "add needs a direction measured at site mlp")
        alpha = request.get("alpha", ADD_ALPHA)
        require(type(alpha) in (int, float) and math.isfinite(alpha) and 0 < alpha <= 2, "alpha must be in (0, 2]")
    else:
        columns = request.get("columns", 64)
        require(type(columns) is int and 1 <= columns <= MAX_EDIT_COLUMNS, f"columns must be 1–{MAX_EDIT_COLUMNS}")
        alpha = request.get("alpha", 1.0)
        require(type(alpha) in (int, float) and math.isfinite(alpha) and 0 < alpha <= 2, "alpha must be in (0, 2]")
    plan = {}
    for layer in sorted(layers):
        control = None if seed is None else control_direction(directions[layer], seed, layer)
        for writer in mlp_writers(rows, layer):
            row, kind = writer["row"], writer["kind"]
            if method == "add":
                key = f"{layer}.{kind}"
                require(f"inputs.{key}" in loaded and f"norms.{key}" in loaded,
                        f"the direction has no input statistics for layer {layer}'s {kind} MLP writer; "
                        "measure it again (add needs a direction measured by this version)")
                inputs, norms = loaded[f"inputs.{key}"], loaded[f"norms.{key}"]

                def apply(arrays, dose=1.0, row=row, layer=layer, kind=kind, inputs=inputs, norms=norms, control=control):
                    result, updates = add_along(arrays, row, directions[layer], inputs, norms, alpha * dose, control)
                    return dict(result, layer=layer, writer=kind), updates
            else:
                width = row["shape"][-1]
                require(columns <= width,
                        f"{row['name']} has only {width} columns; use `columns` of at most {width}")

                def apply(arrays, dose=1.0, row=row, layer=layer, kind=kind, control=control):
                    result, updates = project_out(arrays, row, directions[layer], columns, alpha * dose, control)
                    return dict(result, layer=layer, writer=kind), updates
            plan.setdefault(row["shard"], []).append((row, apply))
    record = {"alpha": alpha, "columns": columns, "control_seed": seed,
              "direction": {"site": metadata.get("site"), "model_id": metadata["model_id"],
                            "span": json.loads(metadata.get("span") or "null"),
                            "contrast": metadata.get("contrast", "groups")},
              # What the evaluation needs to make the matching control copy.
              "request": {"method": method, "direction": source, "layers": sorted(layers),
                          "columns": columns, "alpha": alpha}}
    return plan, record, {"alpha": alpha, "columns": columns, "site": metadata.get("site"), "control_seed": seed}


def check_selection(row, change):
    """Rows, columns or experts of a scale change must exist in the tensor; says what the tensor has."""
    name, shape = row["name"], row["shape"]
    require(len(shape) in (1, 2, 3),
            f"{name} has shape {shape}; only vectors, matrices and expert stacks can be scaled")
    if len(shape) == 3:
        require(change.get("rows") is None and change.get("columns") is None,
                f"{name} is a stack of experts; select `experts` instead of rows or columns")
        keys = (("experts", shape[0]),)
    else:
        require(change.get("experts") is None,
                f"{name} is not a stack of experts; `experts` applies only to switch (MoE) tensors")
        keys = (("rows", shape[0]), ("columns", shape[-1] if len(shape) == 2 else None))
    for key, limit in keys:
        selected = change.get(key)
        if selected is None:
            continue
        require(limit is not None, f"{name} is a vector and has no {key}")
        require(isinstance(selected, list) and 1 <= len(selected) <= MAX_EDIT_INDICES
                and all(type(n) is int and 0 <= n < limit for n in selected),
                f"{name}: {key} must be 1-{MAX_EDIT_INDICES} integers below {limit} (shape {shape})")


def attention_output(rows, layer):
    found = [row for row in rows if row["layer"] == layer and row["component"] == "attention"
             and row["name"].endswith("o_proj.weight")]
    require(len(found) == 1, f"layer {layer} has no single attention output projection (o_proj) to select heads in")
    return found[0]


def layer_tensor(rows, layer, suffix, what):
    """The one checkpoint tensor of `layer` whose name ends with `suffix`."""
    found = [row for row in rows if row["layer"] == layer and row["name"].endswith(suffix)]
    require(len(found) == 1, f"layer {layer} has no single {what} to scale")
    return found[0]


def resolve_selector(change, rows, config):
    """A `{layer, heads|neurons|experts|shared_expert}` change as the tensor and columns it means.

    Head h of an attention layer owns columns h*size..(h+1)*size of that layer's o_proj; MLP neuron n
    owns column n of its down_proj; expert e is slice e of the layer's switch_mlp down_proj stack; the
    shared expert is the layer's shared_expert down_proj. `attribute` reports the same identifiers.
    """
    layer = change.get("layer")
    shared = change.get("shared_expert")
    require(shared is None or shared is True, "shared_expert must be true when given")
    chosen = [key for key in ("heads", "neurons", "experts") if change.get(key) is not None]
    if shared:
        chosen.append("shared_expert")
    require(type(layer) is int and layer >= 0 and len(chosen) == 1,
            "a change without `tensor` needs an integer `layer` and exactly one of `heads`, `neurons`, "
            "`experts` or `shared_expert`")
    require(change.get("rows") is None and change.get("columns") is None,
            "`rows` and `columns` apply to a change with a `tensor`, not to heads, neurons or experts")
    kind, selected = chosen[0], None
    if kind == "heads":
        row = attention_output(rows, layer)
        count = architecture(config)["heads"]
        require(type(count) is int and count > 0, "config.json does not give the attention head count")
        width = row["shape"][-1]
        require(width % count == 0, f"{row['name']} width {width} does not split into {count} heads")
        require(isinstance(change["heads"], list) and 1 <= len(change["heads"]) <= count
                and all(type(h) is int and 0 <= h < count for h in change["heads"]),
                f"layer {layer} has heads 0-{count - 1}; `heads` must list some of them")
        size = width // count
        selected = [h * size + i for h in sorted(set(change["heads"])) for i in range(size)]
    elif kind == "neurons":
        row, selected = output_projection(rows, layer), change["neurons"]
    elif kind == "experts":
        row = layer_tensor(rows, layer, ".mlp.switch_mlp.down_proj.weight", "expert stack (switch_mlp down_proj)")
    else:
        row = layer_tensor(rows, layer, ".mlp.shared_expert.down_proj.weight", "shared expert down_proj")
    return dict(change, tensor=row["name"], columns=selected,
                layer=None, heads=None, neurons=None, shared_expert=None)


def planned_scaling(request, root, rows, config):
    """`scale`: exact tensors (or a layer's heads or neurons) multiplied by exact factors."""
    changes = request.get("changes")
    require(isinstance(changes, list) and 1 <= len(changes) <= MAX_EDIT_CHANGES,
            f"changes must list 1-{MAX_EDIT_CHANGES} tensors to scale")
    require(all(isinstance(change, dict) for change in changes), "every change must be an object")
    changes = [change if "tensor" in change else resolve_selector(change, rows, config) for change in changes]
    by_name = {row["name"]: row for row in rows}
    names = [change.get("tensor") if isinstance(change, dict) else None for change in changes]
    require(all(isinstance(name, str) for name in names) and len(set(names)) == len(names),
            "every change needs a distinct `tensor` name")
    plan = {}
    for change in changes:
        name = change["tensor"]
        require(name in by_name,
                f"{name} is not a tensor of this model; use the map or stats operation for exact names")
        row = by_name[name]
        check_selection(row, change)
        factor = change.get("scale")
        require(type(factor) in (int, float) and math.isfinite(factor) and -MAX_EDIT_SCALE <= factor <= MAX_EDIT_SCALE,
                f"{name}: scale must be a number in [-{MAX_EDIT_SCALE}, {MAX_EDIT_SCALE}]")

        def apply(arrays, dose=1.0, row=row, factor=float(factor),
                  selection=(change.get("rows"), change.get("columns"), change.get("experts"))):
            return scale_tensor(arrays, row, 1.0 + dose * (factor - 1.0), *selection)
        plan.setdefault(row["shard"], []).append((row, apply))
    return plan, {"changes": [{key: value for key, value in change.items() if value is not None}
                              for change in changes]}, {}


def add_delta(arrays, row, delta):
    """Add a dense (out x in) delta to one projection, written back in its stored form."""
    import mlx.core as mx
    import numpy as np
    name = row["name"]
    dense, form = dense_weight(arrays, row)
    require(dense.ndim == 2 and tuple(dense.shape) == tuple(delta.shape),
            f"{name} does not have the shape of the steering delta")
    changed = np.array(mx.abs(delta) > 0)
    edited = dense + delta
    touched = None
    if row["quantized"]:
        group = row["quantization"]["group_size"]
        require(dense.shape[1] % group == 0, f"{name}: packed layout does not split into quantization groups")
        touched = changed.reshape(dense.shape[0], dense.shape[1] // group, group).any(axis=2)
    updates, stored, rewritten = store_edit(arrays, row, form, edited, touched, int(changed.sum()))
    values = mx.stack([mx.sqrt(mx.sum(dense * dense)), mx.sqrt(mx.sum(stored * stored)),
                       mx.sqrt(mx.sum(delta * delta)), mx.sqrt(mx.sum((stored - edited) ** 2))])
    mx.eval(values, *updates.values())
    before, after, size, error = values.tolist()
    require(all(math.isfinite(value) for value in (before, after, size, error)), f"{name}: edit produced nonfinite values")
    return {"tensor": name, "norm_before": before, "norm_after": after, "edit_norm": size,
            "requantization_error": error / size if size > 0 else 0.0, "quantized": row["quantized"],
            "edited_params": int(changed.sum()), "rewritten_params": rewritten}, updates


def planned_steering(request, root, rows):
    """`steering`: the rank-1 delta that `steering.bake` solves for a bundle, folded into its placed projection.

    bake writes its adapter into a scratch directory; the delta (lora_b outer lora_a, with the bundle's
    strength already in lora_b) is read from it and the scratch directory is removed, so the edited copy
    carries the change in its own weights and no adapter is left behind.
    """
    import mlx.core as mx
    bundle, strength, data = request.get("bundle"), request.get("strength"), request.get("data")
    require(isinstance(bundle, str) and os.path.isabs(bundle) and Path(bundle).is_file(),
            "bundle must be an absolute path to a steering bundle")
    require(isinstance(data, str) and os.path.isabs(data), "data must be an absolute path to records")
    ridge = request.get("ridge", STEERING_RIDGE)
    scratch = Path(tempfile.mkdtemp(prefix=".steering-"))
    try:
        found = steering.bake({"model": str(root), "bundle": bundle, "strength": strength, "data": data,
                               "ridge": ridge, "output": str(scratch / "adapter")})
        loaded = mx.load(str(Path(found["output"]) / "adapters.safetensors"))
        mx.eval(*loaded.values())
    finally:
        shutil.rmtree(scratch, ignore_errors=True)
    modules = sorted(key[:-len(".lora_a")] for key in loaded if key.endswith(".lora_a"))
    require(len(modules) == 1 and modules[0] + ".lora_b" in loaded, "steering must place exactly one projection")
    module = modules[0]
    matches = [row for row in rows if row["name"] == module + ".weight"] \
        or [row for row in rows if row["name"].endswith("." + module + ".weight")]
    require(len(matches) == 1, f"placed module {module} is not a single projection tensor of this checkpoint")
    row = matches[0]
    delta = loaded[module + ".lora_b"].reshape(-1, 1).astype(mx.float32) * \
        loaded[module + ".lora_a"].reshape(1, -1).astype(mx.float32)
    require(tuple(delta.shape) == tuple(row["shape"]), f"{row['name']} does not match the steering delta's shape")
    info = {"layer": found["layer"], "module": row["name"], "strength": strength,
            "relative_error": found["relative_error"], "samples": found["samples"]}
    mx.eval(delta)

    def apply(arrays, dose=1.0, row=row, delta=delta):
        return add_delta(arrays, row, dose * delta)
    return {row["shard"]: [(row, apply)]}, {"steering": info}, {"steering": info}


def planned_edit(request, root, rows, config, method):
    require(method in ("project_out", "add", "scale", "steering"), "method must be project_out, add, scale or steering")
    if method in ("project_out", "add"):
        return planned_projection(request, root, rows, method)
    require(request.get("control_seed") is None, "control_seed applies only to project_out and add")
    if method == "steering":
        return planned_steering(request, root, rows)
    return planned_scaling(request, root, rows, config)


def edit(request, root, config):
    """Write an edited copy of the model at `output`; the base directory is never modified."""
    import mlx.core as mx
    method = request.get("method", "project_out")
    output = request.get("output")
    require(isinstance(output, str) and bool(output), "output must be a new model directory")
    out = Path(output)
    require(not out.exists() and out.parent.is_dir(), "output must be a new directory in an existing parent")
    require(root not in out.resolve().parents, "output must be outside the base model directory")
    rows = tensors(root, config)
    plan, details, summary = planned_edit(request, root, rows, config, method)
    allow_noisy = request.get("allow_noisy", False)
    require(type(allow_noisy) is bool, "allow_noisy must be true or false")
    count = sum(len(items) for items in plan.values())
    stage = Path(tempfile.mkdtemp(prefix=f".{out.name}.partial-", dir=out.parent))
    try:
        edits = []
        for shard, items in plan.items():
            arrays, shard_metadata = mx.load(str(root / shard), return_metadata=True)
            for row, apply in items:
                report(request, "editing tensors", len(edits), count)
                result, updates = apply(arrays)
                if not allow_noisy and result["requantization_error"] > MAX_REQUANTIZATION:
                    raise WeightsError(
                        f"{result['tensor']}: requantizing to {row['quantization']['bits']} bits changes the weights "
                        f"{result['requantization_error']:.2f}x as much as the edit itself, so the edit would be mostly "
                        "rounding noise. Edit an 8-bit or full-precision copy of the model, make a larger edit, "
                        "or pass allow_noisy to accept it")
                arrays.update(updates)
                edits.append(result)
            mx.save_safetensors(str(stage / shard), arrays, metadata=shard_metadata)
            del arrays
            mx.clear_cache()
        linked, copied = link_tree(root, stage, set(plan))
        total = sum(row["params"] for row in rows)
        record = {"version": VERSION, "method": method, "base": str(root), "base_id": steering.identity(root),
                  **details, "edits": rounded(edits)}
        if method in ("project_out", "add"):
            record["layers"] = record.pop("edits")
        steering.atomic_json(stage / EDIT_RECORD, record)
        os.rename(stage, out)
    except BaseException:
        shutil.rmtree(stage, ignore_errors=True)
        raise
    report(request, "edited", count, count)
    rewritten = sum(row["rewritten_params"] for row in edits)
    shown = [dict(row, columns=row["columns"][:16]) if isinstance(row.get("columns"), list) else row for row in edits]
    # Column indices stay in the edit record; results carry a preview.
    return {"method": method, "output": str(out), "output_id": steering.identity(out), **summary,
            "layers" if method in ("project_out", "add") else "changes": shown,
            "edited_params": sum(row["edited_params"] for row in edits), "rewritten_params": rewritten,
            "total_params": total, "fraction_changed": rewritten / total if total else 0.0,
            "quantized": any(by["quantized"] for items in plan.values() for by, _ in items),
            "shards_rewritten": len(plan), "files_linked": linked, "files_copied": copied}


def neutral_logprobs(model, encoded):
    """Log-probabilities at the first NEUTRAL_TOKENS answer positions of each case."""
    import mlx.core as mx
    out = []
    for tokens, offset in encoded:
        logits = model(mx.array([tokens[:-1]], dtype=mx.int32))[0, offset - 1:].astype(mx.float32)
        logp = logits - mx.logsumexp(logits, axis=-1, keepdims=True)
        mx.eval(logp)
        out.append(logp)
    return out


def divergence(base, edited):
    """Mean KL(base || edited) per answer position over all neutral cases."""
    import mlx.core as mx
    total, positions = 0.0, 0
    for before, after in zip(base, edited):
        kl = mx.sum(mx.exp(before) * (before - after), axis=-1)
        mx.eval(kl)
        total += float(mx.sum(kl).item())
        positions += before.shape[0]
    require(math.isfinite(total), "divergence is not finite")
    return total / positions


@contextlib.contextmanager
def patched(model, updates):
    """Put checkpoint tensors into the loaded model, restoring the originals on exit."""
    from mlx.utils import tree_flatten
    current = dict(tree_flatten(model.parameters()))
    missing = sorted(name for name in updates if name not in current)
    if missing:
        raise WeightsError(f"{missing[0]} is not a parameter of the loaded model; "
                           "this architecture cannot be tried in memory, use edit and evaluate instead")
    originals = [(name, current[name]) for name in updates]
    model.load_weights(list(updates.items()), strict=False)
    try:
        yield
    finally:
        model.load_weights(originals, strict=False)


class Gate:
    """Per-component multipliers (all 1) that a differentiated loss reads through `value`."""
    value = None


def scale_outputs(owner, key, restore, scale):
    """Multiply the output of `owner.key` by `scale(output, *inputs)` during forward passes."""
    from mlx import nn
    inner = getattr(owner, key)

    class Scaled(nn.Module):
        def __init__(self):
            super().__init__()
            self["inner"] = inner

        def __call__(self, *args):
            return scale(self.inner(*args), *args)
    setattr(owner, key, Scaled())
    restore.append((owner, key, inner))


def gated(model, index, gates, heads):
    """Route layer `index`'s attention-head outputs, MLP neurons, routed experts and shared expert through multipliers.

    Returns what to restore. The gradient of the loss with respect to a multiplier at 1 is the
    first-order change in loss per unit of that component kept, so its negative estimates what
    removing the component would do. A routed expert's multiplier scales its output before the router
    weight, so each expert is ranked on the tokens that actually select it.
    """
    from mlx import nn
    import mlx.core as mx
    layer = steering.layer_list(model)[index]
    restore = []

    def wrap(owner, key, make):
        inner = getattr(owner, key)

        class Gated(nn.Module):
            def __init__(self):
                super().__init__()
                self["inner"] = inner

            def __call__(self, x):
                return self.inner(make(x))
        setattr(owner, key, Gated())
        restore.append((owner, key, inner))
    attention = getattr(layer, "self_attn", None) or getattr(layer, "attention", None)
    if attention is not None and hasattr(attention, "o_proj") and heads[index]:
        count = heads[index]

        def split(x):
            width = x.shape[-1]
            require(width % count == 0, f"layer {index} attention width does not split into {count} heads")
            return x * mx.repeat(gates[("head", index)].value, width // count).astype(x.dtype)
        wrap(attention, "o_proj", split)
    mlp = getattr(layer, "mlp", None)
    if mlp is not None and hasattr(mlp, "down_proj"):
        wrap(mlp, "down_proj", lambda x: x * gates[("neuron", index)].value.astype(x.dtype))
    if mlp is not None and hasattr(mlp, "switch_mlp") and ("expert", index) in gates:
        scale_outputs(mlp, "switch_mlp", restore,
                      lambda output, x, indices: output * mx.take(gates[("expert", index)].value, indices,
                                                                  axis=0)[..., None].astype(output.dtype))
    if mlp is not None and hasattr(mlp, "shared_expert") and ("shared", index) in gates:
        scale_outputs(mlp, "shared_expert", restore,
                      lambda output, x: output * gates[("shared", index)].value.astype(output.dtype))
    return restore


REPEAT_NGRAM = 3
EXPLAIN_TOKENS = 5


def repeat_positions(tokens, offset):
    """Answer positions whose last REPEAT_NGRAM tokens already occurred earlier in the same answer."""
    seen, found = set(), []
    for index in range(offset + REPEAT_NGRAM - 1, len(tokens)):
        gram = tuple(tokens[index - REPEAT_NGRAM + 1:index + 1])
        if gram in seen:
            found.append(index)
        seen.add(gram)
    return found


def unembedding(model):
    """The function mapping a final hidden vector to vocabulary logits."""
    head = getattr(model, "lm_head", None)
    if head is not None:
        return head
    embed = getattr(getattr(model, "model", None), "embed_tokens", None)
    require(embed is not None and hasattr(embed, "as_linear"), "the model has no output embedding to read neurons through")
    return embed.as_linear


def neuron_effects(model, tokenizer, layers, found):
    """Tokens each listed neuron promotes and suppresses when active, read from its down_proj column.

    The column is pushed through the final norm and the output embedding (a logit lens): it ignores
    the layers between this one and the output, so it says what the neuron writes, not what it does.
    """
    import mlx.core as mx
    import numpy as np
    project = unembedding(model)
    norm = getattr(getattr(model, "model", None), "norm", None)
    for row in found:
        down = layers[row["layer"]].mlp.down_proj
        size = neuron_count(layers[row["layer"]])
        dtype = getattr(down, "scales", down.weight).dtype
        one_hot = (mx.arange(size) == row["neuron"]).astype(dtype)[None, None, :]
        vector = down(one_hot)
        logits = project(norm(vector) if norm is not None else vector)[0, 0].astype(mx.float32)
        mx.eval(logits)
        scores = np.array(logits)
        row["promotes"] = [tokenizer.decode([int(i)]) for i in np.argsort(-scores)[:EXPLAIN_TOKENS]]
        row["suppresses"] = [tokenizer.decode([int(i)]) for i in np.argsort(scores)[:EXPLAIN_TOKENS]]


def attention_pattern(attention, heads, x):
    """Softmax attention weights, (heads, length, length), recomputed from the layer's input.

    Qwen3.5 full-attention layers carry a per-head output gate in q_proj (width 2*heads*head_dim:
    queries then gate, split per head) and rotate with `rotary_emb.apply_rotary` over mrope
    position ids; text-only positions are the plain 0..L-1 sequence on all three axes.
    """
    import mlx.core as mx
    batch, length, _ = x.shape
    kv_heads = next((getattr(attention, key) for key in ("n_kv_heads", "num_key_value_heads")
                     if type(getattr(attention, key, None)) is int), heads)
    rotary = hasattr(attention, "rope") or hasattr(getattr(attention, "rotary_emb", None), "apply_rotary")
    require(all(hasattr(attention, key) for key in ("q_proj", "k_proj", "scale")) and rotary,
            "this attention layer does not expose q_proj, k_proj, a rotary embedding and scale")
    raw_queries, raw_keys = attention.q_proj(x), attention.k_proj(x)
    key_width = raw_keys.shape[-1] // kv_heads
    if raw_queries.shape[-1] == 2 * heads * key_width:
        queries = mx.split(raw_queries.reshape(batch, length, heads, -1), 2, axis=-1)[0]
    else:
        queries = raw_queries.reshape(batch, length, heads, -1)
    keys = raw_keys.reshape(batch, length, kv_heads, -1)
    if getattr(attention, "q_norm", None) is not None:
        queries = attention.q_norm(queries)
    if getattr(attention, "k_norm", None) is not None:
        keys = attention.k_norm(keys)
    queries, keys = queries.transpose(0, 2, 1, 3), keys.transpose(0, 2, 1, 3)
    if hasattr(attention, "rope"):
        queries, keys = attention.rope(queries), attention.rope(keys)
    else:
        positions = mx.broadcast_to(mx.arange(length)[None, None, :], (3, batch, length))
        queries, keys = attention.rotary_emb.apply_rotary(queries, keys, positions, unsqueeze_dim=1)
    if kv_heads != heads:
        keys = mx.repeat(keys, heads // kv_heads, axis=1)
    scores = (queries * attention.scale) @ keys.transpose(0, 1, 3, 2)
    scores = scores.astype(mx.float32) + mx.triu(mx.full((length, length), -1e9, dtype=mx.float32), k=1)
    return mx.softmax(scores, axis=-1)[0]


@contextlib.contextmanager
def layer_inputs(model, indices, locate):
    """Record the first input of one module per listed layer during forward passes; yields {layer: input}.

    `locate(layer)` returns (owner, key) naming the module to wrap, or None to skip that layer.
    """
    from mlx import nn
    layers = steering.layer_list(model)
    seen = {}

    class Recorder(nn.Module):
        def __init__(self, inner, layer):
            super().__init__()
            self["inner"] = inner
            self.layer = layer

        def __getattr__(self, name):
            # The forward pass may read attributes of the module it expects here (mlx-vlm's
            # Qwen3.5 reads `self_attn.rotary_emb`), so forward them to the wrapped module.
            try:
                return super().__getattr__(name)
            except AttributeError:
                if "inner" not in self:
                    raise
                return getattr(self["inner"], name)

        def __call__(self, *args, **kwargs):
            seen[self.layer] = args[0] if args else kwargs["x"]
            return self.inner(*args, **kwargs)
    restore = []
    try:
        for index in indices:
            found = locate(layers[index])
            if found is None:
                continue
            owner, key = found
            restore.append((owner, key, getattr(owner, key)))
            setattr(owner, key, Recorder(getattr(owner, key), index))
        yield seen
    finally:
        for owner, key, inner in restore:
            setattr(owner, key, inner)


def attention_inputs(model, indices):
    """Record the input of each listed layer's attention during forward passes."""
    def locate(layer):
        return layer, "self_attn" if getattr(layer, "self_attn", None) is not None else "attention"
    return layer_inputs(model, indices, locate)


def copy_match(inputs, queries):
    """Rows for `queries`: 1 at each key that follows an earlier copy of that query's token."""
    import mlx.core as mx
    import numpy as np
    previous = np.concatenate([[-1], inputs[:-1]])
    match = (previous[None, :] == inputs[:, None]) & np.tril(np.ones((len(inputs), len(inputs)), dtype=bool))
    return mx.array(match[queries].astype(np.float32))


def induction_scores(model, encoded, wanted, focus):
    """Mean attention each listed head pays to the token after an earlier copy of the current token.

    This is the usual induction/copy signature and the most likely way a head sustains a repeat.
    `wanted` maps layer -> head indices; the result maps (layer, head) -> score in [0, 1].
    """
    import mlx.core as mx
    import numpy as np
    modules = {index: attention_heads(model, index) for index in wanted}
    totals = {(index, head): [] for index, group in wanted.items() for head in group}
    with attention_inputs(model, list(wanted)) as seen:
        for tokens, offset in encoded:
            inputs = np.array(tokens[:-1])
            targets = repeat_positions(tokens, offset) if focus == "repeats" else range(offset, len(tokens))
            queries = [position - 1 for position in targets]
            if not queries:
                continue
            model(mx.array([tokens[:-1]], dtype=mx.int32))
            match = copy_match(inputs, queries)
            for index, group in wanted.items():
                pattern = attention_pattern(*modules[index], seen[index])
                mass = mx.mean(mx.sum(pattern[:, queries, :] * match[None], axis=-1), axis=-1)
                mx.eval(mass)
                for head in group:
                    totals[(index, head)].append(float(mass[head].item()))
            seen.clear()
            mx.clear_cache()
    return {key: float(np.mean(values)) for key, values in totals.items() if values}


def attribute(request, root, config):
    """Which attention heads and MLP neurons make the given answers likely, from backward passes.

    `support` is the estimated rise in answer loss if the component were removed: large positive
    means the component pushes toward these answers. `focus: repeats` scores only the positions that
    repeat an earlier 3-token sequence, so a loop is not diluted by the ordinary text around it.
    `wanted` answers to the same kind of prompt and `neutral` ordinary answers are subtracted, which
    favours components that drive the unwanted answers without being needed elsewhere. `agreement`
    is the share of cases where the component supports the answer. Top neurons list the tokens they
    promote and top heads their induction attention. An estimate to rank candidates; `try` checks it.
    """
    import mlx.core as mx
    import numpy as np
    from dial_worker import encode, reasoning_renderer
    cases = trace_cases(request)
    wanted = trace_cases(request, "wanted") if request.get("wanted") is not None else []
    neutral = trace_cases(request, "neutral", MAX_NEUTRAL_CASES) if request.get("neutral") is not None else []
    top = request.get("top", 20)
    require(type(top) is int and 1 <= top <= MAX_ATTRIBUTION_ROWS, f"top must be 1-{MAX_ATTRIBUTION_ROWS}")
    focus = request.get("focus", "answer")
    require(focus in ("answer", "repeats"), "focus must be answer or repeats")
    explain = request.get("explain", True)
    require(type(explain) is bool, "explain must be true or false")
    model, tokenizer = steering.load(request)
    render = reasoning_renderer(tokenizer)
    layers = steering.layer_list(model)
    heads = []
    for index in range(len(layers)):
        try:
            heads.append(attention_heads(model, index)[1])
        except WeightsError:
            heads.append(0)

    def tokens_of(items, key, cap=None):
        encoded = []
        for number, case in enumerate(items):
            tokens, offset = encode(tokenizer, render(case["messages"]), case.get("tools"))
            require(0 < offset < len(tokens) <= MAX_TOKENS,
                    f"{key} {number} has no answer tokens or exceeds {MAX_TOKENS} tokens")
            encoded.append((tokens[:offset + cap] if cap else tokens, offset))
        return encoded
    groups = {"cases": tokens_of(cases, "cases"), "wanted": tokens_of(wanted, "wanted"),
              "neutral": tokens_of(neutral, "neutral", NEUTRAL_TOKENS)}
    gates, sizes = {}, {}
    for index, layer in enumerate(layers):
        if heads[index]:
            gates[("head", index)], sizes[("head", index)] = Gate(), heads[index]
        if getattr(getattr(layer, "mlp", None), "down_proj", None) is not None:
            gates[("neuron", index)], sizes[("neuron", index)] = Gate(), neuron_count(layer)
        if hasattr(getattr(layer, "mlp", None), "switch_mlp"):
            # Routed experts get one multiplier each; the shared expert gets one per layer.
            gates[("expert", index)], sizes[("expert", index)] = Gate(), int(layer.mlp.gate.weight.shape[0])
            gates[("shared", index)], sizes[("shared", index)] = Gate(), 1
    require(gates, "this architecture has no attention heads, dense MLP or mixture-of-experts layers to attribute")
    restore = []
    for index in range(len(layers)):
        restore += gated(model, index, gates, heads)
    keys = list(gates)

    def scored(name, item):
        """Target positions (indices into the shifted log-probabilities) a case is scored on."""
        tokens, offset = item
        if name == "cases" and focus == "repeats":
            return [position - 1 for position in repeat_positions(tokens, offset)]
        return list(range(offset - 1, len(tokens) - 1))

    def mean_loss(values, item, positions):
        for key, value in zip(keys, values):
            gates[key].value = value
        tokens = item[0]
        logits = model(mx.array([tokens[:-1]], dtype=mx.int32)).astype(mx.float32)
        logp = mx.take_along_axis(logits - mx.logsumexp(logits, axis=-1, keepdims=True),
                                  mx.array([tokens[1:]], dtype=mx.int32)[..., None], axis=-1).squeeze(-1)
        return -mx.mean(logp[0][mx.array(positions, dtype=mx.int32)])
    ones = [mx.ones((sizes[key],), dtype=mx.float32) for key in keys]
    done, total = 0, sum(len(items) for items in groups.values())
    support, agreement = {}, {}
    try:
        for name, encoded in groups.items():
            per_case, counts = [], []
            for item in encoded:
                report(request, f"attributing {name}", done, total)
                done += 1
                positions = scored(name, item)
                if not positions:
                    continue
                grads = mx.grad(lambda values: mean_loss(values, item, positions))(ones)
                mx.eval(grads)
                per_case.append([np.array(-g) for g in grads])
                counts.append(len(positions))
                mx.clear_cache()
            if encoded:
                require(per_case, f"no {name} case has a repeated {REPEAT_NGRAM}-token sequence to score; "
                        "use focus answer" if name == "cases" else f"no {name} case has answer tokens")
                weights = np.array(counts, dtype=np.float64) / sum(counts)
                support[name] = [sum(w * case[k] for w, case in zip(weights, per_case)) for k in range(len(keys))]
                agreement[name] = [np.mean([case[k] > 0 for case in per_case], axis=0) for k in range(len(keys))]
    finally:
        for owner, key, inner in restore:
            setattr(owner, key, inner)
    report(request, "attributed", total, total)
    chosen = support["cases"]
    if wanted:
        chosen = [a - b for a, b in zip(chosen, support["wanted"])]
    if neutral:
        chosen = [a - np.maximum(b, 0.0) for a, b in zip(chosen, support["neutral"])]
    label = "specific" if neutral else "selective" if wanted else "support"

    def rows_of(kind, field):
        found = []
        for number, key in enumerate(keys):
            if key[0] != kind:
                continue
            for item in np.argsort(-chosen[number])[:top].tolist():
                row = {"layer": key[1], field: item, label: float(chosen[number][item]),
                       "support": float(support["cases"][number][item]),
                       "agreement": float(agreement["cases"][number][item])}
                for extra in ("wanted", "neutral"):
                    if extra in support:
                        row[extra] = float(support[extra][number][item])
                found.append(row)
        found.sort(key=lambda row: -row[label])
        return found[:top]
    found_heads, found_neurons = rows_of("head", "head"), rows_of("neuron", "neuron")
    found_experts = rows_of("expert", "expert")
    notes = []
    if explain:
        try:
            neuron_effects(model, tokenizer, layers, found_neurons)
        except WeightsError as error:
            notes.append(f"neuron tokens unavailable: {error}")
        try:
            wanted_heads = {}
            for row in found_heads:
                wanted_heads.setdefault(row["layer"], []).append(row["head"])
            scores = induction_scores(model, groups["cases"], wanted_heads, focus)
            for row in found_heads:
                if (row["layer"], row["head"]) in scores:
                    row["induction"] = scores[(row["layer"], row["head"])]
        except WeightsError as error:
            notes.append(f"head induction attention unavailable: {error}")
    components = {"head": "attention", "neuron": "mlp", "expert": "moe", "shared": "shared_expert"}
    totals = [{"layer": key[1], "component": components[key[0]],
               label: float(np.sum(scores))} for key, scores in zip(keys, chosen)]
    return {"cases": len(cases), "wanted": len(wanted), "neutral": len(neutral), "focus": focus,
            "ranked_by": label, "heads": found_heads, "neurons": found_neurons, "experts": found_experts,
            "layers": totals, **({"notes": notes} if notes else {})}


PROBES = (
    "The capital of France is Paris, and the capital of Italy is Rome. Water boils at one hundred "
    "degrees Celsius at sea level, and it freezes at zero.",
    "def add(a, b):\n    return a + b\n\nprint(add(2, 3))\n\n# The function adds two numbers and "
    "prints the result.",
    "She opened the old wooden door and stepped into the quiet room. Dust floated in the light "
    "from the window, and somewhere a clock was ticking.",
    "Q: What is the largest planet in the solar system?\nA: Jupiter is the largest planet in the "
    "solar system.\nQ: Which gas do plants absorb?\nA: Plants absorb carbon dioxide.",
    "The blue key opens the red door in the old tower. The blue key opens the red door in the old "
    "tower. The blue key opens the red door in the old tower. The blue key opens the red door.",
    "alpha seven kilo, delta nine mike, tango two whiskey. alpha seven kilo, delta nine mike, "
    "tango two whiskey. alpha seven kilo, delta nine mike, tango two whiskey.",
)
ANALYZE_NEURONS = 8
INDUCTION_ROLE, PREVIOUS_ROLE, SINK_ROLE = 0.3, 0.5, 0.8
SELF_ROLE, DUPLICATE_ROLE, LOCAL_ROLE, LONG_ROLE = 0.5, 0.3, 0.7, 0.5
LOCAL_WINDOW = 4
HEAD_ROLES = ("induction", "previous-token", "attention-sink", "current-token", "same-token", "local",
              "long-range", "mixed")
# Labelled text for `analyze`: each category is scored separately so a layer, expert or neuron can be
# tied to the kind of text it serves. `evaluate` keeps the short PROBES above for its quality check.
CATEGORY_PROBES = (
    ("facts", (
        PROBES[0],
        "Mount Everest is the highest mountain on Earth. The Amazon is the largest rainforest, and the "
        "Pacific is the largest ocean. The human heart has four chambers.",
        "The Second World War ended in 1945. The first person to walk on the Moon was Neil Armstrong, "
        "in 1969. Shakespeare wrote Hamlet and Romeo and Juliet.",
    )),
    ("code", (
        PROBES[1],
        "for (let i = 0; i < items.length; i++) {\n  if (items[i].done) {\n    count += 1;\n  }\n}\n"
        "console.log(`finished ${count} items`);",
        "SELECT name, COUNT(*) AS orders\nFROM customers JOIN orders ON customers.id = orders.customer_id\n"
        "GROUP BY name\nORDER BY orders DESC\nLIMIT 10;",
    )),
    ("maths", (
        "12 + 30 = 42. 7 * 8 = 56. 100 - 37 = 63. 144 / 12 = 12. The square root of 81 is 9, and 2 to "
        "the power of 10 is 1024.",
        "Solve for x: 3x + 5 = 20. Subtract 5 from both sides to get 3x = 15, then divide by 3, so x = 5.",
        "The area of a circle is pi times the radius squared. A circle with radius 3 has an area of about "
        "28.27 square units.",
    )),
    ("story", (
        PROBES[2],
        "The fox waited at the edge of the forest until the farmer had gone to bed. Then it crept across "
        "the frozen field towards the henhouse.",
        "When the letter finally arrived, Maria read it twice before she sat down. Her brother was coming "
        "home after ten years at sea.",
    )),
    ("other languages", (
        "Le chat dort sur le canapé pendant que les enfants jouent dans le jardin. Il fait beau "
        "aujourd'hui à Paris.",
        "Der Zug nach Berlin fährt um acht Uhr ab. Wir müssen uns beeilen, sonst verpassen wir ihn.",
        "El mercado abre temprano los sábados y la gente compra fruta fresca, pan y flores.",
        "東京は日本の首都です。毎年たくさんの観光客が訪れます。",
    )),
    ("instructions", (
        PROBES[3],
        "User: Write a short email asking my manager for Friday off.\nAssistant: Subject: Request for "
        "Friday off\n\nHi Sam,\n\nCould I take this Friday off? My work is up to date.\n\nThanks,\nAlex",
        "User: List three tips for better sleep.\nAssistant: 1. Go to bed at the same time every night. "
        "2. Avoid screens for an hour before bed. 3. Keep your bedroom cool and dark.",
    )),
    ("reasoning", (
        "If all birds have feathers and a penguin is a bird, then a penguin has feathers. If it rains, "
        "the ground gets wet. The ground is dry, so it did not rain.",
        "Tom is taller than Ann, and Ann is taller than Ben. So Tom is taller than Ben, and Ben is the "
        "shortest of the three.",
        "A train leaves at 3 pm and the trip takes two and a half hours, so it arrives at 5:30 pm. If it "
        "is twenty minutes late, it arrives at 5:50 pm.",
    )),
    ("repetition", (PROBES[4], PROBES[5])),
)
# More labelled text for the observation passes (logit lens and layer write-out, heads, neurons,
# routing, expert profiles, linear-attention memory, self-explanation). Layer skipping, which runs
# the model once per layer, keeps the shorter CATEGORY_PROBES.
OBSERVATION_PROBES = (
    ("facts", (
        "The Nile flows north through eleven countries before it reaches the Mediterranean Sea. For "
        "thousands of years its yearly flood brought rich silt to the farms of Egypt, and the Aswan High "
        "Dam, finished in 1970, now controls that flood.",
        "Water boils at 100 degrees Celsius at sea level and freezes at 0 degrees. Light from the Sun "
        "takes about eight minutes to reach the Earth, and the Moon is roughly 384,000 kilometres away.",
        "Marie Curie was the first person to win two Nobel Prizes, one in physics in 1903 and one in "
        "chemistry in 1911. She discovered the elements polonium and radium with her husband Pierre.",
        "The Great Wall of China was built over many centuries to protect northern borders. Tokyo, "
        "Delhi and Shanghai are among the largest cities in the world, and Canberra is the capital of "
        "Australia.",
        "DNA carries genetic information in a sequence of four bases: adenine, thymine, guanine and "
        "cytosine. Proteins are built from twenty amino acids, and enzymes speed up chemical reactions "
        "in the cell.",
    )),
    ("code", (
        "fn parse_port(value: &str) -> Result<u16, String> {\n    let port: u16 = value\n        .trim()\n"
        "        .parse()\n        .map_err(|error| format!(\"invalid port {value}: {error}\"))?;\n"
        "    if port == 0 {\n        return Err(\"port must not be zero\".into());\n    }\n    Ok(port)\n}",
        "class Stack:\n    def __init__(self):\n        self.items = []\n\n    def push(self, item):\n"
        "        self.items.append(item)\n\n    def pop(self):\n        if not self.items:\n"
        "            raise IndexError(\"pop from empty stack\")\n        return self.items.pop()",
        "#!/bin/sh\nset -eu\nfor file in logs/*.txt; do\n  lines=$(wc -l < \"$file\")\n"
        "  echo \"$file has $lines lines\"\ndone\ngrep -c ERROR logs/app.log || true",
        "<ul class=\"menu\">\n  <li><a href=\"/\">Home</a></li>\n  <li><a href=\"/about\">About</a></li>\n"
        "</ul>\n<style>\n  .menu { display: flex; gap: 1rem; list-style: none; }\n</style>",
        "async function loadUser(id) {\n  const response = await fetch(`/api/users/${id}`);\n"
        "  if (!response.ok) {\n    throw new Error(`request failed: ${response.status}`);\n  }\n"
        "  return response.json();\n}",
    )),
    ("maths", (
        "Let f(x) = x^2 - 4x + 3. Setting f(x) = 0 and factoring gives (x - 1)(x - 3) = 0, so the roots "
        "are x = 1 and x = 3. The vertex is at x = 2, where f(2) = -1.",
        "A shop sells pencils at 3 for 45 cents. Each pencil costs 15 cents, so 12 pencils cost 1 dollar "
        "and 80 cents. With a 10 percent discount the price falls to 1 dollar and 62 cents.",
        "The sum of the angles of a triangle is 180 degrees. In a right triangle with legs 6 and 8, the "
        "hypotenuse is the square root of 36 + 64, which is 10.",
        "Proof that the square root of 2 is irrational: suppose it equals p/q in lowest terms. Then "
        "p^2 = 2q^2, so p is even. Write p = 2k; then q^2 = 2k^2, so q is even too, a contradiction.",
        "The derivative of sin(x) is cos(x), and the integral of 1/x is ln|x| + C. The probability of "
        "rolling two sixes with two fair dice is 1/36.",
    )),
    ("story", (
        "The lighthouse keeper climbed the spiral stairs every evening at dusk. One night the lamp would "
        "not light, and far out in the dark he heard a ship's horn calling, closer each time.",
        "Grandmother kept a tin of buttons in the kitchen drawer. On rainy afternoons we tipped them onto "
        "the table and she told us which coat or dress each one had come from.",
        "The dragon had not left its cave for a hundred years, so when the knight arrived with a basket "
        "of bread instead of a sword, it did not know whether to roar or to say thank you.",
        "Leo missed the last bus and walked home through the empty town. Every window was dark except "
        "one, where an old man sat at a piano, playing the same slow tune again and again.",
        "After the storm the village children ran down to the beach. The sand was covered in shells, "
        "broken oars and, half buried near the rocks, a wooden chest with a rusted lock.",
    )),
    ("other languages", (
        "Je voudrais réserver une table pour deux personnes ce soir à vingt heures, s'il vous plaît. "
        "Nous préférons une table près de la fenêtre.",
        "Gestern haben wir einen langen Spaziergang am Fluss gemacht. Danach haben wir in einem kleinen "
        "Café Kuchen gegessen und Kaffee getrunken.",
        "Mi hermana estudia medicina en Madrid y quiere trabajar en un hospital infantil cuando termine "
        "la carrera.",
        "Москва — столица России. Зимой здесь очень холодно, а летом бывает жарко.",
        "我每天早上七点起床，然后吃早饭，坐地铁去上班。周末我喜欢和朋友一起去公园。",
        "Il treno per Roma parte dal binario tre. Ricordati di comprare il biglietto prima di salire.",
        "Eu gosto de ler livros à noite antes de dormir. O meu autor favorito é brasileiro.",
    )),
    ("instructions", (
        "User: How do I make a cup of tea?\nAssistant: Boil fresh water, pour it over a tea bag in a mug "
        "and let it steep for three to five minutes. Remove the bag, then add milk or sugar if you like.",
        "User: Translate 'good morning' into French.\nAssistant: 'Good morning' in French is 'bonjour'.",
        "User: Summarise this in one sentence: The meeting was moved from Monday to Wednesday because "
        "two speakers were ill.\nAssistant: The meeting moved to Wednesday because two speakers were ill.",
        "To reset your password, open Settings, choose Account, then select Reset password. Enter the "
        "code sent to your email and choose a new password of at least twelve characters.",
        "User: Give me a name for a coffee shop.\nAssistant: How about 'The Daily Grind'? It is short, "
        "friendly and easy to remember.",
    )),
    ("reasoning", (
        "There are three boxes: one has apples, one has oranges and one has both, and every label is "
        "wrong. Take a fruit from the box labelled 'both'. If it is an apple, that box holds only apples.",
        "Every student in the class passed the test. Sara is in the class, so Sara passed. Ali did not "
        "pass, so Ali is not in the class.",
        "If the shop opens at 9 and the queue takes forty minutes, someone who joins at 8:50 is served at "
        "about 9:30. Arriving at 9:10 means being served at about 9:50.",
        "The keys are either in the car or in the kitchen. They are not in the car, because I checked. "
        "Therefore they must be in the kitchen.",
        "Doubling a number and adding six gives twenty. Working backwards, twenty minus six is fourteen, "
        "and half of fourteen is seven, so the number was seven.",
    )),
    ("repetition", (
        "Monday, Tuesday, Wednesday, Thursday, Friday, Saturday, Sunday. Monday, Tuesday, Wednesday, "
        "Thursday, Friday, Saturday, Sunday. Monday, Tuesday, Wednesday,",
        "red apple, green pear, yellow lemon, purple plum; red apple, green pear, yellow lemon, purple "
        "plum; red apple, green pear, yellow lemon,",
        "id,name,city\n1,Ana,Lisbon\n2,Ben,Leeds\n3,Cho,Seoul\nid,name,city\n1,Ana,Lisbon\n2,Ben,Leeds\n"
        "3,Cho,Seoul",
        "The quick brown fox jumps over the lazy dog. The quick brown fox jumps over the lazy dog. The "
        "quick brown fox jumps over the lazy dog.",
        "zq7 mlx 4kd vbo; zq7 mlx 4kd vbo; zq7 mlx 4kd vbo; zq7 mlx 4kd",
    )),
)
SPECIALTY_MIN, SPECIALTY_RATIO = 0.05, 2.0
LENS_FORMED, LENS_TOP = 0.5, 10
EXPERT_LIFT, EXPERT_MIN_SLOTS = 2.0, 8
NEURON_CONTEXTS, CONTEXT_BEFORE, CONTEXT_AFTER = 3, 8, 3
OUTLIER, OUTLIER_MATRICES = 5.0, 3
LENS_POSITIONS, WRITE_TOP, WRITE_LIST = 32, 5, 8
PROFILE_EXPERTS, PROFILE_PEAKS, PROFILE_TOKENS = 6, 6, 6
MEMORY_LONG, MEMORY_SHORT = 64.0, 4.0
# Self-explanation: the model describes this many experts and neurons in at most EXPLAIN_WORDS
# words; each description is scored on EXPLAIN_TEST held-out firing and EXPLAIN_TEST quiet snippets.
EXPLAIN_EXPERTS, EXPLAIN_NEURONS, EXPLAIN_TEST, EXPLAIN_WORDS, EXPLAIN_MAX_TOKENS = 8, 6, 5, 12, 48
EXPLAIN_CONTROL = "a token in ordinary text"
# What an outside explainer is asked when it rates held-out snippets for a description (Rust fills
# {description}); `explanation_request` fills the snippets and their count.
EXPLAIN_RATE = ("A part of a language model is described as: \"{description}\".\nEach snippet below has one token "
                "marked «like this». For each snippet, rate from 0 to 10 how strongly you expect this part to "
                "activate on the marked token, judging only by the description.\n{snippets}\nReply with only a JSON "
                "array of {count} numbers, in snippet order.")
EXPLAIN_TRUSTED = 0.25
EXPLAIN_HARD = 0.6
# Causal checks. A pool of up to CAUSAL_POOL candidates is screened on one half of the text (at
# most CAUSAL_SCREEN_TEXTS texts each) and the CAUSAL_ITEMS with the largest effect are tested on
# the other half: removing a part must change the output where it fires more than removing an
# equally active part, with a signed-rank z of at least CAUSAL_Z over at least CAUSAL_MIN_TEXTS
# texts. A neuron fires at the strongest 1 - NEURON_FIRING share of positions. Parts that fire in
# too few held-out texts get up to CAUSAL_EXTEND_TEXTS greedy continuations of CAUSAL_EXTEND_TOKENS.
CAUSAL_ITEMS, CAUSAL_POOL, CAUSAL_SCREEN_TEXTS, CAUSAL_Z, NEURON_FIRING = 16, 32, 12, 2.0, 0.9
CAUSAL_MIN_TEXTS, CAUSAL_MIN_GROUP, CAUSAL_EXTEND_TEXTS, CAUSAL_EXTEND_TOKENS = 6, 3, 16, 96
CHECK_FAILURES = {"causal": "removing it changes the output where it fires no more than removing an equally "
                            "active part",
                  "category": "removing it does not change its category more than other text",
                  "replicated": "the two halves of the text disagree",
                  "explanation": "its explanation does not beat a neutral one"}
CHECK_UNTESTED = {"causal": "it fires in too few held-out texts to test causally",
                  "category": "too few held-out texts to compare its category"}
STRUCTURE_PARTS = (("query", "self_attn", "q_proj"), ("key", "self_attn", "k_proj"),
                   ("value", "self_attn", "v_proj"), ("attention_output", "self_attn", "o_proj"),
                   ("gate", "mlp", "gate_proj"), ("up", "mlp", "up_proj"), ("down", "mlp", "down_proj"))
# Matrices that write the residual stream: their rows, not columns, are residual dimensions.
RESIDUAL_WRITERS = ("attention_output", "down")


def category_encoded(tokenizer, probes=CATEGORY_PROBES):
    """Labelled probe texts as (tokens, 1) pairs, with the category of each."""
    encoded, labels = [], []
    for category, texts in probes:
        for text in texts:
            tokens = [int(token) for token in tokenizer.encode(text)]
            # Gemma-family models degrade badly without their BOS token, and some tokenizers
            # (mlx-vlm's) do not add it to plain text.
            bos = getattr(tokenizer, "bos_token_id", None)
            if type(bos) is int and tokens and tokens[0] != bos:
                tokens.insert(0, bos)
            require(2 <= len(tokens) <= MAX_TOKENS, f"a {category} probe does not tokenize to 2-{MAX_TOKENS} tokens")
            encoded.append((tokens, 1))
            labels.append(category)
    return encoded, labels


def observation_encoded(tokenizer):
    """The short probe texts followed by OBSERVATION_PROBES, labelled by category."""
    extra = dict(OBSERVATION_PROBES)
    merged = [(category, texts + extra.get(category, ())) for category, texts in CATEGORY_PROBES]
    return category_encoded(tokenizer, merged)


def category_loss(model, encoded, labels):
    """(overall loss, {category: loss}, answer tokens) as mean negative log-likelihood per token."""
    from dial_worker import target_logprobs
    totals = {}
    for label, (total, count) in zip(labels, target_logprobs(model, encoded)):
        entry = totals.setdefault(label, [0.0, 0])
        entry[0] -= total
        entry[1] += count
    tokens = sum(count for _, count in totals.values())
    require(tokens > 0, "probe texts have no answer tokens")
    return (sum(total for total, _ in totals.values()) / tokens,
            {label: total / count for label, (total, count) in totals.items() if count}, tokens)


def specialty(by_category):
    """The category a component matters most for, when it clearly stands out from the others."""
    if not by_category:
        return None
    values = sorted(by_category.values())
    median = values[len(values) // 2]
    best = max(by_category, key=by_category.get)
    value = by_category[best]
    return best if value >= SPECIALTY_MIN and value >= SPECIALTY_RATIO * max(median, 0.0) else None


def attention_type(layer):
    """"sliding" or "full" when the layer declares its attention span (Gemma), else None."""
    attention = getattr(layer, "self_attn", None)
    kind = getattr(layer, "layer_type", None) or getattr(attention, "layer_type", None)
    if isinstance(kind, str):
        return "sliding" if "sliding" in kind else "full" if "full" in kind else kind
    if isinstance(getattr(attention, "is_sliding", None), bool):
        return "sliding" if attention.is_sliding else "full"
    return None


def head_behaviour(model, encoded, standard):
    """Mean attention measures per head on the probe text: {layer: [{measure: value}, ...]}.

    previous: on the token before; sink: on the first token; self: on the current token; local: on
    the LOCAL_WINDOW tokens before; far: on tokens in the earlier half of the context, excluding the
    first token (a sink would otherwise read as long-range);
    duplicate: on earlier copies of the current token (where one exists); induction: on the token
    after an earlier copy (at positions that repeat an earlier 3-token sequence). Query position 0
    is excluded from all but self, since it can attend only to itself.
    """
    import mlx.core as mx
    import numpy as np
    kinds = ("previous", "sink", "self", "local", "far", "duplicate", "induction")
    modules = {index: attention_heads(model, index) for index in standard}
    seen_values = {(index, kind): [] for index in standard for kind in kinds}
    with attention_inputs(model, list(standard)) as seen:
        for tokens, _ in encoded:
            inputs = np.array(tokens[:-1])
            length = len(inputs)
            queries = [position - 1 for position in repeat_positions(tokens, 0)]
            model(mx.array([tokens[:-1]], dtype=mx.int32))
            match = copy_match(inputs, queries) if queries else None
            gap = np.arange(length)[:, None] - np.arange(length)[None, :]
            band = mx.array(((gap >= 1) & (gap <= LOCAL_WINDOW)).astype(np.float32))
            keys = np.arange(length)[None, :]
            far = mx.array(((gap > np.arange(length)[:, None] / 2) & (keys >= 1)).astype(np.float32))
            same = (inputs[None, :] == inputs[:, None]) & (gap > 0)
            repeated = [position for position in range(length) if same[position].any()]
            duplicate = mx.array(same[repeated].astype(np.float32)) if repeated else None
            for index in standard:
                pattern = attention_pattern(*modules[index], seen[index])
                later = pattern[:, 1:, :]
                measured = {"previous": mx.mean(mx.diagonal(pattern, offset=-1, axis1=1, axis2=2), axis=-1),
                            "sink": mx.mean(later[:, :, 0], axis=-1),
                            "self": mx.mean(mx.diagonal(pattern, axis1=1, axis2=2), axis=-1),
                            "local": mx.mean(mx.sum(later * band[1:], axis=-1), axis=-1),
                            "far": mx.mean(mx.sum(later * far[1:], axis=-1), axis=-1)}
                if duplicate is not None:
                    measured["duplicate"] = mx.mean(mx.sum(pattern[:, repeated, :] * duplicate[None], axis=-1), axis=-1)
                if match is not None:
                    measured["induction"] = mx.mean(mx.sum(pattern[:, queries, :] * match[None], axis=-1), axis=-1)
                mx.eval(*measured.values())
                for kind, value in measured.items():
                    seen_values[(index, kind)].append(np.array(value))
            seen.clear()
            mx.clear_cache()
    out = {}
    for index in standard:
        mean = {kind: np.mean(seen_values[(index, kind)], axis=0) if seen_values[(index, kind)] else None
                for kind in kinds}
        out[index] = [{kind: None if mean[kind] is None else float(mean[kind][head]) for kind in kinds}
                      for head in range(modules[index][1])]
    return out


ROLE_MEASURES = (("induction", "induction", INDUCTION_ROLE), ("previous-token", "previous", PREVIOUS_ROLE),
                 ("attention-sink", "sink", SINK_ROLE), ("current-token", "self", SELF_ROLE),
                 ("same-token", "duplicate", DUPLICATE_ROLE), ("local", "local", LOCAL_ROLE),
                 ("long-range", "far", LONG_ROLE))


def head_role(previous, sink, induction, current=None, duplicate=None, local=None, far=None):
    """A coarse label from the attention pattern; thresholds are heuristics, not a proof of function."""
    measured = {"previous": previous, "sink": sink, "induction": induction, "self": current,
                "duplicate": duplicate, "local": local, "far": far}
    for role, key, threshold in ROLE_MEASURES:
        if measured[key] is not None and measured[key] >= threshold:
            return role
    return "mixed"


def head_leaning(measured):
    """For a mixed head, the role whose measure comes closest to its threshold (relative to it)."""
    scored = [(measured[key] / threshold, role) for role, key, threshold in ROLE_MEASURES
              if measured.get(key) is not None]
    return max(scored)[1] if scored else None


def neuron_activity(model, encoded, mlp_layers):
    """Root-mean-square activation of every MLP neuron (the input of down_proj) on the probe text."""
    import mlx.core as mx
    import numpy as np

    def locate(layer):
        return layer.mlp, "down_proj"
    totals = {}
    with layer_inputs(model, mlp_layers, locate) as seen:
        for tokens, _ in encoded:
            model(mx.array([tokens[:-1]], dtype=mx.int32))
            for index, value in seen.items():
                activity = mx.sqrt(mx.mean(mx.square(value[0].astype(mx.float32)), axis=0))
                mx.eval(activity)
                totals.setdefault(index, []).append(np.array(activity))
            seen.clear()
            mx.clear_cache()
    return {index: np.mean(values, axis=0) for index, values in totals.items()}


def snippet(tokenizer, tokens, position):
    """The text around `position` with the token itself in «guillemets»."""
    before = tokenizer.decode(tokens[max(0, position - CONTEXT_BEFORE):position])
    after = tokenizer.decode(tokens[position + 1:position + 1 + CONTEXT_AFTER])
    return f"{before}«{tokenizer.decode([tokens[position]])}»{after}"


def neuron_contexts(model, tokenizer, encoded, labels, halves, listed):
    """Adds to each listed neuron what fires it, the category it favours and held-out tests.

    A neuron's strength at a position is the magnitude of its activation. `contexts`, `evidence`
    and the private `_test` come from part_evidence. `fires_on` is the category with the highest
    mean strength when that is at least SPECIALTY_RATIO times the mean over all categories, and
    `replicated` says whether each half of the texts alone picks the same category. Returns
    {(layer, neuron): per-text strength arrays} for the causal checks.
    """
    import mlx.core as mx
    import numpy as np
    wanted = {}
    for row in listed:
        wanted.setdefault(row["layer"], []).append(row["neuron"])
    traces = {(row["layer"], row["neuron"]): [] for row in listed}
    with layer_inputs(model, list(wanted), lambda layer: (layer.mlp, "down_proj")) as seen:
        for tokens, _ in encoded:
            model(mx.array([tokens[:-1]], dtype=mx.int32))
            for index, value in seen.items():
                chosen = mx.abs(value[0][:, mx.array(wanted[index])].astype(mx.float32))
                mx.eval(chosen)
                values = np.array(chosen)
                for column, neuron in enumerate(wanted[index]):
                    traces[(index, neuron)].append(values[:, column])
            seen.clear()
            mx.clear_cache()
    rng = np.random.default_rng(0)
    for row in listed:
        trace = traces[(row["layer"], row["neuron"])]
        row["contexts"], row["evidence"], row["_test"] = part_evidence(tokenizer, encoded, labels, halves, trace, rng)
        row["fires_on"] = favoured(trace, labels)
        if row["fires_on"] is not None:
            row["replicated"] = all(
                favoured([values for values, side in zip(trace, halves) if side == half],
                         [label for label, side in zip(labels, halves) if side == half]) == row["fires_on"]
                for half in (0, 1))
    return traces


def favoured(trace, labels):
    """The category whose texts drive a part hardest, if at least SPECIALTY_RATIO times the mean."""
    import numpy as np
    by = {}
    for values, label in zip(trace, labels):
        if len(values) > 1:
            by.setdefault(label, []).append(float(np.mean(values[1:])))
    means = {label: float(np.mean(values)) for label, values in by.items()}
    if len(means) < 2:
        return None
    overall = float(np.mean(list(means.values())))
    best = max(means, key=means.get)
    return best if overall > 0 and means[best] >= SPECIALTY_RATIO * overall else None


def text_halves(labels):
    """0 or 1 per text, alternating within each category: two independent batches of the same mix."""
    counts, halves = {}, []
    for label in labels:
        halves.append(counts.get(label, 0) % 2)
        counts[label] = counts.get(label, 0) + 1
    return halves


def part_evidence(tokenizer, encoded, labels, halves, trace, rng):
    """Display contexts, explanation evidence and graded held-out tests for one part.

    `trace[n]` is the part's strength at each input position of text n (|activation| for a neuron,
    router probability for an expert); the BOS position is ignored. `contexts` are the peaks of the
    NEURON_CONTEXTS texts that drive it hardest. An explanation is written from the peaks of the
    first half of the texts only (`evidence`) and tested on the second half only (`_test`): the
    EXPLAIN_TEST strongest peaks ("firing"), EXPLAIN_TEST hard negatives ("hard": the tokens of its
    peaks where it stays below its median strength, else the weakest positions of its firing texts)
    and EXPLAIN_TEST random positions ("random"), as (snippet, strength, kind).
    """
    import numpy as np

    def text(number, position):
        return snippet(tokenizer, encoded[number][0], position)
    peaks = sorted(((float(values[1 + int(np.argmax(values[1:]))]), number, 1 + int(np.argmax(values[1:])))
                    for number, values in enumerate(trace) if len(values) > 1), reverse=True)
    contexts = [{"text": text(number, position), "strength": value, "category": labels[number]}
                for value, number, position in peaks[:NEURON_CONTEXTS]]
    evidence = [text(number, position) for _, number, position in peaks if halves[number] == 0][:NEURON_CONTEXTS]
    firing = [peak for peak in peaks if halves[peak[1]] == 1][:EXPLAIN_TEST]
    test = [(text(number, position), value, "firing") for value, number, position in firing]
    places = [(number, position) for number, values in enumerate(trace) if halves[number] == 1
              for position in range(1, len(values))]
    if not places:
        return contexts, evidence, test
    strengths = np.array([trace[number][position] for number, position in places], dtype=np.float64)
    median = float(np.median(np.concatenate([values[1:] for values in trace if len(values) > 1])))
    peak_tokens = {encoded[number][0][position] for _, number, position in peaks[:2 * NEURON_CONTEXTS]}
    taken = {(number, position) for _, number, position in firing}
    hard = [item for item in np.argsort(strengths, kind="stable").tolist()
            if encoded[places[item][0]][0][places[item][1]] in peak_tokens and strengths[item] < median
            and places[item] not in taken][:EXPLAIN_TEST]
    texts = {number for _, number, _ in firing}
    hard += [item for item in np.argsort(strengths, kind="stable").tolist()
             if places[item][0] in texts and item not in hard and places[item] not in taken][:EXPLAIN_TEST - len(hard)]
    test += [(text(*places[item]), float(strengths[item]), "hard") for item in hard]
    test += [(text(*places[item]), float(strengths[item]), "random")
             for item in rng.choice(len(places), size=min(EXPLAIN_TEST, len(places)), replace=False).tolist()]
    return contexts, evidence, test


def rms_recorder(sink):
    """A module-hook callback that records the root-mean-square of a module's output."""
    import mlx.core as mx

    def record(output):
        value = steering.hidden(output).astype(mx.float32)
        sink.append(float(mx.sqrt(mx.mean(mx.square(value))).item()))
        return output
    return record


def moe_router(layer):
    """How a layer routes tokens to experts: (owner, key, scores, top_k, shared_gate, run) or None.

    `scores(x)` maps the recorded input of `owner.key` to router logits over experts and `run(x,
    experts)` gives each row of x's output from one expert (shape [rows, hidden]). Qwen-style blocks
    route inside `mlp` (`mlp.gate`, `mlp.switch_mlp`); Gemma 4 has a separate `router` and `experts`
    beside an always-on dense `mlp`, normalizing the router and expert inputs itself.
    """
    import mlx.core as mx
    mlp = getattr(layer, "mlp", None)
    if hasattr(mlp, "switch_mlp") and hasattr(mlp, "gate"):
        shared = getattr(mlp, "shared_expert_gate", None)

        def run_qwen(x, experts):
            return mlp.switch_mlp(x, experts[:, None]).reshape(x.shape[0], -1)
        return layer, "mlp", mlp.gate, int(mlp.top_k), shared, run_qwen
    router = getattr(layer, "router", None)
    if router is not None and getattr(layer, "experts", None) is not None and hasattr(router, "proj"):
        top_k = getattr(getattr(router, "config", None), "top_k_experts", None)
        if type(top_k) is int:
            def scores(x):
                return router.proj(mx.fast.rms_norm(x, router.scale * router._root_size, router.eps))

            def run_gemma(x, experts):
                before = getattr(layer, "pre_feedforward_layernorm_2", None)
                after = getattr(layer, "post_feedforward_layernorm_2", None)
                out = layer.experts.switch_glu(before(x) if before is not None else x, experts[:, None])
                out = out.reshape(x.shape[0], -1)
                return after(out) if after is not None else out
            return layer, "router", scores, top_k, None, run_gemma
    return None


def linear_memory(attention, x):
    """Per-head (sum of log decay, sum of write strength) of a Gated DeltaNet block on input x.

    The recurrent state is multiplied by g = exp(-exp(A_log) * softplus(a + dt_bias)) at each token
    and written with strength beta = sigmoid(b) (Qwen3.5/3.6 `linear_attn`). None when the block
    does not expose these parameters.
    """
    import mlx.core as mx
    from mlx import nn
    parts = ("in_proj_a", "in_proj_b", "A_log", "dt_bias")
    if not all(hasattr(attention, name) for name in parts):
        return None
    a = attention.in_proj_a(x).astype(mx.float32)
    log_decay = -mx.exp(attention.A_log.astype(mx.float32)) * nn.softplus(a + attention.dt_bias.astype(mx.float32))
    write = mx.sigmoid(attention.in_proj_b(x).astype(mx.float32))
    flat = log_decay.reshape(-1, log_decay.shape[-1])
    return mx.sum(flat, axis=0), mx.sum(write.reshape(flat.shape), axis=0), flat.shape[0]


def memory_row(sums):
    """Summary of linear-attention memory per head from accumulated (log decay, write, tokens)."""
    import numpy as np
    log_decay, write, tokens = sums
    mean = np.minimum(log_decay / tokens, -1e-9)
    half_life = math.log(0.5) / mean
    return {"half_life": [float(value) for value in half_life], "median_half_life": float(np.median(half_life)),
            "longest_half_life": float(np.max(half_life)), "write_strength": float(np.mean(write / tokens)),
            "long_heads": int(np.sum(half_life >= MEMORY_LONG)), "short_heads": int(np.sum(half_life <= MEMORY_SHORT))}


def router_read(model, encoded, moe_layers, linear_layers, labels=None, halves=None):
    """Router statistics of each mixture-of-experts layer and a reading of each linear-attention block.

    A routed slot is one (token, chosen expert) pair and `share` is an expert's fraction of the slots.
    `entropy` is the mean per-token entropy of the router distribution over log(experts): 1 is uniform
    routing, 0 sends every token to one expert. `shared_gate` is the mean sigmoid gate of the shared
    expert. With `labels` (one category per text) each layer also lists `specialists`: experts whose
    share of one category's slots is at least EXPERT_LIFT times their overall share (`lift`), with
    the lift in each half of the texts (`half_lifts`, text_halves) and `replicated` when both reach
    EXPERT_LIFT. Linear-attention rows hold `output_rms` and, for Gated DeltaNet blocks, `memory`
    (memory_row). Returns ({layer: experts row}, {layer: linear_attention row}, {layer: observed
    routing}) where the observed routing (chosen experts and router probabilities per text, the
    latter as float16 [positions, experts]) feeds expert_profiles and the causal checks.
    """
    import mlx.core as mx
    import numpy as np
    layers = steering.layer_list(model)
    routers = {index: moe_router(layers[index]) for index in moe_layers}
    labels = labels or ["all"] * len(encoded)
    halves = halves or text_halves(labels)
    totals, outputs, inputs, memory = {}, {index: [] for index in linear_layers}, {}, {}
    originals = {index: layers[index].linear_attn for index in linear_layers}

    def observe(index):
        def record(x, *args, **kwargs):
            inputs[index] = x
        return record
    with contextlib.ExitStack() as stack:
        seen = stack.enter_context(layer_inputs(model, moe_layers, lambda layer: moe_router(layer)[:2]))
        for index in linear_layers:
            layers[index].linear_attn = steering.wrap(originals[index], rms_recorder(outputs[index]), observe(index))
        stack.callback(lambda: [setattr(layers[index], "linear_attn", module) for index, module in originals.items()])
        for number, ((tokens, _), label) in enumerate(zip(encoded, labels)):
            model(mx.array([tokens[:-1]], dtype=mx.int32))
            for index, x in inputs.items():
                read = linear_memory(originals[index], x)
                if read is not None:
                    mx.eval(*read[:2])
                    entry = memory.setdefault(index, [0.0, 0.0, 0])
                    entry[0] += np.array(read[0])
                    entry[1] += np.array(read[1])
                    entry[2] += read[2]
            inputs.clear()
            for index, value in seen.items():
                _, _, scores, top_k, shared_gate, _ = routers[index]
                probs = mx.softmax(scores(value).astype(mx.float32), axis=-1, precise=True)
                count = int(probs.shape[-1])
                chosen = mx.argpartition(probs, kth=-top_k, axis=-1)[..., -top_k:]
                entropy = -mx.sum(probs * mx.log(probs + 1e-12), axis=-1) / math.log(count)
                parts = [entropy, chosen, probs]
                shared = mx.sigmoid(shared_gate(value).astype(mx.float32)) if shared_gate is not None else None
                if shared is not None:
                    parts.append(shared)
                mx.eval(*parts)
                total = totals.setdefault(index, {"counts": np.zeros(count), "by": {}, "entropy": 0.0,
                                                  "tokens": 0, "shared": [], "top_k": top_k, "chosen": [],
                                                  "probs": []})
                picked = np.array(chosen).reshape(-1, top_k)
                total["chosen"].append(picked)
                used = np.bincount(picked.reshape(-1), minlength=count)
                total["counts"] += used
                key = (label, halves[number])
                total["by"][key] = total["by"].get(key, np.zeros(count)) + used
                total["entropy"] += float(np.sum(np.array(entropy)))
                total["tokens"] += int(np.array(entropy).size)
                if shared is not None:
                    total["shared"].append(float(np.mean(np.array(shared))))
                total["probs"].append(np.array(probs).reshape(-1, count).astype(np.float16))
            seen.clear()
            mx.clear_cache()
    routed, observed = {}, {}
    for index, total in totals.items():
        counts = total["counts"]
        ranked = np.argsort(-counts, kind="stable")[:8].tolist()
        specialists = []
        by_label, by_half = {}, {}
        for (label, half), part in total["by"].items():
            by_label[label] = by_label.get(label, 0) + part
            by_half[half] = by_half.get(half, 0) + part
        for label, part in by_label.items() if len(by_label) > 1 else ():
            lift = (part / part.sum()) / np.maximum(counts / counts.sum(), 1e-12)
            for expert in np.flatnonzero((part >= EXPERT_MIN_SLOTS) & (lift >= EXPERT_LIFT)).tolist():
                half_lifts = []
                for half, slots in sorted(by_half.items()):
                    mine = total["by"].get((label, half))
                    half_lifts.append(float((mine[expert] / mine.sum()) / max(slots[expert] / slots.sum(), 1e-12))
                                      if mine is not None and mine.sum() > 0 else 0.0)
                specialists.append({"expert": expert, "category": label, "lift": float(lift[expert]),
                                    "category_share": float(part[expert] / part.sum()), "half_lifts": half_lifts,
                                    "replicated": len(half_lifts) == 2 and min(half_lifts) >= EXPERT_LIFT})
        specialists.sort(key=lambda item: -item["lift"])
        best = {}
        for item in specialists:
            best.setdefault(item["expert"], item)
        routed[index] = {"count": len(counts), "top_k": total["top_k"],
                         "entropy": total["entropy"] / total["tokens"],
                         "unused": int(np.sum(counts == 0)),
                         "top": [{"expert": item, "share": float(counts[item] / counts.sum()),
                                  **({"category": best[item]["category"]} if item in best else {})}
                                 for item in ranked if counts[item] > 0],
                         "specialists": list(best.values())[:8],
                         "shared_gate": float(np.mean(total["shared"])) if total["shared"] else None}
        observed[index] = {key: total[key] for key in ("chosen", "probs")}
    measured = {index: {"output_rms": float(np.mean(values))} for index, values in outputs.items() if values}
    for index, sums in memory.items():
        measured.setdefault(index, {})["memory"] = memory_row(sums)
    return routed, measured, observed


def expert_profiles(model, tokenizer, encoded, labels, halves, routed, observed):
    """What the most specialised and busiest experts of each MoE layer handle and write.

    For up to PROFILE_EXPERTS experts per layer (category specialists first, then the busiest), with
    the router probability as the expert's strength: `tokens` are input tokens routed to it at least
    twice and at least EXPERT_LIFT times as often as their overall frequency predicts; `contexts`,
    `evidence` and the private `_test` come from part_evidence; `promotes`/`suppresses` are the
    tokens its mean output on its PROFILE_PEAKS strongest positions pushes toward or away from,
    relative to the layer's other profiled experts (final norm and output embedding, ignoring the
    later layers). Returns {layer: [profile]}.
    """
    import mlx.core as mx
    import numpy as np
    layers = steering.layer_list(model)
    project = unembedding(model)
    norm = getattr(getattr(model, "model", None), "norm", None)
    rng = np.random.default_rng(0)
    profiles, needed = {}, {}
    for index, data in observed.items():
        row = routed[index]
        picks = list(dict.fromkeys([item["expert"] for item in row["specialists"]] +
                                   [item["expert"] for item in row["top"]]))[:PROFILE_EXPERTS]
        chosen = np.concatenate(data["chosen"])
        places = [(number, position) for number, part in enumerate(data["chosen"]) for position in range(len(part))]
        current = np.array([encoded[number][0][position] for number, position in places])
        overall = dict(zip(*np.unique(current, return_counts=True)))
        starts = np.array([position == 0 for _, position in places])
        for expert in picks:
            routed_here = np.any(chosen == expert, axis=1)
            slots = int(np.sum(routed_here))
            if not slots:
                continue
            tokens = []
            for token, count in zip(*np.unique(current[routed_here], return_counts=True)):
                lift = (count / slots) / (overall[token] / len(current))
                if count >= 2 and lift >= EXPERT_LIFT:
                    tokens.append((int(count), float(lift), int(token)))
            tokens.sort(reverse=True)
            trace = [probs[:, expert].astype(np.float32) for probs in data["probs"]]
            contexts, evidence, test = part_evidence(tokenizer, encoded, labels, halves, trace, rng)
            special = next((item for item in row["specialists"] if item["expert"] == expert), None)
            profiles.setdefault(index, []).append({
                "layer": index, "expert": expert, "share": float(slots / len(current)),
                "category": special["category"] if special else None,
                **({"replicated": special["replicated"]} if special else {}),
                "tokens": [tokenizer.decode([token]) for _, _, token in tokens[:PROFILE_TOKENS]],
                "contexts": contexts, "evidence": evidence, "_test": test})
            strengths = np.where(starts, -1.0, np.concatenate(trace))
            for item in np.argsort(-strengths, kind="stable")[:PROFILE_PEAKS].tolist():
                number, position = places[item]
                needed.setdefault(number, []).append((index, expert, position))
    sums = {}
    with layer_inputs(model, list(profiles), lambda layer: moe_router(layer)[:2]) as seen:
        for number in sorted(needed):
            model(mx.array([encoded[number][0][:-1]], dtype=mx.int32))
            for index in {item[0] for item in needed[number]}:
                wanted = [(expert, position) for layer, expert, position in needed[number] if layer == index]
                x = seen[index][0][mx.array([position for _, position in wanted])]
                out = moe_router(layers[index])[5](x, mx.array([expert for expert, _ in wanted])).astype(mx.float32)
                mx.eval(out)
                for (expert, _), vector in zip(wanted, np.array(out)):
                    entry = sums.setdefault((index, expert), [0.0, 0])
                    entry[0] = entry[0] + vector
                    entry[1] += 1
            seen.clear()
            mx.clear_cache()
    for index, items in profiles.items():
        found = [(item, sums[(index, item["expert"])]) for item in items if (index, item["expert"]) in sums]
        if not found:
            continue
        vectors = mx.array(np.stack([total / count for _, (total, count) in found])[:, None, :])
        logits = project(norm(vectors) if norm is not None else vectors)[:, 0].astype(mx.float32)
        mx.eval(logits)
        scores = np.array(logits)
        # Contrast with the other profiled experts of the layer: what all of them write (the shared
        # direction of the block's output) says nothing about any one expert.
        if len(found) > 1:
            scores = scores - scores.mean(axis=0, keepdims=True)
        for (item, _), row in zip(found, scores):
            item["promotes"] = [tokenizer.decode([int(i)]) for i in np.argsort(-row)[:EXPLAIN_TOKENS]]
            item["suppresses"] = [tokenizer.decode([int(i)]) for i in np.argsort(row)[:EXPLAIN_TOKENS]]
    return profiles


def logit_lens(model, tokenizer, encoded, labels):
    """How close each layer's output is to the final prediction, and what each layer's update writes.

    Each layer's hidden state, and the input of layer 0, is pushed through the final norm and output
    embedding (with the logit soft-cap when the model has one) at up to LENS_POSITIONS evenly spaced
    positions per text. Returns {layer: {"agreement", "top10", "target_logprob", "by_category",
    "top10_by_category", "writes"}}: `agreement` is the fraction of positions whose top token equals
    the final top token, `top10` the fraction where the final top token is among the layer's ten
    best, and `target_logprob` the mean log-probability of the actual next token. `writes` is what
    the layer's update changes in that read-out: `target_gain`, the change in the next token's
    log-probability, and `promotes` / `suppresses`, the tokens most often among the WRITE_TOP
    largest log-probability rises / falls, with the `share` of positions where they are.
    """
    import mlx.core as mx
    import numpy as np
    from collections import Counter
    layers = steering.layer_list(model)
    project = unembedding(model)
    norm = getattr(getattr(model, "model", None), "norm", None)
    cap = getattr(model, "final_logit_softcapping", None)
    states = {}

    def recorder(index):
        def record(output):
            states[index] = steering.hidden(output)
            return output
        return record

    def first_input(*args, **kwargs):
        states[-1] = args[0] if args else kwargs.get("x")

    def read(state):
        logits = project(norm(state) if norm is not None else state)[0].astype(mx.float32)
        if isinstance(cap, (int, float)) and cap > 0:
            logits = mx.tanh(logits / cap) * cap
        return logits - mx.logsumexp(logits, axis=-1, keepdims=True)
    sums, writes = {}, {index: {"gain": 0.0, "positions": 0, "rises": Counter(), "falls": Counter()}
                        for index in range(len(layers))}
    originals = list(layers)
    try:
        for index, layer in enumerate(originals):
            layers[index] = steering.wrap(layer, recorder(index), first_input if index == 0 else None)
        for (tokens, _), label in zip(encoded, labels):
            length = len(tokens) - 1
            sample = np.unique(np.linspace(0, length - 1, min(LENS_POSITIONS, length)).round().astype(np.int32))
            chosen = mx.array(sample)
            final = model(mx.array([tokens[:-1]], dtype=mx.int32))[0][chosen]
            final_top = mx.argmax(final, axis=-1)
            targets = mx.array(tokens[1:], dtype=mx.int32)[chosen]
            previous = read(states[-1][:, chosen]) if -1 in states else None
            for index in range(len(originals)):
                logprobs = read(states[index][:, chosen])
                agree = mx.sum(mx.argmax(logprobs, axis=-1) == final_top)
                best = mx.argpartition(-logprobs, kth=LENS_TOP - 1, axis=-1)[:, :LENS_TOP]
                near = mx.sum(mx.any(best == final_top[:, None], axis=-1))
                target = mx.take_along_axis(logprobs, targets[:, None], axis=-1)
                parts = [agree, near, target]
                if previous is not None:
                    change = logprobs - previous
                    rises = mx.argpartition(-change, kth=WRITE_TOP - 1, axis=-1)[:, :WRITE_TOP]
                    falls = mx.argpartition(change, kth=WRITE_TOP - 1, axis=-1)[:, :WRITE_TOP]
                    gain = mx.sum(mx.take_along_axis(change, targets[:, None], axis=-1))
                    parts += [rises, falls, gain]
                mx.eval(*parts)
                entry = sums.setdefault((index, label), [0.0, 0.0, 0, 0.0])
                entry[0] += float(agree.item())
                entry[1] += float(mx.sum(target).item())
                entry[2] += len(sample)
                entry[3] += float(near.item())
                if previous is not None:
                    written = writes[index]
                    written["gain"] += float(gain.item())
                    written["positions"] += len(sample)
                    written["rises"].update(np.array(rises).reshape(-1).tolist())
                    written["falls"].update(np.array(falls).reshape(-1).tolist())
                previous = logprobs
            states.clear()
            mx.clear_cache()
    finally:
        layers[:] = originals
    out = {}
    for index in range(len(originals)):
        parts = {label: value for (layer, label), value in sums.items() if layer == index}
        count = sum(value[2] for value in parts.values())
        out[index] = {"agreement": sum(value[0] for value in parts.values()) / count,
                      "top10": sum(value[3] for value in parts.values()) / count,
                      "target_logprob": sum(value[1] for value in parts.values()) / count,
                      "by_category": {label: value[0] / value[2] for label, value in parts.items()},
                      "top10_by_category": {label: value[3] / value[2] for label, value in parts.items()}}
    # Rank a layer's written tokens by how much more often they move in it than in the other layers:
    # tokens that rise in every layer (often unused or special tokens) say nothing about one layer.
    totals = {source: Counter() for source in ("rises", "falls")}
    for written in writes.values():
        for source in totals:
            totals[source].update(written[source])
    others = max(1, len([written for written in writes.values() if written["positions"]]) - 1)
    for index in range(len(originals)):
        written = writes[index]
        if not written["positions"]:
            continue
        positions = written["positions"]
        out[index]["writes"] = {"target_gain": written["gain"] / positions}
        for key, source in (("promotes", "rises"), ("suppresses", "falls")):
            ranked = sorted(((hits / positions - (totals[source][token] - hits) / positions / others, hits, token)
                             for token, hits in written[source].items()), reverse=True)
            out[index]["writes"][key] = [{"token": tokenizer.decode([token]), "share": hits / positions,
                                          "excess": excess}
                                         for excess, hits, token in ranked[:WRITE_LIST] if excess > 0]
    return out


def module_matrix(linear):
    """The float32 (out, in) weight of a linear module, dequantized when it is quantized."""
    import mlx.core as mx
    weight = linear.weight
    if hasattr(linear, "scales"):
        options = {"group_size": linear.group_size, "bits": linear.bits}
        if getattr(linear, "mode", "affine") != "affine":
            options["mode"] = linear.mode
        weight = mx.dequantize(weight, linear.scales, getattr(linear, "biases", None), **options)
    return weight.astype(mx.float32)


def matrix_structure(weight):
    """Spectrum and outliers of one (out, in) matrix.

    `effective_rank` is exp(entropy of the normalized singular values) over the smaller dimension:
    near 1 the matrix uses all its directions, near 0 a few dominate. `top_energy` is the share of
    squared singular values in the top 1% of directions. Outlier rows/columns have a norm above
    OUTLIER times the median; their indices are returned for residual-stream dimensions.
    """
    import mlx.core as mx
    import numpy as np
    rows, columns = weight.shape
    gram = weight @ weight.T if rows <= columns else weight.T @ weight
    row_norms, column_norms = mx.sqrt(mx.sum(weight * weight, axis=1)), mx.sqrt(mx.sum(weight * weight, axis=0))
    mx.eval(gram, row_norms, column_norms)
    energy = np.clip(np.linalg.eigvalsh(np.array(gram)), 0, None)[::-1].astype(np.float64)
    require(energy.sum() > 0, "a weight matrix is all zeros")
    singular = np.sqrt(energy)
    p = singular / singular.sum()
    p = p[p > 0]
    row_norms, column_norms = np.array(row_norms), np.array(column_norms)
    outlying = lambda norms: np.flatnonzero(norms > OUTLIER * np.median(norms))
    return {"shape": [int(rows), int(columns)],
            "effective_rank": float(np.exp(-np.sum(p * np.log(p)))) / min(rows, columns),
            "top_energy": float(energy[:max(1, len(energy) // 100)].sum() / energy.sum()),
            "outlier_rows": outlying(row_norms).tolist(), "outlier_columns": outlying(column_norms).tolist()}


def weight_structure(model, request):
    """Per layer and dense matrix: effective rank, top-direction energy and outlier counts, plus the
    residual-stream dimensions that many matrices read or write with outsized weights.

    Expert stacks are not decomposed (one matrix per expert would dominate the run time).
    """
    from collections import Counter
    layers = steering.layer_list(model)
    rows, dimensions = {}, Counter()
    for index, layer in enumerate(layers):
        report(request, "reading weight structure", index, len(layers))
        found = {}
        for role, owner_key, key in STRUCTURE_PARTS:
            module = getattr(getattr(layer, owner_key, None), key, None)
            if module is None or getattr(module, "weight", None) is None or module.weight.ndim != 2:
                continue
            measured = matrix_structure(module_matrix(module))
            residual = measured["outlier_rows"] if role in RESIDUAL_WRITERS else measured["outlier_columns"]
            dimensions.update(residual)
            found[role] = {"shape": measured["shape"], "effective_rank": measured["effective_rank"],
                           "top_energy": measured["top_energy"], "outlier_rows": len(measured["outlier_rows"]),
                           "outlier_columns": len(measured["outlier_columns"])}
        if found:
            rows[index] = found
    common = [(dim, count) for dim, count in dimensions.most_common(10) if count >= OUTLIER_MATRICES]
    return rows, [{"dimension": int(dim), "matrices": count} for dim, count in common]


def chat_tokens(tokenizer, text):
    """One user turn with the generation prompt, thinking disabled where the template supports it."""
    messages = [{"role": "user", "content": text}]
    try:
        tokens = tokenizer.apply_chat_template(messages, add_generation_prompt=True, tokenize=True,
                                               enable_thinking=False)
    except TypeError:
        tokens = tokenizer.apply_chat_template(messages, add_generation_prompt=True, tokenize=True)
    return [int(token) for token in tokens]


def explanation_prompt(item, own=True):
    lines = [f"You are looking at one part of {'your own network' if own else 'a language model'}: {item['what']}. "
             "It activates most strongly on the token marked «like this» in these snippets:"]
    lines += [f"{number}. {text}" for number, text in enumerate(item["evidence"], 1)]
    if item.get("tokens"):
        lines.append("Input tokens it handles far more than usual: " + ", ".join(repr(t) for t in item["tokens"]))
    if item.get("promotes"):
        lines.append("When active it pushes the next token toward: " + ", ".join(repr(t) for t in item["promotes"]))
    lines.append(f"In one phrase of at most {EXPLAIN_WORDS} words, say what this part detects or does. "
                 "Reply with the phrase only.")
    return "\n".join(lines)


def clean_explanation(text):
    text = text.split("</think>")[-1].strip()
    line = next((part.strip() for part in text.splitlines() if part.strip()), "")
    line = line.strip("\"'*` ").rstrip(".")
    words = line.split()
    return " ".join(words[:EXPLAIN_WORDS * 2])


def answer_ids(tokenizer, words):
    """The first token id of each word, ignoring a BOS token the tokenizer adds."""
    bos = getattr(tokenizer, "bos_token_id", None)
    ids = set()
    for word in words:
        encoded = [int(token) for token in tokenizer.encode(word) if token != bos]
        if encoded:
            ids.add(encoded[0])
    return sorted(ids)


def explain(model, tokenizer, items):
    """The model describes each item (an expert or neuron) from evidence, and the description is tested.

    Each item has `what`, `evidence` (snippets from the first half of the texts), optional `tokens` /
    `promotes`, and `_test` held-out (snippet, strength, kind) triples from the second half
    (part_evidence). The model writes `explanation` greedily; then, shown only a description, it
    answers yes/no for every held-out snippet. `explanation_score` is the rank correlation between its
    yes-margin and the part's real strength (0 = chance, 1 = it ranks the snippets exactly);
    `control_score` is the same for the neutral EXPLAIN_CONTROL description, showing how far the
    snippets alone give the answer away; `hard_negative_score` is the share of (firing, hard
    negative) pairs where the firing snippet gets the stronger yes under the explanation (0.5 = chance).
    """
    import mlx.core as mx
    from mlx_lm import stream_generate
    from mlx_lm.sample_utils import make_sampler
    yes, no = answer_ids(tokenizer, ("yes", "Yes", " yes", " Yes")), answer_ids(tokenizer, ("no", "No", " no", " No"))
    require(yes and no, "the tokenizer has no yes/no tokens")
    for item in items:
        text = ""
        for piece in stream_generate(model, tokenizer, prompt=chat_tokens(tokenizer, explanation_prompt(item)),
                                     max_tokens=EXPLAIN_MAX_TOKENS, sampler=make_sampler(temp=0.0)):
            text += piece.text
        item["explanation"] = clean_explanation(text)
        strengths = [strength for _, strength, _ in item["_test"]]
        for key, description in (("explanation_score", item["explanation"]), ("control_score", EXPLAIN_CONTROL)):
            margins = description_margins(model, tokenizer, description, item["_test"], yes, no)
            item[key] = rank_correlation(margins, strengths)
            if key == "explanation_score":
                kinds = [kind for _, _, kind in item["_test"]]
                item["hard_negative_score"] = pairwise_auc(
                    [margin for margin, kind in zip(margins, kinds) if kind == "firing"],
                    [margin for margin, kind in zip(margins, kinds) if kind == "hard"])
        mx.clear_cache()


def description_margins(model, tokenizer, description, test, yes, no):
    """The model's yes-minus-no log-odds that `description` fits each test snippet's marked token."""
    import mlx.core as mx
    margins = []
    for snippet_text, _, _ in test:
        question = (f"A part of a language model is described as: \"{description}\".\nText, with one token "
                    f"marked «like this»: {snippet_text}\nDoes the description fit the marked token? "
                    "Answer yes or no.")
        logits = model(mx.array([chat_tokens(tokenizer, question)], dtype=mx.int32))[0, -1].astype(mx.float32)
        margins.append(float((mx.logsumexp(logits[mx.array(yes)]) - mx.logsumexp(logits[mx.array(no)])).item()))
    return margins


def explanation_request(item):
    """What an outside model needs to describe `item` and rate its held-out snippets (`explained`
    scores the answers). The snippets are shuffled so their kind cannot be read from their order;
    `test` keeps each one's strength and kind, in the same order, for scoring only."""
    import numpy as np
    kind, layer, unit = part_key(item)
    order = np.random.default_rng([layer, unit]).permutation(len(item["_test"])).tolist()
    test = [item["_test"][number] for number in order]
    snippets = "\n".join(f"{number}. {text}" for number, (text, _, _) in enumerate(test, 1))
    return {"kind": kind, "layer": layer, "unit": unit, "describe": explanation_prompt(item, own=False),
            "rate": EXPLAIN_RATE.replace("{snippets}", snippets).replace("{count}", str(len(test))),
            "count": len(test), "control": EXPLAIN_CONTROL,
            "test": [{"strength": strength, "kind": label} for _, strength, label in test]}


def score_ratings(item, ratings, control, test):
    """Scores an outside model's 0-10 ratings like `explain` scores the model's own yes-margins."""
    strengths, kinds = [entry["strength"] for entry in test], [entry["kind"] for entry in test]
    item["explanation_score"] = rank_correlation(ratings, strengths)
    item["control_score"] = rank_correlation(control, strengths) if control is not None else None
    item["hard_negative_score"] = pairwise_auc([rating for rating, kind in zip(ratings, kinds) if kind == "firing"],
                                               [rating for rating, kind in zip(ratings, kinds) if kind == "hard"])


PART_FIELDS = ("layer", "category", "replicated", "explanation", "explanation_score", "control_score",
               "hard_negative_score", "causal", "checks", "confirmed")


def explanation_summary(listed, profiles):
    """summary.explanations: every explained neuron and expert profile, confirmed and best-scoring first."""
    return sorted(
        [{"kind": "expert", "expert": p["expert"], **{key: p.get(key) for key in PART_FIELDS}}
         for p in profiles if p.get("explanation")] +
        [{"kind": "neuron", "neuron": row["neuron"], **{key: row.get(key) for key in PART_FIELDS},
          "category": row.get("fires_on")} for row in listed if row.get("explanation")],
        key=lambda item: (not item["confirmed"], -((item["explanation_score"] or 0) - (item["control_score"] or 0))))


def finish_explanations(request, root, config):
    """Completes an analyze result whose explanations an outside model wrote (analyze with explainer
    "external"). `answers` are {index, explanation, ratings, control_ratings} for entries of the
    result's `explanation_requests`; each is scored like the model's own explanations, the parts are
    judged again and the summary and findings rewritten. `explainer` names the outside model and
    `note` says why some answers are missing. Loads no model."""
    result, answers = request.get("result"), request.get("answers")
    require(isinstance(result, dict) and isinstance(answers, list), "explained needs a `result` and `answers`")
    asks = result.pop("explanation_requests", None) or []

    def ratings(values, count):
        return isinstance(values, list) and len(values) == count and all(
            isinstance(value, (int, float)) and not isinstance(value, bool) and math.isfinite(value) for value in values)
    scored = {}
    for number, answer in enumerate(answers):
        index = answer.get("index") if isinstance(answer, dict) else None
        require(isinstance(index, int) and 0 <= index < len(asks), f"answer {number} names no explanation request")
        ask, text = asks[index], clean_explanation(str(answer.get("explanation") or ""))
        require(text and ratings(answer.get("ratings"), ask["count"]),
                f"answer {number} needs an explanation and {ask['count']} ratings")
        item = {"explanation": text}
        control = answer.get("control_ratings")
        score_ratings(item, answer["ratings"], control if ratings(control, ask["count"]) else None, ask["test"])
        scored[(ask["kind"], ask["layer"], ask["unit"])] = item
    rows = result.get("layers") or []
    listed = [part for row in rows for part in row.get("neurons") or []]
    profiles = [part for row in rows for part in (row.get("experts") or {}).get("profiles") or []]
    summary = result.setdefault("summary", {})
    for part in listed + profiles + (summary.get("notable_neurons") or []) + (summary.get("checked_parts") or []):
        if part_key(part) in scored:
            part.update(scored[part_key(part)])
            judged(part, part.get("causal"))
    summary["explanations"] = explanation_summary(listed, profiles)
    summary["checked_parts"] = sorted(summary.get("checked_parts") or [], key=lambda item: not item["confirmed"])
    if isinstance(request.get("explainer"), str):
        result["explainer"] = request["explainer"][:256]
    if isinstance(request.get("note"), str):
        result.setdefault("notes", []).append(request["note"][:1000])
    result["findings"] = findings(result)
    return result


def average_ranks(values):
    """0-based ranks of `values`, ties sharing their mean rank."""
    import numpy as np
    values = np.asarray(values, dtype=np.float64)
    order = np.argsort(values, kind="stable")
    out = np.empty(len(values))
    start = 0
    while start < len(values):
        end = start
        while end + 1 < len(values) and values[order[end + 1]] == values[order[start]]:
            end += 1
        out[order[start:end + 1]] = (start + end) / 2
        start = end + 1
    return out


def rank_correlation(first, second):
    """Spearman correlation with average ranks for ties; None when either side is constant or short."""
    import numpy as np
    x, y = average_ranks(first), average_ranks(second)
    if len(x) < 3 or x.std() == 0 or y.std() == 0:
        return None
    return float(np.corrcoef(x, y)[0, 1])


def signed_rank_z(differences):
    """Wilcoxon signed-rank z (normal approximation, tie-corrected): how consistently the differences
    are positive, insensitive to a few extreme values. Zeros are dropped; None with under two left."""
    import numpy as np
    values = np.asarray([value for value in differences if value != 0], dtype=np.float64)
    if len(values) < 2:
        return None
    n = len(values)
    statistic = float((average_ranks(np.abs(values)) + 1)[values > 0].sum())
    _, counts = np.unique(np.abs(values), return_counts=True)
    variance = n * (n + 1) * (2 * n + 1) / 24 - float(np.sum(counts ** 3 - counts)) / 48
    return (statistic - n * (n + 1) / 4) / math.sqrt(variance) if variance > 0 else 0.0


def rank_sum_z(first, second):
    """Mann-Whitney z (normal approximation, tie-corrected) that `first` tends to exceed `second`;
    None when either group has under two values."""
    import numpy as np
    first, second = np.asarray(first, dtype=np.float64), np.asarray(second, dtype=np.float64)
    if len(first) < 2 or len(second) < 2:
        return None
    both = np.concatenate([first, second])
    n1, n2, n = len(first), len(second), len(both)
    u = float((average_ranks(both) + 1)[:n1].sum()) - n1 * (n1 + 1) / 2
    _, counts = np.unique(both, return_counts=True)
    variance = n1 * n2 / 12 * ((n + 1) - float(np.sum(counts ** 3 - counts)) / (n * (n - 1)))
    return (u - n1 * n2 / 2) / math.sqrt(variance) if variance > 0 else 0.0


def pairwise_auc(high, low):
    """Share of (high, low) pairs ordered correctly, ties counting half; None without both groups."""
    if not (high and low):
        return None
    return sum((a > b) + 0.5 * (a == b) for a in high for b in low) / (len(high) * len(low))


def welch_z(first, second):
    """Difference of means over its standard error (Welch); None when a group has under two values."""
    import numpy as np
    first, second = np.asarray(first, dtype=np.float64), np.asarray(second, dtype=np.float64)
    if len(first) < 2 or len(second) < 2:
        return None
    difference = float(first.mean() - second.mean())
    error = math.sqrt(first.var(ddof=1) / len(first) + second.var(ddof=1) / len(second))
    if error == 0:
        return 0.0 if difference == 0 else math.copysign(1e6, difference)
    return difference / error


def position_nll(model, tokens):
    """Negative log-likelihood of each next token of `tokens` (one value per input position)."""
    import mlx.core as mx
    import numpy as np
    logits = model(mx.array([tokens[:-1]], dtype=mx.int32))[0].astype(mx.float32)
    targets = mx.array(tokens[1:], dtype=mx.int32)
    nll = mx.logsumexp(logits, axis=-1) - mx.take_along_axis(logits, targets[:, None], axis=-1)[:, 0]
    mx.eval(nll)
    return np.array(nll)


def router_logits(layer):
    """(owner, key) of the module producing a MoE layer's router logits, or None."""
    mlp = getattr(layer, "mlp", None)
    if hasattr(mlp, "switch_mlp") and hasattr(mlp, "gate"):
        return mlp, "gate"
    router = getattr(layer, "router", None)
    if router is not None and hasattr(router, "proj"):
        return router, "proj"
    return None


@contextlib.contextmanager
def ablated(model, kind, index, unit):
    """Run with an MLP neuron silenced ("neuron") or an expert removed from routing ("expert").

    `unit` is one neuron or expert removed at every position, or a boolean [positions, units] array
    marking which to remove at which input position. A silenced neuron's input to down_proj is zero.
    A removed expert's router logit is the dtype's minimum, so its tokens go to their next choice, as
    if the expert were absent there.
    """
    import mlx.core as mx
    from mlx import nn
    layer = steering.layer_list(model)[index]
    chosen = None if isinstance(unit, int) else mx.array(unit)

    def removed(width):
        return mx.arange(width) == unit if chosen is None else chosen
    if kind == "neuron":
        owner, key = layer.mlp, "down_proj"

        def change(inner, x):
            return inner(x * mx.logical_not(removed(x.shape[-1])).astype(x.dtype))
    else:
        found = router_logits(layer)
        require(found is not None, f"layer {index} has no router")
        owner, key = found

        def change(inner, x):
            out = inner(x)
            return mx.where(removed(out.shape[-1]), mx.array(mx.finfo(out.dtype).min, out.dtype), out)

    class Patched(nn.Module):
        def __init__(self, inner):
            super().__init__()
            self["inner"] = inner

        def __getattr__(self, name):
            try:
                return super().__getattr__(name)
            except AttributeError:
                if "inner" not in self:
                    raise
                return getattr(self["inner"], name)

        def __call__(self, x, *args, **kwargs):
            return change(lambda value: self.inner(value, *args, **kwargs), x)
    original = getattr(owner, key)
    setattr(owner, key, Patched(original))
    try:
        yield
    finally:
        setattr(owner, key, original)


def next_token_logprobs(model, tokens):
    """Log-probabilities over the vocabulary after each input position of tokens[:-1]."""
    import mlx.core as mx
    logits = model(mx.array([tokens[:-1]], dtype=mx.int32))[0].astype(mx.float32)
    return logits - mx.logsumexp(logits, axis=-1, keepdims=True)


def output_reference(logprobs, tokens):
    """The unchanged model's output to compare against: per position, the ids and log-probabilities
    of its TOP_LOGITS likeliest tokens and the log-probability of the actual next token."""
    import mlx.core as mx
    import numpy as np
    count = min(TOP_LOGITS, int(logprobs.shape[-1]))
    ids = mx.argpartition(-logprobs, kth=count - 1, axis=-1)[:, :count]
    top = mx.take_along_axis(logprobs, ids, axis=-1)
    target = mx.take_along_axis(logprobs, mx.array(tokens[1:], dtype=mx.int32)[:, None], axis=-1)[:, 0]
    mx.eval(ids, top, target)
    return {"ids": np.array(ids), "top": np.array(top), "target": np.array(target), "actual": list(tokens[1:])}


def output_change(logprobs, reference):
    """Per position, how far a changed model's next-token distribution moved from `reference`, and
    the actual next token's log-probability under the change.

    The distance is KL(reference ‖ changed) over the reference's likeliest tokens with every other
    token merged into one bucket: a lower bound on the full KL that needs no full-vocabulary copy.
    """
    import mlx.core as mx
    import numpy as np
    ids, before = mx.array(reference["ids"]), mx.array(reference["top"])
    after = mx.take_along_axis(logprobs, ids, axis=-1)
    rest_before = mx.maximum(1 - mx.exp(before).sum(axis=-1), 1e-12)
    rest_after = mx.maximum(1 - mx.exp(after).sum(axis=-1), 1e-12)
    divergence = (mx.exp(before) * (before - after)).sum(axis=-1) + \
        rest_before * (mx.log(rest_before) - mx.log(rest_after))
    target = mx.take_along_axis(logprobs, mx.array(reference["actual"], dtype=mx.int32)[:, None], axis=-1)[:, 0]
    mx.eval(divergence, target)
    return np.maximum(np.array(divergence), 0.0), np.array(target)


def causal_record(model, texts, keys):
    """One unchanged pass per text: the output reference (with the actual next tokens) and what each
    part's layer sees. readings[(kind, layer)][n] is |activation| [positions, neurons] for a neuron
    layer or router probabilities [positions, experts] for an expert layer, as float32."""
    import mlx.core as mx
    import numpy as np
    layers = steering.layer_list(model)
    neuron_layers = sorted({layer for kind, layer, _ in keys if kind == "neuron"})
    expert_layers = sorted({layer for kind, layer, _ in keys if kind == "expert"})
    routers = {index: moe_router(layers[index]) for index in expert_layers}
    require(all(routers.values()), "a checked expert's layer has no readable router")
    references, readings = [], {}
    with contextlib.ExitStack() as stack:
        neurons = stack.enter_context(layer_inputs(model, neuron_layers, lambda layer: (layer.mlp, "down_proj")))
        experts = stack.enter_context(layer_inputs(model, expert_layers, lambda layer: moe_router(layer)[:2]))
        for tokens in texts:
            reference = output_reference(next_token_logprobs(model, tokens), tokens)
            references.append(reference)
            for index, value in neurons.items():
                magnitude = mx.abs(value[0].astype(mx.float32))
                mx.eval(magnitude)
                readings.setdefault(("neuron", index), []).append(np.array(magnitude))
            for index, value in experts.items():
                probs = mx.softmax(routers[index][2](value).astype(mx.float32), axis=-1, precise=True)
                mx.eval(probs)
                readings.setdefault(("expert", index), []).append(np.array(probs).reshape(-1, probs.shape[-1]))
            neurons.clear()
            experts.clear()
            mx.clear_cache()
    return references, readings, {index: router[3] for index, router in routers.items()}


def expert_fires(probs, unit, top_k, start):
    """Where an expert is chosen (input positions from `start`) and, at each, its control: the other
    chosen expert whose router probability is closest to its own (-1 where there is none)."""
    import numpy as np
    chosen = np.argpartition(-probs, top_k - 1, axis=1)[:, :top_k]
    fires = np.any(chosen == unit, axis=1) & (np.arange(len(probs)) >= start)
    control = np.full(len(probs), -1)
    for position in np.flatnonzero(fires).tolist():
        others = [int(e) for e in chosen[position] if e != unit]
        if others:
            control[position] = min(others, key=lambda e: abs(float(probs[position, e] - probs[position, unit])))
    return fires, control


def part_plan(key, reading, top_k, starts, base=None):
    """Where one part fires in each text and the matched control removed at the same positions.

    Experts: expert_fires. Neurons fire at the strongest 1 - NEURON_FIRING share of positions over
    the texts (firing_masks); their control is the neuron of the same layer whose mean |activation|
    over those positions is closest. `base` (a plan from other texts) supplies the neuron threshold
    and control for continuations.
    """
    import numpy as np
    kind, _, unit = key
    if kind == "expert":
        pairs = [expert_fires(probs, unit, top_k, start) for probs, start in zip(reading, starts)]
        return {"fires": [fires for fires, _ in pairs], "control": [control for _, control in pairs]}
    if base is not None:
        cut = base["cut"]
        fires = [((values[:, unit] >= cut) if cut > 0 else (values[:, unit] > 0)) & (np.arange(len(values)) >= start)
                 for values, start in zip(reading, starts)]
        return {"fires": fires, "control": [base["control"][0]] * len(fires), "cut": cut}
    fires = firing_masks([values[:, unit] for values in reading], 1 - NEURON_FIRING)
    fires = [mask & (np.arange(len(mask)) >= start) for mask, start in zip(fires, starts)]
    count = sum(int(mask.sum()) for mask in fires)
    means = sum(values[mask].sum(axis=0) for values, mask in zip(reading, fires)) / max(count, 1)
    distance = np.abs(means - means[unit])
    distance[unit] = np.inf
    chosen = np.concatenate([values[mask, unit] for values, mask in zip(reading, fires)])
    return {"fires": fires, "control": [int(np.argmin(distance))] * len(fires),
            "cut": float(chosen.min()) if len(chosen) else math.inf}


def removal(fires, units, width):
    """A [positions, width] mask removing `units` (one index, or one per position with -1 for none)
    at the firing positions."""
    import numpy as np
    mask = np.zeros((len(fires), width), dtype=bool)
    positions = np.flatnonzero(fires)
    columns = np.full(len(positions), units) if np.isscalar(units) else np.asarray(units)[positions]
    keep = columns >= 0
    mask[positions[keep], columns[keep]] = True
    return mask


def continuations(model, tokenizer, sources):
    """Each source text followed by the model's greedy continuation of up to CAUSAL_EXTEND_TOKENS
    tokens, or None when it writes fewer than two."""
    from mlx_lm import stream_generate
    from mlx_lm.sample_utils import make_sampler
    out = []
    for tokens in sources:
        written = [int(piece.token) for piece in stream_generate(model, tokenizer, prompt=list(tokens),
                                                                 max_tokens=CAUSAL_EXTEND_TOKENS,
                                                                 sampler=make_sampler(temp=0.0))]
        out.append(list(tokens) + written if len(written) >= 2 and len(tokens) + len(written) <= MAX_TOKENS else None)
    return out


def causal_study(model, tokenizer, encoded, labels, halves, pool, progress=None):
    """Which candidate parts change the model's output where they fire, measured on held-out text.

    `pool` maps (kind, layer, unit) to a claimed category or None, most promising first. Each part is
    removed only at the positions where it fires, and so is its control (part_plan), so other
    positions are touched only through what follows a firing position. A removal's effect is the
    output change at those positions (output_change). With more than CAUSAL_ITEMS candidates, each
    is screened on its strongest firing texts of half 0 and the CAUSAL_ITEMS with the largest
    effect beyond their control are kept. Kept parts are tested on half 1, plus continuations of
    their category's texts when they fire in under CAUSAL_MIN_TEXTS held-out texts. Per part:
    `effect` and `control_effect` (mean KL in nats where it fires, averaged per text), `loss_change`
    (actual next token; negative when removal helps), `texts`, `firing_positions`, `z` (signed-rank
    over texts of effect minus control) and `confirmed` (None with too few texts). With a claim,
    `category_effect` / `elsewhere_effect` are the mean per-text KL over all positions on held-out
    texts of that category / the others, `category_z` their rank-sum z and `category_confirmed`
    (None when either group has under CAUSAL_MIN_GROUP texts). Returns ({key: result}, kept keys).
    """
    import mlx.core as mx
    import numpy as np
    progress = progress or (lambda phase, done, total: None)
    units = [{"tokens": tokens, "label": label, "half": half, "start": 1}
             for (tokens, _), label, half in zip(encoded, labels, halves)]
    references, readings, top_ks = causal_record(model, [unit["tokens"] for unit in units], list(pool))
    starts = [unit["start"] for unit in units]
    plans = {key: part_plan(key, readings[key[:2]], top_ks.get(key[1]), starts) for key in pool}

    def measure(key, numbers):
        kind, layer, unit = key
        plan, rows = plans[key], []
        width = readings[key[:2]][0].shape[1]
        for number in numbers:
            fires = plan["fires"][number]
            if not fires.any():
                continue
            tokens, reference = units[number]["tokens"], references[number]
            changes = []
            for units_removed in (unit, plan["control"][number]):
                with ablated(model, kind, layer, removal(fires, units_removed, width)):
                    changes.append(output_change(next_token_logprobs(model, tokens), reference))
                mx.clear_cache()
            (effect, after), (control, _) = changes
            loss = reference["target"] - after
            measured = np.arange(len(fires)) >= units[number]["start"]
            rows.append({"number": number, "effect": float(effect[fires].mean()),
                         "control": float(control[fires].mean()), "loss": float(loss[fires].mean()),
                         "spread": float(effect[measured].mean()), "positions": int(fires.sum())})
        return rows
    keys, screened = list(pool), {}
    if len(keys) > CAUSAL_ITEMS:
        for done, key in enumerate(keys):
            progress("screening parts", done, len(keys))
            fires = plans[key]["fires"]
            strongest = sorted((n for n, unit in enumerate(units) if unit["half"] == 0 and fires[n].any()),
                               key=lambda n: -int(fires[n].sum()))[:CAUSAL_SCREEN_TEXTS]
            rows = measure(key, strongest)
            screened[key] = float(np.mean([row["effect"] - row["control"] for row in rows])) if rows else -math.inf
        keys = sorted(keys, key=lambda key: -screened[key])[:CAUSAL_ITEMS]
    held_out = [n for n, unit in enumerate(units) if unit["half"] == 1]
    sources = []
    for key in keys:
        if sum(bool(plans[key]["fires"][n].any()) for n in held_out) >= CAUSAL_MIN_TEXTS:
            continue
        claim = pool[key]
        for n, unit in enumerate(units):
            if (unit["label"] == claim if claim else plans[key]["fires"][n].any()) and n not in sources:
                sources.append(n)
    sources = sources[:CAUSAL_EXTEND_TEXTS]
    if sources:
        progress("writing more text for rarely firing parts", 0, 1)
        written = continuations(model, tokenizer, [units[n]["tokens"] for n in sources])
        added = [{"tokens": tokens, "label": units[n]["label"], "half": 1, "start": len(units[n]["tokens"])}
                 for n, tokens in zip(sources, written) if tokens is not None]
        if added:
            more, extra, _ = causal_record(model, [unit["tokens"] for unit in added], keys)
            for key in keys:
                plan = part_plan(key, extra[key[:2]], top_ks.get(key[1]), [unit["start"] for unit in added],
                                 base=plans[key])
                plans[key]["fires"] += plan["fires"]
                plans[key]["control"] += plan["control"]
            held_out += list(range(len(units), len(units) + len(added)))
            units += added
            references += more
    results = {}
    for done, key in enumerate(keys):
        progress("checking parts causally", done, len(keys))
        rows = measure(key, held_out)
        z = signed_rank_z([row["effect"] - row["control"] for row in rows])
        result = {"effect": float(np.mean([row["effect"] for row in rows])) if rows else None,
                  "control_effect": float(np.mean([row["control"] for row in rows])) if rows else None,
                  "loss_change": float(np.mean([row["loss"] for row in rows])) if rows else None,
                  "texts": len(rows), "firing_positions": sum(row["positions"] for row in rows),
                  "continued_texts": sum(units[row["number"]]["start"] > 1 for row in rows), "z": z,
                  "confirmed": bool(z is not None and z >= CAUSAL_Z) if len(rows) >= CAUSAL_MIN_TEXTS else None}
        if key in screened:
            result["screen_margin"] = screened[key]
        if key[0] == "neuron":
            result["control_neuron"] = plans[key]["control"][0]
        claim = pool[key]
        if claim:
            spread = {row["number"]: row["spread"] for row in rows}
            inside = [spread.get(n, 0.0) for n in held_out if units[n]["label"] == claim]
            outside = [spread.get(n, 0.0) for n in held_out if units[n]["label"] != claim]
            category_z = rank_sum_z(inside, outside)
            enough = min(len(inside), len(outside)) >= CAUSAL_MIN_GROUP and category_z is not None
            result.update(category=claim, category_effect=float(np.mean(inside)) if inside else None,
                          elsewhere_effect=float(np.mean(outside)) if outside else None, category_z=category_z,
                          category_confirmed=bool(category_z >= CAUSAL_Z) if enough else None)
        results[key] = result
    return results, keys


def part_key(item):
    """(kind, layer, unit) of an expert or neuron record."""
    if "expert" in item:
        return "expert", item["layer"], item["expert"]
    return "neuron", item["layer"], item["neuron"]


def explanation_trusted(item):
    """An explanation beats the neutral control (floored at chance) by EXPLAIN_TRUSTED and tells
    look-alikes apart at least EXPLAIN_HARD of the time."""
    score, control, hard = item.get("explanation_score"), item.get("control_score"), item.get("hard_negative_score")
    return (score is not None and score - max(control or 0.0, 0.0) >= EXPLAIN_TRUSTED
            and (hard is None or hard >= EXPLAIN_HARD))


def firing_masks(trace, share):
    """Per-text masks marking the `share` of non-BOS positions where a part is strongest (exactly
    that many, so tied activations cannot make it fire everywhere)."""
    import numpy as np
    flat = np.concatenate([values[1:] for values in trace])
    order = np.argsort(-flat, kind="stable")[:max(1, math.ceil(share * len(flat)))]
    chosen = np.zeros(len(flat), dtype=bool)
    chosen[order] = True
    masks, start = [], 0
    for values in trace:
        masks.append(np.concatenate([[False], chosen[start:start + len(values) - 1]]))
        start += len(values) - 1
    return masks


def judged(item, causal):
    """Records a part's checks (None: untested); `confirmed` needs a causal check and every check passing."""
    checks = {}
    if causal is not None:
        item["causal"] = causal
        checks["causal"] = causal["confirmed"]
        if "category_confirmed" in causal:
            checks["category"] = causal["category_confirmed"]
    if item.get("replicated") is not None:
        checks["replicated"] = bool(item["replicated"])
    if item.get("explanation"):
        checks["explanation"] = explanation_trusted(item)
    item["checks"] = checks
    item["confirmed"] = "causal" in checks and all(value is True for value in checks.values())


def check_note(item):
    """A short clause saying whether a part passed its checks and, if not, which failed or went untested."""
    checks = item.get("checks") or {}
    if "causal" not in checks:
        return ""
    if item.get("confirmed"):
        return " (confirmed by every check)"
    reasons = [CHECK_FAILURES[name] for name, passed in checks.items() if passed is False]
    reasons += [CHECK_UNTESTED[name] for name, passed in checks.items() if passed is None]
    return " (not confirmed: " + "; ".join(reasons) + ")"


def listed_layers(values, limit=3):
    values = list(values)[:limit]
    return ", ".join(str(value) for value in values[:-1]) + (" and " if len(values) > 1 else "") + str(values[-1]) \
        if values else "none"


def findings(result):
    """Plain-English sentences summarising an analyze result, most informative first."""
    rows, summary = result["layers"], result["summary"]
    out = []
    ranked = sorted(rows, key=lambda row: -row["importance"])
    if ranked:
        top = ranked[0]
        out.append(f"Layers {listed_layers(summary['most_important_layers'])} matter most: skipping layer "
                   f"{top['layer']} raises the probe loss by {top['importance']:.2f} (baseline "
                   f"{result['baseline_loss']:.2f}). Layers {listed_layers(summary['least_important_layers'])} "
                   "matter least.")
    special = {}
    for row in rows:
        if row.get("specialty"):
            special.setdefault(row["specialty"], []).append(row["layer"])
    for category, found in sorted(special.items(), key=lambda item: -len(item[1])):
        out.append(f"{category.capitalize()} text depends on layer{'s' if len(found) > 1 else ''} "
                   f"{listed_layers(found, 5)} far more than other text does.")
    forms = summary.get("answer_forms_at") or {}
    if forms:
        known = {category: layer for category, layer in forms.items() if layer is not None}
        if known and len(set(known.values())) == 1:
            out.append(f"For every kind of text the final prediction is among the top {LENS_TOP} guesses only "
                       f"from layer {next(iter(known.values()))} (logit lens); earlier hidden states do not "
                       "read out through the output embedding, so the lens cannot place the work earlier.")
        elif known:
            early, late = min(known, key=known.get), max(known, key=known.get)
            out.append(f"The prediction forms earliest for {early} text (layer {known[early]}) and latest for "
                       f"{late} text (layer {known[late]}); logit lens: the final prediction is among the "
                       f"layer's top {LENS_TOP} guesses at half of positions.")
    roles = summary.get("heads_by_role") or {}
    named = [f"{count} {role}" for role, count in roles.items() if role != "mixed" and count]
    leaning = summary.get("mixed_heads_lean") or {}
    lean = "; they spread attention, leaning toward " + ", ".join(
        f"{role} ({count})" for role, count in sorted(leaning.items(), key=lambda item: -item[1])) if leaning else ""
    if named or roles.get("mixed"):
        out.append(f"Attention heads: {', '.join(named) or 'none with one clear job'}; {roles.get('mixed', 0)} mix "
                   f"several patterns{lean}.")
    induction = summary.get("induction_heads") or []
    if induction:
        best = induction[0]
        out.append(f"Head {best['head']} of layer {best['layer']} is the strongest copy (induction) head: "
                   "it looks back to what followed an earlier copy of the current token, which drives "
                   "repeating text.")
    types = summary.get("attention_types") or {}
    if len(types) > 1:
        out.append("Attention spans: " + ", ".join(
            f"{value['layers']} {kind} layers (mean importance {value['mean_importance']:.2f})"
            for kind, value in types.items()) + ".")
    routing = summary.get("routing") or {}
    for item in (routing.get("specialists") or [])[:3]:
        out.append(f"Expert {item['expert']} of layer {item['layer']} handles {item['category']} text "
                   f"{item['lift']:.1f}x more than its overall share{check_note(item)}.")
    if routing.get("unused_experts"):
        out.append(f"{routing['unused_experts']} expert slots across {routing['moe_layers']} MoE layers were never "
                   "chosen on the probe text.")
    for neuron in (summary.get("notable_neurons") or [])[:3]:
        context = (neuron.get("contexts") or [{}])[0].get("text")
        line = f"Neuron {neuron['neuron']} of layer {neuron['layer']}"
        if neuron.get("fires_on"):
            line += f" fires mostly on {neuron['fires_on']} text"
        if context:
            line += f" (strongest at {context!r})"
        if neuron.get("promotes"):
            line += f" and pushes toward {', '.join(repr(token) for token in neuron['promotes'][:3])}"
        out.append(line + check_note(neuron) + ".")
    checked = summary.get("checked_parts") or []

    def named(item):
        return (f"Expert {item['expert']}" if item["kind"] == "expert" else f"Neuron {item['neuron']}") + \
            f" of layer {item['layer']}"
    for item in [item for item in checked if item["confirmed"]][:6]:
        causal = item["causal"]
        label = f"\"{item['explanation']}\"" if item.get("explanation") else f"{item['category']} text"
        line = (f"Confirmed: {named(item)}, {label}. Removing it where it fires moves the next-token distribution "
                f"by {causal['effect']:.4f} nats against {causal['control_effect']:.4f} for an equally active part, "
                f"consistently over {causal['texts']} held-out texts (z {causal['z']:.1f})")
        if causal.get("category_confirmed"):
            line += (f", and changes {causal['category']} text more than other text ({causal['category_effect']:.4f} "
                     f"against {causal['elsewhere_effect']:.4f} nats per token)")
        if item.get("explanation_score") is not None:
            line += (f"; its explanation ranks held-out snippets at {item['explanation_score']:.2f} (neutral "
                     f"description {item.get('control_score') or 0:.2f}")
            if item.get("hard_negative_score") is not None:
                line += f", look-alike snippets told apart {item['hard_negative_score']:.0%} of the time"
            line += ")"
        out.append(line + ".")
    strongest = sorted((item for item in checked if (item.get("causal") or {}).get("effect") is not None),
                       key=lambda item: -item["causal"]["effect"])[:3]
    if strongest:
        out.append("Largest measured effects: " + "; ".join(
            f"{named(item)} ({item['causal']['effect']:.4f} nats where it fires vs "
            f"{item['causal']['control_effect']:.4f} for its control; removing it makes the actual next token "
            f"{'less' if item['causal']['loss_change'] > 0 else 'more'} likely, loss "
            f"{item['causal']['loss_change']:+.3f})" for item in strongest) + ".")
    if checked:
        untested = sum((item.get("checks") or {}).get("causal", False) is None for item in checked)
        out.append(f"{sum(item['confirmed'] for item in checked)} of {len(checked)} checked parts passed every check "
                   "that applies (removing it where it fires changes the output more than removing an equally active "
                   "part, on held-out text; its category and explanation hold up)"
                   + (f"; {untested} fire in too few held-out texts to test" if untested else "")
                   + "; the others are leads, not established.")
    writes = summary.get("writes") or {}
    if writes.get("most_helpful_layers"):
        helpful = writes["most_helpful_layers"]
        top = next((row for row in rows if row["layer"] == helpful[0]), {}).get("writes") or {}
        line = (f"Layers {listed_layers(helpful)} raise the probability of the right next token the most in the "
                "read-out")
        if top.get("promotes"):
            line += (f"; layer {helpful[0]} most often pushes up "
                     f"{', '.join(repr(item['token']) for item in top['promotes'][:3])}")
        out.append(line + ".")
    if writes.get("most_harmful_layers"):
        out.append(f"Layers {listed_layers(writes['most_harmful_layers'])} lower it in the read-out; the lens shows "
                   "only their direct effect, not what they prepare for later layers.")
    memory = summary.get("memory") or {}
    if memory:
        longest = memory["longest"]
        out.append(f"Linear attention: across {memory['heads']} heads in {memory['layers']} layers the median memory "
                   f"half-life is {memory['median_half_life']:.0f} tokens; {memory['long_heads']} heads keep half "
                   f"their state for {MEMORY_LONG:.0f}+ tokens (longest: head {longest['head']} of layer "
                   f"{longest['layer']}, {longest['half_life']:.0f} tokens) and {memory['short_heads']} forget within "
                   f"{MEMORY_SHORT:.0f}.")
    structure = summary.get("structure") or {}
    if structure.get("lowest_rank"):
        low = structure["lowest_rank"]
        out.append(f"The {low['matrix']} matrix of layer {low['layer']} uses the fewest directions "
                   f"(effective rank {low['effective_rank']:.0%} of its size).")
    if structure.get("outlier_dimensions"):
        dims = structure["outlier_dimensions"]
        out.append(f"Residual dimension{'s' if len(dims) > 1 else ''} "
                   f"{listed_layers([item['dimension'] for item in dims], 3)} carry outsized weights in many "
                   f"matrices ({dims[0]['matrices']} for dimension {dims[0]['dimension']}); such outlier "
                   "features are common and edits there have wide effects.")
    return out


def analyze(request, root, config):
    """An atlas of what the model's parts do, found by probing it with labelled built-in text.

    Per layer: how much the probe loss rises when the layer is skipped (`importance`, overall and per
    text category, with the category it serves most as `specialty`), how close its output is to the
    final prediction (logit lens), the attention heads whose pattern is a recognisable job, the most
    active MLP neurons with the tokens each writes toward and the snippets that fire it, expert
    routing (with experts that favour a category) and the spectrum of its weight matrices.
    `findings` restates the clearest results in plain words. The labels describe patterns on a few
    short texts: leads for `attribute`, not proof.
    """
    import numpy as np
    model, tokenizer = steering.load(request)
    layers = steering.layer_list(model)
    encoded, labels = category_encoded(tokenizer)
    notes = []
    baseline, base_categories, answer_tokens = category_loss(model, encoded, labels)
    # Observation passes read the longer labelled text; layer skipping above keeps the short set.
    observed_text, observed_labels = observation_encoded(tokenizer)
    observed_halves = text_halves(observed_labels)
    rows = [{"layer": index} for index in range(len(layers))]
    for index in range(len(layers)):
        report(request, "scoring layers", index, len(layers))
        with skipped(model, index):
            value, categories, _ = category_loss(model, encoded, labels)
        by_category = {label: categories[label] - base_categories[label] for label in base_categories}
        rows[index].update(importance=value - baseline, importance_by_category=by_category,
                           specialty=specialty(by_category),
                           stage=("early", "middle", "late")[min(2, 3 * index // len(layers))])
        kind = attention_type(layers[index])
        if kind is not None:
            rows[index]["attention_type"] = kind
    report(request, "reading the logit lens", 0, 1)
    forms = {}
    try:
        lens = logit_lens(model, tokenizer, observed_text, observed_labels)
        for index, value in lens.items():
            if "writes" in value:
                rows[index]["writes"] = value.pop("writes")
            rows[index]["lens"] = value
        for label in base_categories:
            forms[label] = next((index for index in range(len(layers))
                                 if lens[index]["top10_by_category"].get(label, 0) >= LENS_FORMED), None)
    except (WeightsError, ValueError) as error:
        notes.append(f"logit lens not read: {error}")
    standard = []
    for index in range(len(layers)):
        try:
            attention_heads(model, index)
            standard.append(index)
        except WeightsError:
            rows[index]["attention"] = "other"
    if len(standard) < len(layers):
        notes.append(f"{len(layers) - len(standard)} layers have no standard multi-head attention "
                     "(linear attention or similar) and their heads are not classified")
    report(request, "reading attention heads", 0, 1)
    try:
        behaviour = head_behaviour(model, observed_text, standard)
    except (WeightsError, ValueError) as error:
        behaviour = {}
        notes.append(f"attention heads not classified: {error}")
    heads_seen = {}
    for index, values in behaviour.items():
        notable, counts = [], {}
        for head, measured in enumerate(values):
            role = head_role(measured["previous"], measured["sink"], measured["induction"], measured["self"],
                             measured["duplicate"], measured["local"], measured["far"])
            counts[role] = counts.get(role, 0) + 1
            if role != "mixed":
                notable.append({"head": head, "role": role, **measured})
            heads_seen[(index, head)] = (role, measured["induction"] or 0.0,
                                         head_leaning(measured) if role == "mixed" else None)
        rows[index].update(attention="standard", head_count=len(values), head_roles=counts, heads=notable)
    mlp_layers = [index for index, layer in enumerate(layers)
                  if hasattr(getattr(layer, "mlp", None), "down_proj")]
    if len(mlp_layers) < len(layers):
        notes.append(f"{len(layers) - len(mlp_layers)} layers have no dense MLP (mixture-of-experts); "
                     "their neurons are not listed")
    report(request, "reading neurons", 0, 1)
    listed = []
    try:
        activity = neuron_activity(model, observed_text, mlp_layers)
        for index, values in activity.items():
            top = np.argsort(-values)[:ANALYZE_NEURONS].tolist()
            rows[index]["neurons"] = [{"layer": index, "neuron": item, "activity": float(values[item])} for item in top]
            listed += rows[index]["neurons"]
        try:
            neuron_effects(model, tokenizer, layers, listed)
        except (WeightsError, ValueError) as error:
            notes.append(f"neuron tokens unavailable: {error}")
        report(request, "finding what fires neurons", 0, 1)
        try:
            neuron_contexts(model, tokenizer, observed_text, observed_labels, observed_halves, listed)
        except (WeightsError, ValueError) as error:
            notes.append(f"neuron contexts unavailable: {error}")
    except (WeightsError, ValueError) as error:
        notes.append(f"neurons not read: {error}")
    moe_layers = [index for index, layer in enumerate(layers) if moe_router(layer) is not None]
    linear_layers = [index for index, layer in enumerate(layers) if getattr(layer, "linear_attn", None) is not None]
    routed, profiles, observed = {}, {}, {}
    if moe_layers or linear_layers:
        report(request, "reading routing", 0, 1)
        try:
            routed, measured, observed = router_read(model, observed_text, moe_layers, linear_layers, observed_labels,
                                                     observed_halves)
            for index, value in routed.items():
                rows[index]["experts"] = value
            for index, value in measured.items():
                rows[index]["linear_attention"] = value
            if observed:
                report(request, "profiling experts", 0, 1)
                profiles = expert_profiles(model, tokenizer, observed_text, observed_labels, observed_halves, routed,
                                           observed)
                for index, value in profiles.items():
                    rows[index]["experts"]["profiles"] = value
        except (WeightsError, ValueError) as error:
            notes.append(f"routing not read: {error}")
    structure_summary = {}
    try:
        structure, dimensions = weight_structure(model, request)
        for index, value in structure.items():
            rows[index]["structure"] = value
        lowest = min(((index, role, value["effective_rank"]) for index, parts in structure.items()
                      for role, value in parts.items()), key=lambda item: item[2], default=None)
        if lowest is not None:
            structure_summary["lowest_rank"] = {"layer": lowest[0], "matrix": lowest[1], "effective_rank": lowest[2]}
        structure_summary["outlier_dimensions"] = dimensions
    except (WeightsError, ValueError) as error:
        notes.append(f"weight structure not read: {error}")
    notable = sorted((row for row in listed if row.get("fires_on")), key=lambda row: -row["activity"]) \
        or sorted(listed, key=lambda row: -row["activity"])
    ranked_profiles = sorted(((index, profile) for index, values in profiles.items() for profile in values),
                             key=lambda pair: (pair[1]["category"] is None, -pair[1]["share"]))
    special = sorted((dict(item, layer=index) for index, value in routed.items() for item in value["specialists"]),
                     key=lambda item: -item["lift"])
    # Causal candidates by cheap signals (replicated specialists, other specialists, profiled experts
    # with a category, the busiest; neurons with a favoured category, the most active), alternating
    # kinds with at most two per layer and kind; causal_study keeps those with the largest effect.
    claim_of, experts_ranked = {}, []
    for item in sorted(special, key=lambda item: (not item.get("replicated"), -item["lift"])):
        experts_ranked.append(("expert", item["layer"], item["expert"]))
        claim_of.setdefault(experts_ranked[-1], item["category"])
    for index, profile in ranked_profiles:
        experts_ranked.append(("expert", index, profile["expert"]))
        claim_of.setdefault(experts_ranked[-1], profile.get("category"))
    neurons_ranked = [("neuron", row["layer"], row["neuron"])
                      for row in sorted(listed, key=lambda row: (row.get("fires_on") is None, -row["activity"]))]
    claim_of.update({("neuron", row["layer"], row["neuron"]): row.get("fires_on") for row in listed})
    pool, per_layer = {}, {}
    for position in range(max(len(experts_ranked), len(neurons_ranked))):
        for ranked in (experts_ranked, neurons_ranked):
            key = ranked[position] if position < len(ranked) else None
            if key is None or key in pool or len(pool) >= CAUSAL_POOL or per_layer.get(key[:2], 0) >= 2:
                continue
            per_layer[key[:2]] = per_layer.get(key[:2], 0) + 1
            pool[key] = claim_of.get(key)
    causal, selected = {}, list(pool)[:CAUSAL_ITEMS]
    if pool:
        report(request, "checking parts causally", 0, len(pool))
        try:
            causal, selected = causal_study(model, tokenizer, observed_text, observed_labels, observed_halves, pool,
                                            lambda phase, done, total: report(request, phase, done, total))
        except (WeightsError, ValueError) as error:
            notes.append(f"causal checks unavailable: {error}")
    # Self-explanation of the kept parts, the largest effects first.
    items_by_key = {part_key(item): item for item in listed + [profile for _, profile in ranked_profiles]}
    explained = []
    for kind, index, unit in selected:
        item = items_by_key.get((kind, index, unit))
        if item is None or not (item.get("evidence") and item.get("_test")) or \
                len(explained) >= EXPLAIN_EXPERTS + EXPLAIN_NEURONS:
            continue
        item["what"] = (f"expert {unit} of the mixture-of-experts block in layer {index}" if kind == "expert"
                        else f"neuron {unit} of the MLP in layer {index}")
        explained.append(item)
    asks = []
    if explained and request.get("explainer") == "external":
        asks = [explanation_request(item) for item in explained]
    elif explained and request.get("explain", True):
        report(request, "asking the model to explain its parts", 0, len(explained))
        try:
            explain(model, tokenizer, explained)
        except (WeightsError, ValueError, TypeError) as error:
            notes.append(f"self-explanations unavailable: {error}")
    every_profile = [profile for values in profiles.values() for profile in values]
    for item in listed + every_profile + special:
        judged(item, causal.get(part_key(item)))
    for item in listed + every_profile:
        for key in ("_test", "what", "evidence"):
            item.pop(key, None)
    report(request, "analyzed", 1, 1)
    ranked = sorted(rows, key=lambda row: -row["importance"])
    induction = sorted(((key, value[1]) for key, value in heads_seen.items() if value[0] == "induction"),
                       key=lambda item: -item[1])
    summary = {"most_important_layers": [row["layer"] for row in ranked[:5]],
               "least_important_layers": [row["layer"] for row in ranked[-5:]],
               "categories": {label: {"baseline_loss": value} for label, value in base_categories.items()},
               "induction_heads": [{"layer": key[0], "head": key[1], "induction": score}
                                   for key, score in induction[:10]],
               "heads_by_role": {role: sum(1 for value in heads_seen.values() if value[0] == role)
                                 for role in HEAD_ROLES}}
    leaning = [value[2] for value in heads_seen.values() if value[2]]
    if leaning:
        summary["mixed_heads_lean"] = {role: leaning.count(role) for role in HEAD_ROLES if role in leaning}
    if forms:
        summary["answer_forms_at"] = forms
    types = {}
    for row in rows:
        if row.get("attention_type"):
            types.setdefault(row["attention_type"], []).append(row["importance"])
    if types:
        summary["attention_types"] = {kind: {"layers": len(values), "mean_importance": float(np.mean(values))}
                                      for kind, values in types.items()}
    if routed:
        summary["routing"] = {"moe_layers": len(routed),
                              "mean_entropy": float(np.mean([value["entropy"] for value in routed.values()])),
                              "unused_experts": int(sum(value["unused"] for value in routed.values())),
                              "specialists": special[:10]}
    summary["notable_neurons"] = [{key: row[key] for key in ("layer", "neuron", "activity", "fires_on", "replicated",
                                                              "promotes", "contexts", "explanation",
                                                              "explanation_score", "control_score",
                                                              "hard_negative_score", "causal", "checks", "confirmed")
                                   if key in row} for row in notable[:5]]
    fields = PART_FIELDS
    summary["explanations"] = explanation_summary(listed, every_profile)
    checked, seen = [], set()
    for item in summary["explanations"] + [dict(item, kind="expert") for item in special] + \
            [dict(row, kind="neuron", category=row.get("fires_on")) for row in listed]:
        key = part_key(item)
        if "causal" in item and key not in seen:
            seen.add(key)
            checked.append({name: item[name] for name in ("kind", "expert", "neuron") + fields if name in item})
    summary["checked_parts"] = sorted(checked, key=lambda item: not item["confirmed"])
    spans = [(index, head, value) for index, row in enumerate(rows)
             for head, value in enumerate(((row.get("linear_attention") or {}).get("memory") or {}).get("half_life", []))]
    if spans:
        longest = max(spans, key=lambda item: item[2])
        summary["memory"] = {"layers": len({index for index, _, _ in spans}), "heads": len(spans),
                             "median_half_life": float(np.median([value for _, _, value in spans])),
                             "longest": {"layer": longest[0], "head": longest[1], "half_life": longest[2]},
                             "long_heads": sum(value >= MEMORY_LONG for _, _, value in spans),
                             "short_heads": sum(value <= MEMORY_SHORT for _, _, value in spans)}
    gains = sorted((row for row in rows if row.get("writes")), key=lambda row: -row["writes"]["target_gain"])
    if gains:
        summary["writes"] = {"most_helpful_layers": [row["layer"] for row in gains[:3]],
                             "most_harmful_layers": [row["layer"] for row in gains[::-1][:3]
                                                     if row["writes"]["target_gain"] < 0]}
    if structure_summary:
        summary["structure"] = structure_summary
    result = {"probes": len(encoded), "probe_tokens": answer_tokens, "baseline_loss": baseline,
              "observation_texts": len(observed_text),
              "observation_tokens": sum(len(tokens) - 1 for tokens, _ in observed_text),
              "categories": [label for label, _ in CATEGORY_PROBES], "layer_count": len(layers),
              "summary": summary, "layers": rows, **({"notes": notes} if notes else {})}
    if asks:
        result["explanation_requests"] = asks
    elif summary["explanations"]:
        result["explainer"] = "self"
    result["findings"] = findings(result)
    return result
    return result


def neuron_count(layer):
    """Width of the MLP output projection's input: the number of neurons in this layer."""
    down = layer.mlp.down_proj
    weight = down.weight
    scales = getattr(down, "scales", None)
    bits = getattr(down, "bits", None)
    return int(weight.shape[-1] * 32 // bits) if scales is not None and bits else int(weight.shape[-1])


def try_edit(request, root, config):
    """Score an edit in memory at one or more strengths without writing any model file.

    Reports the change in answer loss on `cases` (what the edit should improve) and the KL divergence
    from the intact model on `neutral` answers (what it must not disturb), per dose of the edit.
    """
    import mlx.core as mx
    from dial_worker import encode, reasoning_renderer
    cases = trace_cases(request)
    neutral = trace_cases(request, "neutral", MAX_NEUTRAL_CASES) if request.get("neutral") is not None else []
    doses = request.get("doses", [1.0])
    require(isinstance(doses, list) and 1 <= len(doses) <= MAX_DOSES
            and all(type(d) in (int, float) and math.isfinite(d) and 0 < d <= 2 for d in doses),
            f"doses must be 1-{MAX_DOSES} numbers in (0, 2]; 1 is the edit as specified")
    method = request.get("method", "project_out")
    rows = tensors(root, config)
    plan, _, summary = planned_edit(request, root, rows, config, method)
    model, tokenizer = steering.load(request)
    render = reasoning_renderer(tokenizer)

    def tokens_of(items, key, cap=None):
        encoded = []
        for number, case in enumerate(items):
            tokens, offset = encode(tokenizer, render(case["messages"]), case.get("tools"))
            require(0 < offset < len(tokens) <= MAX_TOKENS,
                    f"{key} {number} has no answer tokens or exceeds {MAX_TOKENS} tokens")
            encoded.append((tokens[:offset + cap] if cap else tokens, offset))
        return encoded
    encoded, quiet = tokens_of(cases, "cases"), tokens_of(neutral, "neutral", NEUTRAL_TOKENS)
    baseline, answer_tokens = loss(model, encoded)
    base_logp = neutral_logprobs(model, quiet) if quiet else []
    shards = {shard: mx.load(str(root / shard)) for shard in plan}
    rows_out, details = [], []
    for position, dose in enumerate(doses):
        report(request, "trying doses", position, len(doses))
        updates, results = {}, []
        for shard, items in plan.items():
            for row, apply in items:
                result, changed = apply(shards[shard], dose)
                updates.update(changed)
                results.append(result)
        with patched(model, updates):
            value, _ = loss(model, encoded)
            drift = divergence(base_logp, neutral_logprobs(model, quiet)) if quiet else None
        noise = max(result["requantization_error"] for result in results)
        rows_out.append({"dose": dose, "loss": value, "delta": value - baseline, "neutral_kl": drift,
                         "edited_params": sum(result["edited_params"] for result in results),
                         "max_requantization_error": noise, "noisy": noise > MAX_REQUANTIZATION})
        details = results
        mx.clear_cache()
    report(request, "tried", len(doses), len(doses))
    shown = [{key: value for key, value in result.items() if key in
              ("tensor", "layer", "scale", "edit_norm", "requantization_error", "edited_params")}
             for result in details]
    return {"method": method, "cases": len(cases), "neutral_cases": len(neutral), "answer_tokens": answer_tokens,
            "baseline_loss": baseline, "doses": rows_out, "tensors": shown, **summary}


def combine(request, root, config):
    """Sum the changes of 2-4 edited copies of the base into one new copy: base plus each edit's delta.

    A source's change to a tensor is its dense value minus the base's; shards a source still hard-links
    to the base contribute nothing. Every changed tensor is requantized in the groups its summed change
    touches, so the output keeps the base's quantization. Each source must have been made from this base
    (its recorded base_id), so edits of another checkpoint are refused.
    """
    import mlx.core as mx
    import numpy as np
    sources = request.get("edits")
    require(isinstance(sources, list) and 2 <= len(sources) <= 4
            and all(isinstance(item, str) and bool(item) for item in sources),
            "edits must list 2-4 edited model directories")
    paths = [Path(item) for item in sources]
    require(all(path.is_dir() and (path / EDIT_RECORD).is_file() for path in paths),
            f"each edits entry must be a model directory written by weights edit (it holds {EDIT_RECORD})")
    require(len({path.resolve() for path in paths}) == len(paths), "edits must be distinct directories")
    output = request.get("output")
    require(isinstance(output, str) and bool(output), "output must be a new model directory")
    out = Path(output)
    require(not out.exists() and out.parent.is_dir(), "output must be a new directory in an existing parent")
    require(root not in out.resolve().parents, "output must be outside the base model directory")
    allow_noisy = request.get("allow_noisy", False)
    require(type(allow_noisy) is bool, "allow_noisy must be true or false")
    base_id = steering.identity(root)
    for path in paths:
        record = json.loads((path / EDIT_RECORD).read_text())
        require(isinstance(record, dict) and record.get("base_id") == base_id,
                f"{path.name} was not made from this base model; combine only sums edits of the same base")
    rows = tensors(root, config)
    names = [path.name for path in shards(root)]
    for path in paths:
        require(sorted(item.name for item in shards(path)) == names,
                f"{path.name} has different safetensors files than the base model")
    total = sum(row["params"] for row in rows)
    stage = Path(tempfile.mkdtemp(prefix=f".{out.name}.partial-", dir=out.parent))
    try:
        changes, rewritten_shards = [], set()
        for number, shard in enumerate(names):
            report(request, "combining shards", number, len(names))
            base_path = root / shard
            sourced = [path / shard for path in paths if not os.path.samefile(path / shard, base_path)]
            if not sourced:
                continue
            arrays, shard_metadata = mx.load(str(base_path), return_metadata=True)
            loaded = [mx.load(str(item)) for item in sourced]
            changed_here = False
            for row in (row for row in rows if row["shard"] == shard):
                base_dense, form = dense_weight(arrays, row)
                summed = None
                for source in loaded:
                    delta = dense_weight(source, row)[0] - base_dense
                    summed = delta if summed is None else summed + delta
                if summed is None:
                    continue
                changed = np.array(mx.abs(summed) > 0)
                if not changed.any():
                    continue
                edited = base_dense + summed
                touched = None
                if row["quantized"]:
                    group = row["quantization"]["group_size"]
                    require(base_dense.shape[-1] % group == 0,
                            f"{row['name']}: packed layout does not split into quantization groups")
                    touched = changed.reshape(*base_dense.shape[:-1], base_dense.shape[-1] // group, group).any(axis=-1)
                updates, stored, rewritten = store_edit(arrays, row, form, edited, touched, int(changed.sum()))
                mx.eval(stored, *updates.values())
                arrays.update(updates)
                size, error = [float(value) for value in mx.stack([mx.sqrt(mx.sum(summed * summed)),
                                                                  mx.sqrt(mx.sum((stored - edited) ** 2))]).tolist()]
                noise = error / size if size > 0 else 0.0
                if not allow_noisy and noise > MAX_REQUANTIZATION:
                    raise WeightsError(
                        f"{row['name']}: requantizing to {row['quantization']['bits']} bits changes the weights "
                        f"{noise:.2f}x as much as the summed edits, so the combination would be mostly rounding "
                        "noise. Combine edits of an 8-bit or full-precision copy, or pass allow_noisy to accept it")
                changes.append({"tensor": row["name"], "edit_norm": size, "requantization_error": noise,
                                "quantized": row["quantized"], "edited_params": int(changed.sum()),
                                "rewritten_params": rewritten})
                changed_here = True
            if changed_here:
                mx.save_safetensors(str(stage / shard), arrays, metadata=shard_metadata)
                rewritten_shards.add(shard)
            del arrays, loaded
            mx.clear_cache()
        linked, copied = link_tree(root, stage, rewritten_shards)
        record = {"version": VERSION, "method": "combine", "base": str(root), "base_id": base_id,
                  "sources": [str(path) for path in paths], "edits": rounded(changes)}
        steering.atomic_json(stage / EDIT_RECORD, record)
        os.rename(stage, out)
    except BaseException:
        shutil.rmtree(stage, ignore_errors=True)
        raise
    report(request, "combined", len(names), len(names))
    rewritten = sum(change["rewritten_params"] for change in changes)
    return {"method": "combine", "output": str(out), "output_id": steering.identity(out),
            "sources": [str(path) for path in paths], "changes": changes,
            "edited_params": sum(change["edited_params"] for change in changes), "rewritten_params": rewritten,
            "total_params": total, "fraction_changed": rewritten / total if total else 0.0,
            "quantized": any(change["quantized"] for change in changes), "shards_rewritten": len(rewritten_shards),
            "files_linked": linked, "files_copied": copied}


OPERATIONS = {"weights_map": weight_map, "weights_stats": stats,
              "weights_activations": activations, "weights_trace": trace,
              "weights_generate": generate, "weights_direction": direction, "weights_edit": edit,
              "weights_try": try_edit, "weights_attribute": attribute, "weights_analyze": analyze,
              "weights_combine": combine, "weights_investigate": investigate, "weights_explained": finish_explanations}
# Per-tensor rows returned (Rust stores the same cap); keeps stdout under the worker output limit.
MAX_TENSOR_ROWS = 4096


def operation(request):
    os.environ["HF_HUB_OFFLINE"] = "1"
    name = request.get("op")
    require(name in OPERATIONS, "unsupported weights operation")
    model = request.get("model")
    require(isinstance(model, str) and bool(model), "model must be a local model directory or downloaded model id")
    if not Path(model).is_dir():
        from mlx_lm.utils import _download
        model = str(_download(model).resolve(strict=True))
    root = Path(model).resolve(strict=True)
    require(root.is_dir() and (root / "config.json").is_file() and any(root.glob("*.safetensors")),
            UNSUPPORTED_FORMAT)
    adapter = request.get("adapter")
    require(adapter is None or (isinstance(adapter, str) and Path(adapter).is_dir()),
            "adapter must be a local adapter directory")
    request = dict(request, model=str(root))
    before = steering.identity(root) if name in ("weights_map", "weights_investigate") else None
    if name == "weights_map" and request.get("known_model_id") == before:
        return {"version": VERSION, "op": "map", "model": str(root),
                "model_id": before, "unchanged": True}
    config = read_config(root)
    result = OPERATIONS[name](request, root, config)
    rows = result.get("tensors")
    if isinstance(rows, list) and len(rows) > MAX_TENSOR_ROWS:
        result.update(tensors=rows[:MAX_TENSOR_ROWS], tensors_total=len(rows), tensors_truncated=True)
    model_id = steering.identity(root)
    require(before is None or before == model_id, "model metadata changed during inspection; retry")
    result.update(version=VERSION, op="analyze" if name == "weights_explained" else name.removeprefix("weights_"),
                  model=str(root),
                  model_id=model_id)
    return rounded(result)
