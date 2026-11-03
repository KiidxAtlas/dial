"""MLX weight and activation inspection and budgeted weight edits for Dial's weights surface.

Read-only operations: `weights_map` (checkpoint headers only), `weights_stats` (per-tensor
moments), `weights_activations` (per-layer contrast of matched response pairs),
`weights_trace` (layer or attention-head ablation against scored answers), `weights_generate`
(greedy answers with length/loop metrics) and `weights_direction` (per-layer mean difference of
answer activations, saved for an edit). `weights_edit` never touches the base model: it writes a
new model directory whose unedited files are hard links to the base. Results are bounded JSON
and never echo prompt text.
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


def trace_cases(request):
    cases = request.get("cases")
    require(isinstance(cases, list) and 1 <= len(cases) <= MAX_CASES, f"trace requires 1–{MAX_CASES} cases")
    for number, case in enumerate(cases):
        require(isinstance(case, dict), f"case {number} must be an object")
        raw = case.get("messages")
        require(isinstance(raw, list) and bool(raw) and isinstance(raw[-1], dict) and raw[-1].get("role") == "assistant",
                f"case {number} must end with the assistant answer to score")
        steering.messages(raw, True)
        tool_list(case.get("tools"), f"case {number}")
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
    return {"cases": rows, "max_tokens": limit, "summary": {
        "cases": len(rows), "hit_limit": sum(row["hit_limit"] for row in rows),
        "mean_tokens": sum(row["tokens"] for row in rows) / len(rows),
        "mean_repeat_ratio": sum(row["repeat_ratio"] for row in rows) / len(rows),
        "checked": len(checked), "passed": sum(checked)}}


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
def mlp_hook(model, index, callback):
    """Observe layer `index`'s MLP output: what the MLP writes into the residual stream."""
    layer = steering.layer_list(model)[index]
    original = getattr(layer, "mlp", None)
    require(original is not None, f"layer {index} has no mlp module; use site residual")
    layer.mlp = steering.wrap(original, callback)
    try:
        yield
    finally:
        layer.mlp = original


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


def project_out(arrays, row, vector, columns, alpha):
    """Remove `alpha` of direction d from the `columns` output-projection columns most aligned with it.

    Column j of W is what hidden unit j writes to the residual stream; W[:, j] -= alpha*d*(d.W[:, j]).
    Quantized weights are dequantized, edited and requantized; groups holding no edited column keep
    their original bytes. The requantization error is relative to the size of the intended edit.
    """
    import mlx.core as mx
    import numpy as np
    name = row["name"]
    module = name.rpartition(".")[0]
    weight = arrays[name]
    settings = row["quantization"]
    if row["quantized"]:
        require(settings is not None, f"{name}: quantization parameters are missing from config.json")
        scales, biases = arrays[f"{module}.scales"], arrays.get(f"{module}.biases")
        dense = mx.dequantize(weight, scales, biases, group_size=settings["group_size"],
                              bits=settings["bits"], mode=settings["mode"])
        stored_dtype = dense.dtype
        dense = dense.astype(mx.float32)
    else:
        require(mx.issubdtype(weight.dtype, mx.floating), f"{name} is not a floating-point tensor")
        stored_dtype, dense = weight.dtype, weight.astype(mx.float32)
    rows_out, width = dense.shape
    require(dense.ndim == 2 and rows_out == vector.shape[0], f"{name} does not write the direction's hidden size")
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
    updates = {}
    if row["quantized"]:
        group = settings["group_size"]
        require(width % group == 0 and weight.shape[-1] % (width // group) == 0,
                f"{name}: packed layout does not split into quantization groups")
        packed = mx.quantize(edited.astype(stored_dtype), group_size=group, bits=settings["bits"],
                             mode=settings["mode"])
        touched = mask.reshape(width // group, group).max(axis=1) > 0
        groups = mx.array(touched)[None, :]
        words = mx.repeat(groups, weight.shape[-1] // (width // group), axis=1)
        new_weight = mx.where(words, packed[0], weight)
        new_scales = mx.where(groups, packed[1], scales)
        new_biases = None
        if biases is not None:
            require(len(packed) > 2, f"{name}: requantization returned no biases")
            new_biases = mx.where(groups, packed[2], biases)
            updates[f"{module}.biases"] = new_biases
        updates[name], updates[f"{module}.scales"] = new_weight, new_scales
        stored = mx.dequantize(new_weight, new_scales, new_biases, group_size=group,
                               bits=settings["bits"], mode=settings["mode"]).astype(mx.float32)
        rewritten = rows_out * int(touched.sum()) * group
    else:
        updates[name] = edited.astype(stored_dtype)
        stored = updates[name].astype(mx.float32)
        rewritten = rows_out * len(chosen)
    change = mx.sqrt(mx.sum(delta * delta))
    values = mx.stack([mx.sqrt(mx.sum(coefficients * coefficients)), mx.sqrt(mx.sum((d @ stored) ** 2)),
                       change, mx.sqrt(mx.sum((stored - edited) ** 2))])
    mx.eval(values, *updates.values())
    before, after, size, error = values.tolist()
    require(all(math.isfinite(value) for value in (before, after, size, error)), f"{name}: edit produced nonfinite values")
    return {"tensor": name, "columns": chosen.tolist(), "projection_before": before, "projection_after": after,
            "edit_norm": size, "requantization_error": error / size if size > 0 else 0.0,
            "edited_params": rows_out * len(chosen), "rewritten_params": rewritten}, updates


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


def edit(request, root, config):
    """Write an edited copy of the model at `output`; the base directory is never modified."""
    import mlx.core as mx
    require(request.get("method", "project_out") == "project_out", "method must be project_out")
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
            and len(set(layers)) == len(layers), f"layers must be 1–{MAX_EDIT_LAYERS} distinct measured layers")
    columns = request.get("columns", 64)
    require(type(columns) is int and 1 <= columns <= MAX_EDIT_COLUMNS, f"columns must be 1–{MAX_EDIT_COLUMNS}")
    alpha = request.get("alpha", 1.0)
    require(type(alpha) in (int, float) and math.isfinite(alpha) and 0 < alpha <= 2, "alpha must be in (0, 2]")
    output = request.get("output")
    require(isinstance(output, str) and bool(output), "output must be a new model directory")
    out = Path(output)
    require(not out.exists() and out.parent.is_dir(), "output must be a new directory in an existing parent")
    require(root not in out.resolve().parents, "output must be outside the base model directory")
    rows = tensors(root, config)
    plan = {}
    for layer in sorted(layers):
        row = output_projection(rows, layer)
        plan.setdefault(row["shard"], []).append((layer, row))
    stage = Path(tempfile.mkdtemp(prefix=f".{out.name}.partial-", dir=out.parent))
    try:
        edits = []
        for shard, items in plan.items():
            arrays, shard_metadata = mx.load(str(root / shard), return_metadata=True)
            for layer, row in items:
                report(request, "editing layers", len(edits), len(layers))
                result, updates = project_out(arrays, row, directions[layer], columns, alpha)
                arrays.update(updates)
                edits.append(dict(result, layer=layer))
            mx.save_safetensors(str(stage / shard), arrays, metadata=shard_metadata)
            del arrays
            mx.clear_cache()
        linked, copied = link_tree(root, stage, set(plan))
        total = sum(row["params"] for row in rows)
        record = {"version": VERSION, "method": "project_out", "base": str(root), "base_id": steering.identity(root),
                  "direction": {"site": metadata.get("site"), "model_id": metadata["model_id"]},
                  "alpha": alpha, "columns": columns, "layers": rounded(edits)}
        steering.atomic_json(stage / EDIT_RECORD, record)
        os.rename(stage, out)
    except BaseException:
        shutil.rmtree(stage, ignore_errors=True)
        raise
    report(request, "edited", len(layers), len(layers))
    rewritten = sum(row["rewritten_params"] for row in edits)
    return {"method": "project_out", "output": str(out), "output_id": steering.identity(out), "alpha": alpha,
            "columns": columns, "site": metadata.get("site"),
            # Column indices stay in the edit record; results carry a preview.
            "layers": [dict(row, columns=row["columns"][:16]) for row in edits],
            "edited_params": sum(row["edited_params"] for row in edits), "rewritten_params": rewritten,
            "total_params": total, "fraction_changed": rewritten / total if total else 0.0,
            "quantized": any(row["quantized"] for _, items in plan.items() for _, row in items),
            "shards_rewritten": len(plan), "files_linked": linked, "files_copied": copied}


OPERATIONS = {"weights_map": weight_map, "weights_stats": stats,
              "weights_activations": activations, "weights_trace": trace,
              "weights_generate": generate, "weights_direction": direction, "weights_edit": edit}
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
