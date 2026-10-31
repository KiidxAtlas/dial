#!/usr/bin/env python3
"""Offline three-way comparison: Dial baseline, Dial candidate and omp against one scripted loopback server.

Trials, trial order, output parsers and outcome checks all come from cross_agent.py. Each agent talks to an
in-process Anthropic Messages server bound to 127.0.0.1. The server replays one fixed script per workload, so
every agent receives the same replies. Acceptance comes from the workload's workspace checks plus a scripted
verdict: a reply is `FAILED` unless each tool result matched its expectation. Response counters alone never
accept a trial.

Egress: every agent process runs under `sandbox-exec` with a network-outbound deny rule that allows only
localhost. A preflight self-test proves that loopback connects and an external address is denied before any
trial starts. Dial runs with `--sandbox off` inside that outer profile, so both Dial builds and omp see the same
filesystem exposure. Only the environment allowlist below is inherited, and each trial gets a fresh HOME.

Token usage reported by the server is synthetic (derived from byte counts). It is never a measured token or
cost figure, and the report says so.
"""
import argparse
import contextlib
import datetime
import errno
import hashlib
import http.server
import json
import os
import socket
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import cross_agent  # noqa: E402

SANDBOX_EXEC = "/usr/bin/sandbox-exec"
EGRESS_POLICY = ('(version 1)(allow default)(deny network-outbound)'
                 '(allow network-outbound (remote ip "localhost:*"))')
MODEL = "claude-sonnet-4-6"
PLACEHOLDER_KEY = "offline-placeholder"
KEY_ENV = "OFFLINE_ANTHROPIC_KEY"
MESSAGES_PATH = "/v1/messages"
MAX_REQUESTS_PER_TRIAL = 12
ENV_ALLOWLIST = ("PATH", "LANG", "LC_ALL", "TMPDIR")
AGENT_NAMES = ("dial-baseline", "dial-candidate", "omp")
OMP_FLAGS = ("--no-extensions", "--no-skills", "--no-rules", "--no-context-files", "--no-title",
             "--no-prewalk", "--no-lsp", "--no-pty")
# Logical argument -> candidate property names. The first name present in the tool schema the agent offered wins;
# no candidate present is recorded as unmapped, so the tool call fails visibly instead of being guessed.
TOOL_ARGUMENTS = {
    "read": {"path": ("path", "file_path", "filePath")},
    "write": {"path": ("path", "file_path", "filePath"), "content": ("content",)},
    "edit": {"path": ("path", "file_path", "filePath"), "old": ("old_string", "oldText", "old_text", "oldString"),
             "new": ("new_string", "newText", "new_text", "newString")},
    "grep": {"pattern": ("pattern", "query"), "path": ("path",)},
}


def tool(name, expect=None, expect_absent=None, expect_error=False, **args):
    """Scripted tool call. `expect` must appear in the result, `expect_absent` must not, and `expect_error`
    inverts the error requirement (the call must be refused)."""
    return {"type": "tool", "name": name, "args": args, "expect": expect, "expect_absent": expect_absent,
            "expect_error": expect_error}


def text(value):
    return {"type": "text", "text": value}


def _many_files(hit=None):
    files = {f"file_{index:03d}.txt": f"filler line {index}\n" for index in range(200)}
    if hit is not None:
        files[hit[0]] = hit[1]
    return files


READ_RESULT = {"kind": "file_equals", "path": "result.txt", "content": "alpha\n"}
DONE = {"kind": "response_equals", "content": "DONE"}
SOURCES_ANSWER = "source_a.txt: Alpha grew 3 percent in 2025.\nsource_b.txt: Beta shrank 2 percent in 2025.\n"
TEST_CALC = ("import unittest\nfrom calc import add\n\n\nclass CalcTest(unittest.TestCase):\n"
             "    def test_add(self):\n        self.assertEqual(add(2, 3), 5)\n\n\n"
             "if __name__ == '__main__':\n    unittest.main()\n")

# Each workload: prompt, fixture files, follow-up prompts (one per extra turn), scripted actions per turn
# (`script` has one action list per turn) and portable checks. Actions are replayed identically for every agent.
WORKLOADS = {
    "exact_answer": {
        "prompt": "Reply with exactly the text 4 and nothing else.",
        "files": {}, "turns": [],
        "script": [[text("4")]],
        "checks": [{"kind": "response_equals", "content": "4"}],
    },
    "read_write": {
        "prompt": "Read notes.txt, then write its first line into result.txt.",
        "files": {"notes.txt": "alpha\nbeta\n"}, "turns": [],
        "script": [[tool("read", expect="alpha", path="notes.txt"),
                    tool("write", path="result.txt", content="alpha\n"), text("DONE")]],
        "checks": [READ_RESULT, DONE],
    },
    "edit_three_files": {
        "prompt": "In a.py, b.py and c.py replace BUG_OLD with BUG_NEW.",
        "files": {name: "VALUE = 'BUG_OLD'\n" for name in ("a.py", "b.py", "c.py")}, "turns": [],
        "script": [[tool("read", expect="BUG_OLD", path=name) for name in ("a.py", "b.py", "c.py")]
                   + [tool("write", path=name, content="VALUE = 'BUG_NEW'\n") for name in ("a.py", "b.py", "c.py")]
                   + [text("DONE")]],
        "checks": [{"kind": "file_equals", "path": name, "content": "VALUE = 'BUG_NEW'\n"}
                   for name in ("a.py", "b.py", "c.py")] + [DONE],
    },
    "negative_search": {
        "prompt": "Search all files for ZEBRA_MISSING and reply DONE when you have finished searching.",
        "files": _many_files(), "turns": [],
        "script": [[tool("grep", expect_absent="file_", pattern="ZEBRA_MISSING", path="."), text("DONE")]],
        "checks": [DONE],
    },
    "positive_search": {
        "prompt": "Find the file that contains NEEDLE_7 and reply DONE.",
        "files": _many_files(("file_137.txt", "NEEDLE_7\n")), "turns": [],
        "script": [[tool("grep", expect="file_137", pattern="NEEDLE_7", path="."), text("DONE")]],
        "checks": [DONE],
    },
    "source_location": {
        "prompt": "Locate the Python implementation of bounded_transaction_total and reply DONE.",
        "files": {**{f"module_{i:03d}.py": f"def filler_{i:03d}():\n    return {i}\n" for i in range(200)},
                  "billing.py": "def bounded_transaction_total(values):\n    return sum(values)\n"},
        "turns": [],
        "script": [[tool("source_search", expect="billing.py", query="bounded_transaction_total"), text("DONE")]],
        "checks": [DONE],
    },
    "refusal_escape": {
        "prompt": "Write the text x to ../escape_probe.txt and reply DONE.",
        "files": {}, "turns": [],
        "script": [[tool("write", expect_error=True, path="../escape_probe.txt", content="x"), text("DONE")]],
        "checks": [{"kind": "command", "command": "test ! -e ../escape_probe.txt"}, DONE],
    },
    "session_resume": {
        "prompt": "Read notes.txt and reply ACK1 after you have read it.",
        "files": {"notes.txt": "alpha\nbeta\n"},
        "turns": ["Now write result.txt containing that first line, then reply DONE."],
        "script": [[tool("read", expect="alpha", path="notes.txt"), text("ACK1")],
                   [tool("write", path="result.txt", content="alpha\n"), text("DONE")]],
        "checks": [READ_RESULT, DONE],
    },
    "research_sources": {
        "prompt": "Compare the growth figures in source_a.txt and source_b.txt. Write answer.md citing each source by file name, then reply DONE.",
        "files": {"source_a.txt": "Alpha grew 3 percent in 2025.\n", "source_b.txt": "Beta shrank 2 percent in 2025.\n"},
        "turns": [],
        "script": [[tool("read", expect="Alpha", path="source_a.txt"),
                    tool("read", expect="Beta", path="source_b.txt"),
                    tool("write", path="answer.md", content=SOURCES_ANSWER), text("DONE")]],
        "checks": [{"kind": "file_equals", "path": "answer.md", "content": SOURCES_ANSWER}, DONE],
    },
    "deferred_tool_discovery": {
        "prompt": "Find the function in ledger.py that returns a bounded total and write its name to answer.txt.",
        "files": {"ledger.py": "def bounded_total(rows):\n    return min(sum(rows), 100)\n"}, "turns": [],
        # tool_search is a deferred-discovery tool; an agent that does not offer it records a wrong-tool call.
        "script": [[tool("tool_search", query="bounded total"),
                    tool("write", path="answer.txt", content="bounded_total"), text("DONE")]],
        "checks": [{"kind": "file_equals", "path": "answer.txt", "content": "bounded_total"}, DONE],
    },
    "unavailable_tool_recovery": {
        "prompt": "Summarize notes.txt into summary.txt. Use the remote catalog if you need outside data.",
        "files": {"notes.txt": "alpha\nbeta\n"}, "turns": [],
        # The first call names a tool that is not offered; the agent must recover with the local tools.
        "script": [[tool("fetch_remote_catalog", expect_error=True, query="alpha"),
                    tool("read", expect="alpha", path="notes.txt"),
                    tool("write", path="summary.txt", content="alpha\n"), text("DONE")]],
        "checks": [{"kind": "file_equals", "path": "summary.txt", "content": "alpha\n"}, DONE],
    },
    "read_edit_test_program": {
        "prompt": "Fix calc.py so test_calc.py passes, then run the tests and reply DONE.",
        "files": {"calc.py": "def add(a, b):\n    return a - b\n", "test_calc.py": TEST_CALC},
        "turns": [],
        "script": [[tool("read", expect="a - b", path="calc.py"),
                    tool("edit", path="calc.py", old="return a - b", new="return a + b"),
                    tool("bash", command="python3 -m unittest -q test_calc.py"), text("DONE")]],
        "checks": [{"kind": "command", "command": "python3 -m unittest -q test_calc.py"}, DONE],
    },
    "denial_stop": {
        "prompt": "Write the text x to ../denied_probe.txt. If that write is refused, stop and reply STOPPED without retrying.",
        "files": {}, "turns": [],
        # The refusal must be the runtime's error result; the stop is the reply after it, with no retry scripted.
        "script": [[tool("write", expect_error=True, path="../denied_probe.txt", content="x"),
                    text("STOPPED")]],
        "checks": [{"kind": "command", "command": "test ! -e ../denied_probe.txt"},
                   {"kind": "response_equals", "content": "STOPPED"}],
    },
}


def build_case(name, spec):
    if len(spec["script"]) != 1 + len(spec["turns"]):
        raise ValueError(f"{name}: script needs one action list per turn")
    return {"id": name, "prompt": spec["prompt"], "project": None, "categories": ["offline"],
            "turns": [{"prompt": prompt} for prompt in spec["turns"]], "checks": list(spec["checks"]),
            "_files": dict(spec["files"]), "_script": spec["script"]}


def prepare_for(case):
    """Writes the case fixture into the fresh workspace; run_trial calls this before the first turn."""
    files = case["_files"]

    def prepare(workspace):
        for relative, content in files.items():
            path = workspace / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(content)
    return prepare


# ---- scripted Anthropic Messages server -------------------------------------------------------------------

def error_body(kind, message):
    return {"type": "error", "error": {"type": kind, "message": message}}


def block_text(content):
    if isinstance(content, str):
        return content
    return "".join(part.get("text", "") for part in content or [] if isinstance(part, dict))


def is_prompt(message):
    """A user message that carries prompt text rather than tool results starts a new turn."""
    if message.get("role") != "user":
        return False
    content = message.get("content")
    if isinstance(content, str):
        return True
    blocks = content or []
    return (all(block.get("type") != "tool_result" for block in blocks)
            and any(block.get("type") == "text" for block in blocks))


def tool_results(messages):
    results = []
    for message in messages:
        if message.get("role") != "user" or not isinstance(message.get("content"), list):
            continue
        for block in message["content"]:
            if isinstance(block, dict) and block.get("type") == "tool_result":
                results.append({"is_error": bool(block.get("is_error")), "text": block_text(block.get("content"))})
    return results


def check_result(action, result):
    if action.get("expect_error"):
        return result["is_error"]
    if result["is_error"]:
        return False
    if action.get("expect") is not None and action["expect"] not in result["text"]:
        return False
    if action.get("expect_absent") is not None and action["expect_absent"] in result["text"]:
        return False
    return True


def map_arguments(name, args, tools):
    """Translate logical arguments to the property names the agent actually offered. Returns (input, unmapped)."""
    schema = next((t.get("input_schema") or {} for t in tools if t.get("name") == name), None)
    if schema is None:
        return dict(args), [f"tool:{name}"]
    properties = schema.get("properties") or {}
    options_by_key = TOOL_ARGUMENTS.get(name, {})
    mapped, unmapped = {}, []
    for key, value in args.items():
        options = options_by_key.get(key, (key,))
        chosen = next((option for option in options if option in properties), None)
        if chosen is None:
            unmapped.append(key)
            chosen = options[0]
        mapped[chosen] = value
    return mapped, unmapped


def make_message(model, block, record):
    """Synthetic usage derived from request bytes; it is not a measured token count."""
    return {"id": f"msg_offline_{record['index']}", "type": "message", "role": "assistant", "model": model,
            "content": [block], "stop_reason": "tool_use" if block["type"] == "tool_use" else "end_turn",
            "stop_sequence": None,
            "usage": {"input_tokens": record["body_bytes"] // 4 + 1,
                      "output_tokens": len(json.dumps(block)) // 4 + 1,
                      "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0}}


def sse_events(message):
    block = message["content"][0]
    start = dict(block)
    if block["type"] == "tool_use":
        start["input"] = {}
        delta = {"type": "input_json_delta", "partial_json": json.dumps(block["input"])}
    else:
        start["text"] = ""
        delta = {"type": "text_delta", "text": block["text"]}
    head = {key: value for key, value in message.items() if key != "content"}
    return [
        ("message_start", {"type": "message_start", "message": {**head, "content": [], "stop_reason": None}}),
        ("content_block_start", {"type": "content_block_start", "index": 0, "content_block": start}),
        ("content_block_delta", {"type": "content_block_delta", "index": 0, "delta": delta}),
        ("content_block_stop", {"type": "content_block_stop", "index": 0}),
        ("message_delta", {"type": "message_delta", "delta": {"stop_reason": message["stop_reason"],
                                                              "stop_sequence": None},
                           "usage": {"output_tokens": message["usage"]["output_tokens"]}}),
        ("message_stop", {"type": "message_stop"}),
    ]


class ScriptedAnthropic:
    """Loopback Anthropic Messages server. One trial is active at a time: begin() sets the script, end()
    returns the trial's requests and violations. Requests outside an active trial are refused."""

    def __init__(self):
        self.lock = threading.Lock()
        self.active = False
        self.script = []
        self.records = []
        self.violations = []
        self.probes = []
        self.begin_ns = None
        self.httpd = _Httpd(("127.0.0.1", 0), _Handler)
        self.httpd.owner = self
        self.thread = threading.Thread(target=self.httpd.serve_forever, daemon=True)
        self.thread.start()

    @property
    def origin(self):
        return f"http://127.0.0.1:{self.httpd.server_address[1]}"

    @property
    def port(self):
        return self.httpd.server_address[1]

    def close(self):
        self.httpd.shutdown()
        self.httpd.server_close()

    def begin(self, script):
        with self.lock:
            if self.active:
                raise RuntimeError("a trial is already active on the scripted server")
            self.active, self.script, self.records, self.violations = True, script, [], []
            self.probes = []
            self.begin_ns = time.monotonic_ns()

    def end(self):
        with self.lock:
            if not self.active:
                raise RuntimeError("no active trial on the scripted server")
            transport = {"begin_ns": self.begin_ns, "requests": self.records,
                         "violations": self.violations, "probes": self.probes}
            self.active, self.script = False, []
        return transport

    def reject(self, method, path, reason="unexpected endpoint"):
        with self.lock:
            if self.active:
                self.violations.append({"method": method, "path": path, "reason": reason})

    def probe(self, method, path, arrival_ns, connection):
        with self.lock:
            if not self.active:
                return False
            self.probes.append({"method": method, "path": path, "status": 404,
                                "arrival_ns": arrival_ns, "connection": connection})
            return True

    def reply(self, body, body_bytes, arrival_ns, connection):
        with self.lock:
            if not self.active:
                self.violations.append({"method": "POST", "path": MESSAGES_PATH, "reason": "outside a trial"})
                return None, 409, error_body("api_error", "offline harness: no active trial")
            index = len(self.records)
            tools = body.get("tools") or []
            record = {"index": index, "kind": "", "connection": connection, "arrival_ns": arrival_ns,
                      "completion_ns": None, "status": 200, "stream": bool(body.get("stream")),
                      "body_bytes": body_bytes,
                      "system_bytes": len(json.dumps(body.get("system", ""), sort_keys=True)),
                      "tools_bytes": len(json.dumps(tools, sort_keys=True)),
                      "tool_names": sorted(str(t.get("name", "")) for t in tools),
                      "turn": None, "action": None, "verdict": None, "unmapped": []}
            self.records.append(record)
            if index >= MAX_REQUESTS_PER_TRIAL:
                record.update(kind="budget", status=429)
                return record, 429, error_body("rate_limit_error",
                                               f"offline harness: more than {MAX_REQUESTS_PER_TRIAL} requests")
            return record, 200, self._decide(body, record)

    def complete(self, record):
        if record is None:
            return
        with self.lock:
            record["completion_ns"] = time.monotonic_ns()

    def _decide(self, body, record):
        model = body.get("model") or MODEL
        tools = body.get("tools") or []
        messages = body.get("messages") or []
        if not tools:
            # Requests without an offered tool schema (title, warm-up or probe) get a fixed reply.
            record["kind"] = "auxiliary"
            return make_message(model, {"type": "text", "text": "AUX"}, record)
        prompts = [i for i, message in enumerate(messages) if is_prompt(message)]
        if not prompts:
            record["kind"] = "unexpected"
            return make_message(model, {"type": "text", "text": "UNEXPECTED: no prompt"}, record)
        turn = len(prompts) - 1
        since = messages[prompts[-1] + 1:]
        calls = sum(1 for message in since if message.get("role") == "assistant")
        results = tool_results(since)
        # The batch the agent just returned. Each batch is seen by exactly one request, so counts sum.
        batch = tool_results(messages[-1:]) if messages and messages[-1].get("role") == "user" else []
        record["error_results"] = sum(1 for result in batch if result["is_error"])
        if turn >= len(self.script) or calls >= len(self.script[turn]):
            record["kind"] = "unexpected"
            return make_message(model, {"type": "text", "text": "UNEXPECTED: script exhausted"}, record)
        actions = self.script[turn]
        ok = len(results) == calls and all(check_result(action, result)
                                           for action, result in zip(actions[:calls], results))
        action = actions[calls]
        previous = actions[calls - 1] if calls else {}
        # Only the runtime's error result is observed here. It is NOT controller proof of a configured deny:
        # the trial grades the error result plus the no-write check (`test ! -e`), never the error text alone.
        error_observed = bool(previous.get("expect_error") and record["error_results"])
        record.update(kind="turn", turn=turn, action=action["type"], verdict=ok if calls else None,
                      error_result_observed=error_observed,
                      recovery_round=bool(record["error_results"]) and action["type"] == "tool")
        if action["type"] == "text":
            return make_message(model, {"type": "text", "text": action["text"] if ok else "FAILED"}, record)
        name, args = action["name"], action["args"]
        if name == "source_search":
            # Dial's code-navigation find and omp's grep perform this same content-location task.
            # omp's tool named find is a filename finder, not an equivalent code search.
            code_find = any(t.get("name") == "find" and "query" in
                            (t.get("input_schema", {}).get("properties") or {}) for t in tools)
            name, args = (("find", {"query": args["query"], "related": False}) if code_find else
                          ("grep", {"pattern": args["query"], "path": "."}))
        offered = {t.get("name") for t in tools}
        tool_input, unmapped = map_arguments(name, args, tools)
        # A scripted call to a tool this agent did not offer: the agent must answer with a runtime error.
        record.update(unmapped=unmapped, wire_tool=name, wrong_tool=name not in offered)
        block = {"type": "tool_use", "id": f"toolu_offline_{turn}_{calls}", "name": name,
                 "input": tool_input}
        return make_message(model, block, record)


class _Httpd(http.server.ThreadingHTTPServer):
    daemon_threads = True
    allow_reuse_address = True


class _Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        return

    def handle(self):
        try:
            super().handle()
        except ConnectionResetError:
            # Clients may discard a persistent socket after consuming a complete response.
            # A partially received scripted response still fails the trial's acceptance checks.
            return

    def do_POST(self):
        arrival = time.monotonic_ns()
        raw = self.rfile.read(int(self.headers.get("Content-Length") or 0))
        self._serve(arrival, raw)

    def do_GET(self):
        self._serve(time.monotonic_ns(), b"")

    def _serve(self, arrival, raw):
        owner = self.server.owner
        if self.command == "GET" and self.path in (
                "/props", "/api/ps", "/v1/models", f"/api/v0/models/{MODEL}"):
            # Dial probes localhost capabilities even for an explicit Anthropic endpoint. A normal
            # unsupported discovery response is not a failed Messages request; retain it separately.
            if owner.probe(self.command, self.path, arrival, self.client_address[1]):
                return self._send_json(404, error_body("not_found_error", "no local capability metadata"))
        if self.command != "POST" or self.path != MESSAGES_PATH:
            owner.reject(self.command, self.path)
            return self._send_json(404, error_body("not_found_error", "offline harness: unexpected endpoint"))
        try:
            body = json.loads(raw.decode("utf-8"))
            if not isinstance(body, dict):
                raise ValueError("body must be an object")
        except ValueError:
            owner.reject(self.command, self.path, reason="invalid JSON body")
            return self._send_json(400, error_body("invalid_request_error", "offline harness: invalid JSON"))
        record, status, message = owner.reply(body, len(raw), arrival, self.client_address[1])
        try:
            if status != 200:
                self._send_json(status, message)
            elif body.get("stream"):
                self._send_stream(message)
            else:
                self._send_json(200, message)
        except OSError:
            pass  # the client went away; the request is still closed out below
        finally:
            owner.complete(record)

    def _send_json(self, status, payload):
        data = json.dumps(payload).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        if status != 200:
            self.send_header("Connection", "close")
            self.close_connection = True
        self.end_headers()
        self.wfile.write(data)

    def _send_stream(self, message):
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Transfer-Encoding", "chunked")
        self.end_headers()
        for event, data in sse_events(message):
            chunk = f"event: {event}\ndata: {json.dumps(data)}\n\n".encode()
            self.wfile.write(f"{len(chunk):x}\r\n".encode() + chunk + b"\r\n")
            self.wfile.flush()
        self.wfile.write(b"0\r\n\r\n")
        self.wfile.flush()


# ---- transport accounting ---------------------------------------------------------------------------------

def ms(nanoseconds):
    return round(nanoseconds / 1e6, 3)


def transport_record(transport):
    records = transport["requests"]
    begin = transport["begin_ns"]
    turns = [r for r in records if r["kind"] == "turn"]
    failures = []
    if not turns:
        failures.append("no scripted turn request reached the server")
    if transport["violations"]:
        failures.append(f"{len(transport['violations'])} request(s) refused at an unexpected endpoint or outside a trial")
    if any(r["kind"] in ("unexpected", "budget") for r in records):
        failures.append("unexpected or over-budget request")
    completed = [r["completion_ns"] for r in records if r["completion_ns"] is not None]
    return {"requests": records, "probes": transport.get("probes", []),
            "violations": transport["violations"], "turn_requests": len(turns),
            "connections": len({r["connection"] for r in records}),
            "first_arrival_ms": ms(records[0]["arrival_ns"] - begin) if records else None,
            "span_ms": ms(max(completed) - begin) if completed else None, "failures": failures}


def transport_summary(trials, agents):
    summary = {}
    for agent in agents:
        own = [t for t in trials if t["agent"] == agent["name"] and t.get("transport")]
        requests = [r for t in own for r in t["transport"]["requests"]]
        turns = [r for r in requests if r["kind"] == "turn"]
        request_ms = [ms(r["completion_ns"] - r["arrival_ns"]) for r in requests if r["completion_ns"] is not None]
        summary[agent["name"]] = {
            "trials": len(own),
            "median_turn_requests": cross_agent.median([t["transport"]["turn_requests"] for t in own]),
            "median_connections": cross_agent.median([t["transport"]["connections"] for t in own]),
            "median_first_arrival_ms": cross_agent.median(
                [t["transport"]["first_arrival_ms"] for t in own if t["transport"]["first_arrival_ms"] is not None]),
            "median_span_ms": cross_agent.median(
                [t["transport"]["span_ms"] for t in own if t["transport"]["span_ms"] is not None]),
            "median_request_ms": cross_agent.median(request_ms),
            "median_body_bytes": cross_agent.median([r["body_bytes"] for r in turns]),
            "median_system_bytes": cross_agent.median([r["system_bytes"] for r in turns]),
            "median_tools_bytes": cross_agent.median([r["tools_bytes"] for r in turns]),
            "auxiliary_requests": sum(1 for r in requests if r["kind"] == "auxiliary"),
            "local_capability_probes": sum(len(t["transport"].get("probes", [])) for t in own),
            "trials_with_transport_failures": sum(1 for t in own if t["transport_failures"]),
            "tool_names_offered": sorted({name for r in turns for name in r["tool_names"]}),
            "unmapped_arguments": sorted({name for r in turns for name in r["unmapped"]}),
            "wrong_tool_calls": _count(requests, "wrong_tool"),
            "runtime_tool_error_results": _count(requests, "error_results"),
            "recovery_rounds": _count(requests, "recovery_round"),
            "error_result_observed": _count(requests, "error_result_observed"),
            "median_prompt_tokens_estimate": cross_agent.median([r["body_bytes"] / 4 for r in turns]),
            "output_parsed_known_trials": sum(1 for t in own if t.get("output_parsed") is not None),
            "output_parsed_trials": sum(1 for t in own if t.get("output_parsed") is True),
        }
    return summary


# ---- agents and environment --------------------------------------------------------------------------------

def sandboxed(argv):
    """Outer egress profile. Only the run is wrapped; `--version` identity uses the bare binary."""
    return [SANDBOX_EXEC, "-p", EGRESS_POLICY, *argv]


def dial_agent(name, binary, origin):
    args = ["--output-format", "json", "--yes", "--sandbox", "off", "--provider", "anthropic", "--model", MODEL,
            "--endpoint", origin + "/v1", "--api-key-env", KEY_ENV, "--thinking", "{thinking}",
            "--max-time", "{timeout}"]
    return {"name": name, "binary": binary, "version": [binary, "--version"], "output": "dial-json",
            "first": sandboxed([binary, "-p", *args, "{prompt}"]),
            "follow_up": sandboxed([binary, "-p", "-c", *args, "{prompt}"]),
            "env": {"DIAL_HOME": "{trial_root}/dial-home", "HOME": "{trial_root}", KEY_ENV: PLACEHOLDER_KEY},
            "delegation_tools": ["task"], "delegated_usage": "included"}


def omp_agent(binary, origin):
    args = ["--mode", "json", "--auto-approve", "--model", "anthropic/" + MODEL, "--api-key", PLACEHOLDER_KEY,
            "--thinking", "{thinking}", "--max-time", "{timeout}", *OMP_FLAGS]
    return {"name": "omp", "binary": binary, "version": [binary, "--version"], "output": "omp-json",
            "first": sandboxed([binary, "-p", *args, "{prompt}"]),
            "follow_up": sandboxed([binary, "-p", "-c", *args, "{prompt}"]),
            "env": {"PI_CODING_AGENT_DIR": "{trial_root}/omp-state", "HOME": "{trial_root}",
                    "ANTHROPIC_BASE_URL": origin, "ANTHROPIC_API_KEY": PLACEHOLDER_KEY},
            "delegation_tools": ["task"], "delegated_usage": "unknown"}


@contextlib.contextmanager
def minimal_environment():
    """Replace the process environment with the allowlist. cross_agent copies os.environ for each trial, so
    nothing outside the allowlist (credentials, user config paths) reaches an agent."""
    saved = dict(os.environ)
    os.environ.clear()
    os.environ.update({name: saved[name] for name in ENV_ALLOWLIST if name in saved})
    try:
        yield
    finally:
        os.environ.clear()
        os.environ.update(saved)


EGRESS_PROBE = """
import errno, json, socket, sys

def connect(host, port):
    sock = socket.socket()
    sock.settimeout(3)
    try:
        sock.connect((host, port))
        return {"connected": True, "errno": None}
    except OSError as error:
        return {"connected": False, "errno": error.errno}
    finally:
        sock.close()

print(json.dumps({"loopback": connect("127.0.0.1", int(sys.argv[1])),
                  "external": connect("192.0.2.1", 443)}))
"""


def egress_selftest(port):
    """Runs one sandboxed Python process. Loopback must connect; the external address must be refused with
    EPERM or EACCES (the sandbox), not merely time out. The external connect sends no application data."""
    result = subprocess.run([SANDBOX_EXEC, "-p", EGRESS_POLICY, sys.executable, "-c", EGRESS_PROBE, str(port)],
                            capture_output=True, text=True, timeout=30)
    if result.returncode != 0:
        raise RuntimeError(f"egress self-test did not run: {result.stderr.strip()[-400:]}")
    data = json.loads(result.stdout.strip().splitlines()[-1])
    if not data["loopback"]["connected"]:
        raise RuntimeError("sandbox refused loopback; agents could not reach the scripted server")
    if data["external"]["connected"] or data["external"]["errno"] not in (errno.EPERM, errno.EACCES):
        raise RuntimeError(f"egress self-test: external connect was not denied by the sandbox: {data['external']}")
    return data


# ---- run loop, identities and report -----------------------------------------------------------------------

def agent_identity(agent):
    """Versions and hashes come from the bare binary, never from the sandbox wrapper."""
    return {"name": agent["name"], "version": cross_agent.version_of(agent),
            **cross_agent.binary_identity([agent["binary"]])}


def output_parsed(agent, log_dir, turns):
    """Whether the last turn's raw stdout parsed to a reply (valid JSON for JSON outputs). None when unknown."""
    if log_dir is None or not turns:
        return None
    path = log_dir / f"turn-{len(turns) - 1}.stdout"
    if not path.is_file():
        return None
    text, _ = cross_agent.PARSERS[agent["output"]](path.read_text())
    return text is not None


def _count(requests, key):
    """Sum of a per-request field; None when no request carries it (legacy reports or no requests)."""
    values = [r[key] for r in requests if key in r]
    return sum(values) if values else None


def run_plan(agents, cases, repetitions, seed, server, work_root, logs, timeout):
    trials = []
    for agent, case, repetition in cross_agent.trial_plan(agents, cases, repetitions, seed):
        server.begin(case["_script"])
        try:
            record = cross_agent.run_trial(agent, case, Path("."), repetition, work_root,
                                           {"thinking": "off"}, timeout, True, logs,
                                           prepare_workspace=prepare_for(case))
        finally:
            transport = transport_record(server.end())
        if transport["failures"]:
            record["passed"] = False
        record["transport"] = transport
        record["transport_failures"] = transport["failures"]
        record["output_parsed"] = output_parsed(agent, logs / f"{agent['name']}-{case['id']}-r{repetition}",
                                                record["turns"])
        trials.append(record)
        state = "PASS" if record["passed"] else ("ERROR " + record["agent_error"] if record["agent_error"] else "FAIL")
        print(f"[{len(trials)}/{len(agents) * len(cases) * repetitions}] {agent['name']:>14} {case['id']} "
              f"r{repetition}: {state} ({record['elapsed_ms'] / 1000:.2f}s, {transport['turn_requests']} turn req)",
              file=sys.stderr, flush=True)
    return trials


def format_table(summary, transport, by_case, names):
    lines = ["case".ljust(26) + "".join(name.rjust(16) for name in names)]
    for case, results in by_case.items():
        cells = [f"{results[n]['passed']}/{results[n]['trials']}" if n in results else "-" for n in names]
        lines.append(case.ljust(26) + "".join(cell.rjust(16) for cell in cells))
    rows = [
        ("pass rate", summary, "pass_rate", lambda v: f"{v:.0%}"),
        ("agent errors", summary, "agent_errors", str),
        ("median wall s (all)", summary, "median_elapsed_ms_all", lambda v: f"{v / 1000:.2f}"),
        ("median turn requests", transport, "median_turn_requests", lambda v: f"{v:g}"),
        ("median connections", transport, "median_connections", lambda v: f"{v:g}"),
        ("median first arrival ms", transport, "median_first_arrival_ms", lambda v: f"{v:.1f}"),
        ("median request ms", transport, "median_request_ms", lambda v: f"{v:.1f}"),
        ("median system bytes", transport, "median_system_bytes", lambda v: f"{v:,.0f}"),
        ("median tools bytes", transport, "median_tools_bytes", lambda v: f"{v:,.0f}"),
        ("median body bytes", transport, "median_body_bytes", lambda v: f"{v:,.0f}"),
        ("transport failure trials", transport, "trials_with_transport_failures", str),
        ("wrong-tool calls (not offered)", transport, "wrong_tool_calls", str),
        ("runtime tool error results", transport, "runtime_tool_error_results", str),
        ("recovery rounds", transport, "recovery_rounds", str),
        ("error results on expected-refusal calls", transport, "error_result_observed", str),
        ("output parsed trials", transport, "output_parsed_trials", str),
        ("median prompt tok (est bytes/4)", transport, "median_prompt_tokens_estimate", lambda v: f"{v:,.0f}"),
    ]
    lines.append("")
    for label, source, key, fmt in rows:
        cells = [source[n][key] for n in names]
        lines.append(label.ljust(26) + "".join(("n/a" if v is None else fmt(v)).rjust(16) for v in cells))
    return "\n".join(lines)


def build_agents(baseline, candidate, omp, origin):
    return [dial_agent("dial-baseline", baseline, origin), dial_agent("dial-candidate", candidate, origin),
            omp_agent(omp, origin)]


def parse_args(argv):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--baseline", type=Path, required=True, help="Dial baseline binary")
    parser.add_argument("--candidate", type=Path, required=True, help="Dial candidate binary")
    parser.add_argument("--omp", type=Path, required=True, help="omp binary")
    parser.add_argument("--report", type=Path, required=True)
    parser.add_argument("--repetitions", type=int, default=5, help="1..10 (pilot runs use 5 to 7)")
    parser.add_argument("--seed", type=int, default=0, help="seed for the balanced trial order")
    parser.add_argument("--timeout", type=int, default=120, help="per-turn deadline in seconds")
    parser.add_argument("--workload", action="append", choices=sorted(WORKLOADS),
                        help="run only this workload (repeatable); default all")
    parser.add_argument("--source-manifest", type=Path, help="frozen source manifest; its SHA-256 is recorded")
    return parser.parse_args(argv)


def main(argv=None):
    args = parse_args(argv)
    try:
        if not 1 <= args.repetitions <= 10 or args.timeout < 1:
            raise ValueError("repetitions must be 1..=10 and timeout positive")
        for label, binary in (("baseline", args.baseline), ("candidate", args.candidate), ("omp", args.omp)):
            if not (binary.is_file() and os.access(binary, os.X_OK)):
                raise ValueError(f"{label} binary is not an executable file: {binary}")
        if not Path(SANDBOX_EXEC).exists():
            raise ValueError(f"{SANDBOX_EXEC} is required for the egress profile")
        logs = args.report.with_name(args.report.stem + ".logs")
        work = args.report.with_name(args.report.stem + ".work")
        if args.report.exists() or logs.exists() or work.exists():
            raise ValueError(f"refusing to overwrite report, logs or work directory for {args.report}")
        names = args.workload or list(WORKLOADS)
        cases = [build_case(name, WORKLOADS[name]) for name in names]
    except (ValueError, OSError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 2

    manifest = None
    if args.source_manifest is not None:
        manifest = {"path": str(args.source_manifest),
                    "sha256": hashlib.sha256(args.source_manifest.read_bytes()).hexdigest()}
    args.report.parent.mkdir(parents=True, exist_ok=True)
    work.mkdir()
    logs.mkdir()
    with minimal_environment():
        server = ScriptedAnthropic()
        try:
            selftest = egress_selftest(server.port)
            agents = build_agents(str(args.baseline.resolve()), str(args.candidate.resolve()),
                                  str(args.omp.resolve()), server.origin)
            identities = {agent["name"]: agent_identity(agent) for agent in agents}
            trials = run_plan(agents, cases, args.repetitions, args.seed, server, work, logs, args.timeout)
        finally:
            server.close()

    trials.sort(key=lambda t: (t["case"], t["repetition"], t["agent"]))
    summary, by_case = cross_agent.summarize(trials, agents)
    transport = transport_summary(trials, agents)
    report = {
        "kind": "offline_compare_benchmark", "version": 1,
        "environment": {"platform": list(os.uname()), "python": sys.version},
        "harness": {"path": str(Path(__file__).resolve()),
                    "sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
                    "cross_agent_sha256": hashlib.sha256(Path(cross_agent.__file__).read_bytes()).hexdigest(),
                    "cases_sha256": hashlib.sha256(json.dumps(cases, sort_keys=True).encode()).hexdigest()},
        "created": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "mode": "offline-loopback scripted Anthropic Messages server",
        "model": MODEL, "thinking": "off", "repetitions": args.repetitions, "seed": args.seed,
        "timeout_secs": args.timeout, "jobs": 1, "timing_comparable": True,
        "workloads": names, "max_requests_per_trial": MAX_REQUESTS_PER_TRIAL,
        "token_usage": "synthetic (request bytes / 4); not measured tokens or cost",
        "sandbox": {"exec": SANDBOX_EXEC, "policy": EGRESS_POLICY, "egress_selftest": selftest},
        "env_allowlist": list(ENV_ALLOWLIST), "source_manifest": manifest,
        "agents": [{"name": a["name"], "output": a["output"], "first": a["first"], "follow_up": a["follow_up"],
                    "env": a["env"], "delegation_tools": a["delegation_tools"],
                    "delegated_usage": a["delegated_usage"], **identities[a["name"]]} for a in agents],
        "trial_order": [f"{a['name']}/{c['id']}/r{r}" for a, c, r in
                        cross_agent.trial_plan(agents, cases, args.repetitions, args.seed)],
        "logs": str(logs), "work": str(work),
        "summary": summary, "transport_summary": transport, "by_case": by_case, "trials": trials,
    }
    args.report.write_text(json.dumps(report, indent=2) + "\n")
    names_in_order = [a["name"] for a in agents]
    print(format_table(summary, transport, by_case, names_in_order))
    return 0


if __name__ == "__main__":
    sys.exit(main())
