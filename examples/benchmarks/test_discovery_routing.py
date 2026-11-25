"""Synthetic report and disposable fixture checks; no tool decisions are simulated as live results."""
import shutil
import subprocess
import sys
import tempfile
import tomllib
import unittest
from pathlib import Path

from discovery_routing import grade
from test_quality import checks_pass
from test_tool_effectiveness import tool_trace, trial_case

ROOT = Path(__file__).resolve().parent
SUITE = tomllib.loads((ROOT / "discovery-routing.toml").read_text())


def report_with(routes):
    cases = []
    for spec, (recorded, passed) in zip(SUITE["cases"], routes, strict=True):
        case = trial_case(0, spec["id"], passed, trace=recorded)
        cases.append({**case, "id": spec["id"]})
    return {"report": {"suite": SUITE["name"], "cases": cases}}


def dispatched(*tools, complete=True):
    return tool_trace(complete=complete, offered=tools, invoked=dict.fromkeys(tools, 1))


class DiscoveryRoutingTests(unittest.TestCase):
    def test_preferred_routes_and_independent_outcomes(self):
        for path_tool in ("find", "glob"):
            with self.subTest(path_tool=path_tool):
                routes = [dispatched(path_tool, "read"), dispatched("find"), dispatched("grep"),
                          dispatched("verify"), dispatched("bash")]
                rows = grade(report_with(zip(routes, (True, False, True, True, True))))
                self.assertEqual([r["route"] for r in rows], ["preferred"] * 5)
                self.assertEqual([r["checked_success"] for r in rows], [True, False, True, True, True])

    def test_discovery_via_shell_and_absent_tools_do_not_get_route_credit(self):
        routes = [dispatched("find", "bash"), dispatched("bash"), dispatched("find"),
                  dispatched("verify", "bash"), dispatched("read")]
        rows = grade(report_with((route, True) for route in routes))
        self.assertEqual([r["route"] for r in rows], ["nonpreferred"] * 5)
        self.assertTrue(all(r["checked_success"] for r in rows))

    def test_no_routing_verdict_from_absent_partial_or_empty_request_evidence(self):
        routes = [None, dispatched("find", complete=False), tool_trace(requests=0),
                  tool_trace(offered=["verify"]), dispatched("bash", complete=False)]
        rows = grade(report_with((route, True) for route in routes))
        self.assertEqual([r["route"] for r in rows],
                         ["unknown", "unknown", "unknown", "nonpreferred", "unknown"])

    def test_targeted_test_and_build_need_the_expected_outputs(self):
        for spec in SUITE["cases"][-2:]:
            with self.subTest(case=spec["id"]), tempfile.TemporaryDirectory() as tmp:
                project = Path(tmp) / "project"
                shutil.copytree(ROOT.parents[1] / spec["project"], project)
                self.assertFalse(checks_pass(spec, project))
                if spec["id"] == "focused-preview-test":
                    def run_test(name):
                        subprocess.run([sys.executable, "-B", "-m", "unittest", name],
                                       cwd=project, check=True, capture_output=True, timeout=10)
                    run_test("test_samples.SampleTests.test_desktop")
                    self.assertTrue(checks_pass(spec, project))
                    run_test("test_samples")
                    self.assertFalse(checks_pass(spec, project), "running the whole suite is not focused")
                else:
                    subprocess.run([sys.executable, "-B", "build_catalog.py"], cwd=project,
                                   check=True, capture_output=True, timeout=10)
                    self.assertTrue(checks_pass(spec, project))


if __name__ == "__main__":
    unittest.main()
