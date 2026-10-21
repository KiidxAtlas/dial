"""Offline integration contract tests. Never connect, spawn services, install or call providers."""
import copy
import pathlib
import shutil
import tempfile
import tomllib
import unittest
from test_quality import checks_pass

ROOT = pathlib.Path(__file__).resolve().parent
REPO = ROOT.parent.parent


class IntegrationFixtureTests(unittest.TestCase):
    def test_contracts_have_path_coverage_and_separate_independent_outcomes(self):
        for lane, tool in [('browser', 'browser'), ('delegation', 'task'), ('mcp', 'mcp__fixture__multiply')]:
            suite = tomllib.loads((ROOT / f'integration-{lane}.toml').read_text())
            for case in suite['cases']:
                self.assertIn(lane, case['categories'])
                self.assertEqual([c['tool'] for c in case['checks'] if c['kind'] == 'tool_invoked'], [tool])
                self.assertTrue(any(c['kind'] in ('command', 'file_equals') for c in case['checks']))
                self.assertTrue((REPO / case['project']).is_dir())

    def test_reference_repairs_pass_outcomes_without_claiming_integration_was_exercised(self):
        for lane in ('browser', 'delegation', 'mcp'):
            if lane == 'browser' and shutil.which('node') is None:
                continue  # The browser is not needed; the optional JS-only grader cannot run.
            case = tomllib.loads((ROOT / f'integration-{lane}.toml').read_text())['cases'][0]
            case = copy.deepcopy(case)
            case['checks'] = [c for c in case['checks'] if c['kind'] != 'tool_invoked']
            with self.subTest(lane=lane), tempfile.TemporaryDirectory() as temp:
                workspace = pathlib.Path(temp) / 'project'
                shutil.copytree(REPO / case['project'], workspace,
                                ignore=shutil.ignore_patterns('__pycache__'))
                self.assertFalse(checks_pass(case, workspace))
                if lane == 'browser':
                    path = workspace / 'price.mjs'
                    path.write_text(path.read_text().replace('return price;', 'return price * quantity;'))
                elif lane == 'delegation':
                    path = workspace / 'billing.py'
                    path.write_text(path.read_text().replace('item["price"] for', 'item["price"] * item["quantity"] for'))
                else:
                    (workspace / 'service-result.txt').write_text('437')
                self.assertTrue(checks_pass(case, workspace))


if __name__ == '__main__':
    unittest.main()
