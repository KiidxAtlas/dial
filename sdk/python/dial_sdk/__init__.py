"""Write Dial plugins in Python.

A plugin is one Python file that declares tools and lifecycle hooks with decorators. Dial runs
it through its existing extension points, so a plugin needs nothing beyond the standard library:

* each tool becomes an operator-configured tool (``[tools.<name>]``): Dial sends the call's
  arguments as one JSON object on stdin, and the tool's return value is the result;
* each hook becomes a ``[[hooks]]`` entry: Dial sends the event as JSON on stdin, and the
  handler's return value is the hook's answer.

``python my_plugin.py config`` prints the TOML to add to Dial's user configuration
(``~/.dial/config.toml``) or project state. Like every configured tool and hook, a
plugin is trusted code that runs unsandboxed with your privileges.

Example::

    from dial_sdk import Annotated, Block, Plugin, ToolError

    plugin = Plugin("notes")

    @plugin.tool
    def word_count(text: Annotated[str, "Text to count"], unique: bool = False) -> int:
        \"\"\"Count the words in a text.\"\"\"
        words = text.split()
        return len(set(words)) if unique else len(words)

    @plugin.hook("pre_tool_use", matcher="write|edit")
    def no_env_files(event):
        if event.input.get("path", "").endswith(".env"):
            return Block("editing .env files is not allowed")

    if __name__ == "__main__":
        plugin.main()
"""

from __future__ import annotations

import inspect
import json
import os
import re
import sys
import traceback
import typing
from dataclasses import dataclass, field
from typing import Annotated, Any, Callable, Dict, List, Mapping, Optional

__all__ = [
    "Annotated",
    "Block",
    "EVENTS",
    "Event",
    "Plugin",
    "Reply",
    "ToolError",
    "schema_for",
]

__version__ = "0.1.0"

#: Lifecycle events Dial runs hooks for (``config::hooks::EVENTS``), with Dial's aliases.
EVENTS = (
    "session_start",
    "session_end",
    "user_prompt_submit",
    "pre_tool_use",
    "post_tool_use",
    "stop",
    "before_compact",
    "before_subagent_spawn",
    "before_provider_request",
    "context",
    "assistant_message",
    "after_provider_response",
    "session_before_branch",
    "session_before_switch",
)
_ALIASES = {
    "tool_call": "pre_tool_use",
    "tool_result": "post_tool_use",
    "message_end": "assistant_message",
    "provider_response": "after_provider_response",
    "before_branch": "session_before_branch",
    "before_switch": "session_before_switch",
}

#: Exit status that makes Dial treat a hook's answer as a block (stderr is the reason).
BLOCK_EXIT = 2

_NAME = re.compile(r"^[A-Za-z][A-Za-z0-9_-]{0,63}$")


class ToolError(Exception):
    """Raise from a tool to fail the call with this message (shown to the model)."""


@dataclass
class Block:
    """Return from a hook to refuse the action: a tool call (``pre_tool_use``), a prompt
    (``user_prompt_submit``), a provider request, or to keep the agent going (``stop``)."""

    reason: str


@dataclass
class Reply:
    """A structured hook answer. Set only what the event supports:

    * ``input``: replacement tool arguments (``pre_tool_use``);
    * ``content`` / ``is_error``: replacement tool result (``post_tool_use``);
    * ``additional_context``: a note for the next model request;
    * ``prompt``: replacement user message (``user_prompt_submit``);
    * ``payload``: replacement request body (``before_provider_request``);
    * ``messages``: replacement message list for one request (``context``);
    * ``notice``: text shown to the user.
    """

    input: Optional[Dict[str, Any]] = None
    content: Optional[str] = None
    is_error: Optional[bool] = None
    additional_context: Optional[str] = None
    prompt: Optional[str] = None
    payload: Optional[Any] = None
    messages: Optional[List[Any]] = None
    notice: Optional[str] = None

    def to_json(self) -> Dict[str, Any]:
        return {key: value for key, value in vars(self).items() if value is not None}


class Event(dict):
    """A hook event: the JSON object Dial sent, with attribute access to common fields."""

    @property
    def name(self) -> str:
        return self.get("event", "")

    @property
    def tool(self) -> str:
        return self.get("tool", "")

    @property
    def input(self) -> Dict[str, Any]:
        value = self.get("input")
        return value if isinstance(value, dict) else {}

    @property
    def output(self) -> str:
        return self.get("output", "")

    @property
    def cwd(self) -> str:
        return self.get("cwd") or os.environ.get("DIAL_PROJECT_DIR", "")


# --- Parameter schemas ---------------------------------------------------------------------

_SCALARS = {str: "string", int: "integer", float: "number", bool: "boolean"}


def _schema_of(annotation: Any) -> Dict[str, Any]:
    """The JSON Schema of one parameter annotation."""
    if annotation is inspect.Parameter.empty or annotation is Any:
        return {}
    origin = typing.get_origin(annotation)
    args = typing.get_args(annotation)
    if origin is Annotated:
        schema = _schema_of(args[0])
        for extra in args[1:]:
            if isinstance(extra, str):
                schema["description"] = extra
            elif isinstance(extra, Mapping):
                schema.update(extra)
        return schema
    if annotation in _SCALARS:
        return {"type": _SCALARS[annotation]}
    if origin is typing.Literal:
        return {"enum": list(args)}
    if origin is typing.Union or type(annotation).__name__ == "UnionType":
        members = [member for member in args if member is not type(None)]
        if len(members) == 1:
            return _schema_of(members[0])
        return {"anyOf": [_schema_of(member) for member in members]}
    if annotation is list or origin in (list, tuple, set, frozenset):
        schema: Dict[str, Any] = {"type": "array"}
        if args and args[-1] is not Ellipsis:
            schema["items"] = _schema_of(args[0])
        return schema
    if annotation is dict or origin is dict:
        return {"type": "object"}
    raise TypeError(f"unsupported parameter type {annotation!r}")


def _optional(annotation: Any) -> bool:
    if typing.get_origin(annotation) is Annotated:
        annotation = typing.get_args(annotation)[0]
    origin = typing.get_origin(annotation)
    return (origin is typing.Union or type(annotation).__name__ == "UnionType") and type(
        None
    ) in typing.get_args(annotation)


def schema_for(function: Callable[..., Any]) -> Dict[str, Any]:
    """The JSON Schema object for ``function``'s keyword parameters.

    Parameters without a default (and not ``Optional``) are required. Unknown arguments are
    refused. ``Annotated[T, "text"]`` adds a description; ``Annotated[T, {...}]`` adds schema
    keywords such as ``minimum``.
    """
    hints = typing.get_type_hints(function, include_extras=True)
    properties: Dict[str, Any] = {}
    required: List[str] = []
    for name, parameter in inspect.signature(function).parameters.items():
        if parameter.kind in (parameter.VAR_POSITIONAL, parameter.VAR_KEYWORD):
            raise TypeError(f"{function.__name__}: *args/**kwargs cannot be tool parameters")
        if parameter.kind is parameter.POSITIONAL_ONLY:
            raise TypeError(f"{function.__name__}: positional-only parameter {name}")
        annotation = hints.get(name, inspect.Parameter.empty)
        schema = _schema_of(annotation)
        if parameter.default is not parameter.empty and parameter.default is not None:
            schema["default"] = parameter.default
        properties[name] = schema
        if parameter.default is parameter.empty and not _optional(annotation):
            required.append(name)
    schema = {"type": "object", "properties": properties, "additionalProperties": False}
    if required:
        schema["required"] = required
    return schema


_CHECKS: Dict[str, Callable[[Any], bool]] = {
    "string": lambda value: isinstance(value, str),
    "integer": lambda value: isinstance(value, int) and not isinstance(value, bool),
    "number": lambda value: isinstance(value, (int, float)) and not isinstance(value, bool),
    "boolean": lambda value: isinstance(value, bool),
    "array": lambda value: isinstance(value, list),
    "object": lambda value: isinstance(value, dict),
}


def _check(name: str, value: Any, schema: Mapping[str, Any]) -> None:
    """Refuse a value of the wrong JSON type, so the tool body sees the declared types."""
    if value is None:
        return
    if "enum" in schema and value not in schema["enum"]:
        raise ToolError(f"argument `{name}` must be one of {schema['enum']}")
    if "anyOf" in schema:
        for option in schema["anyOf"]:
            try:
                _check(name, value, option)
                return
            except ToolError:
                continue
        raise ToolError(f"argument `{name}` has the wrong type")
    kind = schema.get("type")
    if kind and not _CHECKS[kind](value):
        raise ToolError(f"argument `{name}` must be of type {kind}")
    if kind == "array" and "items" in schema:
        for index, item in enumerate(value):
            _check(f"{name}[{index}]", item, schema["items"])


# --- Plugin --------------------------------------------------------------------------------


@dataclass
class _Tool:
    name: str
    function: Callable[..., Any]
    description: str
    parameters: Dict[str, Any]
    timeout_secs: int


@dataclass
class _Hook:
    name: str
    event: str
    function: Callable[[Event], Any]
    matcher: Optional[str]
    timeout_secs: int
    fail_closed: Optional[bool]


@dataclass
class Plugin:
    """A set of tools and hooks. ``prefix`` (default ``name``) is prepended to tool names in the
    generated configuration, so plugins cannot collide with each other or built-in tools."""

    name: str
    prefix: Optional[str] = None
    tools: Dict[str, _Tool] = field(default_factory=dict)
    hooks: Dict[str, _Hook] = field(default_factory=dict)

    def __post_init__(self) -> None:
        if not _NAME.match(self.name):
            raise ValueError(f"plugin name {self.name!r} must be letters, digits, - or _")
        if self.prefix is None:
            self.prefix = self.name.replace("-", "_")

    # Registration ----------------------------------------------------------------------

    def tool(
        self,
        function: Optional[Callable[..., Any]] = None,
        *,
        name: Optional[str] = None,
        description: Optional[str] = None,
        timeout_secs: int = 60,
    ) -> Any:
        """Register a tool. Usable bare (``@plugin.tool``) or with options."""

        def register(function: Callable[..., Any]) -> Callable[..., Any]:
            tool_name = name or function.__name__
            if not _NAME.match(tool_name):
                raise ValueError(f"tool name {tool_name!r} must be letters, digits, - or _")
            if tool_name in self.tools:
                raise ValueError(f"tool {tool_name!r} is already registered")
            if len(self.tool_name(tool_name)) > 64:
                raise ValueError(f"tool name {self.tool_name(tool_name)!r} exceeds 64 characters")
            text = description or inspect.getdoc(function) or ""
            self.tools[tool_name] = _Tool(
                tool_name,
                function,
                text.strip(),
                schema_for(function),
                timeout_secs,
            )
            return function

        return register(function) if function is not None else register

    def hook(
        self,
        event: str,
        *,
        matcher: Optional[str] = None,
        name: Optional[str] = None,
        timeout_secs: int = 30,
        fail_closed: Optional[bool] = None,
    ) -> Callable[[Callable[[Event], Any]], Callable[[Event], Any]]:
        """Register a handler for a lifecycle event. ``matcher`` is an anchored regex on the
        tool name for tool events. Return ``None`` to continue, a :class:`Block` to refuse, a
        :class:`Reply` (or a dict of Dial's reply keys) to change the action, or a string to
        show as a notice."""
        canonical = _ALIASES.get(event, event)
        if canonical not in EVENTS:
            raise ValueError(f"unknown hook event {event!r}; use one of {', '.join(EVENTS)}")
        if matcher is not None:
            re.compile(matcher)

        def register(function: Callable[[Event], Any]) -> Callable[[Event], Any]:
            hook_name = name or function.__name__
            if not _NAME.match(hook_name):
                raise ValueError(f"hook name {hook_name!r} must be letters, digits, - or _")
            if hook_name in self.hooks:
                raise ValueError(f"hook {hook_name!r} is already registered")
            self.hooks[hook_name] = _Hook(
                hook_name, canonical, function, matcher, timeout_secs, fail_closed
            )
            return function

        return register

    # Running ---------------------------------------------------------------------------

    def call_tool(self, name: str, arguments: Mapping[str, Any]) -> str:
        """Run tool ``name`` with ``arguments`` and return its result text. Raises
        :class:`ToolError` for invalid arguments or a failure the tool reports."""
        tool = self.tools.get(name)
        if tool is None:
            raise ToolError(f"plugin {self.name} has no tool {name!r}")
        if not isinstance(arguments, Mapping):
            raise ToolError("tool arguments must be a JSON object")
        properties = tool.parameters["properties"]
        unknown = sorted(set(arguments) - set(properties))
        if unknown:
            raise ToolError(f"unknown argument(s): {', '.join(unknown)}")
        for key in tool.parameters.get("required", []):
            if key not in arguments:
                raise ToolError(f"missing required argument `{key}`")
        for key, value in arguments.items():
            _check(key, value, properties[key])
        return _render(tool.function(**arguments))

    def run_hook(self, name: str, event: Mapping[str, Any]) -> Optional[Dict[str, Any]]:
        """Run hook ``name`` on ``event``. Returns Dial's JSON answer, or ``None`` to continue
        silently. A :class:`Block` answer is returned as ``{"block": reason}``."""
        hook = self.hooks.get(name)
        if hook is None:
            raise ValueError(f"plugin {self.name} has no hook {name!r}")
        answer = hook.function(Event(event))
        if answer is None:
            return None
        if isinstance(answer, Block):
            return {"block": answer.reason}
        if isinstance(answer, Reply):
            return answer.to_json()
        if isinstance(answer, str):
            return {"notice": answer}
        if isinstance(answer, Mapping):
            return dict(answer)
        raise TypeError(f"hook {name} returned {type(answer).__name__}")

    # Configuration ---------------------------------------------------------------------

    def tool_name(self, name: str) -> str:
        """The name the model sees for tool ``name``."""
        return f"{self.prefix}_{name}" if self.prefix else name

    def manifest(self) -> Dict[str, Any]:
        """Everything Dial needs to run this plugin, as data."""
        return {
            "plugin": self.name,
            "tools": [
                {
                    "name": self.tool_name(tool.name),
                    "description": tool.description,
                    "parameters": tool.parameters,
                    "timeout_secs": tool.timeout_secs,
                }
                for tool in self.tools.values()
            ],
            "hooks": [
                {
                    "name": hook.name,
                    "event": hook.event,
                    "matcher": hook.matcher,
                    "timeout_secs": hook.timeout_secs,
                    "fail_closed": hook.fail_closed,
                }
                for hook in self.hooks.values()
            ],
        }

    def config(self, script: str, python: str) -> str:
        """The TOML that registers this plugin with Dial, run as ``python script``."""
        lines = [f"# Dial plugin {self.name} ({os.path.basename(script)}), generated by dial_sdk"]
        for tool in self.tools.values():
            lines += [
                "",
                f"[tools.{_toml_key(self.tool_name(tool.name))}]",
                f"program = {_toml(python)}",
                f"args = {_toml([script, 'tool', tool.name])}",
                f"timeout_secs = {tool.timeout_secs}",
                f"description = {_toml(tool.description)}",
                f"parameters = {_toml(tool.parameters)}",
            ]
        for hook in self.hooks.values():
            lines += [
                "",
                "[[hooks]]",
                f"event = {_toml(hook.event)}",
            ]
            if hook.matcher is not None:
                lines.append(f"matcher = {_toml(hook.matcher)}")
            lines += [
                f"command = {_toml([python, script, 'hook', hook.name])}",
                f"timeout_secs = {hook.timeout_secs}",
            ]
            if hook.fail_closed is not None:
                lines.append(f"fail_closed = {_toml(hook.fail_closed)}")
        return "\n".join(lines) + "\n"

    # Command line ----------------------------------------------------------------------

    def main(self, argv: Optional[List[str]] = None) -> None:
        """Entry point: ``tool NAME``, ``hook NAME``, ``config`` or ``manifest``."""
        sys.exit(self._main(sys.argv[1:] if argv is None else argv))

    def _main(self, argv: List[str]) -> int:
        usage = "usage: PLUGIN.py tool NAME | hook NAME | config | manifest"
        if argv[:1] == ["config"] and len(argv) == 1:
            script = os.path.abspath(sys.argv[0])
            sys.stdout.write(self.config(script, sys.executable))
            return 0
        if argv[:1] == ["manifest"] and len(argv) == 1:
            json.dump(self.manifest(), sys.stdout, indent=2)
            sys.stdout.write("\n")
            return 0
        if len(argv) != 2 or argv[0] not in ("tool", "hook"):
            print(usage, file=sys.stderr)
            return 64
        kind, name = argv
        try:
            payload = json.loads(sys.stdin.read() or "{}")
        except ValueError as error:
            print(f"invalid JSON on stdin: {error}", file=sys.stderr)
            return 1
        if kind == "tool":
            return self._tool_main(name, payload)
        return self._hook_main(name, payload)

    def _tool_main(self, name: str, arguments: Any) -> int:
        try:
            text = self.call_tool(name, arguments)
        except ToolError as error:
            print(str(error) or type(error).__name__, file=sys.stderr)
            return 1
        except Exception:  # noqa: BLE001 - reported to Dial, which shows the stderr tail
            traceback.print_exc()
            return 1
        sys.stdout.write(text)
        return 0

    def _hook_main(self, name: str, event: Any) -> int:
        if not isinstance(event, dict):
            print("hook event must be a JSON object", file=sys.stderr)
            return 1
        try:
            answer = self.run_hook(name, event)
        except Exception:  # noqa: BLE001 - Dial applies the hook's fail_closed policy
            traceback.print_exc()
            return 1
        if answer is None:
            return 0
        if set(answer) == {"block"}:
            # Exit 2 blocks every event, including those that read only the exit status.
            print(answer["block"], file=sys.stderr)
            return BLOCK_EXIT
        json.dump(answer, sys.stdout)
        return 0


def _render(result: Any) -> str:
    if result is None:
        return ""
    if isinstance(result, str):
        return result
    return json.dumps(result, ensure_ascii=False, default=str)


_BARE_KEY = re.compile(r"^[A-Za-z0-9_-]+$")


def _toml_key(key: str) -> str:
    return key if _BARE_KEY.match(key) else json.dumps(key)


def _toml(value: Any) -> str:
    """A TOML value. JSON string escapes are valid TOML basic-string escapes."""
    if isinstance(value, bool):
        return "true" if value else "false"
    if isinstance(value, (int, float)):
        return json.dumps(value)
    if isinstance(value, str):
        return json.dumps(value, ensure_ascii=False)
    if isinstance(value, (list, tuple)):
        return "[" + ", ".join(_toml(item) for item in value) + "]"
    if isinstance(value, Mapping):
        items = ", ".join(f"{_toml_key(str(key))} = {_toml(item)}" for key, item in value.items())
        return "{" + items + "}" if items else "{}"
    if value is None:
        raise ValueError("TOML has no null value")
    raise TypeError(f"cannot write {type(value).__name__} as TOML")
