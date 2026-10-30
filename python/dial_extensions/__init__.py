"""Stdlib-only SDK for trusted Dial extensions (Python 3.11+).

The managed host selects framed requests or optional session workers. Use explicit
state for data that must survive restarts. Python is NOT sandboxed; host tools and
sensitive actions retain their normal approval and execution boundaries.
"""
from __future__ import annotations

import asyncio
from contextlib import redirect_stdout
from dataclasses import dataclass
import inspect
import json
import os
from pathlib import Path
import re
import sys
import tempfile
import traceback
from typing import Any, Callable

from .host import Host, HostError, _binding
from .contributions import Contributions, frozen
from .scheduler import Scheduler

__all__ = ["Extension", "Reply", "ToolResult", "JsonState", "RpcError", "Host", "HostError"]


class RpcError(Exception):
    """An explicit JSON-RPC failure; no exception details are sent by default."""

    def __init__(self, code: int, message: str):
        super().__init__(message)
        self.code = code


class Reply(dict):
    """Fluent protocol reply. Helpers only describe changes; the host validates them.

    Example: ``Reply().text('Ready').ui('status', ['Ready'])``.
    ``rewrite`` is for blocking hooks, not observational events.
    """

    def text(self, text: str) -> Reply:
        self["text"] = text
        return self

    def panel(self, title: str, elements: list[dict], generation: int = 0) -> Reply:
        self["panel"] = dict(title=title, elements=elements, generation=generation)
        return self

    def rewrite(self, payload: dict | None = None, **fields: Any) -> Reply:
        self["rewrite"] = dict(payload or {}, **fields)
        return self

    def block(self, reason: str) -> Reply:
        return self.rewrite(block=reason)

    def action(self, action_type: str, **fields: Any) -> Reply:
        self.setdefault("actions", []).append(dict(type=action_type, **fields))
        return self

    def ui(self, slot: str, lines: list[str]) -> Reply:
        if slot not in ("status", "header", "footer", "title"):
            raise ValueError("unknown UI slot")
        return self.action("ui", slot=slot, lines=lines)

    def send_user_message(self, text: str) -> Reply:
        return self.action("send_user_message", text=text)

    def append_entry(self, kind: str, text: str, renderer: dict | None = None) -> Reply:
        fields = dict(kind=kind, text=text)
        if renderer is not None:
            fields["renderer"] = renderer
        return self.action("append_entry", **fields)

    def set_model(self, model: str, effort: str | None = None) -> Reply:
        fields = dict(model=model)
        if effort is not None:
            fields["effort"] = effort
        return self.action("set_model", **fields)

    def set_thinking(self, effort: str) -> Reply:
        return self.action("set_thinking", effort=effort)

    def register_provider(self, name: str, config: dict) -> Reply:
        return self.action("register_provider", name=name, config=config)

    def unregister_provider(self, name: str) -> Reply:
        return self.action("unregister_provider", name=name)


@dataclass(frozen=True)
class ToolResult:
    text: str
    is_error: bool = False
    data: Any = None
    renderer: str | None = None


class JsonState:
    """Explicit JSON files in the host-provided state directory.

    Writes are atomic replacements, not cross-process transactions. Concurrent
    read/modify/write users must supply their own coordination. Corrupt JSON is
    an error, never silently replaced with defaults. No state directory fallback
    writes into the workspace.
    """

    def __init__(self, directory: str | Path | None = None):
        directory = directory or os.environ.get("DIAL_EXTENSION_STATE_DIR")
        if not directory:
            raise RuntimeError("DIAL_EXTENSION_STATE_DIR is not set")
        self.directory = Path(directory)

    def _path(self, name: str) -> Path:
        if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_-]{0,63}", name):
            raise ValueError("state name must be a short alphanumeric identifier")
        return self.directory / (name + ".json")

    def load(self, name: str = "state", default: Any = None) -> Any:
        try:
            return json.loads(self._path(name).read_text(encoding="utf-8"))
        except FileNotFoundError:
            return default

    def save(self, value: Any, name: str = "state") -> None:
        path = self._path(name)
        data = json.dumps(value, ensure_ascii=False, allow_nan=False) + "\n"
        self.directory.mkdir(parents=True, exist_ok=True)
        temporary = None
        try:
            with tempfile.NamedTemporaryFile(mode="w", encoding="utf-8", dir=self.directory,
                                             prefix=".state-", delete=False) as stream:
                temporary = Path(stream.name)
                stream.write(data)
                stream.flush()
                os.fsync(stream.fileno())
            os.replace(temporary, path)
        finally:
            if temporary is not None:
                temporary.unlink(missing_ok=True)

    def delete(self, name: str = "state") -> None:
        self._path(name).unlink(missing_ok=True)


def _object(value: Any, label: str) -> dict:
    if not isinstance(value, dict):
        raise RpcError(-32602, label + " must be an object")
    return value


def _string(params: dict, key: str) -> str:
    value = params.get(key)
    if not isinstance(value, str):
        raise RpcError(-32602, key + " must be a string")
    return value


def _validate_extension_name(name: str) -> None:
    if not isinstance(name, str) or not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._-]{0,127}", name):
        raise ValueError("invalid extension name: use 1–128 ASCII letters/digits/._-, starting with a letter or digit")


def _tool_namespace(owner: str, local: str) -> str:
    _validate_extension_name(owner)
    escaped = owner.replace("_", "_u").replace(".", "_d")
    namespace = "ext__" + escaped + "__" + local
    if len(namespace) > 128:
        raise ValueError("namespaced tool name exceeds 128 characters; shorten the extension name or local tool name")
    return namespace


class Extension(Contributions, Scheduler):
    """Register decorators and call ``serve()`` under a main guard.

    Callback signatures: command(args), on(payload), panel_close(outcome,
    generation), autocomplete(query), tool(arguments, workspace). Any callback
    may be ``async def``. Return a Reply/dict, text string, or None. Tools return
    ToolResult or a text string. Declare action capabilities explicitly in the
    constructor (e.g. ``capabilities=['ui.status', 'actions.set_model']``).
    """

    def __init__(self, name: str, *, capabilities: list[str] | tuple[str, ...] = ()):
        _validate_extension_name(name)
        self._owner = os.environ.get("DIAL_EXTENSION_NAME", name)
        _validate_extension_name(self._owner)
        self.name = name
        self.capabilities = set(capabilities)
        self._commands: dict[str, Callable] = {}
        self._events: dict[str, Callable] = {}
        self._panels: dict[str, Callable] = {}
        self._tools: dict[str, tuple[dict, Callable]] = {}
        self._autocomplete: Callable | None = None
        self.host = Host()
        self._init_contributions()
        self._init_scheduler()

    @property
    def root(self) -> Path:
        value = os.environ.get("DIAL_EXTENSION_ROOT")
        if not value:
            raise RuntimeError("DIAL_EXTENSION_ROOT is not set")
        return Path(value)

    @property
    def settings(self) -> dict:
        value = json.loads(os.environ.get("DIAL_EXTENSION_SETTINGS", "{}"))
        if not isinstance(value, dict):
            raise ValueError("DIAL_EXTENSION_SETTINGS must be a JSON object")
        return value

    @property
    def state(self) -> JsonState:
        return JsonState()

    @staticmethod
    def _register(registry: dict, name: str):
        def decorate(callback):
            if name in registry:
                raise ValueError("duplicate registration: " + name)
            registry[name] = callback
            return callback
        return decorate

    def command(self, name: str, *, description: str | None = None,
                parameters: dict | None = None, keybinding: str | None = None):
        if not name or len(name) > 48 or name.startswith("/") or any(
                not 33 <= ord(char) <= 126 for char in name):
            raise ValueError("invalid command name")
        if description is not None or parameters is not None or keybinding is not None:
            self._contribute("commands", dict(name=name, description=description or name,
                parameters=parameters or {"type": "object"}, keybinding=keybinding))
        return self._register(self._commands, name)

    def on(self, event: str):
        """Subscribe to a blocking hook or observational event by protocol name."""
        if not event:
            raise ValueError("event must not be empty")
        return self._register(self._events, event)

    def panel_close(self, command: str):
        return self._register(self._panels, command)

    def autocomplete(self, callback):
        """Return a list of {value, label?, description?} suggestion objects."""
        if self._autocomplete is not None:
            raise ValueError("duplicate autocomplete registration")
        self._autocomplete = callback
        self.capabilities.add("autocomplete")
        return callback

    def tool(self, name: str, description: str, parameters: dict, *, revision: int = 1):
        if not re.fullmatch(r"[A-Za-z0-9_-]{1,48}", name):
            raise ValueError("invalid tool name")
        _tool_namespace(self._owner, name)
        if not isinstance(description, str) or not description.strip():
            raise ValueError("tool description must not be empty")
        if not isinstance(parameters, dict) or parameters.get("type") != "object":
            raise ValueError("tool parameters must be an object JSON Schema")
        if type(revision) is not int or revision < 1:
            raise ValueError("tool revision must be a positive integer")
        # Freeze the declaration independently of the caller's mutable schema.
        declaration = json.loads(json.dumps(dict(name=name, description=description,
                                                 parameters=parameters, revision=revision),
                                           allow_nan=False))
        def decorate(callback):
            if name in self._tools:
                raise ValueError("duplicate tool registration: " + name)
            if len(self._tools) >= 64:
                raise ValueError("at most 64 tools may be registered")
            self._tools[name] = (declaration, callback)
            return callback
        return decorate

    @staticmethod
    def _invoke(callback: Callable, *args):
        value = callback(*args)
        return asyncio.run(value) if inspect.isawaitable(value) else value

    @staticmethod
    def _reply(value):
        if value is None:
            return {}
        if isinstance(value, str):
            return {"text": value}
        if isinstance(value, dict):
            return value
        raise TypeError("callback must return Reply, dict, string, or None")

    @staticmethod
    def _lookup(registry: dict, name: str):
        if name not in registry:
            raise RpcError(-32601, "No handler registered for " + name)
        return registry[name]

    def dispatch(self, method: str, params: dict) -> dict:
        """Dispatch decoded protocol data (useful for unit tests; does not print)."""
        _object(params, "params")
        if method == "initialize":
            return dict(protocolVersion="2", apiVersion=1, capabilities=sorted(self.capabilities),
                        commands=list(self._commands), events=list(self._events),
                        tools=[item[0] for item in self._tools.values()],
                        contributions=frozen(self._contributions),
                        subscriptions=dict(timers=len(self._timers), watches=len(self._watches)))
        if method == "command":
            callback = self._lookup(self._commands, _string(params, "name"))
            value = self._invoke(callback, _string(params, "args"))
        elif method == "command.panel":
            callback = self._lookup(self._panels, _string(params, "name"))
            generation = params.get("generation")
            if type(generation) is not int or generation < 0:
                raise RpcError(-32602, "generation must be a nonnegative integer")
            value = self._invoke(callback, _object(params.get("outcome"), "outcome"), generation)
        elif method == "event":
            callback = self._lookup(self._events, _string(params, "event"))
            value = self._invoke(callback, _object(params.get("payload"), "payload"))
        elif method == "autocomplete":
            if self._autocomplete is None:
                raise RpcError(-32601, "No autocomplete handler registered")
            value = self._invoke(self._autocomplete, _string(params, "query"))
            return value if isinstance(value, dict) else {"items": value}
        elif method == "tool.call":
            _, callback = self._lookup(self._tools, _string(params, "name"))
            value = self._invoke(callback, _object(params.get("arguments"), "arguments"),
                                 _string(params, "workspace"))
            if isinstance(value, str):
                value = ToolResult(value)
            if not isinstance(value, ToolResult) or not isinstance(value.text, str) or type(value.is_error) is not bool:
                raise TypeError("tools must return a string or ToolResult")
            reply = dict(text=value.text, is_error=value.is_error)
            if value.data is not None:
                reply["data"] = value.data
            if value.renderer is not None:
                reply["renderer"] = value.renderer
            return reply
        elif method == "view.event":
            callback = self._lookup(self._view_handlers, _string(params, "name"))
            value = self._invoke(callback, _object(params.get("event"), "event"))
        elif method == "context.get":
            callback = self._lookup(self._context_handlers, _string(params, "name"))
            value = self._invoke(callback, params.get("query", ""))
        elif method == "poll":
            return self._poll(params)
        elif method == "shutdown":
            for callback in reversed(self._shutdown_handlers):
                self._invoke(callback)
            return {"shutdown": True}
        else:
            raise RpcError(-32601, "Method not found")
        return self._reply(value)

    def _serve_one(self, raw, source, output, framed):
        request_id = None
        notification = False
        token = None
        try:
            try:
                request = json.loads(raw, parse_constant=lambda _: (_ for _ in ()).throw(ValueError()))
            except (ValueError, UnicodeError):
                raise RpcError(-32700, "Parse error") from None
            if (not isinstance(request, dict) or request.get("jsonrpc") != "2.0"
                    or not isinstance(request.get("method"), str)
                    or ("id" in request and (type(request["id"]) not in (str, int, float, type(None))))):
                raise RpcError(-32600, "Invalid Request")
            request_id = request.get("id")
            notification = "id" not in request
            if framed:
                token = _binding.set((source, output, request_id))
            with redirect_stdout(sys.stderr):
                result = self.dispatch(request["method"], request.get("params", {}))
                response = dict(result=result)
                # Validate before writing: failures never emit a partial frame.
                json.dumps(response, ensure_ascii=False, allow_nan=False)
        except RpcError as error:
            response = dict(error=dict(code=error.code, message=str(error)))
        except Exception:
            traceback.print_exc(file=sys.stderr)
            response = dict(error=dict(code=-32603, message="Internal error"))
        finally:
            if token is not None:
                _binding.reset(token)
        if not notification:
            envelope = (dict(result=dict(response=response)) if framed else response)
            output.write(json.dumps(dict(jsonrpc="2.0", id=request_id, **envelope),
                                    ensure_ascii=False, allow_nan=False) + "\n")
            output.flush()
        return isinstance(request if 'request' in locals() else None, dict) and request.get("method") == "shutdown"

    def serve(self, *, stdin=None, stdout=None) -> None:
        """Serve legacy EOF requests, or managed framed requests/session workers.

        The host selects framing. Worker crashes are not silently retried; the
        owning session controls restart. All callback prints go to stderr.
        """
        source = stdin if stdin is not None else sys.stdin
        output = stdout if stdout is not None else sys.__stdout__
        runtime = os.environ.get("DIAL_EXTENSION_RUNTIME")
        if runtime not in ("request", "worker"):
            self._serve_one(source.read(), source, output, False)
            return
        while True:
            raw = source.readline(1024 * 1024 + 2)
            if not raw:
                return
            if len(raw.encode("utf-8")) > 1024 * 1024 or not raw.endswith("\n"):
                raise ValueError("extension request frame missing newline or exceeds 1 MiB")
            shutdown = self._serve_one(raw, source, output, True)
            if shutdown or runtime == "request":
                return
