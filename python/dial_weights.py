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


def weight_map(request, root, config):
    rows = tensors(root, config)
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
               "passed": result["passed"] if steering.contract(case) else None, "seconds": result["seconds"]}
        if tail:
            row["tail"] = result["tail"]
        rows.append(row)
    report(request, "generated", len(cases), len(cases))
    checked = [row["passed"] for row in rows if row["passed"] is not None]
    probes = {}
    if probe:
        value, count = loss(model, probe_encoded(tokenizer))
        probes = {"probe_loss": value, "probe_tokens": count}
    return {"cases": rows, "max_tokens": limit, "summary": {
        "cases": len(rows), "hit_limit": sum(row["hit_limit"] for row in rows),
        "mean_tokens": sum(row["tokens"] for row in rows) / len(rows),
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


def direction(request, root, config):
    """Unit mean difference (positive minus negative) of answer-token activations, per layer.

    `positive` conversations show the behaviour the direction points toward (the one an edit
    removes); `negative` ones show the wanted behaviour. Site `mlp` reads each layer's MLP output,
    `residual` the layer output. The directions are saved to `output` for `weights_edit`.
    """
    import mlx.core as mx
    from dial_worker import encode, reasoning_renderer
    site = request.get("site", "mlp")
    require(site in ("mlp", "residual"), "site must be mlp or residual")
    output = request.get("output")
    require(isinstance(output, str) and output.endswith(".safetensors") and not Path(output).exists()
            and Path(output).parent.is_dir(), "output must be a new .safetensors path in an existing directory")
    groups = {key: conversations(request, key) for key in ("positive", "negative")}
    model, tokenizer = steering.load(request)
    render = reasoning_renderer(tokenizer)
    count = len(steering.layer_list(model))
    total, done, samples = sum(len(items) for items in groups.values()), 0, {}
    for key, items in groups.items():
        rows = []
        for number, item in enumerate(items):
            report(request, "capturing answers", done, total)
            tokens, offset = encode(tokenizer, render(item["messages"]), item.get("tools"))
            require(0 < offset < len(tokens) <= MAX_TOKENS,
                    f"{key} {number} has no answer tokens or exceeds {MAX_TOKENS} tokens")
            sinks = [[] for _ in range(count)]
            with contextlib.ExitStack() as stack:
                for index in range(count):
                    callback = answer_mean(sinks[index], offset)
                    stack.enter_context(mlp_hook(model, index, callback) if site == "mlp"
                                        else steering.hook(model, index, callback))
                model(mx.array([tokens], dtype=mx.int32))
                # Only the captures are evaluated, so the vocabulary projection is never computed.
                mx.eval(*[value for sink in sinks for value in sink])
            require(all(len(sink) == 1 for sink in sinks), "every layer must run exactly once per conversation")
            rows.append(mx.stack([sink[0] for sink in sinks]))
            done += 1
            mx.clear_cache()
        samples[key] = mx.stack(rows)
    positive, negative = samples["positive"], samples["negative"]
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
    staged = Path(output).with_name(f".{Path(output).stem}.partial.safetensors")
    mx.save_safetensors(str(staged), {"directions": unit}, metadata={
        "format": DIRECTION_FORMAT, "site": site, "model_id": steering.identity(root),
        "layers": str(count), "dimension": str(unit.shape[-1])})
    os.replace(staged, output)
    report(request, "measured", total, total)
    ranked = sorted(rows, key=lambda row: -(row["separation"] if row["separation"] is not None else row["relative_gap"]))
    return {"site": site, "positive": len(groups["positive"]), "negative": len(groups["negative"]),
            "dimension": int(unit.shape[-1]), "layer_count": count, "layers": rows,
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


def project_out(arrays, row, vector, columns, alpha):
    """Remove `alpha` of direction d from the `columns` output-projection columns most aligned with it.

    Column j of W is what hidden unit j writes to the residual stream; W[:, j] -= alpha*d*(d.W[:, j]).
    Quantized weights are dequantized, edited and requantized; groups holding no edited column keep
    their original bytes. The requantization error is relative to the size of the intended edit.
    """
    import mlx.core as mx
    import numpy as np
    name = row["name"]
    dense, form = dense_weight(arrays, row)
    require(dense.ndim == 2 and dense.shape[0] == vector.shape[0], f"{name} does not write the direction's hidden size")
    rows_out, width = dense.shape
    require(columns <= width, f"{name} has only {width} columns")
    d = vector / mx.maximum(mx.sqrt(mx.sum(vector * vector)), 1e-12)
    coefficients = d @ dense
    mx.eval(coefficients)
    raw = np.array(coefficients)
    chosen = np.sort(np.argsort(-np.abs(raw), kind="stable")[:columns])
    mask = np.zeros(width, dtype=np.float32)
    mask[chosen] = 1.0
    delta = alpha * d[:, None] * (coefficients * mx.array(mask))[None, :]
    edited = dense - delta
    touched = None
    if row["quantized"]:
        group = row["quantization"]["group_size"]
        require(width % group == 0, f"{name}: packed layout does not split into quantization groups")
        columns_touched = mask.reshape(width // group, group).max(axis=1) > 0
        touched = np.ascontiguousarray(np.broadcast_to(columns_touched, (rows_out, width // group)))
    updates, stored, rewritten = store_edit(arrays, row, form, edited, touched, rows_out * len(chosen))
    change = mx.sqrt(mx.sum(delta * delta))
    values = mx.stack([mx.sqrt(mx.sum(coefficients * coefficients)), mx.sqrt(mx.sum((d @ stored) ** 2)),
                       change, mx.sqrt(mx.sum((stored - edited) ** 2))])
    mx.eval(values, *updates.values())
    before, after, size, error = values.tolist()
    require(all(math.isfinite(value) for value in (before, after, size, error)), f"{name}: edit produced nonfinite values")
    return {"tensor": name, "columns": chosen.tolist(), "projection_before": before, "projection_after": after,
            "edit_norm": size, "requantization_error": error / size if size > 0 else 0.0,
            "edited_params": rows_out * len(chosen), "rewritten_params": rewritten}, updates


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


def planned_projection(request, root, rows):
    """`project_out`: one dense MLP output projection per layer, edited with a measured direction."""
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
    columns = request.get("columns", 64)
    require(type(columns) is int and 1 <= columns <= MAX_EDIT_COLUMNS, f"columns must be 1–{MAX_EDIT_COLUMNS}")
    alpha = request.get("alpha", 1.0)
    require(type(alpha) in (int, float) and math.isfinite(alpha) and 0 < alpha <= 2, "alpha must be in (0, 2]")
    plan = {}
    for layer in sorted(layers):
        row = output_projection(rows, layer)
        width = row["shape"][-1]
        require(columns <= width,
                f"{row['name']} has only {width} columns; use `columns` of at most {width}")

        def apply(arrays, dose=1.0, row=row, layer=layer):
            result, updates = project_out(arrays, row, directions[layer], columns, alpha * dose)
            return dict(result, layer=layer), updates
        plan.setdefault(row["shard"], []).append((row, apply))
    record = {"alpha": alpha, "columns": columns,
              "direction": {"site": metadata.get("site"), "model_id": metadata["model_id"]}}
    return plan, record, {"alpha": alpha, "columns": columns, "site": metadata.get("site")}


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
    require(method in ("project_out", "scale", "steering"), "method must be project_out, scale or steering")
    if method == "project_out":
        return planned_projection(request, root, rows)
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
        if method == "project_out":
            record["layers"] = record.pop("edits")
        steering.atomic_json(stage / EDIT_RECORD, record)
        os.rename(stage, out)
    except BaseException:
        shutil.rmtree(stage, ignore_errors=True)
        raise
    report(request, "edited", count, count)
    rewritten = sum(row["rewritten_params"] for row in edits)
    shown = ([dict(row, columns=row["columns"][:16]) for row in edits] if method == "project_out" else edits)
    # Column indices stay in the edit record; results carry a preview.
    return {"method": method, "output": str(out), "output_id": steering.identity(out), **summary,
            "layers" if method == "project_out" else "changes": shown,
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


def head_behaviour(model, encoded, standard):
    """Mean previous-token, first-token and induction attention per head, on the probe text.

    Returns {layer: [(previous, sink, induction or None), ...]}; induction is measured only at
    positions that repeat an earlier 3-token sequence.
    """
    import mlx.core as mx
    import numpy as np
    modules = {index: attention_heads(model, index) for index in standard}
    seen_values = {(index, kind): [] for index in standard for kind in ("previous", "sink", "induction")}
    with attention_inputs(model, list(standard)) as seen:
        for tokens, _ in encoded:
            inputs = np.array(tokens[:-1])
            queries = [position - 1 for position in repeat_positions(tokens, 0)]
            model(mx.array([tokens[:-1]], dtype=mx.int32))
            match = copy_match(inputs, queries) if queries else None
            for index in standard:
                pattern = attention_pattern(*modules[index], seen[index])
                measured = {"previous": mx.mean(mx.diagonal(pattern, offset=-1, axis1=1, axis2=2), axis=-1),
                            "sink": mx.mean(pattern[:, 1:, 0], axis=-1)}
                if match is not None:
                    measured["induction"] = mx.mean(mx.sum(pattern[:, queries, :] * match[None], axis=-1), axis=-1)
                mx.eval(*measured.values())
                for kind, value in measured.items():
                    seen_values[(index, kind)].append(np.array(value))
            seen.clear()
            mx.clear_cache()
    out = {}
    for index in standard:
        mean = {kind: np.mean(values, axis=0) if values else None
                for kind in ("previous", "sink", "induction") for values in [seen_values[(index, kind)]]}
        out[index] = [(float(mean["previous"][head]), float(mean["sink"][head]),
                       None if mean["induction"] is None else float(mean["induction"][head]))
                      for head in range(modules[index][1])]
    return out


def head_role(previous, sink, induction):
    """A coarse label from the attention pattern; thresholds are heuristics, not a proof of function."""
    if induction is not None and induction >= INDUCTION_ROLE:
        return "induction"
    if previous >= PREVIOUS_ROLE:
        return "previous-token"
    if sink >= SINK_ROLE:
        return "attention-sink"
    return "mixed"


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


def rms_recorder(sink):
    """A module-hook callback that records the root-mean-square of a module's output."""
    import mlx.core as mx

    def record(output):
        value = steering.hidden(output).astype(mx.float32)
        sink.append(float(mx.sqrt(mx.mean(mx.square(value))).item()))
        return output
    return record


def router_read(model, encoded, moe_layers, linear_layers):
    """Router statistics of each mixture-of-experts layer and output size of each linear-attention block.

    A routed slot is one (token, chosen expert) pair and `share` is an expert's fraction of the slots.
    `entropy` is the mean per-token entropy of the router distribution over log(experts): 1 is uniform
    routing, 0 sends every token to one expert. `shared_gate` is the mean sigmoid gate of the shared
    expert. Returns ({layer: experts row}, {layer: linear_attention row}) for the probe text.
    """
    import mlx.core as mx
    import numpy as np
    layers = steering.layer_list(model)
    blocks = {index: layers[index].mlp for index in moe_layers}
    totals, outputs = {}, {index: [] for index in linear_layers}
    with contextlib.ExitStack() as stack:
        seen = stack.enter_context(layer_inputs(model, moe_layers, lambda layer: (layer, "mlp")))
        for index in linear_layers:
            stack.enter_context(module_hook(model, index, "linear_attn", rms_recorder(outputs[index])))
        for tokens, _ in encoded:
            model(mx.array([tokens[:-1]], dtype=mx.int32))
            for index, value in seen.items():
                block = blocks[index]
                count = int(block.gate.weight.shape[0])
                probs = mx.softmax(block.gate(value).astype(mx.float32), axis=-1, precise=True)
                chosen = mx.argpartition(probs, kth=-block.top_k, axis=-1)[..., -block.top_k:]
                entropy = -mx.sum(probs * mx.log(probs + 1e-12), axis=-1) / math.log(count)
                parts = [entropy, chosen]
                shared = mx.sigmoid(block.shared_expert_gate(value).astype(mx.float32)) \
                    if hasattr(block, "shared_expert_gate") else None
                if shared is not None:
                    parts.append(shared)
                mx.eval(*parts)
                total = totals.setdefault(index, {"counts": np.zeros(count), "entropy": 0.0, "tokens": 0, "shared": []})
                total["counts"] += np.bincount(np.array(chosen).reshape(-1), minlength=count)
                total["entropy"] += float(np.sum(np.array(entropy)))
                total["tokens"] += int(np.array(entropy).size)
                if shared is not None:
                    total["shared"].append(float(np.mean(np.array(shared))))
            seen.clear()
            mx.clear_cache()
    routed = {}
    for index, total in totals.items():
        counts = total["counts"]
        ranked = np.argsort(-counts, kind="stable")[:8].tolist()
        routed[index] = {"count": len(counts), "top_k": int(blocks[index].top_k),
                         "entropy": total["entropy"] / total["tokens"],
                         "top": [{"expert": item, "share": float(counts[item] / counts.sum())}
                                 for item in ranked if counts[item] > 0],
                         "shared_gate": float(np.mean(total["shared"])) if total["shared"] else None}
    measured = {index: {"output_rms": float(np.mean(values))} for index, values in outputs.items() if values}
    return routed, measured


def analyze(request, root, config):
    """An atlas of what the model's parts do, found by probing it with built-in text.

    Per layer: how much the probe loss rises when the layer is skipped (`importance`), the attention
    heads whose pattern is a recognisable job (induction/copy, previous-token, attention sink) and
    the most active MLP neurons with the tokens each writes toward. The labels describe attention
    patterns and logit-lens effects on a few short texts: leads for `attribute`, not proof.
    """
    import numpy as np
    model, tokenizer = steering.load(request)
    layers = steering.layer_list(model)
    encoded = probe_encoded(tokenizer)
    notes = []
    baseline, answer_tokens = loss(model, encoded)
    rows = [{"layer": index} for index in range(len(layers))]
    for index in range(len(layers)):
        report(request, "scoring layers", index, len(layers))
        with skipped(model, index):
            value, _ = loss(model, encoded)
        rows[index]["importance"] = value - baseline
        rows[index]["stage"] = ("early", "middle", "late")[min(2, 3 * index // len(layers))]
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
        behaviour = head_behaviour(model, encoded, standard)
    except (WeightsError, ValueError) as error:
        behaviour = {}
        notes.append(f"attention heads not classified: {error}")
    heads_seen = {}
    for index, values in behaviour.items():
        notable, counts = [], {}
        for head, (previous, sink, induction) in enumerate(values):
            role = head_role(previous, sink, induction)
            counts[role] = counts.get(role, 0) + 1
            if role != "mixed":
                notable.append({"head": head, "role": role, "previous": previous, "sink": sink,
                                "induction": induction})
            heads_seen[(index, head)] = (role, induction or 0.0)
        rows[index].update(attention="standard", head_count=len(values), head_roles=counts, heads=notable)
    mlp_layers = [index for index, layer in enumerate(layers)
                  if hasattr(getattr(layer, "mlp", None), "down_proj")]
    if len(mlp_layers) < len(layers):
        notes.append(f"{len(layers) - len(mlp_layers)} layers have no dense MLP (mixture-of-experts); "
                     "their neurons are not listed")
    report(request, "reading neurons", 0, 1)
    try:
        activity = neuron_activity(model, encoded, mlp_layers)
        listed = []
        for index, values in activity.items():
            top = np.argsort(-values)[:ANALYZE_NEURONS].tolist()
            rows[index]["neurons"] = [{"layer": index, "neuron": item, "activity": float(values[item])} for item in top]
            listed += rows[index]["neurons"]
        try:
            neuron_effects(model, tokenizer, layers, listed)
        except (WeightsError, ValueError) as error:
            notes.append(f"neuron tokens unavailable: {error}")
    except (WeightsError, ValueError) as error:
        notes.append(f"neurons not read: {error}")
    moe_layers = [index for index, layer in enumerate(layers) if hasattr(getattr(layer, "mlp", None), "switch_mlp")]
    linear_layers = [index for index, layer in enumerate(layers) if getattr(layer, "linear_attn", None) is not None]
    routed = {}
    if moe_layers or linear_layers:
        report(request, "reading routing", 0, 1)
        try:
            routed, measured = router_read(model, encoded, moe_layers, linear_layers)
            for index, value in routed.items():
                rows[index]["experts"] = value
            for index, value in measured.items():
                rows[index]["linear_attention"] = value
        except (WeightsError, ValueError) as error:
            notes.append(f"routing not read: {error}")
    report(request, "analyzed", 1, 1)
    ranked = sorted(rows, key=lambda row: -row["importance"])
    induction = sorted(((key, value[1]) for key, value in heads_seen.items() if value[0] == "induction"),
                       key=lambda item: -item[1])
    summary = {"most_important_layers": [row["layer"] for row in ranked[:5]],
               "least_important_layers": [row["layer"] for row in ranked[-5:]],
               "induction_heads": [{"layer": key[0], "head": key[1], "induction": score}
                                   for key, score in induction[:10]],
               "heads_by_role": {role: sum(1 for value in heads_seen.values() if value[0] == role)
                                 for role in ("induction", "previous-token", "attention-sink", "mixed")}}
    if routed:
        summary["routing"] = {"moe_layers": len(routed),
                              "mean_entropy": float(np.mean([value["entropy"] for value in routed.values()]))}
    return {"probes": len(PROBES), "probe_tokens": answer_tokens, "baseline_loss": baseline,
            "layer_count": len(layers), "summary": summary, "layers": rows,
            **({"notes": notes} if notes else {})}


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
              "weights_combine": combine}
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
    config = read_config(root)
    result = OPERATIONS[name](request, root, config)
    rows = result.get("tensors")
    if isinstance(rows, list) and len(rows) > MAX_TENSOR_ROWS:
        result.update(tensors=rows[:MAX_TENSOR_ROWS], tensors_total=len(rows), tensors_truncated=True)
    result.update(version=VERSION, op=name.removeprefix("weights_"), model=str(root),
                  model_id=steering.identity(root))
    return rounded(result)
