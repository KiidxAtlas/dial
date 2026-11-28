"""Real MLX adapter contract tests; run with Dial's managed Python runtime."""
import json
import tempfile
import unittest
from pathlib import Path

try:
    import mlx.core as mx
    from mlx import nn
    from mlx.utils import tree_flatten
    from mlx_lm.tuner.utils import linear_to_lora_layers
except ImportError as error:
    raise unittest.SkipTest(f"MLX/Metal runtime required: {error}")
from dial_worker import (
    RequestError,
    expected_weights,
    probe_adapter,
    query_targets,
    validate_adapter,
)


class Block(nn.Module):
    def __init__(self, key):
        super().__init__()
        self[key] = nn.Linear(32, 32, bias=False)
        self.key = key

    def __call__(self, x):
        return x + self[self.key](x)


class Model(nn.Module):
    def __init__(self, keys):
        super().__init__()
        self.embed = nn.Embedding(32, 32)
        self.layers = [Block(key) for key in keys]
        self.head = nn.Linear(32, 32, bias=False)

    def __call__(self, tokens):
        x = self.embed(tokens)
        for layer in self.layers:
            x = layer(x)
        return self.head(x)


class AdapterTests(unittest.TestCase):
    def test_training_entry_accepts_validated_composed_ranks_only_on_resume(self):
        from unittest.mock import patch
        import dial_worker as worker

        class Validated(Exception):
            pass

        for rank in (129, worker.MAX_ADAPTER_RANK):
            with self.subTest(rank=rank), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                model = Model(("q_proj",))
                keys, ids = query_targets(model, 1)
                expected = expected_weights(model, ids, rank)
                parameters = {"rank": rank, "scale": 1.0, "dropout": 0.0, "keys": keys}
                adapter = worker.write_adapter(root / "adapter", "test", 1, parameters,
                                               {name: mx.zeros(shape) for name, shape in expected.items()})
                request = {"model": "test", "iters": 1, "batch_size": 1, "num_layers": 1,
                           "rank": rank, "max_seq_length": 32, "train_data": "train",
                           "valid_data": "valid", "output_dir": str(root / "output")}
                with self.assertRaisesRegex(RequestError, "rank"):
                    worker.train_in(request, root)
                request["resume_adapter"] = str(adapter)
                resume = worker.resume_layout

                def validated(*args):
                    count, own, shapes, weights = resume(*args)
                    self.assertEqual((count, own["rank"], own["scale"]), (1, rank, 20.0))
                    self.assertEqual(shapes, expected)
                    self.assertTrue(Path(weights).is_file())
                    raise Validated

                with patch.object(worker, "read_datasets", return_value=([{}], [{}])), \
                     patch.object(worker, "resolve_model", return_value="test"), \
                     patch.object(worker, "load_base", return_value=(model, None)), \
                     patch.object(worker, "render_records", side_effect=lambda records, _: records), \
                     patch.object(worker, "resume_layout", side_effect=validated):
                    with self.assertRaises(Validated):
                        worker.train_in(request, root)
                    # The higher request bound must not bypass actual tensor validation.
                    mx.save_safetensors(str(adapter / "adapters.safetensors"),
                                        {name: mx.zeros((1,)) for name in expected})
                    with self.assertRaisesRegex(RequestError, "shape"):
                        worker.train_in(request, root)

    def test_training_pause_restores_optimizer_rng_and_batch_cursor(self):
        self.check_training_pause("sft", 2)

    def test_preference_phase_resumes_without_repeating_sft(self):
        self.check_training_pause("preference", 1, True)

    def check_training_pause(self, phase, iteration, preference=False):
        import copy
        from unittest.mock import patch

        import dial_worker as worker
        from mlx_lm.lora import CONFIG_DEFAULTS
        mx.random.seed(42)
        original = Model(("q_proj", "out_proj"))
        weights = dict(tree_flatten(original.parameters()))
        mx.eval(weights)
        keys, ids = query_targets(original, 2)
        expected = expected_weights(original, ids, 2)
        args = copy.deepcopy(CONFIG_DEFAULTS)
        args.update(model="test", num_layers=2, iters=4, batch_size=1, max_seq_length=32,
                    learning_rate=1e-3, lora_parameters={"rank":2,"scale":20.0,"dropout":0.0,"keys":keys})
        dataset = [([1,2,3,4],1), ([2,3,4,5],1), ([3,4,5,6],1)]
        def model():
            result=Model(("q_proj","out_proj"))
            result.load_weights(list(weights.items()))
            return result
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory)
            data=root / "data.jsonl"
            data.write_text("verified immutable examples")
            request={"model":"test","resume_adapter":None,"iters":4,"batch_size":1,"num_layers":2,
                     "rank":2,"max_seq_length":32,"train_data":str(data),"valid_data":str(data),
                     "pause_file":str(root / "pause"),"progress_file":str(root / "progress.json"),
                     "output_dir":str(root / "baseline"),"resume_checkpoint":False}
            baseline=model()
            encoded=[(dataset[i % 3],dataset[(i+1) % 3]) for i in range(12)]
            def run(target):
                with patch.object(worker,"tokenize_preference_pairs",return_value=encoded):
                    worker.pausable_train(request,target,None,dataset,dataset,[True] if preference else [],args,expected)
            run(baseline)
            request["output_dir"]=str(root / "resumed")
            atomic=worker.atomic_json
            def pause_after_two(path,value):
                atomic(path,value)
                if Path(path)==root / "progress.json" and value["phase"]==phase and value["iteration"]==iteration:
                    (root / "pause").touch()
            with patch.object(worker,"atomic_json",pause_after_two):
                with self.assertRaises(worker.TrainingPaused):
                    run(model())
            checkpoint=json.loads((root / "resumed/resume.json").read_text())
            self.assertEqual(checkpoint["completed"],iteration)
            self.assertEqual(checkpoint["phase"],phase)
            (root / "pause").unlink()
            request["resume_checkpoint"]=True
            resumed=model()
            run(resumed)
            for name,tensor in tree_flatten(baseline.trainable_parameters()):
                actual=dict(tree_flatten(resumed.trainable_parameters()))[name]
                self.assertLess(float(mx.max(mx.abs(tensor-actual)).item()),1e-6,name)
            data.write_text("changed examples")
            with self.assertRaisesRegex(worker.RequestError,"inputs changed"):
                run(model())

    def test_hybrid_and_fused_gradient_save_reload(self):
        for keys in [("q_proj", "out_proj"), ("qkv_proj", "qkv_proj"), ("q_proj", "q_proj")]:
            with self.subTest(keys=keys):
                model = Model(keys)
                targets = probe_adapter(model, [1, 2, 3, 4])
                self.assertEqual(targets, sorted(set(keys)))

    def test_quantized_expected_weights_and_roundtrip(self):
        model = Model(("q_proj", "out_proj"))
        nn.quantize(model, group_size=32, bits=4)
        probe_adapter(model, [1, 2, 3, 4])

    def test_layer_scope_union_and_resume_validation(self):
        model = Model(("q_proj", "q_proj", "out_proj"))
        # The fallback path also occurs in an ordinary attention block: MLX converts both.
        model.layers[1].out_proj = nn.Linear(32, 32, bias=False)
        keys, ids = query_targets(model, 2)
        expected = expected_weights(model, ids, 2)
        self.assertEqual(len(expected), 6)
        self.assertFalse(any(name.startswith("layers.0.") for name in expected))
        parameters = {"rank": 2, "scale": 20.0, "dropout": 0.0, "keys": keys}
        model.freeze()
        linear_to_lora_layers(model, 2, parameters)
        tensors = dict(tree_flatten(model.trainable_parameters()))
        self.assertEqual({k: tuple(v.shape) for k, v in tensors.items()}, expected)
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory)
            (path / "adapter_config.json").write_text(json.dumps({"model": "test", "fine_tune_type": "lora", "num_layers": 2, "lora_parameters": parameters}))
            mx.save_safetensors(str(path / "adapters.safetensors"), tensors)
            validate_adapter(path, "test", 2, parameters, expected)
            with self.assertRaises(RequestError):
                validate_adapter(path, "other-model", 2, parameters, expected)

    def test_qwen35_hybrid_architecture(self):
        from mlx_lm.models.qwen3_5 import Model as Qwen
        from mlx_lm.models.qwen3_5 import ModelArgs
        model = Qwen(ModelArgs(model_type="qwen3_5", text_config={
            "hidden_size": 32, "intermediate_size": 64, "num_hidden_layers": 2,
            "num_attention_heads": 2, "num_key_value_heads": 2, "head_dim": 16,
            "vocab_size": 32, "linear_num_value_heads": 2, "linear_num_key_heads": 2,
            "linear_key_head_dim": 16, "linear_value_head_dim": 16,
            "full_attention_interval": 2,
        }))
        keys = probe_adapter(model, [1, 2, 3, 4])
        self.assertTrue(any("out_proj" in key for key in keys))
        self.assertTrue(any("q_proj" in key for key in keys))

    def test_moe_expert_adapter_shapes_match_mlx(self):
        try:
            from mlx_lm.layers.moe import SwitchLinear
        except ImportError:
            from mlx_lm.models.switch_layers import SwitchLinear
        model = Model(("q_proj",))
        model.layers[0] = nn.Module()
        model.layers[0].experts = SwitchLinear(32, 32, 2, bias=False)
        keys, ids = query_targets(model)
        expected = expected_weights(model, ids, 2)
        model.freeze()
        linear_to_lora_layers(model, 1, {"rank": 2, "scale": 20.0, "dropout": 0.0, "keys": keys})
        actual = {name: tuple(tensor.shape) for name, tensor in tree_flatten(model.trainable_parameters())}
        self.assertEqual(actual, expected)
        self.assertEqual(len(actual), 2)

    def test_nontrainable_block_is_rejected(self):
        model = Model(("q_proj",))
        model.layers = [nn.Identity()]
        with self.assertRaisesRegex(RequestError, "no MLX LoRA-compatible"):
            query_targets(model)

    def test_target_modes_all_linear_attention_qproj_and_custom(self):
        class RichBlock(nn.Module):
            def __init__(self):
                super().__init__()
                self.self_attn = nn.Module()
                self.self_attn.q_proj = nn.Linear(32, 32, bias=False)
                self.self_attn.k_proj = nn.Linear(32, 32, bias=False)
                self.self_attn.v_proj = nn.Linear(32, 32, bias=False)
                self.self_attn.o_proj = nn.Linear(32, 32, bias=False)
                self.mlp = nn.Module()
                self.mlp.gate_proj = nn.Linear(32, 64, bias=False)
                self.mlp.up_proj = nn.Linear(32, 64, bias=False)
                self.mlp.down_proj = nn.Linear(64, 32, bias=False)

            def __call__(self, x):
                return x + self.self_attn.o_proj(self.self_attn.q_proj(x))

        class RichModel(nn.Module):
            def __init__(self):
                super().__init__()
                self.embed = nn.Embedding(32, 32)
                self.layers = [RichBlock(), RichBlock()]
                self.head = nn.Linear(32, 32, bias=False)

            def __call__(self, tokens):
                x = self.embed(tokens)
                for layer in self.layers:
                    x = layer(x)
                return self.head(x)

        model = RichModel()
        all_keys, _ = query_targets(model, 2, target_mode="all-linear")
        self.assertEqual(len(all_keys), 7)
        attn_keys, _ = query_targets(model, 2, target_mode="attention")
        self.assertEqual(attn_keys, ["self_attn.k_proj", "self_attn.o_proj", "self_attn.q_proj", "self_attn.v_proj"])
        q_keys, _ = query_targets(model, 2, target_mode="q-proj")
        self.assertEqual(q_keys, ["self_attn.q_proj"])
        custom_keys, _ = query_targets(model, 2, target_mode="q_proj,down_proj")
        self.assertEqual(custom_keys, ["mlp.down_proj", "self_attn.q_proj"])

    def test_dora_expected_weights_train_and_validate(self):
        model = Model(("q_proj", "out_proj"))
        keys, ids = query_targets(model, 2, target_mode="all-linear")
        expected = expected_weights(model, ids, 4, fine_tune_type="dora")
        self.assertTrue(any(name.endswith(".m") for name in expected))
        parameters = {"rank": 4, "dropout": 0.05, "scale": 16.0, "keys": keys}
        model.freeze()
        linear_to_lora_layers(model, 2, parameters, use_dora=True)
        tensors = dict(tree_flatten(model.trainable_parameters()))
        self.assertEqual({k: tuple(v.shape) for k, v in tensors.items()}, expected)
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory)
            (path / "adapter_config.json").write_text(json.dumps({
                "model": "test", "fine_tune_type": "dora", "num_layers": 2, "lora_parameters": parameters
            }))
            mx.save_safetensors(str(path / "adapters.safetensors"), tensors)
            validate_adapter(path, "test", 2, parameters, expected, fine_tune_type="dora")

    def test_export_peft_and_gguf_lora_roundtrip_math(self):
        import struct
        import numpy as np
        from safetensors.numpy import load_file
        import dial_lora_export as exp

        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            model_dir = root / "model"
            model_dir.mkdir()
            (model_dir / "config.json").write_text(json.dumps({"model_type": "qwen2"}))
            adapter_dir = root / "mlx_adapter"
            adapter_dir.mkdir()
            rank, scale = 4, 20.0
            rng = np.random.default_rng(7)
            a_mlx = rng.standard_normal((16, rank)).astype(np.float32)
            b_mlx = rng.standard_normal((rank, 24)).astype(np.float32)
            (adapter_dir / "adapter_config.json").write_text(json.dumps({
                "model": str(model_dir),
                "fine_tune_type": "lora",
                "num_layers": 1,
                "lora_parameters": {"rank": rank, "dropout": 0.0, "scale": scale, "keys": ["self_attn.q_proj"]},
            }))
            mx.save_safetensors(str(adapter_dir / "adapters.safetensors"), {
                "model.layers.0.self_attn.q_proj.lora_a": mx.array(a_mlx),
                "model.layers.0.self_attn.q_proj.lora_b": mx.array(b_mlx),
            })

            peft_dir = root / "peft_out"
            peft_res = exp.export_peft(str(model_dir), str(adapter_dir), str(peft_dir))
            self.assertEqual(peft_res["format"], "peft")
            self.assertAlmostEqual(peft_res["lora_alpha"], 80.0)
            cfg = json.loads((peft_dir / "adapter_config.json").read_text())
            self.assertEqual(cfg["peft_type"], "LORA")
            self.assertEqual(cfg["layers_to_transform"], [0])
            peft_tensors = load_file(str(peft_dir / "adapter_model.safetensors"))
            a_peft = peft_tensors["base_model.model.model.layers.0.self_attn.q_proj.lora_A.weight"]
            b_peft = peft_tensors["base_model.model.model.layers.0.self_attn.q_proj.lora_B.weight"]
            self.assertEqual(a_peft.shape, (rank, 16))
            self.assertEqual(b_peft.shape, (24, rank))
            # Verify exact mathematical equivalence between MLX and PEFT LoRA forward delta
            x = rng.standard_normal((3, 16)).astype(np.float32)
            delta_mlx = scale * ((x @ a_mlx) @ b_mlx)
            delta_peft = (cfg["lora_alpha"] / rank) * ((x @ a_peft.T) @ b_peft.T)
            self.assertTrue(np.allclose(delta_mlx, delta_peft, rtol=1e-5, atol=1e-4))

            # Export to GGUF v3 LoRA and verify binary header + tensor layout
            gguf_out = root / "adapter.gguf"
            gguf_res = exp.export_gguf_lora(str(model_dir), str(adapter_dir), str(gguf_out))
            self.assertEqual(gguf_res["format"], "gguf-lora")
            self.assertEqual(gguf_res["architecture"], "qwen2")
            raw = gguf_out.read_bytes()
            self.assertEqual(raw[:4], b"GGUF")
            version, n_tensors, n_kv = struct.unpack("<IQQ", raw[4:24])
            self.assertEqual((version, n_tensors, n_kv), (3, 2, 6))
            self.assertIn(b"blk.0.attn_q.weight.lora_a", raw)
            self.assertIn(b"blk.0.attn_q.weight.lora_b", raw)
            self.assertIn(b"adapter.lora.alpha", raw)

            # DoRA must be rejected for GGUF LoRA rather than silently dropping magnitude vectors
            dora_dir = root / "dora_adapter"
            dora_dir.mkdir()
            (dora_dir / "adapter_config.json").write_text(json.dumps({
                "model": str(model_dir),
                "fine_tune_type": "dora",
                "num_layers": 1,
                "lora_parameters": {"rank": rank, "dropout": 0.0, "scale": scale, "keys": ["self_attn.q_proj"]},
            }))
            mx.save_safetensors(str(dora_dir / "adapters.safetensors"), {
                "model.layers.0.self_attn.q_proj.lora_a": mx.array(a_mlx),
                "model.layers.0.self_attn.q_proj.lora_b": mx.array(b_mlx),
                "model.layers.0.self_attn.q_proj.m": mx.ones((24,), dtype=mx.float32),
            })
            with self.assertRaisesRegex(exp.ExportError, "DoRA"):
                exp.export_gguf_lora(str(model_dir), str(dora_dir), str(root / "dora.gguf"))

    def test_adamw_cosine_grad_checkpoint_pause_resume_exact(self):
        import copy
        import dial_worker as worker
        from mlx_lm.lora import CONFIG_DEFAULTS
        mx.random.seed(99)
        original = Model(("q_proj", "out_proj"))
        weights = dict(tree_flatten(original.parameters()))
        mx.eval(weights)
        keys, ids = query_targets(original, 2, target_mode="all-linear")
        expected = expected_weights(original, ids, 2, fine_tune_type="dora")
        args = copy.deepcopy(CONFIG_DEFAULTS)
        args.update(
            model="test", fine_tune_type="dora", num_layers=2, iters=6, batch_size=1,
            max_seq_length=32, learning_rate=5e-4, lr_schedule="cosine", warmup_ratio=0.33,
            optimizer="adamw", weight_decay=0.01, grad_checkpoint=True, grad_accumulation_steps=2,
            lora_parameters={"rank": 2, "scale": 20.0, "dropout": 0.0, "keys": keys},
        )
        dataset = [([1, 2, 3, 4], 1), ([2, 3, 4, 5], 1), ([3, 4, 5, 6], 1)]

        def make_model():
            m = Model(("q_proj", "out_proj"))
            m.load_weights(list(weights.items()))
            return m

        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            data = root / "data.jsonl"
            data.write_text("verified")
            request = {
                "model": "test", "resume_adapter": None, "iters": 6, "batch_size": 1,
                "num_layers": 2, "rank": 2, "max_seq_length": 32,
                "fine_tune_type": "dora", "target_modules": "all-linear",
                "learning_rate": 5e-4, "lr_schedule": "cosine", "warmup_ratio": 0.33,
                "optimizer": "adamw", "weight_decay": 0.01, "grad_checkpoint": True,
                "grad_accumulation_steps": 2, "checkpoint_every": 4,
                "train_data": str(data), "valid_data": str(data),
                "pause_file": str(root / "pause"), "progress_file": str(root / "progress.json"),
                "output_dir": str(root / "baseline"), "resume_checkpoint": False,
            }
            baseline = make_model()
            worker.pausable_train(request, baseline, None, dataset, dataset, [], args, expected)
            request["output_dir"] = str(root / "resumed")
            atomic = worker.atomic_json

            def pause_at_three(path, value):
                atomic(path, value)
                if Path(path) == root / "progress.json" and value["iteration"] == 3:
                    (root / "pause").touch()

            from unittest.mock import patch
            with patch.object(worker, "atomic_json", pause_at_three):
                with self.assertRaises(worker.TrainingPaused):
                    worker.pausable_train(request, make_model(), None, dataset, dataset, [], args, expected)
            (root / "pause").unlink()
            request["resume_checkpoint"] = True
            resumed = make_model()
            worker.pausable_train(request, resumed, None, dataset, dataset, [], args, expected)
            for name, tensor in tree_flatten(baseline.trainable_parameters()):
                actual = dict(tree_flatten(resumed.trainable_parameters()))[name]
                self.assertLess(float(mx.max(mx.abs(tensor - actual)).item()), 1e-5, name)

if __name__ == "__main__":
    unittest.main()
