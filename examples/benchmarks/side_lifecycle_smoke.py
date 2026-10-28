#!/usr/bin/env python3
"""Real-PTY side-thread lifecycle: SIGKILL persistence, same-ID /move, and UI delete.

Each run uses an isolated tmux server, an independent strict local provider and generated
fixture data only; no user files are read or written. Captures are terminal text, NOT pixel
screenshots, and this script makes no model-quality or performance claims.
Run after building: python3 examples/benchmarks/side_lifecycle_smoke.py --dial target/debug/dial
"""
import argparse
import http.server
import json
import pathlib
import secrets
import subprocess
import sys
import tempfile
import threading
import time

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
import interactive_smoke as smoke  # noqa: E402  (shared fixture and PTY terminal)

HEARTBEAT = b": heartbeat\n\n"
HOLD_DEADLINE = 60


def markers(nonce):
    # Every fixture string carries the per-run nonce so UI selection never depends on prose.
    return {
        "main": f"LIFE-MAIN-{nonce}",
        "main_answer": f"LIFE-MAIN-ANSWER-{nonce}",
        "held": f"LIFE-HELD-{nonce}",
        "prefix": f"LIFE-PREFIX-{nonce}",
        "after": f"LIFE-AFTER-Q-{nonce}",
        "after_answer": f"LIFE-AFTER-ANSWER-{nonce}",
    }


def text_of(content):
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        return "\n".join(part.get("text", "") for part in content if isinstance(part, dict))
    return ""


def completion(answer):
    return {
        "id": "life-completion",
        "object": "chat.completion",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": answer}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 128, "completion_tokens": 32},
    }


class Provider(http.server.ThreadingHTTPServer):
    daemon_threads = True

    def __init__(self, nonce):
        super().__init__(("127.0.0.1", 0), Handler)
        self.nonce = nonce
        self.markers = markers(nonce)
        self.lock = threading.Lock()
        self.counts = {"main": 0, "side-held": 0, "side-after": 0, "models": 0}
        self.errors = []
        self.held_started = threading.Event()
        self.held_disconnected = threading.Event()
        self.release_held = threading.Event()

    def error(self, message):
        with self.lock:
            self.errors.append(message)

    def count(self, kind):
        with self.lock:
            self.counts[kind] += 1


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
            pass  # The SIGKILL test deliberately closes connections.

    def event(self, delta, finish=None):
        body = {"id": "life-stream", "object": "chat.completion.chunk",
                "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]}
        self.wfile.write(("data: " + json.dumps(body) + "\n\n").encode())
        self.wfile.flush()

    def do_GET(self):
        if self.path.rstrip("/") == "/v1/models":
            self.server.count("models")
            self.respond({"data": [{"id": "mock-main"}]})
        elif smoke.unsupported_probe(self):
            return
        else:
            self.server.error(f"unexpected GET {self.path}")
            self.respond({"error": "fixture endpoint not found"}, 404)

    def do_POST(self):
        server = self.server
        if not self.path.rstrip("/").endswith("/chat/completions"):
            server.error(f"unexpected POST {self.path}")
            self.respond({"error": "fixture endpoint not found"}, 404)
            return
        request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        messages = request.get("messages", [])
        last = next((text_of(m.get("content")) for m in reversed(messages) if m.get("role") == "user"), "")
        context = json.dumps(messages)
        m = server.markers
        if last == m["main"]:
            server.count("main")
            self.respond(completion(m["main_answer"]))
        elif m["held"] in last or m["after"] in last:
            # Side requests are tools-free and must still see the completed main conversation.
            if request.get("tools"):
                server.error("side request carried tools")
            if m["main_answer"] not in context:
                server.error("side request lost the completed main context")
            if m["held"] in last:
                server.count("side-held")
                self.hold(m["prefix"])
            else:
                server.count("side-after")
                self.respond(completion(m["after_answer"]))
        else:
            server.error(f"unexpected chat request: {last[:120]!r}")
            self.respond({"error": "unexpected fixture request"}, 500)

    def hold(self, prefix):
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.end_headers()
        self.close_connection = True
        server = self.server
        try:
            self.event({"role": "assistant", "content": prefix})
            server.held_started.set()
            deadline = time.monotonic() + HOLD_DEADLINE
            # Heartbeat comments keep the socket probed so a SIGKILLed client is detected promptly.
            while not server.release_held.wait(0.1):
                if time.monotonic() >= deadline:
                    server.error("held side request deadline")
                    return
                self.wfile.write(HEARTBEAT)
                self.wfile.flush()
            self.event({}, "stop")
            self.wfile.write(b"data: [DONE]\n\n")
            self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            server.held_disconnected.set()


def session_files(home, session_id=None):
    """Session journals under any project's sessions directory, as (path, header) pairs."""
    found = []
    for path in home.rglob("*.jsonl"):
        if path.parent.name != "sessions":
            continue
        with path.open() as handle:
            first = handle.readline()
        try:
            header = json.loads(first)
        except json.JSONDecodeError:
            continue
        if header.get("type") == "session" and (session_id is None or header.get("id") == session_id):
            found.append((path, header))
    return found


def side_of(session_path):
    return session_path.with_suffix(".side.json")


def exchanges_with(saved, question):
    return [e for thread in saved["threads"] for e in thread["exchanges"] if e["question"] == question]


def scenario(binary, server, captures, root):
    m = server.markers
    project, home = smoke.fixture(root, server.server_port)
    destination = root / "moved-project"
    destination.mkdir()
    subprocess.run(["git", "-c", "core.fsmonitor=false", "-c", "init.templateDir=", "init", "--quiet",
                    "--initial-branch=main", str(destination)], check=True, capture_output=True, timeout=10)
    terminal = smoke.Terminal(binary, project, home, captures)
    try:
        # 1. One completed main conversation.
        terminal.wait("mock-main")
        terminal.command(m["main"])
        terminal.wait(m["main_answer"])
        terminal.capture("life-main-completed")
        assert server.counts["main"] == 1, server.counts
        files = session_files(home)
        assert len(files) == 1, files
        source, header = files[0]
        session_id = header["id"]
        source_side = side_of(source)

        # 2. Tools-free side question streams a prefix, then is held until the process dies.
        terminal.command(f"/btw {m['held']}")
        assert server.held_started.wait(12), "side request never reached the provider"
        terminal.wait(m["prefix"])
        terminal.capture("life-side-prefix-visible")
        saved = json.loads(source_side.read_text())
        pending = saved.get("pending")
        assert pending is not None, saved
        assert pending["question"] == m["held"], pending
        assert m["prefix"] in pending["answer"], pending
        thread_id = pending["thread"]
        assert not exchanges_with(saved, m["held"]), "unfinished side answer was already an exchange"

        # 3. SIGKILL, then relaunch with --continue. Recovery must not reissue any request.
        terminal.crash_and_resume()
        assert server.held_disconnected.wait(12), "SIGKILL did not disconnect the side request"
        terminal.wait("mock-main")
        terminal.wait(m["main_answer"])
        terminal.command("/btw threads")
        terminal.wait("exchange(s)")
        saved = json.loads(source_side.read_text())
        assert "pending" not in saved, saved
        thread = next(t for t in saved["threads"] if t["id"] == thread_id)
        assert len(thread["exchanges"]) == 1, thread
        exchange = thread["exchanges"][0]
        assert exchange["outcome"] == "interrupted", exchange
        assert exchange["question"] == m["held"], exchange
        assert m["prefix"] in exchange["answer"], exchange
        terminal.command(f"/btw open {thread_id}")
        terminal.wait(f"(interrupted) {m['prefix']}")
        terminal.capture("life-interrupted-thread")
        assert server.counts["main"] == 1, server.counts
        assert server.counts["side-held"] == 1, server.counts
        assert server.counts["side-after"] == 0, server.counts

        # 4. Same-ID /move to a second project; the private side file must travel with the journal.
        before_side = source_side.read_bytes()
        terminal.command(f"/move {destination}")
        terminal.wait("Moved to")
        moved = session_files(home, session_id)
        assert len(moved) == 1, moved
        dest, dest_header = moved[0]
        assert dest != source, "session did not move to a new path"
        assert pathlib.Path(dest_header["cwd"]).resolve() == destination.resolve(), dest_header
        assert not source.exists(), "source journal was not retired"
        assert not source_side.exists(), "source side file was not retired"
        dest_side = side_of(dest)
        assert dest_side.read_bytes() == before_side, "side file bytes changed during the move"
        terminal.capture("life-moved")

        # 5. A new completed side question after the move updates only the destination sidecar.
        terminal.command(f"/btw new {m['after']}")
        terminal.wait(m["after_answer"])
        saved = json.loads(dest_side.read_text())
        answered = exchanges_with(saved, m["after"])
        assert len(answered) == 1, saved
        assert answered[0]["outcome"] == "answered", answered[0]
        assert answered[0]["answer"] == m["after_answer"], answered[0]
        assert dest_side.read_bytes() != before_side, "post-move side answer was not saved"
        assert next(t for t in saved["threads"] if t["id"] == thread_id)["exchanges"][0]["outcome"] == "interrupted"
        assert not source.exists() and not source_side.exists(), "old sidecar path was recreated"
        terminal.capture("life-after-move-side")
        assert server.counts["side-after"] == 1, server.counts
        assert server.counts["side-held"] == 1, server.counts
        assert server.counts["main"] == 1, server.counts

        # 6. Delete the moved session through the real confirmation; its side data must disappear.
        terminal.command("/delete")
        terminal.wait("Delete session?")
        terminal.wait(session_id)
        terminal.key("d")
        terminal.wait(f"Deleted session {session_id}")
        assert not dest.exists(), "moved journal survived deletion"
        assert not dest_side.exists(), "moved side file survived deletion"
        leftovers = [p for p in home.rglob("*") if p.name.startswith(dest.stem)]
        assert not leftovers, leftovers
        assert not session_files(home, session_id), "deleted session id still has a journal"
        terminal.capture("life-deleted")
    finally:
        terminal.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--dial", type=pathlib.Path, required=True)
    parser.add_argument("--captures", type=pathlib.Path)
    args = parser.parse_args()
    binary = args.dial.resolve(strict=True)
    nonce = secrets.token_hex(4)
    server = Provider(nonce)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        with tempfile.TemporaryDirectory(prefix="dial-side-lifecycle-") as directory:
            scenario(binary, server, args.captures, pathlib.Path(directory))
        assert not server.errors, server.errors
        print(json.dumps({
            "kind": "side-lifecycle-smoke",
            "checks": ["sigkill-persists-pending-prefix", "resume-converts-to-interrupted-without-reissue",
                       "same-id-move-carries-side-bytes", "post-move-side-updates-destination-only",
                       "ui-delete-removes-side-data"],
            "requests": dict(server.counts),
            "pixel_screenshot_verification": False,
            "model_quality_evidence": False,
            "performance_evidence": False,
        }))
    finally:
        server.release_held.set()
        server.shutdown()
        server.server_close()
        thread.join(timeout=5)


if __name__ == "__main__":
    main()
