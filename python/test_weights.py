"""Read-only weights surface: checkpoint layout, tensor moments, activation contrast and ablation."""
import json
import math
import struct
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

import dial_weights as w


def write_safetensors(path, tensors):
    """Minimal header-only writer for layout tests (data bytes are zeros)."""
    header, offset = {}, 0
    for name, (dtype, shape, size) in tensors.items():
        header[name] = {"dtype": dtype, "shape": shape, "data_offsets": [offset, offset + size]}
        offset += size
    raw = json.dumps(header).encode()
    path.write_bytes(struct.pack("<Q", len(raw)) + raw + b"\0" * offset)


class Layout(unittest.TestCase):
    def test_quantized_weights_fold_scales_and_report_logical_shape(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "config.json").write_text(json.dumps({
                "model_type": "llama", "num_hidden_layers": 2, "hidden_size": 64,
                "quantization": {"group_size": 32, "bits": 4, "model.layers.1.mlp.up_proj": False}}))
            write_safetensors(root / "model.safetensors", {
                "model.layers.0.self_attn.q_proj.weight": ("U32", [64, 8], 2048),
                "model.layers.0.self_attn.q_proj.scales": ("BF16", [64, 2], 256),
                "model.layers.0.self_attn.q_proj.biases": ("BF16", [64, 2], 256),
                "model.layers.1.mlp.up_proj.weight": ("BF16", [128, 64], 16384),
                "model.norm.weight": ("BF16", [64], 128),
            })
            result = w.weight_map({}, root, w.read_config(root))
        names = [row["name"] for row in result["tensors"]]
        self.assertNotIn("model.layers.0.self_attn.q_proj.scales", names)
        q = result["tensors"][names.index("model.layers.0.self_attn.q_proj.weight")]
        self.assertEqual((q["shape"], q["quantized"], q["component"], q["layer"], q["bytes"]),
                         ([64, 64], True, "attention", 0, 2560))
        up = result["tensors"][names.index("model.layers.1.mlp.up_proj.weight")]
        self.assertEqual((up["quantized"], up["component"], up["layer"]), (False, "mlp", 1))
        self.assertEqual([row["layer"] for row in result["layers"]], [0, 1])
        self.assertTrue(result["totals"]["quantized"])
        self.assertEqual(result["architecture"]["layers"], 2)

    def test_components_classify_hybrid_moe_and_vision_names(self):
        cases = {"model.layers.3.linear_attn.in_proj_qkvz.weight": "linear_attention",
                 "model.layers.3.mlp.switch_mlp.gate_proj.weight": "moe",
                 "model.layers.3.mlp.gate.weight": "moe",
                 "vision_tower.blocks.0.attn.qkv.weight": "vision",
                 "model.embed_tokens.weight": "embedding", "lm_head.weight": "head",
                 "model.layers.0.input_layernorm.weight": "norm"}
        for name, kind in cases.items():
            with self.subTest(name=name):
                self.assertEqual(w.component(name), kind)

    def test_gguf_or_missing_config_is_refused_before_loading(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp);(root / "model.gguf").write_bytes(b"GGUF")
            with self.assertRaisesRegex(w.WeightsError, "GGUF"):
                w.operation({"op": "weights_map", "model": str(root)})
            with self.assertRaisesRegex(w.WeightsError, "unsupported weights operation"):
                w.operation({"op": "weights_merge", "model": str(root)})

    def test_corrupt_header_is_a_contract_error(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp);(root / "config.json").write_text("{}")
            (root / "model.safetensors").write_bytes(struct.pack("<Q", 10) + b"not json!!")
            with self.assertRaisesRegex(w.WeightsError, "invalid safetensors header"):
                w.weight_map({}, root, {})


class Metal(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        try:
            import mlx.core as mx
            from mlx_lm.models.llama import Model, ModelArgs
        except ImportError as error:
            raise unittest.SkipTest(str(error))
        cls.mx, cls.Model, cls.ModelArgs = mx, Model, ModelArgs

    ARGS = dict(model_type="llama", hidden_size=64, num_hidden_layers=3, intermediate_size=128,
                num_attention_heads=4, num_key_value_heads=4, rms_norm_eps=1e-5, vocab_size=64)

    def model(self):
        self.mx.random.seed(0)
        return self.Model(self.ModelArgs(**self.ARGS))

    def save(self, root, model, config):
        from mlx.utils import tree_flatten
        root.mkdir();(root / "config.json").write_text(json.dumps(config))
        self.mx.save_safetensors(str(root / "model.safetensors"), dict(tree_flatten(model.parameters())))

    def tokenizer(self):
        from mlx_lm.tokenizer_utils import TokenizerWrapper
        from tokenizers import Tokenizer
        from tokenizers.models import WordLevel
        from tokenizers.pre_tokenizers import Whitespace
        from transformers import PreTrainedTokenizerFast
        vocab = {"<unk>": 0, "<eos>": 1, "user": 2, "assistant": 3}
        for word in "yes no task a b c d e f g h".split():
            vocab[word] = len(vocab)
        raw = Tokenizer(WordLevel(vocab, unk_token="<unk>"));raw.pre_tokenizer = Whitespace()
        hf = PreTrainedTokenizerFast(tokenizer_object=raw, unk_token="<unk>", eos_token="<eos>")
        hf.chat_template = ("{% for m in messages %}{{ m['role'] }} {{ m['content'] }} {% endfor %}"
                            "{% if add_generation_prompt %}assistant{% endif %}")
        return TokenizerWrapper(hf)

    def test_stats_match_direct_norms_for_float_and_quantized_checkpoints(self):
        from mlx import nn
        mx = self.mx
        with tempfile.TemporaryDirectory() as tmp:
            model = self.model();self.save(Path(tmp) / "float", model, self.ARGS)
            result = w.stats({}, Path(tmp) / "float", self.ARGS)
            row = next(r for r in result["tensors"] if r["name"] == "model.layers.1.mlp.up_proj.weight")
            expected = float(mx.sqrt(mx.sum(model.layers[1].mlp.up_proj.weight.astype(mx.float32) ** 2)).item())
            self.assertAlmostEqual(row["norm"], expected, places=3)
            self.assertEqual([layer["layer"] for layer in result["layers"]], [0, 1, 2])

            nn.quantize(model, group_size=32, bits=4)
            config = dict(self.ARGS, quantization={"group_size": 32, "bits": 4})
            self.save(Path(tmp) / "quant", model, config)
            result = w.stats({"prefix": "model.layers.1."}, Path(tmp) / "quant", config)
            row = next(r for r in result["tensors"] if r["name"] == "model.layers.1.mlp.up_proj.weight")
            layer = model.layers[1].mlp.up_proj
            dense = mx.dequantize(layer.weight, layer.scales, layer.biases, group_size=32, bits=4)
            self.assertTrue(row["quantized"]);self.assertEqual(row["shape"], [128, 64])
            self.assertAlmostEqual(row["norm"], float(mx.sqrt(mx.sum(dense.astype(mx.float32) ** 2)).item()), places=3)
            self.assertTrue(all(r["name"].startswith("model.layers.1.") for r in result["tensors"]))

    def test_activations_rank_layers_and_restore_the_model(self):
        mx = self.mx;model = self.model();tokens = mx.array([[2, 5, 3]], dtype=mx.int32)
        before = model(tokens);mx.eval(before);originals = list(model.layers)
        pairs = [{"positive": [{"role": "user", "content": f"task {c}"}, {"role": "assistant", "content": "yes"}],
                  "negative": [{"role": "user", "content": f"task {c}"}, {"role": "assistant", "content": "no"}]}
                 for c in "abcd"]
        with patch.object(w.steering, "load", return_value=(model, self.tokenizer())):
            result = w.activations({"pairs": pairs, "model": "unused"}, Path("."), {})
        self.assertEqual(result["pairs"], 4);self.assertEqual(result["dimension"], 64)
        self.assertEqual([row["layer"] for row in result["layers"]], [0, 1, 2])
        self.assertTrue(all(row["contrast"] > 0 and 0 <= row["alignment"] <= 1.0001 for row in result["layers"]))
        self.assertIn(result["suggested_layer"], (0, 1, 2))
        self.assertEqual(model.layers, originals)
        after = model(tokens);mx.eval(after)
        self.assertLess(float(mx.max(mx.abs(before - after))), 1e-6)

    def test_trace_ablates_layers_and_heads_against_an_intact_baseline(self):
        mx = self.mx;model = self.model();originals = list(model.layers)
        projection = model.layers[1].self_attn.o_proj
        cases = [{"messages": [{"role": "user", "content": f"task {c}"}, {"role": "assistant", "content": "yes b"}]}
                 for c in "ab"]
        with patch.object(w.steering, "load", return_value=(model, self.tokenizer())):
            layers = w.trace({"cases": cases}, Path("."), {})
            heads = w.trace({"cases": cases, "mode": "heads", "layer": 1}, Path("."), {})
            again = w.trace({"cases": cases, "layers": [2]}, Path("."), {})
        self.assertEqual([row["layer"] for row in layers["layers"]], [0, 1, 2])
        self.assertTrue(any(abs(row["delta"]) > 1e-6 for row in layers["layers"]))
        for row in layers["layers"]:
            self.assertAlmostEqual(row["delta"], row["loss"] - layers["baseline_loss"], places=6)
        self.assertEqual([row["head"] for row in heads["heads"]], [0, 1, 2, 3])
        self.assertTrue(any(abs(row["delta"]) > 1e-7 for row in heads["heads"]))
        self.assertAlmostEqual(again["baseline_loss"], layers["baseline_loss"], places=5)
        self.assertEqual(model.layers, originals);self.assertIs(model.layers[1].self_attn.o_proj, projection)

    def test_rejected_inputs_fail_before_loading(self):
        load = patch.object(w.steering, "load", side_effect=AssertionError("loaded"))
        bad_pair = [{"positive": [{"role": "user", "content": "x"}, {"role": "assistant", "content": "a"}],
                     "negative": [{"role": "user", "content": "y"}, {"role": "assistant", "content": "b"}]}]
        with load:
            with self.assertRaisesRegex(w.WeightsError, "share a prompt"):
                w.activations({"pairs": bad_pair}, Path("."), {})
            with self.assertRaisesRegex(w.WeightsError, "end with the assistant"):
                w.trace({"cases": [{"messages": [{"role": "user", "content": "x"}]}]}, Path("."), {})
            with self.assertRaisesRegex(w.WeightsError, "heads mode"):
                w.trace({"cases": [{"messages": bad_pair[0]["positive"]}], "mode": "heads"}, Path("."), {})

    def test_worker_operation_reports_identity_and_rounds_values(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp) / "model";self.save(root, self.model(), self.ARGS)
            result = w.operation({"op": "weights_map", "model": str(root)})
        self.assertEqual((result["op"], result["version"]), ("map", 1))
        self.assertEqual(len(result["model_id"]), 64)
        self.assertTrue(math.isfinite(result["totals"]["params"]))

    def test_worker_operation_caps_tensor_rows_but_keeps_layer_totals(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp) / "model";self.save(root, self.model(), self.ARGS)
            full = w.operation({"op": "weights_map", "model": str(root)})
            with patch.object(w, "MAX_TENSOR_ROWS", 1):
                capped = w.operation({"op": "weights_map", "model": str(root)})
        self.assertEqual(len(capped["tensors"]), 1)
        self.assertEqual(capped["tensors_total"], len(full["tensors"]))
        self.assertTrue(capped["tensors_truncated"])
        self.assertEqual(capped["layers"], full["layers"])
        self.assertNotIn("tensors_truncated", full)

    def test_generate_reports_limit_loop_and_contract_without_prompt_text(self):
        model = self.model()
        cases = [{"messages": [{"role": "user", "content": f"task {c}"}], "required": ["yes"]} for c in "ab"]
        cases.append({"messages": [{"role": "user", "content": "task c"}]})
        with patch.object(w.steering, "load", return_value=(model, self.tokenizer())):
            result = w.generate({"cases": cases, "max_tokens": 4}, Path("."), {})
            tailed = w.generate({"cases": cases[:1], "max_tokens": 4, "tail": 10}, Path("."), {})
        self.assertEqual([row["case"] for row in result["cases"]], [0, 1, 2])
        for row in result["cases"]:
            self.assertLessEqual(row["tokens"], 4)
            self.assertEqual(row["hit_limit"], row["finish_reason"] == "length")
            self.assertNotIn("tail", row)
        self.assertIsNone(result["cases"][2]["passed"])
        self.assertEqual(result["summary"]["checked"], 2)
        self.assertEqual(result["summary"]["hit_limit"], sum(row["hit_limit"] for row in result["cases"]))
        self.assertNotIn("task", json.dumps(result))
        self.assertLessEqual(len(tailed["cases"][0]["tail"]), 10)
        with self.assertRaisesRegex(w.WeightsError, "end with the user turn"):
            w.generate({"cases": [{"messages": [{"role": "user", "content": "x"}, {"role": "assistant", "content": "y"}]}]},
                       Path("."), {})

    def direction_inputs(self):
        def chat(task, answer):
            return {"messages": [{"role": "user", "content": f"task {task}"}, {"role": "assistant", "content": answer}]}
        return ([chat(c, "a a a a a a") for c in "abcd"], [chat(c, "yes b c") for c in "abcd"])

    def measured(self, root, model, site="mlp"):
        positive, negative = self.direction_inputs()
        output = root.parent / f"{root.name}-{site}.safetensors"
        with patch.object(w.steering, "load", return_value=(model, self.tokenizer())):
            result = w.direction({"positive": positive, "negative": negative, "site": site,
                                  "output": str(output)}, root, {})
        return result, output

    def test_direction_saves_unit_directions_and_restores_the_model(self):
        mx = self.mx
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp) / "base";model = self.model();self.save(root, model, self.ARGS)
            mlps = [layer.mlp for layer in model.layers];originals = list(model.layers)
            for site in ("mlp", "residual"):
                with self.subTest(site=site):
                    result, output = self.measured(root, model, site)
                    saved, metadata = mx.load(str(output), return_metadata=True)
                    self.assertEqual(saved["directions"].shape, (3, 64))
                    norms = mx.sqrt(mx.sum(saved["directions"] ** 2, axis=-1)).tolist()
                    self.assertTrue(all(abs(n - 1) < 1e-4 for n in norms))
                    self.assertEqual((metadata["format"], metadata["site"], metadata["model_id"]),
                                     (w.DIRECTION_FORMAT, site, w.steering.identity(root)))
                    self.assertEqual([row["layer"] for row in result["layers"]], [0, 1, 2])
                    self.assertTrue(all(row["gap"] > 0 for row in result["layers"]))
                    self.assertEqual(len(result["suggested_layers"]), 2)
                    self.assertNotIn("task", json.dumps(result))
            self.assertEqual(model.layers, originals)
            self.assertEqual([layer.mlp for layer in model.layers], mlps)
            with self.assertRaisesRegex(w.WeightsError, "new .safetensors path"):
                self.measured(root, model)

    def test_float_edit_projects_out_chosen_columns_in_a_linked_copy(self):
        import hashlib
        mx = self.mx
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp) / "base";model = self.model();self.save(root, model, self.ARGS)
            before = hashlib.sha256((root / "model.safetensors").read_bytes()).hexdigest()
            _, direction = self.measured(root, model)
            out = Path(tmp) / "edited"
            result = w.operation({"op": "weights_edit", "model": str(root), "direction": str(direction),
                                  "layers": [1], "columns": 8, "alpha": 1.0, "output": str(out)})
            self.assertEqual(hashlib.sha256((root / "model.safetensors").read_bytes()).hexdigest(), before)
            self.assertEqual((root / "config.json").stat().st_ino, (out / "config.json").stat().st_ino)
            self.assertEqual((result["shards_rewritten"], result["files_linked"], result["edited_params"]), (1, 1, 64 * 8))
            self.assertFalse(result["quantized"])
            self.assertGreater(result["fraction_changed"], 0)
            self.assertLess(result["fraction_changed"], 0.05)
            base, edited = mx.load(str(root / "model.safetensors")), mx.load(str(out / "model.safetensors"))
            name = "model.layers.1.mlp.down_proj.weight"
            changed = mx.any(base[name] != edited[name], axis=0).tolist()
            row = result["layers"][0]
            self.assertEqual([j for j, flag in enumerate(changed) if flag], sorted(row["columns"]))
            d = mx.load(str(direction))["directions"][1]
            columns = mx.array(row["columns"])
            after = mx.max(mx.abs(d @ edited[name].astype(mx.float32)[:, columns])).item()
            self.assertLess(after, 1e-3 * max(1.0, row["projection_before"]))
            for key in base:
                if key != name:
                    self.assertTrue(mx.array_equal(base[key], edited[key]), key)
            record = json.loads((out / w.EDIT_RECORD).read_text())
            self.assertEqual((record["base"], record["method"], record["layers"][0]["columns"]),
                             (str(root.resolve()), "project_out", row["columns"]))
            with self.assertRaisesRegex(w.WeightsError, "new directory"):
                w.operation({"op": "weights_edit", "model": str(root), "direction": str(direction),
                             "layers": [1], "output": str(out)})
            again = Path(tmp) / "again"
            with self.assertRaisesRegex(w.WeightsError, "different model"):
                w.operation({"op": "weights_edit", "model": str(out), "direction": str(direction),
                             "layers": [1], "output": str(again)})
            self.assertFalse(again.exists())
            self.assertEqual([p.name for p in Path(tmp).iterdir() if p.name.startswith(".")], [])

    def test_quantized_edit_keeps_untouched_groups_byte_identical(self):
        from mlx import nn
        mx = self.mx
        with tempfile.TemporaryDirectory() as tmp:
            model = self.model();nn.quantize(model, group_size=32, bits=4)
            config = dict(self.ARGS, quantization={"group_size": 32, "bits": 4})
            root = Path(tmp) / "base";self.save(root, model, config)
            _, direction = self.measured(root, model)
            out = Path(tmp) / "edited"
            result = w.operation({"op": "weights_edit", "model": str(root), "direction": str(direction),
                                  "layers": [2], "columns": 1, "output": str(out)})
            row = result["layers"][0]
            self.assertTrue(result["quantized"])
            self.assertTrue(math.isfinite(row["requantization_error"]))
            self.assertEqual(row["rewritten_params"], 64 * 32)
            base, edited = mx.load(str(root / "model.safetensors")), mx.load(str(out / "model.safetensors"))
            module = "model.layers.2.mlp.down_proj"
            group = row["columns"][0] // 32
            scales = mx.any(base[f"{module}.scales"] != edited[f"{module}.scales"], axis=0).tolist()
            self.assertTrue(all(not flag for index, flag in enumerate(scales) if index != group))
            words = mx.any(base[f"{module}.weight"] != edited[f"{module}.weight"], axis=0).tolist()
            self.assertTrue(all(not flag for index, flag in enumerate(words) if index // 4 != group))
            self.assertTrue(any(words))
            self.assertLess(row["projection_after"], row["projection_before"])


if __name__ == "__main__":
    unittest.main()
