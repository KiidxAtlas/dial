#!/usr/bin/env python3
"""Real-PTY Dial workflow regression using an isolated tmux server and local provider.

No external model, credentials, installs or user configuration. Captures are actual
terminal text, NOT pixel screenshots or proof of color/visual appearance.
Delayed SSE covers queue management, refused restart and help during a live turn.
Run after building: python3 examples/benchmarks/interactive_smoke.py --dial target/debug/dial
"""
import argparse
from contextlib import ExitStack
import base64
import http.server
import json
import os
import pathlib
import re
import shlex
import signal
import socket
import subprocess
import tempfile
import threading
import time
import tomllib

PIXEL = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aY4cAAAAASUVORK5CYII="


def unsupported_probe(handler):
    """Explicit negative responses to Dial's optional local-runtime metadata probes."""
    if handler.path.rstrip("/") not in {
        "/props", "/api/ps", "/api/show", "/api/v1/models", "/api/v0/models",
        "/api/v0/models/mock-main", "/api/v0/models/mock-next"
    }:
        return False
    data = b'{"error":"optional runtime metadata is not provided by this fixture"}'
    handler.send_response(404)
    handler.send_header("Content-Type", "application/json")
    handler.send_header("Content-Length", str(len(data)))
    handler.end_headers()
    handler.wfile.write(data)
    return True


class DiscoveryGate:
    def __init__(self):
        self.armed = threading.Event()
        self.requested = threading.Event()
        self.release = threading.Event()

    def open(self):
        self.release.set()
        self.armed.set()


class Provider(http.server.ThreadingHTTPServer):
    daemon_threads = True

    def __init__(self, listening=True):
        super().__init__(("127.0.0.1", 0), Handler, bind_and_activate=False)
        # A bound, non-listening socket times out on Darwin. Close the allocated port
        # for genuine ECONNREFUSED; repair rebinds the same address or fails on collision.
        self.server_bind()
        if listening:
            self.server_activate()
        else:
            self.socket.close()
        self.discovery = DiscoveryGate()
        self.chat_requests = 0
        self.lock = threading.Lock()
        self.stream_requests = 0
        self.stream_gates = [threading.Event() for _ in range(3)]
        self.stream_complete = threading.Event()
        self.stream_errors = []
        self.crash_waiting = threading.Event()
        self.crash_release = threading.Event()
        self.image_checks = 0
        self.crash_checks = 0
        self.fixture_errors = []
        self.worker_started = threading.Event()
        self.worker_release = threading.Event()
        self.worker_restored = threading.Event()
        self.worker_beta_requests = 0
        self.worker_parent_requests = 0
        self.draft_requested = threading.Event()
        self.draft_release = threading.Event()
        self.side_main_release = threading.Event()
        self.side_advance = threading.Event()
        self.side_release = threading.Event()
        self.side_disconnected = threading.Event()
        self.side_requests = 0
        self.pool_runs = []
        self.diagnostic_requests = 0
        self.auth_repaired = threading.Event()
        self.auth_requests = 0
        self.busy_release = threading.Event()
        self.busy_requests = []
        self.redirect_release = threading.Event()
        self.redirect_disconnected = threading.Event()
        self.redirect_requests = []
        self.job_starts = 0

    def repair_listener(self):
        assert self.socket.fileno() == -1, "repair requires a closed fixture endpoint"
        self.socket = socket.socket(self.address_family, self.socket_type)
        self.server_bind()
        self.server_activate()


class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def respond(self, body, status=200):
        data = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        try:
            self.wfile.write(data)
        except (BrokenPipeError, ConnectionResetError):
            pass  # SIGKILL and explicit cancellation deliberately close fixture connections.

    def do_GET(self):
        if self.path.rstrip("/") == "/v1/models":
            # Snapshot this scenario's gate: startup can request discovery before
            # the terminal is ready, and later scenarios must not reuse its deadline.
            gate = self.server.discovery
            gate.requested.set()
            gate.armed.wait()
            if not gate.release.wait(15):
                self.respond({"error": "fixture discovery deadline"}, 503)
                return
            self.respond({"data": [{"id": "mock-main"}, {"id": "mock-next"}]})
        else:
            self.respond({"error": "fixture endpoint not found"}, 404)

    def do_POST(self):
        request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        if unsupported_probe(self):
            return
        with self.server.lock:
            self.server.chat_requests += 1
            index = self.server.chat_requests
        messages = request["messages"]
        # The request-tail latest-user note is runtime context, not the scenario prompt.
        last_user = next((m.get("content") for m in reversed(messages) if m["role"] == "user" and "Latest admitted user request" not in str(m.get("content", ""))), "")
        if any(tool.get("function", {}).get("name") == "dial_diagnostic_echo"
               for tool in request.get("tools", [])):
            message = self.coding_diagnostic(index, messages)
            self.respond({"id": f"pty-{index}", "choices": [{"index": 0, "message": message,
                          "finish_reason": "tool_calls" if message.get("tool_calls") else "stop"}]})
            return
        if last_user == "PTY-EXPIRED-CREDENTIAL":
            self.server.auth_requests += 1
            if not self.server.auth_repaired.is_set():
                self.respond({"error": {"message": "PTY credential expired; repair before retry",
                                       "type": "authentication_error", "code": "invalid_api_key"}}, 401)
            else:
                self.respond({"id": f"pty-{index}", "choices": [{"index": 0,
                              "message": {"role": "assistant", "content": "AUTH-RECOVERY-EXPLICIT-RETRY"},
                              "finish_reason": "stop"}]})
            return
        if last_user in ("PTY-REDIRECT-MAIN", "PTY-REDIRECT-NOW"):
            self.server.redirect_requests.append(last_user)
            if last_user == "PTY-REDIRECT-MAIN":
                self.gated_stream(index, "REDIRECT-MAIN-PREFIX",
                                  [(self.server.redirect_release, "REDIRECT-STALE-END")],
                                  self.server.redirect_disconnected)
            else:
                self.respond({"id": f"pty-{index}", "choices": [{"index": 0,
                              "message": {"role": "assistant", "content": "REDIRECT-CURRENT-INSTRUCTION-ANSWER"},
                              "finish_reason": "stop"}]})
            return
        if last_user == "PTY-OS-JOB":
            results = [m for m in messages if m["role"] == "tool"]
            calls = [call["function"]["name"] for m in messages for call in m.get("tool_calls", [])]
            offered = any(tool.get("function", {}).get("name") == "bg" for tool in request.get("tools", []))
            if "bg" not in calls and not offered:
                # Like a real provider, load job control when the budgeted core offer omits it.
                message = self.call(index, "tool_load", {"names": ["bg"]})
            elif "bg" not in calls:
                self.server.job_starts += 1
                message = self.call(index, "bg", {"action": "start", "name": "pty-os-job",
                                    "command": "printf 'OS-JOB-OUTPUT-KEPT\\n'; exec sleep 120",
                                    "ready": {"log": "OS-JOB-OUTPUT-KEPT", "timeout": 5}})
            else:
                self.server.job_receipt = str(results[-1]["content"])
                assert "pty-os-job" in self.server.job_receipt, "job start receipt missing"
                message = {"role": "assistant", "content": "OS-JOB-STARTED"}
            self.respond({"id": f"pty-{index}", "choices": [{"index": 0, "message": message,
                          "finish_reason": "tool_calls" if message.get("tool_calls") else "stop"}]})
            return
        if last_user in ("PTY-BUSY-MAIN", "PTY-BUSY-FOLLOW-UP", "PTY-BUSY-REMOVED"):
            self.server.busy_requests.append(last_user)
            if last_user == "PTY-BUSY-MAIN":
                self.gated_stream(index, "BUSY-MAIN-PREFIX", [(self.server.busy_release, "BUSY-MAIN-END")])
            else:
                self.respond({"id": f"pty-{index}", "choices": [{"index": 0,
                              "message": {"role": "assistant", "content": "BUSY-FOLLOW-UP-ADMITTED"},
                              "finish_reason": "stop"}]})
            return
        if isinstance(last_user, str) and "PTY-SIDE-QUESTION" in last_user:
            assert any(m.get("content") == "PTY-SIDE-MAIN" for m in messages), "side context lost the active main prompt"
            self.server.side_requests += 1
            self.gated_stream(index, "SIDE-PREFIX", [
                (self.server.side_advance, "SIDE-SURVIVED-MAIN-CANCEL"),
                (self.server.side_release, "SIDE-END"),
            ], self.server.side_disconnected)
            return
        if isinstance(last_user, str) and "PTY-SIDE-COMPLETE" in last_user:
            self.server.side_requests += 1
            self.respond({"id": f"pty-{index}", "choices": [{"index": 0, "message": {"role": "assistant", "content": "SIDE-COMPLETED-ANSWER"}, "finish_reason": "stop"}]})
            return
        if last_user == "PTY-SIDE-MAIN":
            self.gated_stream(index, "SIDE-MAIN-PREFIX", [(self.server.side_main_release, "SIDE-MAIN-END")])
            return
        if "PTY-WORKER" in json.dumps(messages) or "PTY-POOL" in json.dumps(messages):
            message = (self.pool if "PTY-POOL" in json.dumps(messages) else self.workers)(index, messages, request.get("tools", []))
            self.respond({"id": "pty-" + str(index), "object": "chat.completion", "choices": [{"index": 0, "message": message, "finish_reason": "tool_calls" if message.get("tool_calls") else "stop"}], "usage": {"prompt_tokens": 128, "completion_tokens": 32}})
            return
        # Task packets may quote older prompts. Only actual standalone user messages
        # select this fixture scenario; their latest instruction remains authoritative.
        user_index = next(i for i in range(len(messages) - 1, -1, -1)
                          if messages[i]["role"] == "user" and messages[i].get("content") in ("PTY-INPUT", "PTY-DRAFT-INPUT", "PTY-BLOCK", "PTY-REVIEW", "PTY-STREAM", "PTY-IMAGE", "PTY-IMAGE-RECHECK", "PTY-CRASH", "PTY-AFTER-RESUME"))
        prompt = messages[user_index]["content"]
        names = [call["function"]["name"] for message in messages[user_index + 1:] for call in message.get("tool_calls", [])]
        if prompt == "PTY-STREAM":
            with self.server.lock:
                self.server.stream_requests += 1
            self.stream(index)
            return
        if prompt in ("PTY-IMAGE", "PTY-IMAGE-RECHECK"):
            if prompt == "PTY-IMAGE" and "read" not in names:
                message = self.call(index, "read", {"path": "pixel.png"})
            else:
                if "data:image/png;base64," + PIXEL not in json.dumps(messages):
                    self.server.fixture_errors.append("historical pixels missing from actual provider request")
                self.server.image_checks += 1
                message = {"role": "assistant", "content": "Image-history-verified." if prompt == "PTY-IMAGE" else "Deleted-image-history-verified."}
        elif prompt == "PTY-CRASH":
            if "write" not in names:
                message = self.call(index, "write", {"path": "crash-marker.txt", "content": "effect-before-crash"})
            else:
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.end_headers()
                self.stream_event(index, {"role": "assistant", "content": "Crash-stream-prefix-kept."})
                self.wfile.flush()
                self.server.crash_waiting.set()
                if not self.server.crash_release.wait(20):
                    self.server.fixture_errors.append("crash fixture release deadline")
                return  # The real client has been killed; never complete this response.
        elif prompt == "PTY-AFTER-RESUME":
            writes = [call for message in messages for call in message.get("tool_calls", [])
                      if call["function"]["name"] == "write" and "crash-marker.txt" in call["function"]["arguments"]]
            if len(writes) != 1:
                self.server.fixture_errors.append(f"crash resume must retain one original write, saw {len(writes)}")
            self.server.crash_checks += 1
            message = {"role": "assistant", "content": "Crash-history-resumed-without-replay."}
        elif prompt in ("PTY-INPUT", "PTY-DRAFT-INPUT"):
            if "finish" in names:
                message = {"role": "assistant", "content": "Draft-focus-recovered." if prompt == "PTY-DRAFT-INPUT" else "Cancelled-input-recovered."}
            else:
                if prompt == "PTY-DRAFT-INPUT":
                    self.server.draft_requested.set()
                    if not self.server.draft_release.wait(15):
                        self.server.fixture_errors.append("draft fixture release deadline")
                message = self.call(index, "finish", {"outcome": "needs_input", "summary": "Choose destination", "reason": "Which destination should I use?", "options": ["Left", "Right"]})
        elif prompt == "PTY-REVIEW":
            if names.count("read") == 0:
                message = self.call(index, "read", {"path": "a.txt"})
                message["tool_calls"].extend(self.call(index + 100000, "read", {"path": "b.txt"})["tool_calls"])
            elif names.count("write") == 0:
                message = self.call(index, "write", {"path": "a.txt", "content": "edited-a"})
            elif names.count("write") == 1:
                message = self.call(index, "write", {"path": "b.txt", "content": "edited-b"})
            elif "finish" not in names:
                message = self.call(index, "finish", {"outcome": "completed", "summary": "Review-fixture-ready."})
            else:
                message = {"role": "assistant", "content": "Review-fixture-ready."}
        elif "todo" not in names and not any(tool.get("function", {}).get("name") == "todo" for tool in request.get("tools", [])):
            # Like a real provider, load a tool the route's budgeted core offer omits.
            message = self.call(index, "tool_load", {"names": ["todo"]})
        elif names.count("todo") == 0:
            message = self.call(index, "todo", {"op": "set", "tasks": [{"content": "Inspect the isolated fixture"}]})
        elif names.count("todo") == 1:
            reason = "The fixture deliberately lacks an operator choice. " + "Keep the code and request the missing prerequisite. " * 8 + "BLOCKER-END-MARKER"
            message = self.call(index, "todo", {"op": "block", "reason": reason})
        else:
            message = self.call(index, "finish", {"outcome": "blocked", "summary": "Fixture work is preserved.", "reason": "The fixture needs an operator choice."})
        finish = "tool_calls" if message.get("tool_calls") else "stop"
        self.respond({"id": "pty-" + str(index), "object": "chat.completion", "choices": [{"index": 0, "message": message, "finish_reason": finish}], "usage": {"prompt_tokens": 128, "completion_tokens": 32}})

    def stream_event(self, index, delta, finish=None):
        body = {"id": "pty-" + str(index), "object": "chat.completion.chunk", "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]}
        if finish:
            body["usage"] = {"prompt_tokens": 128, "completion_tokens": 32}
        self.wfile.write(("data: " + json.dumps(body) + "\n\n").encode())
        self.wfile.flush()

    def stream(self, index):
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.end_headers()
        self.close_connection = True
        try:
            self.stream_event(index, {"role": "assistant", "content": "SSE-PREFIX-MARKER\n\nSSE-WAITING"})
            for gate, marker in zip(self.server.stream_gates, ("SSE-AFTER-QUEUE", "SSE-AFTER-RESTART", "SSE-END-MARKER")):
                deadline = time.monotonic() + 15
                while not gate.wait(0.1):
                    if time.monotonic() >= deadline:
                        raise TimeoutError("fixture streaming command deadline")
                    # Real deltas keep the terminal input pump running while the
                    # test controls when the next observable answer fragment arrives.
                    self.stream_event(index, {"content": " "})
                self.stream_event(index, {"content": "\n\n" + marker})
            self.stream_event(index, {}, "stop")
            self.wfile.write(b"data: [DONE]\n\n")
            self.wfile.flush()
            self.server.stream_complete.set()
        except (BrokenPipeError, ConnectionResetError, TimeoutError) as error:
            with self.server.lock:
                self.server.stream_errors.append(str(error))

    def gated_stream(self, index, prefix, stages, disconnected=None):
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.end_headers()
        self.close_connection = True
        try:
            self.stream_event(index, {"role": "assistant", "content": prefix})
            for gate, marker in stages:
                deadline = time.monotonic() + 15
                while not gate.wait(0.1):
                    if time.monotonic() >= deadline:
                        raise TimeoutError("gated fixture deadline")
                    self.stream_event(index, {"content": " "})
                self.stream_event(index, {"content": "\n" + marker})
            self.stream_event(index, {}, "stop")
            self.wfile.write(b"data: [DONE]\n\n")
            self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            if disconnected:
                disconnected.set()

    def coding_diagnostic(self, index, messages):
        """Synthetic tool protocol only: never dispatch a real registry tool."""
        self.server.diagnostic_requests += 1
        results = [message for message in messages if message["role"] == "tool"]
        if not results:
            return self.call(index, "dial_diagnostic_echo", {"value": "dial-probe"})
        assert len(results) == 1, "coding diagnostic must have one synthetic result"
        receipt = json.loads(results[0]["content"])["receipt"]
        assert isinstance(receipt, str) and receipt, "coding result must supply a receipt"
        return {"role": "assistant", "content": receipt}

    @staticmethod
    def call(index, name, arguments):
        return {"role": "assistant", "content": "", "tool_calls": [{"id": "pty-call-" + str(index), "type": "function", "function": {"name": name, "arguments": json.dumps(arguments)}}]}

    def workers(self, index, messages, tools):
        if any(m["role"] == "user" and m.get("content") == "PTY-WORKERS" for m in messages):
            with self.server.lock:
                self.server.worker_parent_requests += 1
            # Respect lazy exposure just like a real provider: tool_load enables task for
            # the next request (discovery only describes it), rather than mistaking an unexecuted call for started workers.
            if not any(tool.get("function", {}).get("name") == "task" for tool in tools):
                return self.call(index, "tool_load", {"names": ["task"]})
            names = [call["function"]["name"] for message in messages for call in message.get("tool_calls", [])]
            if "task" not in names:
                return self.call(index, "task", {"action": "start", "tasks": [
                    {"agent": "scout", "task": "PTY-WORKER-ALPHA summarize the fixture", "solution_space": "read-only"},
                    {"agent": "scout", "task": "PTY-WORKER-BETA hold until cancelled", "solution_space": "read-only"},
                ]})
            return {"role": "assistant", "content": "Workers-started."}
        if "PTY-WORKER-BETA" in json.dumps(messages):
            with self.server.lock:
                self.server.worker_beta_requests += 1
                if self.server.worker_beta_requests == 2:
                    self.server.worker_restored.set()
            self.server.worker_started.set()
            if not self.server.worker_release.wait(60):
                self.server.fixture_errors.append("beta worker release deadline")
            return {"role": "assistant", "content": "Beta-held-open."}
        return {"role": "assistant", "content": "Alpha-summary-ready."}
    def pool(self, index, messages, tools):
        if any(m["role"] == "user" and m.get("content") == "PTY-POOL" for m in messages):
            if not any(tool.get("function", {}).get("name") == "task" for tool in tools):
                return self.call(index, "tool_load", {"names": ["task"]})
            calls = [json.loads(call["function"]["arguments"]) for message in messages
                     for call in message.get("tool_calls", []) if call["function"]["name"] == "task"]
            actions = [call.get("action") for call in calls]
            if not actions:
                return self.call(index, "task", {"action": "create", "concurrency": 1, "tasks": [
                    {"agent": "scout", "task": "PTY-POOL-FIRST complete the first item", "solution_space": "read-only"},
                ]})
            # The model sees the readable pool summary ("Worker pool pool-1: running; ..."), not the
            # typed report, which observers keep in the tool output's data.
            pool_ids = [match.group(1) for message in messages
                        if message["role"] == "tool" and isinstance(message.get("content"), str)
                        for match in [re.match(r"Worker pool (pool-\d+):", message["content"])] if match]
            assert pool_ids, "pool creation did not expose its usable id"
            pool_id = pool_ids[-1]
            if "push" not in actions:
                return self.call(index, "task", {"action": "push", "id": pool_id, "tasks": [
                    {"agent": "scout", "task": "PTY-POOL-SECOND continue on the retained worker", "solution_space": "read-only"},
                ]})
            for action in ("wait", "peek", "close"):
                if action not in actions:
                    return self.call(index, "task", {"action": action, "id": pool_id})
            return {"role": "assistant", "content": "POOL-API-CYCLE-COMPLETE"}
        second = "PTY-POOL-SECOND" in json.dumps(messages)
        if second and not any(
            m["role"] == "assistant" and m.get("content") == "POOL-FIRST-ANSWER" for m in messages
        ):
            self.server.fixture_errors.append("pool continuation lost the first item's assistant answer")
        with self.server.lock:
            self.server.pool_runs.append("second" if second else "first")
        return {"role": "assistant", "content": "POOL-SECOND-ANSWER" if second else "POOL-FIRST-ANSWER"}



class Terminal:
    def __init__(self, binary, project, home, captures, arguments=()):
        self.socket = f"dial-pty-smoke-{os.getpid()}-{time.monotonic_ns()}"
        self.captures = captures
        self.binary, self.project = binary, project
        # A real Dial home is private state beneath (never equal to) HOME. Sharing one directory
        # puts HOME toolchain read roots inside the denied private tree, which the Linux sandbox refuses.
        user_home = pathlib.Path(home).parent / "user-home"
        user_home.mkdir(mode=0o700, exist_ok=True)
        self.environment = dict(os.environ, HOME=str(user_home), DIAL_HOME=str(home), TERM="xterm-256color")
        for key in ("OPENAI_API_KEY", "ANTHROPIC_API_KEY", "GOOGLE_API_KEY", "OPENROUTER_API_KEY"):
            self.environment.pop(key, None)
        # Configure retention before launching Dial so an early startup failure remains readable.
        # Each instance owns a distinct server, including a second fixture after kill-server.
        self.run("new-session", "-d", "-s", "smoke", "-x", "110", "-y", "32", "-c", str(project), "sleep 300")
        self.run("set-option", "-t", "smoke", "remain-on-exit", "on")
        self.run("respawn-pane", "-k", "-t", "smoke", "-c", str(project), "exec " + shlex.join([str(binary), "--dir", str(project), *arguments]))

    def run(self, *arguments):
        return subprocess.run(["tmux", "-L", self.socket, "-f", "/dev/null", *arguments], env=self.environment, text=True, capture_output=True, check=True, timeout=10).stdout

    def text(self, history=True):
        history_args = ("-S", "-400") if history else ()
        return self.run("capture-pane", "-p", *history_args, "-t", "smoke")

    def wait(self, fragment, timeout=12, history=True):
        deadline = time.monotonic() + timeout
        text = ""
        while time.monotonic() < deadline:
            text = self.text(history=history)
            if fragment in text:
                return text
            time.sleep(0.05)
        raise AssertionError(f"terminal never displayed {fragment!r}:\n{text}")

    def command(self, command, key="Enter"):
        self.run("send-keys", "-t", "smoke", "-l", "\x1b[200~" + command + "\x1b[201~")
        # Observe paste consumption before starting the existing guard-settle period.
        # Waiting from send-keys alone can batch paste and Enter on a busy UI loop.
        self.wait(command, history=False)
        time.sleep(0.5)
        self.run("send-keys", "-t", "smoke", key)

    def key(self, key):
        self.run("send-keys", "-t", "smoke", key)
        time.sleep(0.3)

    def wait_panel(self, opened=True, timeout=12):
        deadline = time.monotonic() + timeout
        expected = "1" if opened else "0"
        while time.monotonic() < deadline:
            if self.run("display-message", "-p", "-t", "smoke", "#{alternate_on}").strip() == expected:
                return
            time.sleep(0.05)
        raise AssertionError(f"terminal panel did not {'open' if opened else 'close'}:\n{self.text()}")

    def select_worker(self, job_id):
        self.key("/")
        self.run("send-keys", "-t", "smoke", *(["BSpace"] * 64))
        self.run("send-keys", "-t", "smoke", "-l", job_id)
        self.key("Enter")

    def draft(self, text):
        self.run("send-keys", "-t", "smoke", "-l", "\x1b[200~" + text + "\x1b[201~")


    def capture(self, name, history=True):
        text = self.text(history=history)
        if self.captures:
            self.captures.mkdir(parents=True, exist_ok=True)
            (self.captures / (name + ".txt")).write_text(text)
        return text

    def crash_and_resume(self, restore=False):
        self.run("set-option", "-t", "smoke", "remain-on-exit", "on")
        pid = int(self.run("display-message", "-p", "-t", "smoke", "#{pane_pid}").strip())
        os.kill(pid, signal.SIGKILL)
        deadline = time.monotonic() + 5
        while self.run("display-message", "-p", "-t", "smoke", "#{pane_dead}").strip() != "1":
            if time.monotonic() >= deadline:
                raise AssertionError("owned Dial process did not exit after SIGKILL")
            time.sleep(0.05)
        self.run("respawn-pane", "-t", "smoke", "-c", str(self.project),
                 "exec " + shlex.join([str(self.binary), "--dir", str(self.project), "--restore" if restore else "--continue"]))

    def close(self):
        # Killing the tmux server only signals Dial; wait for it to exit so callers can remove its
        # project and home directories without racing its final writes.
        try:
            pid = int(self.run("display-message", "-p", "-t", "smoke", "#{pane_pid}").strip())
        except (subprocess.SubprocessError, ValueError):
            pid = None
        subprocess.run(["tmux", "-L", self.socket, "kill-server"], env=self.environment, capture_output=True, timeout=10)
        if pid is None:
            return
        deadline = time.monotonic() + 10
        while True:
            try:
                os.kill(pid, 0)
            except ProcessLookupError:
                return
            if time.monotonic() >= deadline:
                raise AssertionError(f"Dial process {pid} did not exit after tmux kill-server")
            time.sleep(0.05)


def settled(read, quiet=2.0, timeout=30):
    """Return read() once it has stayed unchanged for `quiet` seconds."""
    deadline = time.monotonic() + timeout
    value, since = read(), time.monotonic()
    while time.monotonic() < deadline:
        time.sleep(0.1)
        current = read()
        if current != value:
            value, since = current, time.monotonic()
        elif time.monotonic() - since >= quiet:
            return value
    raise AssertionError(f"value never settled within {timeout}s (last {value})")

def model(home):
    files = list((home / "projects").glob("*/config.toml"))
    files.append(home / "config.toml")
    for path in files:
        if path.exists():
            value = tomllib.loads(path.read_text()).get("model")
            if value:
                return value
    return None


def wait_model(home, expected):
    deadline = time.monotonic() + 12
    while time.monotonic() < deadline:
        if model(home) == expected:
            return
        time.sleep(0.05)
    raise AssertionError(f"expected saved model {expected}, got {model(home)}")


def fixture(root, port, busy_send="choose"):
    project, home = root / "project", root / "home"
    project.mkdir()
    home.mkdir(mode=0o700)
    subprocess.run(["git", "-c", "core.fsmonitor=false", "-c", "init.templateDir=", "init", "--quiet", "--initial-branch=main", str(project)], check=True, capture_output=True, timeout=10)
    url = f"http://127.0.0.1:{port}/v1"
    config = f'''model="mock-main"
provider="openai"
context_window=32768
approval="auto"
worker_timeout_secs=30
model_favorites=[{{provider="openai",model="mock-main",base_url="{url}",allow_insecure=true}},{{provider="openai",model="mock-next",base_url="{url}",allow_insecure=true}}]
[endpoint]
base_url="{url}"
allow_insecure=true
[improve]
enabled=false
[codebase]
enabled=false
[ui]
theme="dark"
busy_send="{busy_send}"
[harness]
max_steps=8
max_tokens=2048
[task]
persist=true
[task.provider_concurrency]
# This threaded mock can serve the parent and both scouts concurrently. A local endpoint
# otherwise defaults to one slot: held BETA would prevent ALPHA from ever replying first.
openai=3
'''
    (home / "config.toml").write_text(config)
    (home / "config.toml").chmod(0o600)
    # This fixture exercises coding interaction, not external package-manager availability.
    (home / "runtime-update-check").write_text(str(int(time.time())))
    return project, home


def workers_case(server, binary, captures):
    # A scripted parent launches two background scouts; BETA stays held until it is cancelled.
    with tempfile.TemporaryDirectory(prefix="dial-pty-workers-") as directory:
        project, home = fixture(pathlib.Path(directory), server.server_port)
        terminal = Terminal(binary, project, home, captures)
        try:
            terminal.wait("mock-main")
            terminal.command("PTY-WORKERS")
            terminal.wait("Workers-started")
            terminal.wait("scout#1 finished")
            assert server.worker_started.wait(12), "beta worker never started"
            # The pinned block and status segment both read one running worker out of two.
            pinned = terminal.wait("1 active")
            assert "1/2 workers" in pinned, pinned
            terminal.capture("workers-pinned")
            # The parent turn keeps going while workers run: each finished result and each
            # live-worker notice prompts one more parent request. Take the baseline once that
            # count has settled, or a slow host attributes an in-flight request to the restore.
            parent_requests = settled(lambda: server.worker_parent_requests)
            terminal.crash_and_resume(restore=True)
            terminal.wait("mock-main")
            assert server.worker_restored.wait(12), "restore did not rebind the saved interrupted worker"
            assert server.worker_parent_requests == parent_requests, (
                f"restore resubmitted the main prompt ({parent_requests} -> {server.worker_parent_requests} parent requests)")
            terminal.wait("restored 1: task-2")
            terminal.capture("parent-first-worker-restore")
            terminal.command("/agents")
            terminal.wait_panel()
            roster = terminal.wait("PTY-WORKER-BETA")
            assert "PTY-WORKER-ALPHA" in roster, roster
            terminal.capture("workers-roster")
            terminal.select_worker("task-2")
            terminal.key("Enter")
            terminal.command("/model mock-next")
            terminal.wait("queued")
            assert model(home) == "mock-main", "a focused worker setting changed the main model"
            terminal.draft("xkeep-the-worker-draft")
            terminal.key("Escape")
            terminal.select_worker("task-2")
            terminal.key("Enter")
            terminal.wait("xkeep-the-worker-draft")
            terminal.capture("worker-focused-model-and-preserved-draft")
            terminal.key("Escape")
            terminal.select_worker("task-2")
            terminal.key("x")
            terminal.wait("Press x again")
            terminal.key("x")
            terminal.wait("· cancelled")
            terminal.capture("workers-cancelled")
            terminal.key("Tab")
            terminal.wait("Alpha-summary-ready")
            terminal.capture("workers-activity")
        finally:
            terminal.close()


def pool_case(server, binary, captures):
    with tempfile.TemporaryDirectory(prefix="dial-pty-pool-") as directory:
        project, home = fixture(pathlib.Path(directory), server.server_port)
        terminal = Terminal(binary, project, home, captures)
        try:
            terminal.wait("mock-main")
            terminal.command("PTY-POOL")
            terminal.wait("POOL-API-CYCLE-COMPLETE", timeout=25)
            assert server.pool_runs == ["first", "second"], server.pool_runs
            assert not server.fixture_errors, server.fixture_errors
            terminal.capture("retained-pool-create-push-wait-peek-close")
        finally:
            terminal.close()


def draft_focus_case(server, binary, captures):
    with tempfile.TemporaryDirectory(prefix="dial-pty-draft-") as directory:
        project, home = fixture(pathlib.Path(directory), server.server_port)
        terminal = Terminal(binary, project, home, captures)
        try:
            terminal.wait("mock-main")
            terminal.command("PTY-DRAFT-INPUT")
            assert server.draft_requested.wait(10), "main provider did not reach the guarded prompt"
            terminal.draft("PROTECTED-DRAFT")
            terminal.wait("PROTECTED-DRAFT")
            server.draft_release.set()
            terminal.wait("Choose destination")
            terminal.draft(" y")
            terminal.key("Enter")
            guarded = terminal.capture("question-draft-editing")
            assert "PROTECTED-DRAFT y" in guarded, guarded
            assert "Draft-focus-recovered" not in guarded, "draft Enter answered the question"
            terminal.key("Tab")
            terminal.key("Escape")
            restored = terminal.wait("Draft-focus-recovered")
            assert "PROTECTED-DRAFT y" in restored, "question cancellation lost the edited draft"
            terminal.capture("question-draft-preserved")
        finally:
            server.draft_release.set()
            terminal.close()


def side_thread_case(server, binary, captures):
    with tempfile.TemporaryDirectory(prefix="dial-pty-side-") as directory:
        project, home = fixture(pathlib.Path(directory), server.server_port)
        terminal = Terminal(binary, project, home, captures)
        try:
            terminal.wait("mock-main")
            terminal.command("PTY-SIDE-MAIN")
            terminal.wait("SIDE-MAIN-PREFIX")
            terminal.command("/btw PTY-SIDE-QUESTION")
            terminal.wait("SIDE-PREFIX")
            files = list(home.rglob("*.side.json"))
            assert len(files) == 1, "side question was displayed before being saved"
            pending = json.loads(files[0].read_text())["pending"]
            assert "SIDE-PREFIX" in pending["answer"], "displayed side prefix was not durable"
            # Escape dismisses composer UI; Ctrl+C is the configured running-turn interrupt.
            terminal.key("C-c")
            terminal.wait("interrupted")
            server.side_advance.set()
            terminal.wait("SIDE-SURVIVED-MAIN-CANCEL")
            terminal.capture("side-survives-main-cancel")
            terminal.command("/btw cancel")
            assert server.side_disconnected.wait(5), "side cancellation did not stop its request"
            # Socket teardown precedes the UI's completion poll and durable exchange write.
            # Wait for Dial's completion acknowledgement, not just the provider disconnect.
            terminal.wait("CANCELLED; PARTIAL ANSWER")
            files = list(home.rglob("*.side.json"))
            assert len(files) == 1, files
            side_path = files[0]
            saved = json.loads(side_path.read_text())
            cancelled = saved["threads"][0]["exchanges"][0]
            assert cancelled["outcome"] == "cancelled", cancelled
            assert "SIDE-SURVIVED-MAIN-CANCEL" in cancelled["answer"], cancelled
            terminal.command("/btw new PTY-SIDE-COMPLETE")
            terminal.wait("SIDE-COMPLETED-ANSWER")
            source = side_path.with_name(side_path.name.removesuffix(".side.json") + ".jsonl")
            before = source.read_bytes()
            terminal.command("/btw promote 2")
            terminal.wait("full conversation")
            assert source.read_bytes() == before, "promotion changed the main session journal"
            saved = json.loads(side_path.read_text())
            sibling = saved["threads"][1]["promoted_session"]
            promoted = [p for p in home.rglob("*.jsonl") if sibling in p.name]
            assert len(promoted) == 1, promoted
            assert "SIDE-COMPLETED-ANSWER" in promoted[0].read_text(), "promoted full session lost its answer"
            terminal.capture("side-promoted-sibling")
            terminal.command("/btw threads")
            terminal.wait(sibling)
        finally:
            server.side_main_release.set()
            server.side_release.set()
            terminal.close()


def measured_case(name, scenario, server, binary, captures):
    """Descriptive wall-clock observations, not comparative performance claims."""
    started = time.monotonic()
    before = server.chat_requests
    corrections = scenario(server, binary, captures) or {}
    return {"name": name, "elapsed_ms": round((time.monotonic() - started) * 1000, 3),
            "chat_requests": server.chat_requests - before, **corrections}


def busy_preference_case(server, binary, captures):
    server.busy_release.clear()
    server.busy_requests.clear()
    with tempfile.TemporaryDirectory(prefix="dial-pty-busy-") as directory:
        project, home = fixture(pathlib.Path(directory), server.server_port, busy_send="after_turn")
        terminal = Terminal(binary, project, home, captures)
        try:
            terminal.wait("mock-main")
            terminal.command("PTY-BUSY-MAIN")
            terminal.wait("BUSY-MAIN-PREFIX")
            terminal.command("PTY-BUSY-REMOVED")  # Plain Enter must honor the preference.
            terminal.wait("1 queued", history=False)
            terminal.command("/queue remove 1")
            terminal.wait("Removed only the selected queued message")
            terminal.command("PTY-BUSY-FOLLOW-UP")
            terminal.wait("1 queued", history=False)
            terminal.command("/queue")
            terminal.wait_panel()
            terminal.wait("PTY-BUSY-FOLLOW-UP", history=False)
            assert server.busy_requests == ["PTY-BUSY-MAIN"], server.busy_requests
            terminal.key("Escape")
            terminal.wait_panel(False)
            released = time.monotonic()
            server.busy_release.set()
            terminal.wait("BUSY-MAIN-END")
            terminal.wait("BUSY-FOLLOW-UP-ADMITTED")
            assert server.busy_requests == ["PTY-BUSY-MAIN", "PTY-BUSY-FOLLOW-UP"], server.busy_requests
            terminal.capture("busy-preference-queued-once")
            return {"release_to_follow_up_ms": round((time.monotonic() - released) * 1000, 3)}
        finally:
            server.busy_release.set()
            terminal.close()


def redirect_preference_case(server, binary, captures):
    server.redirect_release.clear()
    server.redirect_disconnected.clear()
    server.redirect_requests.clear()
    with tempfile.TemporaryDirectory(prefix="dial-pty-redirect-") as directory:
        project, home = fixture(pathlib.Path(directory), server.server_port, busy_send="redirect")
        terminal = Terminal(binary, project, home, captures)
        try:
            terminal.wait("mock-main")
            terminal.command("PTY-REDIRECT-MAIN")
            terminal.wait("REDIRECT-MAIN-PREFIX")
            started = time.monotonic()
            terminal.command("PTY-REDIRECT-NOW")  # Plain Enter, not Ctrl+Enter.
            terminal.wait("REDIRECT-CURRENT-INSTRUCTION-ANSWER")
            assert server.redirect_disconnected.wait(5), "redirect did not cancel stale inference"
            assert server.redirect_requests == ["PTY-REDIRECT-MAIN", "PTY-REDIRECT-NOW"], server.redirect_requests
            terminal.command("/help")  # Process a local UI round trip before checking no replay.
            terminal.wait_panel()
            terminal.key("Escape")
            terminal.wait_panel(False)
            text = terminal.capture("busy-preference-redirect-once")
            assert "REDIRECT-STALE-END" not in text, "stale answer committed after redirect"
            assert server.redirect_requests == ["PTY-REDIRECT-MAIN", "PTY-REDIRECT-NOW"], server.redirect_requests
            return {"redirect_to_answer_ms": round((time.monotonic() - started) * 1000, 3)}
        finally:
            server.redirect_release.set()
            terminal.close()


def refused_connection_case(_server, binary, captures):
    server = Provider(listening=False)
    thread = None
    terminal = None
    try:
        with tempfile.TemporaryDirectory(prefix="dial-pty-refused-") as directory:
            project, home = fixture(pathlib.Path(directory), server.server_port)
            try:
                terminal = Terminal(binary, project, home, captures)
                terminal.wait("mock-main")
                terminal.command("/setup choose verify-coding")
                terminal.wait("Check coding/tool compatibility", history=False)
                terminal.key("v")
                terminal.wait("Endpoint unavailable", timeout=30)
                assert server.chat_requests == 0, "refused socket unexpectedly served HTTP"
                terminal.capture("refused-socket-diagnostic-failure")
                server.discovery.open()
                server.repair_listener()
                thread = threading.Thread(target=server.serve_forever, daemon=True)
                thread.start()
                terminal.command("/help")
                terminal.wait_panel()
                terminal.wait("Actions", history=False)
                assert server.chat_requests == 0, "endpoint repair automatically replayed diagnostic"
                terminal.key("Escape")
                terminal.wait_panel(False)
                started = time.monotonic()
                terminal.command("/setup choose verify-coding")
                terminal.wait("Check coding/tool compatibility", history=False)
                terminal.key("v")
                terminal.wait("Structured tool roundtrip verified", timeout=25)
                assert server.chat_requests == server.diagnostic_requests == 2, "explicit retry duplicated work"
                assert_no_project_edits(project)
                terminal.capture("refused-socket-explicit-retry")
                return {"explicit_retry_to_receipt_ms": round((time.monotonic() - started) * 1000, 3),
                        "repaired_endpoint_chat_requests": server.chat_requests}
            finally:
                if terminal:
                    terminal.close()
    finally:
        server.discovery.open()
        if thread:
            server.shutdown()
            thread.join(timeout=5)
        server.server_close()


def assert_stop_receipt(text):
    # A signal request is not proof of exit. Both receipts are honest owner outcomes.
    assert ("stop requested for pty-os-job; termination not yet observed" in text
            or "terminal history and output retained" in text), f"missing honest stop receipt:\n{text}"


def os_job_case(server, binary, captures):
    before = server.job_starts
    with tempfile.TemporaryDirectory(prefix="dial-pty-os-job-") as directory:
        project, home = fixture(pathlib.Path(directory), server.server_port)
        terminal = Terminal(binary, project, home, captures)
        try:
            terminal.wait("mock-main")
            terminal.command("PTY-OS-JOB")
            terminal.wait("OS-JOB-STARTED")
            terminal.command("/processes")
            terminal.wait_panel()
            try:
                terminal.wait("Running", history=False)
            except AssertionError as error:
                raise AssertionError(f"{error}\nbg start receipt:\n{getattr(server, 'job_receipt', None)}") from None
            terminal.key("d")  # Dismiss is not a way to stop a running process.
            terminal.wait("Running", history=False)
            assert "dismissed pty-os-job" not in terminal.text(history=False)
            terminal.key("s")
            terminal.wait("Stopped", history=False)
            assert_stop_receipt(terminal.capture("os-job-stop-receipt", history=False))
            terminal.key("Enter")
            terminal.wait("OS-JOB-OUTPUT-KEPT", history=False)
            terminal.wait("showing its last output", history=False)
            terminal.capture("os-job-stopped-output-retained", history=False)
            terminal.key("Escape")
            terminal.key("d")
            terminal.wait("dismissed pty-os-job from job history", history=False)
            terminal.wait("No background processes yet", history=False)
            terminal.capture("os-job-dismiss-receipt", history=False)
            terminal.key("Escape")
            terminal.wait_panel(False)
            assert server.job_starts - before == 1, "stop/dismiss replayed job start"
        finally:
            terminal.close()


def assert_no_project_edits(project):
    # Startup owns a Cargo target lease even for a scripted model. That runtime lease is
    # not a model/tool edit; no other file, directory or symlink is allowed in this fixture.
    allowed_dirs = {".tmp", ".tmp/cargo-target", ".tmp/cargo-target/.leases"}
    for path in project.rglob("*"):
        relative = path.relative_to(project)
        if relative.parts[0] == ".git":
            continue
        assert not path.is_symlink(), f"unexpected project symlink: {relative}"
        if relative.as_posix() in allowed_dirs and path.is_dir():
            continue
        assert (relative.parent.as_posix() == ".tmp/cargo-target/.leases"
                and path.is_file()), f"unexpected project edit: {relative}"


def coding_diagnostic_case(server, binary, captures):
    with tempfile.TemporaryDirectory(prefix="dial-pty-coding-") as directory:
        project, home = fixture(pathlib.Path(directory), server.server_port)
        terminal = Terminal(binary, project, home, captures)
        try:
            terminal.wait("mock-main")
            before = server.chat_requests
            terminal.command("/setup choose verify-coding")
            terminal.wait("Check coding/tool compatibility", history=False)
            terminal.key("Enter")  # Default is Cancel, never implicit model spending.
            terminal.wait_panel(False)
            terminal.command("/setup choose verify-coding")
            terminal.wait("Check coding/tool compatibility", history=False)
            assert server.chat_requests == before, "diagnostic Cancel sent a request"
            diagnostic_before = server.diagnostic_requests
            started = time.monotonic()
            terminal.key("v")
            terminal.wait("Structured tool roundtrip verified", timeout=25)
            assert server.diagnostic_requests - diagnostic_before == 2, "diagnostic exceeded its two-request contract"
            assert server.chat_requests - before == 2, "unexpected diagnostic or chat dispatch"
            assert_no_project_edits(project)
            terminal.capture("coding-diagnostic-roundtrip")
            return {"confirmation_to_receipt_ms": round((time.monotonic() - started) * 1000, 3)}
        finally:
            terminal.close()


def credential_recovery_case(server, binary, captures):
    server.auth_repaired.clear()
    before = server.auth_requests
    with tempfile.TemporaryDirectory(prefix="dial-pty-auth-") as directory:
        project, home = fixture(pathlib.Path(directory), server.server_port)
        terminal = Terminal(binary, project, home, captures)
        try:
            terminal.wait("mock-main")
            terminal.command("PTY-EXPIRED-CREDENTIAL")
            terminal.wait("PTY credential expired; repair before retry")
            terminal.command("/help")
            terminal.wait_panel()
            terminal.wait("Actions", history=False)
            terminal.key("Escape")
            terminal.wait_panel(False)
            server.auth_repaired.set()
            # A second local UI round trip proves the loop progressed after repair;
            # changing external credentials must never replay the failed prompt.
            terminal.command("/help")
            terminal.wait_panel()
            terminal.wait("Actions", history=False)
            assert server.auth_requests - before == 1, "authentication repair automatically replayed work"
            assert_no_project_edits(project)
            terminal.key("Escape")
            terminal.wait_panel(False)
            started = time.monotonic()
            terminal.command("PTY-EXPIRED-CREDENTIAL")
            terminal.wait("AUTH-RECOVERY-EXPLICIT-RETRY")
            assert server.auth_requests - before == 2, "explicit retry was duplicated"
            terminal.capture("credential-recovery-explicit-retry")
            return {"explicit_retry_to_answer_ms": round((time.monotonic() - started) * 1000, 3)}
        finally:
            terminal.close()


def recovery_preview_case(server, binary, captures):
    with tempfile.TemporaryDirectory(prefix="dial-pty-recovery-") as directory:
        project, home = fixture(pathlib.Path(directory), server.server_port)
        terminal = Terminal(binary, project, home, captures)
        try:
            terminal.wait("mock-main")
            terminal.command("PTY-INPUT")
            terminal.wait("Choose destination")
            terminal.key("Escape")
            terminal.wait("Cancelled-input-recovered")
            terminal.draft("PTY-RECOVERY-HELD-DRAFT")
            terminal.wait("PTY-RECOVERY-HELD-DRAFT", history=False)
            def records():
                return {str(path): json.loads(path.read_text())
                        for path in home.rglob("input-recovery/*.json")}
            saved = settled(records)
            assert "PTY-RECOVERY-HELD-DRAFT" in json.dumps(saved), "draft never became durable"
            before = server.chat_requests
            terminal.crash_and_resume()
            terminal.wait("mock-main")
            terminal.command("/draft preview")
            terminal.wait("Review saved input", history=False)
            terminal.wait("PTY-RECOVERY-HELD-DRAFT", history=False)
            baseline = settled(records)
            terminal.key("Escape")
            terminal.wait("Current input and saved records kept unchanged")
            assert settled(records) == baseline, "preview Cancel mutated saved input"
            assert server.chat_requests == before, "preview Cancel dispatched work"
            terminal.command("/draft restore")
            terminal.wait("Review saved input", history=False)
            restored = time.monotonic()
            terminal.key("r")
            terminal.wait("Input restored and held")
            terminal.wait("PTY-RECOVERY-HELD-DRAFT", history=False)
            assert server.chat_requests == before, "restore automatically sent the recovered draft"
            assert_no_project_edits(project)
            terminal.capture("recovery-preview-cancel-restore-held")
            return {"restore_to_held_preview_ms": round((time.monotonic() - restored) * 1000, 3)}
        finally:
            terminal.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--dial", type=pathlib.Path, required=True)
    parser.add_argument("--captures", type=pathlib.Path)
    args = parser.parse_args()
    binary = args.dial.resolve(strict=True)
    server = Provider()
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    journeys = []
    terminal = None
    try:
        baseline_started, baseline_requests = time.monotonic(), server.chat_requests
        with tempfile.TemporaryDirectory(prefix="dial-pty-regression-") as directory, ExitStack() as cleanup:
            root = pathlib.Path(directory)
            project, home = fixture(root, server.server_port)
            terminal = Terminal(binary, project, home, args.captures)
            cleanup.callback(lambda: terminal.close() if terminal else None)
            terminal.wait("mock-main")
            server.discovery.armed.set()
            terminal.capture("baseline")
            terminal.command("/model next")
            terminal.wait("Discovering available models")
            assert model(home) == "mock-main"
            assert server.discovery.requested.wait(5), "discovery request never reached fixture"
            server.discovery.open()
            try:
                wait_model(home, "mock-next")
            except AssertionError:
                print(terminal.capture("favorite-failure"))
                raise
            terminal.wait("Switched to")
            terminal.capture("favorite-applied-after-discovery")
            # Alt+M selects a model on every platform; Ctrl+K does only on macOS (elsewhere it
            # cuts to line end), so it would leave the Linux picker closed.
            terminal.key("M-m")
            terminal.wait("scope: global")
            terminal.capture("picker")
            terminal.run("resize-window", "-t", "smoke", "-x", "72", "-y", "22")
            time.sleep(0.3)
            terminal.capture("picker-resized")
            terminal.run("resize-window", "-t", "smoke", "-x", "110", "-y", "32")
            terminal.key("Escape")
            terminal.command("PTY-INPUT")
            terminal.wait("Choose destination")
            terminal.key("Escape")
            terminal.wait("Cancelled-input-recovered")
            terminal.capture("input-cancellation")
            (project / "a.txt").write_text("original-a")
            (project / "b.txt").write_text("original-b")
            terminal.command("PTY-REVIEW")
            terminal.wait("Review-fixture-ready")
            review_text = terminal.capture("review-completed")
            assert (project / "a.txt").read_text() == "edited-a", review_text
            assert (project / "b.txt").read_text() == "edited-b", review_text
            manifests = [json.loads(path.read_text()) for path in home.rglob("manifest.json")]
            turn = next(item["turn"] for item in manifests if any(entry["rel"] == "a.txt" for entry in item.get("entries", [])))
            terminal.command(f"/review-file {turn} a.txt")
            terminal.wait("Review file")
            terminal.capture("selective-review-dialog")
            terminal.key("r")
            terminal.wait("Revert file?")
            terminal.key("r")
            terminal.wait("Restored only the selected file")
            terminal.wait("checkpoint for /undo files.")
            assert (project / "a.txt").read_text() == "original-a"
            assert (project / "b.txt").read_text() == "edited-b"
            terminal.capture("selective-revert")
            terminal.wait("Changed files", history=False)
            terminal.key("Escape")
            terminal.wait_panel(False)
            terminal.command("/undo")
            terminal.wait("unrelated newer file checkpoint blocks undo; conversation kept")
            assert (project / "a.txt").read_text() == "original-a"
            assert (project / "b.txt").read_text() == "edited-b"
            terminal.capture("conversation-undo-preserves-selective-revert")
            terminal.command("/undo files")
            terminal.wait('restored ["a.txt"], removed []')
            assert (project / "a.txt").read_text() == "edited-a"
            assert (project / "b.txt").read_text() == "edited-b"
            terminal.capture("selective-revert-undone")
            terminal.command("PTY-STREAM")
            terminal.wait("SSE-PREFIX-MARKER")
            terminal.command("PTY-UNSENT-FOLLOW-UP", key="M-Enter")
            terminal.wait("PTY-UNSENT-FOLLOW-UP")
            terminal.command("/queue remove 1")
            terminal.wait("Removed only the selected queued message")
            terminal.command("/queue")
            terminal.wait("[queue]")
            terminal.wait("No choices to pick from.")
            terminal.capture("streaming-queue-empty")
            terminal.key("Escape")
            terminal.wait_panel(False)
            server.stream_gates[0].set()
            terminal.wait("SSE-AFTER-QUEUE")
            terminal.command("/model restart")
            terminal.wait("needs an idle session; finish or cancel the current turn")
            server.stream_gates[1].set()
            terminal.wait("SSE-AFTER-RESTART")
            terminal.command("/help")
            terminal.wait_panel()
            terminal.wait("Actions", history=False)
            terminal.wait("Start or continue work", history=False)
            terminal.capture("streaming-help-live")
            terminal.key("Escape")
            terminal.wait_panel(False)
            server.stream_gates[2].set()
            terminal.wait("SSE-END-MARKER")
            assert server.stream_complete.wait(5), f"fixture stream did not finish: {server.stream_errors}"
            streamed = terminal.capture("streaming-command-survival")
            assert "EXTERNAL RUNTIME/TOOL UPDATES" not in streamed, "automatic maintenance leaked into the conversation"
            for marker in ("SSE-PREFIX-MARKER", "SSE-AFTER-QUEUE", "SSE-AFTER-RESTART", "SSE-END-MARKER"):
                assert streamed.count(marker) == 1, f"{marker} must commit exactly once:\n{streamed}"
            assert server.stream_requests == 1, "busy commands must not cancel or regenerate the reply"
            assert not server.stream_errors, f"fixture stream was interrupted: {server.stream_errors}"
            (project / "pixel.png").write_bytes(base64.b64decode(PIXEL))
            terminal.command("PTY-IMAGE")
            terminal.wait("Image-history-verified")
            (project / "pixel.png").write_bytes(b"overwritten source")
            (project / "pixel.png").unlink()
            terminal.command("PTY-IMAGE-RECHECK")
            terminal.wait("Deleted-image-history-verified")
            assert server.image_checks == 2 and not server.fixture_errors, server.fixture_errors
            terminal.command("PTY-CRASH")
            assert server.crash_waiting.wait(12), "fixture never reached post-effect inference"
            terminal.wait("Crash-stream-prefix-kept")
            assert (project / "crash-marker.txt").read_text() == "effect-before-crash"
            terminal.crash_and_resume()
            server.crash_release.set()
            terminal.wait("mock-next")
            terminal.wait("Crash-stream-prefix-kept")
            terminal.command("PTY-AFTER-RESUME")
            terminal.wait("Crash-history-resumed-without-replay")
            assert (project / "crash-marker.txt").read_text() == "effect-before-crash"
            assert server.crash_checks == 1 and not server.fixture_errors, server.fixture_errors
            terminal.capture("crash-resume-image-history")
            terminal.command("PTY-BLOCK")
            terminal.wait("Fixture work is preserved")
            terminal.command("/todo")
            terminal.wait("/todo show 1")
            terminal.key("Enter")
            terminal.wait("Step details")
            terminal.key("PageDown")
            terminal.wait("BLOCKER-END-MARKER")
            terminal.capture("full-blocked-reason")
            terminal.key("Enter")
            terminal.wait("/todo show 1")
            terminal.key("Escape")
            terminal.close()
            terminal = None
        journeys.append({"name": "baseline-workflows", "elapsed_ms": round((time.monotonic() - baseline_started) * 1000, 3),
                         "chat_requests": server.chat_requests - baseline_requests})
        for name, scenario in (("workers", workers_case), ("pool", pool_case),
                               ("draft-focus", draft_focus_case), ("side-thread", side_thread_case)):
            journeys.append(measured_case(name, scenario, server, binary, args.captures))
        for name, scenario in (("busy-after-turn", busy_preference_case),
                               ("busy-redirect", redirect_preference_case),
                               ("refused-connection-recovery", refused_connection_case),
                               ("os-job-stop-dismiss", os_job_case),
                               ("coding-diagnostic", coding_diagnostic_case),
                               ("credential-recovery", credential_recovery_case),
                               ("recovery-preview", recovery_preview_case)):
            journeys.append(measured_case(name, scenario, server, binary, args.captures))
        cancel_started, cancel_requests = time.monotonic(), server.chat_requests
        with tempfile.TemporaryDirectory(prefix="dial-pty-cancel-") as directory, ExitStack() as cleanup:
            project, home = fixture(pathlib.Path(directory), server.server_port)
            server.discovery = DiscoveryGate()
            terminal = Terminal(binary, project, home, args.captures)
            cleanup.callback(lambda: terminal.close() if terminal else None)
            terminal.wait("mock-main")
            server.discovery.armed.set()
            terminal.command("/model next")
            terminal.wait("Discovering available models")
            terminal.key("Escape")
            terminal.wait("Pending switch cancelled")
            assert server.discovery.requested.wait(5), "discovery request never reached fixture"
            server.discovery.open()
            # A populated real picker acknowledges that Dial consumed discovery;
            # do not infer completion from an elapsed sleep or HTTP response alone.
            terminal.key("M-m")
            terminal.wait("scope: global")
            terminal.wait("mock-next", history=False)
            terminal.key("Escape")
            assert model(home) == "mock-main", "cancelled switch must not apply after discovery"
            terminal.capture("favorite-cancelled")
            terminal.close()
            terminal = None
        journeys.append({"name": "favorite-cancellation", "elapsed_ms": round((time.monotonic() - cancel_started) * 1000, 3),
                         "chat_requests": server.chat_requests - cancel_requests})
        result = {
            "kind": "real-pty-workflow-regression",
            "checks": [
                "first-press-favorite", "picker-and-resize", "wrapped-input-cancellation",
                "selective-review-revert-undo", "streaming-queue-management",
                "streaming-refused-restart", "streaming-help-without-prefix-replay",
                "full-blocker", "pending-switch-cancellation", "immutable-image-history",
                "sigkill-resume-without-effect-replay", "populated-workers-hub",
                "parent-first-worker-restore", "focused-worker-model-next-request",
                "focused-worker-draft-preservation", "pool-cycle-with-retained-history",
                "question-draft-focus-and-preservation", "side-request-during-main",
                "side-survives-main-cancel", "side-cancellation-persisted",
                "side-promotion-is-independent",
                "busy-enter-after-turn-queued-exactly-once", "coding-diagnostic-cancel-and-roundtrip",
                "expired-credential-explicit-retry-only",
                "recovery-preview-cancel-and-held-restore",
                "busy-enter-redirect-without-stale-replay", "refused-socket-explicit-diagnostic-retry",
                "os-job-stop-output-retained-and-dismiss-receipts",
            ],
            "chat_requests": server.chat_requests,
            "side_requests": server.side_requests,
            "journeys": journeys,
            "pixel_screenshot_verification": False,
            "model_quality_evidence": False,
        }
        if args.captures:
            args.captures.mkdir(parents=True, exist_ok=True)
            (args.captures / "result.json").write_text(json.dumps(result, indent=2) + "\n")
        print(json.dumps(result))
    finally:
        server.discovery.open()
        server.crash_release.set()
        server.worker_release.set()
        server.draft_release.set()
        server.side_main_release.set()
        server.side_advance.set()
        server.side_release.set()
        server.busy_release.set()
        server.redirect_release.set()
        for gate in server.stream_gates:
            gate.set()
        if terminal:
            terminal.close()
        server.shutdown()
        server.server_close()
        thread.join(timeout=5)


if __name__ == "__main__":
    main()
