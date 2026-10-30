#!/usr/bin/env python3
"""Real-PTY regression: peer wake after Completed replies, plus message timing.

Two real Dial TUI sessions share one fixture project and home. Each finishes a user prompt
(Completed reply), then session A queues `/peers send <B-id> ...` to idle session B. The
scripted local provider must receive a wake request and B must show the new assistant reply
without anyone submitting a prompt to B. The reply header must carry an HH:MM[:SS] timestamp
and the footer a measured `N.N tok/s` value (the provider supplies a delay and usage).

Local scripted OpenAI-compatible server only: no credentials, models, installs or paid network.
Captures are terminal text, NOT pixel screenshots.

Build first, then run after production settles:
  python3 examples/benchmarks/peer_timing_smoke.py --dial target/debug/dial
Optional: --hold-seconds N keeps the owned fixture tmux server alive after the assertions
pass so it can be attached and inspected (prints socket/session info, then sleeps N seconds).
"""
import argparse
import contextlib
import io
import http.server
import json
import pathlib
import re
import sys
import tempfile
import threading
import time

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
import interactive_smoke as smoke  # noqa: E402  reuse Terminal, fixture and config helpers

WAKE_MARKER = "PEER-TIMING-WAKE"
WAKE_REPLY = "PEER-WAKE-REPLY"
WAKE_DELAY = 1.0  # seconds before the wake reply; with 32 output tokens this yields a measurable rate
TIME_PATTERN = re.compile(r"Assistant · \d{4}-\d{2}-\d{2} (?:[01]\d|2[0-3]):[0-5]\d:[0-5]\d UTC")
RATE_PATTERN = re.compile(r"(\d+\.\d) tok/s")


class PeerProvider(http.server.ThreadingHTTPServer):
    daemon_threads = True

    def __init__(self):
        super().__init__(("127.0.0.1", 0), PeerHandler)
        self.discovery = smoke.DiscoveryGate()
        self.discovery.open()
        self.lock = threading.Lock()
        self.wake_requests = 0
        self.fixture_errors = []


class PeerHandler(smoke.Handler):
    def do_POST(self):
        body = self.rfile.read(int(self.headers["Content-Length"]))
        try:
            request = json.loads(body)
        except json.JSONDecodeError:
            request = {}
        messages = request.get("messages", [])
        encoded = json.dumps(messages)
        if WAKE_MARKER in encoded:
            with self.server.lock:
                self.server.wake_requests += 1
            time.sleep(WAKE_DELAY)
            self.respond(completion(WAKE_REPLY))
            return
        last_user = next((m.get("content") for m in reversed(messages)
                          if m.get("role") == "user" and m.get("content") in ("PEER-A-PROMPT", "PEER-B-PROMPT")), "")
        if last_user in ("PEER-A-PROMPT", "PEER-B-PROMPT"):
            time.sleep(0.5)
            self.respond(completion(last_user.replace("PROMPT", "DONE")))
            return
        # Everything else keeps the shared fixture behaviour; hand it the already-read body.
        self.rfile = io.BytesIO(body)
        super().do_POST()


def completion(content):
    return {"id": "peer-timing", "object": "chat.completion", "choices": [
        {"index": 0, "message": {"role": "assistant", "content": content}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 128, "completion_tokens": 32}}


def own_and_peer_ids(terminal):
    """Read the latest /peers notice: own id at depth one, peer ids at depth two."""
    notice = terminal.text().rsplit("PROJECT PEERS\n", 1)[-1]
    own = re.findall(r"^  id: ([0-9a-f]{32})\s*$", notice, re.MULTILINE)
    peers = re.findall(r"^    id: ([0-9a-f]{32})\s*$", notice, re.MULTILINE)
    return (own[-1] if own else None), peers


def read_identity(terminal, needs_peer=None, timeout=20):
    """Run /peers until this session's id is visible (and needs_peer, if given, is listed)."""
    deadline = time.monotonic() + timeout
    while True:
        terminal.command("/peers")
        time.sleep(0.5)
        own, peers = own_and_peer_ids(terminal)
        if own and (needs_peer is None or needs_peer in peers):
            return own
        if time.monotonic() >= deadline:
            raise AssertionError(f"/peers never listed the expected ids; own={own} peers={peers}\n{terminal.text()}")


def timing_checks(text):
    """Timestamp in the reply header block and a measured tok/s footer after the reply."""
    lines = text.splitlines()
    reply_line = max(i for i, line in enumerate(lines) if WAKE_REPLY in line)
    header = "\n".join(lines[max(0, reply_line - 8):reply_line + 1])
    assert TIME_PATTERN.search(header), f"no HH:MM[:SS] timestamp near the wake reply:\n{header}"
    after = "\n".join(lines[reply_line:])
    rates = [float(value) for value in RATE_PATTERN.findall(after)]
    assert rates and max(rates) > 0, f"no measured tok/s footer after the wake reply:\n{after}"
    return max(rates)


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--dial", type=pathlib.Path, required=True)
    parser.add_argument("--captures", type=pathlib.Path)
    parser.add_argument("--hold-seconds", type=int, default=0,
                        help="after successful assertions, keep the fixture tmux sessions alive N seconds")
    args = parser.parse_args()
    if args.hold_seconds < 0:
        parser.error("--hold-seconds must be >= 0")
    binary = args.dial.resolve(strict=True)
    server = PeerProvider()
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    first = second = None
    try:
        with tempfile.TemporaryDirectory(prefix="dial-pty-peer-timing-") as directory, contextlib.ExitStack() as terminals:
            project, home = smoke.fixture(pathlib.Path(directory), server.server_port)
            first = smoke.Terminal(binary, project, home, args.captures)
            terminals.callback(first.close)
            second = smoke.Terminal(binary, project, home, args.captures)
            terminals.callback(second.close)
            first.wait("mock-main")
            second.wait("mock-main")

            # Each session completes a user prompt, so both are idle Completed conversations.
            first.command("PEER-A-PROMPT")
            first.wait("PEER-A-DONE")
            second.command("PEER-B-PROMPT")
            second.wait("PEER-B-DONE")

            # Publish both identities, then confirm A can see B's messaging id.
            second_id = read_identity(second)
            first_id = read_identity(first, needs_peer=second_id)
            assert first_id != second_id, "both sessions reported the same peer id"

            # Queue a peer message from idle A to idle B; nothing is typed into B afterwards.
            wake_before = server.wake_requests
            first.command(f"/peers send {second_id} {WAKE_MARKER} please answer briefly")
            deadline = time.monotonic() + 20
            while server.wake_requests == wake_before and time.monotonic() < deadline:
                time.sleep(0.1)
            assert server.wake_requests > wake_before, "idle recipient was never woken by the peer message"
            reply_text = second.wait(WAKE_REPLY, timeout=25)
            # Text is visible before the completed-turn footer is retired to scrollback.
            deadline = time.monotonic() + 10
            while not RATE_PATTERN.search(reply_text[reply_text.rfind(WAKE_REPLY):]) and time.monotonic() < deadline:
                time.sleep(0.05)
                reply_text = second.text()
            rate = timing_checks(reply_text)
            second.capture("peer-wake-timing")
            first.capture("peer-sender")
            assert not server.fixture_errors, server.fixture_errors

            print(json.dumps({
                "kind": "real-pty-peer-timing-regression",
                "checks": ["both-completed-idle", "peer-ids-visible", "idle-recipient-wake",
                           "reply-timestamp", "measured-tok-s-footer"],
                "measured_tok_s": rate,
                "wake_requests": server.wake_requests,
                "pixel_screenshot_verification": False,
            }), flush=True)

            if args.hold_seconds:
                print(f"HOLD fixture tmux sockets (owned, test-only): A={first.socket} B={second.socket}", flush=True)
                print(f"attach: tmux -L {first.socket} attach -t smoke  |  tmux -L {second.socket} attach -t smoke", flush=True)
                print(f"holding for {args.hold_seconds}s before cleanup", flush=True)
                time.sleep(args.hold_seconds)
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=5)


if __name__ == "__main__":
    main()
