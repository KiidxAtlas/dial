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

    def test_try_scores_an_edit_in_memory_and_restores_the_model(self):
        mx = self.mx
        with tempfile.TemporaryDirectory() as tmp:
            model = self.model();root = Path(tmp) / "base";self.save(root, model, self.ARGS)
            name = "model.layers.1.mlp.down_proj.weight"
            before = mx.array(model.layers[1].mlp.down_proj.weight);files = sorted(p.name for p in Path(tmp).rglob("*"))
            cases = [{"messages": [{"role": "user", "content": f"task {c}"}, {"role": "assistant", "content": "yes b"}]}
                     for c in "ab"]
            request = {"cases": cases, "neutral": cases[:1], "method": "scale", "doses": [0.5, 1.0],
                       "changes": [{"tensor": name, "scale": 0.0}]}
            with patch.object(w.steering, "load", return_value=(model, self.tokenizer())):
                result = w.try_edit(request, root, self.ARGS)
                none = w.try_edit(dict(request, changes=[{"tensor": name, "scale": 1.0}]), root, self.ARGS)
            self.assertEqual([row["dose"] for row in result["doses"]], [0.5, 1.0])
            half, full = result["doses"]
            self.assertGreater(abs(full["delta"]), 1e-6);self.assertGreater(full["neutral_kl"], 0)
            self.assertGreater(full["neutral_kl"], half["neutral_kl"])
            for row in none["doses"]:
                self.assertAlmostEqual(row["delta"], 0.0, places=5);self.assertAlmostEqual(row["neutral_kl"], 0.0, places=5)
            self.assertTrue(mx.array_equal(model.layers[1].mlp.down_proj.weight, before))
            self.assertEqual(sorted(p.name for p in Path(tmp).rglob("*")), files)
            with patch.object(w.steering, "load", side_effect=AssertionError("loaded")):
                with self.assertRaisesRegex(w.WeightsError, "doses must be"):
                    w.try_edit(dict(request, doses=[3]), root, self.ARGS)

    def test_attribute_estimates_removal_effects_and_restores_the_model(self):
        mx = self.mx;model = self.model();originals = list(model.layers)
        projection = model.layers[1].self_attn.o_proj;down = model.layers[1].mlp.down_proj
        cases = [{"messages": [{"role": "user", "content": f"task {c}"}, {"role": "assistant", "content": "yes b"}]}
                 for c in "ab"]
        wanted = [{"messages": [{"role": "user", "content": "task c"}, {"role": "assistant", "content": "no a"}]}]
        with patch.object(w.steering, "load", return_value=(model, self.tokenizer())):
            plain = w.attribute({"cases": cases, "top": 5}, Path("."), {})
            selective = w.attribute({"cases": cases, "wanted": wanted, "top": 5}, Path("."), {})
            # The estimate is first order: it should rank the truly most important head near the top.
            exact = w.trace({"cases": cases, "mode": "heads", "layer": 1}, Path("."), {})["heads"]
        self.assertEqual((plain["ranked_by"], selective["ranked_by"]), ("support", "selective"))
        self.assertEqual(len(plain["heads"]), 5);self.assertEqual(len(plain["neurons"]), 5)
        self.assertEqual({row["layer"] for row in plain["layers"]}, {0, 1, 2})
        self.assertTrue(all(plain["neurons"][i]["support"] >= plain["neurons"][i + 1]["support"] for i in range(4)))
        layer_one = {row["head"]: row["support"] for row in plain["heads"] if row["layer"] == 1}
        if len(layer_one) >= 2:
            best = max(exact, key=lambda row: row["delta"])["head"]
            self.assertIn(best, layer_one)
        self.assertEqual(model.layers, originals)
        self.assertIs(model.layers[1].self_attn.o_proj, projection);self.assertIs(model.layers[1].mlp.down_proj, down)

    def test_attribute_scores_repeats_against_neutral_text_and_explains_components(self):
        model = self.model();originals = list(model.layers)
        looping = [{"messages": [{"role": "user", "content": "task a"},
                                 {"role": "assistant", "content": "a b c a b c a b c a b c"}]}]
        ordinary = [{"messages": [{"role": "user", "content": "task b"}, {"role": "assistant", "content": "yes no"}]}]
        with patch.object(w.steering, "load", return_value=(model, self.tokenizer())):
            result = w.attribute({"cases": looping, "neutral": ordinary, "focus": "repeats", "top": 4},
                                 Path("."), {})
            plain = w.attribute({"cases": looping, "top": 4, "explain": False}, Path("."), {})
            with self.assertRaisesRegex(w.WeightsError, "repeated 3-token"):
                w.attribute({"cases": ordinary, "focus": "repeats"}, Path("."), {})
        self.assertEqual((result["ranked_by"], result["focus"], plain["ranked_by"]), ("specific", "repeats", "support"))
        for row in result["heads"] + result["neurons"]:
            self.assertTrue(0.0 <= row["agreement"] <= 1.0)
            self.assertIn("neutral", row)
        self.assertTrue(all(len(row["promotes"]) == 5 and len(row["suppresses"]) == 5 for row in result["neurons"]))
        self.assertTrue(all(0.0 <= row["induction"] <= 1.0 for row in result["heads"]), result.get("notes"))
        self.assertNotIn("promotes", plain["neurons"][0])
        self.assertEqual(model.layers, originals)

    def test_analyze_builds_an_atlas_without_changing_the_model(self):
        model = self.model();originals = list(model.layers)
        with patch.object(w.steering, "load", return_value=(model, self.tokenizer())):
            result = w.analyze({}, Path("."), {})
        self.assertEqual(len(result["layers"]), 3)
        self.assertEqual(result["probes"], len(w.PROBES))
        for row in result["layers"]:
            self.assertIn(row["stage"], ("early", "middle", "late"))
            self.assertEqual(row["head_count"], 4)
            self.assertEqual(sum(row["head_roles"].values()), 4)
            self.assertEqual(len(row["neurons"]), w.ANALYZE_NEURONS)
            self.assertTrue(all(len(n["promotes"]) == 5 for n in row["neurons"]), result.get("notes"))
        self.assertEqual(sorted(result["summary"]["heads_by_role"]), ["attention-sink", "induction", "mixed", "previous-token"])
        self.assertEqual(model.layers, originals)

    def test_recorded_attention_still_exposes_the_wrapped_module(self):
        # Some forward passes read attributes of the attention module (mlx-vlm Qwen3.5 reads
        # `self_attn.rotary_emb`); the recorder must forward them while it is installed.
        model = self.model();attention = model.layers[0].self_attn
        with w.attention_inputs(model, [0]) as seen:
            self.assertIs(model.layers[0].self_attn.rope, attention.rope)
            model(self.mx.array([[4, 5, 6]], dtype=self.mx.int32))
            self.assertEqual(seen[0].shape, (1, 3, 64))
        self.assertIs(model.layers[0].self_attn, attention)

    def test_head_and_neuron_selectors_resolve_to_projection_columns(self):
        mx = self.mx
        with tempfile.TemporaryDirectory() as tmp:
            model = self.model();root = Path(tmp) / "base";self.save(root, model, self.ARGS)
            out = Path(tmp) / "out"
            result = w.operation({"op": "weights_edit", "model": str(root), "method": "scale", "output": str(out),
                                  "changes": [{"layer": 1, "heads": [2], "scale": 0.0},
                                              {"layer": 1, "neurons": [7, 9], "scale": 0.0}]})
            by_name = {row["tensor"]: row for row in result["changes"]}
            self.assertEqual(by_name["model.layers.1.self_attn.o_proj.weight"]["columns"], 16)
            self.assertEqual(by_name["model.layers.1.mlp.down_proj.weight"]["columns"], 2)
            edited = mx.load(str(out / "model.safetensors"))
            o_proj = edited["model.layers.1.self_attn.o_proj.weight"]
            self.assertEqual(float(mx.abs(o_proj[:, 32:48]).max().item()), 0.0)
            self.assertGreater(float(mx.abs(o_proj[:, :32]).max().item()), 0.0)
            for bad, message in (([{"layer": 1, "heads": [4], "scale": 0}], "heads 0-3"),
                                 ([{"layer": 1, "heads": [0], "neurons": [0], "scale": 0}], "exactly one"),
                                 ([{"layer": 9, "heads": [0], "scale": 0}], "no single")):
                with self.assertRaisesRegex(w.WeightsError, message):
                    w.operation({"op": "weights_edit", "model": str(root), "method": "scale",
                                 "output": str(Path(tmp) / "bad"), "changes": bad})

    def test_quantized_edit_smaller_than_its_rounding_noise_is_refused_unless_allowed(self):
        from mlx import nn
        with tempfile.TemporaryDirectory() as tmp:
            model = self.model();nn.quantize(model, group_size=32, bits=4)
            config = dict(self.ARGS, quantization={"group_size": 32, "bits": 4})
            root = Path(tmp) / "base";self.save(root, model, config)
            change = [{"tensor": "model.layers.2.mlp.down_proj.weight", "scale": 1.001, "rows": [4], "columns": [40]}]
            request = {"op": "weights_edit", "model": str(root), "method": "scale", "changes": change}
            with self.assertRaisesRegex(w.WeightsError, "rounding noise"):
                w.operation(dict(request, output=str(Path(tmp) / "refused")))
            self.assertFalse((Path(tmp) / "refused").exists())
            result = w.operation(dict(request, output=str(Path(tmp) / "forced"), allow_noisy=True))
            self.assertGreater(result["changes"][0]["requantization_error"], 0.5)

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


    def test_scale_edit_changes_only_the_named_elements(self):
        mx = self.mx
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp) / "base";model = self.model();self.save(root, model, self.ARGS)
            out = Path(tmp) / "scaled"
            name = "model.layers.1.mlp.down_proj.weight"
            result = w.operation({"op": "weights_edit", "model": str(root), "method": "scale", "output": str(out),
                                  "changes": [{"tensor": name, "scale": 0.5, "rows": [0, 3], "columns": [1, 2, 5]}]})
            self.assertEqual((result["method"], result["edited_params"], result["quantized"]), ("scale", 6, False))
            base, edited = mx.load(str(root / "model.safetensors")), mx.load(str(out / "model.safetensors"))
            expected = base[name]
            mask = mx.zeros(expected.shape, dtype=mx.bool_)
            for r in (0, 3):
                for c in (1, 2, 5):
                    mask[r, c] = True
            self.assertTrue(mx.allclose(edited[name], mx.where(mask, expected * 0.5, expected)))
            for key in base:
                if key != name:
                    self.assertTrue(mx.array_equal(base[key], edited[key]), key)
            record = json.loads((out / w.EDIT_RECORD).read_text())
            self.assertEqual((record["method"], record["changes"][0]["tensor"]), ("scale", name))
            for bad, message in (
                    ([{"tensor": "no.such.weight", "scale": 1}], "not a tensor"),
                    ([{"tensor": name, "scale": 99}], "scale must be"),
                    ([{"tensor": name, "scale": 1, "rows": [64]}], "integers below"),
                    ([{"tensor": name, "scale": 1}, {"tensor": name, "scale": 2}], "distinct")):
                with self.assertRaisesRegex(w.WeightsError, message):
                    w.operation({"op": "weights_edit", "model": str(root), "method": "scale",
                                 "output": str(Path(tmp) / "bad"), "changes": bad})
            self.assertFalse((Path(tmp) / "bad").exists())

    def test_scale_edit_on_quantized_weights_rewrites_only_touched_groups(self):
        from mlx import nn
        mx = self.mx
        with tempfile.TemporaryDirectory() as tmp:
            model = self.model();nn.quantize(model, group_size=32, bits=4)
            config = dict(self.ARGS, quantization={"group_size": 32, "bits": 4})
            root = Path(tmp) / "base";self.save(root, model, config)
            out = Path(tmp) / "scaled"
            module = "model.layers.2.mlp.down_proj"
            result = w.operation({"op": "weights_edit", "model": str(root), "method": "scale", "output": str(out),
                                  "changes": [{"tensor": f"{module}.weight", "scale": 0.0, "rows": [4], "columns": [40]}]})
            row = result["changes"][0]
            self.assertTrue(result["quantized"])
            self.assertEqual(row["rewritten_params"], 32)
            self.assertTrue(math.isfinite(row["requantization_error"]))
            base, edited = mx.load(str(root / "model.safetensors")), mx.load(str(out / "model.safetensors"))
            scales = mx.any(base[f"{module}.scales"] != edited[f"{module}.scales"], axis=1).tolist()
            self.assertTrue(all(not flag for index, flag in enumerate(scales) if index != 4))
            self.assertLess(row["norm_after"], row["norm_before"])

    def test_combine_sums_scale_edits_exactly_and_refuses_edits_of_another_base(self):
        mx = self.mx
        name = "model.layers.1.mlp.down_proj.weight"
        with tempfile.TemporaryDirectory() as tmp:
            root, first, second = Path(tmp) / "base", Path(tmp) / "first", Path(tmp) / "second"
            model = self.model();self.save(root, model, self.ARGS)
            w.operation({"op": "weights_edit", "model": str(root), "method": "scale", "output": str(first),
                         "changes": [{"tensor": name, "scale": 0.5, "rows": [0, 3]}]})
            w.operation({"op": "weights_edit", "model": str(root), "method": "scale", "output": str(second),
                         "changes": [{"tensor": name, "scale": 2.0, "columns": [1, 2]}]})
            out = Path(tmp) / "combined"
            result = w.operation({"op": "weights_combine", "model": str(root), "edits": [str(first), str(second)],
                                  "output": str(out)})
            base, a, b, combined = [mx.load(str(path / "model.safetensors")) for path in (root, first, second, out)]
            expected = base[name] + (a[name] - base[name]) + (b[name] - base[name])
            self.assertTrue(mx.allclose(combined[name], expected, atol=1e-6))
            # Rows 0 and 3 (256 elements) plus columns 1 and 2 (128), sharing 4 elements.
            self.assertEqual((result["method"], result["changes"][0]["edited_params"]), ("combine", 380))
            for key in base:
                if key != name:
                    self.assertTrue(mx.array_equal(base[key], combined[key]), key)
            record = json.loads((out / w.EDIT_RECORD).read_text())
            self.assertEqual((record["method"], record["sources"]), ("combine", [str(first), str(second)]))
            other, refused = Path(tmp) / "other", Path(tmp) / "other-edit"
            self.save(other, self.model(), self.ARGS)
            w.operation({"op": "weights_edit", "model": str(other), "method": "scale", "output": str(refused),
                         "changes": [{"tensor": name, "scale": 0.5}]})
            with self.assertRaisesRegex(w.WeightsError, "not made from this base"):
                w.operation({"op": "weights_combine", "model": str(root), "edits": [str(first), str(refused)],
                             "output": str(Path(tmp) / "refused")})
            self.assertFalse((Path(tmp) / "refused").exists())

    def test_steering_edit_folds_the_placed_delta_into_the_copy_without_an_adapter(self):
        mx = self.mx
        name = "model.layers.1.mlp.down_proj"
        with tempfile.TemporaryDirectory() as tmp:
            root, out = Path(tmp) / "base", Path(tmp) / "steered"
            model = self.model();self.save(root, model, self.ARGS)
            bundle, data = Path(tmp) / "bundle.json", Path(tmp) / "records.jsonl"
            bundle.write_text("{}");data.write_text("")
            column, row = mx.random.normal((128, 1)), mx.random.normal((1, 64))
            seen = {}

            def bake(request):
                # The contract of steering.bake: a scratch adapter directory holding the placed pair.
                seen["output"] = Path(request["output"])
                seen["output"].mkdir(parents=True)
                mx.save_safetensors(str(seen["output"] / "adapters.safetensors"),
                                    {name + ".lora_a": column, name + ".lora_b": row})
                return {"output": str(seen["output"]), "layer": 1, "module": "mlp.down_proj",
                        "relative_error": 0.0, "samples": 4}
            with patch.object(w.steering, "bake", side_effect=bake):
                result = w.operation({"op": "weights_edit", "model": str(root), "method": "steering",
                                      "bundle": str(bundle), "strength": 1.0, "data": str(data), "output": str(out)})
            base, edited = mx.load(str(root / "model.safetensors")), mx.load(str(out / "model.safetensors"))
            delta = row.reshape(-1, 1) * column.reshape(1, -1)
            self.assertTrue(mx.allclose(edited[name + ".weight"], base[name + ".weight"] + delta, atol=1e-5))
            self.assertEqual((result["method"], result["changes"][0]["tensor"]), ("steering", name + ".weight"))
            self.assertFalse(seen["output"].parent.exists())
            self.assertEqual(sorted(p.name for p in out.iterdir() if p.name != w.EDIT_RECORD), sorted(
                p.name for p in root.iterdir()))
            with self.assertRaisesRegex(w.WeightsError, "absolute path"):
                w.operation({"op": "weights_edit", "model": str(root), "method": "steering", "bundle": "bundle.json",
                             "strength": 1.0, "data": str(data), "output": str(Path(tmp) / "bad")})

    def test_generate_reports_probe_loss_only_when_requested(self):
        model = self.model()
        cases = [{"messages": [{"role": "user", "content": "task a"}]}]
        with patch.object(w.steering, "load", return_value=(model, self.tokenizer())):
            plain = w.generate({"cases": cases, "max_tokens": 2}, Path("."), {})
            probed = w.generate({"cases": cases, "max_tokens": 2, "probe_loss": True}, Path("."), {})
            expected, count = w.loss(model, w.probe_encoded(self.tokenizer()))
        self.assertNotIn("probe_loss", plain)
        self.assertAlmostEqual(probed["probe_loss"], expected, places=5)
        self.assertEqual(probed["probe_tokens"], count)
        with self.assertRaisesRegex(w.WeightsError, "probe_loss must be"):
            w.generate({"cases": cases, "probe_loss": "yes"}, Path("."), {})


class Qwen35Moe(unittest.TestCase):
    """A tiny Qwen3.5-MoE language model built from mlx-vlm's own classes: gated attention on every
    fourth layer, linear attention elsewhere, and a routed mixture of experts with a shared expert."""
    @classmethod
    def setUpClass(cls):
        try:
            import mlx.core as mx
            from mlx_vlm.models.qwen3_5_moe.config import TextConfig
            from mlx_vlm.models.qwen3_5_moe.language import LanguageModel
        except ImportError as error:
            raise unittest.SkipTest(str(error))
        cls.mx, cls.TextConfig, cls.LanguageModel = mx, TextConfig, LanguageModel

    ARGS = dict(model_type="qwen3_5_moe", hidden_size=64, num_hidden_layers=4, num_attention_heads=4,
                num_key_value_heads=2, head_dim=16, linear_num_value_heads=2, linear_num_key_heads=2,
                linear_key_head_dim=16, linear_value_head_dim=16, linear_conv_kernel_dim=4, num_experts=8,
                num_experts_per_tok=2, shared_expert_intermediate_size=32, moe_intermediate_size=32,
                rms_norm_eps=1e-6, vocab_size=64, max_position_embeddings=512, full_attention_interval=4)

    def language(self):
        import types
        self.mx.random.seed(0)
        # bfloat16 weights, as the checkpoints the worker serves are stored.
        language = self.LanguageModel(self.TextConfig(**self.ARGS), config=types.SimpleNamespace(
            vision_config=types.SimpleNamespace(spatial_merge_size=2), image_token_id=1, video_token_id=2,
            vision_start_token_id=3))
        language.set_dtype(self.mx.bfloat16)
        return language

    def model(self):
        # The worker sees a vision-language model: its decoder is `language_model`, exposed through steering.
        import types
        return w.steering.text_model(types.SimpleNamespace(language_model=self.language()))

    def tokenizer(self):
        return Metal.tokenizer(self)

    def test_gated_attention_scores_only_the_query_half_of_q_proj(self):
        mx = self.mx
        attention = w.steering.layer_list(self.model())[3].self_attn
        x = mx.random.normal((1, 5, 64))
        before = w.attention_pattern(attention, 4, x)
        mx.eval(before)
        # Per head, q_proj emits 16 query rows then 16 gate rows; the gate never enters the scores.
        gate_rows = mx.arange(attention.q_proj.weight.shape[0]) % 32 >= 16
        weight = attention.q_proj.weight
        attention.q_proj.weight = mx.where(gate_rows[:, None], mx.random.normal(weight.shape), weight)
        self.assertLess(float(mx.max(mx.abs(before - w.attention_pattern(attention, 4, x)))), 1e-5)
        attention.q_proj.weight = mx.where(gate_rows[:, None], weight, mx.random.normal(weight.shape))
        self.assertGreater(float(mx.max(mx.abs(before - w.attention_pattern(attention, 4, x)))), 1e-3)
        attention.q_proj.weight = weight
        self.assertEqual(before.shape, (4, 5, 5))
        self.assertLess(float(mx.max(mx.abs(before * mx.triu(mx.ones((5, 5)), k=1)))), 1e-6)
        self.assertLess(float(mx.max(mx.abs(mx.sum(before, axis=-1) - 1))), 1e-4)

    def test_analyze_classifies_full_attention_heads_and_reads_routing(self):
        model = self.model()
        with patch.object(w.steering, "load", return_value=(model, self.tokenizer())):
            result = w.analyze({}, Path("."), {})
        rows = {row["layer"]: row for row in result["layers"]}
        for index in (0, 1, 2):
            self.assertEqual(rows[index]["attention"], "other")
            self.assertGreater(rows[index]["linear_attention"]["output_rms"], 0)
        self.assertEqual((rows[3]["attention"], rows[3]["head_count"]), ("standard", 4))
        self.assertEqual(sum(rows[3]["head_roles"].values()), 4)
        for index in range(4):
            experts = rows[index]["experts"]
            self.assertEqual((experts["count"], experts["top_k"]), (8, 2))
            self.assertTrue(0 <= experts["entropy"] <= 1)
            self.assertAlmostEqual(sum(item["share"] for item in experts["top"]), 1.0, places=5)
            self.assertTrue(0 <= experts["shared_gate"] <= 1)
        self.assertEqual(result["summary"]["routing"]["moe_layers"], 4)
        # Linear-attention and MoE layers are expected notes; a failed routing or head read would be a note too.
        self.assertFalse([note for note in result.get("notes", []) if "not read" in note or "not classified:" in note])

    def test_expert_and_shared_selectors_scale_only_their_slice(self):
        mx = self.mx
        from mlx.utils import tree_flatten
        with tempfile.TemporaryDirectory() as tmp:
            root, out = Path(tmp) / "base", Path(tmp) / "scaled"
            root.mkdir()
            (root / "config.json").write_text(json.dumps(self.ARGS))
            mx.save_safetensors(str(root / "model.safetensors"), dict(tree_flatten(self.language().parameters())))
            experts = "model.layers.1.mlp.switch_mlp.down_proj.weight"
            shared = "model.layers.2.mlp.shared_expert.down_proj.weight"
            w.operation({"op": "weights_edit", "model": str(root), "method": "scale", "output": str(out),
                         "changes": [{"layer": 1, "experts": [2, 5], "scale": 0.0},
                                     {"layer": 2, "shared_expert": True, "scale": 0.5}]})
            base, edited = mx.load(str(root / "model.safetensors")), mx.load(str(out / "model.safetensors"))
            chosen = mx.array([index in (2, 5) for index in range(8)])[:, None, None]
            self.assertTrue(mx.allclose(edited[experts], mx.where(chosen, 0.0, base[experts])))
            self.assertTrue(mx.allclose(edited[shared], base[shared] * 0.5))
            for key in base:
                if key not in (experts, shared):
                    self.assertTrue(mx.array_equal(base[key], edited[key]), key)
            for bad, message in (
                    ({"layer": 1, "experts": [8], "scale": 1}, "integers below 8"),
                    ({"layer": 2, "shared_expert": True, "experts": [0], "scale": 1}, "exactly one"),
                    ({"tensor": shared, "experts": [0], "scale": 1}, "not a stack of experts"),
                    ({"tensor": experts, "rows": [0], "scale": 1}, "stack of experts")):
                with self.assertRaisesRegex(w.WeightsError, message):
                    w.operation({"op": "weights_edit", "model": str(root), "method": "scale",
                                 "output": str(Path(tmp) / "bad"), "changes": [bad]})

    def test_attribute_ranks_routed_experts_and_restores_the_routing(self):
        model = self.model();block = w.steering.layer_list(model)[1].mlp
        switch, shared = block.switch_mlp, block.shared_expert
        cases = [{"messages": [{"role": "user", "content": f"task {c}"}, {"role": "assistant", "content": "yes b"}]}
                 for c in "ab"]
        with patch.object(w.steering, "load", return_value=(model, self.tokenizer())):
            result = w.attribute({"cases": cases, "top": 5}, Path("."), {})
        self.assertEqual(len(result["experts"]), 5)
        self.assertTrue(all(0 <= row["expert"] < 8 and 0 <= row["layer"] < 4 for row in result["experts"]))
        self.assertTrue(all(0 <= row["agreement"] <= 1 for row in result["experts"]))
        self.assertEqual({row["component"] for row in result["layers"]} & {"moe", "shared_expert"},
                         {"moe", "shared_expert"})
        self.assertIs(block.switch_mlp, switch);self.assertIs(block.shared_expert, shared)


if __name__ == "__main__":
    unittest.main()
