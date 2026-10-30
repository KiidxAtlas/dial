"""Fixture synchronization contracts; real-terminal coverage stays in interactive_smoke."""
import types
import unittest
from unittest import mock

from interactive_smoke import DiscoveryGate, Handler, Terminal


class DiscoveryTests(unittest.TestCase):
    def handler(self, gate):
        handler = object.__new__(Handler)
        handler.path = "/v1/models"
        handler.server = types.SimpleNamespace(discovery=gate)
        handler.respond = mock.Mock()
        return handler

    def test_deadline_starts_only_after_scenario_is_armed(self):
        gate = DiscoveryGate()
        handler = self.handler(gate)
        steps = []
        gate.armed.wait = mock.Mock(side_effect=lambda: steps.append("armed"))

        def release(timeout):
            self.assertTrue(gate.requested.is_set())
            self.assertEqual(steps, ["armed"])
            self.assertEqual(timeout, 15)
            return True

        gate.release.wait = mock.Mock(side_effect=release)
        handler.do_GET()
        self.assertEqual(handler.respond.call_args.args[0]["data"],
                         [{"id": "mock-main"}, {"id": "mock-next"}])

    def test_request_keeps_its_scenario_gate(self):
        gate = DiscoveryGate()
        handler = self.handler(gate)
        replacement = DiscoveryGate()
        gate.armed.wait = mock.Mock(
            side_effect=lambda: setattr(handler.server, "discovery", replacement))
        gate.release.wait = mock.Mock(return_value=True)
        handler.do_GET()
        gate.release.wait.assert_called_once_with(15)
        self.assertFalse(replacement.requested.is_set())
        self.assertFalse(replacement.release.is_set())

    def test_armed_deadline_still_fails_closed(self):
        gate = DiscoveryGate()
        gate.armed.set()
        gate.release.wait = mock.Mock(return_value=False)
        handler = self.handler(gate)
        handler.do_GET()
        handler.respond.assert_called_once_with({"error": "fixture discovery deadline"}, 503)

    def test_cleanup_opens_even_an_unarmed_gate(self):
        gate = DiscoveryGate()
        gate.open()
        self.assertTrue(gate.armed.is_set())
        self.assertTrue(gate.release.is_set())


class CommandTests(unittest.TestCase):
    def test_enter_follows_visible_paste_and_guard_settle(self):
        terminal = object.__new__(Terminal)
        trace = mock.Mock()
        terminal.run = trace.run
        terminal.wait = trace.wait
        with mock.patch("interactive_smoke.time.sleep", trace.sleep):
            terminal.command("PTY-INPUT")
        self.assertEqual(trace.mock_calls, [
            mock.call.run("send-keys", "-t", "smoke", "-l", "\x1b[200~PTY-INPUT\x1b[201~"),
            mock.call.wait("PTY-INPUT", history=False),
            mock.call.sleep(0.5),
            mock.call.run("send-keys", "-t", "smoke", "Enter"),
        ])

    def test_unconsumed_paste_never_sends_enter(self):
        terminal = object.__new__(Terminal)
        terminal.run = mock.Mock()
        terminal.wait = mock.Mock(side_effect=AssertionError("paste was not rendered"))
        with self.assertRaisesRegex(AssertionError, "paste was not rendered"):
            terminal.command("PTY-INPUT")
        self.assertEqual(terminal.run.call_count, 1)


if __name__ == "__main__":
    unittest.main()
