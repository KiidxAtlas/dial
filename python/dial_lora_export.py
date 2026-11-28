"""Cross-format LoRA & checkpoint export/preparation helpers (PEFT, GGUF LoRA, Fused GGUF, EXL3).

Shared by both `dial_worker.py` (Apple Silicon MLX) and `dial_peft_worker.py` (CUDA PyTorch/PEFT).
All outputs are staged in a private sibling directory/file and published atomically via rename.
"""
import json
import math
import os
import re
import shutil
import struct
import subprocess
import tempfile
from pathlib import Path

# Standard HuggingFace -> GGUF (llama.cpp) projection names inside transformer blocks.
HF_TO_GGUF_PROJ = {
    "self_attn.q_proj": "attn_q",
    "self_attn.k_proj": "attn_k",
    "self_attn.v_proj": "attn_v",
    "self_attn.o_proj": "attn_output",
    "self_attn.qkv_proj": "attn_qkv",
    "attention.wq": "attn_q",
    "attention.wk": "attn_k",
    "attention.wv": "attn_v",
    "attention.wo": "attn_output",
    "mlp.gate_proj": "ffn_gate",
    "mlp.up_proj": "ffn_up",
    "mlp.down_proj": "ffn_down",
    "mlp.gate_up_proj": "ffn_up",
    "feed_forward.w1": "ffn_gate",
    "feed_forward.w3": "ffn_up",
    "feed_forward.w2": "ffn_down",
    "self_attn.out_proj": "attn_output",
}

MODEL_TYPE_TO_GGUF_ARCH = {
    "llama": "llama",
    "mistral": "llama",
    "mixtral": "llama",
    "qwen2": "qwen2",
    "qwen2_moe": "qwen2moe",
    "qwen3": "qwen3",
    "qwen3_moe": "qwen3moe",
    "gemma": "gemma",
    "gemma2": "gemma2",
    "gemma3": "gemma3",
    "gemma3_text": "gemma3",
    "phi3": "phi3",
    "deepseek_v2": "deepseek2",
    "deepseek_v3": "deepseek2",
    "starcoder2": "starcoder2",
    "cohere": "command-r",
    "gpt2": "gpt2",
}

GGUF_MAGIC = b"GGUF"
GGUF_VERSION = 3
GGUF_ALIGNMENT = 32
GGUF_TYPE_UINT32 = 4
GGUF_TYPE_FLOAT32 = 6
GGUF_TYPE_STRING = 8
GGML_TYPE_F32 = 0
GGML_TYPE_F16 = 1

LAYER_RE = re.compile(r"(?:^|\.)layers\.(\d+)\.(.+)$")


class ExportError(ValueError):
    """Contract or conversion error with safe, user-actionable message."""


def _require(condition, message):
    if not condition:
        raise ExportError(message)


def _model_type(model_dir):
    config_path = Path(model_dir) / "config.json"
    if not config_path.is_file():
        return "llama"
    try:
        config = json.loads(config_path.read_text())
        return str(config.get("model_type") or "llama").lower()
    except (OSError, ValueError):
        return "llama"


def _load_safetensors_numpy(path):
    from safetensors.numpy import load_file
    return load_file(str(path))


def _save_safetensors_numpy(tensors, path):
    from safetensors.numpy import save_file
    save_file(tensors, str(path))


def _parse_mlx_or_peft_adapter(adapter_dir):
    """Load an MLX or PEFT adapter into canonical PyTorch-oriented (r, in)/(out, r) numpy tensors.

    Returns dict:
        fine_tune_type: "lora" | "dora"
        rank: int
        lora_alpha: float
        dropout: float
        tensors: dict mapping relative module path (e.g. "model.layers.0.self_attn.q_proj") ->
                 {"lora_A": np.ndarray (r, in), "lora_B": np.ndarray (out, r), optional "m": (out,)}
    """
    import numpy as np

    directory = Path(adapter_dir).resolve(strict=True)
    _require(directory.is_dir(), "adapter must be a directory")
    config_file = directory / "adapter_config.json"
    _require(config_file.is_file(), "adapter_config.json is missing")
    config = json.loads(config_file.read_text())
    _require(isinstance(config, dict), "adapter_config.json must be a JSON object")

    if (directory / "adapters.safetensors").is_file() and "lora_parameters" in config:
        # MLX-LM adapter format
        fine_tune_type = str(config.get("fine_tune_type", "lora")).lower()
        _require(fine_tune_type in ("lora", "dora"), f"unsupported MLX fine_tune_type: {fine_tune_type}")
        params = config["lora_parameters"]
        rank = int(params["rank"])
        scale = float(params["scale"])
        dropout = float(params.get("dropout", 0.0))
        _require(rank > 0 and math.isfinite(scale) and scale > 0, "invalid MLX LoRA rank or scale")
        raw = _load_safetensors_numpy(directory / "adapters.safetensors")
        grouped = {}
        for name, arr in raw.items():
            if name.endswith(".lora_a"):
                base = name[:-7]
                _require(arr.ndim == 2, f"cannot export {arr.ndim}D MoE SwitchLinear tensor {name} to PEFT/GGUF")
                grouped.setdefault(base, {})["lora_A"] = np.ascontiguousarray(arr.astype(np.float32).T)
            elif name.endswith(".lora_b"):
                base = name[:-7]
                _require(arr.ndim == 2, f"cannot export {arr.ndim}D MoE SwitchLinear tensor {name} to PEFT/GGUF")
                grouped.setdefault(base, {})["lora_B"] = np.ascontiguousarray(arr.astype(np.float32).T)
            elif name.endswith(".m") and fine_tune_type == "dora":
                base = name[:-2]
                grouped.setdefault(base, {})["m"] = np.ascontiguousarray(arr.astype(np.float32))
            else:
                raise ExportError(f"unexpected adapter tensor: {name}")
        _require(bool(grouped), "adapter contains no tensors")
        for base, pair in grouped.items():
            _require("lora_A" in pair and "lora_B" in pair, f"incomplete LoRA pair for {base}")
            _require(pair["lora_A"].shape[0] == rank and pair["lora_B"].shape[1] == rank,
                     f"rank mismatch for {base}")
            _require(bool(np.all(np.isfinite(pair["lora_A"])) and np.all(np.isfinite(pair["lora_B"]))),
                     f"nonfinite values in {base}")
        return {
            "fine_tune_type": fine_tune_type,
            "rank": rank,
            "lora_alpha": scale * rank,
            "dropout": dropout,
            "tensors": grouped,
        }

    if (directory / "adapter_model.safetensors").is_file() and config.get("peft_type") == "LORA":
        # Already HuggingFace PEFT format
        fine_tune_type = "dora" if config.get("use_dora") else "lora"
        rank = int(config["r"])
        lora_alpha = float(config.get("lora_alpha", rank))
        dropout = float(config.get("lora_dropout", 0.0))
        raw = _load_safetensors_numpy(directory / "adapter_model.safetensors")
        grouped = {}
        for name, arr in raw.items():
            clean = name
            if clean.startswith("base_model.model."):
                clean = clean[len("base_model.model."):]
            if clean.endswith(".lora_A.weight") or clean.endswith(".lora_A.default.weight"):
                base = clean.rsplit(".lora_A.", 1)[0]
                grouped.setdefault(base, {})["lora_A"] = np.ascontiguousarray(arr.astype(np.float32))
            elif clean.endswith(".lora_B.weight") or clean.endswith(".lora_B.default.weight"):
                base = clean.rsplit(".lora_B.", 1)[0]
                grouped.setdefault(base, {})["lora_B"] = np.ascontiguousarray(arr.astype(np.float32))
            elif ".lora_magnitude_vector" in clean:
                base = clean.split(".lora_magnitude_vector", 1)[0]
                grouped.setdefault(base, {})["m"] = np.ascontiguousarray(arr.astype(np.float32))
        _require(bool(grouped), "PEFT adapter contains no LoRA tensors")
        return {
            "fine_tune_type": fine_tune_type,
            "rank": rank,
            "lora_alpha": lora_alpha,
            "dropout": dropout,
            "tensors": grouped,
        }

    raise ExportError("adapter directory is neither a valid MLX nor PEFT LoRA adapter")


def export_peft(model_name, adapter_dir, output_dir):
    """Export an MLX (or PEFT) adapter to standard HuggingFace PEFT format.

    Produces `adapter_config.json` + `adapter_model.safetensors` compatible with
    ExLlamaV3 / TabbyAPI, vLLM, HuggingFace PEFT, and llama.cpp's `convert_lora_to_gguf.py`.
    """
    output = Path(output_dir).resolve()
    _require(not output.exists(), "output path must not already exist")
    _require(output.parent.is_dir(), "output parent directory must exist")
    parsed = _parse_mlx_or_peft_adapter(adapter_dir)

    peft_tensors = {}
    target_modules = set()
    layer_indices = set()
    for module_path, pair in sorted(parsed["tensors"].items()):
        prefix = f"base_model.model.{module_path}"
        peft_tensors[f"{prefix}.lora_A.weight"] = pair["lora_A"]
        peft_tensors[f"{prefix}.lora_B.weight"] = pair["lora_B"]
        if "m" in pair:
            peft_tensors[f"{prefix}.lora_magnitude_vector"] = pair["m"]
        leaf = module_path.rsplit(".", 1)[-1]
        target_modules.add(leaf)
        match = LAYER_RE.search(module_path)
        if match:
            layer_indices.add(int(match.group(1)))

    adapter_config = {
        "peft_type": "LORA",
        "task_type": "CAUSAL_LM",
        "auto_mapping": None,
        "base_model_name_or_path": str(model_name),
        "bias": "none",
        "fan_in_fan_out": False,
        "inference_mode": True,
        "r": parsed["rank"],
        "lora_alpha": parsed["lora_alpha"],
        "lora_dropout": parsed["dropout"],
        "target_modules": sorted(target_modules),
        "use_dora": parsed["fine_tune_type"] == "dora",
    }
    if layer_indices:
        adapter_config["layers_to_transform"] = sorted(layer_indices)

    staging = Path(tempfile.mkdtemp(prefix=".peft-", dir=output.parent))
    try:
        (staging / "adapter_config.json").write_text(json.dumps(adapter_config, indent=2) + "\n")
        _save_safetensors_numpy(peft_tensors, staging / "adapter_model.safetensors")
        os.rename(staging, output)
    except BaseException:
        shutil.rmtree(staging, ignore_errors=True)
        raise

    return {
        "output": str(output),
        "format": "peft",
        "fine_tune_type": parsed["fine_tune_type"],
        "rank": parsed["rank"],
        "lora_alpha": parsed["lora_alpha"],
        "tensors": len(peft_tensors),
        "target_modules": sorted(target_modules),
    }


def _pack_gguf_str(text):
    data = text.encode("utf-8")
    return struct.pack("<Q", len(data)) + data


def _align(offset, alignment=GGUF_ALIGNMENT):
    rem = offset % alignment
    return offset if rem == 0 else offset + (alignment - rem)


def _write_gguf_v3(output_path, kv_pairs, tensors):
    """Write a spec-compliant GGUF v3 binary file with aligned F32/F16 tensor payloads.

    `kv_pairs`: list of (key: str, gguf_type: int, value)
    `tensors`: list of (name: str, ggml_type: int, ggml_dims: tuple[int, ...], raw_bytes: bytes)
    """
    header = bytearray()
    header += GGUF_MAGIC
    header += struct.pack("<IQQ", GGUF_VERSION, len(tensors), len(kv_pairs))
    for key, vtype, value in kv_pairs:
        header += _pack_gguf_str(key)
        header += struct.pack("<I", vtype)
        if vtype == GGUF_TYPE_STRING:
            header += _pack_gguf_str(value)
        elif vtype == GGUF_TYPE_UINT32:
            header += struct.pack("<I", int(value))
        elif vtype == GGUF_TYPE_FLOAT32:
            header += struct.pack("<f", float(value))
        else:
            raise ExportError(f"unsupported GGUF KV type {vtype}")

    data_offset = 0
    for name, ggml_type, dims, raw in tensors:
        header += _pack_gguf_str(name)
        header += struct.pack("<I", len(dims))
        for d in dims:
            header += struct.pack("<Q", int(d))
        header += struct.pack("<IQ", ggml_type, data_offset)
        data_offset = _align(data_offset + len(raw), GGUF_ALIGNMENT)

    padded_header_len = _align(len(header), GGUF_ALIGNMENT)
    header += b"\x00" * (padded_header_len - len(header))

    with Path(output_path).open("wb") as handle:
        handle.write(header)
        written = 0
        for _, _, _, raw in tensors:
            pad = _align(written, GGUF_ALIGNMENT) - written
            if pad:
                handle.write(b"\x00" * pad)
                written += pad
            handle.write(raw)
            written += len(raw)
        handle.flush()
        os.fsync(handle.fileno())


def export_gguf_lora(model_name, adapter_dir, output_file, *, converter_script=None, dtype="f32"):
    """Export an MLX or PEFT LoRA adapter directly to a GGUF LoRA file (`llama-server --lora`).

    Uses llama.cpp's `convert_lora_to_gguf.py` when supplied; otherwise writes a spec-compliant
    GGUF v3 LoRA binary directly in pure Python + NumPy (zero extra dependencies).
    """
    import numpy as np

    output = Path(output_file).resolve()
    _require(not output.exists(), "output file must not already exist")
    _require(output.parent.is_dir(), "output parent directory must exist")
    _require(dtype in ("f32", "f16"), "dtype must be f32 or f16")

    parsed = _parse_mlx_or_peft_adapter(adapter_dir)
    if parsed["fine_tune_type"] == "dora":
        raise ExportError(
            "llama.cpp GGUF LoRA does not support DoRA magnitude vectors; use `dial improve export-gguf` "
            "to fuse DoRA into standalone GGUF weights, or train with training.fine_tune_type = 'lora' for GGUF LoRA"
        )

    if converter_script and Path(converter_script).is_file():
        with tempfile.TemporaryDirectory(prefix=".gguf-lora-peft-", dir=output.parent) as scratch:
            peft_dir = Path(scratch) / "peft"
            export_peft(model_name, adapter_dir, peft_dir)
            staged = Path(scratch) / "adapter.gguf"
            cmd = [
                shutil.which("python3") or "python3",
                str(converter_script),
                "--base", str(model_name),
                "--outtype", dtype,
                "--outfile", str(staged),
                str(peft_dir),
            ]
            proc = subprocess.run(cmd, capture_output=True, text=True)
            if proc.returncode == 0 and staged.is_file():
                os.replace(staged, output)
                return {
                    "output": str(output),
                    "format": "gguf-lora",
                    "rank": parsed["rank"],
                    "lora_alpha": parsed["lora_alpha"],
                    "tensors": len(parsed["tensors"]) * 2,
                    "converter": "convert_lora_to_gguf.py",
                }

    # Built-in GGUF v3 LoRA writer
    mtype = _model_type(model_name)
    arch = MODEL_TYPE_TO_GGUF_ARCH.get(mtype, mtype)
    np_dtype = np.float16 if dtype == "f16" else np.float32
    ggml_type = GGML_TYPE_F16 if dtype == "f16" else GGML_TYPE_F32

    gguf_tensors = []
    for module_path, pair in sorted(parsed["tensors"].items()):
        match = LAYER_RE.search(module_path)
        _require(match is not None, f"cannot map layer index for GGUF LoRA tensor: {module_path}")
        layer_idx, suffix = int(match.group(1)), match.group(2)
        gguf_proj = HF_TO_GGUF_PROJ.get(suffix)
        _require(
            gguf_proj is not None,
            f"projection {suffix!r} has no built-in GGUF LoRA mapping; install llama.cpp's convert_lora_to_gguf.py (DIAL_CONVERT_LORA_TO_GGUF) or fuse with export-gguf",
        )
        # PyTorch lora_A is (r, in_features) -> GGML dims (in_features, r)
        # PyTorch lora_B is (out_features, r) -> GGML dims (r, out_features)
        lora_a = np.ascontiguousarray(pair["lora_A"], dtype=np_dtype)
        lora_b = np.ascontiguousarray(pair["lora_B"], dtype=np_dtype)
        r_a, in_dim = lora_a.shape
        out_dim, r_b = lora_b.shape
        _require(r_a == r_b == parsed["rank"], f"inconsistent LoRA rank on {module_path}")
        base_name = f"blk.{layer_idx}.{gguf_proj}.weight"
        gguf_tensors.append((f"{base_name}.lora_a", ggml_type, (in_dim, r_a), lora_a.tobytes()))
        gguf_tensors.append((f"{base_name}.lora_b", ggml_type, (r_b, out_dim), lora_b.tobytes()))

    kv_pairs = [
        ("general.architecture", GGUF_TYPE_STRING, arch),
        ("general.type", GGUF_TYPE_STRING, "adapter"),
        ("general.file_type", GGUF_TYPE_UINT32, 1 if dtype == "f16" else 0),
        ("adapter.type", GGUF_TYPE_STRING, "lora"),
        ("adapter.lora.alpha", GGUF_TYPE_FLOAT32, float(parsed["lora_alpha"])),
        ("general.base_model.0.name", GGUF_TYPE_STRING, Path(model_name).name),
    ]

    staging = output.with_name(f".{output.name}.tmp")
    try:
        _write_gguf_v3(staging, kv_pairs, gguf_tensors)
        os.replace(staging, output)
    except BaseException:
        staging.unlink(missing_ok=True)
        raise

    return {
        "output": str(output),
        "format": "gguf-lora",
        "architecture": arch,
        "rank": parsed["rank"],
        "lora_alpha": parsed["lora_alpha"],
        "tensors": len(gguf_tensors),
        "converter": "dial-builtin-gguf-v3",
    }


def export_gguf_fused(fused_dir, output_file, *, quant="f16", converter_script=None, quantize_bin=None, uv_bin=None):
    """Convert a dequantized HF/MLX safetensors checkpoint into a standalone GGUF file.

    Runs llama.cpp's `convert_hf_to_gguf.py` to produce F16 GGUF, then optionally runs
    `llama-quantize` for quantized targets (e.g., Q4_K_M, Q8_0, Q6_K).
    """
    fused = Path(fused_dir).resolve(strict=True)
    output = Path(output_file).resolve()
    _require(fused.is_dir(), "fused checkpoint must be a directory")
    _require(not output.exists(), "output GGUF file must not already exist")
    _require(output.parent.is_dir(), "output parent directory must exist")
    quant_norm = str(quant or "f16").strip().upper()
    _require(bool(re.fullmatch(r"[A-Z0-9_]+", quant_norm)), f"invalid GGUF quantization type: {quant}")

    converter = Path(converter_script) if converter_script else None
    if not converter or not converter.is_file():
        raise ExportError(
            "fused GGUF export requires llama.cpp's convert_hf_to_gguf.py (set DIAL_CONVERT_HF_TO_GGUF "
            "or place convert_hf_to_gguf.py alongside llama-server); the dequantized safetensors checkpoint "
            "can still be created via `dial improve fuse --dequantize`"
        )

    need_quantize = quant_norm not in ("F16", "BF16", "F32", "Q8_0")
    if quant_norm != "F16" and quant_norm not in ("BF16", "F32"):
        if not quantize_bin or not Path(quantize_bin).is_file():
            if need_quantize:
                raise ExportError(
                    f"quantizing to {quant_norm} requires llama-quantize (install llama.cpp or set DIAL_LLAMA_QUANTIZE)"
                )

    with tempfile.TemporaryDirectory(prefix=".export-gguf-", dir=output.parent) as scratch:
        outtype = quant_norm.lower() if quant_norm in ("F16", "BF16", "F32", "Q8_0") and not quantize_bin else "f16"
        f16_path = Path(scratch) / f"model-{outtype}.gguf"
        python_cmd = _python_with_gguf_cmd(uv_bin)
        cmd = python_cmd + [str(converter), str(fused), "--outtype", outtype, "--outfile", str(f16_path)]
        proc = subprocess.run(cmd, capture_output=True, text=True)
        if proc.returncode != 0 or not f16_path.is_file():
            tail = (proc.stderr or proc.stdout or "")[-600:].strip()
            raise ExportError(f"convert_hf_to_gguf.py failed: {tail or 'unknown error'}")

        final_staged = f16_path
        if quant_norm != outtype.upper():
            _require(quantize_bin and Path(quantize_bin).is_file(),
                     f"llama-quantize is required to quantize {outtype.upper()} to {quant_norm}")
            quant_path = Path(scratch) / f"model-{quant_norm}.gguf"
            qproc = subprocess.run([str(quantize_bin), str(f16_path), str(quant_path), quant_norm],
                                   capture_output=True, text=True)
            if qproc.returncode != 0 or not quant_path.is_file():
                tail = (qproc.stderr or qproc.stdout or "")[-600:].strip()
                raise ExportError(f"llama-quantize failed: {tail or 'unknown error'}")
            final_staged = quant_path

        os.replace(final_staged, output)

    return {
        "output": str(output),
        "format": "gguf",
        "quant": quant_norm,
        "bytes": output.stat().st_size,
    }


def _python_with_gguf_cmd(uv_bin=None):
    try:
        import gguf  # noqa: F401
        import torch  # noqa: F401
        import transformers  # noqa: F401
        import sys
        return [sys.executable]
    except ImportError:
        uv = uv_bin or shutil.which("uv")
        if uv:
            return [
                str(uv), "run", "--no-project",
                "--with", "torch", "--with", "transformers", "--with", "gguf",
                "--with", "safetensors", "--with", "numpy", "--with", "sentencepiece", "--with", "protobuf",
                "python",
            ]
        import sys
        return [sys.executable]


def prepare_gguf(source_gguf, output_dir, *, uv_bin=None):
    """Dequantize a single unsplit local GGUF into an independent FP16 safetensors training base."""
    source = Path(source_gguf).resolve(strict=True)
    output = Path(output_dir).resolve()
    _require(source.is_file(), "source GGUF must be an existing file")
    _require(not output.exists(), "output directory must not already exist")
    _require(output.parent.is_dir(), "output parent directory must exist")

    script = r"""
import json, shutil, sys, tempfile
from pathlib import Path
import torch
from gguf import GGUFReader
from transformers import AutoModelForCausalLM, AutoTokenizer

source, output = Path(sys.argv[1]), Path(sys.argv[2])
reader = GGUFReader(str(source))
if "split.count" in reader.fields or not reader.tensors:
    raise ValueError("Choose a single, unsplit GGUF language checkpoint with weights.")
required = 2 * sum(int(t.n_elements) for t in reader.tensors)
if shutil.disk_usage(output.parent).free < required + (64 << 20):
    raise ValueError("Not enough free disk space for the dequantized FP16 weights.")
del reader

with tempfile.TemporaryDirectory() as source_dir:
    Path(source_dir, "source.gguf").symlink_to(source.absolute())
    kwargs = dict(gguf_file="source.gguf", local_files_only=True, trust_remote_code=False)
    tokenizer = AutoTokenizer.from_pretrained(source_dir, **kwargs)
    if not tokenizer.chat_template:
        raise ValueError("No chat template in the GGUF. Use the original chat checkpoint instead.")
    model, info = AutoModelForCausalLM.from_pretrained(
        source_dir, dtype=torch.float16, output_loading_info=True, **kwargs
    )
    if any(info.get(k) for k in ("missing_keys", "unexpected_keys", "mismatched_keys", "error_msgs")):
        raise ValueError("The GGUF did not map completely to the model architecture.")
    staging = Path(tempfile.mkdtemp(prefix=".prepare-gguf-", dir=output.parent))
    try:
        model.save_pretrained(staging)
        tokenizer.save_pretrained(staging)
        staging.rename(output)
    except BaseException:
        shutil.rmtree(staging, ignore_errors=True)
        raise
"""
    cmd = _python_with_gguf_cmd(uv_bin) + ["-c", script, str(source), str(output)]
    proc = subprocess.run(cmd, capture_output=True, text=True)
    if proc.returncode != 0 or not output.is_dir():
        tail = (proc.stderr or proc.stdout or "")[-600:].strip()
        raise ExportError(f"prepare_gguf failed: {tail or 'unknown error'}")
    return {
        "output": str(output),
        "source": str(source),
        "format": "mlx",
    }
