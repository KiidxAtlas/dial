"""Offline registry discovery acceptance: no binary, provider, network or model required."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import tomllib
import unittest
from unittest.mock import patch

from cross_agent import run_check, run_trial
from registry_discovery import EVIDENCE, capture_trial_trace, grade_messages, grade_trial_session

ROOT = Path(__file__).resolve().parent
FIXTURE = ROOT / "registry-discovery-fixture"
CASES = tomllib.loads((ROOT / "registry-discovery.toml").read_text())["cases"]
ANSWER = {"commands": ["/compact", "/help", "/label", "/settings", "/tree"], "count": 5,
          "tree_up": "Ctrl+Up", "smooth_reveal": True, "page_size": 24, "documented_tree": True}
SOURCES = {"registry": "src/command_registry.rs", "docs": "docs/interface.md",
           "settings": "config/settings.toml", "keys": "src/keybindings.rs"}


def round_trip(index, sources, *, combined=False):
    groups = [sources] if combined else [[source] for source in sources]
    calls, replies = [], []
    for offset, group in enumerate(groups):
        call_id = f"{index}-{offset}"
        calls.append({"id": call_id, "function": {"name": "read", "arguments": json.dumps(
            {"paths": [SOURCES[source] for source in group]})}})
        replies.append({"role": "tool", "tool_call_id": call_id,
                        "content": "\n".join((FIXTURE / SOURCES[source]).read_text() for source in group)})
    return [{"role": "assistant", "content": None, "tool_calls": calls}, *replies]


def good_messages(mode="batch"):
    messages = [{"role": "user", "content": "Inspect the registry"},
                {"role": "dial_state", "state": {"reliability": {
                    "call_policy": {"mode": mode, "reasons": []}}}}]
    messages += round_trip(0, ["registry"])
    if mode == "batch":
        messages += round_trip(1, ["docs", "settings", "keys"])
    else:
        for index, source in enumerate(["docs", "settings", "keys"], 1):
            messages += round_trip(index, [source])
    return messages + [{"role": "assistant", "content": "DONE"}]


def write_session(home, messages):
    home.mkdir(parents=True, exist_ok=True)
    records = [{"type": "session", "id": "abc"}]
    for index, message in enumerate(messages):
        records.append({"type": "message", "id": str(index),
                        "parent": str(index - 1) if index else None, "message": message})
    (home / "fixture_abc.jsonl").write_text("\n".join(json.dumps(record) for record in records))


class DiscoveryTraceTests(unittest.TestCase):
    def test_declaration_first_and_accurate_evidence_passes_both_modes(self):
        for mode, rounds in [("batch", 3), ("one_call", 5)]:
            with self.subTest(mode=mode):
                result = grade_messages(good_messages(mode), mode)
                self.assertEqual(result["result"], "passed", result)
                self.assertEqual(result["round_trips"], rounds)
                self.assertEqual(result["inspected_sources"], sorted(EVIDENCE))

    def test_single_multi_path_read_is_useful_batching(self):
        messages = round_trip(0, list(SOURCES), combined=True) + [{"role": "assistant", "content": "DONE"}]
        for mode in ["batch", "one_call"]:
            self.assertEqual(grade_messages(messages, mode)["result"], "passed")

    def test_serial_guesses_and_policy_violations_fail(self):
        guesses = []
        for index in range(2):
            guesses += [{"role": "assistant", "tool_calls": [{"id": f"guess{index}", "function": {
                "name": "grep", "arguments": json.dumps({"path": "src/command_registry.rs", "pattern": "name:"})}}]},
                        {"role": "tool", "tool_call_id": f"guess{index}", "content": "no matches"}]
        cases = [
            (guesses + good_messages(), "batch", "serial_registry_guesses"),
            (good_messages("one_call"), "batch", "no_useful_batch"),
            (good_messages("batch"), "one_call", "one_call_violation"),
            (round_trip(0, ["registry"]) + [{"role": "assistant", "content": "DONE"}],
             "batch", "missing_source_inspection"),
            (good_messages()[:-1], "batch", "missing_final_reply"),
        ]
        for messages, mode, failure in cases:
            with self.subTest(failure=failure):
                self.assertIn(failure, grade_messages(messages, mode)["failures"])

    def test_missing_results_duplicates_and_round_trip_overhead_fail(self):
        messages = good_messages()
        messages.pop(3)  # declaration tool result
        self.assertIn("unsettled_calls", grade_messages(messages, "batch")["failures"])
        messages = good_messages()
        messages += round_trip(0, ["registry"])
        self.assertIn("malformed_call", grade_messages(messages, "batch")["failures"])
        messages = good_messages()[:-1]
        for index in range(10, 14):
            messages += round_trip(index, ["registry"])
        messages.append({"role": "assistant", "content": "DONE"})
        self.assertIn("excess_round_trips", grade_messages(messages, "batch")["failures"])

    def test_duplicate_evidence_does_not_earn_batch_credit(self):
        messages = round_trip(0, ["registry", "registry"])
        for index, source in enumerate(["docs", "settings", "keys"], 1):
            messages += round_trip(index, [source])
        messages.append({"role": "assistant", "content": "DONE"})
        self.assertIn("no_useful_batch", grade_messages(messages, "batch")["failures"])

    def test_positive_search_locations_are_not_empty_representation_guesses(self):
        searches = []
        for index in range(2):
            searches += [{"role": "assistant", "tool_calls": [{"id": f"location{index}", "function": {
                "name": "grep", "arguments": json.dumps({"path": "src/command_registry.rs", "pattern": "COMMANDS"})}}]},
                         {"role": "tool", "tool_call_id": f"location{index}", "content": "src/command_registry.rs:1:pub const COMMANDS"}]
        self.assertEqual(grade_messages(searches + good_messages(), "batch")["result"], "passed")


class DiscoveryRunnerTests(unittest.TestCase):
    def test_independent_accuracy_checks_reject_wrong_answers(self):
        with tempfile.TemporaryDirectory() as temporary:
            workspace = Path(temporary)
            for case in CASES:
                for answer in [None, ANSWER, dict(ANSWER, count=4), dict(ANSWER, tree_up="Up"),
                               dict(ANSWER, smooth_reveal=False), dict(ANSWER, commands=[])]:
                    with self.subTest(case=case["id"], answer=answer):
                        path = workspace / "answer.json"
                        if answer is not None:
                            path.write_text(json.dumps(answer))
                        self.assertEqual(run_check(case["checks"][0], workspace, "DONE"), answer == ANSWER)

    def test_existing_runner_jointly_scores_accuracy_and_trace_and_preserves_export(self):
        for case in CASES:
            mode = "batch" if case["id"].endswith("batch") else "one_call"
            for valid_answer, valid_trace in [(True, True), (False, True), (True, False)]:
                with self.subTest(mode=mode, answer=valid_answer, trace=valid_trace), tempfile.TemporaryDirectory() as temporary:
                    root = Path(temporary)
                    agent = {"name": "dial", "output": "dial-json", "first": ["dial", "{prompt}"],
                             "follow_up": ["dial", "{prompt}"], "delegation_tools": ["task"],
                             "env": {"DIAL_HOME": "{trial_root}/dial-home"}, "delegated_usage": "included"}
                    def execute(command, *, cwd, env, **kwargs):
                        (cwd / "answer.json").write_text(json.dumps(ANSWER if valid_answer else {}))
                        messages = good_messages(mode) if valid_trace else good_messages(mode)[:-1]
                        write_session(Path(env["DIAL_HOME"]), messages)
                        return subprocess.CompletedProcess(command, 0, json.dumps({"type": "result", "text": "DONE",
                            "session_id": "abc", "usage": {"total": {"all": {"requests": 3}}}}), "")
                    with patch("cross_agent.run_agent_process", side_effect=execute):
                        report = run_trial(agent, case, ROOT.parents[1], 0, root, {}, 10, False, logs=root / "logs")
                    self.assertEqual(report["passed"], valid_answer and valid_trace, report)
                    self.assertIn("round_trips", report["discovery_trace"])
                    self.assertTrue(list((root / "logs").rglob("discovery-trace.jsonl")))
                    self.assertEqual([p for p in root.iterdir() if p.name != "logs"], [])

    def test_missing_policy_and_runtime_one_call_are_unavailable_not_batch_failures(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            home = root / "home"
            for messages, failure in [(good_messages("one_call"), "runtime_disallows_batch"),
                                      ([m for m in good_messages() if m["role"] != "dial_state"], "call_policy_unavailable")]:
                write_session(home, messages)
                result = grade_trial_session({"output": "dial-json"}, CASES[0], {"DIAL_HOME": str(home)}, "abc", root)
                self.assertEqual(result["result"], "unavailable")
                self.assertIn(failure, result["failures"])
            result = grade_trial_session({"output": "dial-json"}, CASES[0], {"DIAL_HOME": str(home)}, "abc", root / "other")
            self.assertEqual(result["result"], "unavailable")

    def test_branched_reset_or_malformed_journals_are_unavailable(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            home = root / "home"
            for mutation in ["parent", "duplicate", "reset_boundary", "message"]:
                write_session(home, good_messages())
                path = home / "fixture_abc.jsonl"
                records = [json.loads(line) for line in path.read_text().splitlines()]
                if mutation == "parent":
                    records[-1]["parent"] = None
                elif mutation == "duplicate":
                    records[-1]["id"] = records[-2]["id"]
                elif mutation == "reset_boundary":
                    records[-1]["type"] = "reset_boundary"
                else:
                    records[-1]["message"] = "unsupported"
                path.write_text("\n".join(json.dumps(record) for record in records))
                result = grade_trial_session({"output": "dial-json"}, CASES[0], {"DIAL_HOME": str(home)}, "abc", root)
                self.assertEqual(result["result"], "unavailable", (mutation, result))


    def test_trace_capture_reports_failed_turns_as_unavailable(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            agent = {"name": "dial", "output": "dial-json", "first": ["dial", "{prompt}"],
                     "follow_up": ["dial", "{prompt}"], "delegation_tools": ["task"],
                     "env": {"DIAL_HOME": "{trial_root}/home"}, "delegated_usage": "included"}
            for error, reason in [(subprocess.TimeoutExpired(["dial"], 1), "turn_timeout_no_terminal_session"),
                                  (OSError("cannot launch"), "agent_start_failed")]:
                with self.subTest(reason=reason), patch("cross_agent.run_agent_process", side_effect=error):
                    report = run_trial(agent, {"id": reason, "prompt": "go", "checks": []},
                                       root, 0, root, {}, 1, False, logs=root / "logs", capture_traces=True)
                self.assertEqual(report["turns"][0]["trace"], {"status": "unavailable", "reason": reason})
                self.assertFalse(report["usage"]["usage_complete"])


class SessionCaptureTests(unittest.TestCase):
    def test_opt_in_capture_retains_each_turn_before_cleanup_without_changing_score(self):
        for capture in (False, True):
            with self.subTest(capture=capture), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                agent = {"name": "dial", "output": "dial-json", "first": ["dial", "{prompt}"],
                         "follow_up": ["dial", "{prompt}"], "delegation_tools": ["task"],
                         "env": {"DIAL_HOME": "{trial_root}/home"}, "delegated_usage": "included"}
                case = {"id": "capture", "prompt": "first", "turns": [{"prompt": "second"}],
                        "checks": [{"kind": "response_equals", "content": "DONE"}]}
                def execute(command, *, cwd, env, **kwargs):
                    messages = good_messages()
                    messages[0]["content"] = command[-1]
                    write_session(Path(env["DIAL_HOME"]), messages)
                    return subprocess.CompletedProcess(command, 0, json.dumps({"type": "result", "text": "DONE",
                        "session_id": "abc", "usage": {"total": {"all": {"requests": 3}}}}), "")
                with patch("cross_agent.run_agent_process", side_effect=execute):
                    report = run_trial(agent, case, root, 0, root, {}, 10, False,
                                       logs=root / "logs", capture_traces=capture)
                self.assertTrue(report["passed"], report)
                self.assertEqual([p.name for p in root.iterdir()], ["logs"])
                for index, turn in enumerate(report["turns"]):
                    if not capture:
                        self.assertNotIn("trace", turn)
                        continue
                    trace = turn["trace"]
                    self.assertEqual(trace["status"], "captured")
                    path = Path(trace["path"])
                    self.assertEqual(path.stat().st_mode & 0o777, 0o600)
                    self.assertEqual(path.parent.stat().st_mode & 0o777, 0o700)
                    records = [json.loads(line) for line in path.read_text().splitlines()]
                    messages = [r["message"] for r in records if r["type"] == "message"]
                    self.assertEqual(messages[0]["content"], ["first", "second"][index])
                    self.assertEqual(messages[2]["tool_calls"][0]["function"]["name"], "read")
                    self.assertIn('COMMANDS: &[(&str, &str)]', messages[3]["content"])
                    self.assertEqual(trace["bytes"], path.stat().st_size)
                    self.assertNotIn("COMMANDS", json.dumps(report))
                    self.assertNotIn("discovery_trace", report)

    def test_capture_rejects_unisolated_ambiguous_oversized_or_escaping_evidence(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            home = root / "home"
            logs = root / "logs"
            logs.mkdir(mode=0o700)
            for mutation in ("external_home", "invalid_id", "duplicate", "oversized", "symlink", "fifo", "malformed"):
                with self.subTest(mutation=mutation):
                    write_session(home, good_messages())
                    path = home / "fixture_abc.jsonl"
                    env, identity = {"DIAL_HOME": str(home)}, "abc"
                    extra = home / "duplicate_abc.jsonl"
                    if mutation == "external_home":
                        env = {"DIAL_HOME": str(root.parent)}
                    elif mutation == "invalid_id":
                        identity = "../abc"
                    elif mutation == "duplicate":
                        extra.write_text(path.read_text())
                    elif mutation == "oversized":
                        path.write_bytes(b" " * (4 * 1024 * 1024 + 1))
                    elif mutation == "symlink":
                        target = root / "outside.jsonl"
                        path.rename(target)
                        path.symlink_to(target)
                    elif mutation == "fifo":
                        path.unlink()
                        os.mkfifo(path)
                    else:
                        path.write_text("not json")
                    result = capture_trial_trace({"output": "dial-json"}, env, identity, root, logs, 0)
                    self.assertEqual(result["status"], "unavailable", result)
                    self.assertEqual(list(logs.iterdir()), [])
                    if extra.exists():
                        extra.unlink()
                    path.unlink()

    def test_capture_stops_after_the_lookup_entry_budget(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            home = root / "home"
            home.mkdir()
            (home / "unrelated").touch()
            with os.scandir(home) as actual:
                entry = next(actual)
            entries = iter([entry] * 10_002)
            with patch("registry_discovery.os.scandir") as scan:
                scan.return_value.__enter__.return_value = entries
                result = capture_trial_trace({"output": "dial-json"}, {"DIAL_HOME": str(home)},
                                             "abc", root, root / "logs", 0)
            self.assertEqual(result["status"], "unavailable")
            self.assertIs(next(entries), entry)  # The reader stopped before exhausting the walk.

    def test_capture_never_overwrites_a_retained_trace(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            home = root / "home"
            logs = root / "logs"
            logs.mkdir(mode=0o700)
            write_session(home, good_messages())
            target = logs / "turn-0.session.jsonl"
            target.write_text("earlier evidence")
            result = capture_trial_trace({"output": "dial-json"}, {"DIAL_HOME": str(home)}, "abc", root, logs, 0)
            self.assertEqual(result["status"], "unavailable")
            self.assertEqual(result["reason"], "trace_write_failed")
            self.assertEqual(target.read_text(), "earlier evidence")



if __name__ == "__main__":
    unittest.main()
