"""Executable external graders; no model calls, services, package installation or fetching."""
import pathlib
import shutil
import subprocess
import tempfile
import tomllib
import unittest

from test_quality import checks_pass

ROOT = pathlib.Path(__file__).resolve().parent
REPO = ROOT.parent.parent


def reference_repair(case, workspace):
    if case['id'] == 'rust-workspace-nondefault-member':
        path = workspace / 'pricing/src/lib.rs'
        path.write_text(path.read_text().replace('Ok(price)', 'Ok(price * quantity)'))
        (workspace / 'diagnosis.txt').write_text('baseline-unrelated')
    elif case['id'] == 'typescript-boundary-javascript-caller':
        for name in ('settings.ts', 'pages.ts'):
            path = workspace / name
            path.write_text(path.read_text().replace('DEFAULT_PAGE_SIZE', 'PAGE_SIZE'))
        path = workspace / 'pages.ts'
        path.write_text(path.read_text().replace('return Math.ceil', 'if (rows === null) return 0;\n    return Math.ceil'))
    elif case['id'] == 'corrected-retracted-compaction-journey':
        for name in ('settings.py', 'reports.py'):
            path = workspace / name
            path.write_text(path.read_text().replace('DEFAULT_PAGE_SIZE', 'PAGE_SIZE'))
        path = workspace / 'reports.py'
        path.write_text(path.read_text().replace('def page_count(rows):\n', 'def page_count(rows):\n    if rows is None:\n        return 0\n'))
    else:
        raise AssertionError('case needs independent reference repair')


class RepresentativeFixtureTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.suite = tomllib.loads((ROOT / 'representative.toml').read_text())

    def require_runtime(self, case):
        if 'rust' in case['categories'] and shutil.which('cargo') is None:
            self.skipTest('unavailable: installed cargo toolchain required; no install attempted')
        if 'typescript' in case['categories']:
            if shutil.which('node') is None:
                self.skipTest('unavailable: Node with native TypeScript stripping required')
            probe = subprocess.run(['node', '--input-type=module', '-e',
                                    "import './settings.ts'"], cwd=ROOT / 'representative-ts',
                                   capture_output=True, timeout=10)
            if probe.returncode:
                self.skipTest('unavailable: Node does not support native TypeScript stripping')

    def test_originals_fail_reference_repairs_pass(self):
        for case in self.suite['cases']:
            with self.subTest(case=case['id']):
                self.require_runtime(case)
                with tempfile.TemporaryDirectory() as temp:
                    workspace = pathlib.Path(temp) / 'project'
                    shutil.copytree(REPO / case['project'], workspace,
                                    ignore=shutil.ignore_patterns('__pycache__', 'target'))
                    self.assertFalse(checks_pass(case, workspace))
                    reference_repair(case, workspace)
                    self.assertTrue(checks_pass(case, workspace))

    def test_visible_test_tampering_does_not_replace_independent_rust_acceptance(self):
        case = self.suite['cases'][0]
        self.require_runtime(case)
        with tempfile.TemporaryDirectory() as temp:
            workspace = pathlib.Path(temp) / 'project'
            shutil.copytree(REPO / case['project'], workspace)
            (workspace / 'diagnosis.txt').write_text('baseline-unrelated')
            with (workspace / 'pricing/src/lib.rs').open('a') as file:
                file.write('\n#[test] fn fake_success() { assert!(true); }\n')
            self.assertFalse(checks_pass(case, workspace))
            reference_repair(case, workspace)
            self.assertTrue(checks_pass(case, workspace))
            # The original unrelated failure remains after the accepted repair.
            baseline = subprocess.run(['cargo', 'test', '--offline', '--target-dir',
                                       str(pathlib.Path(temp) / 'target')], cwd=workspace,
                                      capture_output=True, timeout=30)
            self.assertNotEqual(baseline.returncode, 0)
            self.assertIn(b'known unrelated baseline failure', baseline.stdout + baseline.stderr)

    def test_journey_rejects_stale_correction_and_retracted_artifact(self):
        case = self.suite['cases'][2]
        self.assertGreaterEqual(len(case['turns']), 6)
        self.assertGreaterEqual(sum(t.get('compact_before', False) for t in case['turns']), 3)
        with tempfile.TemporaryDirectory() as temp:
            workspace = pathlib.Path(temp) / 'project'
            shutil.copytree(REPO / case['project'], workspace,
                            ignore=shutil.ignore_patterns('__pycache__'))
            reference_repair(case, workspace)
            self.assertTrue(checks_pass(case, workspace))
            (workspace / 'migration.txt').write_text('planned')
            self.assertFalse(checks_pass(case, workspace))
            (workspace / 'migration.txt').unlink()
            path = workspace / 'settings.py'
            path.write_text(path.read_text().replace('25', '50'))
            self.assertFalse(checks_pass(case, workspace))


if __name__ == '__main__':
    unittest.main()
