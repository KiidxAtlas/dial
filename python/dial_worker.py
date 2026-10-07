"""One-shot MLX worker. The Rust caller owns serialization and deadlines."""
import contextlib
import copy
import hashlib
import json
import math
import os
from pathlib import Path
import re
import sys
from types import SimpleNamespace

UNSUPPORTED = [
    "expert_growth", "routing_calibration", "attention_retrofit",
    "learned_step_probe",
]
MAX_REQUEST = 8 * 1024 * 1024
MAX_DATA = 128 * 1024 * 1024


def watch_parent():
    """Release model memory if the Rust worker is killed without running Drop."""
    import threading
    import time
    parent = int(os.environ.get("DIAL_PARENT_PID", "0"))
    if parent > 1:
        def watch():
            while True:
                if os.getppid() != parent:
                    os._exit(0)
                time.sleep(1)
        threading.Thread(target=watch, daemon=True).start()


class RequestError(ValueError):
    """Messages contain field names/positions only, never supplied content."""


def require(condition, message):
    if not condition:
        raise RequestError(message)


def integer(request, key, low, high):
    value = request.get(key)
    require(type(value) is int and low <= value <= high,
            f"{key} must be an integer in [{low}, {high}]")
    return value


def string(request, key):
    value = request.get(key)
    require(isinstance(value, str) and bool(value.strip()), f"{key} must be nonempty")
    return value


def tool_calls(value):
    require(isinstance(value, list) and bool(value), "tool_calls must be a nonempty array")
    for call in value:
        require(isinstance(call, dict) and set(call) == {"id", "type", "function"}
                and isinstance(call["id"], str) and bool(call["id"]) and call["type"] == "function",
                "invalid tool call")
        function = call["function"]
        require(isinstance(function, dict) and set(function) == {"name", "arguments"}
                and isinstance(function["name"], str) and bool(function["name"])
                and isinstance(function["arguments"], dict),
                "tool call arguments must be an object")


def messages(value, training=False):
    require(isinstance(value, list) and bool(value), "messages must be a nonempty array")
    for message in value:
        require(isinstance(message, dict) and isinstance(message.get("role"), str), "invalid message")
        role = message["role"]
        if training and role == "assistant" and "tool_calls" in message:
            # Tool-call turn: text is optional but must be a string when present.
            require(set(message) <= {"role", "content", "tool_calls"}, "invalid tool-call message")
            require(message.get("content") is None or isinstance(message["content"], str),
                    "tool-call message content must be text")
            tool_calls(message["tool_calls"])
            continue
        if training and role == "tool":
            require(set(message) == {"role", "content", "tool_call_id"}
                    and isinstance(message["content"], str)
                    and isinstance(message["tool_call_id"], str) and bool(message["tool_call_id"]),
                    "invalid tool message")
            continue
        require(set(message) == {"role", "content"}, "each message must contain only role and content")
        require(role in ("system", "user", "assistant"), "unsupported message role")
        require(isinstance(message["content"], str) and bool(message["content"].strip()),
                "message content must be nonempty text")
    if training:
        require(value[-1]["role"] == "assistant", "training messages must end with assistant")
        require(any(m["role"] == "user" for m in value[:-1]),
                "training messages require a preceding user message")
    return value


def tool_schemas(value):
    require(isinstance(value, list) and bool(value), "tools must be a nonempty array")
    for schema in value:
        require(isinstance(schema, dict) and schema.get("type") == "function"
                and isinstance(schema.get("function"), dict)
                and isinstance(schema["function"].get("name"), str),
                "invalid tool schema")
    return value


def read_records(path, *, trajectory_ids=None):
    """Accept only records marked verified by an external check; raw trajectory logs are never ingested."""
    records, hashes = [], set()
    total = 0
    with Path(path).open("rb") as source:
        for number, line in enumerate(iter(lambda: source.readline(MAX_DATA + 1), b""), 1):
            total += len(line)
            require(total <= MAX_DATA, "dataset exceeds 128 MiB")
            require(bool(line.strip()), f"dataset line {number} is empty")
            try:
                record = json.loads(line)
            except (ValueError, UnicodeError):
                raise RequestError(f"dataset line {number} is not valid JSON") from None
            require(isinstance(record, dict) and {"messages", "source", "verified"} <= set(record)
                    and set(record) <= {"messages", "source", "verified", "tools", "preference_pairs"},
                    f"dataset line {number} requires messages, source, verified and optional tools/preference_pairs only")
            require(record["verified"] is True, f"dataset line {number} is not verified")
            string(record, "source")
            provenance = record["source"]
            external = provenance.split(":", 3)[-1] if provenance.startswith("dial:verified:") else provenance
            require(not external.startswith("check:"), "agent command exit codes are not verified task outcomes")
            trajectory = re.fullmatch(r"dial:(?:verified|tool-recovery):(\d+)(?::.*)?", provenance)
            if trajectory_ids is not None and trajectory is not None:
                trajectory_ids.add(int(trajectory.group(1)))
            chat = messages(record["messages"], training=True)
            digest = hashlib.sha256(json.dumps(chat, sort_keys=True, ensure_ascii=False,
                                               separators=(",", ":")).encode()).hexdigest()
            hashes.add(digest)
            entry = {"messages": chat}
            if "tools" in record:
                entry["tools"] = tool_schemas(record["tools"])
            if "preference_pairs" in record:
                require(provenance.startswith("dial:tool-recovery:"),
                        "preference pairs are allowed only on verified tool-repair records")
                require(isinstance(record["preference_pairs"], list),
                        f"dataset line {number} preference_pairs must be an array")
                for pair in record["preference_pairs"]:
                    require(isinstance(pair, dict) and set(pair) == {"context", "tools", "chosen", "rejected"},
                            f"dataset line {number} contains an invalid preference pair")
                    context = pair["context"]
                    require(isinstance(context, list) and bool(context), "preference context must be nonempty")
                    tool_schemas(pair["tools"])
                    for answer in (pair["chosen"], pair["rejected"]):
                        messages(context + [answer], training=True)
                entry["preference_pairs"] = record["preference_pairs"]
            records.append(entry)
    require(bool(records), "dataset must not be empty")
    return records, hashes


def resolve_model(model_name):
    from mlx_lm.utils import _download
    return str(_download(model_name).resolve(strict=True))


def read_datasets(train_path, valid_path):
    train_trajectories, valid_trajectories = set(), set()
    train, train_hashes = read_records(train_path, trajectory_ids=train_trajectories)
    valid, valid_hashes = read_records(valid_path, trajectory_ids=valid_trajectories)
    valid = [{key: value for key, value in record.items() if key != "preference_pairs"} for record in valid]
    require(train_trajectories.isdisjoint(valid_trajectories),
            "training and validation trajectories overlap")
    require(not train_hashes.intersection(valid_hashes),
            "training and validation messages overlap")
    train_prompts = {next(m["content"].strip() for m in r["messages"] if m["role"] == "user") for r in train}
    valid_prompts = {next(m["content"].strip() for m in r["messages"] if m["role"] == "user") for r in valid}
    require(train_prompts.isdisjoint(valid_prompts),
            "training and validation prompts overlap")
    return train, valid


def collect_preference_pairs(records):
    """Collect only the capped, verified pair payload carried by training records."""
    pairs = []
    for record in records:
        for pair in record.get("preference_pairs", []):
            pairs.append(pair)
            if len(pairs) == MAX_PREFERENCE_PAIRS:
                return pairs
    return pairs


def preference_iterations(training_iters, pair_count):
    return min(8, training_iters, max(1, (min(pair_count, MAX_PREFERENCE_PAIRS) + PREFERENCE_PAIRS_PER_BATCH - 1) // PREFERENCE_PAIRS_PER_BATCH))


MAX_PREFERENCE_PAIRS = 32
PREFERENCE_PAIRS_PER_BATCH = 4
PREFERENCE_WEIGHT = 0.1


def preference_objective(chosen_nll, chosen_logp, rejected_logp):
    """Chosen SFT NLL plus a reference-free pairwise logistic ranking loss."""
    gap = rejected_logp - chosen_logp
    return chosen_nll + PREFERENCE_WEIGHT * (max(gap, 0.0) + math.log1p(math.exp(-abs(gap))))




def completion_mask(length, offset):
    require(0 <= offset < length, "preference completion target bounds are invalid")
    return [offset <= position < length for position in range(length)]


def tokenize_preference_pairs(pairs, tokenizer, max_seq_length):
    """Tokenize complete prompt/answer pairs; mask the prompt and never truncate answers."""
    encoded = []
    for pair in pairs[:MAX_PREFERENCE_PAIRS]:
        context, tools = pair["context"], pair["tools"]
        prompt = tokenizer.apply_chat_template(
            context, tools=tools, add_generation_prompt=True, tokenize=True)
        targets = []
        for answer in (pair["chosen"], pair["rejected"]):
            full = tokenizer.apply_chat_template(
                context + [answer], tools=tools, add_generation_prompt=False, tokenize=True)
            require(isinstance(prompt, list) and isinstance(full, list)
                    and len(prompt) < len(full) and full[:len(prompt)] == prompt,
                    "preference prompt does not align with the completion token boundary")
            require(2 <= len(full) <= max_seq_length,
                    "preference example has no completion tokens or exceeds max_seq_length")
            offset = len(prompt)
            require(0 <= offset < len(full) and len(full) > offset,
                    "preference completion target bounds are invalid")
            targets.append((full, offset))
        encoded.append((targets[0], targets[1]))
    return encoded


def preference_loss(model, batch, lengths):
    """Chosen SFT NLL + 0.1 * pairwise softplus ranking for all chosen/rejected pairs in a batch."""
    import mlx.core as mx
    import mlx.nn as nn
    inputs, targets = batch[:, :-1], batch[:, 1:]
    logits = model(inputs).astype(mx.float32)
    log_probs = nn.log_softmax(logits, axis=-1)
    token_log_probs = mx.take_along_axis(log_probs, targets[..., None], axis=-1).squeeze(-1)
    positions = mx.arange(1, targets.shape[1] + 1)
    mask = mx.logical_and(positions >= lengths[:, 0:1], positions <= lengths[:, 1:])
    counts = mask.sum(axis=1)
    completion_logp = (token_log_probs * mask).sum(axis=1) / counts
    chosen = completion_logp[::2]
    rejected = completion_logp[1::2]
    chosen_nll = -chosen.mean()
    ranking = nn.softplus(-(chosen - rejected)).mean()
    return chosen_nll + PREFERENCE_WEIGHT * ranking, counts[::2].sum()


def pack_preference_batch(pairs, max_seq_length):
    require(bool(pairs), "preference batch must contain at least one pair")
    sequences = [sequence for pair in pairs for sequence in pair]
    max_length = max(len(tokens) for tokens, _ in sequences)
    require(max_length <= max_seq_length, "preference sequence exceeds max_seq_length")
    batch, lengths = [[0] * max_length for _ in sequences], []
    for row, (tokens, offset) in enumerate(sequences):
        require(0 <= offset < len(tokens) <= max_seq_length,
                "preference completion target bounds are invalid")
        batch[row][:len(tokens)] = tokens
        lengths.append((offset, len(tokens)))
    return batch, lengths


def iterate_preference_batches(dataset, batch_size, max_seq_length, loop=False, seed=None, comm_group=None):
    import mlx.core as mx
    import numpy as np
    require(batch_size > 0, "preference pair batch size must be positive")
    order = np.random.permutation(len(dataset)).tolist()
    index = 0
    while index < len(order) or loop:
        if index == len(order):
            order = np.random.permutation(len(dataset)).tolist()
            index = 0
        pairs = [dataset[i] for i in order[index:index + batch_size]]
        batch, lengths = pack_preference_batch(pairs, max_seq_length)
        yield mx.array(batch), mx.array(lengths)
        index += len(pairs)
        if not loop:
            break


def load_base(model):
    from mlx_lm import load
    return load(model, tokenizer_config={"trust_remote_code": False})


LORA_PROBE_VERSION = 2


def query_targets(model, count=None):
    """Preserve query adapters; fall back to the architecture's linear projections.

    MLX applies a union of relative keys to every selected block. Resolve that union
    before computing expected tensors, including matches in heterogeneous blocks.
    """
    import mlx.nn as nn
    from mlx_lm.models.switch_layers import SwitchLinear, QuantizedSwitchLinear
    supported = (nn.Linear, nn.QuantizedLinear, SwitchLinear, QuantizedSwitchLinear)
    layers = getattr(model, "layers", None)
    require(layers is not None and len(layers) > 0, "model exposes no trainable transformer layers")
    if count is not None:
        require(0 < count <= len(layers), "num_layers exceeds model layer count")
    selected = layers if count is None else layers[-count:]
    keys = set()
    for layer in selected:
        eligible = [(name, module) for name, module in layer.named_modules()
                    if name and isinstance(module, supported)]
        # Keep existing query-only adapters byte-for-byte compatible on ordinary attention.
        found = [(name, module) for name, module in eligible if name.rsplit(".", 1)[-1] == "q_proj"]
        if not found:
            # Prefer a single output projection for fused attention and recurrent blocks.
            found = [(name, module) for name, module in eligible
                     if name.rsplit(".", 1)[-1] in ("o_proj", "out_proj")]
        if not found:
            found = eligible
        require(bool(found), "selected block exposes no MLX LoRA-compatible linear projections")
        keys.update(name for name, _ in found)
    modules = [module for layer in selected for name, module in layer.named_modules() if name in keys]
    require(all(isinstance(module, supported) for module in modules), "target path has incompatible types across blocks")
    selected_ids = {id(module) for module in modules}
    require(not any(name in keys and id(module) not in selected_ids
                    for name, module in model.named_modules()), "ambiguous adapter projection paths")
    return sorted(keys), selected_ids


def expected_weights(model, selected_ids, rank):
    from mlx_lm.models.switch_layers import SwitchLinear, QuantizedSwitchLinear
    from mlx_lm.tuner.lora import LoRALinear, LoRASwitchLinear
    from mlx.utils import tree_flatten
    expected = {}
    for name, module in model.named_modules():
        if id(module) not in selected_ids:
            continue
        wrapper = LoRASwitchLinear if isinstance(module, (SwitchLinear, QuantizedSwitchLinear)) else LoRALinear
        adapter = wrapper.from_base(module, r=rank, scale=20.0, dropout=0.0)
        for key, tensor in tree_flatten(adapter.parameters()):
            if key in ("lora_a", "lora_b"):
                expected[name + "." + key] = tuple(tensor.shape)
    require(bool(expected), "no adapter parameters found")
    return expected


def validate_adapter(path, model_name, count, parameters, expected):
    import mlx.core as mx
    directory = Path(path).resolve(strict=True)
    require(directory.is_dir(), "adapter must be a directory")
    with (directory / "adapter_config.json").open() as source:
        config = json.load(source)
    require(isinstance(config, dict), "adapter configuration must be an object")
    require(config.get("model") == model_name and config.get("fine_tune_type") == "lora"
            and config.get("num_layers") == count and config.get("lora_parameters") == parameters,
            "adapter model, layer count or LoRA parameters are incompatible")
    tensors = mx.load(str(directory / "adapters.safetensors"))
    require(set(tensors) == set(expected), "adapter tensor names are incomplete or incompatible")
    for name, tensor in tensors.items():
        require(tuple(tensor.shape) == expected[name], "adapter tensor shape is incompatible")
        require(bool(mx.all(mx.isfinite(tensor)).item()), "adapter contains nonfinite values")
    return directory


def probe_adapter(model, tokens):
    """One small update and a serialized reload; never saves an admitted adapter."""
    import tempfile
    import mlx.core as mx
    import mlx.nn as nn
    from mlx.utils import tree_flatten, tree_unflatten
    from mlx_lm.tuner.utils import linear_to_lora_layers, load_adapters
    # Exercise all block types, with rank one and a short sequence to bound overhead.
    keys, ids = query_targets(model)
    parameters = {"rank": 1, "dropout": 0.0, "scale": 20.0, "keys": keys}
    expected = expected_weights(model, ids, 1)
    originals = [(name, module) for name, module in model.named_modules() if id(module) in ids]
    model.freeze()
    linear_to_lora_layers(model, len(model.layers), parameters)
    model.train()
    batch = mx.array([tokens[:8]])
    require(batch.shape[1] >= 2, "verification prompt has insufficient tokens")
    def loss_fn(network):
        return nn.losses.cross_entropy(network(batch[:, :-1]), batch[:, 1:], reduction="mean")
    loss, grads = nn.value_and_grad(model, loss_fn)(model)
    flat = dict(tree_flatten(grads))
    mx.eval(loss, grads)
    require(math.isfinite(loss.item()), "adapter verification loss is nonfinite")
    require(set(flat) == set(expected), "adapter verification trainable tensors differ from targets")
    require(all(bool(mx.all(mx.isfinite(g)).item()) for g in flat.values()), "adapter verification gradient is nonfinite")
    require(any(bool(mx.any(g != 0).item()) for g in flat.values()), "adapter verification gradients are all zero")
    weights = dict(tree_flatten(model.trainable_parameters()))
    updated = {name: weights[name] - 1e-3 * gradient for name, gradient in flat.items()}
    model.update(tree_unflatten(list(updated.items())))
    model.eval()
    logits = model(batch[:, :-1])
    mx.eval(logits, updated)
    require(bool(mx.all(mx.isfinite(logits)).item()), "adapter verification logits are nonfinite")
    with tempfile.TemporaryDirectory(prefix="dial-lora-probe-") as directory:
        path = Path(directory)
        (path / "adapter_config.json").write_text(json.dumps({"fine_tune_type": "lora",
            "num_layers": len(model.layers), "lora_parameters": parameters}))
        mx.save_safetensors(str(path / "adapters.safetensors"), updated)
        model.update_modules(tree_unflatten(originals))
        load_adapters(model, directory)
        model.eval()
        reloaded = model(batch[:, :-1])
        mx.eval(reloaded)
        require(bool(mx.allclose(logits, reloaded, rtol=1e-3, atol=1e-3).item()), "adapter reload changed model output")
    return keys


def capabilities(request):
    model_name = string(request, "model")
    resolved = None
    try:
        resolved = resolve_model(model_name)
        model, tokenizer = load_base(resolved)
        tokenizer.apply_chat_template([{"role": "user", "content": "Hello"}],
                                      add_generation_prompt=True, tokenize=True)
    except Exception as error:
        return {"inference": False, "lora": False,
                "reason": f"MLX model/tokenizer loading or chat template unavailable ({type(error).__name__}); remote code is disabled",
                "unsupported": UNSUPPORTED, "resolved_model": resolved}
    try:
        tokens = tokenizer.apply_chat_template([{"role": "user", "content": "Hello"}], add_generation_prompt=True, tokenize=True)
        keys = probe_adapter(model, tokens)
    except RequestError as error:
        return {"inference": True, "lora": False, "lora_probe_version": LORA_PROBE_VERSION, "reason": "Dial adapter verification failed: " + str(error),
                "unsupported": UNSUPPORTED, "resolved_model": resolved}
    return {"inference": True, "lora": True, "lora_probe_version": LORA_PROBE_VERSION,
            "reason": "Gradient update and adapter save/reload passed; LoRA/QLoRA targets: " + ", ".join(keys),
            "unsupported": UNSUPPORTED, "resolved_model": resolved}


def generate(request):
    import mlx.core as mx
    from mlx_lm import generate as mlx_generate
    from mlx_lm.sample_utils import make_sampler
    from mlx_lm.tuner.utils import load_adapters
    model_name = string(request, "model")
    chat = messages(request.get("messages"))
    limit = integer(request, "max_tokens", 1, 32768)
    seed = integer(request, "seed", 0, 2**64 - 1)
    temperature = request.get("temperature")
    require(type(temperature) in (int, float) and math.isfinite(temperature)
            and 0 <= temperature <= 2, "temperature must be finite in [0, 2]")
    model_name = resolve_model(model_name)
    model, tokenizer = load_base(model_name)
    if request.get("adapter") is not None:
        directory = Path(string(request, "adapter")).resolve(strict=True)
        with (directory / "adapter_config.json").open() as source:
            config = json.load(source)
        count = integer(config, "num_layers", 1, 256)
        parameters = config.get("lora_parameters")
        require(isinstance(parameters, dict), "missing adapter LoRA parameters")
        rank = integer(parameters, "rank", 1, 128)
        keys, ids = query_targets(model, count)
        require(parameters == {"rank": rank, "dropout": 0.0, "scale": 20.0, "keys": keys},
                "adapter must use supported architecture-specific LoRA parameters")
        validate_adapter(directory, model_name, count, parameters, expected_weights(model, ids, rank))
        load_adapters(model, str(directory))
    prompt = tokenizer.apply_chat_template(chat, add_generation_prompt=True, tokenize=True)
    mx.random.seed(seed)
    text = mlx_generate(model, tokenizer, prompt=prompt, max_tokens=limit,
                        sampler=make_sampler(temp=temperature), verbose=False)
    require(isinstance(text, str), "backend returned non-text generation")
    return {"text": text}


class TrainingPaused(Exception):
    """A complete training checkpoint was committed; the process can release its GPU memory."""


def atomic_json(path, value):
    path = Path(path)
    temporary = path.with_suffix(path.suffix + ".tmp")
    with temporary.open("w") as handle:
        json.dump(value, handle)
        handle.flush()
        os.fsync(handle.fileno())
    os.replace(temporary, path)


def replay_batches(factory, completed, seed):
    """Give each batch stream its own RNG and replay its cursor without recomputing gradients."""
    def batches(**kwargs):
        import numpy as np
        kwargs.pop("seed", None)
        loop = kwargs.get("loop", False)
        iterator = factory(**kwargs)
        state = np.random.RandomState(seed).get_state()
        index = 0
        while True:
            previous = np.random.get_state()
            np.random.set_state(state)
            try:
                batch = next(iterator)
                state = np.random.get_state()
            except StopIteration:
                return
            finally:
                np.random.set_state(previous)
            index += 1
            if not loop or index > completed:
                yield batch
    return batches


def pausable_train(request, model, tokenizer, train_set, valid_set, pairs, args, expected):
    import mlx.core as mx
    import mlx.optimizers as optimizers
    from mlx.utils import tree_flatten, tree_unflatten
    from mlx_lm.tuner import TrainingArgs, train as tuner_train
    from mlx_lm.tuner.trainer import iterate_batches
    from mlx_lm.tuner.callbacks import TrainingCallback
    from mlx_lm.tuner.utils import linear_to_lora_layers
    import tempfile

    output = Path(request["output_dir"])
    pause = Path(string(request, "pause_file"))
    progress = Path(string(request, "progress_file"))
    identity = {key: request[key] for key in (
        "model", "resume_adapter", "iters", "batch_size", "num_layers", "rank", "max_seq_length")}
    identity["datasets"] = [hashlib.sha256(Path(request[key]).read_bytes()).hexdigest()
                            for key in ("train_data", "valid_data")]
    fingerprint = hashlib.sha256(json.dumps(identity, sort_keys=True).encode()).hexdigest()
    mx.random.seed(args["seed"])
    model.freeze()
    linear_to_lora_layers(model, args["num_layers"], args["lora_parameters"])
    checkpoint = None
    if request.get("resume_checkpoint"):
        checkpoint = json.loads((output / "resume.json").read_text())
        require(checkpoint["fingerprint"] == fingerprint, "training checkpoint inputs changed")
        directory = (output / checkpoint["directory"]).resolve()
        require(directory.parent == (output / "checkpoints").resolve(), "invalid checkpoint directory")
        tensors = mx.load(str(directory / "adapters.safetensors"))
        require({name: tuple(value.shape) for name, value in tensors.items()} == expected,
                "checkpoint adapter tensor contract changed")
        model.load_weights(list(tensors.items()), strict=False)
    elif args["resume_adapter_file"]:
        model.load_weights(args["resume_adapter_file"], strict=False)
    output.mkdir(parents=True, exist_ok=True)
    atomic_json(output / "adapter_config.json", args)
    tokenized_pairs = tokenize_preference_pairs(pairs, tokenizer, args["max_seq_length"]) if pairs else []
    phases = [("sft", args["iters"], train_set, valid_set, args["batch_size"], iterate_batches, None)]
    if tokenized_pairs:
        phases.append(("preference", preference_iterations(args["iters"], len(tokenized_pairs)),
                       tokenized_pairs, None, PREFERENCE_PAIRS_PER_BATCH, iterate_preference_batches, preference_loss))
    require(checkpoint is None or checkpoint["phase"] in [phase[0] for phase in phases], "invalid checkpoint phase")
    for phase, total, dataset, validation, batch_size, factory, loss in phases:
        if checkpoint and checkpoint["phase"] == "preference" and phase == "sft":
            continue
        completed = checkpoint["completed"] if checkpoint and checkpoint["phase"] == phase else 0
        require(type(completed) is int and 0 <= completed <= total, "invalid checkpoint iteration")
        optimizer = optimizers.Adam(learning_rate=args["learning_rate"])
        if checkpoint and checkpoint["phase"] == phase:
            state = tree_unflatten(list(mx.load(str(directory / "state.npz")).items()))
            optimizer.state = state.get("optimizer", {})
            require(len(mx.random.state) == len(state["rng"]), "checkpoint RNG contract changed")
            for current, saved_key in zip(mx.random.state, state["rng"]):
                current[:] = saved_key

        class Callback(TrainingCallback):
            def update(self, iteration, info):
                atomic_json(progress, {"stage": f"training {phase} {iteration}/{total}",
                                      "phase": phase, "iteration": iteration, "total": total,
                                      "peak_memory_gb": info.get("peak_memory")})
                root = output / "checkpoints"
                root.mkdir(exist_ok=True)
                saved = Path(tempfile.mkdtemp(prefix="step-", dir=root))
                weights = dict(tree_flatten(model.trainable_parameters()))
                state = dict(tree_flatten({"optimizer": optimizer.state, "rng": list(mx.random.state)}))
                mx.eval(weights, state)
                mx.save_safetensors(str(saved / "adapters.safetensors"), weights)
                mx.savez(str(saved / "state.npz"), **state)
                # Commit the pointer last: an interrupted save cannot become a resumable checkpoint.
                prior = json.loads((output / "resume.json").read_text()) if (output / "resume.json").exists() else None
                atomic_json(output / "resume.json", {"fingerprint": fingerprint, "phase": phase,
                            "completed": iteration, "directory": str(saved.relative_to(output))})
                if prior:
                    import shutil
                    old = (output / prior["directory"]).resolve()
                    if old.parent == root.resolve() and old != saved.resolve():
                        shutil.rmtree(old)
                if pause.exists():
                    raise TrainingPaused()

            def on_train_loss_report(self, info):
                self.update(completed + info["iteration"], info)

            def on_val_loss_report(self, info):
                self.update(completed + info["iteration"], info)

        if completed < total:
            callback = Callback()
            callback.update(completed, {})
            training_args = TrainingArgs(batch_size=batch_size, iters=total-completed,
                val_batches=-1 if validation else 0, steps_per_report=1,
                steps_per_eval=min(100, total), steps_per_save=total+1,
                adapter_file=output / "adapters.safetensors", max_seq_length=args["max_seq_length"])
            kwargs = {"model": model, "optimizer": optimizer, "train_dataset": dataset,
                      "val_dataset": validation, "args": training_args,
                      "iterate_batches": replay_batches(factory, completed, args["seed"] + (1 if phase == "preference" else 0)),
                      "training_callback": callback}
            if loss is not None:
                kwargs["loss"] = loss
            tuner_train(**kwargs)
        checkpoint = None
    mx.save_safetensors(str(output / "adapters.safetensors"), dict(tree_flatten(model.trainable_parameters())))


def train(request):
    import numpy as np
    from mlx_lm.lora import CONFIG_DEFAULTS, train_model
    from mlx_lm.tuner.datasets import ChatDataset
    model_name = string(request, "model")
    bounds = {"iters": (1, 10000), "batch_size": (1, 16), "num_layers": (1, 256),
              "rank": (1, 128), "max_seq_length": (32, 8192)}
    values = {key: integer(request, key, *limits) for key, limits in bounds.items()}
    train_records, valid_records = read_datasets(string(request, "train_data"), string(request, "valid_data"))
    require(min(len(train_records), len(valid_records)) >= values["batch_size"],
            "each dataset needs at least batch_size records")
    pairs = collect_preference_pairs(train_records)
    sft_records = [{key: value for key, value in record.items() if key != "preference_pairs"}
                   for record in train_records]
    output = Path(string(request, "output_dir")).resolve()
    require(not output.exists() or (request.get("pause_file") and request.get("resume_checkpoint")
            and (output / "resume.json").is_file()), "output_dir must not already exist")
    model_name = resolve_model(model_name)
    model, tokenizer = load_base(model_name)
    keys, ids = query_targets(model, values["num_layers"])
    parameters = {"rank": values["rank"], "dropout": 0.0, "scale": 20.0, "keys": keys}
    expected = expected_weights(model, ids, values["rank"])
    resume = None
    if request.get("resume_adapter") is not None:
        resume = validate_adapter(string(request, "resume_adapter"), model_name,
                                  values["num_layers"], parameters, expected)
    train_set = ChatDataset(sft_records, tokenizer, mask_prompt=True)
    valid_set = ChatDataset(valid_records, tokenizer, mask_prompt=True)
    # Do not silently truncate away the externally verified assistant answer.
    for dataset in (train_set, valid_set):
        for index in range(len(dataset)):
            tokens, offset = dataset.process(dataset[index])
            require(0 <= offset < len(tokens) and 2 <= len(tokens) <= values["max_seq_length"],
                    "training example has no assistant target tokens or exceeds max_seq_length")
    args = copy.deepcopy(CONFIG_DEFAULTS)
    args.update(values)
    args.update(model=model_name, train=True, fine_tune_type="lora", adapter_path=str(output),
                data={"train": str(Path(request["train_data"]).resolve()),
                      "valid": str(Path(request["valid_data"]).resolve())},
                lora_parameters=parameters, resume_adapter_file=str(resume / "adapters.safetensors") if resume else None,
                val_batches=-1, steps_per_report=min(10, values["iters"]),
                steps_per_eval=min(100, values["iters"]), save_every=values["iters"],
                mask_prompt=True, trust_remote_code=False)
    np.random.seed(args["seed"])
    if request.get("pause_file"):
        try:
            pausable_train(request, model, tokenizer, train_set, valid_set, pairs, args, expected)
        except TrainingPaused:
            return {"paused": True}
        validate_adapter(output, model_name, values["num_layers"], parameters, expected)
        return {"adapter": str(output)}
    train_model(SimpleNamespace(**args), model, train_set, valid_set)
    # Without eligible pairs, preserve the established SFT behavior exactly.
    if pairs:
        tokenized_pairs = tokenize_preference_pairs(pairs, tokenizer, values["max_seq_length"])
        if tokenized_pairs:
            import mlx.optimizers as optimizers
            from mlx_lm.tuner import TrainingArgs, train as tuner_train
            iters = preference_iterations(values["iters"], len(tokenized_pairs))
            preference_args = TrainingArgs(
                batch_size=PREFERENCE_PAIRS_PER_BATCH, iters=iters, val_batches=0, steps_per_report=iters,
                steps_per_eval=iters, steps_per_save=iters + 1,
                adapter_file=output / "adapters.safetensors",
                max_seq_length=values["max_seq_length"])
            optimizer = optimizers.Adam(learning_rate=args["learning_rate"])
            tuner_train(model=model, optimizer=optimizer, train_dataset=tokenized_pairs,
                        val_dataset=None, args=preference_args, loss=preference_loss,
                        iterate_batches=iterate_preference_batches)
    validate_adapter(output, model_name, values["num_layers"], parameters, expected)
    return {"adapter": str(output)}


SENSITIVITY_SEED = 0
LAYER_PARAMETER = re.compile(r"(?:^|\.)layers\.(\d+)\.")


def sensitivity_request(request):
    """Validated (model, train_data, rank, samples, max_seq_length)."""
    return (string(request, "model"), string(request, "train_data"),
            integer(request, "rank", 1, 128), integer(request, "samples", 1, 64),
            integer(request, "max_seq_length", 32, 8192))


def layer_scores(squares, layer_count):
    """Per-layer L2 norms from {parameter name: sum of squares}; every parameter must map to a layer."""
    totals = [0.0] * layer_count
    for name, value in squares.items():
        found = LAYER_PARAMETER.search(name)
        require(found is not None and int(found.group(1)) < layer_count,
                "LoRA parameter does not belong to a transformer layer")
        require(math.isfinite(value) and value >= 0.0, "sensitivity produced invalid squared gradients")
        totals[int(found.group(1))] += value
    scores = [math.sqrt(total) for total in totals]
    require(all(math.isfinite(score) for score in scores), "sensitivity produced nonfinite values")
    return scores


def sensitivity(request):
    """MAPLE-style score: norm of the loss gradient w.r.t. each layer's LoRA parameters at the seeded init."""
    import mlx.core as mx
    import mlx.nn as nn
    import numpy as np
    from mlx.utils import tree_flatten, tree_map
    from mlx_lm.tuner.datasets import ChatDataset
    from mlx_lm.tuner.trainer import default_loss
    from mlx_lm.tuner.utils import linear_to_lora_layers
    model_name, train_data, rank, samples, max_seq_length = sensitivity_request(request)
    records, _ = read_records(train_data)
    model_name = resolve_model(model_name)
    model, tokenizer = load_base(model_name)
    keys, _ = query_targets(model)
    layer_count = len(model.layers)
    dataset = ChatDataset(records, tokenizer, mask_prompt=True)
    usable = []
    for index in range(len(dataset)):
        tokens, offset = dataset.process(dataset[index])
        if 0 <= offset < len(tokens) and 2 <= len(tokens) <= max_seq_length:
            usable.append((tokens, offset))
    require(bool(usable), "no training example has assistant target tokens within max_seq_length")
    order = np.random.default_rng(SENSITIVITY_SEED).permutation(len(usable))[:samples]
    mx.random.seed(SENSITIVITY_SEED)
    model.freeze()
    linear_to_lora_layers(model, layer_count, {"rank": rank, "dropout": 0.0, "scale": 20.0, "keys": keys})
    model.train()
    loss_and_grad = nn.value_and_grad(model, default_loss)
    total, total_loss = None, 0.0
    for position in order:
        tokens, offset = usable[int(position)]
        batch = mx.array([tokens])
        lengths = mx.array([[offset, len(tokens)]])
        (loss, _), grads = loss_and_grad(model, batch, lengths)
        total = grads if total is None else tree_map(lambda a, b: a + b, total, grads)
        mx.eval(loss, total)
        value = loss.item()
        require(math.isfinite(value), "sensitivity loss is nonfinite")
        total_loss += value
        mx.clear_cache()
    used = len(order)
    squares = {}
    for name, gradient in tree_flatten(total):
        squares[name] = float(mx.sum(mx.square(gradient.astype(mx.float32) / used)).item())
    return {"layers": layer_scores(squares, layer_count), "loss": total_loss / used, "samples": used}


@contextlib.contextmanager
def diagnostics_to_stderr():
    """Redirect both Python prints and native library fd 1 output."""
    sys.stdout.flush()
    saved = os.dup(1)
    try:
        os.dup2(2, 1)
        with contextlib.redirect_stdout(sys.stderr):
            yield
    finally:
        sys.stdout.flush()
        os.dup2(saved, 1)
        os.close(saved)


def main():
    watch_parent()
    phase = "request validation"
    try:
        payload = sys.stdin.buffer.read(MAX_REQUEST + 1)
        require(len(payload) <= MAX_REQUEST, "request exceeds 8 MiB")
        try:
            request = json.loads(payload)
        except (ValueError, UnicodeError):
            raise RequestError("request is not valid JSON") from None
        require(isinstance(request, dict), "request must be an object")
        from dial_steering import operation as steering_operation
        operations = {"steering_extract":steering_operation,"steering_evaluate":steering_operation,"capabilities": capabilities, "generate": generate, "train": train,
                      "sensitivity": sensitivity}
        operation = request.get("op")
        require(isinstance(operation, str) and operation in operations, "unsupported operation")
        phase = operation
        with diagnostics_to_stderr():
            result = operations[operation](request)
        sys.stdout.write(json.dumps(result, allow_nan=False) + "\n")
    except Exception as error:
        detail = str(error) if isinstance(error, RequestError) or type(error).__name__ == "SteeringError" else type(error).__name__
        print(f"dial-worker: {phase} failed: {detail}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
