import json
import os
import select
import subprocess
import sys
import unittest
from pathlib import Path
from dataclasses import dataclass
from types import SimpleNamespace
from unittest import mock

import dial_server
from dial_server import ToolCallFormatter


class EngineTests(unittest.TestCase):
    def test_vision_runner_uses_its_own_engine_and_compatible_steering_boundary(self):
        server = SimpleNamespace(main=mock.Mock())
        steering = SimpleNamespace(install_vision_server=mock.Mock())
        with mock.patch.dict(os.environ, {"DIAL_MODEL_ENGINE": "mlx-vlm"}), \
             mock.patch.dict(sys.modules, {"mlx_vlm": SimpleNamespace(server=server),
                                           "mlx_lm": None, "dial_steering": steering}), \
             mock.patch.object(dial_server, "watch_parent"):
            dial_server.main()
        server.main.assert_called_once_with()
        steering.install_vision_server.assert_called_once_with(server, None)

    def test_embedding_runner_maps_vlm_checkpoints_to_their_embedding_model(self):
        server = SimpleNamespace(main=mock.Mock())
        loader = SimpleNamespace(EMBEDDING_MODEL_REMAPPING={"siglip": "siglip"})
        with mock.patch.dict(os.environ, {"DIAL_MODEL_ENGINE": "mlx-vlm-embedding"}), \
             mock.patch.dict(sys.modules, {"mlx_vlm": SimpleNamespace(server=server),
                                           "mlx_vlm.server": server,
                                           "mlx_vlm.embedding_loader": loader,
                                           "mlx_lm": None}), \
             mock.patch.object(dial_server, "watch_parent"):
            dial_server.main()
        server.main.assert_called_once_with()
        self.assertEqual(loader.EMBEDDING_MODEL_REMAPPING,
                         {"siglip": "siglip", "qwen3_vl": "qwen3_vl_embedding"})

    def test_embedding_remapping_keeps_an_existing_upstream_mapping(self):
        loader = SimpleNamespace(EMBEDDING_MODEL_REMAPPING={"qwen3_vl": "upstream"})
        dial_server.install_embedding_remapping(loader)
        self.assertEqual(loader.EMBEDDING_MODEL_REMAPPING["qwen3_vl"], "upstream")


class ParentLifetimeTests(unittest.TestCase):
    def test_model_processes_exit_when_their_dial_parent_is_killed(self):
        for module in ["dial_server", "dial_worker"]:
            with self.subTest(module=module):
                child_code = f"import {module}; {module}.watch_parent(); print('ready', flush=True); import time; time.sleep(30)"
                parent_code = (
                    "import os, subprocess, sys, time\n"
                    "env = dict(os.environ, DIAL_PARENT_PID=str(os.getpid()))\n"
                    f"child = subprocess.Popen([sys.executable, '-B', '-c', {child_code!r}], env=env)\n"
                    "print(child.pid, flush=True)\n"
                    "time.sleep(30)\n"
                )
                parent = subprocess.Popen([sys.executable, "-B", "-c", parent_code], stdout=subprocess.PIPE,
                                          cwd=Path(__file__).resolve().parent, bufsize=0)
                child_pid = None
                finished = False
                try:
                    for _ in range(2):
                        self.assertTrue(select.select([parent.stdout], [], [], 5)[0], "model did not start")
                        line = parent.stdout.readline().decode().strip()
                        if line.isdigit(): child_pid = int(line)
                        else: self.assertEqual(line, "ready")
                    parent.kill()
                    # The model inherits this pipe. EOF proves it also exited after its parent died.
                    parent.communicate(timeout=5)
                    finished = True
                finally:
                    if parent.poll() is None: parent.kill()
                    parent.wait()
                    if child_pid is not None and not finished:
                        try: os.kill(child_pid, 9)
                        except ProcessLookupError: pass


class FormatterTests(unittest.TestCase):
    def format(self, text, streaming=False):
        return ToolCallFormatter(lambda text, tools: json.loads(text), [], streaming)([text])

    def test_missing_arguments_are_explicitly_non_executable(self):
        calls = self.format('{"name":"bash"}')
        self.assertEqual(calls[0]['function'], {'name':'bash', 'arguments':'null'})

    def test_invalid_types_and_truncated_output_are_not_server_exceptions(self):
        for text in ['{', 'null', '[]', '"text"', '{"name":3,"arguments":{}}', '{"name":"read","arguments":[]}']:
            with self.subTest(text=text):
                call = self.format(text)[0]
                self.assertEqual(call['function']['arguments'], 'null')

    def test_valid_call_and_failed_call_keep_distinct_stream_indices(self):
        calls = self.format('[{"name":"read","arguments":{"path":"a.txt"}},{"name":"bash"}]', True)
        self.assertEqual([c['index'] for c in calls], [0, 1])
        self.assertEqual(json.loads(calls[0]['function']['arguments']), {'path':'a.txt'})
        self.assertEqual(calls[1]['function']['arguments'], 'null')
        self.assertNotEqual(calls[0]['id'], calls[1]['id'])

    def test_formatting_does_not_mutate_parser_owned_objects(self):
        parsed = {'id':'old', 'name':'read', 'arguments':{'path':'a.txt'}}
        formatter = ToolCallFormatter(lambda text, tools: parsed, [])
        self.assertEqual(formatter.count, 0)
        formatter(['x'])
        self.assertEqual(formatter.count, 1)
        self.assertEqual(parsed, {'id':'old', 'name':'read', 'arguments':{'path':'a.txt'}})

    def test_backend_failures_are_not_mislabelled_as_model_errors(self):
        def fail(text, tools):
            raise MemoryError('backend allocation failed')
        with self.assertRaises(MemoryError):
            ToolCallFormatter(fail, [])(['anything'])


_SPECIAL_TEXT = {1: "<|message_model|>", 2: "<|content_thinking|>", 3: "<|content_invoke_tool_json|>",
                 4: "<|end_message|>", 5: "<|content_model_end_sampling|>", 6: "<|content_text|>"}
MSG, THINK, INVOKE, END, EOS, TEXT = 1, 2, 3, 4, 5, 6


@dataclass(frozen=True)
class _Delta:
    content: str = ""
    reasoning: str = ""
    tool_calls: tuple = ()

    def __add__(self, other):
        return _Delta(self.content + other.content, self.reasoning + other.reasoning,
                      self.tool_calls + other.tool_calls)


class _TextMachine:
    """Stands in for the pinned text state machine: every chunk is content."""
    def reader(self, state="normal"):
        return self

    def feed(self, text):
        return _Delta(content=text)

    def finish(self, text=""):
        return _Delta(content=text)


class _Tokenizer:
    all_special_ids = [*_SPECIAL_TEXT, 9]  # 9 is an unrelated special (padding).
    eos_token_ids = {5}
    tool_parser = None

    def convert_tokens_to_ids(self, text):
        return {v: k for k, v in _SPECIAL_TEXT.items()}.get(text)

    def decode(self, ids):
        return "".join(_SPECIAL_TEXT[i] if i in _SPECIAL_TEXT else "" if i < 1000 else chr(i - 1000)
                       for i in ids)


def _encode(parts):
    """Each str is ordinary text, one token per character; each int is a reserved token."""
    tokens = []
    for part in parts:
        if isinstance(part, int):
            tokens.append(part)
        else:
            tokens.extend(1000 + ord(c) for c in part)
    return tokens


def _turn(parts, *, tools=("read_file",), model_type="inkling2_loop", finish="stop", tool_parser=None):
    """Run one generated turn through the installed hooks, consumed as the handler consumes it.

    Returns (content, [(name, arguments)], tokenizer, tokens)."""
    tokenizer = _Tokenizer()
    tokens = _encode(parts)
    responses = [SimpleNamespace(text=tokenizer.decode([t]), token=t, finish_reason=None,
                                 logprob=0.0, top_tokens=()) for t in tokens]
    responses[-1].finish_reason = finish

    class ResponseGenerator:
        def __init__(self):
            self.model_provider = SimpleNamespace(
                model=SimpleNamespace(args=SimpleNamespace(model_type=model_type)))
            self.ctx = SimpleNamespace(text_sm=_TextMachine(), tool_parser=tool_parser,
                                       initial_state="normal", prompt=[])

        def _tokenize(self, tokenizer, request, args):
            return ()

        def generate(self, request, args, progress_callback=None):
            self._tokenize(tokenizer, request, args)
            return self.ctx, iter(responses)

    server = SimpleNamespace(ResponseGenerator=ResponseGenerator)
    dial_server.install_native_tools(server)
    request = SimpleNamespace(request_type="chat", tools=[
        {"type": "function", "function": {"name": name, "parameters": {"type": "object"}}} for name in tools])
    ctx, response = ResponseGenerator().generate(request, SimpleNamespace())
    reader = ctx.text_sm.reader(state=ctx.initial_state)
    content, raw_calls = "", []
    for gen in response:
        delta = reader.finish(gen.text) if gen.finish_reason is not None else reader.feed(gen.text)
        content += delta.content
        raw_calls.extend(delta.tool_calls)
    calls = ToolCallFormatter(ctx.tool_parser, request.tools)(raw_calls)
    parsed = [(c["function"]["name"], json.loads(c["function"]["arguments"])) for c in calls]
    return content, parsed, tokenizer, tokens


class NativeToolTests(unittest.TestCase):
    """Inkling 2 tool calls are promoted only from the exact reserved-token sequence."""

    def test_exact_frames_are_promoted_and_their_markup_is_not_text(self):
        frame = lambda name, body: [MSG, name, INVOKE, body, END]
        reasoning = [MSG, THINK, "plan", END]
        cases = [
            ("direct call", frame("read_file", '{"name":"read_file","args":{"path":"a.txt"}}') + [EOS],
             "", [("read_file", {"path": "a.txt"})]),
            ("empty arguments", frame("read_file", '{"name":"read_file","args":{}}') + [EOS],
             "", [("read_file", {})]),
            ("after a complete reasoning frame",
             reasoning + frame("read_file", '{"name":"read_file","args":{"path":"b"}}') + [EOS],
             "<|message_model|><|content_thinking|>plan<|end_message|>", [("read_file", {"path": "b"})]),
            ("after complete thinking and visible text frames",
             reasoning + [MSG, TEXT, "I'll look.", END] + frame("read_file", '{"name":"read_file","args":{}}') + [EOS],
             "<|message_model|><|content_thinking|>plan<|end_message|>"
             "<|message_model|><|content_text|>I'll look.<|end_message|>", [("read_file", {})]),
        ]
        for label, parts, content, calls in cases:
            with self.subTest(label):
                got_content, got_calls, _, _ = _turn(parts)
                self.assertEqual((got_content, got_calls), (content, calls))

    def test_anything_else_keeps_the_text_for_the_rust_fallback(self):
        valid = '{"name":"read_file","args":{"path":"a"}}'
        frame = lambda name, body: [MSG, name, INVOKE, body, END]
        deep = "[" * 100000 + "]" * 100000
        cases = [
            ("literal markup", ["<|message_model|>read_file<|content_invoke_tool_json|>" + valid + "<|end_message|>", EOS], {}),
            ("triangle markup", ["◁message_model▷read_file◁content_invoke_tool_json▷" + valid + "◁end_message▷", EOS], {}),
            ("truncated frame", [MSG, "read_file", INVOKE, '{"name":"read_file","args":{'], {"finish": "length"}),
            ("no end before eos", [MSG, "read_file", INVOKE, valid, EOS], {}),
            ("no eos after end", frame("read_file", valid), {"finish": "length"}),
            ("eos without stop", frame("read_file", valid) + [EOS], {"finish": "length"}),
            ("text after the frame", frame("read_file", valid) + ["ok", EOS], {}),
            ("text before the frame", ["Sure. ", MSG, "read_file", INVOKE, valid, END, EOS], {}),
            ("unoffered name", frame("write_file", '{"name":"write_file","args":{}}') + [EOS], {}),
            ("payload names another tool", frame("read_file", '{"name":"write_file","args":{}}') + [EOS], {}),
            ("duplicate key", frame("read_file", '{"name":"read_file","name":"read_file","args":{}}') + [EOS], {}),
            ("arguments not an object", frame("read_file", '{"name":"read_file","args":[]}') + [EOS], {}),
            ("extra payload key", frame("read_file", '{"name":"read_file","args":{},"id":"1"}') + [EOS], {}),
            ("non-standard constant", frame("read_file", '{"name":"read_file","args":{"n":NaN}}') + [EOS], {}),
            ("deeply nested arguments", frame("read_file", '{"name":"read_file","args":' + deep + '}') + [EOS], {}),
            ("name outside the frame grammar", frame("read.file", '{"name":"read.file","args":{}}') + [EOS],
             {"tools": ("read.file",)}),
            ("empty recipient", [MSG, INVOKE, '{"name":"","args":{}}', END, EOS], {}),
            ("unclosed content frame before the tool", [MSG, TEXT, "hi"] + frame("read_file", valid) + [EOS], {}),
            ("tool example within content", [MSG, TEXT, "<|message_model|>read_file<|content_invoke_tool_json|>" + valid + "<|end_message|>", END, EOS], {}),
            ("literal end cannot close native content", [MSG, TEXT, "hi<|end_message|>"] + frame("read_file", valid) + [EOS], {}),
            ("reserved token inside arguments",
             [MSG, "read_file", INVOKE, '{"name":"read_file","args":{"p":', TEXT, '"x"}}', END, EOS], {}),
            ("reserved token inside reasoning",
             [MSG, THINK, "example ", MSG, "read_file", INVOKE, valid, END, EOS], {}),
            ("other model family", frame("read_file", valid) + [EOS], {"model_type": "llama"}),
            ("no offered tools", frame("read_file", valid) + [EOS], {"tools": ()}),
            ("upstream parser owns tool calls", frame("read_file", valid) + [EOS],
             {"tool_parser": lambda text, tools: None}),
        ]
        for label, parts, options in cases:
            with self.subTest(label):
                content, calls, tokenizer, tokens = _turn(parts, **options)
                self.assertEqual(calls, [])
                self.assertEqual(content, tokenizer.decode(tokens))

    def test_reserved_markers_must_be_distinct_round_tripping_specials(self):
        class Collapsed(_Tokenizer):
            def convert_tokens_to_ids(self, text):
                return 6  # Every marker falls back to one shared unknown-token ID.

        class Missing(_Tokenizer):
            def convert_tokens_to_ids(self, text):
                return None

        generator = SimpleNamespace(model_provider=SimpleNamespace(
            model=SimpleNamespace(args=SimpleNamespace(model_type="inkling2_loop"))))
        request = SimpleNamespace(request_type="chat", tools=[{"function": {"name": "read_file"}}])
        for tokenizer in (Collapsed(), Missing()):
            with self.subTest(tokenizer=type(tokenizer).__name__):
                self.assertIsNone(dial_server._native_tool_spec(generator, tokenizer, request))


    def test_held_frames_are_bounded_and_released_as_text(self):
        body = '{"name":"read_file","args":{"p":"' + "x" * (dial_server._NATIVE_TOOL_MAX_TOKENS + 10) + '"}}'
        parts = [MSG, "read_file", INVOKE, body, END, EOS]
        content, calls, tokenizer, tokens = _turn(parts)
        self.assertEqual(calls, [])
        self.assertEqual(content, tokenizer.decode(tokens))


if __name__ == '__main__':
    unittest.main()
