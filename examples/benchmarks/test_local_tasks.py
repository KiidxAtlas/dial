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
SNAKE = "snake-harness-recovery"

# Model sessions may repair the live fixture. Keep these unit-test inputs deterministic.
SNAKE_STARTING_FILES = {
    "snake.py": "def advance(body, direction):\n"
                "    y, x = body[-1]\n"
                "    dy, dx = direction\n"
                "    return [(y + dy, x + dx), *body[:-1]]\n",
    "test_snake.py": """import unittest
from snake import advance


class SnakeTest(unittest):
    def test_moves_from_head(self):
        body = [(3, 4), (3, 3), (3, 2)]
        self.assertEqual(advance(body, (0, 1)), [(3, 5), (3, 4), (3, 3)])
        self.assertEqual(body, [(3, 4), (3, 3), (3, 2)])

    def test_single_segment(self):
        self.assertEqual(advance([(2, 2)], (-1, 0)), [(1, 2)])
""",
    "RUN.md": "",
}

REFERENCE = {
    "research-cited-comparison": {"answer.md": "source_a.md: Alpha grew 3 percent in 2025.\n"
                                               "source_b.md: Beta shrank 2 percent in 2025.\n"},
    "deferred-tool-discovery": {"ledger.py": "def bounded_total(rows):\n    return min(sum(rows), 250)\n\n\n"
                                             "def total(rows):\n    return sum(rows)\n"},
    "unavailable-tool-recovery": {"summary.txt": "alpha beta\n"},
    "read-edit-test-program": {"calc.py": "def add(a, b):\n    return a + b\n"},
    DENIAL: {},
    SNAKE: {
        "snake.py": SNAKE_STARTING_FILES["snake.py"].replace("body[-1]", "body[0]"),
        "test_snake.py": SNAKE_STARTING_FILES["test_snake.py"].replace(
            "class SnakeTest(unittest):", "class SnakeTest(unittest.TestCase):"),
        "RUN.md": "python3 -m unittest -q test_snake.py\n",
    },
}


def command_outcomes(case_id, files=None, outside=None):
    """Runs each command check of the case in a disposable copy. Returns one bool per command check, in order."""
    with tempfile.TemporaryDirectory() as tmp:
        project = pathlib.Path(tmp) / "project"
        shutil.copytree(FIXTURE, project, ignore=shutil.ignore_patterns("__pycache__"))
        baseline = SNAKE_STARTING_FILES if case_id == SNAKE else {}
        for name, content in {**baseline, **(files or {})}.items():
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
    def test_suite_declares_each_task_on_the_local_fixture(self):
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

    def test_snake_requires_both_the_program_and_harness_repairs(self):
        reference = REFERENCE[SNAKE]
        for missing in ("snake.py", "test_snake.py", "RUN.md"):
            with self.subTest(missing=missing):
                partial = {name: content for name, content in reference.items() if name != missing}
                self.assertFalse(all(command_outcomes(SNAKE, partial)))

    def test_snake_independent_grader_rejects_weakened_tests_and_input_mutation(self):
        weakened = {**REFERENCE[SNAKE], "snake.py": SNAKE_STARTING_FILES["snake.py"],
                    "test_snake.py": "import unittest\n"}
        self.assertFalse(command_outcomes(SNAKE, weakened)[0])
        fixed_with_weakened_tests = {**REFERENCE[SNAKE], "test_snake.py": "import unittest\n"}
        outcomes = command_outcomes(SNAKE, fixed_with_weakened_tests)
        self.assertTrue(outcomes[0], "the movement grader is independent of visible tests")
        self.assertFalse(all(outcomes), "removed assertions violate the task even with fixed code")
        replaced_assertions = {**REFERENCE[SNAKE], "test_snake.py":
                               REFERENCE[SNAKE]["test_snake.py"].replace("self.assertEqual(", "self.assertNotEqual(")}
        self.assertFalse(command_outcomes(SNAKE, replaced_assertions)[2])
        mutating = {**REFERENCE[SNAKE], "snake.py":
                    "def advance(body, direction):\n"
                    "    y, x = body[0]\n"
                    "    dy, dx = direction\n"
                    "    body.insert(0, (y + dy, x + dx))\n"
                    "    body.pop()\n"
                    "    return body\n"}
        self.assertFalse(command_outcomes(SNAKE, mutating)[0])

    def test_snake_rejects_skipped_assertions(self):
        skipped = {**REFERENCE[SNAKE], "test_snake.py":
                   REFERENCE[SNAKE]["test_snake.py"].replace(
                       "    def test_", "    @unittest.skip('skip all')\n    def test_")}
        self.assertFalse(all(command_outcomes(SNAKE, skipped)))

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

    def test_deferred_discovery_needs_a_real_deferred_tool_path(self):
        case = CASES["deferred-tool-discovery"]
        self.assertIn("ast_edit", case["prompt"])
        tools = {c.get("tool") for c in case["checks"]} - {None}
        self.assertEqual(tools, {"tool_search", "ast_edit"})
        # tool_load and ast_search were folded into tool_search and grep; checks on them cannot pass.
        self.assertFalse(tools & {"tool_load", "ast_search"})

    def test_deferred_discovery_rejects_a_changed_neighbour(self):
        broken = REFERENCE["deferred-tool-discovery"]["ledger.py"].replace("return sum(rows)", "return 250")
        self.assertFalse(command_outcomes("deferred-tool-discovery", {"ledger.py": broken})[0])

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
