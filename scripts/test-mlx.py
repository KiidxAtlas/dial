#!/usr/bin/env python3
"""Run optional local MLX contracts, never provisioning dependencies or model weights."""
import importlib.util
import os
import platform
import subprocess
import sys
from pathlib import Path


def main():
    if platform.system() != "Darwin" or platform.machine() not in ("arm64", "aarch64"):
        print("SKIP optional MLX tier: macOS Apple Silicon is required")
        return 0
    missing = [name for name in ("mlx", "mlx_lm", "pytest")
               if importlib.util.find_spec(name) is None]
    if missing:
        print("SKIP optional MLX tier: local runtime lacks " + ", ".join(missing))
        return 0
    # Import/device errors on a provisioned host are failures, not successful skips.
    import mlx.core as mx
    if not mx.metal.is_available():
        print("SKIP optional MLX tier: Metal is unavailable")
        return 0
    root = Path(__file__).resolve().parents[1]
    env = dict(os.environ, HF_HUB_OFFLINE="1", TRANSFORMERS_OFFLINE="1")
    print("Running local MLX contracts offline; tokenizer/vision prerequisites report individual skips", flush=True)
    return subprocess.call([
        sys.executable, "-m", "pytest", "-rs",
        "python/test_lora_targets.py", "python/test_cheap_improvement.py",
        "python/test_steering.py", "python/test_thinking.py",
    ], cwd=root, env=env)


if __name__ == "__main__":
    raise SystemExit(main())
