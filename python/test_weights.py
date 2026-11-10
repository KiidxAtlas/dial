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

    def test_structural_roles_do_not_claim_semantic_specialization(self):
        cases = {
            "language_model.model.layers.0.self_attn.q_proj.weight": "query",
            "language_model.model.layers.0.router.proj.weight": "router",
            "language_model.model.layers.0.experts.switch_glu.down_proj.weight": "down",
            "language_model.model.layers.0.input_layernorm.weight": "norm",
            "vision_tower.encoder.layers.0.self_attn.q_proj.weight": "vision",
            "language_model.model.layers.0.router.per_expert_scale": "unknown",
            "model.layers.0.linear_attn.in_proj_qkvz.weight": "unknown",
        }
        for name, role in cases.items():
            self.assertEqual(w.tensor_role({"name": name, "component": w.component(name)}), role)

    def test_map_cache_refreshes_on_metadata_change_without_touching_weights(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            config = root / "config.json"
            config.write_text('{"model_type":"gemma4"}')
            shard = root / "model.safetensors"
            write_safetensors(shard, {"model.layers.0.self_attn.q_proj.weight": ("BF16", [2, 2], 8)})
            before = shard.read_bytes()
            request = {"op": "weights_map", "model": str(root)}
            first = w.operation(request)
            self.assertEqual(first["tensors"][0]["role"], "query")
            self.assertEqual(first["knowledge"]["roles"]["query"]["evidence_kind"], "structural_inference")
            self.assertIn("No activations", first["knowledge"]["limits"])
            request["known_model_id"] = first["model_id"]
            with patch.object(w, "tensors", side_effect=AssertionError("cache should skip headers")):
                self.assertTrue(w.operation(request)["unchanged"])
            config.write_text('{"model_type":"gemma4", "hidden_size":2}')
            refreshed = w.operation(request)
            self.assertNotEqual(refreshed["model_id"], first["model_id"])
            self.assertEqual(refreshed["architecture"]["hidden_size"], 2)
            self.assertEqual(shard.read_bytes(), before)

    def test_map_refuses_metadata_change_during_scan(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "config.json").write_text('{}')
            write_safetensors(root / "model.safetensors", {"w": ("BF16", [1], 2)})
            with patch.object(w.steering, "identity", side_effect=["before", "after"]):
                with self.assertRaisesRegex(w.WeightsError, "metadata changed"):
                    w.operation({"op": "weights_map", "model": str(root)})

    def test_gemma_mixed_precision_keeps_role_and_group_metadata(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            name = "language_model.model.layers.0.router.proj"
            config = {"model_type": "gemma4", "text_config": {"hidden_size":64, "num_experts":128},
                      "quantization": {"bits":5, "group_size":64, name: {"bits":8}}}
            write_safetensors(root / "model.safetensors", {
                name + ".weight": ("U32", [128, 16], 8192),
                name + ".scales": ("BF16", [128, 1], 256),
                name + ".biases": ("BF16", [128, 1], 256)})
            result = w.weight_map({}, root, config)
            row = result["tensors"][0]
            self.assertEqual((row["role"], row["shape"], row["quantization"]["bits"]),
                             ("router", [128, 64], 8))
            self.assertEqual(result["architecture"]["experts"], 128)

    def test_investigation_plan_is_bounded_and_distributed(self):
        self.assertEqual(w.investigation_plan(30, 4, None, 24), ([0, 9, 19, 29], 24))
        self.assertEqual(w.investigation_plan(30, 4, None, 12), ([15], 12))
        self.assertEqual(w.investigation_plan(1, 4, None, 24), ([0], 12))
        for selected, budget in (([0, 0], 24), ([30], 24), ([True], 24), ([], 24), (None, 11), ([0, 1], 12)):
            with self.subTest(selected=selected, budget=budget):
                with self.assertRaises(w.WeightsError):
                    w.investigation_plan(30, 4, selected, budget)

    def test_investigation_refuses_invalid_budget_before_model_load(self):
        with patch.object(w.steering, "load", side_effect=AssertionError("model must not load")):
            for request in ({"budget":0}, {"budget":257}, {"budget":True}, {"budget":11},
                            {"layers":[0, 0]}, {"cases":[]}, {"layers":[99]}):
                with self.subTest(request=request), self.assertRaises(w.WeightsError):
                    w.investigate(request, Path("."), {"num_hidden_layers":30})

    def test_investigation_case_deltas_and_weighted_mean_known_answer(self):
        from types import SimpleNamespace
        from contextlib import nullcontext
        import dial_worker
        model = SimpleNamespace(eval=lambda: None, layers=[object()])
        cases = [{"messages":[{"role":"user","content":"private question"},
                              {"role":"assistant","content":"private answer"}]}] * 2
        scores = [(2., 1), (4., 3), (3., 1), (2., 3), (2., 1), (4., 3)]
        with patch.object(w.steering, "load", return_value=(model, object())), \
             patch.object(dial_worker, "reasoning_renderer", return_value=lambda x:x), \
             patch.object(dial_worker, "encode", return_value=([1, 2, 3, 4], 1)), \
             patch.object(w, "skipped", return_value=nullcontext()), \
             patch.object(w, "loss", side_effect=scores) as score:
            result = w.investigate({"cases":cases,"budget":6}, Path("."), {"num_hidden_layers":1})
        self.assertEqual(score.call_count, 6)
        self.assertEqual(result["forward_passes"], 6)
        self.assertEqual([r["delta"] for r in result["layers"][0]["cases"]], [1., -2.])
        self.assertEqual(result["layers"][0]["delta"], -1.25)
        self.assertTrue(result["restoration_checked"])
        self.assertNotIn("private question", json.dumps(result))
        self.assertNotIn("private answer", json.dumps(result))
        self.assertEqual(result["cases"][0]["fingerprint"], w.steering.digest(cases[0]))


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

    def test_investigate_measures_effects_and_restores_real_mlx_layers(self):
        mx = self.mx
        model = self.model(); originals = list(model.layers)
        cases = [{"messages":[{"role":"user","content":"task a"},
                              {"role":"assistant","content":answer}]} for answer in ("yes", "no b")]
        tokens = mx.array([[2, 4, 3]])
        before = model(tokens); mx.eval(before)
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp) / "model"
            self.save(root, model, self.ARGS)
            files = {p.name:p.read_bytes() for p in root.iterdir()}
            with patch.object(w.steering, "load", return_value=(model, self.tokenizer())), \
                 patch.object(w, "loss", wraps=w.loss) as score:
                result = w.investigate({"cases":cases,"layers":[0,2],"budget":8}, root, self.ARGS)
                self.assertEqual(score.call_count, 8)
            self.assertEqual({p.name:p.read_bytes() for p in root.iterdir()}, files)
        self.assertEqual(result["forward_passes"], 8)
        self.assertTrue(result["restoration_checked"])
        self.assertEqual([r["layer"] for r in result["layers"]], [0, 2])
        self.assertTrue(any(abs(r["delta"]) > 1e-6 for r in result["layers"]))
        self.assertEqual(model.layers, originals)
        after = model(tokens); mx.eval(after)
        self.assertLess(float(mx.max(mx.abs(before - after))), 1e-6)

    def test_investigate_restores_on_failure_and_refuses_baseline_drift(self):
        model = self.model(); originals = list(model.layers)
        cases = [{"messages":[{"role":"user","content":"task a"},
                              {"role":"assistant","content":"yes"}]}]
        request = {"cases":cases,"layers":[0],"budget":3}
        with patch.object(w.steering, "load", return_value=(model, self.tokenizer())):
            with patch.object(w, "loss", side_effect=[(1., 1), RuntimeError("scoring failed")]):
                with self.assertRaisesRegex(RuntimeError, "scoring failed"):
                    w.investigate(request, Path("."), self.ARGS)
            self.assertEqual(model.layers, originals)
            with patch.object(w, "loss", side_effect=[(1., 1), (2., 1), (3., 1)]):
                with self.assertRaisesRegex(w.WeightsError, "not reproduced"):
                    w.investigate(request, Path("."), self.ARGS)
            self.assertEqual(model.layers, originals)

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
        self.assertEqual(result["probes"], sum(len(texts) for _, texts in w.CATEGORY_PROBES))
        self.assertEqual(result["categories"], [label for label, _ in w.CATEGORY_PROBES])
        for row in result["layers"]:
            self.assertIn(row["stage"], ("early", "middle", "late"))
            self.assertEqual(row["head_count"], 4)
            self.assertEqual(sum(row["head_roles"].values()), 4)
            self.assertEqual(len(row["neurons"]), w.ANALYZE_NEURONS)
            self.assertTrue(all(len(n["promotes"]) == 5 for n in row["neurons"]), result.get("notes"))
            self.assertTrue(all(1 <= len(n["contexts"]) <= w.NEURON_CONTEXTS and "«" in n["contexts"][0]["text"]
                                for n in row["neurons"]), result.get("notes"))
            self.assertEqual(sorted(row["importance_by_category"]), sorted(result["categories"]))
            self.assertEqual(sorted(row["structure"]), ["attention_output", "down", "gate", "key", "query", "up", "value"])
            self.assertTrue(0 < row["structure"]["down"]["effective_rank"] <= 1)
        # The last layer's output is what the model predicts from, so its logit lens agrees everywhere.
        self.assertAlmostEqual(result["layers"][-1]["lens"]["agreement"], 1.0)
        self.assertAlmostEqual(result["layers"][-1]["lens"]["top10"], 1.0)
        self.assertTrue(all(row["lens"]["top10"] >= row["lens"]["agreement"] for row in result["layers"]))
        self.assertEqual(sorted(result["summary"]["heads_by_role"]), sorted(w.HEAD_ROLES))
        self.assertTrue(result["findings"] and all(isinstance(line, str) for line in result["findings"]))
        self.assertFalse([note for note in result.get("notes", []) if "not read" in note or "unavailable" in note])
        self.assertEqual(result["observation_texts"],
                         sum(len(texts) for _, texts in w.CATEGORY_PROBES + w.OBSERVATION_PROBES))
        rows = result["layers"]
        # Each layer's write-out is the change between consecutive read-outs at the same positions.
        for before, after in zip(rows, rows[1:]):
            self.assertAlmostEqual(after["writes"]["target_gain"],
                                   after["lens"]["target_logprob"] - before["lens"]["target_logprob"], places=4)
        self.assertTrue(all(0 < len(row["writes"]["promotes"]) <= w.WRITE_LIST for row in rows))
        self.assertTrue(all(item["excess"] > 0 for row in rows for item in row["writes"]["promotes"]))
        self.assertTrue(all(0 < item["share"] <= 1 for row in rows for item in row["writes"]["suppresses"]))
        self.assertFalse([key for row in rows for neuron in row["neurons"] for key in ("_test", "what", "evidence")
                          if key in neuron])
        checked = result["summary"]["checked_parts"]
        self.assertTrue(checked and all(item["kind"] == "neuron" for item in checked))
        for item in checked:
            self.assertIsInstance(item["causal"]["effect"], float)
            self.assertGreaterEqual(item["causal"]["effect"], 0)
            self.assertIn(item["causal"]["confirmed"], (True, False, None))
            self.assertEqual(item["confirmed"], all(item["checks"].values()))
            self.assertEqual(set(item["checks"]) - {"causal", "category", "replicated", "explanation"}, set())
        self.assertTrue(all("hard_negative_score" in item for item in result["summary"]["explanations"]))
        self.assertEqual(model.layers, originals)

    def test_an_outside_explainer_gets_requests_and_its_answers_are_scored_like_its_own(self):
        import json
        model = self.model()
        with patch.object(w.steering, "load", return_value=(model, self.tokenizer())):
            result = json.loads(json.dumps(w.analyze({"explainer": "external"}, Path("."), {})))
        asks = result["explanation_requests"]
        self.assertTrue(asks)
        self.assertFalse(result["summary"]["explanations"])
        self.assertNotIn("explainer", result)
        for ask in asks:
            self.assertIn("a language model", ask["describe"])
            self.assertNotIn("your own network", ask["describe"])
            self.assertIn("{description}", ask["rate"])
            self.assertEqual(ask["rate"].count("«"), ask["count"] + 1)
            self.assertEqual(len(ask["test"]), ask["count"])
        first = asks[0]
        strengths = [entry["strength"] for entry in first["test"]]
        answers = [{"index": 0, "explanation": "\"Commas in lists.\"", "ratings": strengths,
                    "control_ratings": [5] * first["count"]}]
        done = w.finish_explanations({"result": result, "answers": answers, "explainer": "frontier/x",
                                      "note": "1 part not explained"}, Path("."), {})
        self.assertNotIn("explanation_requests", done)
        self.assertEqual(done["explainer"], "frontier/x")
        self.assertIn("1 part not explained", done["notes"])
        [entry] = done["summary"]["explanations"]
        self.assertEqual((entry["kind"], entry["layer"], entry["explanation"]),
                         (first["kind"], first["layer"], "Commas in lists"))
        self.assertAlmostEqual(entry["explanation_score"], 1.0)
        self.assertIsNone(entry["control_score"])
        self.assertIn("explanation", entry["checks"])
        checked = {w.part_key(item): item for item in done["summary"]["checked_parts"]}
        key = (first["kind"], first["layer"], first["unit"])
        if key in checked:
            self.assertEqual(checked[key]["explanation"], "Commas in lists")
            self.assertIn("explanation", checked[key]["checks"])
        self.assertTrue(done["findings"])
        with self.assertRaisesRegex(w.WeightsError, "2 ratings|ratings"):
            w.finish_explanations({"result": dict(result, explanation_requests=asks),
                                   "answers": [{"index": 0, "explanation": "x", "ratings": [1, 2]}]}, Path("."), {})

    def test_neuron_ablation_matches_a_zeroed_down_proj_column(self):
        mx = self.mx
        model = self.model();tokens = mx.array([[2, 5, 7, 9, 4]])
        down = model.layers[1].mlp.down_proj
        before = model(tokens)
        with w.ablated(model, "neuron", 1, 5):
            ablated = model(tokens)
        self.assertIs(model.layers[1].mlp.down_proj, down)
        self.assertTrue(mx.allclose(model(tokens), before))
        weight = down.weight
        down.weight = mx.where(mx.arange(weight.shape[1])[None, :] == 5, 0.0, weight)
        self.assertTrue(mx.allclose(ablated, model(tokens), atol=1e-5))
        down.weight = weight

    def test_ablation_at_chosen_positions_leaves_earlier_outputs_alone(self):
        import numpy as np
        mx = self.mx
        model = self.model();tokens = mx.array([[2, 5, 7, 9, 4]])
        before = model(tokens)
        everywhere = np.zeros((5, self.ARGS["intermediate_size"]), dtype=bool);everywhere[:, 5] = True
        with w.ablated(model, "neuron", 1, 5):
            whole = model(tokens)
        with w.ablated(model, "neuron", 1, everywhere):
            self.assertTrue(mx.allclose(model(tokens), whole, atol=1e-6))
        late = np.zeros_like(everywhere);late[3:, 5] = True
        with w.ablated(model, "neuron", 1, late):
            partial = model(tokens)
        self.assertTrue(mx.allclose(partial[:, :3], before[:, :3], atol=1e-6))
        self.assertFalse(mx.allclose(partial[:, 3:], before[:, 3:], atol=1e-6))

    def test_output_change_is_the_exact_kl_when_the_vocabulary_fits(self):
        import numpy as np
        mx = self.mx
        rng = np.random.default_rng(1)
        base, changed = (mx.array(rng.normal(size=(3, 8)).astype(np.float32)) for _ in range(2))
        base, changed = (value - mx.logsumexp(value, axis=-1, keepdims=True) for value in (base, changed))
        tokens = [0, 4, 2, 7]
        reference = w.output_reference(base, tokens)
        same, _ = w.output_change(base, reference)
        self.assertTrue(np.allclose(same, 0, atol=1e-6))
        kl, target = w.output_change(changed, reference)
        p, q = np.exp(np.array(base)), np.array(changed)
        self.assertTrue(np.allclose(kl, (p * (np.array(base) - q)).sum(axis=1), atol=1e-5))
        self.assertTrue(np.allclose(target, q[np.arange(3), tokens[1:]]))

    def test_causal_study_screens_then_tests_on_held_out_and_continued_text(self):
        model, tokenizer = self.model(), self.tokenizer()
        encoded = [([1] + [4 + (number + step) % 8 for step in range(12)], 1) for number in range(8)]
        labels = ["a", "b"] * 4
        pool = {("neuron", 1, unit): "a" for unit in range(4)}
        phases = []
        with patch.multiple(w, CAUSAL_ITEMS=2, CAUSAL_MIN_TEXTS=10, CAUSAL_EXTEND_TEXTS=3, CAUSAL_EXTEND_TOKENS=4):
            results, kept = w.causal_study(model, tokenizer, encoded, labels, w.text_halves(labels), pool,
                                           lambda phase, done, total: phases.append(phase))
        self.assertEqual(phases[:4], ["screening parts"] * 4)
        self.assertIn("writing more text for rarely firing parts", phases)
        self.assertEqual(len(kept), 2)
        self.assertEqual(set(results), set(kept))
        margins = {key: results[key]["screen_margin"] for key in kept}
        self.assertEqual(kept, sorted(kept, key=lambda key: -margins[key]))
        for result in results.values():
            # Held-out half only: four texts plus at most three continuations, all too few to test.
            self.assertLessEqual(result["texts"], 7)
            self.assertIsNone(result["confirmed"])
            self.assertGreaterEqual(result["effect"], 0)
            self.assertEqual(result["category"], "a")
            self.assertNotEqual(result["control_neuron"], None)

    def test_part_evidence_explains_from_one_half_and_tests_on_the_other(self):
        import numpy as np

        class Tokens:
            def decode(self, ids):
                return " ".join(f"t{i}" for i in ids)
        encoded = [([0] + [10 + number] * 3 + list(range(20, 26)), 1) for number in range(6)]
        halves = [0, 1, 0, 1, 0, 1]
        trace = [np.array([9.0] + [float(number + 1)] + [0.1 * position for position in range(8)])
                 for number in range(6)]
        contexts, evidence, test = w.part_evidence(Tokens(), encoded, ["a"] * 6, halves, trace,
                                                   np.random.default_rng(0))
        # The BOS position never counts, however strong.
        self.assertEqual([item["strength"] for item in contexts], [6.0, 5.0, 4.0])
        self.assertEqual(evidence, [w.snippet(Tokens(), encoded[number][0], 1) for number in (4, 2, 0)])
        firing = [item for item in test if item[2] == "firing"]
        self.assertEqual([strength for _, strength, _ in firing], [6.0, 4.0, 2.0])
        hard = [item for item in test if item[2] == "hard"]
        self.assertEqual(len(hard), w.EXPLAIN_TEST)
        # Hard negatives share a token with the peaks but stay below the part's median strength.
        self.assertTrue(all(text.split("«")[1].startswith("t1") and strength < 0.5 for text, strength, _ in hard))
        held_out = {w.snippet(Tokens(), encoded[number][0], position) for number in (1, 3, 5) for position in range(1, 10)}
        self.assertTrue(all(text in held_out for text, _, _ in test))

    def test_checks_confirm_only_parts_that_pass_everything(self):
        part = {"layer": 0, "neuron": 1, "replicated": True, "explanation": "commas",
                "explanation_score": 0.7, "control_score": 0.1, "hard_negative_score": 0.9}
        w.judged(part, {"confirmed": True, "category_confirmed": True})
        self.assertTrue(part["confirmed"]);self.assertIn("confirmed", w.check_note(part))
        unchecked = dict(part);w.judged(unchecked, None)
        self.assertFalse(unchecked["confirmed"]);self.assertEqual(w.check_note(unchecked), "")
        weak = dict(part, hard_negative_score=0.5);w.judged(weak, {"confirmed": True})
        self.assertFalse(weak["confirmed"])
        self.assertEqual(weak["checks"], {"causal": True, "replicated": True, "explanation": False})
        self.assertIn(w.CHECK_FAILURES["explanation"], w.check_note(weak))
        # A neutral description that anti-correlates does not lend the explanation a margin.
        self.assertFalse(w.explanation_trusted(dict(part, explanation_score=0.2, control_score=-0.6)))
        untested = dict(part);w.judged(untested, {"confirmed": None, "category_confirmed": True})
        self.assertFalse(untested["confirmed"])
        self.assertIn(w.CHECK_UNTESTED["causal"], w.check_note(untested))

    def test_rank_tests_known_answers(self):
        # All six differences positive: W = 21, mean 10.5, sd sqrt(6*7*13/24).
        self.assertAlmostEqual(w.signed_rank_z([1, 2, 3, 4, 5, 6]), 10.5 / math.sqrt(22.75))
        self.assertAlmostEqual(w.signed_rank_z([-1, -2, 3, 4, 5, 0]), (12 - 7.5) / math.sqrt(13.75))
        self.assertIsNone(w.signed_rank_z([0, 0, 1]))
        # One huge value moves a mean but not the ranks.
        self.assertAlmostEqual(w.signed_rank_z([1, 2, 3, 4, 5, 600]), w.signed_rank_z([1, 2, 3, 4, 5, 6]))
        self.assertAlmostEqual(w.rank_sum_z([4, 5, 6], [1, 2, 3]), 4.5 / math.sqrt(5.25))
        self.assertAlmostEqual(w.rank_sum_z([1, 2, 3], [4, 5, 6]), -4.5 / math.sqrt(5.25))
        self.assertIsNone(w.rank_sum_z([1], [2, 3]))

    def test_controls_are_matched_parts_at_the_same_positions(self):
        import numpy as np
        probs = np.array([[.9, .05, .03, .02], [.1, .5, .3, .1], [.05, .35, .32, .28], [.4, .1, .3, .2]])
        fires, control = w.expert_fires(probs, 1, 2, start=1)
        # Position 0 is before `start`; at 3 expert 1 is not chosen. Its co-chosen expert is the control.
        self.assertEqual(fires.tolist(), [False, True, True, False])
        self.assertEqual(control.tolist(), [-1, 2, 2, -1])
        mask = w.removal(fires, control, 4)
        self.assertEqual(np.argwhere(mask).tolist(), [[1, 2], [2, 2]])
        self.assertEqual(np.argwhere(w.removal(fires, 1, 4)).tolist(), [[1, 1], [2, 1]])
        # Neuron 0 fires at its strongest tenth; neuron 2 is closest to it in mean |activation| there.
        reading = [np.array([[99., 0, 0], [8, 1, 7], [1, 1, 1]] + [[1, 1, 1]] * 7)]
        plan = w.part_plan(("neuron", 0, 0), reading, None, [1])
        self.assertEqual(np.flatnonzero(plan["fires"][0]).tolist(), [1])
        self.assertEqual((plan["control"], plan["cut"]), ([2], 8.0))
        more = w.part_plan(("neuron", 0, 0), [np.array([[5., 0, 0], [9, 0, 0], [2, 0, 0]])], None, [1], base=plan)
        self.assertEqual(more["fires"][0].tolist(), [False, True, False])

    def test_firing_masks_take_exactly_the_strongest_share_despite_ties(self):
        import numpy as np
        trace = [np.array([99.0, 1, 1, 1, 1, 5]), np.array([99.0, 1, 1, 1, 1, 1])]
        masks = w.firing_masks(trace, 0.2)
        self.assertEqual([mask.tolist() for mask in masks],
                         [[False, True, False, False, False, True], [False] * 6])

    def test_matrix_structure_measures_rank_and_outliers(self):
        mx = self.mx
        identity = w.matrix_structure(mx.eye(8))
        self.assertAlmostEqual(identity["effective_rank"], 1.0, places=5)
        self.assertEqual((identity["outlier_rows"], identity["outlier_columns"]), ([], []))
        rank_one = mx.ones((6, 1)) @ mx.ones((1, 8))
        self.assertAlmostEqual(w.matrix_structure(rank_one)["effective_rank"], 1 / 6, places=3)
        self.assertAlmostEqual(w.matrix_structure(rank_one)["top_energy"], 1.0, places=5)
        spiked = mx.ones((4, 10)) * mx.array([1.0] * 9 + [50.0])
        self.assertEqual(w.matrix_structure(spiked)["outlier_columns"], [9])

    def test_head_roles_and_specialty_follow_thresholds(self):
        self.assertEqual(w.head_role(0.1, 0.1, None, 0.9), "current-token")
        self.assertEqual(w.head_role(0.1, 0.1, 0.1, 0.1, 0.4), "same-token")
        self.assertEqual(w.head_role(0.1, 0.1, None, 0.1, None, 0.8, 0.1), "local")
        self.assertEqual(w.head_role(0.1, 0.1, None, 0.1, None, 0.2, 0.6), "long-range")
        self.assertEqual(w.head_role(0.6, 0.9, 0.5, 0.9), "induction")
        self.assertEqual(w.head_role(0.1, 0.1, None), "mixed")
        self.assertEqual(w.specialty({"code": 0.5, "facts": 0.1, "story": 0.1}), "code")
        self.assertIsNone(w.specialty({"code": 0.15, "facts": 0.1, "story": 0.1}))
        self.assertIsNone(w.specialty({"code": 0.01, "facts": -0.1, "story": -0.1}))
        spread = {"previous": 0.1, "sink": 0.36, "induction": 0.05, "self": 0.13, "duplicate": 0.04,
                  "local": 0.27, "far": 0.19}
        self.assertEqual(w.head_role(spread["previous"], spread["sink"], spread["induction"], spread["self"],
                                     spread["duplicate"], spread["local"], spread["far"]), "mixed")
        self.assertEqual(w.head_leaning(spread), "attention-sink")
        self.assertEqual(w.head_leaning(dict(spread, far=0.4)), "long-range")

    def test_explanations_are_cleaned_to_one_phrase(self):
        self.assertEqual(w.clean_explanation("<think>hmm</think>\n\n\"Detects French words.\"\nMore text"),
                         "Detects French words")
        self.assertEqual(w.clean_explanation(""), "")

    def test_linear_memory_half_life_follows_the_decay_gate(self):
        import numpy as np
        from mlx import nn
        mx = self.mx

        class Block:
            in_proj_a, in_proj_b = nn.Linear(4, 2, bias=False), nn.Linear(4, 2, bias=False)
            A_log, dt_bias = mx.zeros(2), mx.array([0.0, -6.0])
        Block.in_proj_a.weight = mx.zeros((2, 4));Block.in_proj_b.weight = mx.zeros((2, 4))
        read = w.linear_memory(Block, mx.ones((1, 5, 4)))
        row = w.memory_row((np.array(read[0]), np.array(read[1]), read[2]))
        # With a = 0 and A_log = 0 every token multiplies the state by exp(-softplus(dt_bias)).
        expected = [math.log(0.5) / -math.log1p(math.exp(bias)) for bias in (0.0, -6.0)]
        self.assertEqual(read[2], 5)
        for got, want in zip(row["half_life"], expected):
            self.assertAlmostEqual(got, want, places=2)
        self.assertAlmostEqual(row["write_strength"], 0.5)
        self.assertEqual((row["long_heads"], row["short_heads"]), (1, 1))
        self.assertIsNone(w.linear_memory(object(), mx.ones((1, 5, 4))))

    def test_probe_text_gets_the_bos_token_once(self):
        class Tokens:
            bos_token_id = 7

            def __init__(self, first):
                self.first = first

            def encode(self, text):
                return self.first + [3, 4]
        for first in ([], [7]):
            encoded, labels = w.category_encoded(Tokens(first))
            self.assertEqual(encoded[0], ([7, 3, 4], 1))
            self.assertEqual(len(encoded), len(labels))

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

    def test_span_direction_contrasts_within_answers_and_records_writer_statistics(self):
        mx = self.mx
        from dial_worker import encode, reasoning_renderer
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp) / "base";model = self.model();self.save(root, model, self.ARGS)
            positive, _ = self.direction_inputs()
            output = Path(tmp) / "within.safetensors"
            tokenizer = self.tokenizer()
            with patch.object(w.steering, "load", return_value=(model, tokenizer)):
                result = w.direction({"positive": positive, "span": {"last": 2}, "output": str(output)}, root, {})
                with self.assertRaisesRegex(w.WeightsError, "end-of-thinking"):
                    w.direction({"positive": positive, "span": {"last": 2, "before": "think_end"},
                                 "output": str(Path(tmp) / "t.safetensors")}, root, {})
                with self.assertRaisesRegex(w.WeightsError, "or a `span`"):
                    w.direction({"positive": positive, "output": str(Path(tmp) / "n.safetensors")}, root, {})
            self.assertEqual((result["contrast"], result["negative"], result["writers"]), ("within", None, ["dense"]))
            saved, metadata = mx.load(str(output), return_metadata=True)
            self.assertEqual((metadata["contrast"], json.loads(metadata["span"])), ("within", {"last": 2, "before": "answer_end"}))
            self.assertEqual(saved["inputs.1.dense"].shape, (1, 128))
            # Independently: the mean input to layer 1's down projection over every answer token.
            seen = []
            mlp = model.layers[1].mlp
            inner = mlp.down_proj
            mlp.down_proj = w.steering.wrap(inner, lambda out: out, lambda x: seen.append(x))
            render, total, count, norm = reasoning_renderer(tokenizer), mx.zeros((128,)), 0, 0.0
            for item in positive:
                tokens, offset = encode(tokenizer, render(item["messages"]))
                model(mx.array([tokens], dtype=mx.int32))
                x = seen[-1][0, offset:].astype(mx.float32)
                total = total + mx.sum(x, axis=0);count += x.shape[0]
                norm += float(mx.sum(mx.sqrt(mx.sum(inner(x) ** 2, axis=-1))).item())
            mlp.down_proj = inner
            self.assertTrue(mx.allclose(saved["inputs.1.dense"][0], total / count, atol=1e-4))
            self.assertAlmostEqual(float(saved["norms.1.dense"][0].item()), norm / count, places=3)
            self.assertEqual(float(saved["counts.1.dense"][0].item()), count)

    def test_add_edit_writes_alpha_n_along_d_and_its_control_matches_the_norm_orthogonally(self):
        mx = self.mx
        name = "model.layers.1.mlp.down_proj.weight"
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp) / "base";model = self.model();self.save(root, model, self.ARGS)
            _, direction = self.measured(root, model)
            loaded = mx.load(str(direction))
            d, u, n = loaded["directions"][1], loaded["inputs.1.dense"][0], loaded["norms.1.dense"][0]
            request = {"op": "weights_edit", "model": str(root), "direction": str(direction), "layers": [1],
                       "method": "add", "alpha": 0.1}
            result = w.operation(dict(request, output=str(Path(tmp) / "add")))
            control = w.operation(dict(request, output=str(Path(tmp) / "control"), control_seed=7))
            base = mx.load(str(root / "model.safetensors"))[name].astype(mx.float32)
            delta = mx.load(str(Path(tmp) / "add" / "model.safetensors"))[name].astype(mx.float32) - base
            other = mx.load(str(Path(tmp) / "control" / "model.safetensors"))[name].astype(mx.float32) - base
            self.assertTrue(mx.allclose(delta @ u, 0.1 * n * d, atol=2e-2 * float(n.item())))
            self.assertEqual((result["method"], result["layers"][0]["writer"], control["control_seed"]), ("add", "dense", 7))
            self.assertAlmostEqual(result["layers"][0]["edit_norm"], control["layers"][0]["edit_norm"], places=4)
            self.assertLess(float(mx.max(mx.abs(d @ other)).item()), 1e-3 * float(mx.max(mx.abs(other)).item()) * 64)
            record = json.loads((Path(tmp) / "control" / w.EDIT_RECORD).read_text())
            self.assertEqual((record["control_seed"], record["request"]["method"], record["request"]["layers"]), (7, "add", [1]))
            # project_out's control rewrites the same columns with an orthogonal edit of the same norm.
            project = {"op": "weights_edit", "model": str(root), "direction": str(direction), "layers": [1], "columns": 8}
            real = w.operation(dict(project, output=str(Path(tmp) / "p")))["layers"][0]
            fake = w.operation(dict(project, output=str(Path(tmp) / "pc"), control_seed=3))["layers"][0]
            self.assertEqual(real["columns"], fake["columns"])
            self.assertAlmostEqual(real["edit_norm"], fake["edit_norm"], places=4)
            self.assertAlmostEqual(fake["projection_after"], fake["projection_before"], places=3)
            stale = Path(tmp) / "old.safetensors"
            _, metadata = mx.load(str(direction), return_metadata=True)
            mx.save_safetensors(str(stale), {"directions": loaded["directions"]}, metadata=metadata)
            with self.assertRaisesRegex(w.WeightsError, "measure it again"):
                w.operation(dict(request, direction=str(stale), output=str(Path(tmp) / "stale")))
            with self.assertRaisesRegex(w.WeightsError, "control_seed applies only"):
                w.operation({"op": "weights_edit", "model": str(root), "method": "scale", "control_seed": 1,
                             "output": str(Path(tmp) / "s"), "changes": [{"tensor": name, "scale": 0.5}]})

    def test_thinking_tokens_count_up_to_the_end_marker(self):
        import types
        tokenizer = types.SimpleNamespace(think_start_tokens=(7,), think_end_tokens=(8, 9))
        self.assertEqual(w.steering.thinking_tokens(tokenizer, [1, 7], [4, 5, 8, 9, 6]), 2)
        self.assertEqual(w.steering.thinking_tokens(tokenizer, [1, 7], [4, 5, 6]), 3)
        self.assertEqual(w.steering.thinking_tokens(tokenizer, [1, 2], [4, 5, 6]), 0)
        self.assertIsNone(w.steering.thinking_tokens(types.SimpleNamespace(), [1], [2]))


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
        for index in (0, 1, 2):
            memory = rows[index]["linear_attention"]["memory"]
            self.assertEqual(len(memory["half_life"]), 2)
            self.assertTrue(all(value > 0 for value in memory["half_life"]))
            self.assertTrue(0 < memory["write_strength"] < 1)
        self.assertEqual(result["summary"]["memory"]["heads"], 6)
        profiles = [profile for index in range(4) for profile in rows[index]["experts"].get("profiles", [])]
        self.assertTrue(profiles)
        for profile in profiles:
            self.assertTrue(0 <= profile["expert"] < 8 and 0 < profile["share"] <= 1)
            self.assertEqual(len(profile["promotes"]), w.EXPLAIN_TOKENS)
            self.assertTrue(profile["contexts"] and all("«" in item["text"] for item in profile["contexts"]))
            self.assertFalse({"_test", "what", "evidence"} & set(profile))
        explained = result["summary"]["explanations"]
        self.assertTrue(explained)
        self.assertTrue(all(item[key] is None or -1 <= item[key] <= 1
                            for item in explained for key in ("explanation_score", "control_score")))
        checked = result["summary"]["checked_parts"]
        self.assertTrue(checked and all(item["kind"] == "expert" for item in checked))
        self.assertTrue(all(isinstance(item["causal"]["confirmed"], bool) for item in checked))
        self.assertTrue(all(len(item["half_lifts"]) == 2 for row in rows.values()
                            for item in row["experts"]["specialists"]))
        # Linear-attention and MoE layers are expected notes; a failed routing or head read would be a note too.
        self.assertFalse([note for note in result.get("notes", []) if "not read" in note or "not classified:" in note])

    def test_expert_ablation_reroutes_its_tokens(self):
        mx = self.mx
        model = self.model();mlp = w.steering.layer_list(model)[1].mlp
        switch, gate = mlp.switch_mlp, mlp.gate
        chosen = []
        mlp.switch_mlp = w.steering.wrap(switch, lambda out: out, lambda x, indices: chosen.append(indices))
        tokens = mx.array([[2, 5, 7, 9, 4, 6, 8, 10]])
        model(tokens)
        busiest = int(chosen[-1].reshape(-1)[0].item())
        with w.ablated(model, "expert", 1, busiest):
            model(tokens)
        self.assertNotIn(busiest, chosen[-1].reshape(-1).tolist())
        self.assertIs(mlp.gate, gate)
        mlp.switch_mlp = switch

    def test_direction_records_routed_and_shared_writers_and_add_edits_each_expert(self):
        mx = self.mx
        from mlx.utils import tree_flatten
        experts = "model.layers.1.mlp.switch_mlp.down_proj.weight"
        shared = "model.layers.1.mlp.shared_expert.down_proj.weight"
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp) / "base";root.mkdir()
            (root / "config.json").write_text(json.dumps(self.ARGS))
            mx.save_safetensors(str(root / "model.safetensors"), dict(tree_flatten(self.language().parameters())))
            positive, negative = Metal.direction_inputs(self)
            output = Path(tmp) / "d.safetensors"
            with patch.object(w.steering, "load", return_value=(self.model(), self.tokenizer())):
                result = w.direction({"positive": positive, "negative": negative, "output": str(output)}, root, {})
            self.assertEqual(result["writers"], ["experts", "shared"])
            saved = mx.load(str(output))
            counts = saved["counts.1.experts"]
            # Two experts per token: the routed counts are twice the shared expert's positions.
            self.assertEqual(float(mx.sum(counts).item()), 2 * float(saved["counts.1.shared"][0].item()))
            self.assertEqual(saved["inputs.1.experts"].shape, (8, 32))
            request = {"op": "weights_edit", "model": str(root), "direction": str(output), "layers": [1]}
            added = w.operation(dict(request, method="add", output=str(Path(tmp) / "add")))
            projected = w.operation(dict(request, columns=4, output=str(Path(tmp) / "proj")))
            self.assertEqual(sorted(row["writer"] for row in added["layers"]), ["experts", "shared"])
            base, edited = mx.load(str(root / "model.safetensors")), mx.load(str(Path(tmp) / "add" / "model.safetensors"))
            changed = mx.any(mx.abs(edited[experts] - base[experts]) > 0, axis=(1, 2)).tolist()
            self.assertEqual(changed, [count > 0 for count in counts.tolist()])
            d = saved["directions"][1]
            for index, used in enumerate(counts.tolist()):
                if used:
                    delta = (edited[experts][index] - base[experts][index]).astype(mx.float32)
                    u, n = saved["inputs.1.experts"][index], saved["norms.1.experts"][index]
                    self.assertTrue(mx.allclose(delta @ u, 0.1 * n * d, atol=0.05 * float(n.item())))
            self.assertFalse(mx.array_equal(edited[shared], base[shared]))
            stack = next(row for row in projected["layers"] if row["writer"] == "experts")
            self.assertEqual((stack["experts"], stack["columns_per_expert"], stack["edited_params"]), (8, 4, 8 * 64 * 4))
            self.assertLess(stack["projection_after"], stack["projection_before"])

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


class Gemma4Moe(unittest.TestCase):
    """A tiny Gemma 4 text model: decoder layers return `(h, shared_kv, offset)`, sliding and full
    attention alternate, and a separate router and expert block sit beside the dense MLP."""
    @classmethod
    def setUpClass(cls):
        try:
            import mlx.core as mx
            from mlx_lm.models.gemma4_text import Model, ModelArgs
        except ImportError as error:
            raise unittest.SkipTest(str(error))
        cls.mx, cls.Model, cls.ModelArgs = mx, Model, ModelArgs

    ARGS = dict(model_type="gemma4_text", hidden_size=64, num_hidden_layers=5, intermediate_size=128,
                num_attention_heads=4, head_dim=16, global_head_dim=32, num_key_value_heads=2,
                num_global_key_value_heads=1, num_kv_shared_layers=0, hidden_size_per_layer_input=0,
                vocab_size=64, vocab_size_per_layer_input=64, enable_moe_block=True, num_experts=8,
                top_k_experts=2, moe_intermediate_size=32, attention_k_eq_v=True, sliding_window=512)

    def model(self):
        self.mx.random.seed(0)
        return self.Model(self.ModelArgs(**self.ARGS))

    def test_analyze_reads_gemma4_layers_routing_and_attention_spans(self):
        model = self.model();originals = list(model.layers)
        with patch.object(w.steering, "load", return_value=(model, Metal.tokenizer(self))):
            result = w.analyze({}, Path("."), {})
        rows = result["layers"]
        self.assertEqual([row["attention_type"] for row in rows], ["sliding"] * 4 + ["full"])
        for row in rows:
            self.assertTrue(math.isfinite(row["importance"]))
            self.assertEqual(sum(row["head_roles"].values()), 4)
            experts = row["experts"]
            self.assertEqual((experts["count"], experts["top_k"]), (8, 2))
            self.assertTrue(0 <= experts["entropy"] <= 1)
            self.assertAlmostEqual(sum(item["share"] for item in experts["top"]), 1.0, places=5)
            self.assertIsNone(experts["shared_gate"])
        self.assertEqual(result["summary"]["routing"]["moe_layers"], 5)
        self.assertEqual(sorted(result["summary"]["attention_types"]), ["full", "sliding"])
        self.assertAlmostEqual(rows[-1]["lens"]["agreement"], 1.0)
        self.assertFalse([note for note in result.get("notes", []) if "not read" in note or "not classified:" in note
                          or "unavailable" in note])
        self.assertEqual(model.layers, originals)
        checked = result["summary"]["checked_parts"]
        self.assertTrue(checked and {item["kind"] for item in checked} <= {"expert", "neuron"})
        self.assertTrue(all(isinstance(item["causal"]["z"], float) for item in checked))

    def test_direction_records_the_dense_mlp_and_the_expert_block_as_separate_writers(self):
        mx = self.mx
        from mlx.utils import tree_flatten
        with tempfile.TemporaryDirectory() as tmp:
            model = self.model();root = Path(tmp) / "base";root.mkdir()
            (root / "config.json").write_text(json.dumps(self.ARGS))
            mx.save_safetensors(str(root / "model.safetensors"), dict(tree_flatten(model.parameters())))
            positive, negative = Metal.direction_inputs(self)
            output = Path(tmp) / "d.safetensors"
            with patch.object(w.steering, "load", return_value=(model, Metal.tokenizer(self))):
                result = w.direction({"positive": positive, "negative": negative, "output": str(output)}, root, {})
            self.assertEqual(result["writers"], ["dense", "experts"])
            saved = mx.load(str(output))
            self.assertEqual(float(mx.sum(saved["counts.2.experts"]).item()), 2 * float(saved["counts.2.dense"][0].item()))
            added = w.operation({"op": "weights_edit", "model": str(root), "direction": str(output), "layers": [2],
                                 "method": "add", "output": str(Path(tmp) / "add")})
            self.assertEqual(sorted(row["writer"] for row in added["layers"]), ["dense", "experts"])

    def test_expert_ablation_reroutes_its_tokens(self):
        mx = self.mx
        model = self.model();layer = model.layers[2]
        experts, proj = layer.experts, layer.router.proj
        chosen = []
        layer.experts = w.steering.wrap(experts, lambda out: out, lambda x, indices, weights: chosen.append(indices))
        tokens = mx.array([[2, 5, 7, 9, 4, 6, 8, 10]])
        model(tokens)
        busiest = int(chosen[-1].reshape(-1)[0].item())
        with w.ablated(model, "expert", 2, busiest):
            model(tokens)
        self.assertNotIn(busiest, chosen[-1].reshape(-1).tolist())
        self.assertIs(layer.router.proj, proj)
        layer.experts = experts


if __name__ == "__main__":
    unittest.main()
