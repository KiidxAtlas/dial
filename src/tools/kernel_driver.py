"""Trusted JSON-lines kernel driver. Model code runs only inside the workspace OS sandbox."""
import ast
import contextlib
import io
import json
import os
import sys
import traceback

LIMIT = 20000
protocol = os.fdopen(os.dup(1), "w", buffering=1)
# Native writes and child processes cannot corrupt the JSON protocol.
os.dup2(2, 1)
namespace = {"__name__": "__dial_kernel__"}

class Capture(io.TextIOBase):
    def __init__(self):
        self.parts = []
        self.size = 0
        self.dropped = 0
    def writable(self):
        return True
    def write(self, text):
        take = min(len(text), max(0, LIMIT - self.size))
        if take:
            self.parts.append(text[:take])
        self.size += take
        self.dropped += len(text) - take
        return len(text)
    def value(self):
        return "".join(self.parts) + (f"\n[truncated {self.dropped} characters]" if self.dropped else "")

for line in sys.stdin:
    request = json.loads(line)
    stdout, stderr = Capture(), Capture()
    result = {"stdout": "", "stderr": "", "value": None, "error": None}
    try:
        with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
            tree = ast.parse(request["code"], filename="<dial-kernel>", mode="exec")
            tail = tree.body.pop() if tree.body and isinstance(tree.body[-1], ast.Expr) else None
            exec(compile(tree, "<dial-kernel>", "exec"), namespace)
            if tail is not None:
                value = eval(compile(ast.Expression(tail.value), "<dial-kernel>", "eval"), namespace)
                if value is not None:
                    namespace["_"] = value
                    result["value"] = repr(value)[:LIMIT]
    except BaseException:
        result["error"] = traceback.format_exc()[-LIMIT:]
    result["stdout"], result["stderr"] = stdout.value(), stderr.value()
    protocol.write(json.dumps({"id": request["id"], "result": result}) + "\n")
