"""Default extension SDK tests: no models, services, pip or virtualenv needed."""
from contextlib import redirect_stderr
import io
import json
import os
import runpy
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

from dial_extensions import Extension, JsonState, Reply, ToolResult
from dial_extensions.__main__ import check, create

SDK = Path(__file__).resolve().parent / "dial_extensions"
SCHEMA = {"type": "object", "properties": {"text": {"type": "string"}}}


class ExtensionTests(unittest.TestCase):
    def test_tool_namespace_bounds_follow_escaped_actual_owner(self):
        cases = [
            ("a" * 117, "echo", True), ("a" * 118, "echo", False),
            ("a" + "_" * 58, "echo", True), ("a" + "." * 58 + "-", "echo", False),
            ("a" + "-" * 116, "echo", True),
            ("a" * 73, "t" * 48, True), ("a" * 74, "t" * 48, False),
        ]
        for owner, local, accepted in cases:
            for hosted in (False, True):
                with self.subTest(owner=owner, local=local, hosted=hosted), patch.dict(os.environ, {}, clear=True):
                    if hosted:
                        os.environ["DIAL_EXTENSION_NAME"] = owner
                    extension = Extension("declared-name" if hosted else owner)
                    if accepted:
                        extension.tool(local, "Tool", SCHEMA)(lambda arguments, workspace: "ok")
                        self.assertEqual(extension.dispatch("initialize", {})["tools"][0]["name"], local)
                    else:
                        with self.assertRaisesRegex(ValueError, "namespaced tool name exceeds 128"):
                            extension.tool(local, "Tool", SCHEMA)
        with patch.dict(os.environ, {"DIAL_EXTENSION_NAME": "short"}):
            Extension("a" * 128).tool("echo", "Tool", SCHEMA)(lambda arguments, workspace: "ok")
        for name in ("", "_bad", "a/b", "é", "a" * 129):
            with self.subTest(invalid=name), patch.dict(os.environ, {}, clear=True):
                with self.assertRaisesRegex(ValueError, "invalid extension name"):
                    Extension(name)
                os.environ["DIAL_EXTENSION_NAME"] = name
                with self.assertRaisesRegex(ValueError, "invalid extension name"):
                    Extension("valid")

    def test_scaffold_namespace_bound_fails_before_files_and_long_command_only_check_passes(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            for index, name in enumerate(("a" * 118, "a" + "_" * 59)):
                directory = root / str(index)
                with self.subTest(name=name), self.assertRaisesRegex(ValueError, "namespaced tool name exceeds 128"):
                    create(directory, name)
                self.assertFalse(directory.exists())
            directory = root / "valid"
            create(directory, "a" * 117)
            check(directory)
            manifest_path = directory / "dial-plugin.json"
            manifest = json.loads(manifest_path.read_text())
            manifest["name"] = "a" * 128
            manifest_path.write_text(json.dumps(manifest))
            (directory / "extension.py").write_text(
                'from dial_extensions import Extension\nextension = Extension("' + manifest["name"] + '")\n'
                '@extension.command("hello")\ndef hello(args): return "hi"\n')
            self.assertEqual(check(directory)["name"], manifest["name"])
            with patch.dict(os.environ, {}, clear=True):
                extension = Extension(manifest["name"])
                extension.command("hello")(lambda args: "hi")
                self.assertEqual(extension.dispatch("initialize", {})["commands"], ["hello"])

    def test_static_check_rejects_unknown_fields_and_requirement_injection(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary) / "example"
            create(root, "example")
            path = root / "dial-plugin.json"
            baseline = json.loads(path.read_text())
            cases = [
                {"unknown": True}, {"interpreter": "-c"}, {"interpreter": "python\n"},
                {"dependencies": ["--target=/tmp"]}, {"dependencies": [" --editable"]},
                {"dependencies": ["package\nother"]}, {"dependencies": ["package\tother"]},
                {"dependencies": ["x"] * 257}, {"dependencies": ["x" * 4097]},
            ]
            for changed in cases:
                with self.subTest(changed=changed):
                    manifest = dict(baseline, python=dict(baseline["python"], **changed))
                    path.write_text(json.dumps(manifest))
                    with self.assertRaises(ValueError):
                        check(root)

    def test_launcher_rejects_unsupported_python_with_clear_diagnosis(self):
        from dial_extensions._launcher import main
        with patch.object(sys, "version_info", (3, 10)):
            with self.assertRaisesRegex(SystemExit, "require Python 3.11 or newer"):
                main()

    def exchange(self, extension, request):
        output, errors = io.StringIO(), io.StringIO()
        raw = request if isinstance(request, str) else json.dumps(request)
        with redirect_stderr(errors):
            extension.serve(stdin=io.StringIO(raw), stdout=output)
        return json.loads(output.getvalue()) if output.getvalue() else None, errors.getvalue()

    def request(self, method, params=None):
        return dict(jsonrpc="2.0", id=7, method=method, params=params or {})

    def test_negotiation_and_all_callback_routes(self):
        extension = Extension("demo", capabilities=["ui.status"])
        extension.command("hello")(lambda args: "Hello " + args)
        extension.on("turn_start")(lambda payload: Reply().ui("status", [payload["prompt"]]))
        extension.on("user_prompt_submit")(lambda payload: Reply().rewrite(prompt=payload["prompt"].upper()))
        extension.panel_close("hello")(lambda outcome, generation: f"{outcome['status']}:{generation}")
        extension.autocomplete(lambda query: [{"value": query + " world"}])
        @extension.tool("echo", "Echo", SCHEMA, revision=3)
        async def echo(arguments, workspace):
            return ToolResult(workspace + ":" + arguments["text"], True)
        initialized = extension.dispatch("initialize", {})
        self.assertEqual(initialized["protocolVersion"], "2")
        self.assertEqual(initialized["capabilities"], ["autocomplete", "ui.status"])
        self.assertEqual(initialized["commands"], ["hello"])
        self.assertEqual(initialized["events"], ["turn_start", "user_prompt_submit"])
        self.assertEqual(initialized["tools"], [dict(name="echo", description="Echo", parameters=SCHEMA, revision=3)])
        cases = [
            ("command", dict(name="hello", args="Ada"), {"text": "Hello Ada"}),
            ("event", dict(event="turn_start", payload={"prompt": "Hi"}), {"actions": [dict(type="ui", slot="status", lines=["Hi"])]}),
            ("event", dict(event="user_prompt_submit", payload={"prompt": "Hi"}), {"rewrite": {"prompt": "HI"}}),
            ("command.panel", dict(name="hello", outcome={"status": "cancelled"}, generation=4), {"text": "cancelled:4"}),
            ("autocomplete", dict(query="hello"), {"items": [{"value": "hello world"}]}),
            ("tool.call", dict(name="echo", arguments={"text": "Hi"}, workspace="/work"), {"text": "/work:Hi", "is_error": True}),
        ]
        for method, params, expected in cases:
            with self.subTest(method=method, params=params):
                response, _ = self.exchange(extension, self.request(method, params))
                self.assertEqual(response, dict(jsonrpc="2.0", id=7, result=expected))

    def test_protocol_errors_have_no_partial_or_sensitive_output(self):
        extension = Extension("demo")
        def broken(args):
            print("callback diagnostic")
            raise RuntimeError("private exception content")
        extension.command("broken")(broken)
        cases = [
            ("{", -32700), ("NaN", -32700), ("[]", -32600),
            ({"method": "initialize", "id": 2}, -32600),
            (dict(jsonrpc="2.0", method="initialize", id=[]), -32600),
            (self.request("missing"), -32601),
            (self.request("command", dict(name="unknown", args="")), -32601),
            (self.request("command", dict(name="broken", args=4)), -32602),
            (self.request("initialize", []), None),
            (dict(jsonrpc="2.0", id=7, method="initialize", params=[]), -32602),
            (self.request("command", dict(name="broken", args="")), -32603),
        ]
        for request, code in cases:
            with self.subTest(request=request):
                response, errors = self.exchange(extension, request)
                if code is not None:
                    self.assertEqual(response["error"]["code"], code)
                    self.assertNotIn("private exception content", json.dumps(response))
                else:
                    self.assertIn("result", response)
        response, errors = self.exchange(extension, self.request("command", dict(name="broken", args="")))
        self.assertIn("callback diagnostic", errors)
        self.assertIn("private exception content", errors)

    def test_notification_no_response_and_serialization_failures_are_atomic(self):
        extension = Extension("demo")
        extension.command("bad")(lambda args: {"value": float("nan")})
        response, _ = self.exchange(extension, self.request("command", dict(name="bad", args="")))
        self.assertEqual(response["error"]["code"], -32603)
        response, _ = self.exchange(extension, dict(jsonrpc="2.0", method="initialize"))
        self.assertIsNone(response)

    def test_action_and_panel_helpers_preserve_protocol_shapes(self):
        reply = (Reply().text("ready").panel("Title", [{"type": "button", "id": "ok", "label": "OK"}], 9)
                 .ui("footer", []).send_user_message("question").append_entry("note", "body", {"title": "Note"})
                 .set_model("provider/model", "high").set_thinking("low")
                 .register_provider("local", {"base_url": "http://localhost"}).unregister_provider("local"))
        self.assertEqual(reply["panel"]["generation"], 9)
        self.assertEqual(reply["actions"], [
            dict(type="ui", slot="footer", lines=[]), dict(type="send_user_message", text="question"),
            dict(type="append_entry", kind="note", text="body", renderer={"title": "Note"}),
            dict(type="set_model", model="provider/model", effort="high"), dict(type="set_thinking", effort="low"),
            dict(type="register_provider", name="local", config={"base_url": "http://localhost"}),
            dict(type="unregister_provider", name="local"),
        ])
        self.assertEqual(Reply().block("No"), {"rewrite": {"block": "No"}})

    def test_registration_refuses_ambiguous_or_invalid_contracts(self):
        extension = Extension("demo")
        extension.command("hello")(lambda args: None)
        with self.assertRaises(ValueError):
            extension.command("hello")(lambda args: None)
        for name in ("", "/hello", "two words", "x" * 49, "nonascii-é"):
            with self.subTest(command=name), self.assertRaises(ValueError):
                extension.command(name)
        for revision in (0, -1, True, 1.5):
            with self.subTest(revision=revision), self.assertRaises(ValueError):
                extension.tool("echo", "Echo", SCHEMA, revision=revision)
        for index in range(64):
            extension.tool("tool" + str(index), "Tool", SCHEMA)(lambda args, workspace: "ok")
        with self.assertRaises(ValueError):
            extension.tool("overflow", "Tool", SCHEMA)(lambda args, workspace: "ok")

    def test_settings_and_state_are_explicit_atomic_and_survive_instances(self):
        with tempfile.TemporaryDirectory() as temporary, patch.dict(os.environ, {
            "DIAL_EXTENSION_SETTINGS": '{"greeting":"Hi"}', "DIAL_EXTENSION_ROOT": temporary,
            "DIAL_EXTENSION_STATE_DIR": temporary,
        }):
            extension = Extension("demo")
            self.assertEqual(extension.settings, {"greeting": "Hi"})
            self.assertEqual(extension.root, Path(temporary))
            self.assertEqual(extension.state.load(default={"count": 0}), {"count": 0})
            extension.state.save({"count": 1, "text": "é"})
            self.assertEqual(Extension("fresh-process").state.load(), {"count": 1, "text": "é"})
            with self.assertRaises(ValueError):
                extension.state.save(float("nan"))
            self.assertEqual(extension.state.load()["count"], 1)
            self.assertEqual(sorted(path.name for path in Path(temporary).iterdir()), ["state.json"])
            for name in ("../escape", "a/b", "", ".", "x" * 65):
                with self.subTest(name=name), self.assertRaises(ValueError):
                    extension.state.save({}, name)
            (Path(temporary) / "state.json").write_text("{", encoding="utf-8")
            with self.assertRaises(json.JSONDecodeError):
                extension.state.load(default={})
            extension.state.delete()
            self.assertIsNone(extension.state.load())
        with patch.dict(os.environ, {}, clear=True):
            with self.assertRaises(RuntimeError):
                JsonState()
            self.assertEqual(Extension("demo").settings, {})
        with patch.dict(os.environ, {"DIAL_EXTENSION_SETTINGS": "[]"}):
            with self.assertRaises(ValueError):
                Extension("demo").settings

    def managed(self, root, entry, request):
        runtime = root / ".dial-python"
        if not runtime.exists():
            runtime.mkdir()
            shutil.copytree(SDK, runtime / "sdk" / "dial_extensions", ignore=shutil.ignore_patterns("__pycache__"))
            shutil.copyfile(SDK / "_launcher.py", runtime / "launcher.py")
        environment = dict(os.environ, DIAL_EXTENSION_ROOT=str(root), DIAL_EXTENSION_STATE_DIR=str(root / "state"), DIAL_EXTENSION_SETTINGS='{"greeting":"Hi"}')
        result = subprocess.run([sys.executable, "-I", str(runtime / "launcher.py"), str(entry)],
                                input=json.dumps(request), text=True, capture_output=True, env=environment, timeout=10)
        self.assertEqual(result.returncode, 0, result.stderr)
        return json.loads(result.stdout), result.stderr

    def test_managed_launcher_redirects_import_callback_and_async_prints(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "helper.py").write_text('print("import diagnostic")\n', encoding="utf-8")
            entry = root / "extension.py"
            entry.write_text('''import helper
from dial_extensions import Extension
extension = Extension("demo")
@extension.command("hello")
async def hello(args):
    print("callback diagnostic")
    return "Hi " + args
extension.serve()
''', encoding="utf-8")
            response, errors = self.managed(root, entry, self.request("command", dict(name="hello", args="Ada")))
            self.assertEqual(response["result"], {"text": "Hi Ada"})
            self.assertIn("import diagnostic", errors)
            self.assertIn("callback diagnostic", errors)

    def test_scaffold_static_check_and_cross_process_state_workflow(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary) / "example"
            create(root, "example")
            self.assertEqual(check(root)["name"], "example")
            with self.assertRaises(FileExistsError):
                create(root, "example")
            entry = root / "extension.py"
            initialized, _ = self.managed(root, entry, self.request("initialize"))
            self.assertEqual(initialized["result"]["tools"][0]["name"], "echo")
            for generation in (1, 2):
                response, _ = self.managed(root, entry, self.request("command", dict(name="hello", args="Ada")))
                self.assertEqual(response["result"]["panel"]["generation"], generation)
            response, _ = self.managed(root, entry, self.request("command.panel", dict(name="hello", generation=2,
                outcome=dict(status="submitted", button="save", values={"name": "Ada"}))))
            self.assertEqual(response["result"]["text"], "Hello, Ada")
            response, _ = self.managed(root, entry, self.request("tool.call", dict(name="echo", arguments={"text": "ok"}, workspace=str(root))))
            self.assertEqual(response["result"], {"text": "ok", "is_error": False})
            entry.write_text('raise RuntimeError("must not execute")', encoding="utf-8")
            check(root)
            manifest = json.loads((root / "dial-plugin.json").read_text())
            manifest["python"]["entry"] = "../outside.py"
            (root / "dial-plugin.json").write_text(json.dumps(manifest))
            with self.assertRaises(ValueError):
                check(root)

    def test_framed_worker_preserves_state_and_roundtrips_host_callbacks(self):
        extension = Extension("demo", capabilities=["host.tools"])
        visits = []
        @extension.command("count")
        def count(args):
            result = extension.host.tool("read", {"path": args})
            visits.append(args)
            return dict(text=result["text"], count=len(visits))
        frames = []
        for operation, callback in ((7, 1), (8, 2)):
            frames.append(dict(jsonrpc="2.0", id=operation, method="command", params=dict(name="count", args="file")))
            frames.append(dict(operation_id=operation, callback_id=callback, result=dict(text="contents", is_error=False)))
        frames.append(dict(jsonrpc="2.0", id=9, method="shutdown", params={}))
        source = io.StringIO("".join(json.dumps(frame) + "\n" for frame in frames))
        output = io.StringIO()
        with patch.dict(os.environ, {"DIAL_EXTENSION_RUNTIME": "worker"}):
            extension.serve(stdin=source, stdout=output)
        replies = [json.loads(line) for line in output.getvalue().splitlines()]
        self.assertEqual([replies[index]["callback_id"] for index in (0, 2)], [1, 2])
        self.assertEqual(replies[0]["callback"]["method"], "tools.call")
        self.assertEqual([replies[index]["result"]["response"]["result"]["count"] for index in (1, 3)], [1, 2])
        self.assertEqual(replies[4]["result"]["response"]["result"], {"shutdown": True})

    def test_host_binding_refuses_mismatched_responses_and_fake_approvals(self):
        from dial_extensions import HostError
        from dial_extensions.testing import FakeHost
        extension = Extension("demo")
        extension.command("call")(lambda args: extension.host.tool("write", {"path": args}))
        host = FakeHost().handle("tools.call", lambda params: {"text": "written", "is_error": False})
        with host.attach(extension):
            with self.assertRaisesRegex(HostError, "approval declined"):
                extension.dispatch("command", dict(name="call", args="file"))
        host.handle("tools.call", lambda params: {"text": "written", "is_error": False}, approved=True)
        with host.attach(extension):
            self.assertEqual(extension.dispatch("command", dict(name="call", args="file"))["text"], "written")
        request = self.request("command", dict(name="call", args="file"))
        wrong = dict(operation_id=999, callback_id=1, result={})
        output = io.StringIO()
        with patch.dict(os.environ, {"DIAL_EXTENSION_RUNTIME": "request"}), redirect_stderr(io.StringIO()):
            extension.serve(stdin=io.StringIO(json.dumps(request) + "\n" + json.dumps(wrong) + "\n"), stdout=output)
        response = json.loads(output.getvalue().splitlines()[-1])["result"]["response"]
        self.assertEqual(response["error"]["code"], -32603)
        with self.assertRaisesRegex(HostError, "active framed callback"):
            extension.host.tools()

    def test_contributions_are_frozen_and_route_view_context_and_custom_data(self):
        extension = Extension("demo")
        schema = {"type": "object", "properties": {"enabled": {"type": "boolean", "default": True}}}
        extension.settings_schema(schema)
        schema["properties"].clear()
        extension.view("dashboard", "Dashboard", elements=[{"type": "text", "text": "Ready"}])(lambda event: event["type"])
        extension.context_source("guide", "Guide")(lambda query: "Guide for " + query)
        extension.agent_profile("reviewer", "Reviewer", tools=["read"], guidance="Review only")
        extension.workflow("review", "Review", [{"id": "confirm", "title": "Confirm", "kind": "approval", "prompt": "Proceed?"}])
        extension.renderer("summary", "Summary", fields=[{"label": "Count", "path": "/count"}])
        extension.preset("strict", "Strict", {"enabled": True}).keybinding("alt+r", "review")
        extension.tool("data", "Data", SCHEMA)(lambda args, workspace: ToolResult("one", data={"count": 1}, renderer="summary"))
        initialized = extension.dispatch("initialize", {})
        self.assertTrue(initialized["contributions"]["settings"]["properties"]["enabled"]["default"])
        self.assertEqual(extension.dispatch("view.event", {"name": "dashboard", "event": {"type": "click"}}), {"text": "click"})
        self.assertEqual(extension.dispatch("context.get", {"name": "guide", "query": "tests"}), {"text": "Guide for tests"})
        self.assertEqual(extension.dispatch("tool.call", {"name": "data", "arguments": {}, "workspace": "/tmp"}),
                         {"text": "one", "is_error": False, "data": {"count": 1}, "renderer": "summary"})
        with self.assertRaisesRegex(ValueError, "duplicate"):
            extension.register_view("dashboard", "Duplicate", [])

    def test_polled_timers_debounce_watches_and_queue_display_updates(self):
        from dial_extensions.testing import FakeHost
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            entry = root / "file.py"
            entry.write_text("a")
            extension = Extension("demo")
            clock = [0.0]
            extension._clock = lambda: clock[0]
            ticks, changes = [], []
            extension.every(1)(lambda: ticks.append(clock[0]))
            extension.watch(["*.py"], debounce=0.25)(lambda paths: changes.append(paths))
            extension.dispatch("poll", {"workspace": temporary})
            entry.write_text("longer")
            clock[0] = 0.5
            extension.dispatch("poll", {"workspace": temporary})
            self.assertEqual(changes, [])
            clock[0] = 1.0
            extension.dispatch("poll", {"workspace": temporary})
            self.assertEqual(ticks, [1.0])
            self.assertEqual(changes, [["file.py"]])
            extension.publish("progress", {"text": "Ready"})
            host = FakeHost().handle("progress", lambda params: {})
            with host.attach(extension):
                extension.dispatch("poll", {"workspace": temporary})
            self.assertEqual(host.calls, [("progress", {"text": "Ready"})])
            with self.assertRaises(ValueError):
                extension.publish("tools.call", {"name": "write"})

    def test_static_runtime_compatibility_and_lockfile_boundaries(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary) / "demo"
            create(root, "demo")
            path = root / "dial-plugin.json"
            baseline = json.loads(path.read_text())
            for changes in ({"apiVersion": 2}, {"apiVersion": True}, {"python": dict(baseline["python"], runtime="unknown")},
                            {"python": dict(baseline["python"], lockfile="../outside")}):
                path.write_text(json.dumps(dict(baseline, **changes)))
                with self.subTest(changes=changes), self.assertRaises(ValueError):
                    check(root)
            manifest = dict(baseline, apiVersion={"min": 1, "max": 2},
                            python=dict(baseline["python"], runtime="worker", lockfile="requirements.lock"))
            path.write_text(json.dumps(manifest))
            (root / "requirements.lock").write_text("# no dependencies\n")
            self.assertEqual(check(root)["python"]["runtime"], "worker")

    def test_examples_initialize_without_effects_and_execute_explicit_commands(self):
        from dial_extensions.testing import FakeHost
        examples = Path(__file__).resolve().parent / "examples"
        cases = [
            ("pr-review", "pr-review", "", {"max_diff_chars": 12000}, "git"),
            ("build-dashboard", "build-dashboard", "test", {"build_command": "cargo check --locked", "test_command": "cargo test --locked --lib"}, "bash"),
            ("workflow-pack", "personal-review", "", {"focus": "security"}, None),
        ]
        for owner, command, args, settings, tool in cases:
            with self.subTest(owner=owner):
                manifest = check(examples / owner)
                self.assertEqual(manifest["python"]["runtime"], "worker")
                namespace = runpy.run_path(str(examples / owner / "extension.py"))
                extension = namespace["extension"]
                initialized = extension.dispatch("initialize", {})
                self.assertIn(command, initialized["commands"])
                host = FakeHost()
                host.handle("settings.get", lambda params: settings)
                host.handle("tools.call", lambda params: {"text": "diff --git a/a b/a\n+new", "is_error": False}, approved=True)
                host.handle("models.call", lambda params: {"text": "No concrete regression found."}, approved=True)
                host.handle("progress", lambda params: {})
                host.handle("views.update", lambda params: {"revision": params["revision"] + 1})
                host.handle("workflows.run", lambda params: {"steps": [{"id": "confirm", "status": "approved"}]}, approved=True)
                with host.attach(extension):
                    reply = extension.dispatch("command", {"name": command, "args": args})
                if tool is not None:
                    actual = next(params for method, params in host.calls if method == "tools.call")
                    self.assertEqual(actual["name"], tool)
                    self.assertTrue(any(method == "views.update" for method, params in host.calls))
                else:
                    self.assertEqual(reply["steps"][0]["status"], "approved")
                    self.assertEqual(host.calls, [("workflows.run", {"id": "review", "arguments": {}})])


if __name__ == "__main__":
    unittest.main()
