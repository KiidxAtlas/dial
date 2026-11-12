"""Example Dial plugin: two tools and a policy hook.

    python3 sdk/python/examples/word_tools.py config >> ~/.dial/config.toml

The script imports dial_sdk from the checkout; with the SDK installed (pip install sdk/python)
the sys.path line is unnecessary.
"""

import os
import sys
from typing import List, Literal, Optional

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), ".."))

from dial_sdk import Annotated, Block, Plugin, Reply, ToolError  # noqa: E402

plugin = Plugin("words")


@plugin.tool
def count(
    text: Annotated[str, "Text to count"],
    unit: Literal["words", "lines", "characters"] = "words",
    unique: bool = False,
) -> dict:
    """Count the words, lines or characters of a text."""
    items = {"words": text.split(), "lines": text.splitlines(), "characters": list(text)}[unit]
    return {"unit": unit, "count": len(set(items)) if unique else len(items)}


@plugin.tool(description="Join strings with a separator.")
def join(parts: List[str], separator: Optional[str] = None) -> str:
    if not parts:
        raise ToolError("parts is empty")
    return (", " if separator is None else separator).join(parts)


@plugin.hook("pre_tool_use", matcher="write|edit")
def no_env_files(event):
    path = event.input.get("path", "")
    if os.path.basename(path).startswith(".env"):
        return Block(f"{path}: editing .env files is not allowed")
    return None


@plugin.hook("post_tool_use", matcher="bash")
def tag_failures(event):
    if event.get("is_error"):
        return Reply(additional_context="The last shell command failed; read its output first.")
    return None


if __name__ == "__main__":
    plugin.main()
