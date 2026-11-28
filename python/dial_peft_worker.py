"""One-shot cross-platform PyTorch/PEFT + EXL3 worker (CUDA/Linux/Windows/CPU).

Mirrors `dial_worker.py`'s JSON-over-stdin/stdout wire contract so Dial's Rust orchestration
drives Apple Silicon MLX and NVIDIA CUDA PyTorch/PEFT + ExLlamaV3 identically.
"""
import contextlib
import hashlib
import importlib.util
import json
import math
import os
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

UNSUPPORTED = [
    "expert_growth", "routing_calibration", "attention_retrofit",
    "learned_step_probe",
]
LORA_PROBE_VERSION = 3
MAX_REQUEST = 8 * 1024 * 1024
MAX_DATA = 128 * 1024 * 1024
ATTENTION_PROJECTIONS = frozenset({
    "q_proj", "k_proj", "v_proj", "o_proj", "out_proj",
    "qkv_proj", "in_proj", "wq", "wk", "wv", "wo",
})
LAYER_RE = re.compile(r"(?:^|\.)layers\.(\d+)\.")


class RequestError(ValueError):
    """Safe validation error naming fields/positions, never sensitive payload content."""


def require(condition, message):
    if not condition:
        raise RequestError(message)


def string(request, key):
    value = request.get(key)
    require(isinstance(value, str) and bool(value.strip()), f"{key} must be nonempty")
    return value


def integer(request, key, low, high):
    value = request.get(key)
    require(type(value) is int and low <= value <= high,
            f"{key} must be an integer in [{low}, {high}]")
    return value


def opt_float(request, key, default, low, high):
    value = request.get(key, default)
    require(type(value) in (int, float) and math.isfinite(value) and low <= float(value) <= high,
            f"{key} must be finite in [{low}, {high}]")
    return float(value)


def opt_int(request, key, default, low, high):
    value = request.get(key, default)
    require(type(value) is int and low <= value <= high,
            f"{key} must be an integer in [{low}, {high}]")
    return value


def opt_bool(request, key, default):
    value = request.get(key, default)
    require(type(value) is bool, f"{key} must be a boolean")
    return value


def watch_parent():
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


def atomic_json(path, value):
    path = Path(path)
    temporary = path.with_suffix(path.suffix + ".tmp")
    with temporary.open("w") as handle:
        json.dump(value, handle)
        handle.flush()
        os.fsync(handle.fileno())
    os.replace(temporary, path)


def is_exl3_dir(path):
    cfg_path = Path(path) / "config.json"
    if not cfg_path.is_file():
        return False
    try:
        cfg = json.loads(cfg_path.read_text())
    except (OSError, ValueError):
        return False
    qcfg = cfg.get("quantization_config") or {}
    method = str(qcfg.get("quant_method") or "").lower()
    return method in ("exl3", "exllamav3") or "exl3_config" in cfg or "exl3_config" in qcfg


def has_package(name):
    return importlib.util.find_spec(name) is not None


def resolve_target_modules(model, target_mode="all-linear"):
    """Resolve leaf projection names for HuggingFace PEFT `LoraConfig.target_modules`."""
    import torch.nn as nn

    mode = (target_mode or "all-linear").strip()
    require(bool(mode), "target_modules must not be empty")
    candidates = set()
    for name, module in model.named_modules():
        if not isinstance(module, nn.Linear):
            continue
        if not LAYER_RE.search(name):
            continue
        leaf = name.rsplit(".", 1)[-1]
        candidates.add(leaf)
    require(bool(candidates), "model exposes no trainable transformer linear layers")

    if mode == "all-linear":
        return sorted(candidates)
    if mode == "attention":
        matched = sorted(c for c in candidates if c in ATTENTION_PROJECTIONS)
        return matched or sorted(candidates)
    if mode == "q-proj":
        if "q_proj" in candidates:
            return ["q_proj"]
        out = [c for c in ("o_proj", "out_proj") if c in candidates]
        return out or sorted(candidates)
    custom = {part.strip() for part in mode.split(",") if part.strip()}
    matched = sorted(c for c in candidates if c in custom)
    require(bool(matched), "target_modules matched no linear projections in this model")
    return matched


def count_layers(model):
    indices = set()
    for name, _ in model.named_modules():
        match = LAYER_RE.search(name)
        if match:
            indices.add(int(match.group(1)))
    return max(indices) + 1 if indices else 0


def read_records(path):
    records, hashes = [], set()
    total = 0
    with Path(path).open("rb") as source:
        for number, line in enumerate(iter(lambda: source.readline(MAX_DATA + 1), b""), 1):
            total += len(line)
            require(total <= MAX_DATA, "dataset exceeds 128 MiB")
            require(bool(line.strip()), f"dataset line {number} is empty")
            record = json.loads(line)
            require(isinstance(record, dict) and record.get("verified") is True,
                    f"dataset line {number} is not verified")
            chat = record.get("messages")
            require(isinstance(chat, list) and len(chat) >= 2 and chat[-1].get("role") == "assistant",
                    f"dataset line {number} has invalid training messages")
            digest = hashlib.sha256(json.dumps(chat, sort_keys=True, ensure_ascii=False,
                                               separators=(",", ":")).encode()).hexdigest()
            hashes.add(digest)
            entry = {"messages": chat}
            if "tools" in record:
                entry["tools"] = record["tools"]
            records.append(entry)
    require(bool(records), "dataset must not be empty")
    return records, hashes


def encode(tokenizer, chat, tools=None):
    full = tokenizer.apply_chat_template(chat, tools=tools, add_generation_prompt=False, tokenize=True)
    prompt = tokenizer.apply_chat_template(chat[:-1], tools=tools, add_generation_prompt=True, tokenize=True)
    require(isinstance(prompt, list) and isinstance(full, list), "chat template did not return tokens")
    offset = next((i for i, (a, b) in enumerate(zip(prompt, full)) if a != b), min(len(prompt), len(full)))
    return full, offset


def capabilities(request):
    model_name = string(request, "model")
    path = Path(model_name).resolve()
    resolved = str(path) if path.exists() else None
    exl3_pkg = has_package("exllamav3")
    torch_ok = has_package("torch")
    peft_ok = has_package("peft")
    transformers_ok = has_package("transformers")

    cuda_ok = False
    if torch_ok:
        import torch
        cuda_ok = bool(torch.cuda.is_available())

    if resolved and Path(resolved).is_dir() and is_exl3_dir(resolved):
        return {
            "inference": cuda_ok and (exl3_pkg or bool(os.environ.get("DIAL_EXL3_SERVER"))),
            "lora": False,
            "lora_probe_version": LORA_PROBE_VERSION,
            "backend": "peft",
            "cuda": cuda_ok,
            "exl3": True,
            "reason": (
                "EXL3 quantized checkpoints serve PEFT LoRA adapters via ExLlamaV3/TabbyAPI on CUDA, "
                "but QLoRA training requires the unquantized HF/safetensors base checkpoint (then export PEFT or EXL3)."
            ),
            "unsupported": UNSUPPORTED,
            "resolved_model": resolved,
        }

    if not (torch_ok and transformers_ok and peft_ok):
        missing = [p for p, ok in (("torch", torch_ok), ("transformers", transformers_ok), ("peft", peft_ok)) if not ok]
        return {
            "inference": False,
            "lora": False,
            "lora_probe_version": LORA_PROBE_VERSION,
            "backend": "peft",
            "cuda": cuda_ok,
            "exl3": False,
            "reason": f"PyTorch/PEFT backend missing Python packages: {', '.join(missing)}",
            "unsupported": UNSUPPORTED,
            "resolved_model": resolved,
        }

    if not resolved or not Path(resolved).is_dir():
        return {
            "inference": False,
            "lora": False,
            "lora_probe_version": LORA_PROBE_VERSION,
            "backend": "peft",
            "cuda": cuda_ok,
            "exl3": False,
            "reason": "PEFT backend requires a local HuggingFace safetensors directory",
            "unsupported": UNSUPPORTED,
            "resolved_model": resolved,
        }

    try:
        from transformers import AutoTokenizer
        tokenizer = AutoTokenizer.from_pretrained(resolved, local_files_only=True, trust_remote_code=False)
        tokenizer.apply_chat_template([{"role": "user", "content": "Hello"}],
                                      add_generation_prompt=True, tokenize=True)
    except Exception as error:
        return {
            "inference": False,
            "lora": False,
            "lora_probe_version": LORA_PROBE_VERSION,
            "backend": "peft",
            "cuda": cuda_ok,
            "exl3": False,
            "reason": f"Tokenizer or chat template unavailable ({type(error).__name__})",
            "unsupported": UNSUPPORTED,
            "resolved_model": resolved,
        }

    device_note = "CUDA" if cuda_ok else "CPU (no CUDA GPU detected)"
    return {
        "inference": True,
        "lora": True,
        "lora_probe_version": LORA_PROBE_VERSION,
        "backend": "peft",
        "cuda": cuda_ok,
        "exl3": exl3_pkg,
        "reason": f"PyTorch/PEFT LoRA/DoRA ready on {device_note}; produces PEFT adapters compatible with ExLlamaV3/TabbyAPI & GGUF export",
        "unsupported": UNSUPPORTED,
        "resolved_model": resolved,
    }


def _select_device_and_dtype():
    import torch
    if torch.cuda.is_available():
        dtype = torch.bfloat16 if torch.cuda.is_bf16_supported() else torch.float16
        return torch.device("cuda"), dtype
    return torch.device("cpu"), torch.float32


def _lr_multiplier(step, total_steps, schedule, warmup_ratio):
    if schedule != "cosine" or total_steps <= 1:
        return 1.0
    warmup = min(int(round(total_steps * warmup_ratio)), max(0, total_steps - 1))
    if warmup > 0 and step < warmup:
        return 0.01 + 0.99 * ((step + 1) / warmup)
    decay_steps = max(1, total_steps - warmup)
    progress = min(1.0, max(0.0, (step - warmup) / decay_steps))
    return 0.1 + 0.9 * 0.5 * (1.0 + math.cos(math.pi * progress))


def train(request):
    import torch
    from peft import LoraConfig, PeftModel, TaskType, get_peft_model
    from transformers import AutoModelForCausalLM, AutoTokenizer

    model_name = str(Path(string(request, "model")).resolve(strict=True))
    require(not is_exl3_dir(model_name),
            "cannot train directly on an EXL3 quantized checkpoint; train on the FP16/BF16 safetensors base and export PEFT or EXL3")

    iters = integer(request, "iters", 1, 10000)
    batch_size = integer(request, "batch_size", 1, 16)
    num_layers = integer(request, "num_layers", 1, 256)
    rank = integer(request, "rank", 1, 128)
    max_seq_length = integer(request, "max_seq_length", 32, 8192)
    fine_tune_type = str(request.get("fine_tune_type", "lora"))
    require(fine_tune_type in ("lora", "dora"), "fine_tune_type must be lora or dora")
    target_modules = str(request.get("target_modules", "all-linear"))
    learning_rate = opt_float(request, "learning_rate", 1e-4, 1e-6, 1e-2)
    lr_schedule = str(request.get("lr_schedule", "cosine"))
    require(lr_schedule in ("cosine", "constant"), "lr_schedule must be cosine or constant")
    warmup_ratio = opt_float(request, "warmup_ratio", 0.1, 0.0, 0.5)
    optimizer_name = str(request.get("optimizer", "adamw"))
    require(optimizer_name in ("adamw", "adam"), "optimizer must be adamw or adam")
    weight_decay = opt_float(request, "weight_decay", 0.01, 0.0, 1.0)
    dropout = opt_float(request, "dropout", 0.0, 0.0, 0.5)
    scale = opt_float(request, "scale", 20.0, 0.1, 128.0)
    grad_checkpoint = opt_bool(request, "grad_checkpoint", True)
    grad_accum = opt_int(request, "grad_accumulation_steps", 4, 1, 64)
    checkpoint_every = opt_int(request, "checkpoint_every", 8, 1, 1000)

    train_records, train_hashes = read_records(string(request, "train_data"))
    valid_records, valid_hashes = read_records(string(request, "valid_data"))
    require(train_hashes.isdisjoint(valid_hashes), "training and validation messages overlap")
    output = Path(string(request, "output_dir")).resolve()
    pause_path = Path(request["pause_file"]) if request.get("pause_file") else None
    progress_path = Path(request["progress_file"]) if request.get("progress_file") else None
    require(not output.exists() or (pause_path and request.get("resume_checkpoint") and (output / "resume.json").is_file()),
            "output_dir must not already exist")

    device, dtype = _select_device_and_dtype()
    tokenizer = AutoTokenizer.from_pretrained(model_name, local_files_only=True, trust_remote_code=False)
    base_model = AutoModelForCausalLM.from_pretrained(
        model_name, torch_dtype=dtype, local_files_only=True, trust_remote_code=False
    )
    if grad_checkpoint and hasattr(base_model, "gradient_checkpointing_enable"):
        base_model.gradient_checkpointing_enable()
        if hasattr(base_model.config, "use_cache"):
            base_model.config.use_cache = False

    total_layers = count_layers(base_model)
    effective_layers = min(num_layers, total_layers) if total_layers > 0 else num_layers
    layers_to_transform = list(range(total_layers - effective_layers, total_layers)) if total_layers > 0 else None

    if request.get("resume_adapter"):
        model = PeftModel.from_pretrained(base_model, string(request, "resume_adapter"), is_trainable=True)
    else:
        targets = resolve_target_modules(base_model, target_modules)
        lora_cfg = LoraConfig(
            task_type=TaskType.CAUSAL_LM,
            r=rank,
            lora_alpha=scale * rank,
            lora_dropout=dropout,
            target_modules=targets,
            layers_to_transform=layers_to_transform,
            use_dora=(fine_tune_type == "dora"),
            bias="none",
        )
        model = get_peft_model(base_model, lora_cfg)

    model.to(device)
    model.train()

    encoded = []
    for rec in train_records:
        tokens, offset = encode(tokenizer, rec["messages"], rec.get("tools"))
        require(0 < offset < len(tokens) and 2 <= len(tokens) <= max_seq_length,
                "training example has no assistant target tokens or exceeds max_seq_length")
        encoded.append((tokens, offset))

    trainable = [p for p in model.parameters() if p.requires_grad]
    if optimizer_name == "adamw":
        optimizer = torch.optim.AdamW(trainable, lr=learning_rate, weight_decay=weight_decay)
    else:
        optimizer = torch.optim.Adam(trainable, lr=learning_rate)

    total_opt_steps = max(1, iters // grad_accum)
    start_iter = 0
    if pause_path and request.get("resume_checkpoint") and (output / "resume.json").is_file():
        meta = json.loads((output / "resume.json").read_text())
        start_iter = int(meta["completed"])
        ckpt_dir = (output / meta["directory"]).resolve()
        opt_state = torch.load(ckpt_dir / "optimizer.pt", map_location=device, weights_only=True)
        optimizer.load_state_dict(opt_state)

    output.mkdir(parents=True, exist_ok=True)
    optimizer.zero_grad(set_to_none=True)
    opt_step = start_iter // grad_accum

    for iteration in range(start_iter + 1, iters + 1):
        batch_items = [encoded[(iteration - 1 + b) % len(encoded)] for b in range(batch_size)]
        max_len = max(len(t) for t, _ in batch_items)
        pad_id = tokenizer.pad_token_id if tokenizer.pad_token_id is not None else 0
        input_ids = torch.full((batch_size, max_len), pad_id, dtype=torch.long, device=device)
        labels = torch.full((batch_size, max_len), -100, dtype=torch.long, device=device)
        attention_mask = torch.zeros((batch_size, max_len), dtype=torch.long, device=device)
        for row, (tokens, offset) in enumerate(batch_items):
            length = len(tokens)
            input_ids[row, :length] = torch.tensor(tokens, dtype=torch.long, device=device)
            labels[row, offset:length] = torch.tensor(tokens[offset:], dtype=torch.long, device=device)
            attention_mask[row, :length] = 1

        out = model(input_ids=input_ids, attention_mask=attention_mask, labels=labels)
        loss = out.loss / grad_accum
        require(torch.isfinite(loss).item(), "training produced nonfinite loss")
        loss.backward()

        on_step = (iteration % grad_accum == 0) or (iteration == iters)
        if on_step:
            lr_now = learning_rate * _lr_multiplier(opt_step, total_opt_steps, lr_schedule, warmup_ratio)
            for group in optimizer.param_groups:
                group["lr"] = lr_now
            torch.nn.utils.clip_grad_norm_(trainable, 1.0)
            optimizer.step()
            optimizer.zero_grad(set_to_none=True)
            opt_step += 1

        if progress_path:
            atomic_json(progress_path, {
                "stage": f"training sft {iteration}/{iters}",
                "phase": "sft",
                "iteration": iteration,
                "total": iters,
                "train_loss": float(loss.item() * grad_accum),
                "peak_memory_gb": (torch.cuda.max_memory_allocated() / 1e9) if torch.cuda.is_available() else None,
            })

        paused = bool(pause_path and pause_path.exists())
        if on_step and pause_path and (paused or iteration == iters or iteration % checkpoint_every == 0):
            ckpt_root = output / "checkpoints"
            ckpt_root.mkdir(exist_ok=True)
            saved = Path(tempfile.mkdtemp(prefix="step-", dir=ckpt_root))
            model.save_pretrained(str(saved))
            torch.save(optimizer.state_dict(), saved / "optimizer.pt")
            atomic_json(output / "resume.json", {
                "phase": "sft", "completed": iteration, "directory": str(saved.relative_to(output))
            })
            if paused:
                return {"paused": True}

    model.save_pretrained(str(output))
    return {"adapter": str(output), "format": "peft"}


def fuse(request):
    import torch
    from peft import PeftModel
    from transformers import AutoModelForCausalLM, AutoTokenizer

    model_name = str(Path(string(request, "model")).resolve(strict=True))
    adapter = string(request, "adapter")
    output = Path(string(request, "output")).resolve()
    require(output.is_absolute() and not output.exists(), "output must be a new absolute path")
    require(output.parent.is_dir(), "output parent must exist")

    tokenizer = AutoTokenizer.from_pretrained(model_name, local_files_only=True, trust_remote_code=False)
    base = AutoModelForCausalLM.from_pretrained(
        model_name, torch_dtype=torch.float16, local_files_only=True, trust_remote_code=False
    )
    peft_model = PeftModel.from_pretrained(base, adapter)
    merged = peft_model.merge_and_unload()
    staging = Path(tempfile.mkdtemp(prefix=".fuse-", dir=output.parent))
    try:
        merged.save_pretrained(str(staging))
        tokenizer.save_pretrained(str(staging))
        os.rename(staging, output)
    except BaseException:
        shutil.rmtree(staging, ignore_errors=True)
        raise
    return {
        "output": str(output),
        "model_type": getattr(merged.config, "model_type", None),
        "layers": count_layers(merged),
        "dequantized": True,
    }


def export_exl3(request):
    """Quantize a fused (or adapter+base) HuggingFace safetensors checkpoint to EXL3 format (CUDA required)."""
    model_name = str(Path(string(request, "model")).resolve(strict=True))
    adapter = request.get("adapter")
    output = Path(string(request, "output")).resolve()
    bpw = opt_float(request, "bpw", 4.0, 1.5, 8.0)
    require(output.is_absolute() and not output.exists(), "output must be a new absolute path")
    require(output.parent.is_dir(), "output parent must exist")

    if not has_package("torch"):
        raise RequestError("EXL3 export requires PyTorch with NVIDIA CUDA and the exllamav3 package")
    import torch
    if not torch.cuda.is_available():
        raise RequestError("EXL3 quantization requires an NVIDIA CUDA GPU (not available on this host)")
    if not has_package("exllamav3"):
        raise RequestError("exllamav3 Python package is not installed; install exllamav3 on your CUDA host")

    with tempfile.TemporaryDirectory(prefix=".exl3-", dir=output.parent) as scratch:
        source_dir = Path(model_name)
        if adapter:
            fused_dir = Path(scratch) / "fused"
            fuse({"model": model_name, "adapter": adapter, "output": str(fused_dir), "dequantize": True})
            source_dir = fused_dir
        work_dir = Path(scratch) / "work"
        work_dir.mkdir()
        staged_out = Path(scratch) / "exl3_out"
        cmd = [
            sys.executable, "-m", "exllamav3.conversion.convert",
            "-i", str(source_dir),
            "-o", str(staged_out),
            "-w", str(work_dir),
            "-b", f"{bpw:.2f}",
        ]
        proc = subprocess.run(cmd, capture_output=True, text=True)
        if proc.returncode != 0 or not staged_out.is_dir():
            tail = (proc.stderr or proc.stdout or "")[-600:].strip()
            raise RequestError(f"exllamav3 quantization failed: {tail or 'unknown error'}")
        os.rename(staged_out, output)

    return {"output": str(output), "format": "exl3", "bpw": bpw}


def export_peft_op(request):
    import dial_lora_export
    try:
        return dial_lora_export.export_peft(
            string(request, "model"), string(request, "adapter"), string(request, "output")
        )
    except dial_lora_export.ExportError as error:
        raise RequestError(str(error)) from None


def export_gguf_lora_op(request):
    import dial_lora_export
    try:
        return dial_lora_export.export_gguf_lora(
            string(request, "model"),
            string(request, "adapter"),
            string(request, "output"),
            converter_script=request.get("converter_script"),
            dtype=request.get("dtype", "f32"),
        )
    except dial_lora_export.ExportError as error:
        raise RequestError(str(error)) from None


def export_gguf_op(request):
    import dial_lora_export
    model_name = string(request, "model")
    adapter = string(request, "adapter")
    output = Path(string(request, "output")).resolve()
    require(output.is_absolute() and not output.exists(), "output must be a new absolute path")
    require(output.parent.is_dir(), "output parent must exist")
    with tempfile.TemporaryDirectory(prefix=".fuse-for-gguf-", dir=output.parent) as scratch:
        fused_dir = Path(scratch) / "fused"
        fuse({"model": model_name, "adapter": adapter, "output": str(fused_dir), "dequantize": True})
        try:
            return dial_lora_export.export_gguf_fused(
                fused_dir,
                output,
                quant=request.get("quant", "f16"),
                converter_script=request.get("converter_script"),
                quantize_bin=request.get("quantize_bin"),
                uv_bin=request.get("uv_bin"),
            )
        except dial_lora_export.ExportError as error:
            raise RequestError(str(error)) from None


def prepare_gguf_op(request):
    import dial_lora_export
    try:
        return dial_lora_export.prepare_gguf(
            string(request, "source"), string(request, "output"), uv_bin=request.get("uv_bin")
        )
    except dial_lora_export.ExportError as error:
        raise RequestError(str(error)) from None


@contextlib.contextmanager
def diagnostics_to_stderr():
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
        request = json.loads(payload)
        require(isinstance(request, dict), "request must be an object")
        operations = {
            "capabilities": capabilities,
            "train": train,
            "fuse": fuse,
            "export_peft": export_peft_op,
            "export_gguf_lora": export_gguf_lora_op,
            "export_gguf": export_gguf_op,
            "export_exl3": export_exl3,
            "prepare_gguf": prepare_gguf_op,
        }
        operation = request.get("op")
        require(isinstance(operation, str) and operation in operations, "unsupported operation")
        phase = operation
        with diagnostics_to_stderr():
            result = operations[operation](request)
        sys.stdout.write(json.dumps(result, allow_nan=False) + "\n")
    except Exception as error:
        detail = str(error) if isinstance(error, RequestError) else f"{type(error).__name__}: {error}"
        print(f"dial-peft-worker: {phase} failed: {detail}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
