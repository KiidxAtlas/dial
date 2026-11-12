# dial-sdk: Dial plugins in Python

Write [Dial](../../README.md) tools and lifecycle hooks as decorated Python functions. The SDK uses
only the standard library (Python 3.9+). Plugins run through Dial's existing extension points:
configured tools (`[tools.<name>]`) and `[[hooks]]`. They need no daemon and no Dial-side runtime.

```python
from dial_sdk import Annotated, Block, Plugin, ToolError

plugin = Plugin("notes")

@plugin.tool
def word_count(text: Annotated[str, "Text to count"], unique: bool = False) -> int:
    """Count the words in a text."""
    words = text.split()
    return len(set(words)) if unique else len(words)

@plugin.hook("pre_tool_use", matcher="write|edit")
def no_env_files(event):
    if event.input.get("path", "").endswith(".env"):
        return Block("editing .env files is not allowed")

if __name__ == "__main__":
    plugin.main()
```

```sh
pip install ./sdk/python                 # or put dial_sdk on PYTHONPATH
python3 notes.py config >> ~/.dial/config.toml
```

`config` prints the TOML that registers every tool (named `<plugin>_<tool>`, here
`notes_word_count`) and hook, run with the current interpreter and the script's absolute path.
`manifest` prints the same as JSON. See [extensibility](../../docs/extensibility.md#python-plugin-sdk)
for the full contract, and [`examples/word_tools.py`](examples/word_tools.py) for a runnable plugin.

Plugins are trusted code. Like every configured tool and hook, they run unsandboxed with your privileges.

Tests: `python3 -m unittest discover -s sdk/python/tests` (also run by `make test-python`).
