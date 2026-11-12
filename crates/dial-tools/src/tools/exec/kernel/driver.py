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
import re
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
        return "".join(self.parts) + (f"\n[truncated {self.dropped} characters; the start was kept. Keep large results in a variable and print slices or summaries]" if self.dropped else "")


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
namespace["__dial_check__"] = lambda name, passed: callback("check", name=name, passed=passed)


class _ToolResult(dict):
    """A synchronous callback receipt; awaiting it never dispatches the tool again."""

    def __await__(self):
        yield from ()
        return self


class _BatchResult(list):
    """Ordered `tool.batch` receipts; awaiting them never dispatches the tools again."""

    def __await__(self):
        yield from ()
        return self


# Reserved controller name (BATCH_CALLBACK in eval.rs); no provider tool can carry it.
BATCH = "dial.batch"


def _batch_call(index, entry):
    if not isinstance(entry, dict):
        raise ToolError(f"tool.batch entry {index}: expected a {{name, args}} object")
    name, args = entry.get("name"), entry.get("args")
    if not isinstance(name, str) or not name or len(name) > 256:
        raise ToolError(f"tool.batch entry {index}: tool name must be a nonempty string of at most 256 characters")
    if args is None:
        args = {}
    if not isinstance(args, dict):
        raise ToolError(f"tool.batch entry {index}: tool arguments must be an object")
    return {"name": name, "arguments": args}


class ToolProxy:
    def batch(self, entries):
        """Run `[{"name": ..., "args": {...}}, ...]` as one controller batch, in order.

        Read-only entries may run together; anything else runs one at a time. Each entry counts
        against the cell's callback limit. A controller without batch support refuses the reserved
        name and the entries run serially, each failure kept as its own error result.
        """
        if not isinstance(entries, (list, tuple)) or len(entries) > MAX_CALLBACKS:
            raise ToolError(f"tool.batch expects a list of at most {MAX_CALLBACKS} {{name, args}} entries")
        calls = [_batch_call(index, entry) for index, entry in enumerate(entries)]
        if not calls:
            return _BatchResult()
        operation = current_operation.get()
        if operation is not None:
            operation["callbacks"] += len(calls) - 1
        try:
            result = callback("tool", name=BATCH, arguments={"calls": calls})
        except ToolError as error:
            if not (error.result and isinstance(error.result.get("data"), dict)
                    and error.result["data"].get("code") == "unsupported_batch"
                    and error.result["data"].get("executed") is False):
                raise
            if operation is not None:
                operation["callbacks"] -= len(calls)
            receipts = _BatchResult()
            for call in calls:
                try:
                    receipts.append(self.call(call["name"], call["arguments"]))
                except ToolError as failure:
                    receipts.append(_ToolResult(failure.result if failure.result is not None
                                                else {"is_error": True, "text": str(failure), "data": None}))
            return receipts
        data = result.get("data")
        if (not isinstance(data, list) or len(data) != len(calls)
                or any(not isinstance(item, dict) or not isinstance(item.get("is_error"), bool) for item in data)):
            raise ToolError("controller returned a malformed tool.batch result", result)
        return _BatchResult(_ToolResult(item) if isinstance(item, dict) else item for item in data)

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
        return _ToolResult(callback("tool", name=name, arguments=args))

    def __getattr__(self, name):
        if name.startswith("_"):
            raise AttributeError(name)
        def invoke(arguments=None, /, **kwargs):
            if arguments is not None and kwargs:
                raise ToolError("pass tool arguments as an object or keyword arguments, not both")
            return self.call(name, kwargs if arguments is None else arguments)

        return invoke


tool = ToolProxy()


def helper_hint(error):
    """Name the `dial.` form when a bare portable-helper name such as readText is undefined."""
    if not isinstance(error, NameError):
        return ""
    match = re.search(r"name '([A-Za-z_][A-Za-z0-9_]*)' is not defined", str(error))
    helpers = namespace.get("dial")
    if not match or helpers is None or not hasattr(helpers, match.group(1)):
        return ""
    return "\n[dial] Portable helpers are methods of `dial`, not globals: use dial.%s(...)." % match.group(1)


_HARNESS_FILE = (lambda: None).__code__.co_filename


def user_traceback(error):
    """Formats a cell traceback without the driver's own frames or the asyncio frames that precede user code."""
    pending = [traceback.TracebackException(type(error), error, error.__traceback__)]
    seen = set()
    while pending:
        item = pending.pop()
        if id(item) in seen:
            continue
        seen.add(id(item))
        frames = list(item.stack)
        first_user = next((i for i, frame in enumerate(frames) if frame.filename == "<dial-kernel>"), 0)
        item.stack = traceback.StackSummary.from_list(
            [frame for i, frame in enumerate(frames) if frame.filename != _HARNESS_FILE and i >= first_user]
        )
        pending.extend(link for link in (item.__cause__, item.__context__) if link is not None)
        pending.extend(getattr(item, "exceptions", None) or [])
    return "".join(item.format())


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
        result["error"] = user_traceback(error)[-LIMIT:]
        hint = helper_hint(error)
        if hint:
            result["error"] += hint
        if getattr(error, "result", None) is not None:
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
