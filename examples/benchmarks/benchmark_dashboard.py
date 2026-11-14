#!/usr/bin/env python3
"""Local benchmark selection and live comparisons, using cross_agent's runner and grader."""
import argparse
import copy
import datetime
import html
import json
import os
from pathlib import Path
import secrets
import shutil
import signal
import threading
import time
import tomllib
import traceback
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import cross_agent


class InvalidRequest(ValueError):
    pass


class Busy(InvalidRequest):
    pass


METRICS = [("Input tokens (incl. cache)", "input_total"), ("Uncached input", "uncached_input"),
           ("Cache read", "cache_read"), ("Cache write", "cache_write"), ("Output tokens", "output"),
           ("Requests", "requests"), ("Tool calls", "tool_calls"), ("Tool errors", "tool_errors"),
           ("Delegations", "delegations")]


def comparison_svg(state):
    """Use the same measurements as the live scoreboard, with explicit partial coverage."""
    names = state.get("agents", [])
    height = 225 + 340 * max(1, len(names))
    esc = lambda value: html.escape(str(value), quote=True)
    parts = [f'<svg xmlns="http://www.w3.org/2000/svg" width="1200" height="{height}" viewBox="0 0 1200 {height}">',
             '<rect width="1200" height="100%" rx="20" fill="#101820"/>',
             '<g fill="#e8f0f6" font-family="system-ui, sans-serif">',
             '<text x="40" y="50" font-size="26" font-weight="700">Dial · benchmark comparison</text>',
             f'<text x="40" y="80" fill="#a8b9c8" font-size="14">{esc(state.get("model", "")[:95])} · {esc(state["status"])} · {state.get("completed", 0)}/{state.get("total", 0)} trials</text>',
             f'<text x="40" y="105" fill="#a8b9c8" font-size="13">{esc(", ".join(state.get("suites", []))[:150])}</text>']
    for index, name in enumerate(names):
        row = state.get("summary", {}).get(name, {})
        y = 135 + index * 340
        parts += [f'<rect x="28" y="{y}" width="1144" height="320" rx="10" fill="#1b2935"/>',
                  f'<text x="48" y="{y + 32}" font-size="21" font-weight="700">{esc(name[:40])}</text>']
        scored = row.get("scored_trials", 0)
        score = f'{row.get("passed_trials", 0)} / {scored} passed' if scored else 'not scored'
        parts.append(f'<text x="480" y="{y + 32}" font-size="16">{score} · {row.get("agent_errors", 0)} agent errors · {row.get("failed_check_trials", 0)} check failures</text>')
        metrics = []
        for label, key in [("Total wall", "total_elapsed_ms_all"), ("Median wall", "median_elapsed_ms_all"),
                           ("P95 wall", "p95_elapsed_ms_all")]:
            value = row.get(key)
            metrics.append((label, "unknown" if value is None else f"{value / 1000:.1f}s"))
        for label, field in METRICS:
            metric = row.get("measurements", {}).get(field, {})
            value = metric.get("total")
            coverage = f' [{metric.get("known_trials", 0)}/{metric.get("trials", scored)} trials]'
            metrics.append((label, "unknown" if value is None else f"{value:,.0f}" + coverage))
        for position, (label, value) in enumerate(metrics):
            x = 48 + (position % 3) * 375
            top = y + 67 + (position // 3) * 55
            parts += [f'<text x="{x}" y="{top}" font-size="12" fill="#a8b9c8">{esc(label)}</text>',
                      f'<text x="{x}" y="{top + 22}" font-size="17">{esc(value)}</text>']
        observed = ", ".join(row.get("observed_models", [])) or "unknown"
        parts.append(f'<text x="48" y="{y + 305}" font-size="11" fill="#a8b9c8">Observed models: {esc(observed[:150])}</text>')
    parts += [f'<text x="40" y="{height - 50}" font-size="12" fill="#a8b9c8">Sequential trials · installed-agent comparison · fresh Dial home; other agents retain installed state. Not a sandbox.</text>',
              f'<text x="40" y="{height - 28}" font-size="12" fill="#a8b9c8">Usage totals cover only known trials; unknown is not zero. Cache read/write are included in input, not extra input.</text>',
              '</g></svg>']
    return "\n".join(parts)


def private_json(path, value):
    """Publish a complete JSON snapshot atomically inside a private run directory."""
    temporary = path.with_suffix(".tmp")
    with temporary.open("w", encoding="utf-8") as stream:
        json.dump(value, stream, indent=2, allow_nan=False)
        stream.write("\n")
    temporary.replace(path)


class Benchmarks:
    """One run at a time. Validation and execution are shared by HTTP and offline tests."""
    def __init__(self, root, dial_executable=None):
        self.root = Path(root).resolve()
        self.tools = self.root / "examples" / "benchmarks"
        self.agent_file = self.tools / "cross-agent.toml"
        self.dial_executable = dial_executable
        self.lock = threading.Lock()
        self.cancel = threading.Event()
        self.thread = None
        self.state = {"status": "idle", "completed": 0, "total": 0, "trials": [], "summary": {}}

    def definitions(self):
        agents = cross_agent.load_agents(self.agent_file)
        if self.dial_executable:
            for agent in agents:
                if agent["output"] == "dial-json":
                    for key in ("first", "follow_up", "version"):
                        if agent.get(key):
                            agent[key][0] = self.dial_executable
        return agents

    def catalog(self):
        agents = [{"id": a["name"], "available": shutil.which(a["first"][0]) is not None,
                   "isolation": "fresh Dial home" if a["env"].get("DIAL_HOME", "").startswith("{trial_root}")
                   else "installed configuration"} for a in self.definitions()]
        suites = []
        for path in sorted(self.tools.glob("*.toml")):
            data = tomllib.loads(path.read_text())
            cases = data.get("cases")
            if not isinstance(cases, list) or not cases:
                continue
            portable = all(any(cross_agent.portable(c) for c in case.get("checks", [])) for case in cases)
            projects = all(not case.get("project") or (self.root / case["project"]).is_dir() for case in cases)
            # Integration/trace-specific suites require their native lane rather than a misleading score.
            special = any(c.get("integration") or any(str(tag).startswith(("registry_discovery", "integration"))
                          for tag in c.get("categories", [])) for c in cases)
            reason = "" if portable and projects and not special else "Requires the native benchmark lane or unavailable fixtures/checks"
            suites.append({"id": path.stem, "name": data.get("name", path.stem), "cases": len(cases),
                           "available": not reason, "reason": reason})
        return {"agents": agents, "suites": suites, "metrics": METRICS,
                "notice": "Uses cross_agent.py and local TOML suites. Harbor and SWE-bench require their separate provisioned runners; they are not silently mapped to local tasks."}

    def snapshot(self):
        with self.lock:
            return copy.deepcopy(self.state)

    def validate(self, request):
        allowed = {"agents", "suites", "model", "thinking", "repetitions", "timeout", "confirm"}
        if not isinstance(request, dict) or request.keys() - allowed:
            raise InvalidRequest("Unknown benchmark request fields")
        if request.get("confirm") is not True:
            raise InvalidRequest("Confirm model quota use and execution of selected agent CLIs and fixture checks")
        catalog = self.catalog()
        for key in ("agents", "suites"):
            selected = request.get(key)
            if (not isinstance(selected, list) or not selected or len(selected) > 32
                    or any(not isinstance(item, str) for item in selected) or len(set(selected)) != len(selected)):
                raise InvalidRequest(f"Select distinct {key} (1–32)")
            available = {item["id"] for item in catalog[key] if item["available"]}
            if not set(selected) <= available:
                raise InvalidRequest(f"Unknown or unavailable {key}")
        for key, default, low, high in [("repetitions", 3, 1, 10), ("timeout", 600, 1, 3600)]:
            value = request.get(key, default)
            if type(value) is not int or not low <= value <= high:
                raise InvalidRequest(f"{key} must be {low}–{high}")
        for key, default in [("model", ""), ("thinking", "off")]:
            value = request.get(key, default)
            if not isinstance(value, str) or not value.strip() or len(value) > 200 or any(ord(c) < 32 for c in value):
                raise InvalidRequest(f"Provide a valid {key}")
        if request.get("thinking", "off") not in ("off", "low", "medium", "high"):
            raise InvalidRequest("Thinking must be off, low, medium or high")
        # Read only files the catalog listed; request names select among them, never form a path.
        listed = {path.stem: path for path in self.tools.glob("*.toml")}
        suites = [tomllib.loads(listed[name].read_text()) for name in request["suites"]]
        cases = [case for suite in suites for case in suite["cases"]]
        ids = [case["id"] for case in cases]
        if len(set(ids)) != len(ids):
            raise InvalidRequest("Selected suites have overlapping case IDs; run them separately")
        total = len(cases) * len(request["agents"]) * request.get("repetitions", 3)
        if total > 1000:
            raise InvalidRequest("Selection exceeds 1000 trials; select fewer suites or repetitions")
        return cases, total

    def start(self, request):
        cases, total = self.validate(request)
        with self.lock:
            if self.thread is not None and self.thread.is_alive():
                raise Busy("A benchmark is already running")
            self.cancel.clear()
            stamp = datetime.datetime.now(datetime.timezone.utc).strftime("%Y%m%dT%H%M%SZ")
            output = self.root / "benchmarks" / "runs" / f"{stamp}-{secrets.token_hex(4)}"
            output.mkdir(parents=True, mode=0o700)
            self.state = {"status": "running", "completed": 0, "total": total, "trials": [], "summary": {},
                          "agents": list(request["agents"]), "suites": list(request["suites"]),
                          "model": request["model"], "thinking": request.get("thinking", "off"),
                          "repetitions": request.get("repetitions", 3), "timeout_secs": request.get("timeout", 600),
                          "output": str(output), "image": None, "error": None, "current": None,
                          "skipped_trials": [], "agent_issues": {}}
            private_json(output / "selection.json", {"version": 1, **request})
            self.thread = threading.Thread(target=self.run, args=(cases, output), daemon=True)
            self.thread.start()
        return self.snapshot()

    def stop(self):
        self.cancel.set()
        return self.snapshot()

    def run(self, cases, output):
        state = self.snapshot()
        work = output / "work"
        try:
            agents = [a for a in self.definitions() if a["name"] in state["agents"]]
            work.mkdir(mode=0o700)
            identities = {a["name"]: cross_agent.agent_identity(a, cancel=self.cancel) for a in agents}
            plan = cross_agent.trial_plan(agents, cases, state["repetitions"], 0)
            disabled = {}
            with self.lock:
                self.state["identities"] = identities
                self.state["trial_order"] = [f"{a['name']}/{c['id']}/r{r}" for a, c, r in plan]
            for agent, case, repetition in plan:
                if self.cancel.is_set():
                    break
                if agent["name"] in disabled:
                    with self.lock:
                        self.state["skipped_trials"].append({"agent": agent["name"], "case": case["id"],
                            "repetition": repetition, "reason": disabled[agent["name"]]})
                        private_json(output / "progress.json", self.state)
                    continue
                with self.lock:
                    self.state["current"] = {"agent": agent["name"], "case": case["id"], "repetition": repetition}
                trial = cross_agent.run_trial(agent, case, self.root, repetition, work,
                    {"model": state["model"], "thinking": state["thinking"]}, state["timeout_secs"], False,
                    logs=output / "logs", cancel=self.cancel)
                with self.lock:
                    self.state["trials"].append(trial)
                    self.state["completed"] += 1
                    # Setup failures (credentials, model, CLI flags) recur on every trial, so later
                    # trials of that agent are skipped. Text matching alone is not trusted: an agent
                    # that already ran tools got past setup, and its stderr may echo tool output.
                    if (trial.get("error_kind") in ("authentication", "model", "startup")
                            and trial["usage"].get("tool_calls") == 0):
                        detail = next((tail.strip() for t in reversed(trial["turns"])
                                       for tail in (t["stderr_tail"], t.get("result_tail", "")) if tail.strip()),
                                      trial["agent_error"])
                        issue = f"{agent['name']}: {trial['error_kind']} error — {detail}"
                        disabled[agent["name"]] = issue
                        self.state["agent_issues"][agent["name"]] = issue
                        self.state["error"] = "\n".join(disabled.values())
                    self.state["summary"], self.state["by_case"] = cross_agent.summarize(self.state["trials"], agents)
                    private_json(output / "progress.json", self.state)
            with self.lock:
                self.state["status"] = ("cancelled" if self.cancel.is_set() else
                    "incomplete" if self.state["skipped_trials"] else "completed")
                self.state["current"] = None
                if not self.state["trials"] or all(t["agent_error"] for t in self.state["trials"]):
                    self.state["status"] = "cancelled" if self.cancel.is_set() else "invalid"
                    self.state["error"] = self.state["error"] or "No successful agent execution; inspect trial diagnostics before comparing scores."
                self.state["excluded_checks"] = [f"{c['id']}#{i}:{check['kind']}" for c in cases
                    for i, check in enumerate(c.get("checks", [])) if not cross_agent.portable(check)]
                self.state["image"] = str(output / "comparison.svg")
                (output / "comparison.svg").write_text(comparison_svg(self.state))
                private_json(output / "report.json", {"kind": "cross_agent_dashboard", "version": 1, **self.state})
                private_json(output / "progress.json", self.state)
        except Exception as error:
            # Keep the cause in the private run directory; the browser only sees the summary.
            logs = output / "logs"
            try:
                logs.mkdir(mode=0o700, exist_ok=True)
                (logs / "runner-error.txt").write_text(cross_agent.redact(traceback.format_exc()))
                where = f"see {logs / 'runner-error.txt'}"
            except OSError:
                where = "the traceback could not be saved"
            with self.lock:
                self.state["status"] = "failed"
                self.state["error"] = f"Benchmark runner failed: {type(error).__name__}; {where}."
                self.state["current"] = None
                private_json(output / "report.json", self.state)
                private_json(output / "progress.json", self.state)
        finally:
            shutil.rmtree(work, ignore_errors=True)


def serve(root, dial_executable=None, port=0, parent_pid=None):
    engine = Benchmarks(root, dial_executable)
    token = secrets.token_urlsafe(24)
    prefix = f"/{token}"
    page = (Path(__file__).with_name("benchmark_dashboard.html")).read_bytes()

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass  # URLs contain the capability token; don't log them.

        def reply(self, status, body, content_type="application/json"):
            if not isinstance(body, bytes):
                body = json.dumps(body, allow_nan=False).encode()
            self.send_response(status)
            self.send_header("Content-Type", content_type)
            self.send_header("Content-Length", str(len(body)))
            self.send_header("Cache-Control", "no-store")
            self.send_header("X-Content-Type-Options", "nosniff")
            self.send_header("Referrer-Policy", "no-referrer")
            self.send_header("Content-Security-Policy", "default-src 'self'; script-src 'self' 'unsafe-inline'; style-src 'self' 'unsafe-inline'; img-src 'self' blob: data:; frame-ancestors 'none'; base-uri 'none'; form-action 'none'")
            self.end_headers()
            self.wfile.write(body)

        def valid(self):
            expected = f"127.0.0.1:{self.server.server_port}"
            origin = self.headers.get("Origin")
            return (self.headers.get("Host") == expected and (origin is None or origin == f"http://{expected}")
                    and self.path.startswith(prefix + "/"))

        def do_GET(self):
            if not self.valid():
                return self.reply(403, {"error": "Forbidden"})
            route = self.path[len(prefix):].split("?", 1)[0]
            if route == "/":
                return self.reply(200, page, "text/html; charset=utf-8")
            if route == "/api/catalog":
                return self.reply(200, engine.catalog())
            if route == "/api/state":
                return self.reply(200, engine.snapshot())
            if route in ("/comparison.svg", "/report.json"):
                state = engine.snapshot()
                if not state.get("image") or state["status"] == "running":
                    return self.reply(404, {"error": "No finalized comparison yet"})
                path = Path(state["output"]) / route[1:]
                return self.reply(200, path.read_bytes(), "image/svg+xml" if route.endswith("svg") else "application/json")
            self.reply(404, {"error": "Not found"})

        def do_POST(self):
            if not self.valid() or self.headers.get("X-Dial-Benchmark") != "1" or self.headers.get_content_type() != "application/json":
                return self.reply(403, {"error": "Forbidden"})
            try:
                length = int(self.headers.get("Content-Length", "0"))
                if not 0 < length <= 16384:
                    raise InvalidRequest("Request body must be 1–16384 bytes")
                request = json.loads(self.rfile.read(length))
                route = self.path[len(prefix):]
                if route == "/api/start":
                    return self.reply(200, engine.start(request))
                if route == "/api/cancel":
                    return self.reply(200, engine.stop())
                if route == "/api/shutdown":
                    engine.stop()
                    stopping.set()
                    return self.reply(200, {"status": "stopping"})
                self.reply(404, {"error": "Not found"})
            except (ValueError, OSError) as error:
                self.reply(409 if isinstance(error, Busy) else 400, {"error": str(error)})

    server = ThreadingHTTPServer(("127.0.0.1", port), Handler)
    server.daemon_threads = True
    server.timeout = .3
    print(json.dumps({"url": f"http://127.0.0.1:{server.server_port}{prefix}/"}), flush=True)
    stopping = threading.Event()
    for sig in (signal.SIGTERM, signal.SIGINT):
        signal.signal(sig, lambda *_: stopping.set())
    try:
        while not stopping.is_set() and (parent_pid is None or os.getppid() == parent_pid):
            server.handle_request()
    finally:
        engine.stop()
        if engine.thread:
            engine.thread.join(timeout=10)
        server.server_close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", required=True, type=Path)
    parser.add_argument("--dial-executable")
    parser.add_argument("--port", type=int, default=0)
    parser.add_argument("--parent-pid", type=int)
    args = parser.parse_args()
    serve(args.root, args.dial_executable, args.port, args.parent_pid)
