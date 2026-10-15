"""Real MLX adapter contract tests; run with Dial's managed Python runtime."""
import unittest
import tempfile
import json
from pathlib import Path
try:
    import mlx.core as mx
    import mlx.nn as nn
    from mlx.utils import tree_flatten
    from mlx_lm.tuner.utils import linear_to_lora_layers
except ImportError as error:
    raise unittest.SkipTest(f"MLX/Metal runtime required: {error}")
from dial_worker import query_targets, expected_weights, probe_adapter, validate_adapter, RequestError


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
    def test_training_pause_restores_optimizer_rng_and_batch_cursor(self):
        self.check_training_pause("sft", 2)

    def test_preference_phase_resumes_without_repeating_sft(self):
        self.check_training_pause("preference", 1, True)

    def check_training_pause(self, phase, iteration, preference=False):
        import dial_worker as worker
        from unittest.mock import patch
        from mlx_lm.lora import CONFIG_DEFAULTS
        import copy
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
        from mlx_lm.models.qwen3_5 import Model as Qwen, ModelArgs
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


if __name__ == "__main__":
    unittest.main()
