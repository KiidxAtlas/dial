#!/usr/bin/env python3
"""Offline tests for release benchmark measurement; no compiler is invoked."""
import importlib.util
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest import mock

spec = importlib.util.spec_from_file_location('benchmark_release', Path(__file__).with_name('benchmark-release.py'))
benchmark = importlib.util.module_from_spec(spec)
spec.loader.exec_module(benchmark)


class BenchmarkTests(unittest.TestCase):
    def test_measure_known_durations_and_command_failure(self):
        with mock.patch.object(benchmark.time, 'perf_counter', side_effect=[1, 3, 4, 7]), \
             mock.patch.object(benchmark.subprocess, 'run') as run:
            self.assertEqual(benchmark.measure(['fixture'], 2), [2, 3])
            self.assertEqual(run.call_count, 2)
            run.assert_called_with(['fixture'], check=True, stdout=subprocess.DEVNULL)
        with mock.patch.object(benchmark.subprocess, 'run', side_effect=subprocess.CalledProcessError(1, 'fixture')):
            with self.assertRaises(subprocess.CalledProcessError):
                benchmark.measure(['fixture'])

    def test_both_lto_modes_preserve_features_and_measure_the_selected_artifact(self):
        for lto, expected in [('thin', 'thin'), ('off', 'false')]:
            with self.subTest(lto=lto), tempfile.TemporaryDirectory() as temporary:
                target_dir = Path(temporary)
                binary = target_dir / 'native-target/release/dial'
                binary.parent.mkdir(parents=True)
                binary.write_bytes(b'abc')
                with mock.patch.object(benchmark.subprocess, 'run') as run, \
                     mock.patch.object(benchmark.subprocess, 'check_output', side_effect=['dial fixture', 'rustc fixture', 'abc123', '']), \
                     mock.patch.object(benchmark.time, 'perf_counter', side_effect=[10, 15]), \
                     mock.patch.object(benchmark, 'measure', side_effect=[[0.1], [7, 3, 4, 1, 2, 6, 5]]):
                    report = benchmark.benchmark('native-target', lto, target_dir)
                command = run.call_args.args[0]
                env = run.call_args.kwargs['env']
                self.assertIn('--release', command)
                self.assertIn('--locked', command)
                self.assertNotIn('--no-default-features', command)
                self.assertEqual(env['CARGO_PROFILE_RELEASE_LTO'], expected)
                self.assertEqual(env['CARGO_TARGET_DIR'], str(target_dir.resolve()))
                self.assertEqual(report['build_seconds'], 5)
                self.assertEqual(report['binary_bytes'], 3)
                self.assertEqual(report['help_median_seconds'], 4)
                self.assertEqual(report['sha256'], 'ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad')
                self.assertFalse(report['dirty'])

    def test_shipping_profile_is_unchanged_and_benchmark_never_publishes(self):
        root = Path(__file__).resolve().parent.parent
        cargo = (root / 'Cargo.toml').read_text().split('[profile.release]', 1)[1].split('\n[', 1)[0]
        workflow = (root / '.github/workflows/release-benchmark.yml').read_text()
        self.assertIn('lto = "thin"', cargo)
        self.assertIn('workflow_dispatch:', workflow)
        self.assertNotIn('secrets.', workflow)
        self.assertNotIn('contents: write', workflow)
        self.assertNotIn('gh release', workflow)

    def test_ci_records_nextest_timings_and_make_preserves_both_profiles(self):
        root = Path(__file__).resolve().parent.parent
        ci = (root / '.github/workflows/ci.yml').read_text()
        self.assertIn('TEST_PROFILE=ci NEXTEST_PROFILE=ci', ci)
        self.assertIn('target/nextest/ci/junit.xml', ci)
        for profile in ('default', 'ci'):
            command = subprocess.check_output(['make', '-n', 'test-nextest', 'TEST_PROFILE=ci',
                                               f'NEXTEST_PROFILE={profile}'], cwd=root, text=True)
            self.assertIn(f'--cargo-profile ci --profile {profile}', command)
            self.assertIn('test --locked --workspace --doc --profile ci', command)


if __name__ == '__main__':
    unittest.main()
