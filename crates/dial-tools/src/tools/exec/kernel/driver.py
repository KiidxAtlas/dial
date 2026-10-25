"""Trusted framed driver; native process mitigation precedes all user code."""
import os
import sys

if sys.platform == "win32":
    import ctypes

    # PROCESS_CHILD_PROCESS_POLICY (13), NoChildProcessCreation (Flags bit 0).
    class _ChildProcessPolicy(ctypes.Structure):
        _fields_ = [("Flags", ctypes.c_uint32)]

    _child_policy = _ChildProcessPolicy(1)
    _set_mitigation = ctypes.WinDLL("kernel32", use_last_error=True).SetProcessMitigationPolicy
    _set_mitigation.argtypes = [ctypes.c_int, ctypes.c_void_p, ctypes.c_size_t]
    _set_mitigation.restype = ctypes.c_int
    if not _set_mitigation(13, ctypes.byref(_child_policy), ctypes.sizeof(_child_policy)):
        raise ctypes.WinError(ctypes.get_last_error())

import ast
import asyncio
import contextlib
import contextvars
import inspect
import io
import json
import signal
import site
import traceback

LIMIT = 20000
MAX_FRAME = 12 * 1024 * 1024
MAX_CALLBACKS = 128
protocol = os.fdopen(os.dup(1), "w", buffering=1)
protocol_input = os.fdopen(os.dup(0), "rb")
# Native writes cannot become protocol frames; Python writes are captured per cell.
os.dup2(2, 1)
sys.stdin = io.StringIO("")
namespace = {"__name__": "__dial_kernel__"}
loop = asyncio.new_event_loop()
asyncio.set_event_loop(loop)
current_operation = contextvars.ContextVar("dial_operation", default=None)
# -I ignores PYTHONPATH, so the controller passes venv site-packages as argv; addsitedir also runs the venv's .pth files.
site_dirs = sys.argv[1:]
del sys.argv[1:]
for directory in site_dirs:
    site.addsitedir(directory)


reading_reply = False
interrupt_deferred = False


def interrupt(signum, frame):
    global interrupt_deferred
    # Only an active cell unwinds; a SIGINT between cells must not truncate the frame reader.
    operation = current_operation.get()
    if operation is not None and operation["active"]:
        if reading_reply:
            # Raising mid-readline would drop a partial reply frame and desynchronize the protocol.
            interrupt_deferred = True
            return
        raise KeyboardInterrupt


signal.signal(signal.SIGINT, interrupt)
callback_sequence = 0


class Capture(io.TextIOBase):
    def __init__(self):
        self.parts = []
        self.size = 0
        self.dropped = 0
        self.original_bytes = 0
        self.retained_bytes = 0

    def writable(self):
        return True

    def write(self, text):
        take = min(len(text), max(0, LIMIT - self.size))
        self.original_bytes += len(text.encode("utf-8", errors="replace"))
        self.retained_bytes += len(text[:take].encode("utf-8", errors="replace"))
        if take:
            self.parts.append(text[:take])
        self.size += take
        self.dropped += len(text) - take
        return len(text)

    def value(self):
        return "".join(self.parts) + (f"\n[truncated {self.dropped} characters]" if self.dropped else "")


class ToolError(RuntimeError):
    def __init__(self, message, result=None):
        super().__init__(message)
        self.result = result


def read_frame():
    line = protocol_input.readline(MAX_FRAME + 1)
    if not line:
        raise EOFError("controller closed the kernel protocol")
    # Rust writes up to MAX_FRAME payload bytes plus the newline.
    if len(line) > MAX_FRAME + 1 or not line.endswith(b"\n"):
        raise ToolError("controller frame exceeds the kernel limit")
    value = json.loads(line)
    if not isinstance(value, dict):
        raise ToolError("controller frame must be an object")
    return value


def _wire_safe(value):
    # serde_json rejects lone surrogate escapes; replace them so user text cannot kill the session.
    if isinstance(value, str):
        return value.encode("utf-8", "backslashreplace").decode("utf-8")
    if isinstance(value, dict):
        return {_wire_safe(key): _wire_safe(item) for key, item in value.items()}
    if isinstance(value, (list, tuple)):
        return [_wire_safe(item) for item in value]
    return value


def send_frame(value):
    frame = json.dumps(_wire_safe(value), ensure_ascii=True, allow_nan=False)
    # Rust reads the newline as part of the frame, so the payload limit excludes it.
    if len(frame) + 1 > MAX_FRAME:
        raise ToolError("kernel frame exceeds the protocol limit")
    protocol.write(frame + "\n")


def callback(kind, **fields):
    global callback_sequence, reading_reply, interrupt_deferred
    operation = current_operation.get()
    if operation is None or not operation["active"]:
        raise ToolError("tool callback belongs to an expired cell")
    operation["callbacks"] += 1
    if operation["callbacks"] > MAX_CALLBACKS:
        raise ToolError("cell callback limit exceeded")
    callback_sequence += 1
    callback_id = callback_sequence
    reading_reply = True
    try:
        send_frame({"callback": kind, "callback_id": callback_id,
                    "operation_id": operation["id"], **fields})
        reply = read_frame()
    finally:
        reading_reply = False
    if interrupt_deferred:
        # Raised only after the reply frame is fully consumed, so the protocol stays in sync.
        interrupt_deferred = False
        raise KeyboardInterrupt
    if reply.get("callback_id") != callback_id or reply.get("operation_id") != operation["id"]:
        raise ToolError("controller callback response identity mismatch")
    if "error" in reply:
        raise ToolError(str(reply["error"]))
    result = reply.get("result")
    if not isinstance(result, dict) or not isinstance(result.get("is_error"), bool):
        raise ToolError("malformed controller tool result")
    if result["is_error"]:
        raise ToolError(result.get("text", "tool call denied or failed"), result)
    return result


async def __dial_prelude__(name, parameters):
    return callback("prelude", name=name, parameters=parameters)["data"]


def __dial_display__(value):
    callback("display", text=value if isinstance(value, str) else repr(value))


namespace["__dial_prelude__"] = __dial_prelude__
namespace["__dial_display__"] = __dial_display__


class ToolProxy:
    @property
    def schemas(self):
        result = callback("schemas")
        schemas = result.get("data")
        if not isinstance(schemas, list):
            raise ToolError("controller did not return current tool schemas", result)
        return schemas

    def call(self, name, args=None):
        if not isinstance(name, str) or not name or len(name) > 256:
            raise ToolError("tool name must be a nonempty string of at most 256 characters")
        if args is None:
            args = {}
        if not isinstance(args, dict):
            raise ToolError("tool arguments must be an object")
        return callback("tool", name=name, arguments=args)

    def __getattr__(self, name):
        if name.startswith("_"):
            raise AttributeError(name)
        return lambda **kwargs: self.call(name, kwargs)


tool = ToolProxy()


def execute(tree, mode):
    compiled = compile(tree, "<dial-kernel>", mode, flags=ast.PyCF_ALLOW_TOP_LEVEL_AWAIT)
    value = eval(compiled, namespace)
    if compiled.co_flags & inspect.CO_COROUTINE:
        # This driver owns one persistent loop and never calls asyncio.run inside it.
        value = loop.run_until_complete(value)
    return value


while True:
    try:
        request = read_frame()
    except EOFError:
        break
    except (ToolError, ValueError) as error:
        # A malformed frame has no trustworthy id; reply so the controller fails cleanly instead of the driver crashing.
        send_frame({"id": None, "error": str(error)})
        continue
    request_id = request.get("id")
    interrupt_deferred = False
    operation = {"id": request_id, "active": True, "callbacks": 0}
    token = current_operation.set(operation)
    stdout, stderr = Capture(), Capture()
    result = {"stdout": "", "stderr": "", "value": None, "error": None}
    # Reinstall the current bridge every cell, but do not otherwise replace user state.
    namespace["tool"] = tool
    try:
        with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
            tree = ast.parse(request["code"], filename="<dial-kernel>", mode="exec")
            tail = tree.body.pop() if tree.body and isinstance(tree.body[-1], ast.Expr) else None
            execute(tree, "exec")
            if tail is not None:
                value = execute(ast.Expression(tail.value), "eval")
                if value is not None:
                    namespace["_"] = value
                    result["value"] = repr(value)[:LIMIT]
    except BaseException as error:
        result["error"] = traceback.format_exc()[-LIMIT:]
        if isinstance(error, ToolError) and error.result is not None:
            result["tool_error"] = error.result
    finally:
        # Detached asyncio tasks keep this token, so subsequent cells cannot authorize their callbacks.
        operation["active"] = False
        current_operation.reset(token)
    result["stdout"], result["stderr"] = stdout.value(), stderr.value()
    # Counts redirected text streams, not native writes or bounded repr/traceback formatting.
    result["output_integrity"] = {
        "original_bytes": stdout.original_bytes + stderr.original_bytes,
        "retained_bytes": stdout.retained_bytes + stderr.retained_bytes,
    }
    try:
        send_frame({"id": request_id, "result": result})
    except (ToolError, ValueError):
        # Controller-supplied tool_error data may exceed the frame limit or contain NaN; keep the cell outcome instead of crashing.
        result.pop("tool_error", None)
        send_frame({"id": request_id, "result": result})
