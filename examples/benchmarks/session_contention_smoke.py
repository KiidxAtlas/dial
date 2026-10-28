#!/usr/bin/env python3
"""Real-CLI session writer-contention smoke.

An owner Dial holds a streamed assistant request after a unique prefix while it keeps its
session writer lease. A second `dial --continue` on the same private project/HOME must fork a
sibling journal instead of failing, hijacking the owner, or replaying the owner's in-flight
inference. Evidence is protocol/state: fixture request records and persisted journal files.
Terminal captures are text only.

Storage keeps live observations (the interrupted prefix) separate from canonical committed
chat, so the sibling is expected to surface the prefix as recovery runtime state in its UI while
its completed message history excludes it.

Usage:
    python3 examples/benchmarks/session_contention_smoke.py --dial PATH [--captures DIR]
"""

import argparse
import http.server
import json
import pathlib
import tempfile
import threading
import uuid

from interactive_smoke import Terminal, fixture, unsupported_probe

CHAT_PATH = "/v1/chat/completions"
MODELS_PATH = "/v1/models"
WAIT = 20


def last_user_text(messages):
    for message in reversed(messages):
        if message.get("role") != "user":
            continue
        content = message.get("content")
        if isinstance(content, list):
            return "".join(part.get("text", "") for part in content if isinstance(part, dict))
        return content if isinstance(content, str) else json.dumps(content)
    return None


class Server(http.server.ThreadingHTTPServer):
    daemon_threads = True

    def __init__(self, token):
        super().__init__(("127.0.0.1", 0), Handler)
        self.owner_prompt = f"OWNER-PROMPT-{token}"
        self.owner_reply = f"HELD-REPLY-{token}"
        self.owner_end = f"HELD-END-{token}"
        self.sibling_prompt = f"SIBLING-PROMPT-{token}"
        self.sibling_answer = f"SIBLING-ANSWER-{token}"
        self.lock = threading.Lock()
        self.fixture_errors = []
        self.owner_inferences = 0
        self.owner_started = threading.Event()
        self.owner_release = threading.Event()
        self.owner_finished = threading.Event()
        self.owner_disconnected = False
        self.sibling_messages = None
        self.sibling_seen = threading.Event()


class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def fail(self, message, status=500):
        with self.server.lock:
            self.server.fixture_errors.append(message)
        data = json.dumps({"error": {"message": message}}).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def respond_models(self):
        data = json.dumps(
            {"object": "list", "data": [{"id": "mock-main", "object": "model"}]}
        ).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        if self.path == MODELS_PATH:
            self.respond_models()
            return
        if unsupported_probe(self):
            return
        self.fail(f"unexpected GET {self.path}", 404)

    def open_stream(self):
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.end_headers()

    def write_chunk(self, delta, finish=None):
        body = {
            "id": "contention-smoke",
            "object": "chat.completion.chunk",
            "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
        }
        if finish:
            body["usage"] = {"prompt_tokens": 128, "completion_tokens": 32}
        self.wfile.write(("data: " + json.dumps(body) + "\n\n").encode())
        self.wfile.flush()

    def do_POST(self):
        if self.path != CHAT_PATH:
            self.fail(f"unexpected POST {self.path}", 404)
            return
        request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        if request.get("stream") is not True:
            self.fail("fixture requires streamed chat requests")
            return
        messages = request.get("messages", [])
        # Controller context may quote an older goal after the real user message.
        # Select only fixture-owned standalone prompts, not mentions inside that context.
        last_user = None
        for message in reversed(messages):
            text = last_user_text([message])
            if text in (self.server.owner_prompt, self.server.sibling_prompt):
                last_user = text
                break
        if last_user == self.server.sibling_prompt:
            self.sibling_request(messages)
        elif last_user == self.server.owner_prompt:
            self.owner_request()
        else:
            self.fail("chat request without a recognized standalone user prompt")

    def owner_request(self):
        server = self.server
        with server.lock:
            server.owner_inferences += 1
            count = server.owner_inferences
        if count != 1:
            self.fail(f"owner inference replayed (attempt {count})")
            return
        self.open_stream()
        try:
            self.write_chunk({"role": "assistant", "content": server.owner_reply})
            server.owner_started.set()
            # Hold the response open, sending keep-alive deltas, until the test releases it.
            while not server.owner_release.wait(0.2):
                self.write_chunk({"content": " "})
            self.write_chunk({"content": " " + server.owner_end})
            self.write_chunk({}, "stop")
            server.owner_finished.set()
        except (BrokenPipeError, ConnectionResetError):
            with server.lock:
                server.owner_disconnected = True

    def sibling_request(self, messages):
        server = self.server
        with server.lock:
            if server.sibling_messages is not None:
                duplicate = True
            else:
                duplicate = False
                server.sibling_messages = messages
        if duplicate:
            self.fail("sibling sent more than one chat request")
            return
        self.open_stream()
        self.write_chunk({"role": "assistant", "content": server.sibling_answer})
        self.write_chunk({}, "stop")
        server.sibling_seen.set()


def session_journals(home):
    """Every journal under HOME whose first line is a session header, with its parsed header."""
    found = []
    for path in sorted(home.rglob("*.jsonl")):
        if "sessions" not in path.parts:
            continue
        first = path.read_bytes().split(b"\n", 1)[0]
        try:
            header = json.loads(first)
        except ValueError:
            continue
        if isinstance(header, dict) and header.get("type") == "session":
            found.append((path, header))
    return found


def require(event, server, what):
    assert event.wait(WAIT), f"{what} never happened; fixture errors: {server.fixture_errors}"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--dial", type=pathlib.Path, required=True)
    parser.add_argument("--captures", type=pathlib.Path)
    args = parser.parse_args()
    binary = args.dial.resolve(strict=True)

    token = uuid.uuid4().hex[:12]
    server = Server(token)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        with tempfile.TemporaryDirectory(prefix="dial-session-contention-") as directory:
            root = pathlib.Path(directory)
            project, home = fixture(root, server.server_port)
            owner = None
            sibling = None
            try:
                owner = Terminal(binary, project, home, args.captures)
                owner.wait("mock-main")
                owner.command(server.owner_prompt)
                require(server.owner_started, server, "owner held request")
                owner.wait(server.owner_reply)

                owner_journals = session_journals(home)
                assert len(owner_journals) == 1, f"expected one owner journal, saw {len(owner_journals)}"
                owner_path, owner_header = owner_journals[0]
                owner_id = owner_header["id"]
                assert "forked_from" not in owner_header, "owner must be an original session"
                owner_bytes = owner_path.read_bytes()
                owner.capture("owner-held")

                sibling = Terminal(binary, project, home, args.captures, arguments=("--continue",))
                sibling.wait("mock-main")
                # Positive runtime evidence: the sibling UI surfaces the observed (interrupted)
                # prefix as recovery state, not as a completed assistant message.
                sibling.wait(server.owner_reply)
                sibling.capture("sibling-recovered-prefix")
                sibling.command(server.sibling_prompt)
                require(server.sibling_seen, server, "sibling request")
                sibling.wait(server.sibling_answer)
                sibling.capture("sibling-forked")

                assert not server.owner_disconnected, "owner held stream disconnected during contention"
                assert server.owner_inferences == 1, "owner inference must not be replayed"
                assert owner_path.read_bytes() == owner_bytes, "owner main journal changed while held"

                after = [item for item in session_journals(home) if item[0] != owner_path]
                assert len(after) == 1, f"expected exactly one sibling journal, saw {len(after)}"
                sibling_path, sibling_header = after[0]
                sibling_id = sibling_header["id"]
                assert sibling_id != owner_id, "sibling must have its own session identity"
                assert sibling_header.get("forked_from") == owner_id, (
                    f"sibling must record owner as forked_from: {sibling_header}"
                )

                # Canonical source history is retained: the owner's original user prompt is in the
                # sibling's provider request, and the sibling's new request comes last.
                messages = server.sibling_messages
                assert messages is not None, "sibling request was not recorded"
                user_texts = [
                    last_user_text(messages[: index + 1])
                    for index, message in enumerate(messages)
                    if message.get("role") == "user"
                ]
                assert any(server.owner_prompt in text for text in user_texts), (
                    "sibling request lost the original user/prefix history"
                )
                submitted = [text for text in user_texts if text in (server.owner_prompt, server.sibling_prompt)]
                assert submitted == [server.owner_prompt, server.sibling_prompt], submitted
                assert not any(
                    message.get("role") == "tool" or message.get("tool_calls") for message in messages
                ), "sibling request must not replay tool calls"

                # Both terminals stay usable: release the owner's held stream and let it finish.
                server.owner_release.set()
                require(server.owner_finished, server, "owner stream completion")
                owner.wait(server.owner_end)
                owner.capture("owner-after-release")
                assert not server.owner_disconnected, "owner stream was disconnected"

                owner_final = owner_path.read_text()
                sibling_final = sibling_path.read_text()
                assert server.owner_end in owner_final, "owner did not commit its completed reply"
                assert server.sibling_prompt not in owner_final, "sibling request leaked into owner"
                assert server.sibling_answer not in owner_final, "sibling answer leaked into owner"
                assert server.owner_end not in sibling_final, "owner reply leaked into sibling"
                assert server.sibling_answer in sibling_final, "sibling did not persist its answer"
                # The interrupted prefix is never committed as a completed message in the sibling.
                for line in sibling_final.splitlines()[1:]:
                    entry = json.loads(line)
                    if entry.get("type") == "message":
                        assert server.owner_reply not in json.dumps(entry), (
                            "interrupted prefix committed as a completed sibling message"
                        )
                assert not server.fixture_errors, server.fixture_errors
            finally:
                server.owner_release.set()
                for terminal in (sibling, owner):
                    if terminal is not None:
                        terminal.close()
    finally:
        server.owner_release.set()
        server.shutdown()
        server.server_close()
        thread.join(timeout=5)

    print(json.dumps({
        "kind": "session-writer-contention-smoke",
        "owner_session": owner_id,
        "sibling_session": sibling_id,
        "sibling_forked_from": owner_id,
        "owner_journal_unchanged_while_held": True,
        "owner_inferences": server.owner_inferences,
        "sibling_ui_shows_observed_prefix": True,
        "sibling_history_has_owner_prompt": True,
        "sibling_completed_messages_exclude_prefix": True,
        "fixture_errors": server.fixture_errors,
        "checks": ["writer-contention-fork", "fork-provenance", "history-isolation",
                   "owner-journal-bytes-stable", "no-inference-replay", "both-terminals-usable",
                   "observed-prefix-recovery-ui"],
        "pixel_screenshot_verification": False,
        "model_quality_evidence": False,
        "limitations": [
            "terminal captures are text evidence only",
            "stream sidecar is not asserted directly; the sibling UI prefix is the observable proxy",
            "not run; assumes the sibling TUI renders the recovered observed prefix",
        ],
    }))


if __name__ == "__main__":
    main()
