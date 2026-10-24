#!/usr/bin/env python3
"""Deterministic real-TUI directory capability regression (local scripted provider).

Build Dial separately, then run:
  python3 examples/benchmarks/directory_access_smoke.py --dial target/debug/dial
--hold pauses at the first approval popup and prints an exact tmux attach command;
press Enter in this script's terminal to resume the automated yes/no checks.
Captures are terminal text, not pixel screenshots or model-quality evidence.
"""
import argparse
import http.server
import json
import os
import pathlib
import shlex
import tempfile
import threading

from interactive_smoke import Handler as BaseHandler, Terminal, fixture


class Provider(http.server.ThreadingHTTPServer):
    daemon_threads = True

    def __init__(self):
        super().__init__(("127.0.0.1", 0), Handler)
        self.plans = {}
        self.errors = []
        self.requests = 0
        self.checks = 0
        self.results = {}


class Handler(BaseHandler):
    def do_GET(self):
        self.respond({"data": [{"id": "mock-main"}]})

    def do_POST(self):
        try:
            request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            messages = request["messages"]
            self.server.requests += 1
            start = next(i for i in range(len(messages) - 1, -1, -1)
                         if messages[i]["role"] == "user"
                         and messages[i].get("content") in self.server.plans)
            case = messages[start]["content"]
            tail = messages[start + 1:]
            calls = [call for message in tail for call in message.get("tool_calls", [])
                     if call["function"]["name"] != "tool_search"]
            tools = {tool["function"]["name"] for tool in request.get("tools", [])}
            plan = self.server.plans[case]
            if len(calls) < len(plan):
                name, arguments, _ = plan[len(calls)]
                message = self.call(self.server.requests, name, arguments) if name in tools else self.call(
                    self.server.requests, "tool_search", {"query": name})
            else:
                results = {message["tool_call_id"]: json.dumps(message.get("content", ""))
                           for message in tail if message["role"] == "tool"}
                observed = []
                for call, (name, arguments, expected) in zip(calls, plan):
                    assert call["function"]["name"] == name, (case, call)
                    result = results[call["id"]]
                    assert expected.lower() in result.lower(), f"{case} {name}: expected {expected!r}, got {result}"
                    if name == "read" and expected in ("outside the workspace", "protected"):
                        assert "DIRECTORY-SEED-" not in result, f"denied read leaked fixture data: {result}"
                    observed.append({"tool": name, "arguments": arguments, "result": result})
                self.server.results[case] = observed
                self.server.checks += len(plan)
                message = {"role": "assistant", "content": case + "-VERIFIED."}
            self.respond({"id": "directory-" + str(self.server.requests), "object": "chat.completion",
                          "choices": [{"index": 0, "message": message,
                                       "finish_reason": "tool_calls" if message.get("tool_calls") else "stop"}],
                          "usage": {"prompt_tokens": 128, "completion_tokens": 32}})
        except Exception as error:
            self.server.errors.append(repr(error))
            self.respond({"error": str(error)}, 500)


def request(path, sensitive=False):
    return ("request_directory", {"path": str(path), "reason": "Test only this isolated temporary fixture",
                                  "sensitive": sensitive}, "Added")


def popup(terminal, sensitive=False):
    text = terminal.wait("Allow request_sensitive_directory?" if sensitive else "Allow request_directory?")
    assert "Yes" in text and "No" in text, text
    assert "Allow for this session" not in text and "Allow for this project" not in text, text
    if sensitive:
        normalized = " ".join(text.replace("│", " ").split())
        assert "native file tools" in normalized and "Shell commands remain blocked" in normalized, text
    return text


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--dial", type=pathlib.Path, default=pathlib.Path("target/debug/dial"))
    parser.add_argument("--captures", type=pathlib.Path)
    parser.add_argument("--hold", action="store_true")
    args = parser.parse_args()
    binary = args.dial.resolve(strict=True)
    server = Provider()
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    terminal = None
    try:
        with tempfile.TemporaryDirectory(prefix="dial-directory-pty-") as directory:
            root = pathlib.Path(directory).resolve()
            project, home = fixture(root, server.server_port)
            config = home / "config.toml"
            config.write_text(config.read_text().replace("max_steps=8", "max_steps=16"))
            normal = root / "added"
            private = home / "fixture-private"
            sibling = home / "fixture-sibling"
            for path in (normal, private, sibling):
                path.mkdir()
                (path / "seed.txt").write_text("DIRECTORY-SEED-" + path.name)
            normal_denied = list(request(normal))
            normal_denied[2] = "directory access denied"
            sensitive_denied = list(request(private, True))
            sensitive_denied[2] = "sensitive directory access denied"
            server.plans = {
                "DIRECTORY-DENY": [tuple(normal_denied),
                    ("read", {"path": str(normal / "seed.txt")}, "outside the workspace")],
                "DIRECTORY-ALLOW": [request(normal),
                    ("read", {"path": str(normal / "seed.txt")}, "DIRECTORY-SEED-added"),
                    ("write", {"path": str(normal / "native.txt"), "content": "native-write"}, ""),
                    ("bash", {"cwd": str(normal), "command": "cat seed.txt; mkdir -p nested; printf shell-write > nested/shell.txt"}, "DIRECTORY-SEED-added")],
                "DIRECTORY-SENSITIVE-DENY": [tuple(sensitive_denied),
                    ("read", {"path": str(private / "seed.txt")}, "protected")],
                "DIRECTORY-SENSITIVE-ALLOW": [request(private, True),
                    ("read", {"path": str(private / "seed.txt")}, "DIRECTORY-SEED-fixture-private"),
                    ("write", {"path": str(private / "native.txt"), "content": "sensitive-native-write"}, ""),
                    ("bash", {"cwd": str(project), "command": "cat " + shlex.quote(str(private / "seed.txt"))}, ""),
                    ("read", {"path": str(sibling / "seed.txt")}, "protected")],
            }
            # Never inherit an optional shared authentication store from the operator.
            shared_auth = os.environ.pop("DIAL_SUBSCRIPTION_AUTH_HOME", None)
            try:
                terminal = Terminal(binary, project, home, args.captures)
            finally:
                if shared_auth is not None:
                    os.environ["DIAL_SUBSCRIPTION_AUTH_HOME"] = shared_auth
            terminal.wait("mock-main")
            for case, answers in (("DIRECTORY-DENY", "n"), ("DIRECTORY-ALLOW", "y"),
                                  ("DIRECTORY-SENSITIVE-DENY", "yn"), ("DIRECTORY-SENSITIVE-ALLOW", "yy")):
                terminal.command(case)  # No /add-dir or other slash command is sent.
                popup(terminal)
                terminal.capture(case.lower() + "-normal-popup")
                if args.hold:
                    print("Attach to the live popup: " + shlex.join(
                        ["tmux", "-L", terminal.socket, "attach-session", "-t", "smoke"]), flush=True)
                    input("Press Enter here to resume automated checks (detach tmux with Ctrl-b d): ")
                    args.hold = False
                terminal.key(answers[0])
                if len(answers) == 2:
                    popup(terminal, sensitive=True)
                    terminal.capture(case.lower() + "-sensitive-popup")
                    terminal.key(answers[1])
                try:
                    terminal.wait(case + "-VERIFIED")
                except AssertionError:
                    print(json.dumps({"provider_errors": server.errors}), flush=True)
                    raise
                terminal.capture(case.lower() + "-completed")
                assert not server.errors, server.errors
            assert (normal / "native.txt").read_text() == "native-write"
            assert (normal / "nested/shell.txt").read_text() == "shell-write"
            assert (private / "native.txt").read_text() == "sensitive-native-write"
            shell = server.results["DIRECTORY-SENSITIVE-ALLOW"][3]["result"]
            assert "DIRECTORY-SEED-fixture-private" not in shell, shell
            assert any(word in shell.lower() for word in ("denied", "not permitted", "blocked", "sandbox")), shell
            assert not (sibling / "native.txt").exists()
            if args.captures:
                (args.captures / "provider-results.json").write_text(json.dumps(server.results, indent=2))
            print(json.dumps({"kind": "real-pty-directory-access-regression", "cases": 4,
                              "normal_popups": 4, "sensitive_popups": 2,
                              "tool_result_checks": server.checks, "chat_requests": server.requests,
                              "typed_add_dir_commands": 0, "pixel_screenshot_verification": False,
                              "model_quality_evidence": False}))
    except Exception:
        print(json.dumps({"provider_errors": server.errors}), flush=True)
        if terminal:
            terminal.capture("failure")
        raise
    finally:
        if terminal:
            terminal.close()
        server.shutdown()
        server.server_close()
        thread.join(timeout=5)


if __name__ == "__main__":
    main()
