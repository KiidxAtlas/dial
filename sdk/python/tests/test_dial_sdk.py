"""Tests for dial_sdk: schemas, argument checks, hook answers, the command line and config."""

import json
import os
import subprocess
import sys
import tempfile
import unittest
from typing import Dict, List, Literal, Optional

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
sys.path.insert(0, ROOT)

from dial_sdk import (  # noqa: E402
    Annotated,
    Block,
    Plugin,
    Reply,
    ToolError,
    schema_for,
)

EXAMPLE = os.path.join(ROOT, "examples", "word_tools.py")

try:
    import tomllib
except ImportError:  # Python < 3.11
    tomllib = None


def run(args, stdin=""):
    return subprocess.run(
        [sys.executable, EXAMPLE, *args],
        input=stdin,
        capture_output=True,
        text=True,
        timeout=60,
    )


class Schemas(unittest.TestCase):
    def test_type_hints_defaults_and_annotations_map_to_json_schema(self):
        def tool(
            text: Annotated[str, "The text"],
            limit: Annotated[int, {"minimum": 1}] = 10,
            ratio: float = 0.5,
            flag: bool = False,
            tags: List[str] = [],  # noqa: B006 - only read
            meta: Optional[Dict[str, int]] = None,
            mode: Literal["a", "b"] = "a",
        ):
            pass

        schema = schema_for(tool)
        self.assertEqual(schema["required"], ["text"])
        self.assertFalse(schema["additionalProperties"])
        props = schema["properties"]
        self.assertEqual(props["text"], {"type": "string", "description": "The text"})
        self.assertEqual(props["limit"], {"type": "integer", "minimum": 1, "default": 10})
        self.assertEqual(props["ratio"]["type"], "number")
        self.assertEqual(props["flag"]["type"], "boolean")
        self.assertEqual(props["tags"], {"type": "array", "items": {"type": "string"}, "default": []})
        self.assertEqual(props["meta"], {"type": "object"})
        self.assertEqual(props["mode"], {"enum": ["a", "b"], "default": "a"})

    def test_optional_without_default_is_not_required_and_varargs_are_refused(self):
        def tool(a: Optional[str], b: int):
            pass

        self.assertEqual(schema_for(tool)["required"], ["b"])

        def bad(*args):
            pass

        with self.assertRaises(TypeError):
            schema_for(bad)


class Tools(unittest.TestCase):
    def setUp(self):
        self.plugin = Plugin("t")

        @self.plugin.tool
        def add(a: int, b: int = 1) -> int:
            """Add two numbers."""
            return a + b

        @self.plugin.tool(name="fail")
        def failing() -> str:
            raise ToolError("nope")

    def test_call_checks_arguments_and_renders_results(self):
        self.assertEqual(self.plugin.call_tool("add", {"a": 2}), "3")
        self.assertEqual(self.plugin.tools["add"].description, "Add two numbers.")
        for arguments, message in [
            ({}, "missing required argument `a`"),
            ({"a": 1, "c": 2}, "unknown argument(s): c"),
            ({"a": "1"}, "`a` must be of type integer"),
            ({"a": True}, "`a` must be of type integer"),
            ([], "must be a JSON object"),
        ]:
            with self.assertRaises(ToolError) as caught:
                self.plugin.call_tool("add", arguments)
            self.assertIn(message, str(caught.exception))
        with self.assertRaises(ToolError):
            self.plugin.call_tool("missing", {})

    def test_registration_refuses_duplicates_bad_names_and_unknown_events(self):
        with self.assertRaises(ValueError):
            self.plugin.tool(name="add")(lambda: None)
        with self.assertRaises(ValueError):
            self.plugin.tool(name="bad name")(lambda: None)
        with self.assertRaises(ValueError):
            self.plugin.hook("on_save")
        with self.assertRaises(ValueError):
            Plugin("bad name")
        with self.assertRaises(ValueError):
            Plugin("p" * 40).tool(name="t" * 30)(lambda: None)


class Hooks(unittest.TestCase):
    def test_answers_map_to_dial_reply_keys_and_aliases_resolve(self):
        plugin = Plugin("h")
        plugin.hook("tool_call", name="none")(lambda event: None)
        plugin.hook("pre_tool_use", name="block")(lambda event: Block(f"no {event.tool}"))
        plugin.hook("pre_tool_use", name="rewrite")(
            lambda event: Reply(input={**event.input, "path": "safe.txt"})
        )
        plugin.hook("stop", name="notice")(lambda event: "done")
        plugin.hook("context", name="raw")(lambda event: {"messages": []})
        self.assertEqual(plugin.hooks["none"].event, "pre_tool_use")
        event = {"event": "pre_tool_use", "tool": "write", "input": {"path": "a"}}
        self.assertIsNone(plugin.run_hook("none", event))
        self.assertEqual(plugin.run_hook("block", event), {"block": "no write"})
        self.assertEqual(plugin.run_hook("rewrite", event), {"input": {"path": "safe.txt"}})
        self.assertEqual(plugin.run_hook("notice", {}), {"notice": "done"})
        self.assertEqual(plugin.run_hook("raw", {}), {"messages": []})


class CommandLine(unittest.TestCase):
    def test_tools_read_json_arguments_and_report_errors_on_stderr(self):
        result = run(["tool", "count"], json.dumps({"text": "a b a", "unique": True}))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout), {"unit": "words", "count": 2})
        result = run(["tool", "join"], json.dumps({"parts": ["x", "y"], "separator": "-"}))
        self.assertEqual((result.returncode, result.stdout), (0, "x-y"))
        result = run(["tool", "join"], json.dumps({"parts": []}))
        self.assertEqual(result.returncode, 1)
        self.assertEqual(result.stderr.strip(), "parts is empty")
        result = run(["tool", "count"], json.dumps({"text": "x", "unit": "pages"}))
        self.assertEqual(result.returncode, 1)
        self.assertIn("must be one of", result.stderr)

    def test_hooks_block_with_exit_2_and_answer_with_json(self):
        event = {"event": "pre_tool_use", "tool": "write", "input": {"path": "config/.env"}}
        result = run(["hook", "no_env_files"], json.dumps(event))
        self.assertEqual(result.returncode, 2)
        self.assertIn(".env files is not allowed", result.stderr)
        event["input"]["path"] = "src/main.rs"
        result = run(["hook", "no_env_files"], json.dumps(event))
        self.assertEqual((result.returncode, result.stdout), (0, ""))
        failed = {"event": "post_tool_use", "tool": "bash", "is_error": True, "output": "x"}
        result = run(["hook", "tag_failures"], json.dumps(failed))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("additional_context", json.loads(result.stdout))

    def test_usage_errors_exit_64(self):
        self.assertEqual(run(["serve"]).returncode, 64)

    @unittest.skipIf(tomllib is None, "tomllib needs Python 3.11+")
    def test_config_is_valid_toml_matching_the_manifest(self):
        result = run(["config"])
        self.assertEqual(result.returncode, 0, result.stderr)
        config = tomllib.loads(result.stdout)
        manifest = json.loads(run(["manifest"]).stdout)
        self.assertEqual(sorted(config["tools"]), ["words_count", "words_join"])
        count = config["tools"]["words_count"]
        self.assertEqual(count["program"], sys.executable)
        self.assertEqual(count["args"], [os.path.abspath(EXAMPLE), "tool", "count"])
        self.assertEqual(count["parameters"], manifest["tools"][0]["parameters"])
        hooks = config["hooks"]
        self.assertEqual([hook["event"] for hook in hooks], ["pre_tool_use", "post_tool_use"])
        self.assertEqual(hooks[0]["matcher"], "write|edit")
        self.assertEqual(hooks[0]["command"][-2:], ["hook", "no_env_files"])

    @unittest.skipIf(tomllib is None, "tomllib needs Python 3.11+")
    def test_toml_escapes_quotes_newlines_and_unicode(self):
        plugin = Plugin("esc")

        @plugin.tool(description='Say "hi"\n\tthen — leave')
        def odd(x: Annotated[str, "back\\slash"]) -> str:
            return x

        with tempfile.TemporaryDirectory() as directory:
            script = os.path.join(directory, 'my "plugin".py')
            config = tomllib.loads(plugin.config(script, "/usr/bin/python3"))
        tool = config["tools"]["esc_odd"]
        self.assertEqual(tool["description"], 'Say "hi"\n\tthen — leave')
        self.assertEqual(tool["args"][0], script)
        self.assertEqual(tool["parameters"]["properties"]["x"]["description"], "back\\slash")


if __name__ == "__main__":
    unittest.main()
