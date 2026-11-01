"""Cross-agent runner regressions with stub agents; these are not model-quality measurements."""
import hashlib
import json
import subprocess
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path
from unittest.mock import MagicMock, patch

ROOT = Path(__file__).resolve().parent
sys.path.insert(0, str(ROOT))
import cross_agent  # noqa: E402

# Shapes observed from `dial -p --output-format json` and `omp -p --mode json` (trimmed).
DIAL_RESULT = {"type": "result", "text": "4", "tool_calls": [
    {"name": "read", "is_error": False}, {"name": "bash", "is_error": True}],
    "usage": {"total": {"all": {"input": 1000, "cache_read": 600, "cache_write": 300, "output": 50,
                                "requests": 3},
                        "by_model": {"anthropic/claude-sonnet-4-6": {}}}}}
# Inclusive operation costs nest inside `duration_ms`. Shape matches turn_loop.rs: counters are numeric, the
# attribution string is per turn, and count maps (requests_by_phase etc.) live inside `projection_bytes`.
DIAL_EFFICIENCY = {
    "version": 1, "duration_ms": 1000,
    "inference_latency": {"requests": 2, "failed_requests": 0, "retry_backoffs": 0, "response_wait_ms": 700,
                          "loading_ms": 0, "prompt_processing_ms": 0, "generation_ms": 0, "retry_backoff_ms": 0,
                          "cancellation_ms": 0, "total_ms": 700, "attribution": "stub"},
    "storage_io": {"control_publication_micros": 300, "control_publication_attempts": 5,
                   "control_publication_failures": 0},
    "projection_bytes": {"requests": 3, "requests_before_first_workspace_change_result": 1,
                         "stable_message_bytes": 10, "changed_message_bytes": 4,
                         "observed_reply_text_bytes": 2, "observed_tool_argument_bytes": 8,
                         "requests_by_phase": {"investigation": 2, "verification": 1},
                         "prefix_invalidations": {"system_changed": 1},
                         "recovery_attempts": {"tool_failure": 1},
                         "backend_operations": {"check_input_capture": {"calls": 4, "elapsed_us": 900}}},
}
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

    def test_dial_stream_diagnostics_do_not_double_count_final_usage(self):
        result = {**DIAL_RESULT, "efficiency": DIAL_EFFICIENCY}
        events = [
            {"type": "system", "session_id": "test-only"},
            {"type": "tool_call", "call_id": "probe", "name": "bash", "summary": "probe"},
            {"type": "tool_result", "call_id": "probe", "name": "bash", "is_error": True,
             "text": "probe failed", "seconds": 0.1},
            {"type": "usage", "prompt_tokens": 1000, "output_tokens": 50},
            {"type": "efficiency", "report": DIAL_EFFICIENCY},
            result,
        ]
        stream = "\n".join(json.dumps(event) for event in events)
        text, usage = cross_agent.parse_dial_json(stream)
        self.assertEqual(text, "4")
        self.assertTrue(usage["usage_complete"])
        self.assertEqual((usage["requests"], usage["input_total"], usage["tool_calls"],
                          usage["tool_errors"]), (3, 1000, 2, 1))
        self.assertEqual(usage["efficiency"]["projection_bytes"], DIAL_EFFICIENCY["projection_bytes"])
        self.assertEqual((text, usage), cross_agent.parse_dial_json(json.dumps(result)))
        # Diagnostics without a terminal result never establish complete usage or an answer.
        incomplete = "\n".join(json.dumps(event) for event in events[:-1])
        answer, incomplete_usage = cross_agent.parse_dial_json(incomplete)
        self.assertIsNone(answer)
        self.assertFalse(incomplete_usage["usage_complete"])

    def test_missing_output_is_incomplete_not_zero_usage(self):
        for parse in (cross_agent.parse_dial_json, cross_agent.parse_omp_json, cross_agent.parse_text):
            self.assertFalse(parse("not json")[1]["usage_complete"])

    def test_non_object_json_lines_are_ignored(self):
        text, usage = cross_agent.parse_omp_json('null\n[]\n7\n' + '\n'.join(json.dumps(e) for e in OMP_EVENTS))
        self.assertEqual(text, "4")
        self.assertTrue(usage["usage_complete"])

    def test_dial_efficiency_object_is_read_per_field_and_missing_fields_stay_unknown(self):
        full = {**DIAL_RESULT, "efficiency": DIAL_EFFICIENCY}
        _, usage = cross_agent.parse_dial_json(json.dumps(full))
        self.assertEqual(usage["efficiency"]["projection_bytes"]["requests_by_phase"],
                         {"investigation": 2, "verification": 1})
        self.assertEqual(usage["efficiency"]["storage_io"]["control_publication_attempts"], 5)
        # Absent object: every field unknown.
        _, absent = cross_agent.parse_dial_json(json.dumps(DIAL_RESULT))
        self.assertTrue(all(value is None for value in absent["efficiency"].values()))
        # Partial object: the missing top-level field is unknown; reported fields are kept as emitted.
        partial = {k: v for k, v in DIAL_EFFICIENCY.items() if k != "storage_io"}
        _, usage = cross_agent.parse_dial_json(json.dumps({**DIAL_RESULT, "efficiency": partial}))
        self.assertIsNone(usage["efficiency"]["storage_io"])
        self.assertEqual(usage["efficiency"]["projection_bytes"]["recovery_attempts"], {"tool_failure": 1})

    def test_turn_efficiency_aggregates_numeric_leaves_only_when_every_turn_reports_it(self):
        known = {"duration_ms": 1000,
                 "inference_latency": {"requests": 2, "total_ms": 700, "attribution": "x"},
                 "storage_io": {"attempts": 5},
                 "projection_bytes": {"requests": 3, "requests_by_phase": {"investigation": 2},
                                      "recovery_attempts": {}}}
        other = {"duration_ms": 500,
                 "inference_latency": {"requests": 1, "total_ms": 200, "attribution": "y"},
                 "storage_io": {"attempts": 2},
                 "projection_bytes": {"requests": 1, "requests_by_phase": {"investigation": 1, "verification": 1},
                                      "recovery_attempts": {}}}
        total = cross_agent.aggregate_efficiency([{"efficiency": known}, {"efficiency": other}], failed=False)
        self.assertEqual(total["duration_ms"], 1500)
        self.assertEqual(total["storage_io"], {"attempts": 7})
        # Strings (attribution) are per turn only; counters add; dynamic label maps add per label.
        self.assertEqual(total["inference_latency"], {"requests": 3, "total_ms": 900})
        self.assertEqual(total["projection_bytes"]["requests_by_phase"], {"investigation": 3, "verification": 1})
        self.assertEqual(total["projection_bytes"]["requests"], 4)
        # One unreported turn or a failed trial makes the whole trial's efficiency unknown.
        self.assertTrue(all(v is None for v in cross_agent.aggregate_efficiency(
            [{"efficiency": known}, {"efficiency": None}], failed=False).values()))
        self.assertTrue(all(v is None for v in cross_agent.aggregate_efficiency(
            [{"efficiency": known}, {"efficiency": other}], failed=True).values()))

    def test_partial_metadata_marks_trial_unknown_and_excludes_it_from_projection_totals(self):
        full = cross_agent.aggregate_efficiency([{"efficiency": DIAL_EFFICIENCY}], failed=False)
        partial_turn = {k: v for k, v in DIAL_EFFICIENCY.items() if k != "storage_io"}
        partial = cross_agent.aggregate_efficiency([{"efficiency": partial_turn}], failed=False)
        trials = [{"efficiency": full}, {"efficiency": partial}]
        self.assertTrue(cross_agent.efficiency_known(trials[0]))
        self.assertFalse(cross_agent.efficiency_known(trials[1]))
        totals = cross_agent.projection_totals(trials)
        self.assertEqual(totals["requests"], DIAL_EFFICIENCY["projection_bytes"]["requests"])
        self.assertEqual(totals["stable_message_bytes"], DIAL_EFFICIENCY["projection_bytes"]["stable_message_bytes"])
        self.assertIsNone(cross_agent.aggregate_efficiency([{"efficiency": partial_turn}], failed=False)["storage_io"])

    def test_first_result_latencies_stay_per_turn_without_discarding_additive_counters(self):
        turns = []
        for first_tool, first_change in [(None, None), (100, 200), (300, 400)]:
            projection = {**DIAL_EFFICIENCY["projection_bytes"],
                          "first_tool_result_ms": first_tool,
                          "first_workspace_change_result_ms": first_change}
            turns.append({"efficiency": {**DIAL_EFFICIENCY, "projection_bytes": projection}})
        for subset in (turns, turns[1:]):
            with self.subTest(turn_count=len(subset)):
                total = cross_agent.aggregate_efficiency(subset, failed=False)
                self.assertIsInstance(total["projection_bytes"], dict)
                self.assertNotIn("first_tool_result_ms", total["projection_bytes"])
                self.assertNotIn("first_workspace_change_result_ms", total["projection_bytes"])
                self.assertEqual(total["projection_bytes"]["requests"], 3 * len(subset))
        self.assertEqual(turns[1]["efficiency"]["projection_bytes"]["first_tool_result_ms"], 100)


class IdentityAndOrderTests(unittest.TestCase):
    def test_binary_identity_hashes_the_resolved_executable_and_reports_missing_ones(self):
        with tempfile.TemporaryDirectory() as directory:
            tool = Path(directory) / "fake-agent"
            tool.write_bytes(b"agent-bytes")
            with patch.object(cross_agent.shutil, "which", return_value=str(tool)):
                identity = cross_agent.binary_identity(["fake-agent", "--x"])
            self.assertEqual(identity["binary_path"], str(tool))
            self.assertEqual(identity["binary_sha256"], hashlib.sha256(b"agent-bytes").hexdigest())
        with patch.object(cross_agent.shutil, "which", return_value=None):
            self.assertEqual(cross_agent.binary_identity(["missing"]),
                             {"binary_path": None, "binary_sha256": None})

    def test_trial_order_is_seeded_reproducible_and_balanced_across_agents(self):
        agents = [{"name": n} for n in ("a", "b", "c")]
        cases = [{"id": f"c{i}"} for i in range(3)]
        first = cross_agent.trial_plan(agents, cases, 2, seed=7)
        self.assertEqual(first, cross_agent.trial_plan(agents, cases, 2, seed=7))
        self.assertNotEqual(first, cross_agent.trial_plan(agents, cases, 2, seed=8))
        self.assertEqual(len(first), 18)
        # Each (repetition, case) block lists every agent exactly once.
        for offset in range(0, len(first), len(agents)):
            block = first[offset:offset + len(agents)]
            self.assertEqual(sorted(a["name"] for a, _, _ in block), ["a", "b", "c"])
            self.assertEqual({(c["id"], r) for _, c, r in block}, {(block[0][1]["id"], block[0][2])})
        # Slot balance: each agent takes each position equally often.
        positions = {name: [0] * len(agents) for name in ("a", "b", "c")}
        for offset in range(0, len(first), len(agents)):
            for slot, (agent, _, _) in enumerate(first[offset:offset + len(agents)]):
                positions[agent["name"]][slot] += 1
        blocks = len(first) // len(agents)
        self.assertTrue(all(count == blocks // len(agents) for row in positions.values() for count in row))


class RedactionTests(unittest.TestCase):
    def test_secret_shapes_are_redacted_and_excerpts_are_bounded(self):
        raw = ("auth token=abc123secret\nAuthorization: Bearer xyz.987\nkey sk-ant-AAAAAAAAAAAAAA\n"
               + "deadbeef" * 8 + "\n" + "x" * 5000 + "\nlast line")
        excerpt = cross_agent.stderr_excerpt(raw)
        self.assertLessEqual(len(excerpt), cross_agent.STDERR_EXCERPT_CHARS)
        self.assertNotIn("abc123secret", excerpt)
        self.assertIn("last line", excerpt)
        redacted = cross_agent.redact("token=abc123secret Bearer xyz.987 sk-ant-AAAAAAAAAAAAAA " + "f" * 40)
        for secret in ("abc123secret", "xyz.987", "sk-ant-AAAAAAAAAAAAAA", "f" * 40):
            self.assertNotIn(secret, redacted)
        self.assertEqual(cross_agent.stderr_excerpt(None), "")


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

    def test_trial_root_isolates_each_trial_home_and_is_created(self):
        logs = self.root / "logs"
        self.agent["env"] = {"STUB_HOME": "{trial_root}/home"}
        self.agent["first"] = [sys.executable, "-c",
                               "import os; print(os.environ['STUB_HOME'])", "{prompt}"]
        record = self.trial({"id": "isolated", "prompt": "noop", "checks": []}, logs)
        trial_home = Path(record["workspace"]).parent / "home"
        printed = (logs / "stub-isolated-r0" / "turn-0.stdout").read_text().strip()
        self.assertEqual(printed, str(trial_home))
        self.assertTrue(trial_home.is_dir())

    def test_delegated_calls_make_usage_incomplete_unless_the_agent_reports_them(self):
        part = {**cross_agent.empty_usage(), "tool_calls": 2, "tools": {"task": 1, "read": 1}}
        for reported, complete in (("unknown", False), ("included", True)):
            total = cross_agent.empty_usage()
            cross_agent.add_usage(total, part, {"delegation_tools": ["task"], "delegated_usage": reported})
            self.assertEqual((total["delegations"], total["tools"], total["usage_complete"]),
                             (1, {"task": 1, "read": 1}, complete))

    def test_timeout_keeps_partial_logs_and_marks_usage_incomplete(self):
        logs = self.root / "logs"
        with patch.object(cross_agent, "run_agent_process", side_effect=subprocess.TimeoutExpired(
                ["stub"], 60, output=b"partial\n", stderr=b"diagnostic\n")):
            record = self.trial({"id": "timeout", "prompt": "noop", "checks": []}, logs)
        self.assertFalse(record["passed"])
        self.assertFalse(record["usage"]["usage_complete"])
        self.assertEqual((logs / "stub-timeout-r0" / "turn-0.stdout").read_text(), "partial\n")
        self.assertEqual((logs / "stub-timeout-r0" / "turn-0.stderr").read_text(), "diagnostic\n")
        self.assertEqual(record["turns"][0]["stderr_tail"], "diagnostic\n")
        self.assertTrue(record["turns"][0]["stderr_log"].endswith("turn-0.stderr"))

    def test_explicit_hard_deadline_overrides_transcript_cleanup_grace(self):
        for deadline, expected in ((None, 60), (30, 30)):
            with self.subTest(deadline=deadline), patch.object(
                    cross_agent, "run_agent_process", side_effect=subprocess.TimeoutExpired(
                        ["stub"], expected, output="partial", stderr="")) as run:
                record = cross_agent.run_trial(
                    self.agent, {"id": "deadline", "project": "fixture", "prompt": "go", "checks": []},
                    self.root, 0, self.root, {"model": "m"}, 30, True, hard_timeout=deadline)
                self.assertEqual(run.call_args.kwargs["timeout"], expected)
                self.assertEqual(record["agent_error"], f"turn 0 exceeded {expected}s")

    def test_agent_group_is_stopped_on_success_and_before_timeout_output_collection(self):
        for timed_out in (False, True):
            with self.subTest(timed_out=timed_out):
                process = MagicMock(pid=12345, returncode=0)
                events = []

                def communicate(timeout=None):
                    events.append("collect" if timeout is None else "run")
                    if timeout is not None and timed_out:
                        raise subprocess.TimeoutExpired(["stub"], timeout)
                    return "partial", "diagnostic"

                process.communicate.side_effect = communicate
                with patch.object(cross_agent.subprocess, "Popen", return_value=process) as launch, \
                     patch.object(cross_agent.os, "killpg", side_effect=lambda *args: events.append("stop")):
                    if timed_out:
                        with self.assertRaises(subprocess.TimeoutExpired) as raised:
                            cross_agent.run_agent_process(["stub"], timeout=1, cwd=self.root,
                                                          env={}, stdin=subprocess.DEVNULL)
                        self.assertEqual((raised.exception.output, raised.exception.stderr),
                                         ("partial", "diagnostic"))
                        self.assertLess(events.index("stop"), events.index("collect"))
                    else:
                        result = cross_agent.run_agent_process(["stub"], timeout=1, cwd=self.root,
                                                               env={}, stdin=subprocess.DEVNULL)
                        self.assertEqual((result.returncode, result.stdout), (0, "partial"))
                    self.assertIn("stop", events)
                    self.assertTrue(launch.call_args.kwargs["start_new_session"])
                    process.wait.assert_called_once()

    def test_failed_turn_reports_exit_code_stderr_log_and_redacted_excerpt(self):
        self.agent["first"] = [sys.executable, "-c",
                               "import sys; sys.stderr.write('token=hunter2 boom\\n'); sys.exit(7)", "{prompt}"]
        logs = self.root / "logs"
        record = self.trial({"id": "boom", "prompt": "noop", "checks": []}, logs)
        turn = record["turns"][0]
        self.assertEqual(turn["exit"], 7)
        self.assertEqual(record["agent_error"], "turn 0 exited 7")
        self.assertIn("boom", turn["stderr_tail"])
        self.assertNotIn("hunter2", turn["stderr_tail"])
        self.assertTrue(Path(turn["stderr_log"]).is_file())
        self.assertTrue(all(v is None for v in record["efficiency"].values()))

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
                     "--model", "m", "--repetitions", "2", "--root", str(self.root), "--report", str(report),
                     "--seed", "3"]
        completed = subprocess.run(arguments, capture_output=True, text=True, timeout=60)
        self.assertEqual(completed.returncode, 0, completed.stderr)
        payload = json.loads(report.read_text())
        self.assertEqual(payload["summary"]["stub"]["passed_trials"], 2)
        self.assertEqual(payload["by_case"]["c"]["stub"], {"passed": 2, "trials": 2})
        self.assertEqual(payload["seed"], 3)
        self.assertEqual(len(payload["trial_order"]), 2)
        self.assertIn("binary_sha256", payload["agents"][0])
        self.assertEqual(payload["timing_comparable"], payload["jobs"] == 1)
        self.assertEqual(subprocess.run(arguments, capture_output=True, timeout=60).returncode, 2)


if __name__ == "__main__":
    unittest.main()
