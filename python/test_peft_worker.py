"""Unit and wire-contract tests for dial_peft_worker.py (CPU/macOS safe, no CUDA required)."""
import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

import dial_peft_worker as peft_worker


class PeftWorkerContractTests(unittest.TestCase):
    def test_is_exl3_dir_detects_exl3_metadata(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            mlx = root / "mlx"
            mlx.mkdir()
            (mlx / "config.json").write_text(json.dumps({"model_type": "llama"}))
            self.assertFalse(peft_worker.is_exl3_dir(mlx))

            exl3 = root / "exl3"
            exl3.mkdir()
            (exl3 / "config.json").write_text(json.dumps({
                "model_type": "llama",
                "quantization_config": {"quant_method": "exl3", "bits": 4.0},
            }))
            self.assertTrue(peft_worker.is_exl3_dir(exl3))

    def test_cosine_warmup_schedule_bounds(self):
        # Step 0 starts near warmup init, peaks at warmup boundary, decays to 0.1 at end
        s0 = peft_worker._lr_multiplier(0, 100, "cosine", 0.1)
        s9 = peft_worker._lr_multiplier(9, 100, "cosine", 0.1)
        s54 = peft_worker._lr_multiplier(54, 100, "cosine", 0.1)
        s100 = peft_worker._lr_multiplier(100, 100, "cosine", 0.1)
        self.assertGreater(s0, 0.0)
        self.assertLess(s0, 0.2)
        self.assertAlmostEqual(s9, 1.0, places=5)
        self.assertGreater(s54, 0.4)
        self.assertLess(s54, 0.7)
        self.assertAlmostEqual(s100, 0.1, places=5)
        self.assertEqual(peft_worker._lr_multiplier(50, 100, "constant", 0.1), 1.0)

    def test_capabilities_on_exl3_checkpoint_and_export_exl3_rejection_without_cuda(self):
        with tempfile.TemporaryDirectory() as directory:
            exl3 = Path(directory) / "model-exl3"
            exl3.mkdir()
            (exl3 / "config.json").write_text(json.dumps({
                "model_type": "llama",
                "quantization_config": {"quant_method": "exl3", "bits": 4.0},
            }))
            (exl3 / "model.safetensors").write_bytes(b"placeholder")
            caps = peft_worker.capabilities({"op": "capabilities", "model": str(exl3)})
            self.assertTrue(caps["exl3"])
            self.assertEqual(caps["backend"], "peft")
            self.assertEqual(caps["lora_probe_version"], 3)
            self.assertFalse(caps["lora"])
            self.assertIn("EXL3", caps["reason"])

            # Running the worker subprocess for capabilities returns valid JSON on stdout
            proc = subprocess.run(
                [sys.executable, str(Path(peft_worker.__file__).resolve())],
                input=json.dumps({"op": "capabilities", "model": str(exl3)}),
                text=True,
                capture_output=True,
            )
            self.assertEqual(proc.returncode, 0, proc.stderr)
            parsed = json.loads(proc.stdout)
            self.assertTrue(parsed["exl3"])

            # export_exl3 must fail closed without CUDA/exllamav3
            with self.assertRaisesRegex(peft_worker.RequestError, "CUDA|PyTorch|exllamav3"):
                peft_worker.export_exl3({
                    "op": "export_exl3",
                    "model": str(exl3),
                    "output": str(Path(directory) / "out_exl3"),
                    "bpw": 4.0,
                })


if __name__ == "__main__":
    unittest.main()
