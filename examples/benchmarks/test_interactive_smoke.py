"""Fixture synchronization contracts; real-terminal coverage stays in interactive_smoke."""
import io
import json
import pathlib
import tempfile
import socket
import threading
import types
import unittest
from unittest import mock
import urllib.error
import urllib.request

from interactive_smoke import (DiscoveryGate, Handler, Provider, Terminal,
                               assert_no_project_edits, assert_stop_receipt, measured_case)


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


class ScriptedProviderTests(unittest.TestCase):
    def setUp(self):
        self.server = types.SimpleNamespace(
            lock=threading.Lock(), chat_requests=0, diagnostic_requests=0,
            auth_requests=0, auth_repaired=threading.Event(), busy_requests=[],
            busy_release=threading.Event(), redirect_release=threading.Event(),
            redirect_disconnected=threading.Event(), redirect_requests=[], job_starts=0)

    def post(self, messages, tools=()):
        body = json.dumps({"messages": messages, "tools": tools}).encode()
        handler = object.__new__(Handler)
        handler.path = "/v1/chat/completions"
        handler.headers = {"Content-Length": str(len(body))}
        handler.rfile = io.BytesIO(body)
        handler.server = self.server
        handler.respond = mock.Mock()
        handler.gated_stream = mock.Mock()
        handler.do_POST()
        return handler

    def test_coding_probe_roundtrip_echoes_only_the_actual_receipt(self):
        tools = [{"type": "function", "function": {"name": "dial_diagnostic_echo"}}]
        prompt = [{"role": "user", "content": "synthetic coding compatibility probe"}]
        first = self.post(prompt, tools).respond.call_args.args[0]["choices"][0]
        self.assertEqual(first["finish_reason"], "tool_calls")
        call = first["message"]["tool_calls"][0]
        self.assertEqual(call["function"]["name"], "dial_diagnostic_echo")
        self.assertEqual(json.loads(call["function"]["arguments"]), {"value": "dial-probe"})
        # This secret exists only in the synthetic tool result, not the original prompt.
        second = self.post(prompt + [first["message"], {"role": "tool", "tool_call_id": call["id"],
                           "content": json.dumps({"receipt": "test-only-random-receipt"})}], tools)
        answer = second.respond.call_args.args[0]["choices"][0]
        self.assertEqual(answer["message"], {"role": "assistant", "content": "test-only-random-receipt"})
        self.assertEqual(answer["finish_reason"], "stop")
        self.assertEqual((self.server.chat_requests, self.server.diagnostic_requests), (2, 2))

    def test_expired_credential_requires_explicit_retry_after_repair(self):
        prompt = [{"role": "user", "content": "PTY-EXPIRED-CREDENTIAL"}]
        rejected = self.post(prompt)
        self.assertEqual(rejected.respond.call_args.args[1], 401)
        self.assertEqual(self.server.auth_requests, 1)
        self.server.auth_repaired.set()
        self.assertEqual(self.server.auth_requests, 1)
        repaired = self.post(prompt)
        self.assertEqual(repaired.respond.call_args.args[0]["choices"][0]["message"]["content"],
                         "AUTH-RECOVERY-EXPLICIT-RETRY")
        self.assertEqual((self.server.chat_requests, self.server.auth_requests), (2, 2))

    def test_busy_followup_is_counted_separately_from_held_main(self):
        main = self.post([{"role": "user", "content": "PTY-BUSY-MAIN"}])
        main.gated_stream.assert_called_once_with(1, "BUSY-MAIN-PREFIX",
                                                 [(self.server.busy_release, "BUSY-MAIN-END")])
        main.respond.assert_not_called()
        followup = self.post([{"role": "user", "content": "PTY-BUSY-MAIN"},
                              {"role": "user", "content": "PTY-BUSY-FOLLOW-UP"}])
        self.assertEqual(followup.respond.call_args.args[0]["choices"][0]["message"]["content"],
                         "BUSY-FOLLOW-UP-ADMITTED")
        self.assertEqual(self.server.busy_requests, ["PTY-BUSY-MAIN", "PTY-BUSY-FOLLOW-UP"])

    def test_redirect_uses_latest_instruction_instead_of_replaying_held_main(self):
        prompt = [{"role": "user", "content": "PTY-REDIRECT-MAIN"}]
        main = self.post(prompt)
        main.gated_stream.assert_called_once_with(1, "REDIRECT-MAIN-PREFIX",
            [(self.server.redirect_release, "REDIRECT-STALE-END")], self.server.redirect_disconnected)
        main.respond.assert_not_called()
        result = self.post(prompt + [{"role": "user", "content": "PTY-REDIRECT-NOW"}])
        self.assertEqual(result.respond.call_args.args[0]["choices"][0]["message"]["content"],
                         "REDIRECT-CURRENT-INSTRUCTION-ANSWER")
        self.assertEqual(self.server.redirect_requests, ["PTY-REDIRECT-MAIN", "PTY-REDIRECT-NOW"])

    def test_os_job_script_starts_once_then_acknowledges_tool_receipt(self):
        prompt = [{"role": "user", "content": "PTY-OS-JOB"}]
        # Without bg in the offer, the script loads it rather than calling an unavailable tool.
        load = self.post(prompt).respond.call_args.args[0]["choices"][0]["message"]["tool_calls"][0]
        self.assertEqual((load["function"]["name"], json.loads(load["function"]["arguments"])),
                         ("tool_load", {"names": ["bg"]}))
        self.assertEqual(self.server.job_starts, 0)
        offered = [{"type": "function", "function": {"name": "bg"}}]
        first = self.post(prompt, offered).respond.call_args.args[0]["choices"][0]
        call = first["message"]["tool_calls"][0]
        self.assertEqual(call["function"]["name"], "bg")
        arguments = json.loads(call["function"]["arguments"])
        self.assertEqual((arguments["action"], arguments["name"]), ("start", "pty-os-job"))
        self.assertIn("exec sleep 120", arguments["command"])
        result = self.post(prompt + [first["message"], {"role": "tool", "tool_call_id": call["id"],
                           "content": "pty-os-job: running; OS-JOB-OUTPUT-KEPT"}], offered)
        self.assertEqual(result.respond.call_args.args[0]["choices"][0]["message"]["content"], "OS-JOB-STARTED")
        self.assertEqual(self.server.job_starts, 1)


class RefusedEndpointTests(unittest.TestCase):
    def test_closed_socket_refuses_tcp_then_same_endpoint_serves_http(self):
        server = Provider(listening=False)
        thread = None
        try:
            with self.assertRaises(ConnectionRefusedError):
                socket.create_connection(server.server_address, timeout=1)
            self.assertEqual(server.chat_requests, 0)
            server.repair_listener()
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            # A real HTTP response is deliberately different from the refused TCP phase.
            url = f"http://127.0.0.1:{server.server_port}/not-found"
            with self.assertRaises(urllib.error.HTTPError) as response:
                urllib.request.urlopen(url, timeout=2)
            self.assertEqual(response.exception.code, 404)
            response.exception.close()
            self.assertEqual(server.chat_requests, 0)
        finally:
            if thread:
                server.shutdown()
                thread.join(timeout=5)
            server.server_close()


class StopReceiptTests(unittest.TestCase):
    def test_stop_receipt_does_not_confuse_request_exit_and_dismiss(self):
        for receipt in ("stop requested for pty-os-job; termination not yet observed",
                        "pty-os-job: killed (signal 9); terminal history and output retained"):
            with self.subTest(receipt=receipt):
                assert_stop_receipt(receipt)
        for receipt in ("dismissed pty-os-job from job history", "Running", "Stopped", "killed"):
            with self.subTest(receipt=receipt), self.assertRaises(AssertionError):
                assert_stop_receipt(receipt)


class ProjectBoundaryTests(unittest.TestCase):
    def test_only_startup_target_leases_are_runtime_not_project_edits(self):
        with tempfile.TemporaryDirectory() as directory:
            project = pathlib.Path(directory)
            leases = project / ".tmp/cargo-target/.leases"
            leases.mkdir(parents=True)
            (leases / "session").write_text("lease")
            assert_no_project_edits(project)
            for relative in ("unexpected.txt", ".tmp/changed.txt"):
                path = project / relative
                path.write_text("model edit")
                with self.subTest(relative=relative), self.assertRaises(AssertionError):
                    assert_no_project_edits(project)
                path.unlink()


class MeasurementTests(unittest.TestCase):
    def test_reports_request_delta_elapsed_and_available_correction_latency(self):
        server = types.SimpleNamespace(chat_requests=7)
        def scenario(server, binary, captures):
            server.chat_requests += 2
            return {"explicit_retry_to_answer_ms": 12.5}
        with mock.patch("interactive_smoke.time.monotonic", side_effect=[10, 10.125]):
            result = measured_case("auth", scenario, server, "binary", None)
        self.assertEqual(result, {"name": "auth", "elapsed_ms": 125.0, "chat_requests": 2,
                                  "explicit_retry_to_answer_ms": 12.5})


if __name__ == "__main__":
    unittest.main()
