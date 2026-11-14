"""Offline dashboard contracts; real subprocess fixtures, no models or credentials."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import time
import unittest
import urllib.error
import urllib.request
import xml.etree.ElementTree as ET
from unittest.mock import patch

import benchmark_dashboard as dashboard
import cross_agent


class DashboardTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        tools = self.root / "examples/benchmarks"
        tools.mkdir(parents=True)
        executable = json.dumps(sys.executable)
        (tools / "cross-agent.toml").write_text(f'''[[agents]]
name = "fixture"
output = "text"
first = [{executable}, "-c", "print('4')", "{{prompt}}"]
follow_up = [{executable}, "-c", "print('4')", "{{prompt}}"]
''')
        (tools / "tiny.toml").write_text('''version = 1
name = "tiny"
[[cases]]
id = "answer"
prompt = "two plus two"
[[cases.checks]]
kind = "response_equals"
content = "4"
''')
        self.engine = dashboard.Benchmarks(self.root)
        def stop_engine():
            self.engine.stop()
            if self.engine.thread is not None:
                self.engine.thread.join(10)
        self.addCleanup(stop_engine)
        self.request = {"agents": ["fixture"], "suites": ["tiny"], "model": "fixture-no-model",
                        "repetitions": 2, "timeout": 2, "confirm": True}

    def finished(self):
        self.engine.thread.join(10)
        self.assertFalse(self.engine.thread.is_alive())
        return self.engine.snapshot()

    def test_real_trials_publish_live_snapshots_and_final_image(self):
        start = self.engine.start(self.request)
        self.assertEqual(start["total"], 2)
        state = self.finished()
        self.assertEqual(state["status"], "completed")
        self.assertEqual(state["summary"]["fixture"]["passed_trials"], 2)
        self.assertIsNone(state["summary"]["fixture"]["median_input_total_tokens"])
        output = Path(state["output"])
        self.assertEqual(json.loads((output / "progress.json").read_text())["completed"], 2)
        self.assertEqual(json.loads((output / "progress.json").read_text())["status"], "completed")
        self.assertEqual(json.loads((output / "report.json").read_text())["status"], "completed")
        ET.fromstring((output / "comparison.svg").read_text())
        self.assertIn("unknown", (output / "comparison.svg").read_text())
        self.assertFalse((output / "work").exists())
        self.assertEqual(output.stat().st_mode & 0o777, 0o700)
        self.assertEqual(len(state["identities"]["fixture"]["binary_sha256"]), 64)

    def test_validation_does_not_start_work_or_overwrite_runs(self):
        for update in ({"confirm": False}, {"agents": ["missing"]}, {"suites": ["../tiny"]},
                       {"repetitions": True}, {"timeout": 0}, {"model": ""}, {"extra": 1},
                       {"suites": ["tiny", "tiny"]}):
            with self.subTest(update=update), self.assertRaises(dashboard.InvalidRequest):
                self.engine.start({**self.request, **update})
        self.assertFalse((self.root / "benchmarks").exists())
        self.engine.start(self.request)
        first = self.finished()["output"]
        self.engine.start(self.request)
        self.assertNotEqual(first, self.finished()["output"])
        self.assertTrue(Path(first, "report.json").exists())

    def test_busy_cancellation_stops_real_agent_process(self):
        definitions = self.engine.definitions()
        definitions[0]["first"] = [sys.executable, "-c", "import time; time.sleep(60)", "{prompt}"]
        with patch.object(self.engine, "definitions", return_value=definitions):
            self.engine.start(self.request)
            with self.assertRaises(dashboard.Busy):
                self.engine.start(self.request)
            self.engine.stop()
            state = self.finished()
        self.assertEqual(state["status"], "cancelled")
        self.assertLess(state["completed"], state["total"])
        self.assertNotEqual(state["status"], "completed")

    def test_svg_escapes_untrusted_labels_and_preserves_unknowns(self):
        svg = dashboard.comparison_svg({"status": "cancelled", "agents": ["<script>&"], "model": '"<&',
                                        "suites": ["&" * 200], "summary": {}})
        ET.fromstring(svg)
        self.assertNotIn("<script>", svg)
        self.assertIn("unknown", svg)
        self.assertIn("not scored", svg)
        self.assertIn("&amp;" * 150, svg)

    def test_clean_dial_environment_drops_ambient_overrides(self):
        agent = {"env": {"DIAL_HOME": "{trial_root}/dial-home",
                          "DIAL_SUBSCRIPTION_AUTH_HOME": "/explicit/auth"}}
        with patch.dict(os.environ, {"DIAL_EXPERIMENTAL_TURN_OPTIMIZER": "true", "DIAL_HOME": "/wrong"}):
            environment = cross_agent.agent_environment(agent, {"trial_root": str(self.root)})
        self.assertNotIn("DIAL_EXPERIMENTAL_TURN_OPTIMIZER", environment)
        self.assertEqual(environment["DIAL_HOME"], str(self.root / "dial-home"))
        self.assertEqual(environment["DIAL_SUBSCRIPTION_AUTH_HOME"], "/explicit/auth")

    def test_model_setup_error_disables_agent_but_tool_output_does_not(self):
        # A rejected model ID is discovered by the agent CLI, not guessed from the name.
        rejected = [sys.executable, "-c",
            "import sys; print('Error: model claude-sonnet-4.6 was not found', file=sys.stderr); sys.exit(1)",
            "{prompt}"]
        # An agent that already ran tools got past setup, even if its stderr echoes setup-like text.
        worked = [sys.executable, "-c",
            "import json, sys; print(json.dumps(dict(type='tool_execution_end', toolName='bash', isError=False)));"
            "print('grep: unknown option -- z', file=sys.stderr); sys.exit(1)",
            "{prompt}"]
        for command, output, disabled in ((rejected, "text", True), (worked, "omp-json", False)):
            with self.subTest(disabled=disabled):
                agents = self.engine.definitions()
                agents[0].update(first=command, output=output)
                with patch.object(self.engine, "definitions", return_value=agents):
                    self.engine.start({**self.request, "repetitions": 3})
                    state = self.finished()
                self.assertEqual(state["trials"][0]["error_kind"], "model" if disabled else "startup")
                self.assertEqual(state["completed"], 1 if disabled else 3)
                self.assertEqual(len(state["skipped_trials"]), 2 if disabled else 0)

    def test_runner_crash_keeps_the_traceback_privately(self):
        with patch.object(cross_agent, "trial_plan", side_effect=RuntimeError("fixture crash detail")):
            self.engine.start(self.request)
            state = self.finished()
        self.assertEqual(state["status"], "failed")
        self.assertIn("RuntimeError", state["error"])
        self.assertNotIn("fixture crash detail", state["error"])
        saved = Path(state["output"], "logs", "runner-error.txt").read_text()
        self.assertIn("fixture crash detail", saved)
        self.assertIn("Traceback", saved)

    def test_fatal_configuration_error_is_visible_and_not_repeated(self):
        agents = self.engine.definitions()
        agents[0]["first"] = [sys.executable, "-c",
            "import sys; print('No API key found for anthropic. token=sk-fixture-secret-value', file=sys.stderr); sys.exit(2)",
            "{prompt}"]
        with patch.object(self.engine, "definitions", return_value=agents):
            self.engine.start({**self.request, "repetitions": 3})
            state = self.finished()
        self.assertEqual(state["status"], "invalid")
        self.assertEqual(state["completed"], 1)
        self.assertEqual(len(state["skipped_trials"]), 2)
        self.assertEqual(state["trials"][0]["error_kind"], "authentication")
        self.assertIn("fixture", state["error"])
        self.assertNotIn("sk-fixture-secret-value", state["error"])
        self.assertIn("No API key", state["trials"][0]["turns"][0]["stderr_tail"])

    def test_installed_cli_credential_failures_disable_the_agent(self):
        # Claude Code reports auth failures in its stdout result event, not on stderr.
        claude = ("import json,sys; print(json.dumps(dict(type='result', is_error=True, "
                  "result='Failed to authenticate: OAuth session expired and could not be refreshed'))); sys.exit(1)")
        hermes = ("import sys; print(\"hermes -z: agent failed: No Anthropic credentials found. "
                  "Set ANTHROPIC_TOKEN or ANTHROPIC_API_KEY\", file=sys.stderr); sys.exit(1)")
        for script, output, detail in ((claude, "claude-json", "OAuth session expired"),
                                       (hermes, "text", "No Anthropic credentials")):
            with self.subTest(output=output):
                agents = self.engine.definitions()
                agents[0].update(first=[sys.executable, "-c", script, "{prompt}"], output=output)
                with patch.object(self.engine, "definitions", return_value=agents):
                    self.engine.start({**self.request, "repetitions": 3})
                    state = self.finished()
                self.assertEqual(state["trials"][0]["error_kind"], "authentication")
                self.assertEqual(state["completed"], 1)
                self.assertEqual(len(state["skipped_trials"]), 2)
                self.assertIn(detail, state["error"])

    def test_measurements_include_cache_and_never_turn_missing_usage_into_zero(self):
        usage = {**cross_agent.empty_usage(), "input_total": 100, "uncached_input": 30,
                 "cache_read": 60, "cache_write": 10, "output": 20, "requests": 2,
                 "tool_calls": 3, "tools": {"read": 3}}
        trial = {"agent": "fixture", "case": "case", "scored": True, "passed": True,
                 "agent_error": None, "elapsed_ms": 1000, "usage": usage}
        unknown = {**trial, "usage": {**cross_agent.empty_usage(), "usage_complete": False}}
        summary, _ = cross_agent.summarize([trial, unknown], [{"name": "fixture"}])
        row = summary["fixture"]
        self.assertEqual(row["total_elapsed_ms_all"], 2000)
        for field, total in (("input_total", 100), ("cache_read", 60), ("cache_write", 10),
                             ("output", 20), ("requests", 2), ("tool_calls", 3)):
            with self.subTest(field=field):
                self.assertEqual(row["measurements"][field],
                                 {"total": total, "median": total, "known_trials": 1, "trials": 2})
        missing, _ = cross_agent.summarize([unknown], [{"name": "fixture"}])
        self.assertIsNone(missing["fixture"]["measurements"]["tool_calls"]["total"])
        svg = dashboard.comparison_svg({"status": "completed", "agents": ["fixture"], "summary": summary})
        ET.fromstring(svg)
        self.assertIn("Cache read", svg)
        self.assertIn("Requests", svg)

    def test_agent_templates_translate_thinking_and_continue_in_trial_workspace(self):
        agents = {a["name"]: a for a in cross_agent.load_agents(Path(__file__).with_name("cross-agent.toml"))}
        self.assertTrue({"dial", "omp", "claude-code", "hermes"} <= agents.keys())
        for level in ("off", "low", "medium", "high"):
            values = {"thinking": level, "model": "claude-sonnet-4-6", "prompt": "a {literal} prompt",
                      "timeout": "3", "usage_file": "/trial/usage.json"}
            for name in ("claude-code", "hermes"):
                with self.subTest(agent=name, thinking=level):
                    agent = agents[name]
                    mapped = cross_agent.agent_values(agent, values)
                    first = cross_agent.render(agent["first"], mapped)
                    follow = cross_agent.render(agent["follow_up"], mapped)
                    self.assertEqual(first[-1], values["prompt"])
                    self.assertEqual(follow[-1], values["prompt"])
                    if name == "claude-code":
                        self.assertIn("-c", follow)
                        self.assertEqual(json.loads(first[first.index("--settings") + 1]),
                                         {"alwaysThinkingEnabled": level != "off"})
                    else:
                        self.assertEqual(follow[follow.index("--resume") + 1], "latest")
                        self.assertEqual(first[first.index("--reasoning") + 1], "none" if level == "off" else level)
        with self.assertRaises(ValueError):
            cross_agent.agent_values(agents["hermes"], {"thinking": "unsupported"})

    def test_hermes_usage_sidecars_preserve_known_tokens_without_inventing_tool_counts(self):
        script = ("import json,sys; from pathlib import Path; "
                  "Path(sys.argv[1]).write_text(json.dumps(dict(api_calls=2,input_tokens=30,cache_read_tokens=60,"
                  "cache_write_tokens=10,output_tokens=20,provider='anthropic',model='fixture'))); print('4')")
        command = [sys.executable, "-c", script, "{usage_file}", "{prompt}"]
        agent = {"name": "hermes-fixture", "first": command, "follow_up": command, "output": "hermes",
                 "env": {}, "delegation_tools": ["delegate_task"], "delegated_usage": "unknown"}
        case = {"id": "multi-turn", "prompt": "answer", "turns": [{"prompt": "again"}],
                "checks": [{"kind": "response_equals", "content": "4"}]}
        trial = cross_agent.run_trial(agent, case, self.root, 0, self.root,
                                     {"model": "fixture"}, 2, False, self.root / "logs")
        self.assertTrue(trial["passed"])
        self.assertFalse(trial["usage"]["usage_complete"])
        self.assertEqual(trial["usage"]["input_total"], 200)
        self.assertEqual(trial["usage"]["requests"], 4)
        summary, _ = cross_agent.summarize([trial], [agent])
        self.assertEqual(summary[agent["name"]]["measurements"]["input_total"]["total"], 200)
        self.assertIsNone(summary[agent["name"]]["measurements"]["tool_calls"]["total"])
        self.assertIsNone(summary[agent["name"]]["measurements"]["tool_errors"]["total"])
        self.assertTrue((self.root / "logs/hermes-fixture-multi-turn-r0/turn-1.usage.json").is_file())

    def test_ordinary_execution_failures_do_not_disable_later_trials(self):
        agents = self.engine.definitions()
        agents[0]["first"] = [sys.executable, "-c", "import sys; sys.exit(1)", "{prompt}"]
        with patch.object(self.engine, "definitions", return_value=agents):
            self.engine.start(self.request)
            state = self.finished()
        self.assertEqual(state["completed"], 2)
        self.assertEqual(state["skipped_trials"], [])
        self.assertEqual(state["trials"][0]["error_kind"], "execution")

    def test_http_capability_origin_gate_and_live_state(self):
        process = subprocess.Popen([sys.executable, str(Path(dashboard.__file__)), "--root", str(self.root)],
                                   stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        try:
            url = json.loads(process.stdout.readline())["url"]
            def request(path, data=None, headers=None):
                body = None if data is None else json.dumps(data).encode()
                return urllib.request.urlopen(urllib.request.Request(url + path, data=body,
                    headers=headers or {}), timeout=5)
            with request("api/catalog") as response:
                self.assertEqual(json.load(response)["agents"][0]["id"], "fixture")
            for headers in ({}, {"Content-Type": "application/json", "X-Dial-Benchmark": "1", "Origin": "http://evil.invalid"}):
                with self.assertRaises(urllib.error.HTTPError) as error:
                    request("api/start", self.request, headers)
                self.assertEqual(error.exception.code, 403)
                error.exception.close()
            headers = {"Content-Type": "application/json", "X-Dial-Benchmark": "1"}
            with request("api/start", self.request, headers) as response:
                self.assertEqual(json.load(response)["total"], 2)
            deadline = time.monotonic() + 10
            while True:
                with request("api/state") as response:
                    state = json.load(response)
                if state["status"] != "running":
                    break
                self.assertLess(time.monotonic(), deadline)
                time.sleep(.05)
            self.assertEqual(state["status"], "completed")
            with request("comparison.svg?v=cache") as response:
                ET.fromstring(response.read())
            with request("api/shutdown", {}, {**headers, "Content-Type": "application/json; charset=utf-8"}) as response:
                self.assertEqual(json.load(response)["status"], "stopping")
            process.wait(10)
            self.assertEqual(process.returncode, 0)
        finally:
            if process.poll() is None:
                process.terminate()
                process.wait(10)
            process.stdout.close()
            process.stderr.close()


if __name__ == "__main__":
    unittest.main()
