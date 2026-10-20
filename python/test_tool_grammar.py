"""Tool-call spans are schema-constrained; free text, thinking and valid calls are untouched."""
import json
import math
import unittest

import dial_tool_grammar as grammar

TOOLS = [
    {"type": "function", "function": {"name": "read", "parameters": {
        "type": "object", "additionalProperties": False, "required": ["path"],
        "properties": {"path": {"type": "string"}, "offset": {"type": "integer", "minimum": 1}}}}},
    {"type": "function", "function": {"name": "todo", "parameters": {
        "type": "object", "required": ["op"],
        "properties": {"op": {"enum": ["add", "done"]},
                       "items": {"type": "array", "items": {"type": "string"}}}}}},
    {"type": "function", "function": {"name": "mcp_call", "parameters": {
        "type": "object", "additionalProperties": True, "properties": {"q": {"type": "string"}}}}},
]
SCHEMAS = grammar.tool_schemas(TOOLS)


def accepts(text):
    matcher = grammar.call_matcher(SCHEMAS)
    return matcher.feed(text) and matcher.complete


class MatcherTests(unittest.TestCase):
    def test_valid_calls_complete(self):
        for text in ['{"name": "read", "arguments": {"path": "a.py"}}',
                     '{"name":"read","arguments":{"path":"a\\n\\u00e9","offset":3}}\n',
                     '{"name": "todo", "arguments": {"op": "add", "items": ["x", "y"]}}',
                     '{"name": "mcp_call", "arguments": {"anything": [1, {"k": null}]}}',
                     '{"arguments": {"path": "a"}, "name": "read"}']:
            with self.subTest(text=text):
                self.assertTrue(accepts(text))

    def test_rejections_happen_at_the_first_invalid_character(self):
        cases = {
            '{"name": "reed"': '{"name": "re',  # undeclared tool
            '{"name": "read", "arguments": {"file_path"': '{"name": "read", "arguments": {"',
            '{"name": "read", "arguments": {}': '{"name": "read", "arguments": {',  # missing required
            '{"name": "read", "arguments": {"path": 3': '{"name": "read", "arguments": {"path": ',
            '{"name": "read", "arguments": {"path": "a\nb"': '{"name": "read", "arguments": {"path": "a',
            '{"name": "read", "arguments": {"path": "a", "offset": 1.5':
                '{"name": "read", "arguments": {"path": "a", "offset": 1',
            '{"name": "todo", "arguments": {"op": "remove"': '{"name": "todo", "arguments": {"op": "',
            '{"name": "todo", "arguments": {"op": "add", "items": [1':
                '{"name": "todo", "arguments": {"op": "add", "items": [',
            '{"name": "read", "arguments": {"path": "a", "offset": 01':
                '{"name": "read", "arguments": {"path": "a", "offset": 0',
            '{"name": "read", "name"': '{"name": "read", ',  # duplicate key, closed wrapper
            '{"name": "read", "arguments": {"path": "a"}} x': '{"name": "read", "arguments": {"path": "a"}} ',
        }
        for invalid, longest_valid in cases.items():
            with self.subTest(invalid=invalid):
                matcher = grammar.call_matcher(SCHEMAS)
                self.assertTrue(matcher.feed(longest_valid))
                self.assertFalse(matcher.clone().feed(invalid[len(longest_valid):]))

    def test_closed_object_refuses_comma_when_no_keys_remain(self):
        matcher = grammar.call_matcher(SCHEMAS)
        self.assertTrue(matcher.feed('{"name": "read", "arguments": {"path": "a", "offset": 2'))
        self.assertFalse(matcher.clone().feed(","))
        self.assertTrue(matcher.feed("}}"))

    def test_whitespace_loops_are_cut_off(self):
        matcher = grammar.call_matcher(SCHEMAS)
        self.assertFalse(matcher.feed("{" + "\n" * 200))
        self.assertTrue(grammar.call_matcher(SCHEMAS).feed("{\n  " + '"name"'))

    def test_unsupported_combinators_stay_syntax_only(self):
        schemas = {"x": {"type": "object", "properties": {"v": {"anyOf": [{"type": "string"}]}}}}
        matcher = grammar.call_matcher(schemas)
        self.assertTrue(matcher.feed('{"name": "x", "arguments": {"v": [true]}}'))
        self.assertTrue(matcher.complete)


# Fake tokenizer: ids 0-4 are special; every other id is one character or a short word.
EOS, START, END, THINK, UNTHINK = range(5)
WORDS = ["path", "file_path", "read", "name", "arguments", '": "', '{"', '"}', "}}"]
CHARS = [chr(c) for c in range(32, 127)] + ["\n"]
VOCAB = ["<eos>", "<tool_call>", "</tool_call>", "<think>", "</think>"] + WORDS + CHARS
IDS = {text: i for i, text in enumerate(VOCAB)}


class Tokenizer:
    tool_call_start_tokens = (START,)
    tool_call_end_tokens = (END,)
    has_thinking = True
    think_start_tokens = (THINK,)
    think_end_tokens = (UNTHINK,)


class Bytes:
    size = len(VOCAB)

    def __call__(self, token):
        return None if token < 5 else VOCAB[token].encode()


class Token:
    def __init__(self, value):
        self.value = value
        self.reads = 0

    def item(self):
        self.reads += 1
        return self.value


class Ops:
    def force(self, logprobs, token):
        return Token(token)

    def exclude(self, logprobs, tokens):
        return [-math.inf if i in tokens else v for i, v in enumerate(logprobs)]

    def only(self, logprobs, tokens):
        return [v if i in tokens else -math.inf for i, v in enumerate(logprobs)]

    def ranked(self, logprobs, count):
        return sorted(range(len(logprobs)), key=lambda i: -logprobs[i])[:count]


def greedy(logprobs):
    return Token(max(range(len(logprobs)), key=lambda i: logprobs[i]))


def tokenize(script):
    """Greedy longest-match over the fake vocabulary; `<...>` specials pass through."""
    out, i = [], 0
    while i < len(script):
        match = max((t for t in VOCAB if script.startswith(t, i)), key=len)
        out.append(IDS[match])
        i += len(match)
    return out


def run(script, initial_state="normal", steps=None, tokenizer=None):
    """A 'model' that always prefers the next scripted token, then ids in vocabulary order."""
    wanted = tokenize(script)
    sampler = grammar.ToolCallSampler(greedy, tokenizer or Tokenizer(), SCHEMAS, initial_state,
                                      Ops(), Bytes())
    out, issued = [], []
    for step in range(steps or len(wanted) + 40):
        logprobs = [-1.0 - i * 1e-3 for i in range(len(VOCAB))]
        if step < len(wanted):
            logprobs[wanted[step]] = 0.0
        token = sampler(logprobs)
        issued.append(token)
        out.append(token.item())
        if out[-1] == EOS:
            break
    return out, sampler, issued


def text(tokens):
    return "".join(VOCAB[t] for t in tokens)


def call_body(tokens):
    rendered = text(tokens)
    return rendered.split("<tool_call>", 1)[1].split("</tool_call>", 1)[0]


class SamplerTests(unittest.TestCase):
    def test_valid_call_and_free_text_are_reproduced_exactly(self):
        script = 'ok <tool_call>{"name": "read", "arguments": {"path": "a.py"}}</tool_call><eos>'
        out, sampler, _ = run(script)
        self.assertEqual(text(out), script)
        self.assertEqual(sampler.calls, 1)

    def test_invalid_call_is_steered_to_a_schema_valid_call(self):
        script = '<tool_call>{"name": "read", "arguments": {"file_path": "a.py"}}<eos>'
        out, _, _ = run(script, steps=200)
        body = json.loads(call_body(out))
        self.assertEqual(body["name"], "read")
        self.assertIn("path", body["arguments"])
        self.assertLessEqual(set(body["arguments"]), {"path", "offset"})
        self.assertIn(END, out)  # EOS was refused until the call closed.

    def test_raw_newline_in_string_becomes_an_escape_or_is_skipped(self):
        script = '<tool_call>{"name": "read", "arguments": {"path": "a\nb"}}</tool_call>'
        out, _, _ = run(script, steps=200)
        body = json.loads(call_body(out))
        self.assertNotIn("\n", body["arguments"]["path"][:1])
        self.assertTrue(body["arguments"]["path"].startswith("a"))

    def test_thinking_span_is_not_constrained(self):
        script = '<think><tool_call>{"bad"}</think>done'
        out, sampler, _ = run(script, initial_state="normal", steps=len(tokenize(script)))
        self.assertEqual(text(out), script)
        self.assertEqual(sampler.calls, 0)

    def test_tokens_forced_by_an_outer_sampler_update_thinking_state(self):
        sampler = grammar.ToolCallSampler(greedy, Tokenizer(), SCHEMAS, "reasoning", Ops(), Bytes())
        sampler.observe(UNTHINK)
        logprobs = [-1.0] * len(VOCAB)
        logprobs[START] = 0.0
        sampler(logprobs)
        logprobs = [-1.0 - i * 1e-3 for i in range(len(VOCAB))]
        logprobs[IDS["x"]] = 0.0
        self.assertNotEqual(sampler(logprobs).item(), IDS["x"])  # constrained: call must open with {
        self.assertIsNotNone(sampler.matcher)

    def test_free_text_token_is_not_synchronized_inside_its_own_step(self):
        sampler = grammar.ToolCallSampler(greedy, Tokenizer(), SCHEMAS, "normal", Ops(), Bytes())
        first = sampler([0.0 if i == IDS["x"] else -1.0 for i in range(len(VOCAB))])
        self.assertEqual(first.reads, 0)  # read on the next step, once MLX has evaluated it
        sampler([-1.0] * len(VOCAB))
        self.assertEqual(first.reads, 1)


class ParserTokenizer:
    """Only what `format_for` reads: the parser's module path and the call markers."""

    def __init__(self, name, start=(1,), end=(2,)):
        def parser():
            pass
        parser.__module__ = f"mlx_lm.tool_parsers.{name}"
        self.tool_parser = parser
        self.tool_call_start_tokens, self.tool_call_end_tokens = start, end


def drive(name, *chunks):
    """Feed chunks to the named format's matcher; returns (matcher, every chunk accepted)."""
    matcher = grammar.format_for(ParserTokenizer(name)).make(SCHEMAS)
    return matcher, all(matcher.feed(chunk) for chunk in chunks)


class FormatTests(unittest.TestCase):
    def test_registry_covers_the_supported_parsers(self):
        for name in ("json_tools", "gemma4", "function_gemma", "glm47", "qwen3_coder", "mistral"):
            self.assertIsNotNone(grammar.format_for(ParserTokenizer(name)), name)
            self.assertTrue(grammar.supported(ParserTokenizer(name)), name)

    def test_unknown_parsers_and_missing_markers_stay_unconstrained(self):
        self.assertIsNone(grammar.format_for(ParserTokenizer("llama3_json")))
        self.assertFalse(grammar.supported(ParserTokenizer("llama3_json")))
        self.assertIsNone(grammar.format_for(ParserTokenizer("gemma4", end=())))
        self.assertIsNone(grammar.format_for(ParserTokenizer("gemma4", start=())))
        self.assertIsNotNone(grammar.format_for(ParserTokenizer("mistral", end=())))

    def test_gemma4_accepts_tagged_strings_and_bare_keys(self):
        matcher, ok = drive("gemma4", 'call:read{path:<|"|>', 'a.txt<|"|>,offset:2}')
        self.assertTrue(ok)
        self.assertTrue(matcher.complete)

    def test_gemma4_rejects_unknown_name_key_and_type(self):
        self.assertFalse(drive("gemma4", "call:bogus{")[1])
        self.assertFalse(drive("gemma4", "call:read{bogus:1}")[1])
        self.assertFalse(drive("gemma4", "call:read{path:2")[1])

    def test_function_gemma_accepts_escaped_strings_and_rejects_brace_in_string(self):
        matcher, ok = drive("function_gemma", "call:read{path:<escape>a.txt<escape>}")
        self.assertTrue(ok)
        self.assertTrue(matcher.complete)
        self.assertFalse(drive("function_gemma", "call:read{path:<escape>a}b<escape>")[1])

    def test_glm47_completes_only_with_required_keys_seen(self):
        matcher, ok = drive("glm47", "read<arg_key>path</arg_key><arg_value>a.txt</arg_value>")
        self.assertTrue(ok)
        self.assertTrue(matcher.can_end)
        self.assertFalse(matcher.complete)
        matcher, ok = drive("glm47", "read<arg_key>offset</arg_key><arg_value>2</arg_value>")
        self.assertTrue(ok)
        self.assertFalse(matcher.can_end)

    def test_glm47_rejects_unknown_keys(self):
        self.assertFalse(drive("glm47", "read<arg_key>bogus")[1])

    def test_glm47_enum_value(self):
        matcher, ok = drive("glm47", "todo<arg_key>op</arg_key><arg_value>add</arg_value>")
        self.assertTrue(ok)
        self.assertTrue(matcher.can_end)

    def test_qwen3_coder_accepts_valid_calls_across_chunks(self):
        matcher, ok = drive("qwen3_coder", "\n<function=read>\n<para", "meter=path>\na.txt\n</param",
                            "eter>\n<parameter=offset>\n2\n</parameter>\n</function>\n")
        self.assertTrue(ok)
        self.assertTrue(matcher.can_end)
        self.assertFalse(matcher.complete)
        matcher, ok = drive("qwen3_coder", "<function=todo><parameter=op>done</parameter>"
                            "<parameter=items>[\"x\"]</parameter></function>")
        self.assertTrue(ok)
        self.assertTrue(matcher.can_end)

    def test_qwen3_coder_string_values_are_raw_text(self):
        matcher, ok = drive("qwen3_coder", "<function=read><parameter=path>\na <b>\n</c>{\n"
                            "</parameter></function>")
        self.assertTrue(ok)
        self.assertTrue(matcher.can_end)

    def test_qwen3_coder_rejects_unknown_name_key_and_duplicate(self):
        self.assertFalse(drive("qwen3_coder", "<function=reed>")[1])
        self.assertFalse(drive("qwen3_coder", "<function=read><parameter=file_path>")[1])
        self.assertFalse(drive("qwen3_coder", "<function=read><parameter=path>a</parameter>"
                               "<parameter=path>")[1])
        self.assertFalse(drive("qwen3_coder", "<function=read ><parameter=path ")[1])
        self.assertTrue(drive("qwen3_coder", "<function=mcp_call><parameter=anything>1"
                              "</parameter></function>")[1])

    def test_qwen3_coder_checks_value_types_when_the_parameter_closes(self):
        prefix = "<function=read><parameter=path>a</parameter><parameter=offset>"
        self.assertFalse(drive("qwen3_coder", prefix + "x")[1])
        self.assertFalse(drive("qwen3_coder", prefix + "1.5")[1])
        matcher, ok = drive("qwen3_coder", prefix + "-")
        self.assertTrue(ok)
        self.assertFalse(matcher.feed("</parameter>"))
        self.assertTrue(drive("qwen3_coder", prefix + "\n 12 \n</parameter>")[1])
        self.assertFalse(drive("qwen3_coder", "<function=todo><parameter=op>add</parameter>"
                               "<parameter=items>[1]")[1])
        self.assertFalse(drive("qwen3_coder", "<function=read><parameter=path>\nnull\n"
                               "</parameter>")[1])  # Would read back as None.

    def test_qwen3_coder_enum_values_are_exact(self):
        self.assertTrue(drive("qwen3_coder", "<function=todo><parameter=op>\nadd\n</parameter>")[1])
        self.assertFalse(drive("qwen3_coder", "<function=todo><parameter=op>remove")[1])
        self.assertFalse(drive("qwen3_coder", "<function=todo><parameter=op> add</parameter>")[1])
        self.assertFalse(drive("qwen3_coder", "<function=todo><parameter=op>ad</parameter>")[1])

    def test_qwen3_coder_requires_keys_and_one_trailing_newline(self):
        matcher, ok = drive("qwen3_coder", "<function=read>\n")
        self.assertTrue(ok)
        self.assertFalse(matcher.can_end)
        self.assertFalse(matcher.clone().feed("</function>"))  # path is required
        matcher, ok = drive("qwen3_coder", "<function=read><parameter=path>a</parameter></function>")
        self.assertTrue(ok)
        self.assertFalse(matcher.clone().feed("\n\n"))
        self.assertFalse(matcher.clone().feed(" "))
        self.assertTrue(matcher.feed("\n"))
        self.assertTrue(matcher.can_end)
        self.assertFalse(drive("qwen3_coder", "<function=read><parameter=path>a</parameter>"
                               "<parameter=offset>1</parameter><parameter=")[1])  # closed, none left

    def test_qwen3_coder_whitespace_loops_are_cut_off(self):
        self.assertFalse(drive("qwen3_coder", "\n" * 200)[1])
        self.assertTrue(drive("qwen3_coder", "\n  <function= read>\n\n  <parameter= path>")[1])

    def test_mistral_accepts_both_forms_and_splits_across_chunks(self):
        matcher, ok = drive("mistral", '[{"name": "read", "arguments": {"path": "a.txt"}}]')
        self.assertTrue(ok)
        self.assertTrue(matcher.complete)
        matcher, ok = drive("mistral", 'read[AR', 'GS]{"path": "a"}')
        self.assertTrue(ok)
        self.assertTrue(matcher.complete)

    def test_mistral_rejects_wrong_type_and_unknown_name(self):
        self.assertFalse(drive("mistral", 'read[ARGS]{"path": 3}')[1])
        self.assertFalse(drive("mistral", "nope[ARGS]")[1])



try:
    from mlx_lm.tool_parsers import function_gemma as _function_gemma
    from mlx_lm.tool_parsers import gemma4 as _gemma4
    from mlx_lm.tool_parsers import glm47 as _glm47
    from mlx_lm.tool_parsers import json_tools as _json_tools
    from mlx_lm.tool_parsers import mistral as _mistral
    from mlx_lm.tool_parsers import qwen3_coder as _qwen3_coder
    PARSERS = {"json_tools": _json_tools, "gemma4": _gemma4, "function_gemma": _function_gemma,
               "glm47": _glm47, "mistral": _mistral, "qwen3_coder": _qwen3_coder}
except ImportError:
    PARSERS = {}


READ = {"name": "read", "arguments": {"path": "a.txt", "offset": 2}}
ROUND_TRIPS = [
    ("json_tools", '{"name": "read", "arguments": {"path": "a.txt", "offset": 2}}', READ),
    ("gemma4", 'call:read{path:<|"|>a.txt<|"|>,offset:2}', READ),
    ("gemma4", 'call:read{path:<|"|>a}b<|"|>}', {"name": "read", "arguments": {"path": "a}b"}}),
    ("function_gemma", 'call:read{path:<escape>a.txt<escape>,offset:2}', READ),
    ("glm47", "read<arg_key>path</arg_key><arg_value>a.txt</arg_value>"
              "<arg_key>offset</arg_key><arg_value>2</arg_value>", READ),
    ("mistral", '[{"name": "read", "arguments": {"path": "a.txt", "offset": 2}}]', READ),
    ("mistral", 'read[ARGS]{"path": "a.txt", "offset": 2}', READ),
    ("qwen3_coder", "\n<function=read>\n<parameter=path>\na.txt\n</parameter>\n"
                    "<parameter=offset>\n2\n</parameter>\n</function>\n", READ),
    ("qwen3_coder", "<function=read><parameter=path>\n a<b>\n\n</parameter></function>",
     {"name": "read", "arguments": {"path": " a<b>\n"}}),
    ("qwen3_coder", "<function=todo><parameter=op>done</parameter><parameter=items>\n"
                    "[\"x\", \"y\"]\n</parameter></function>",
     {"name": "todo", "arguments": {"op": "done", "items": ["x", "y"]}}),
]


@unittest.skipUnless(PARSERS, "mlx_lm is not importable")
class RoundTripTests(unittest.TestCase):
    def test_grammar_accepted_calls_parse_to_the_same_call(self):
        for fmt, call, expected in ROUND_TRIPS:
            with self.subTest(fmt=fmt, call=call):
                matcher, ok = drive(fmt, call)
                self.assertTrue(ok)
                self.assertTrue(matcher.can_end)
                self.assertEqual(PARSERS[fmt].parse_tool_call(call, TOOLS), expected)

    def test_grammar_rejects_what_the_parser_would_misread(self):
        cases = [
            ("gemma4", 'call:read{bogus:1,path:<|"|>a<|"|>}',
             {"name": "read", "arguments": {"bogus": 1, "path": "a"}}),
            ("mistral", 'read[ARGS]{"path": 3}', {"name": "read", "arguments": {"path": 3}}),
            ("qwen3_coder", "<function=read><parameter=path>null</parameter></function>",
             {"name": "read", "arguments": {"path": None}}),
            ("qwen3_coder", "<function=read><parameter=path>a</parameter><parameter=bogus>1"
                            "</parameter></function>",
             {"name": "read", "arguments": {"path": "a", "bogus": "1"}}),
        ]
        for fmt, call, parsed in cases:
            with self.subTest(fmt=fmt, call=call):
                self.assertFalse(drive(fmt, call)[1])
                self.assertEqual(PARSERS[fmt].parse_tool_call(call, TOOLS), parsed)

    def test_grammar_rejects_brace_in_function_gemma_string_that_the_parser_cannot_read(self):
        call = "call:read{path:<escape>a}b<escape>}"
        self.assertFalse(drive("function_gemma", call)[1])
        with self.assertRaises(ValueError):
            PARSERS["function_gemma"].parse_tool_call(call, TOOLS)


class MistralSettleTests(unittest.TestCase):
    def test_free_text_resumes_after_an_open_ended_call_completes(self):
        script = '<tool_call>read[ARGS]{"path": "a.py"}ok then<eos>'
        out, sampler, _ = run(script, tokenizer=ParserTokenizer("mistral", end=()))
        self.assertEqual(text(out), script)
        self.assertEqual(sampler.calls, 1)
        self.assertIsNone(sampler.matcher)


class QwenCoderSamplerTests(unittest.TestCase):
    def test_valid_call_is_reproduced_exactly(self):
        script = ("ok <tool_call>\n<function=read>\n<parameter=path>\na.py\n</parameter>\n"
                  "</function>\n</tool_call><eos>")
        out, sampler, _ = run(script, tokenizer=ParserTokenizer("qwen3_coder"))
        self.assertEqual(text(out), script)
        self.assertEqual(sampler.calls, 1)

    def test_unknown_key_is_steered_to_a_declared_one(self):
        script = ("<tool_call>\n<function=read>\n<parameter=file_path>\na.py\n</parameter>\n"
                  "</function>\n</tool_call><eos>")
        out, sampler, _ = run(script, steps=300, tokenizer=ParserTokenizer("qwen3_coder"))
        body = call_body(out)
        self.assertNotIn("file_path", body)
        self.assertIn("<parameter=path>", body)
        self.assertIn(END, out)
        self.assertIsNone(sampler.matcher)
        if "qwen3_coder" in PARSERS:
            call = PARSERS["qwen3_coder"].parse_tool_call(body, TOOLS)
            self.assertEqual(call["name"], "read")
            self.assertLessEqual(set(call["arguments"]), {"path", "offset"})


class SpecialTokenTests(unittest.TestCase):
    class Tokenizer:
        def __init__(self, name):
            def parser():
                pass
            parser.__module__ = f"mlx_lm.tool_parsers.{name}"
            self.tool_parser = parser
            self.tool_call_start_tokens, self.tool_call_end_tokens = (48,), (49,)
            self.all_special_ids = {48, 49, 52}
            self.texts = {48: "<|tool_call>", 49: "<tool_call|>", 52: '<|"|>', 7: "a"}

        def decode(self, ids):
            return self.texts[ids[0]]

        def __len__(self):
            return 64

    def test_literal_special_tokens_map_to_their_text_only_for_their_format(self):
        gemma = grammar.TokenBytes(self.Tokenizer("gemma4"))
        self.assertEqual(gemma(52), b'<|"|>')
        self.assertIsNone(gemma(48))
        self.assertEqual(gemma(7), b"a")
        other = grammar.TokenBytes(self.Tokenizer("json_tools"))
        self.assertIsNone(other(52))


if __name__ == "__main__":
    unittest.main()
