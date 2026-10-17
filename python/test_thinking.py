"""Thinking is redirected inside one completion; answer/tool tokens retain normal sampling."""
import os
import unittest
from pathlib import Path
from types import SimpleNamespace as NS

import dial_server as dial


class Token:
    def __init__(self, value): self.value = value
    def item(self): return self.value


class Tokenizer:
    has_thinking = True
    think_start_tokens = (100,)
    think_end_tokens = (101,)
    def decode(self, tokens): return "x" * len(tokens)


def controller(tokens, initial="reasoning", max_bytes=10000, max_seconds=0, max_tokens=8192,
               tokenizer=None):
    iterator = iter(tokens)
    sampled = []
    def base(_):
        token = next(iterator)
        sampled.append(token)
        return Token(token)
    guard = dial._ThinkingSampler(base, tokenizer or Tokenizer(),
        {"max_bytes": max_bytes, "max_seconds": max_seconds}, initial, max_tokens,
        lambda _, token: Token(token))
    return guard, sampled


class ThinkingTests(unittest.TestCase):
    def test_repeating_reasoning_closes_then_same_completion_produces_action(self):
        block = list(range(8))
        # Base sampling stops exactly when the repetition is recognized, then resumes with the
        # action. The injected token does not consume or execute a partial tool call.
        action = [200, 201, 202, 203]
        guard, sampled = controller(block * 4 + action)
        output = [guard(None).item() for _ in range(37)]
        self.assertEqual(output, block * 4 + [101] + action)
        self.assertEqual(sampled, block * 4 + action)
        self.assertFalse(guard.thinking)

    def test_prefill_and_unique_reasoning_are_not_mistaken_for_a_loop(self):
        guard, _ = controller(range(90))
        guard.clock = lambda: 10000  # No constructor-time deadline includes model loading.
        self.assertEqual([guard(None).item() for _ in range(90)], list(range(90)))
        self.assertFalse(guard.redirected)

    def test_gpu_wait_before_the_first_token_does_not_consume_thinking_time(self):
        guard, _ = controller([1, 200], max_seconds=2)
        now = [0]
        base = guard.sampler
        def slow_prefill(scores):
            now[0] = 100
            return base(scores)
        guard.sampler = slow_prefill
        guard.clock = lambda: now[0]
        self.assertEqual(guard(None).item(), 1)
        self.assertEqual(guard.started, 100)
        self.assertEqual(guard(None).item(), 200)
        self.assertFalse(guard.redirected)

    def test_repetition_detection_does_not_depend_on_token_chunk_alignment(self):
        block = list(range(21))
        guard, _ = controller(block * 4 + [200])
        self.assertEqual([guard(None).item() for _ in range(86)], block * 4 + [101, 200])

    def test_bytes_and_time_close_thinking_without_stopping_answer(self):
        guard, _ = controller([1, 2, 3, 200], max_bytes=3)
        self.assertEqual([guard(None).item() for _ in range(5)], [1, 2, 3, 101, 200])
        guard, _ = controller([1, 200], max_seconds=2)
        now = [100]
        guard.clock = lambda: now[0]
        self.assertEqual(guard(None).item(), 1)
        now[0] = 102
        self.assertEqual(guard(None).item(), 101)
        self.assertEqual(guard(None).item(), 200)

    def test_output_budget_reserves_space_for_action(self):
        guard, _ = controller([1, 2, 3, 4, 200, 201, 202], max_tokens=8)
        self.assertEqual([guard(None).item() for _ in range(8)], [1, 2, 3, 4, 101, 200, 201, 202])

    def test_separate_thinking_spans_share_the_byte_allowance(self):
        guard, _ = controller([1, 2, 101, 200, 100, 3, 201], max_bytes=3)
        self.assertEqual([guard(None).item() for _ in range(8)], [1, 2, 101, 200, 100, 3, 101, 201])

    def test_repeated_answer_and_tool_arguments_are_not_changed(self):
        tokens = [100, 1, 101] + list(range(8)) * 10
        guard, _ = controller(tokens, initial="normal")
        self.assertEqual([guard(None).item() for _ in tokens], tokens)
        self.assertFalse(guard.redirected)

    def test_multi_token_markers_close_in_order_and_cannot_restart_overthinking(self):
        tokenizer = Tokenizer()
        tokenizer.think_start_tokens = (100, 102)
        tokenizer.think_end_tokens = (101, 103)
        guard, _ = controller([1, 200, 100, 102, 201], max_bytes=1, tokenizer=tokenizer)
        self.assertEqual([guard(None).item() for _ in range(9)],
                         [1, 101, 103, 200, 100, 102, 101, 103, 201])


class BoundaryTests(unittest.TestCase):
    def server(self):
        base = lambda _: Token(7)
        class Generator:
            model_provider = NS(draft_model=None, is_distributed=False)
            def _tokenize(self, tokenizer, request, args):
                return [], [], [], "reasoning"
            def generate(self, request, args, progress_callback=None):
                self._tokenize(Tokenizer(), request, args)
                return server._make_sampler(args, Tokenizer())
        class Handler:
            def __init__(self, body): self.body = body
            def handle_completion(self):
                self.args = NS(max_tokens=100)
                return Generator().generate(None, self.args)
        server = NS(APIHandler=Handler, ResponseGenerator=Generator,
                    _make_sampler=lambda *a: base)
        return server, base

    def test_limits_cross_gpu_queue_and_do_not_leak_to_other_requests(self):
        server, base = self.server()
        dial.install_thinking_control(server)
        dial.install_thinking_control(server)
        handler = server.APIHandler({"dial_reasoning": {"max_bytes": 12, "max_seconds": 0}})
        controlled = handler.handle_completion()
        self.assertIsInstance(controlled, dial._ThinkingSampler)
        self.assertEqual(controlled.max_bytes, 12)
        self.assertIs(server.APIHandler({}).handle_completion(), base)
        self.assertIs(server.ResponseGenerator().generate(None, NS(max_tokens=100)), base)

    def test_bad_limits_and_speculative_or_distributed_decoders_use_fallback(self):
        server, base = self.server()
        dial.install_thinking_control(server)
        for limits in [None, {}, {"max_bytes": True, "max_seconds": 0},
                       {"max_bytes": 1, "max_seconds": float("nan")},
                       {"max_bytes": 1, "max_seconds": -1}]:
            self.assertIs(server.APIHandler({"dial_reasoning": limits}).handle_completion(), base)
        valid = {"max_bytes": 12, "max_seconds": 0}
        for provider in [NS(draft_model=object()), NS(is_distributed=True)]:
            server.ResponseGenerator.model_provider = provider
            self.assertIs(server.APIHandler({"dial_reasoning": valid}).handle_completion(), base)


@unittest.skipUnless(os.environ.get("DIAL_THINKING_TOKENIZER"), "optional real MLX/tokenizer check")
class MlxTests(unittest.TestCase):
    def test_pinned_sampler_and_text_parser_preserve_a_complete_tool_call(self):
        import mlx.core as mx
        from mlx_lm import server
        from mlx_lm.generate import TextStateMachine, make_text_state_machine
        from mlx_lm.tokenizer_utils import load
        tokenizer = load(Path(os.environ["DIAL_THINKING_TOKENIZER"]), {"trust_remote_code": False})
        dial.install_thinking_control(server)
        args = NS(sampling=NS(temperature=0, top_p=1, top_k=0, min_p=0,
                              xtc_probability=0, xtc_threshold=0), max_tokens=8192,
                  _dial_thinking_limits={"max_bytes":10000, "max_seconds":0},
                  _dial_thinking_supported=True, _dial_initial_state="reasoning")
        sampler = server._make_sampler(args, tokenizer)
        loop = tokenizer.encode("self.bullet.set_linear_velocity(Point(0, 0, 0))\n", add_special_tokens=False)
        action = "<tool_call>read<arg_key>path</arg_key><arg_value>game.py</arg_value></tool_call>"
        action_tokens = tokenizer.encode(action, add_special_tokens=False)
        base_tokens = loop * 4 + action_tokens
        output = []
        index = 0
        while index < len(base_tokens):
            scores = mx.full((1, tokenizer.vocab_size), -100.0)
            scores[:, base_tokens[index]] = 0.0
            forced = bool(sampler.closing) or sampler.redirected and sampler.thinking
            token = sampler(scores).item()
            output.append(token)
            # The first forced marker is recognized by its id; it is absent from the script.
            if not forced and token != tokenizer.think_end_tokens[0]:
                index += 1
        self.assertTrue(sampler.redirected)
        self.assertFalse(sampler.thinking)
        self.assertEqual(output[-len(action_tokens):], action_tokens)
        self.assertEqual(output[len(loop) * 4:len(loop) * 4 + len(tokenizer.think_end_tokens)],
                         list(tokenizer.think_end_tokens))
        state = make_text_state_machine(tokenizer).make_state("reasoning")
        tool_text = ""
        for token in output:
            state, text, phase = TextStateMachine.step(state, tokenizer.decode([token]))
            if phase == "tool": tool_text += text
        tools = [{"type":"function", "function":{"name":"read", "parameters":{
            "type":"object", "properties":{"path":{"type":"string"}}}}}]
        parsed = tokenizer.tool_parser(tool_text, tools)
        self.assertEqual(parsed["name"], "read")
        self.assertEqual(parsed["arguments"], {"path":"game.py"})


if __name__ == "__main__":
    unittest.main()
