#!/usr/bin/env python3
"""Real-PTY acceptance probe for the Dial surface redesign.

Scenarios run in isolated fixtures against a local OpenAI-compatible provider:
  1. Extension panel from the command palette: submit once, then Escape-cancel a second invocation.
  2. `/tree` navigation: native write turns, `/label` on the first answer, palette-opened tree with a
     parked draft, Ctrl-Up/Down/Left to the labeled parent, selection, then a branch prompt.
  3. `ui.smooth_reveal`: one large text delta held with SSE comment heartbeats; the view advances while
     quiet, the newest grapheme stays withheld until finish, and the final text is not duplicated.
  4. Incomplete write arguments: the preview shows the target and content markers before Escape cancels;
     no file is dispatched.

Captures are text evidence only. Assertions use files, provider receipts, plugin receipts, and state.
"""

import argparse
import http.server
import json
import pathlib
import re
import sys
import tempfile
import threading
import time

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
from interactive_smoke import Terminal, fixture, unsupported_probe  # noqa: E402

MARKERS = ("PTY-SURF-FIRST", "PTY-SURF-SECOND", "PTY-SURF-BRANCH", "PTY-REVEAL-HOLD", "PTY-WRITE-CANCEL", "PTY-AFTER-CANCEL")
REVEAL_WORDS = " ".join(f"w{i:04d}" for i in range(1, 401))
REVEAL_TEXT = REVEAL_WORDS + " ENDCHECK\u03a9"
PARTIAL_WRITE = '{"path":"cancel-target.txt","content":"PARTIAL-WRITE-MARKER '

# One process per JSON-RPC request. Appends each method to a trace; appends each command.panel
# outcome as a JSON line to a receipt file so both invocations are observable.
PLUGIN = '''import json, pathlib, sys
request = json.loads(sys.stdin.readline())
method, params = request["method"], request.get("params") or {}
trace, receipts = pathlib.Path(sys.argv[1]), pathlib.Path(sys.argv[2])
with trace.open("a") as log:
    log.write(method + "\\n")
if method == "initialize":
    result = {"protocolVersion": "1", "commands": ["pty-panel"]}
elif method == "command":
    result = {"panel": {"title": "PTY Panel Title", "generation": 1, "elements": [
        {"type": "text", "text": "PTY-PANEL-BODY"},
        {"type": "input", "id": "name", "label": "Name", "value": ""},
        {"type": "select", "id": "mode", "label": "Mode", "options": ["alpha", "beta"], "selected": 0},
        {"type": "button", "id": "save", "label": "Save", "primary": True},
    ]}}
elif method == "command.panel":
    with receipts.open("a") as log:
        log.write(json.dumps(params) + "\\n")
    cancelled = params["outcome"]["status"] == "cancelled"
    result = {"text": "PANEL-CANCELLED-OK" if cancelled else "PANEL-RECEIPT-OK"}
else:
    print(json.dumps({"jsonrpc": "2.0", "id": request["id"], "error": {"code": -32601, "message": "unexpected " + method}}), flush=True)
    sys.exit(0)
print(json.dumps({"jsonrpc": "2.0", "id": request["id"], "result": result}), flush=True)
'''


def find_marker(messages):
    for position in range(len(messages) - 1, -1, -1):
        message = messages[position]
        if message.get("role") != "user" or not isinstance(message.get("content"), str):
            continue
        content = message["content"].strip()
        if content in MARKERS:
            return content, position
        if content == "PARKED-DRAFT-MARKER PTY-SURF-BRANCH":
            return "PTY-SURF-BRANCH", position
    return None, None


class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def send_json(self, body, status=200):
        data = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def send_text(self, index, text):
        self.send_json(
            {"id": f"pty-{index}", "object": "chat.completion",
             "choices": [{"index": 0, "message": {"role": "assistant", "content": text}, "finish_reason": "stop"}],
             "usage": {"prompt_tokens": 64, "completion_tokens": 16}}
        )

    def send_tool(self, index, name, arguments):
        self.send_json(
            {"id": f"pty-{index}", "object": "chat.completion",
             "choices": [{"index": 0, "finish_reason": "tool_calls", "message": {
                 "role": "assistant", "content": None,
                 "tool_calls": [{"id": f"call-{index}", "type": "function",
                                 "function": {"name": name, "arguments": json.dumps(arguments)}}]}}],
             "usage": {"prompt_tokens": 64, "completion_tokens": 16}}
        )

    def begin_stream(self):
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.end_headers()

    def send_event(self, delta, finish=None):
        body = {"id": "pty-surface", "object": "chat.completion.chunk",
                "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]}
        self.wfile.write(b"data: " + json.dumps(body).encode() + b"\n\n")
        self.wfile.flush()

    def do_GET(self):
        if self.path.rstrip("/") == "/v1/models":
            self.send_json({"object": "list", "data": [{"id": "mock-main", "object": "model"}, {"id": "mock-next", "object": "model"}]})
            return
        if unsupported_probe(self):
            return
        self.server.fail(f"unexpected GET {self.path}")
        self.send_json({"error": "unexpected fixture request"}, 500)

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        messages = body.get("messages", [])
        marker, position = find_marker(messages)
        with self.server.lock:
            self.server.chat_requests += 1
            index = self.server.chat_requests
        if marker is None:
            self.server.fail(f"unrecognized chat request {index}")
            self.send_json({"error": "unrecognized fixture request"}, 500)
            return
        with self.server.lock:
            self.server.receipts.setdefault(marker, []).append(json.dumps(messages))
        tools_after = sum(1 for m in messages[position + 1:] if m.get("role") == "tool")
        if marker in ("PTY-SURF-FIRST", "PTY-SURF-SECOND"):
            label = "FIRST" if marker == "PTY-SURF-FIRST" else "SECOND"
            if tools_after == 0:
                self.send_tool(index, "write", {"path": "shared.txt", "content": f"{label}-WRITE-BYTES"})
            else:
                self.send_text(index, f"{label}-ANSWER-DONE")
            return
        if marker == "PTY-SURF-BRANCH":
            self.send_text(index, "BRANCH-ANSWER-DONE")
            return
        if marker == "PTY-AFTER-CANCEL":
            self.send_text(index, "AFTER-CANCEL-ANSWER")
            return
        if marker == "PTY-REVEAL-HOLD":
            self.begin_stream()
            self.send_event({"role": "assistant", "content": REVEAL_TEXT})  # the only text delta
            self.server.reveal_sent.set()
            deadline = time.time() + 30
            try:
                while not self.server.reveal_release.is_set() and time.time() < deadline:
                    self.wfile.write(b": heartbeat\n\n")
                    self.wfile.flush()
                    time.sleep(0.25)
                self.send_event({}, finish="stop")
                self.wfile.write(b"data: [DONE]\n\n")
            except (BrokenPipeError, ConnectionResetError):
                pass
            return
        if marker == "PTY-WRITE-CANCEL":
            self.begin_stream()
            self.send_event({"role": "assistant", "tool_calls": [{"index": 0, "id": "call-cancel", "type": "function",
                                                                 "function": {"name": "write", "arguments": PARTIAL_WRITE}}]})
            self.server.cancel_started.set()
            if not self.server.cancel_release.wait(20):
                self.server.fail("write-cancel hold deadline")
            return
        self.server.fail(f"unhandled marker {marker}")
        self.send_json({"error": "unhandled fixture marker"}, 500)


class Provider(http.server.ThreadingHTTPServer):
    daemon_threads = True

    def __init__(self):
        super().__init__(("127.0.0.1", 0), Handler)
        self.lock = threading.Lock()
        self.chat_requests = 0
        self.receipts = {}
        self.failures = []
        self.reveal_sent = threading.Event()
        self.reveal_release = threading.Event()
        self.cancel_started = threading.Event()
        self.cancel_release = threading.Event()

    def fail(self, message):
        with self.lock:
            self.failures.append(message)

    def receipt(self, marker):
        with self.lock:
            return self.receipts.get(marker, [""])[-1]

    def release_all(self):
        self.reveal_release.set()
        self.cancel_release.set()


def configure(home, smooth=False, plugin=None):
    path = home / "config.toml"
    text = path.read_text()
    if smooth:
        replaced = text.replace('[ui]\ntheme="dark"\n', '[ui]\ntheme="dark"\nsmooth_reveal=true\n', 1)
        assert replaced != text, "fixture [ui] table changed; smooth_reveal could not be enabled"
        text = replaced
    if plugin:
        script, trace, receipts = plugin
        text += (
            "[extensions.plugins.pty-panel]\n"
            f"program={json.dumps(sys.executable)}\n"
            f"args=[{json.dumps(str(script))}, {json.dumps(str(trace))}, {json.dumps(str(receipts))}]\n"
        )
    path.write_text(text)


def type_text(terminal, text):
    terminal.run("send-keys", "-t", "smoke", "-l", text)




def max_reveal_index(text):
    numbers = [int(n) for n in re.findall(r"w(\d{4})", text)]
    return max(numbers) if numbers else 0


def panel_case(server, binary, captures, root):
    project, home = fixture(root / "panel", server.server_port)
    private = root / "panel-private"
    private.mkdir(exist_ok=True)
    script, trace, receipts = private / "pty-panel.py", private / "panel-trace.log", private / "panel-receipts.jsonl"
    script.write_text(PLUGIN)
    configure(home, plugin=(script, trace, receipts))
    terminal = Terminal(binary, project, home, captures)
    try:
        terminal.wait("mock-main")
        terminal.draft("PTY-PANEL-DRAFT")
        terminal.key("C-p")
        type_text(terminal, "pty-panel")
        terminal.key("Enter")
        terminal.wait("PTY-PANEL-BODY", history=False)
        type_text(terminal, "Ada")
        terminal.key("Tab")
        terminal.key("Right")
        terminal.key("Tab")
        terminal.key("Enter")
        terminal.wait("PANEL-RECEIPT-OK")
        terminal.wait("PTY-PANEL-DRAFT")
        terminal.capture("panel-submitted")

        terminal.key("C-p")
        type_text(terminal, "pty-panel")
        terminal.key("Enter")
        terminal.wait("PTY-PANEL-BODY", history=False)
        terminal.key("Escape")
        terminal.wait("PANEL-CANCELLED-OK")
        terminal.wait("PTY-PANEL-DRAFT")
        terminal.capture("panel-cancelled")
    finally:
        terminal.close()
    lines = [json.loads(line) for line in receipts.read_text().splitlines()]
    assert len(lines) == 2, lines
    submitted, cancelled = lines[0]["outcome"], lines[1]["outcome"]
    assert all(reply["generation"] == 1 for reply in lines), lines
    assert submitted["status"] == "submitted" and submitted["button"] == "save", submitted
    assert submitted["values"]["name"] == "Ada" and submitted["values"]["mode"] == 1, submitted
    assert cancelled["status"] == "cancelled", cancelled
    methods = trace.read_text().split()
    assert methods.count("command.panel") == 2 and "command" in methods, methods
    return {"submitted": submitted, "cancelled": cancelled, "plugin_methods": methods, "main_draft_retained": True}


def tree_case(server, binary, captures, root):
    project, home = fixture(root / "tree", server.server_port)
    configure(home)
    terminal = Terminal(binary, project, home, captures)
    shared = project / "shared.txt"
    try:
        terminal.wait("mock-main")
        terminal.command("PTY-SURF-FIRST")
        terminal.wait("FIRST-ANSWER-DONE")
        assert shared.read_text() == "FIRST-WRITE-BYTES"
        terminal.command("/label first-answer")
        terminal.wait("first-answer")
        terminal.command("PTY-SURF-SECOND")
        terminal.wait("SECOND-ANSWER-DONE")
        assert shared.read_text() == "SECOND-WRITE-BYTES"
        chat_before_nav = server.chat_requests

        terminal.draft("PARKED-DRAFT-MARKER")
        terminal.key("C-p")
        type_text(terminal, "tree")
        terminal.key("Enter")
        terminal.wait("PTY-SURF-SECOND")
        terminal.key("C-Up")
        terminal.key("C-Down")
        terminal.key("C-Left")
        terminal.wait("FIRST-ANSWER-DONE")
        picker = terminal.capture("tree-labeled-parent-preview", history=False)
        # The tree is inline: earlier main output legitimately remains above it.
        # The final fixture user marker starts the selected ancestry's native preview.
        start = picker.rfind("PTY-SURF-FIRST")
        assert start >= 0, picker
        preview = picker[start:]
        assert "FIRST-WRITE-BYTES" in preview and "FIRST-ANSWER-DONE" in preview, preview
        assert "SECOND-ANSWER-DONE" not in preview and "SECOND-WRITE-BYTES" not in preview, preview
        terminal.key("Enter")
        terminal.wait("PARKED-DRAFT-MARKER")
        assert shared.read_text() == "SECOND-WRITE-BYTES", "navigation rewrote the file"
        assert server.chat_requests == chat_before_nav, "navigation triggered inference"

        type_text(terminal, " PTY-SURF-BRANCH")
        terminal.key("Enter")
        terminal.wait("BRANCH-ANSWER-DONE")
        terminal.capture("tree-branch-answered")
    finally:
        terminal.close()
    branch = server.receipt("PTY-SURF-BRANCH")
    assert "FIRST-ANSWER-DONE" in branch and "FIRST-WRITE-BYTES" in branch, "branch lost the first turn"
    assert "PTY-SURF-SECOND" not in branch and "SECOND-ANSWER-DONE" not in branch and "SECOND-WRITE-BYTES" not in branch, "branch leaked the second turn"
    return {"branch_retains_first_turn": True, "branch_excludes_second_turn": True, "navigation_inference": 0}


def reveal_case(server, binary, captures, root):
    project, home = fixture(root / "reveal", server.server_port)
    configure(home, smooth=True)
    terminal = Terminal(binary, project, home, captures)
    try:
        terminal.wait("mock-main")
        terminal.command("PTY-REVEAL-HOLD")
        assert server.reveal_sent.wait(15), "large delta was never streamed"
        terminal.wait("w0001", timeout=20)
        # Do not impose a minimum animation duration: a responsive renderer may have
        # finished revealing by the time a screen capture is read. The tail must become
        # visible with no more text deltas, while its newest grapheme remains withheld.
        second = terminal.wait("ENDCHECK", timeout=20)
        assert max_reveal_index(second) == 400, second
        assert "ENDCHECK\u03a9" not in second, "newest grapheme was shown before finish"
        terminal.capture("reveal-held")
        server.reveal_release.set()
        terminal.wait("ENDCHECK\u03a9", timeout=20)
        final = terminal.text()
        words = re.findall(r"\bw\d{4}\b", final)
        assert words == [f"w{i:04d}" for i in range(1, 401)], "revealed words were lost, duplicated or reordered"
        assert final.count("ENDCHECK") == 1, "final text duplicated"
        terminal.capture("reveal-committed")
    finally:
        server.reveal_release.set()
        terminal.close()
    return {"advanced_while_quiet": True, "newest_grapheme_withheld_until_finish": True, "final_text_duplicated": False}


def write_cancel_case(server, binary, captures, root):
    project, home = fixture(root / "cancel", server.server_port)
    configure(home)
    terminal = Terminal(binary, project, home, captures)
    try:
        terminal.wait("mock-main")
        terminal.command("PTY-WRITE-CANCEL")
        assert server.cancel_started.wait(15), "partial write call was never streamed"
        terminal.wait("cancel-target.txt", timeout=15)
        terminal.wait("PARTIAL-WRITE-MARKER", timeout=15)
        terminal.capture("write-preview-incomplete")
        terminal.key("Escape")
        time.sleep(1.5)
        assert not (project / "cancel-target.txt").exists(), "incomplete write arguments were dispatched"
        server.cancel_release.set()
        terminal.capture("write-cancelled")
        terminal.command("PTY-AFTER-CANCEL")
        terminal.wait("AFTER-CANCEL-ANSWER")
        assert not (project / "cancel-target.txt").exists()
        terminal.capture("after-cancel")
    finally:
        server.cancel_release.set()
        terminal.close()
    return {"preview_showed_target_and_content": True, "incomplete_write_dispatched": False}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--dial", type=pathlib.Path, required=True)
    parser.add_argument("--captures", type=pathlib.Path, required=True)
    args = parser.parse_args()
    binary = args.dial.resolve(strict=True)
    server = Provider()
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        with tempfile.TemporaryDirectory(prefix="dial-surfaces-") as directory:
            root = pathlib.Path(directory)
            for name in ("panel", "tree", "reveal", "cancel", "panel-private"):
                (root / name).mkdir(exist_ok=True)
            scenarios = {
                "extension_panel": panel_case(server, binary, args.captures, root),
                "tree_navigation": tree_case(server, binary, args.captures, root),
                "reveal_hold": reveal_case(server, binary, args.captures, root),
                "write_cancel": write_cancel_case(server, binary, args.captures, root),
            }
        assert not server.failures, f"fixture failures: {server.failures}"
        print(json.dumps({
            "kind": "interactive-surfaces-probe",
            "scenarios": scenarios,
            "chat_requests": server.chat_requests,
            "captures_dir": str(args.captures),
            "pixel_verification": False,
            "model_quality_evidence": False,
        }, indent=2))
    finally:
        server.release_all()
        server.shutdown()
        server.server_close()
        thread.join(timeout=5)


if __name__ == "__main__":
    main()
