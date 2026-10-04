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


def query_targets(model, count=None):
    """Only explicit, separately represented q_proj modules are supported."""
    import mlx.nn as nn
    layers = getattr(model, "layers", None)
    require(layers is not None and len(layers) > 0, "model exposes no trainable transformer layers")
    if count is not None:
        require(count <= len(layers), "num_layers exceeds model layer count")
    selected = layers if count is None else layers[-count:]
    keys, modules = set(), []
    for layer in selected:
        found = [(name, module) for name, module in layer.named_modules()
                 if name.rsplit(".", 1)[-1] == "q_proj"
                 and isinstance(module, (nn.Linear, nn.QuantizedLinear))]
        require(bool(found), "selected layer lacks a supported separate q_proj; fused QKV is unsupported")
        keys.update(name for name, _ in found)
        modules.extend(module for _, module in found)
    # Guard against accidental matches outside the selected transformer blocks.
    selected_ids = {id(module) for module in modules}
    require(not any(name in keys and id(module) not in selected_ids
                    for name, module in model.named_modules()),
            "ambiguous query projection paths")
    return sorted(keys), selected_ids


def expected_weights(model, selected_ids, rank):
    import mlx.nn as nn
    expected = {}
    for name, module in model.named_modules():
        if id(module) not in selected_ids:
            continue
        output_dim, input_dim = module.weight.shape
        if isinstance(module, nn.QuantizedLinear):
            input_dim = input_dim * 32 // module.bits
        expected[name + ".lora_a"] = (input_dim, rank)
        expected[name + ".lora_b"] = (rank, output_dim)
    require(bool(expected), "no query projection adapter parameters found")
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
        keys, _ = query_targets(model)
    except RequestError as error:
        return {"inference": True, "lora": False, "reason": str(error),
                "unsupported": UNSUPPORTED, "resolved_model": resolved}
    return {"inference": True, "lora": True,
            "reason": "MLX model and chat template loaded; query-only LoRA/QLoRA targets: " + ", ".join(keys),
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
                "adapter must use supported query-only LoRA parameters")
        validate_adapter(directory, model_name, count, parameters, expected_weights(model, ids, rank))
        load_adapters(model, str(directory))
    prompt = tokenizer.apply_chat_template(chat, add_generation_prompt=True, tokenize=True)
    mx.random.seed(seed)
    text = mlx_generate(model, tokenizer, prompt=prompt, max_tokens=limit,
                        sampler=make_sampler(temp=temperature), verbose=False)
    require(isinstance(text, str), "backend returned non-text generation")
    return {"text": text}


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
    require(not output.exists(), "output_dir must not already exist")
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
    phase = "request validation"
    try:
        payload = sys.stdin.buffer.read(MAX_REQUEST + 1)
        require(len(payload) <= MAX_REQUEST, "request exceeds 8 MiB")
        try:
            request = json.loads(payload)
        except (ValueError, UnicodeError):
            raise RequestError("request is not valid JSON") from None
        require(isinstance(request, dict), "request must be an object")
        operations = {"capabilities": capabilities, "generate": generate, "train": train,
                      "sensitivity": sensitivity}
        operation = request.get("op")
        require(isinstance(operation, str) and operation in operations, "unsupported operation")
        phase = operation
        with diagnostics_to_stderr():
            result = operations[operation](request)
        sys.stdout.write(json.dumps(result, allow_nan=False) + "\n")
    except Exception as error:
        detail = str(error) if isinstance(error, RequestError) else type(error).__name__
        print(f"dial-worker: {phase} failed: {detail}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
