import json
import os
import select
import subprocess
import sys
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest import mock

import dial_server
from dial_server import ToolCallFormatter


class EngineTests(unittest.TestCase):
    def test_vision_runner_uses_its_own_engine_and_compatible_steering_boundary(self):
        server = SimpleNamespace(main=mock.Mock())
        steering = SimpleNamespace(install_vision_server=mock.Mock())
        with mock.patch.dict(os.environ, {"DIAL_MODEL_ENGINE": "mlx-vlm"}), \
             mock.patch.dict(sys.modules, {"mlx_vlm": SimpleNamespace(server=server),
                                           "mlx_lm": None, "dial_steering": steering}), \
             mock.patch.object(dial_server, "watch_parent"):
            dial_server.main()
        server.main.assert_called_once_with()
        steering.install_vision_server.assert_called_once_with(server, None)


class ParentLifetimeTests(unittest.TestCase):
    def test_model_processes_exit_when_their_dial_parent_is_killed(self):
        for module in ["dial_server", "dial_worker"]:
            with self.subTest(module=module):
                child_code = f"import {module}; {module}.watch_parent(); print('ready', flush=True); import time; time.sleep(30)"
                parent_code = (
                    "import os, subprocess, sys, time\n"
                    "env = dict(os.environ, DIAL_PARENT_PID=str(os.getpid()))\n"
                    f"child = subprocess.Popen([sys.executable, '-B', '-c', {child_code!r}], env=env)\n"
                    "print(child.pid, flush=True)\n"
                    "time.sleep(30)\n"
                )
                parent = subprocess.Popen([sys.executable, "-B", "-c", parent_code], stdout=subprocess.PIPE,
                                          cwd=Path(__file__).resolve().parent, bufsize=0)
                child_pid = None
                finished = False
                try:
                    for _ in range(2):
                        self.assertTrue(select.select([parent.stdout], [], [], 5)[0], "model did not start")
                        line = parent.stdout.readline().decode().strip()
                        if line.isdigit(): child_pid = int(line)
                        else: self.assertEqual(line, "ready")
                    parent.kill()
                    # The model inherits this pipe. EOF proves it also exited after its parent died.
                    parent.communicate(timeout=5)
                    finished = True
                finally:
                    if parent.poll() is None: parent.kill()
                    parent.wait()
                    if child_pid is not None and not finished:
                        try: os.kill(child_pid, 9)
                        except ProcessLookupError: pass


class FormatterTests(unittest.TestCase):
    def format(self, text, streaming=False):
        return ToolCallFormatter(lambda text, tools: json.loads(text), [], streaming)([text])

    def test_missing_arguments_are_explicitly_non_executable(self):
        calls = self.format('{"name":"bash"}')
        self.assertEqual(calls[0]['function'], {'name':'bash', 'arguments':'null'})

    def test_invalid_types_and_truncated_output_are_not_server_exceptions(self):
        for text in ['{', 'null', '[]', '"text"', '{"name":3,"arguments":{}}', '{"name":"read","arguments":[]}']:
            with self.subTest(text=text):
                call = self.format(text)[0]
                self.assertEqual(call['function']['arguments'], 'null')

    def test_valid_call_and_failed_call_keep_distinct_stream_indices(self):
        calls = self.format('[{"name":"read","arguments":{"path":"a.txt"}},{"name":"bash"}]', True)
        self.assertEqual([c['index'] for c in calls], [0, 1])
        self.assertEqual(json.loads(calls[0]['function']['arguments']), {'path':'a.txt'})
        self.assertEqual(calls[1]['function']['arguments'], 'null')
        self.assertNotEqual(calls[0]['id'], calls[1]['id'])

    def test_formatting_does_not_mutate_parser_owned_objects(self):
        parsed = {'id':'old', 'name':'read', 'arguments':{'path':'a.txt'}}
        formatter = ToolCallFormatter(lambda text, tools: parsed, [])
        formatter(['x'])
        self.assertEqual(parsed, {'id':'old', 'name':'read', 'arguments':{'path':'a.txt'}})

    def test_backend_failures_are_not_mislabelled_as_model_errors(self):
        def fail(text, tools):
            raise MemoryError('backend allocation failed')
        with self.assertRaises(MemoryError):
            ToolCallFormatter(fail, [])(['anything'])


if __name__ == '__main__':
    unittest.main()
