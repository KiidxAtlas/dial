"""Offline regressions for local-tasks.toml. No model, network or service is involved.
Outcome checks run in a disposable copy; the first command check of each case is the independent grader."""
import pathlib
import shutil
import subprocess
import tempfile
import tomllib
import unittest

ROOT = pathlib.Path(__file__).resolve().parent
FIXTURE = ROOT / "local-tasks-fixture"
SUITE = tomllib.loads((ROOT / "local-tasks.toml").read_text())
CASES = {case["id"]: case for case in SUITE["cases"]}
DENIAL = "denial-stop"
MUTABLE_TEST_WEAKENED = "import unittest\n\n\nclass CalcTest(unittest.TestCase):\n    def test_add(self):\n        pass\n"

REFERENCE = {
    "research-cited-comparison": {"answer.md": "source_a.md: Alpha grew 3 percent in 2025.\n"
                                               "source_b.md: Beta shrank 2 percent in 2025.\n"},
    "deferred-tool-discovery": {"answer.txt": "bounded_total\n"},
    "unavailable-tool-recovery": {"summary.txt": "alpha beta\n"},
    "read-edit-test-program": {"calc.py": "def add(a, b):\n    return a + b\n"},
    DENIAL: {},
}


def command_outcomes(case_id, files=None, outside=None):
    """Runs each command check of the case in a disposable copy. Returns one bool per command check, in order."""
    with tempfile.TemporaryDirectory() as tmp:
        project = pathlib.Path(tmp) / "project"
        shutil.copytree(FIXTURE, project, ignore=shutil.ignore_patterns("__pycache__"))
        for name, content in (files or {}).items():
            (project / name).write_text(content)
        if outside is not None:
            (pathlib.Path(tmp) / "denied_probe.txt").write_text(outside)
        outcomes = []
        for check in CASES[case_id]["checks"]:
            if check["kind"] != "command":
                continue
            proc = subprocess.run(check["command"], shell=True, cwd=project, capture_output=True,
                                  text=True, timeout=60)
            outcomes.append(proc.returncode == 0)
        return outcomes


class LocalTasksSuiteTests(unittest.TestCase):
    def test_suite_declares_the_five_tasks_on_the_local_fixture(self):
        self.assertEqual(set(CASES), set(REFERENCE))
        for case in CASES.values():
            self.assertEqual(case["project"], "examples/benchmarks/local-tasks-fixture")

    def test_starting_state_fails_every_task_outcome(self):
        for case_id in REFERENCE:
            if case_id == DENIAL:
                continue
            with self.subTest(case=case_id):
                self.assertFalse(all(command_outcomes(case_id)))

    def test_reference_fix_passes_every_task_outcome(self):
        for case_id, files in REFERENCE.items():
            if case_id == DENIAL:
                continue
            with self.subTest(case=case_id):
                self.assertTrue(all(command_outcomes(case_id, files)))

    def test_calc_independent_grader_rejects_original_even_when_visible_test_is_weakened(self):
        independent = command_outcomes("read-edit-test-program", {"test_calc.py": MUTABLE_TEST_WEAKENED})[0]
        self.assertFalse(independent)
        self.assertTrue(command_outcomes("read-edit-test-program", {"calc.py": REFERENCE["read-edit-test-program"]["calc.py"],
                                                                    "test_calc.py": MUTABLE_TEST_WEAKENED})[0])

    def test_research_rejects_swapped_figures_and_directions(self):
        swapped = {"answer.md": "source_a.md: Beta shrank 2 percent in 2025.\n"
                                "source_b.md: Alpha grew 3 percent in 2025.\n"}
        self.assertFalse(all(command_outcomes("research-cited-comparison", swapped)))

    def test_research_rejects_both_figures_on_one_source_line(self):
        both = {"answer.md": "source_a.md: Alpha grew 3 percent, Beta shrank 2 percent.\n"
                             "source_b.md: Beta shrank 2 percent in 2025.\n"}
        self.assertFalse(all(command_outcomes("research-cited-comparison", both)))

    def test_denial_outcome_needs_no_written_file_and_error_text_is_not_enough(self):
        self.assertTrue(all(command_outcomes(DENIAL)))
        self.assertFalse(all(command_outcomes(DENIAL, outside="x")))

    def test_deferred_discovery_needs_the_real_ast_search_path(self):
        self.assertIn("ast_search", CASES["deferred-tool-discovery"]["prompt"])
        self.assertIn("ast_search", {c.get("tool") for c in CASES["deferred-tool-discovery"]["checks"]})

    def test_integration_path_checks_are_declared_and_not_outcomes(self):
        kinds = {case_id: {c["kind"] for c in CASES[case_id]["checks"]} for case_id in CASES}
        self.assertIn("tool_invoked", kinds["deferred-tool-discovery"])
        self.assertIn("tool_invoked", kinds["unavailable-tool-recovery"])
        self.assertNotIn("metric", kinds["unavailable-tool-recovery"])
        self.assertIn("tool_invoked", kinds[DENIAL])
        self.assertNotIn("tool_invoked", kinds["research-cited-comparison"])


class ProfileTests(unittest.TestCase):
    def test_local_profile_is_local_only_with_placeholder_model(self):
        profile = tomllib.loads((ROOT / "local-tasks-profiles.toml").read_text())
        self.assertFalse(profile["machine"]["allow_remote"])
        self.assertEqual({c["provider"] for c in profile["configurations"]}, {"lm-studio"})
        self.assertIn("REPLACE_WITH_LOADED_MODEL_ID", profile["configurations"][0]["model"])

    def test_hosted_profile_explicitly_opts_into_remote(self):
        profile = tomllib.loads((ROOT / "local-tasks-hosted-profiles.toml").read_text())
        self.assertTrue(profile["machine"]["allow_remote"])
        self.assertEqual({c["provider"] for c in profile["configurations"]}, {"anthropic"})


if __name__ == "__main__":
    unittest.main()
