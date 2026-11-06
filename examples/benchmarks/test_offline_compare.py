"""Offline comparison harness regressions: scripted loopback server, wrapper and acceptance. No live model calls."""
import json
import os
import sys
import unittest
import urllib.error
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parent
sys.path.insert(0, str(ROOT))
import offline_compare as oc  # noqa: E402


def post(origin, body, path=oc.MESSAGES_PATH):
    request = urllib.request.Request(origin + path, data=json.dumps(body).encode(),
                                     headers={"Content-Type": "application/json"}, method="POST")
    try:
        with urllib.request.urlopen(request, timeout=10) as response:
            return response.status, response.read().decode()
    except urllib.error.HTTPError as error:
        with error:
            return error.code, error.read().decode()


def prompt_body(prompt, tools=(), extra=()):
    messages = [{"role": "user", "content": prompt}, *extra]
    return {"model": oc.MODEL, "messages": messages, "system": "sys",
            "tools": [{"name": name, "input_schema": {"properties": {p: {} for p in props}}}
                      for name, props in tools]}


def tool_turn(name, call_id="t"):
    return {"role": "assistant", "content": [{"type": "tool_use", "id": call_id, "name": name, "input": {}}]}


class PolicyAndWrapperTests(unittest.TestCase):
    def test_wrapper_is_applied_to_runs_only_not_to_version_identity(self):
        agents = oc.build_agents("/bin/dial-b", "/bin/dial-c", "/bin/omp", "http://127.0.0.1:9")
        for agent in agents:
            self.assertEqual(agent["version"], [agent["binary"], "--version"])
            self.assertEqual(agent["first"][:3], [oc.SANDBOX_EXEC, "-p", oc.EGRESS_POLICY])
            self.assertEqual(agent["follow_up"][:3], [oc.SANDBOX_EXEC, "-p", oc.EGRESS_POLICY])

    def test_policy_allows_only_localhost_outbound(self):
        self.assertIn("(deny network-outbound)", oc.EGRESS_POLICY)
        self.assertIn('(allow network-outbound (remote ip "localhost:*"))', oc.EGRESS_POLICY)

    def test_dial_and_omp_use_placeholder_key_and_loopback_only(self):
        origin = "http://127.0.0.1:9"
        dial = oc.dial_agent("dial-baseline", "/bin/dial", origin)
        self.assertEqual(dial["first"][dial["first"].index("--sandbox") + 1], "off")
        self.assertEqual(dial["env"][oc.KEY_ENV], oc.PLACEHOLDER_KEY)
        self.assertEqual(dial["first"][dial["first"].index("--api-key-env") + 1], oc.KEY_ENV)
        self.assertEqual(dial["first"][dial["first"].index("--endpoint") + 1], origin + "/v1")
        omp = oc.omp_agent("/bin/omp", origin)
        self.assertEqual(omp["env"]["ANTHROPIC_BASE_URL"], origin)
        self.assertEqual(omp["env"]["ANTHROPIC_API_KEY"], oc.PLACEHOLDER_KEY)
        for flag in oc.OMP_FLAGS:
            self.assertIn(flag, omp["first"])

    def test_environment_allowlist_is_the_only_inherited_set(self):
        saved = dict(os.environ)
        self.addCleanup(lambda: (os.environ.clear(), os.environ.update(saved)))
        os.environ["OFFLINE_TEST_SECRET"] = "x"
        with oc.minimal_environment():
            self.assertNotIn("OFFLINE_TEST_SECRET", os.environ)
            self.assertTrue(set(os.environ) <= set(oc.ENV_ALLOWLIST))
        self.assertEqual(os.environ.get("OFFLINE_TEST_SECRET"), "x")


class ArgumentMappingTests(unittest.TestCase):
    def test_maps_to_offered_property_name_and_records_unmapped(self):
        tools = [{"name": "read", "input_schema": {"properties": {"file_path": {}}}}]
        self.assertEqual(oc.map_arguments("read", {"path": "a.txt"}, tools), ({"file_path": "a.txt"}, []))
        self.assertEqual(oc.map_arguments("read", {"path": "a.txt"}, [])[1], ["tool:read"])
        mapped, unmapped = oc.map_arguments("grep", {"pattern": "p", "path": "."},
                                            [{"name": "grep", "input_schema": {"properties": {"query": {}}}}])
        self.assertEqual(mapped["query"], "p")
        self.assertEqual(unmapped, ["path"])


class ScriptedServerTests(unittest.TestCase):
    def setUp(self):
        self.server = oc.ScriptedAnthropic()
        self.addCleanup(self.server.close)

    def test_accepts_only_when_tool_result_matches_expectation(self):
        self.server.begin([[oc.tool("read", expect="alpha", path="n.txt"), oc.text("DONE")]])
        tools = [("read", ["path"])]
        status, first = post(self.server.origin, prompt_body("go", tools))
        self.assertEqual(status, 200)
        self.assertIn("tool_use", first)
        result = {"type": "tool_result", "tool_use_id": "t", "content": "alpha\nbeta"}
        status, second = post(self.server.origin, prompt_body("go", tools, extra=[
            tool_turn("read"), {"role": "user", "content": [result]}]))
        self.assertIn("DONE", second)
        transport = oc.transport_record(self.server.end())
        self.assertEqual([r["verdict"] for r in transport["requests"] if r["kind"] == "turn"], [None, True])
        self.assertEqual(transport["turn_requests"], 2)
        self.assertEqual(transport["failures"], [])

    def test_mismatched_result_replies_failed_and_is_recorded(self):
        self.server.begin([[oc.tool("read", expect="gamma", path="n.txt"), oc.text("DONE")]])
        tools = [("read", ["path"])]
        post(self.server.origin, prompt_body("go", tools))
        _, reply = post(self.server.origin, prompt_body("go", tools, extra=[
            tool_turn("read"), {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t",
                                                            "content": "alpha"}]}]))
        self.assertIn("FAILED", reply)
        self.assertIs(self.server.end()["requests"][1]["verdict"], False)

    def test_unexpected_endpoint_is_refused_and_counted(self):
        self.server.begin([[oc.text("4")]])
        status, _ = post(self.server.origin, {}, path="/v1/other")
        self.assertEqual(status, 404)
        transport = oc.transport_record(self.server.end())
        self.assertEqual(len(transport["violations"]), 1)
        self.assertTrue(any("refused" in failure for failure in transport["failures"]))

    def test_supported_local_discovery_probes_are_accounted_separately_from_messages(self):
        self.server.begin([[oc.text("4")]])
        for path in ("/props", "/api/ps", "/v1/models", "/api/v1/models", "/api/v0/models",
                     f"/api/v0/models/{oc.MODEL}"):
            with self.assertRaises(urllib.error.HTTPError) as caught:
                urllib.request.urlopen(self.server.origin + path, timeout=10)
            with caught.exception as error:
                self.assertEqual(error.code, 404)
        status, _ = post(self.server.origin, {"model": oc.MODEL}, path="/api/show")
        self.assertEqual(status, 404)
        post(self.server.origin, prompt_body("go", [("read", ["path"])]))
        transport = oc.transport_record(self.server.end())
        self.assertEqual(len(transport["probes"]), 7)
        self.assertEqual(transport["turn_requests"], 1)
        self.assertEqual(transport["failures"], [])

    def test_unknown_get_is_still_a_transport_violation(self):
        self.server.begin([[oc.text("4")]])
        with self.assertRaises(urllib.error.HTTPError) as caught:
            urllib.request.urlopen(self.server.origin + "/unrecognized", timeout=10)
        caught.exception.close()
        self.assertEqual(len(self.server.end()["violations"]), 1)

    def test_request_outside_a_trial_is_refused_with_no_trial_record(self):
        status, _ = post(self.server.origin, prompt_body("go"))
        self.assertEqual(status, 409)
        self.server.begin([[oc.text("4")]])
        transport = oc.transport_record(self.server.end())
        self.assertEqual(transport["requests"], [])

    def test_request_budget_caps_turn_requests(self):
        self.server.begin([[oc.text("4")]])
        statuses = [post(self.server.origin, prompt_body("go"))[0]
                    for _ in range(oc.MAX_REQUESTS_PER_TRIAL + 1)]
        self.assertEqual(statuses[-1], 429)
        transport = oc.transport_record(self.server.end())
        self.assertIn("unexpected or over-budget request", transport["failures"])

    def test_auxiliary_requests_without_tools_do_not_consume_script(self):
        self.server.begin([[oc.text("4")]])
        post(self.server.origin, {"model": oc.MODEL, "messages": [{"role": "user", "content": "title"}]})
        transport = oc.transport_record(self.server.end())
        self.assertEqual(transport["requests"][0]["kind"], "auxiliary")
        self.assertEqual(transport["turn_requests"], 0)
        self.assertIn("no scripted turn request reached the server", transport["failures"])

    def test_streamed_reply_is_well_formed_sse(self):
        self.server.begin([[oc.text("4")]])
        body = prompt_body("go", [("read", ["path"])])
        body["stream"] = True
        status, raw = post(self.server.origin, body)
        self.assertEqual(status, 200)
        events = [line[len("event: "):] for line in raw.splitlines() if line.startswith("event: ")]
        self.assertEqual(events[0], "message_start")
        self.assertEqual(events[-1], "message_stop")
        self.server.end()

    def test_source_location_uses_code_find_or_content_grep_not_filename_find(self):
        for offered, expected, field in [([("find", ["query", "related"])], "find", "query"),
                                         ([("find", ["pattern"]), ("grep", ["pattern", "path"])], "grep", "pattern")]:
            with self.subTest(tool=expected):
                spec = oc.WORKLOADS["source_location"]
                self.server.begin(spec["script"])
                _, raw = post(self.server.origin, prompt_body(spec["prompt"], offered))
                block = json.loads(raw)["content"][0]
                self.assertEqual(block["name"], expected)
                self.assertEqual(block["input"][field], "bounded_transaction_total")
                self.assertEqual(self.server.end()["requests"][0]["unmapped"], [])

    def test_three_file_mutation_keeps_complete_read_witnesses_and_exact_acceptance(self):
        spec = oc.WORKLOADS["edit_three_files"]
        actions = spec["script"][0]
        self.assertEqual([a["name"] for a in actions[:-1]], ["read"] * 3 + ["write"] * 3)
        self.assertEqual({c["path"] for c in spec["checks"] if c["kind"] == "file_equals"},
                         {"a.py", "b.py", "c.py"})


class EgressSelfTestTests(unittest.TestCase):
    def test_probe_fails_when_loopback_is_refused(self):
        original = oc.subprocess.run
        payload = {"loopback": {"connected": False, "errno": 1}, "external": {"connected": False, "errno": 1}}
        fake = type("R", (), {"returncode": 0, "stderr": "", "stdout": json.dumps(payload)})()
        oc.subprocess.run = lambda *a, **k: fake
        try:
            with self.assertRaisesRegex(RuntimeError, "refused loopback"):
                oc.egress_selftest(9)
        finally:
            oc.subprocess.run = original


if __name__ == "__main__":
    unittest.main()


class NewWorkloadTests(unittest.TestCase):
    def setUp(self):
        self.server = oc.ScriptedAnthropic()
        self.addCleanup(self.server.close)

    def test_every_new_workload_builds_a_case_with_a_script_and_checks(self):
        for name in ("research_sources", "deferred_tool_discovery", "unavailable_tool_recovery",
                     "read_edit_test_program", "denial_stop"):
            with self.subTest(workload=name):
                spec = oc.WORKLOADS[name]
                self.assertTrue(spec["script"] and spec["checks"])
                self.assertIsNotNone(oc.build_case(name, spec))

    def test_unoffered_tool_is_recorded_as_wrong_tool_and_its_error_result_as_observed(self):
        # The retry after the refused call is a real tool call, which is what makes it a recovery round.
        self.server.begin([[oc.tool("fetch_remote_catalog", expect_error=True, query="alpha"),
                            oc.tool("read", expect="alpha", path="notes.txt"), oc.text("DONE")]])
        tools = [("read", ["path"])]
        post(self.server.origin, prompt_body("go", tools))
        error = {"type": "tool_result", "tool_use_id": "t", "content": "unknown tool", "is_error": True}
        post(self.server.origin, prompt_body("go", tools, extra=[tool_turn("fetch_remote_catalog"),
                                                                 {"role": "user", "content": [error]}]))
        requests = self.server.end()["requests"]
        # The unoffered call is recorded on the request that emitted it; its error result arrives next.
        self.assertTrue(requests[0]["wrong_tool"])
        self.assertEqual(requests[1]["error_results"], 1)
        self.assertTrue(requests[1]["error_result_observed"])
        self.assertTrue(requests[1]["recovery_round"])

    def test_error_result_observed_requires_an_expected_refusal_and_an_error_result(self):
        cases = [
            # (expect_error, is_error, expected observed)
            (True, True, True),
            (True, False, False),
            (False, True, False),
            (False, False, False),
        ]
        for expect_error, is_error, expected in cases:
            with self.subTest(expect_error=expect_error, is_error=is_error):
                server = oc.ScriptedAnthropic()
                self.addCleanup(server.close)
                server.begin([[oc.tool("read", expect_error=expect_error, path="n.txt"), oc.text("DONE")]])
                tools = [("read", ["path"])]
                post(server.origin, prompt_body("go", tools))
                result = {"type": "tool_result", "tool_use_id": "t", "content": "x", "is_error": is_error}
                post(server.origin, prompt_body("go", tools, extra=[tool_turn("read"),
                                                                    {"role": "user", "content": [result]}]))
                self.assertIs(server.end()["requests"][1]["error_result_observed"], expected)

    def test_denial_never_passes_on_error_text_alone_without_the_no_write_check(self):
        spec = oc.WORKLOADS["denial_stop"]
        kinds = [check["kind"] for check in spec["checks"]]
        self.assertIn("command", kinds)
        command = next(c["command"] for c in spec["checks"] if c["kind"] == "command")
        self.assertEqual(command, "test ! -e ../denied_probe.txt")

    def test_absent_legacy_counter_summarizes_as_none_not_zero(self):
        self.assertIsNone(oc._count([{"kind": "turn"}], "wrong_tool"))
        self.assertEqual(oc._count([{"wrong_tool": True}, {"wrong_tool": False}], "wrong_tool"), 1)

    def test_output_parsed_is_none_when_unknown_and_not_task_success(self):
        agent = {"name": "x", "output": "dial-json"}
        self.assertIsNone(oc.output_parsed(agent, None, [{}]))
        self.assertIsNone(oc.output_parsed(agent, Path("/nonexistent-offline-log"), []))
