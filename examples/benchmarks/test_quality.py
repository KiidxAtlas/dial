"""Fixture/verifier regressions; these are not model-quality measurements."""
import pathlib
import shutil
import subprocess
import tempfile
import tomllib
import unittest

ROOT = pathlib.Path(__file__).resolve().parent


def checks_pass(case, workspace):
    results = []
    for check in case["checks"]:
        if check["kind"] == "file_equals":
            path = workspace / check["path"]
            results.append(path.is_file() and path.read_bytes() == check["content"].encode())
        elif check["kind"] == "command":
            # Command checks may run a cold `cargo test`; 15s was too tight on shared CI runners.
            result = subprocess.run(check["command"], cwd=workspace, shell=True,
                                    capture_output=True, timeout=120)
            results.append(result.returncode == check.get("expect_exit", 0))
        else:
            raise AssertionError("quality cases require executable outcome checks")
    return all(results)


class QualityFixtureTests(unittest.TestCase):
    def test_unsolved_cases_fail_and_independent_solutions_pass(self):
        suite = tomllib.loads((ROOT / "quality.toml").read_text())
        for case in suite["cases"]:
            with self.subTest(case=case["id"]), tempfile.TemporaryDirectory() as temporary:
                workspace = pathlib.Path(temporary) / "project"
                shutil.copytree(ROOT / "quality-fixture", workspace,
                                ignore=shutil.ignore_patterns("__pycache__"))
                self.assertFalse(checks_pass(case, workspace))
                if case["id"] == "owning-code-and-boundaries":
                    path = workspace / "billing.py"
                    path.write_text(path.read_text().replace('item["price"] for', 'item["price"] * item["quantity"] for'))
                elif case["id"] == "feature-through-callers":
                    for name in ("settings.py", "reports.py"):
                        path = workspace / name
                        path.write_text(path.read_text().replace("DEFAULT_PAGE_SIZE", "PAGE_SIZE"))
                elif case["id"] == "environment-not-code-failure":
                    (workspace / "diagnosis.txt").write_bytes(b"environment")
                elif case["id"] == "current-evidence-over-stale-memory":
                    path = workspace / "checkout.py"
                    path.write_text(path.read_text().replace("DISCOUNT_FRACTION / 100", "DISCOUNT_FRACTION"))
                elif case["id"] == "iterative-feature-journey":
                    path = workspace / "billing.py"
                    path.write_text(path.read_text().replace(
                        '    return sum(item["price"] for item in items)',
                        '    if any(item["quantity"] < 0 for item in items):\n'
                        '        raise ValueError("quantity must be non-negative")\n'
                        '    return sum(item["price"] * item["quantity"] for item in items)'))
                elif case["id"] == "corrected-requirement-journey":
                    path = workspace / "reports.py"
                    path.write_text(path.read_text().replace(
                        "def page_count(rows):\n",
                        "def page_count(rows):\n    if rows is None:\n        return 0\n"))
                else:
                    self.fail("case has no independently tested solution")
                self.assertTrue(checks_pass(case, workspace))

    def test_changing_visible_tests_does_not_defeat_hidden_boundary_checks(self):
        suite = tomllib.loads((ROOT / "quality.toml").read_text())
        case = next(c for c in suite["cases"] if c["id"] == "owning-code-and-boundaries")
        with tempfile.TemporaryDirectory() as temporary:
            workspace = pathlib.Path(temporary) / "project"
            shutil.copytree(ROOT / "quality-fixture", workspace,
                            ignore=shutil.ignore_patterns("__pycache__"))
            (workspace / "test_billing.py").write_text("# Tests deleted by an incorrect solution.\n")
            self.assertFalse(checks_pass(case, workspace))


if __name__ == "__main__":
    unittest.main()
