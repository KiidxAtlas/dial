"""Adversarial grader regressions for reliability.toml: wrong or partial fixes must fail.

Each wrong implementation is a plausible partial fix. Mutants that pass the visible unit tests
are the false-success traps; the acceptance command (the first check) must still reject them.
Offline only: no model, network or service is involved."""
import pathlib
import shutil
import subprocess
import sys
import tempfile
import tomllib
import unittest

ROOT = pathlib.Path(__file__).resolve().parent
FIXTURE = ROOT / "reliability-fixture"
SUITE = tomllib.loads((ROOT / "reliability.toml").read_text())
CASES = {case["id"]: case for case in SUITE["cases"]}

INITIAL_PARSE = "def parse_amount(text):\n    return int(float(text) * 100)\n"
INITIAL_EXPORT = '    return [f"{e[\'kind\']}:{e[\'cents\']}" for e in entries if e["kind"] == "charge"]\n'
SIGNED_EXPORT = ('    return [f"{e[\'kind\']}:{e[\'cents\'] * (-1 if e[\'kind\'] == \'refund\' else 1)}" '
                 'for e in entries]\n')
INITIAL_LOAD = ("        try:\n"
                "            entries.append({\"kind\": kind, \"cents\": parse_amount(amount)})\n"
                "        except ValueError:\n"
                "            continue\n")
FIXED_LOAD = "        entries.append({\"kind\": kind, \"cents\": parse_amount(amount)})\n"

# Correct fix per case. Each case's acceptance command depends only on its own fix.
FIXES = {
    "amount-exact-decimal": [("ledger.py", INITIAL_PARSE,
        "import re\n\n_AMOUNT = re.compile(r\"(0|[1-9][0-9]*)(?:\\.([0-9]{1,2}))?\")\n\n\n"
        "def parse_amount(text):\n"
        "    match = _AMOUNT.fullmatch(text) if isinstance(text, str) else None\n"
        "    if match is None:\n"
        "        raise ValueError(f\"invalid amount: {text!r}\")\n"
        "    whole, fraction = match.groups(default=\"0\")\n"
        "    return int(whole) * 100 + int(fraction.ljust(2, \"0\"))\n")],
    "ledger-export-keeps-refunds": [("ledger.py", INITIAL_EXPORT, SIGNED_EXPORT)],
    "malformed-row-fails-closed": [("ledger.py", INITIAL_LOAD, FIXED_LOAD)],
}

# (case, name, file, anchor, replacement, visible_tests_pass, base).
# base "fix" applies on top of the case's correct fix; "initial" starts from the untouched fixture.
# Anchors must occur exactly once in the file they edit.
MUTANTS = [
    ("amount-exact-decimal", "float-round-accepts-extra-digits", "ledger.py", INITIAL_PARSE,
     "def parse_amount(text):\n    return int(round(float(text) * 100))\n", True, "initial"),
    ("amount-exact-decimal", "decimal-accepts-exponent", "ledger.py", INITIAL_PARSE,
     "import re\nfrom decimal import Decimal\n\n"
     "_AMOUNT = re.compile(r\"(0|[1-9][0-9]*)(?:\\.([0-9]{1,2}))?(?:[eE][+-]?[0-9]+)?\")\n\n\n"
     "def parse_amount(text):\n"
     "    match = _AMOUNT.fullmatch(text) if isinstance(text, str) else None\n"
     "    if match is None:\n"
     "        raise ValueError(f\"invalid amount: {text!r}\")\n"
     "    return int(Decimal(text) * 100)\n",
     True, "initial"),
    ("amount-exact-decimal", "returns-dollars-not-cents", "ledger.py", INITIAL_PARSE,
     "def parse_amount(text):\n    return float(text)\n", False, "initial"),
    ("ledger-export-keeps-refunds", "unsigned-refunds", "ledger.py", SIGNED_EXPORT,
     '    return [f"{e[\'kind\']}:{e[\'cents\']}" for e in entries]\n', True, "fix"),
    ("ledger-export-keeps-refunds", "reversed-input-order", "ledger.py", SIGNED_EXPORT,
     '    return [f"{e[\'kind\']}:{e[\'cents\'] * (-1 if e[\'kind\'] == \'refund\' else 1)}" '
     'for e in reversed(entries)]\n', True, "fix"),
    ("ledger-export-keeps-refunds", "export-fixed-total-sign-flipped", "ledger.py",
     'return sum(e["cents"] if e["kind"] == "charge" else -e["cents"] for e in entries)',
     'return sum(e["cents"] for e in entries)', False, "fix"),
    ("malformed-row-fails-closed", "wrong-exception-type", "ledger.py", FIXED_LOAD,
     "        try:\n            entries.append({\"kind\": kind, \"cents\": parse_amount(amount)})\n"
     "        except ValueError:\n            raise RuntimeError(\"bad row\")\n", True, "fix"),
    ("malformed-row-fails-closed", "rejects-all-refunds", "ledger.py", FIXED_LOAD,
     "        if kind == \"refund\":\n            raise ValueError(\"refunds unsupported\")\n" + FIXED_LOAD,
     True, "fix"),
]


def apply(workspace, edits):
    for name, anchor, replacement in edits:
        path = workspace / name
        text = path.read_text()
        if text.count(anchor) != 1:
            raise AssertionError(f"edit anchor not unique in {name}: {anchor[:40]!r}")
        path.write_text(text.replace(anchor, replacement))


def run_checks(case, workspace):
    """Per-check outcomes in declaration order; the first check is the acceptance command."""
    results = []
    for check in case["checks"]:
        completed = subprocess.run(check["command"], cwd=workspace, shell=True,
                                   capture_output=True, timeout=120)
        results.append(completed.returncode == check.get("expect_exit", 0))
    return results


class ReliabilityFixtureTests(unittest.TestCase):
    def setUp(self):
        self._temp = tempfile.TemporaryDirectory()
        self.addCleanup(self._temp.cleanup)

    def workspace(self, name):
        path = pathlib.Path(self._temp.name) / name
        shutil.copytree(FIXTURE, path, ignore=shutil.ignore_patterns("__pycache__"))
        return path

    def test_unsolved_cases_fail_and_independent_solutions_pass(self):
        for case_id, case in CASES.items():
            with self.subTest(case=case_id):
                workspace = self.workspace(case_id)
                self.assertFalse(all(run_checks(case, workspace)))
                apply(workspace, FIXES[case_id])
                self.assertEqual(run_checks(case, workspace), [True, True])

    def test_wrong_fixes_fail_acceptance_even_when_visible_tests_pass(self):
        for case_id, name, filename, anchor, replacement, visible_passes, base in MUTANTS:
            with self.subTest(case=case_id, mutant=name):
                workspace = self.workspace(f"{case_id}-{name}")
                if base == "fix":
                    apply(workspace, FIXES[case_id])
                apply(workspace, [(filename, anchor, replacement)])
                acceptance, visible = run_checks(CASES[case_id], workspace)
                self.assertFalse(acceptance)
                self.assertEqual(visible, visible_passes)

    def test_exponent_mutant_is_rejected_only_for_exponent_notation(self):
        mutant = next(m for m in MUTANTS if m[1] == "decimal-accepts-exponent")
        workspace = self.workspace("exponent-mutant")
        apply(workspace, [("ledger.py", mutant[3], mutant[4])])
        acceptance = CASES["amount-exact-decimal"]["checks"][0]["command"]
        completed = subprocess.run(acceptance, cwd=workspace, shell=True,
                                   capture_output=True, text=True, timeout=120)
        self.assertNotEqual(completed.returncode, 0)
        self.assertEqual(completed.stderr.strip().splitlines()[-1], "accepted invalid amount '1e3'")
        probe = ("import ledger\n"
                 "for text in ['', '-1.00', '1.234', 'abc', 'nan', ' 1.00', '1.', '.5']:\n"
                 "    try:\n"
                 "        ledger.parse_amount(text)\n"
                 "    except ValueError:\n"
                 "        continue\n"
                 "    raise SystemExit('accepted ' + repr(text))\n")
        rejected = subprocess.run([sys.executable, "-c", probe], cwd=workspace,
                                  capture_output=True, text=True, timeout=120)
        self.assertEqual(rejected.returncode, 0, rejected.stderr)

    def test_replacing_visible_tests_cannot_defeat_the_acceptance_command(self):
        workspace = self.workspace("stub-tests")
        apply(workspace, FIXES["amount-exact-decimal"])
        apply(workspace, [("ledger.py", "_AMOUNT.fullmatch(text)", "_AMOUNT.match(text)")])
        (workspace / "test_ledger.py").write_text(
            "import unittest\n\n\nclass Stub(unittest.TestCase):\n    def test_nothing(self):\n        pass\n")
        self.assertFalse(run_checks(CASES["amount-exact-decimal"], workspace)[0])

    def test_silently_dropped_row_fails_even_though_visible_tests_pass(self):
        workspace = self.workspace("dropped-row")
        case = CASES["malformed-row-fails-closed"]
        apply(workspace, FIXES["malformed-row-fails-closed"])
        self.assertEqual(run_checks(case, workspace), [True, True])
        apply(workspace, [("ledger.py", FIXED_LOAD, INITIAL_LOAD)])
        self.assertEqual(run_checks(case, workspace), [False, True])


if __name__ == "__main__":
    unittest.main()
