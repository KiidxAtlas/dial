"""Cross-agent runner regressions with stub agents; these are not model-quality measurements."""
import json
import subprocess
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path
from unittest.mock import patch

ROOT = Path(__file__).resolve().parent
sys.path.insert(0, str(ROOT))
import cross_agent  # noqa: E402

# Shapes observed from `dial -p --output-format json` and `omp -p --mode json` (trimmed).
DIAL_RESULT = {"type": "result", "text": "4", "tool_calls": [
    {"name": "read", "is_error": False}, {"name": "bash", "is_error": True}],
    "usage": {"total": {"all": {"input": 1000, "cache_read": 600, "cache_write": 300, "output": 50,
                                "requests": 3},
                        "by_model": {"anthropic/claude-sonnet-4-6": {}}}}}
OMP_EVENTS = [
    {"type": "session"},
    {"type": "message_end", "message": {"role": "user", "content": [{"type": "text", "text": "q"}]}},
    {"type": "message_end", "message": {"role": "assistant", "provider": "anthropic", "model": "claude-sonnet-4-6",
                                        "content": [{"type": "toolCall", "name": "read"}],
                                        "usage": {"input": 3, "output": 70, "cacheRead": 0, "cacheWrite": 900}}},
    {"type": "tool_execution_end", "toolName": "read", "isError": False},
    {"type": "message_end", "message": {"role": "assistant", "provider": "anthropic", "model": "claude-sonnet-4-6",
                                        "content": [{"type": "text", "text": "4"}],
                                        "usage": {"input": 1, "output": 10, "cacheRead": 900, "cacheWrite": 20}}},
]

# A stand-in agent: appends each prompt to prompts.txt, applies a scripted edit and prints dial JSON.
STUB = textwrap.dedent("""\
    import json, pathlib, sys
    prompt = sys.argv[-1]
    with open("prompts.txt", "a") as log:
        log.write(json.dumps({"continue": "-c" in sys.argv, "prompt": prompt}) + "\\n")
    if "fail" in prompt:
        sys.exit(4)
    if "write" in prompt:
        pathlib.Path("answer.txt").write_text("done")
    print(json.dumps({"type": "result", "text": " 4 \\n", "tool_calls": [],
                      "usage": {"total": {"all": {"input": 10, "output": 2, "requests": 1}}}}))
""")


class ParserTests(unittest.TestCase):
    def test_both_agents_normalize_to_the_same_usage_fields(self):
        text, dial = cross_agent.parse_dial_json("notice\n" + json.dumps(DIAL_RESULT))
        self.assertEqual(text, "4")
        self.assertEqual((dial["input_total"], dial["uncached_input"], dial["output"], dial["requests"],
                          dial["tool_calls"], dial["tool_errors"]), (1000, 100, 50, 3, 2, 1))
        text, omp = cross_agent.parse_omp_json("\n".join(json.dumps(e) for e in OMP_EVENTS))
        self.assertEqual(text, "4")
        self.assertEqual((omp["input_total"], omp["uncached_input"], omp["cache_read"], omp["output"],
                          omp["requests"], omp["tool_calls"]), (1824, 4, 900, 80, 2, 1))
        for usage in (dial, omp):
            self.assertTrue(usage["usage_complete"])
            self.assertEqual(usage["models"], ["anthropic/claude-sonnet-4-6"])

    def test_missing_output_is_incomplete_not_zero_usage(self):
        for parse in (cross_agent.parse_dial_json, cross_agent.parse_omp_json, cross_agent.parse_text):
            self.assertFalse(parse("not json")[1]["usage_complete"])

    def test_non_object_json_lines_are_ignored(self):
        text, usage = cross_agent.parse_omp_json('null\n[]\n7\n' + '\n'.join(json.dumps(e) for e in OMP_EVENTS))
        self.assertEqual(text, "4")
        self.assertTrue(usage["usage_complete"])


class TrialTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        (self.root / "fixture").mkdir()
        (self.root / "fixture" / "seed.txt").write_text("seed")
        (self.root / "stub.py").write_text(STUB)
        command = [sys.executable, str(self.root / "stub.py"), "{model}", "{prompt}"]
        self.agent = {"name": "stub", "output": "dial-json", "first": command,
                      "follow_up": command[:2] + ["-c"] + command[2:], "env": {},
                      "delegation_tools": ["task"], "delegated_usage": "unknown"}

    def tearDown(self):
        self.temporary.cleanup()

    def trial(self, case, logs=None):
        case = {"project": "fixture", "categories": [], **case}
        return cross_agent.run_trial(self.agent, case, self.root, 0, self.root, {"model": "m"}, 30, True, logs)

    def test_raw_output_is_kept_and_agent_env_expands_work_root(self):
        self.agent["env"] = {"STUB_HOME": "{work_root}/stub-home"}
        self.agent["first"] = [sys.executable, "-c",
                               "import os, sys; print(os.environ['STUB_HOME']); print('err', file=sys.stderr)",
                               "{prompt}"]
        logs = self.root / "logs"
        record = self.trial({"id": "env", "prompt": "noop", "checks": []}, logs)
        stdout = (logs / "stub-env-r0" / "turn-0.stdout").read_text().strip()
        self.assertEqual(stdout, str(self.root / "stub-home"))
        self.assertTrue((self.root / "stub-home").is_dir())
        self.assertEqual((logs / "stub-env-r0" / "turn-0.stderr").read_text(), "err\n")
        self.assertFalse(record["usage"]["usage_complete"])

    def test_delegated_calls_make_usage_incomplete_unless_the_agent_reports_them(self):
        part = {**cross_agent.empty_usage(), "tool_calls": 2, "tools": {"task": 1, "read": 1}}
        for reported, complete in (("unknown", False), ("included", True)):
            total = cross_agent.empty_usage()
            cross_agent.add_usage(total, part, {"delegation_tools": ["task"], "delegated_usage": reported})
            self.assertEqual((total["delegations"], total["tools"], total["usage_complete"]),
                             (1, {"task": 1, "read": 1}, complete))

    def test_timeout_keeps_partial_logs_and_marks_usage_incomplete(self):
        logs = self.root / "logs"
        with patch.object(cross_agent.subprocess, "run", side_effect=subprocess.TimeoutExpired(
                ["stub"], 60, output=b"partial\n", stderr=b"diagnostic\n")):
            record = self.trial({"id": "timeout", "prompt": "noop", "checks": []}, logs)
        self.assertFalse(record["passed"])
        self.assertFalse(record["usage"]["usage_complete"])
        self.assertEqual((logs / "stub-timeout-r0" / "turn-0.stdout").read_text(), "partial\n")
        self.assertEqual((logs / "stub-timeout-r0" / "turn-0.stderr").read_text(), "diagnostic\n")

    def test_turns_continue_context_is_inline_and_non_portable_checks_are_excluded(self):
        record = self.trial({"id": "journey", "prompt": "plan", "context_messages": ["old note"],
                             "turns": [{"prompt": "now write", "compact_before": True}],
                             "checks": [{"kind": "file_equals", "path": "answer.txt", "content": "done"},
                                        {"kind": "response_equals", "content": "4"},
                                        {"kind": "metric", "metric": "compactions", "min": 1}]})
        prompts = [json.loads(line) for line in
                   (Path(record["workspace"]) / "prompts.txt").read_text().splitlines()]
        self.assertEqual([p["continue"] for p in prompts], [False, True])
        self.assertIn("old note", prompts[0]["prompt"])
        self.assertTrue(prompts[0]["prompt"].endswith("plan"))
        self.assertEqual([c["result"] for c in record["checks"]], ["passed", "passed", "not_portable"])
        self.assertTrue(record["passed"])
        self.assertEqual(record["skipped_compactions"], 1)
        self.assertEqual(record["usage"]["requests"], 2)

    def test_failed_turn_or_wrong_outcome_fails_and_escaping_paths_never_pass(self):
        failed = self.trial({"id": "f", "prompt": "fail", "checks": [{"kind": "file_exists", "path": "seed.txt"}]})
        self.assertFalse(failed["passed"])
        self.assertEqual(failed["agent_error"], "turn 0 exited 4")
        wrong = self.trial({"id": "w", "prompt": "noop",
                            "checks": [{"kind": "file_exists", "path": "../fixture/seed.txt"},
                                       {"kind": "command", "command": "test -f answer.txt"}]})
        self.assertEqual([c["result"] for c in wrong["checks"]], ["failed", "failed"])
        unscored = self.trial({"id": "u", "prompt": "noop", "checks": [{"kind": "tool_invoked", "tool": "read"}]})
        self.assertFalse(unscored["scored"])
        self.assertFalse(unscored["passed"])

    def test_cli_writes_report_and_refuses_to_overwrite(self):
        agents = self.root / "agents.toml"
        command = json.dumps([sys.executable, str(self.root / "stub.py"), "{prompt}"])
        agents.write_text(f'[[agents]]\nname="stub"\noutput="dial-json"\nfirst={command}\nfollow_up={command}\n')
        suite = self.root / "suite.toml"
        suite.write_text('version=1\nname="s"\n[[cases]]\nid="c"\nproject="fixture"\nprompt="write"\n'
                         'checks=[{kind="file_equals",path="answer.txt",content="done"}]\n')
        report = self.root / "report.json"
        arguments = [sys.executable, str(ROOT / "cross_agent.py"), str(suite), "--agents", str(agents),
                     "--model", "m", "--repetitions", "2", "--root", str(self.root), "--report", str(report)]
        completed = subprocess.run(arguments, capture_output=True, text=True, timeout=60)
        self.assertEqual(completed.returncode, 0, completed.stderr)
        payload = json.loads(report.read_text())
        self.assertEqual(payload["summary"]["stub"]["passed_trials"], 2)
        self.assertEqual(payload["by_case"]["c"]["stub"], {"passed": 2, "trials": 2})
        self.assertEqual(subprocess.run(arguments, capture_output=True, timeout=60).returncode, 2)


if __name__ == "__main__":
    unittest.main()
