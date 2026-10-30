"""Authoring helpers: python -m dial_extensions new DIR / check DIR.

check compiles source without importing or executing the extension. A static check
is not a trust review or a guarantee that callbacks will succeed at runtime.
"""
import argparse
import ast
import json
from pathlib import Path
import sys

from . import _tool_namespace, _validate_extension_name

EXAMPLE = '''"""A trusted Dial extension. Settings and state live outside the workspace."""
from dial_extensions import Extension, Reply, ToolResult

extension = Extension("{name}", capabilities=["ui.status", "actions.append_entry"])


@extension.command("hello")
def hello(args):
    state = extension.state.load(default={{"visits": 0}})
    state["visits"] += 1
    extension.state.save(state)
    greeting = extension.settings.get("greeting", "Hello")
    return Reply().panel("Greeting", [
        {{"type": "input", "id": "name", "label": greeting, "value": args}},
        {{"type": "button", "id": "save", "label": "Greet", "primary": True}},
    ], generation=state["visits"])


@extension.panel_close("hello")
def closed(outcome, generation):
    if outcome.get("status") == "submitted":
        name = outcome.get("values", {{}}).get("name", "friend")
        return Reply().text("Hello, " + name).append_entry("greeting", "Greeting #" + str(generation))


@extension.on("turn_start")
def turn_start(payload):
    return Reply().ui("status", ["Example extension ready"])


@extension.autocomplete
def complete(query):
    return [{{"value": "hello world", "label": "Greet world"}}] if "hello".startswith(query) else []


@extension.tool("echo", "Return the supplied text", {{
    "type": "object", "properties": {{"text": {{"type": "string"}}}},
    "required": ["text"], "additionalProperties": False,
}}, revision=1)
async def echo(arguments, workspace):
    # All custom tools require host approval. This code is not sandboxed.
    return ToolResult(arguments["text"])


if __name__ == "__main__":
    extension.serve()
'''


def create(directory: Path, name: str):
    _validate_extension_name(name)
    # The starter registers echo; fail before creating any directory or files.
    _tool_namespace(name, "echo")
    # Refuse even an existing empty directory: authoring never overwrites files.
    directory.mkdir(parents=True, exist_ok=False)
    manifest = dict(name=name, version="0.1.0", description="My Python Dial extension",
                    python=dict(entry="extension.py", interpreter="python3", dependencies=[]),
                    settings=dict(greeting="Hello"))
    (directory / "dial-plugin.json").write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
    (directory / "extension.py").write_text(EXAMPLE.format(name=name), encoding="utf-8")


def check(directory: Path):
    manifest = json.loads((directory / "dial-plugin.json").read_text(encoding="utf-8"))
    if not isinstance(manifest, dict):
        raise ValueError("manifest must be an object")
    _validate_extension_name(manifest.get("name"))
    if not isinstance(manifest.get("settings", {}), dict):
        raise ValueError("settings must be an object")
    python = manifest.get("python")
    if not isinstance(python, dict):
        raise ValueError("manifest requires a python object")
    unknown = python.keys() - {"entry", "interpreter", "dependencies", "runtime", "lockfile"}
    if unknown:
        raise ValueError("unknown python fields: " + ", ".join(sorted(unknown)))
    if "program" in manifest or "args" in manifest:
        raise ValueError("python is mutually exclusive with program and args")
    entry = python.get("entry")
    if not isinstance(entry, str) or not entry or len(entry.encode("utf-8")) > 4096 or "\\" in entry or "\0" in entry:
        raise ValueError("python.entry must be a relative Python file")
    relative = Path(entry)
    if relative.is_absolute() or ".." in relative.parts or relative.suffix != ".py":
        raise ValueError("python.entry must be a relative Python file within the extension")
    root = directory.resolve()
    target = (root / relative).resolve()
    if not target.is_relative_to(root):
        raise ValueError("python.entry escapes the extension")
    interpreter = python.get("interpreter", "python3")
    if (not isinstance(interpreter, str) or not interpreter or interpreter.startswith("-")
            or len(interpreter.encode("utf-8")) > 4096 or any(ord(char) < 32 or ord(char) == 127 for char in interpreter)):
        raise ValueError("python.interpreter must name an installed interpreter")
    dependencies = python.get("dependencies", [])
    if (not isinstance(dependencies, list) or len(dependencies) > 256
            or any(not isinstance(item, str) or not item.strip() or item.lstrip().startswith("-")
                   or len(item.encode("utf-8")) > 4096
                   or any(ord(char) < 32 or ord(char) == 127 for char in item)
                   for item in dependencies)):
        raise ValueError("python.dependencies must contain at most 256 bounded, non-option requirement strings without control characters")
    if python.get("runtime", "request") not in ("request", "worker"):
        raise ValueError("python.runtime must be request or worker")
    version = manifest.get("apiVersion", 1)
    if type(version) is int:
        minimum = maximum = version
    elif isinstance(version, dict) and version.keys() <= {"min", "max"}:
        minimum, maximum = version.get("min", 1), version.get("max", 1)
    else:
        raise ValueError("apiVersion must be 1 or a {min, max} range")
    if type(minimum) is not int or type(maximum) is not int or not minimum <= 1 <= maximum:
        raise ValueError("extension API version is incompatible with host version 1")
    if "lockfile" in python:
        lockfile = python["lockfile"]
        if (not isinstance(lockfile, str) or not lockfile or Path(lockfile).is_absolute()
                or ".." in Path(lockfile).parts or "\\" in lockfile):
            raise ValueError("python.lockfile must be a confined relative requirements file")
        locked = (root / lockfile).resolve()
        if not locked.is_relative_to(root) or not locked.is_file():
            raise ValueError("python.lockfile must exist inside the extension")
        if dependencies:
            raise ValueError("use either dependencies or a hash-locked requirements file, not both")
    ast.parse(target.read_text(encoding="utf-8"), filename=str(target))
    return manifest


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    subcommands = parser.add_subparsers(dest="command", required=True)
    new = subcommands.add_parser("new", help="create an extension (never overwrites)")
    new.add_argument("directory", type=Path)
    new.add_argument("--name")
    validate = subcommands.add_parser("check", help="validate manifest and syntax without executing code")
    validate.add_argument("directory", type=Path)
    args = parser.parse_args(argv)
    try:
        if args.command == "new":
            create(args.directory, args.name or args.directory.name)
            print("Created " + str(args.directory) + "; review then install with dial plugin install file:" + str(args.directory))
        else:
            check(args.directory)
            print("Manifest and Python syntax OK (code not executed)")
    except (OSError, ValueError, SyntaxError) as error:
        print("dial_extensions: " + str(error), file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
