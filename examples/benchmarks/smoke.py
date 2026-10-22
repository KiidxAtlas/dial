#!/usr/bin/env python3
"""Deterministic harness regression driver, not a model quality benchmark.

Run: python3 examples/benchmarks/smoke.py --dial ./target/debug/dial
Requires an already built binary and Python 3.11+. Uses a local scripted OpenAI
provider, a disposable project/home, real tools, and verified benchmark reports.
No network model, credentials, training, or generated success reports are used.
"""
import argparse
import http.server
import json
import pathlib
import re
import shutil
import subprocess
import tempfile
import threading
import tomllib


class ScriptedProvider(http.server.ThreadingHTTPServer):
    def __init__(self):
        super().__init__(("127.0.0.1", 0), Handler)
        self.requests = []
        self.lock = threading.Lock()
        self.truncated = set()
        self.errors = []


class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def do_GET(self):
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        self.wfile.write(json.dumps({"object": "list", "data": [{"id": "scripted", "object": "model"}]}).encode())

    def do_POST(self):
        try:
            length = int(self.headers.get("Content-Length", "0"))
            if not 0 < length <= 16 * 1024 * 1024:
                raise ValueError("invalid request size")
            request = json.loads(self.rfile.read(length))
            if not self.path.rstrip("/").endswith("chat/completions"):
                raise ValueError("scripted smoke supports OpenAI chat completions only")
            with self.server.lock:
                self.server.requests.append(request)
                index = len(self.server.requests)
            message, finish = self.reply(request, index)
            self.send_response(200)
            if request.get("stream"):
                self.send_header("Content-Type", "text/event-stream")
                self.end_headers()
                delta = {"role": "assistant"}
                if message.get("content"):
                    delta["content"] = message["content"]
                if message.get("reasoning_content"):
                    delta["reasoning_content"] = message["reasoning_content"]
                if message.get("tool_calls"):
                    delta["tool_calls"] = [dict(call, index=i) for i, call in enumerate(message["tool_calls"])]
                for value in [{"choices": [{"index": 0, "delta": delta, "finish_reason": None}]}, {"choices": [{"index": 0, "delta": {}, "finish_reason": finish}]}]:
                    self.wfile.write(("data: " + json.dumps(value) + "\n\n").encode())
                self.wfile.write(b"data: [DONE]\n\n")
            else:
                self.send_header("Content-Type", "application/json")
                self.end_headers()
                self.wfile.write(json.dumps({"choices": [{"index": 0, "message": message, "finish_reason": finish}]}).encode())
        except Exception as error:
            with self.server.lock:
                self.server.errors.append(str(error))
            self.send_response(500)
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            self.wfile.write(json.dumps({"error": {"message": str(error)}}).encode())

    def reply(self, request, index):
        messages = request["messages"]
        system = "\n".join(str(message.get("content", "")) for message in messages if message["role"] == "system")
        transcript = "\n".join(str(message.get("content", "")) for message in messages)
        if "Summarize user-AI coding-assistant conversations" in system:
            # Portable compaction may summarize archival windows without the original request.
            # Only the final task reply requires the goal: never invent one for an unrelated chunk.
            goal = self.goal(transcript, required=False)
            if goal is None:
                return self.text("Archival context only; this window states no acceptance goal."), "stop"
            return self.text("BENCH_CASE=goal-compaction. Immutable acceptance goal: result.txt must contain exactly " + goal + ". Archival notes are distractors. Read context.txt; complete original goal at finish."), "stop"
        if "rank repository evidence" in system:
            # Semantic `find` asks the configured model to pick candidate indices. Play that role the way
            # a model must: choose only from the supplied candidates, using their grounded source evidence.
            payload = json.loads(str(messages[-1]["content"]))
            chosen = [c["index"] for c in payload["candidates"] if "bounded_transaction_total" in c["source_evidence"]]
            return self.text(json.dumps({"indices": chosen[: payload["max_results"]]})), "stop"
        user = next((message for message in reversed(messages) if message["role"] == "user" and "BENCH_CASE=" in str(message.get("content", ""))), None)
        if user is None:
            raise ValueError("no benchmark scenario in current conversation")
        prompt = str(user["content"])
        case = re.search(r"BENCH_CASE=([\w-]+)", prompt).group(1)
        user_index = messages.index(user)
        calls = {}
        outputs = []
        for message in messages[user_index + 1:]:
            for call in message.get("tool_calls", []):
                calls[call["id"]] = call["function"]["name"]
            if message["role"] == "tool":
                outputs.append((calls.get(message.get("tool_call_id"), message.get("name")), str(message.get("content", ""))))
        names = [name for name, _ in outputs]
        def call(name, arguments):
            return {"role": "assistant", "content": "", "tool_calls": [{"id": "smoke-" + str(index), "type": "function", "function": {"name": name, "arguments": json.dumps(arguments)}}]}, "tool_calls"
        def write(path, content):
            if "write" in names:
                return self.text("Completed the verified requested write."), "stop"
            return call("write", {"path": path, "content": content})
        if case in ("selector-recovery", "reuse-evidence"):
            reads = names.count("read")
            if case == "selector-recovery" and reads == 0:
                return call("read", {"path": "calc.py:1- many"})
            if reads < (2 if case == "selector-recovery" else 1):
                return call("read", {"path": "calc.py:1-4"})
            if "write" not in names:
                return call("write", {"path": "calc.py", "content": "def add(a, b):\n    return a + b\n"})
            if "bash" not in names:
                return call("bash", {"command": "python3 -c 'from calc import add; assert add(-7,3)==-4; assert add(11,-2)==9'"})
            return self.text("Corrected the arithmetic and verified the requested behavior."), "stop"
        if case == "baseline-write":
            return write("answer.txt", "baseline-ok")
        if case == "goal-compaction":
            if "phase=finish" in prompt:
                return write("result.txt", self.goal(transcript))
            if "phase=remember" in prompt:
                return self.text("Remembering the original acceptance goal without writing it yet."), "stop"
            if "read" not in names:
                return call("read", {"path": "context.txt"})
            return self.text("Archival distractors do not override the immutable goal."), "stop"
        if case == "tool-contract-recovery":
            if not outputs:
                return call("write", {"path": "recovered.txt"})
            if len(outputs) == 1:
                if not re.search(r"error|required|missing|content", outputs[0][1], re.IGNORECASE):
                    raise ValueError("malformed tool call did not report a useful failure")
                return call("write", {"path": "recovered.txt", "content": "contract-recovered"})
            return self.text("Observed contract error and recovered."), "stop"
        if case == "artifact-retrieval":
            if "bash" not in names:
                return call("bash", {"command": "python3 -c 'print(\"noise\\n\" * 15000); print(\"artifact-secret=VIOLET-428\")'", "timeout": 20})
            if "artifact_read" not in names:
                uri = re.search(r"artifact://[0-9a-f]{64}", "\n".join(output for _, output in outputs))
                if not uri:
                    raise ValueError("oversized shell output lacks a full artifact URI")
                return call("artifact_read", {"uri": uri.group(0), "start": 14980, "count": 100})
            artifact = next(output for name, output in outputs if name == "artifact_read")
            secret = re.search(r"artifact-secret=([A-Z]+-\d+)", artifact)
            if not secret:
                raise ValueError("artifact pagination did not retrieve the actual secret")
            return write("artifact-result.txt", secret.group(1))
        if case == "dynamic-tool-discovery":
            if "tool_search" not in names:
                return call("tool_search", {"query": "find"})
            if "tool_load" not in names:
                return call("tool_load", {"names": ["find"]})
            if "find" not in names:
                available = {tool["function"]["name"] for tool in request.get("tools", [])}
                if "find" not in available:
                    return self.text("The selected configuration does not provide find; this case cannot be completed."), "stop"
                return call("find", {"query": "function computes bounded transaction sum", "path": "."})
            result = next(output for name, output in outputs if name == "find")
            if "bounded_transaction_total" not in result:
                raise ValueError("find failed to discover fixture implementation")
            return write("dynamic-result.txt", "bounded_transaction_total")
        if case == "reasoning-truncated-recovery":
            key = (request.get("model", "scripted"), system)
            with self.server.lock:
                truncate = key not in self.server.truncated
                self.server.truncated.add(key)
            if truncate:
                return {"role": "assistant", "content": "", "reasoning_content": "Computing 17 times 19; output budget reached."}, "length"
            return write("reasoning-result.txt", str(17 * 19))
        raise ValueError("unknown scenario: " + case)

    @staticmethod
    def goal(transcript, required=True):
        matches = re.findall(r"(?:acceptance goal[^\n]*?exactly|actual task goal is)\s+([A-Z]+-\d+)", transcript, re.IGNORECASE)
        if not matches:
            if required:
                raise ValueError("original goal was not preserved in the conversation")
            return None
        return matches[0]

    @staticmethod
    def text(content):
        return {"role": "assistant", "content": content}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--dial", required=True, type=pathlib.Path)
    parser.add_argument("--report", type=pathlib.Path, help="persist the observed report outside the disposable workspace")
    variants = parser.add_mutually_exclusive_group()
    variants.add_argument("--profiles", type=pathlib.Path, help="operator profile-comparison file; copied unchanged, configurations must exist")
    variants.add_argument("--machine-repetitions", type=int, choices=range(1, 11), help="exercise repeated baseline/compact-tool configurations against the local scripted provider")
    parser.add_argument("--suite", choices=("capabilities", "recovery", "compaction"), default="capabilities", help="select executable harness cases; scripted runs are not model-quality evidence")
    args = parser.parse_args()
    binary = args.dial.resolve(strict=True)
    fixtures = pathlib.Path(__file__).resolve().parent
    tomllib.loads((fixtures / (args.suite + ".toml")).read_text())
    server = ScriptedProvider()
    worker = threading.Thread(target=server.serve_forever, daemon=True)
    worker.start()
    try:
        with tempfile.TemporaryDirectory(prefix="dial-benchmark-smoke-") as temporary:
            root = pathlib.Path(temporary)
            project = root / "project"
            shutil.copytree(fixtures / ("project" if args.suite == "capabilities" else "efficiency-fixture"), project)
            suite_source = (fixtures / (args.suite + ".toml")).read_text()
            suite_data = tomllib.loads(suite_source)
            expected_cases = len(suite_data["cases"])
            if args.suite == "capabilities":
                goal_case = next(case for case in suite_data["cases"] if case["id"] == "goal-compaction")
                archival = (project / "context.txt").read_text()
                seeded_context = goal_case["context_messages"] + [archival[offset:offset + 8000] for offset in range(0, len(archival), 8000)]
                suite_source = re.sub(r"(?m)^context_messages = .*?$", lambda _: "context_messages = " + json.dumps(seeded_context), suite_source, count=1)
            else:
                # CLI --dir points at the copied fixture; keep the original checks/prompts.
                suite_source = re.sub(r"(?m)^project = .*?\n", "", suite_source)
            manifest = root / (args.suite + "-smoke.toml")
            manifest.write_text(suite_source)
            home = root / "home"
            home.mkdir(mode=0o700)
            config = 'model="scripted"\nprovider="openai"\ncontext_window=32768\n[endpoint]\nbase_url="http://127.0.0.1:' + str(server.server_port) + '/v1"\nallow_insecure=true\n[improve]\nenabled=false\n[harness]\nmax_steps=20\nmax_tokens=4096\n'
            for name, model, edit in [("baseline", "scripted-base", "replace"), ("without-integrations", "scripted-ablated", "replace"), ("hashline", "scripted-hashline", "hashline")]:
                config += '[model_profiles.' + name + ']\nprovider="openai"\nmodel="' + model + '"\nmax_tokens=8192\nedit_mode="' + edit + '"\n[model_profiles.' + name + '.capabilities]\ntemperature=true\ntools=true\noutput_limit="max-tokens"\n'
            (home / "config.toml").write_text(config)
            (home / "config.toml").chmod(0o600)
            report_path = root / "observed-report.json"
            command = [str(binary), "--dir", str(project), "benchmark", str(manifest), "--report", str(report_path)]
            if args.profiles:
                command += ["--profiles", str(args.profiles.resolve(strict=True))]
            elif args.machine_repetitions:
                profiles = root / "machine.toml"
                profiles.write_text(
                    '[machine]\nrepetitions=' + str(args.machine_repetitions) + '\nminimum_correctness=1.0\n'
                    '[[configurations]]\nname="baseline"\nprovider="openai"\nmodel="scripted"\ncontext_window=32768\n'
                    '[[configurations]]\nname="compact-tools"\nprovider="openai"\nmodel="scripted"\ncontext_window=32768\n'
                    '[configurations.assistance]\ncompact_tools=true\ninitial_tool_limit=16\n'
                )
                command += ["--profiles", str(profiles)]
            import os
            environment = dict(os.environ, DIAL_HOME=str(home))
            for name in ("OPENAI_API_KEY", "ANTHROPIC_API_KEY", "GOOGLE_API_KEY"):
                environment.pop(name, None)
            result = subprocess.run(command, env=environment, text=True, capture_output=True, timeout=300)
            if result.returncode:
                raise RuntimeError("benchmark process failed:\n" + result.stdout + result.stderr)
            observed = json.loads(report_path.read_text())
            report = observed.get("machine_benchmark", observed.get("profile_comparison", observed.get("report", observed)))
            reliability = None
            if "configurations" in report:
                from reliability import summarize
                reliability = summarize(observed)
                reports = [item["report"] for item in report["configurations"]]
                baseline = next(item["report"] for item in report["configurations"] if item["configuration"]["name"] == report["baseline_configuration"])
                expected_cases *= report["policy"]["repetitions"]
            else:
                reports = [item["report"] for item in report["profiles"]] if "profiles" in report else [report]
                baseline = next((item["report"] for item in report.get("profiles", []) if item["profile"]["name"] == report.get("baseline_profile")), reports[0])
            if len(baseline["cases"]) != expected_cases or any(not case["passed"] for case in baseline["cases"]):
                raise RuntimeError("baseline deterministic capability checks failed:\n" + json.dumps(observed, indent=2) + "\nBenchmark diagnostics:\n" + result.stderr)
            for profile in reports:
                if len(profile["cases"]) != expected_cases:
                    raise RuntimeError("profile omitted benchmark cases")
                expected_rate = sum(bool(case["passed"]) for case in profile["cases"]) / len(profile["cases"])
                if profile["pass_rate"] != expected_rate:
                    raise RuntimeError("report pass rate disagrees with verified case results")
            if server.errors:
                raise RuntimeError("scripted provider errors: " + repr(server.errors))
            if not server.requests:
                raise RuntimeError("no provider requests were exercised")
            if args.report:
                args.report.resolve().write_text(json.dumps(observed, indent=2) + "\n")
            summary = {"kind": "scripted-harness-regression", "exercised_cases": sum(len(profile["cases"]) for profile in reports), "passed_cases": sum(case["passed"] for profile in reports for case in profile["cases"]), "provider_requests": len(server.requests), "model_quality_evidence": False}
            if reliability is not None:
                summary["repeated_checked_tasks"] = sum(item["checked_tasks"] for item in reliability["configurations"])
                if any(item["observed_all_trials_passed_fraction"] != 1 for item in reliability["configurations"]):
                    raise RuntimeError("scripted repeated outcome checks were not consistently successful")
            print(json.dumps(summary))
    finally:
        server.shutdown()
        server.server_close()
        worker.join(timeout=5)


if __name__ == "__main__":
    main()
