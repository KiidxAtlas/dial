"""Reasoning targets, exact adapter combination, likelihood scoring and request contracts."""
import json
import importlib.util
import math
import os
import tempfile
import unittest
from pathlib import Path
import dial_steering as s
from dial_worker import (RequestError, combine_request, encode, messages, reasoning_renderer, score_request)


def call(name="read", path="a"):
    return [{"id": "c1", "type": "function", "function": {"name": name, "arguments": {"path": path}}}]


class ReasoningValidation(unittest.TestCase):
    def test_assistant_reasoning_is_accepted_only_as_nonempty_training_text(self):
        user = {"role": "user", "content": "read a"}
        for answer in ({"role": "assistant", "content": "done", "reasoning_content": "think"},
                       {"role": "assistant", "content": None, "tool_calls": call(), "reasoning_content": "think"}):
            self.assertEqual(messages([user, answer], training=True)[-1], answer)
        for reasoning in ("", "  ", None, 3):
            with self.subTest(reasoning=reasoning), self.assertRaises(RequestError):
                messages([user, {"role": "assistant", "content": "done", "reasoning_content": reasoning}], training=True)
        with self.assertRaises(RequestError):
            messages([dict(user, reasoning_content="x"), {"role": "assistant", "content": "done"}], training=True)
        with self.assertRaises(RequestError):
            messages([user, {"role": "assistant", "content": "done", "reasoning_content": "think"}])


class Template:
    """Renders role/content; optionally the reasoning field; optionally exposes thinking markers."""

    def __init__(self, renders_reasoning, markers):
        self.renders_reasoning = renders_reasoning
        self.think_start, self.think_end = ("<think>", "</think>") if markers else (None, None)
        self.calls = 0

    def apply_chat_template(self, chat, tokenize=False, **kwargs):
        self.calls += 1
        return "".join(f"[{m['role']}]" + (m.get("reasoning_content", "") if self.renders_reasoning else "")
                       + (m.get("content") or "") for m in chat)


class ReasoningRendering(unittest.TestCase):
    chat = [{"role": "user", "content": "q"}, {"role": "assistant", "content": "a", "reasoning_content": "r"},
            {"role": "user", "content": "q2"}, {"role": "assistant", "content": None, "tool_calls": call(),
                                                "reasoning_content": "plan"}]

    def test_native_template_keeps_reasoning(self):
        self.assertEqual(reasoning_renderer(Template(True, True))(self.chat), self.chat)

    def test_thinking_tokenizer_inlines_and_plain_tokenizer_drops(self):
        inlined = reasoning_renderer(Template(False, True))(self.chat)
        self.assertEqual(inlined[1], {"role": "assistant", "content": "<think>\nr\n</think>\n\na"})
        self.assertEqual(inlined[3]["content"], "<think>\nplan\n</think>\n\n")
        self.assertEqual(inlined[3]["tool_calls"], call())
        dropped = reasoning_renderer(Template(False, False))(self.chat)
        self.assertEqual(dropped[1], {"role": "assistant", "content": "a"})
        self.assertEqual(dropped[3], {"role": "assistant", "content": None, "tool_calls": call()})

    def test_reasoning_free_data_never_probes_the_template(self):
        template = Template(False, False)
        chat = [{"role": "user", "content": "q"}, {"role": "assistant", "content": "a"}]
        self.assertEqual(reasoning_renderer(template)(chat), chat)
        self.assertEqual(template.calls, 0)

    def test_target_boundary_is_the_common_prefix_with_the_generation_prompt(self):
        class Tokens:
            def apply_chat_template(self, chat, tools=None, add_generation_prompt=False, tokenize=True):
                # A thinking prompt opens "<think>\n" (9); the finished turn renders "<think>\n\n" (8).
                return [1, 2, 3, 9] if add_generation_prompt else [1, 2, 3, 8, 5, 6]
        self.assertEqual(encode(Tokens(), [{"role": "user", "content": "q"}, {"role": "assistant", "content": "a"}]),
                         ([1, 2, 3, 8, 5, 6], 3))


def thinking_tokenizer():
    candidates = [os.environ.get("DIAL_THINKING_TOKENIZER"),
                  "~/.lmstudio/models/lmstudio-community/Qwen3.5-9B-MLX-8bit",
                  "~/.lmstudio/models/incoai/Qwen3.6-35B-A3B-Splash/tokenizer"]
    for candidate in filter(None, candidates):
        path = Path(candidate).expanduser()
        if (path / "tokenizer_config.json").is_file():
            return path
    return None


@unittest.skipUnless(thinking_tokenizer() and importlib.util.find_spec("mlx_lm"), "optional local Qwen thinking tokenizer and MLX runtime")
class RealThinkingTemplate(unittest.TestCase):
    def test_target_mask_covers_reasoning_and_tool_call_but_not_prompt(self):
        from mlx_lm.tokenizer_utils import load
        tokenizer = load(thinking_tokenizer(), {"trust_remote_code": False})
        tools = [{"type": "function", "function": {"name": "read", "parameters": {"type": "object"}}}]
        chat = [{"role": "user", "content": "Show src/lib.rs"},
                {"role": "assistant", "content": None, "tool_calls": call("read", "src/lib.rs"),
                 "reasoning_content": "Opening the library root first."}]
        rendered = reasoning_renderer(tokenizer)(messages(chat, training=True))
        self.assertEqual(rendered, chat)
        tokens, offset = encode(tokenizer, rendered, tools)
        target, prompt = tokenizer.decode(tokens[offset:]), tokenizer.decode(tokens[:offset])
        self.assertIn("Opening the library root first.", target)
        self.assertIn("src/lib.rs", target)
        self.assertIn("</think>", target)
        self.assertNotIn("Opening the library", prompt)
        self.assertTrue(prompt.endswith("<think>\n"))


class Requests(unittest.TestCase):
    def score(self, **overrides):
        request = {"op": "score", "model": "m", "adapter": None, "steering": None, "data": "d.jsonl",
                   "max_examples": 8, "max_seq_length": 512}
        request.update(overrides)
        return score_request(request)

    def test_score_bounds_and_steering_shape(self):
        self.assertEqual(self.score(), ("m", None, None, "d.jsonl", 8, 512))
        steering = {"bundle": "v.json", "strength": 25.5}
        self.assertEqual(self.score(steering=steering)[2], steering)
        for key, value in [("max_examples", 0), ("max_examples", 257), ("max_seq_length", 31), ("data", ""),
                           ("adapter", " "), ("steering", {"bundle": "v.json"}),
                           ("steering", {"bundle": "v.json", "strength": 0}),
                           ("steering", {"bundle": "v.json", "strength": float("nan")}),
                           ("steering", {"bundle": "v.json", "strength": 1, "layer": 3})]:
            with self.subTest(key=key, value=value), self.assertRaises(RequestError):
                self.score(**{key: value})

    def test_combine_requires_bounded_finite_weighted_inputs(self):
        self.assertEqual(combine_request({"model": "m", "inputs": [{"adapter": "a", "weight": -1}], "output": "o"}),
                         ("m", [("a", -1.0)], "o"))
        for inputs in ([], [{"adapter": "a"}], [{"adapter": "a", "weight": float("inf")}],
                       [{"adapter": "a", "weight": 101}], [{"adapter": "", "weight": 1}],
                       [{"adapter": "a", "weight": True}], [{"adapter": "a", "weight": 1, "scale": 2}],
                       [{"adapter": "a", "weight": 1}] * 17):
            with self.subTest(inputs=inputs), self.assertRaises(RequestError):
                combine_request({"model": "m", "inputs": inputs, "output": "o"})

    def test_bake_requires_finite_bounded_strength_and_ridge_and_fresh_output(self):
        with tempfile.TemporaryDirectory() as tmp:
            request = {"bundle": "v.json", "data": "d.jsonl", "output": str(Path(tmp) / "out"), "adapter": None,
                       "strength": 25.5, "ridge": 1e-3}
            self.assertEqual(s.bake_request(request)[1:4:2], (25.5, 1e-3))
            for key, value in [("strength", 0), ("strength", s.MAX_STRENGTH * 2), ("ridge", 0), ("ridge", 1e4),
                               ("ridge", float("nan")), ("output", tmp), ("data", ""), ("adapter", "")]:
                with self.subTest(key=key, value=value), self.assertRaises(ValueError):
                    s.bake_request(dict(request, **{key: value}))

    def test_absolute_sweep_strength_is_admissible_up_to_the_cap(self):
        from test_steering import bundle
        item = bundle()
        s.validate(item, 25.5)
        s.validate(item, s.MAX_STRENGTH)
        with self.assertRaises(ValueError):
            s.validate(item, s.MAX_STRENGTH * 1.01)

    def test_sweep_rejects_a_fixed_layer_and_non_boolean_flag(self):
        pairs = [{"positive": [{"role": "user", "content": f"task {n}"}, {"role": "assistant", "content": "3"}],
                  "negative": [{"role": "user", "content": f"task {n}"}, {"role": "assistant", "content": "4"}]}
                 for n in range(8)]
        with tempfile.TemporaryDirectory() as tmp:
            (Path(tmp) / "config.json").write_text("{}")
            request = dict(model=tmp, model_name="test", adapter=None, pairs=pairs, output=str(Path(tmp) / "v.json"),
                           progress=str(Path(tmp) / "p.json"))
            with self.assertRaisesRegex(ValueError, "layer must be null"):
                s.extract(dict(request, layer=3, sweep=True))
            with self.assertRaisesRegex(ValueError, "boolean"):
                s.extract(dict(request, layer=None, sweep=1))


try:
    import mlx.core as mx
    import mlx.nn as nn
except ImportError:
    mx = None


@unittest.skipIf(mx is None, "MLX/Metal runtime required")
class AdapterAlgebra(unittest.TestCase):
    class Block(nn.Module if mx else object):
        def __init__(self):
            super().__init__()
            self.q_proj = nn.Linear(16, 16, bias=False)
            self.o_proj = nn.Linear(16, 16, bias=False)

        def __call__(self, x):
            return x + self.o_proj(self.q_proj(x))

    class Model(nn.Module if mx else object):
        def __init__(self):
            super().__init__()
            self.layers = [AdapterAlgebra.Block() for _ in range(3)]

        def __call__(self, x):
            for layer in self.layers:
                x = layer(x)
            return x

    def model(self):
        from mlx.utils import tree_flatten
        mx.random.seed(7)
        model = self.Model()
        if not hasattr(self, "weights"):
            self.weights = dict(tree_flatten(model.parameters()))
        model.load_weights(list(self.weights.items()))
        return model

    def adapter(self, root, name, count, keys, rank, scale):
        from dial_worker import expected_weights, query_targets
        model = self.model()
        _, ids = query_targets(model, count, keys)
        tensors = {key: mx.random.normal(shape) * 0.1 for key, shape in expected_weights(model, ids, rank).items()}
        path = root / name
        path.mkdir()
        (path / "adapter_config.json").write_text(json.dumps({"model": "test", "fine_tune_type": "lora", "num_layers": count,
            "lora_parameters": {"rank": rank, "dropout": 0.0, "scale": scale, "keys": keys}}))
        mx.save_safetensors(str(path / "adapters.safetensors"), tensors)
        return path

    def deltas(self, adapter, x):
        """Each module's output change caused by an adapter, keyed by module path.

        Computed on the CPU stream: Metal float32 matmuls carry ~1e-3 relative error.
        """
        from mlx_lm.tuner.utils import load_adapters
        base, model = self.model(), self.model()
        if adapter is not None:
            load_adapters(model, str(adapter))
        result = {}
        with mx.stream(mx.cpu):
            for index in range(3):
                for key in ("q_proj", "o_proj"):
                    result[f"{index}.{key}"] = getattr(model.layers[index], key)(x) - getattr(base.layers[index], key)(x)
        return result

    def test_combined_adapter_output_is_the_weighted_sum_of_deltas(self):
        from dial_worker import adapter_layout, combine_adapters
        x = mx.random.normal((5, 16))
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            first = self.adapter(root, "first", 2, ["q_proj"], 2, 20.0)
            second = self.adapter(root, "second", 3, ["o_proj", "q_proj"], 3, 1.0)
            result = combine_adapters(self.model(), "test", [(str(first), 0.7), (str(second), -0.4)], root / "soup")
            self.assertEqual((result["rank"], result["keys"], result["num_layers"]), (5, ["o_proj", "q_proj"], 3))
            adapter_layout(self.model(), "test", root / "soup")
            one, two, soup = self.deltas(first, x), self.deltas(second, x), self.deltas(root / "soup", x)
            for name in soup:
                expected = 0.7 * one[name] - 0.4 * two[name]
                self.assertLess(float(mx.max(mx.abs(soup[name] - expected)).item()), 1e-4, name)
            self.assertGreater(float(mx.max(mx.abs(one["2.q_proj"])).item()), 1e-3)
            with self.assertRaises(RequestError):
                combine_adapters(self.model(), "test", [(str(first), 1.0)], root / "soup")

    def test_resume_continues_a_combined_adapter_with_an_identical_function(self):
        from dial_worker import combine_adapters, resume_layout
        x = mx.random.normal((5, 16))
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            first = self.adapter(root, "first", 2, ["q_proj"], 2, 20.0)
            second = self.adapter(root, "second", 1, ["o_proj"], 1, 1.0)
            combine_adapters(self.model(), "test", [(str(first), 1.0), (str(second), 1.0)], root / "soup")
            count, parameters, expected, weights = resume_layout(self.model(), "test", root / "soup", root)
            self.assertEqual((count, parameters), (2, {"rank": 3, "dropout": 0.0, "scale": 20.0,
                                                       "keys": ["o_proj", "q_proj"]}))
            resumed = root / "resumed"
            resumed.mkdir()
            (resumed / "adapter_config.json").write_text(json.dumps({"num_layers": count, "lora_parameters": parameters}))
            (resumed / "adapters.safetensors").write_bytes(Path(weights).read_bytes())
            before, after = self.deltas(root / "soup", x), self.deltas(resumed, x)
            for name in before:
                self.assertLess(float(mx.max(mx.abs(before[name] - after[name])).item()), 1e-5, name)
            with self.assertRaises(RequestError):
                resume_layout(self.model(), "other-model", root / "soup", root)

    def test_layout_rejects_keys_outside_the_model_and_nonzero_dropout(self):
        from dial_worker import adapter_layout
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            path = self.adapter(root, "first", 2, ["q_proj"], 2, 20.0)
            config = json.loads((path / "adapter_config.json").read_text())
            for change in ({"keys": ["k_proj"]}, {"dropout": 0.1}, {"scale": float("inf")}, {"keys": ["q_proj", "o_proj"]}):
                with self.subTest(change=change):
                    (path / "adapter_config.json").write_text(json.dumps(
                        dict(config, lora_parameters=dict(config["lora_parameters"], **change))))
                    with self.assertRaises((RequestError, ValueError)):
                        adapter_layout(self.model(), "test", path)

    def test_batched_scores_match_single_sequence_scores(self):
        from mlx_lm.models.llama import Model, ModelArgs
        from dial_worker import target_logprobs
        mx.random.seed(3)
        model = Model(ModelArgs(model_type="llama", hidden_size=16, num_hidden_layers=2, intermediate_size=32,
                                num_attention_heads=2, num_key_value_heads=2, rms_norm_eps=1e-5, vocab_size=32))
        encoded = [([1, 2, 3, 4, 5, 6], 3), ([7, 8, 9], 1), ([4, 4, 4, 4, 4], 4)]
        together = target_logprobs(model, encoded)
        for item, joint in zip(encoded, together):
            alone = target_logprobs(model, [item])[0]
            self.assertEqual(joint[1], len(item[0]) - item[1])
            self.assertTrue(math.isclose(joint[0], alone[0], abs_tol=1e-4))

    def test_bake_writes_at_the_block_output_then_falls_back_to_the_next_attention(self):
        from unittest.mock import patch
        from mlx_lm.models.llama import Model, ModelArgs
        model = Model(ModelArgs(model_type="llama", hidden_size=16, num_hidden_layers=3, intermediate_size=32,
                                num_attention_heads=2, num_key_value_heads=2, rms_norm_eps=1e-5, vocab_size=32))
        layers = s.layer_list(model)
        place = lambda index: s.placement(model, layers, index, 16)[:3]
        self.assertEqual(place(1), (1, "mlp.down_proj", "model.layers.1.mlp.down_proj"))
        with patch.object(s, "BAKE_MAX_INPUT", 31):
            self.assertEqual(place(1), (2, "self_attn.o_proj", "model.layers.2.self_attn.o_proj"))
            self.assertEqual(place(2), (2, "self_attn.o_proj", "model.layers.2.self_attn.o_proj"))
        self.assertEqual(s.placement(model, layers, 1, 16)[3]["model.layers.1.mlp.down_proj.lora_a"], (32, 1))


if __name__ == "__main__":
    unittest.main()
