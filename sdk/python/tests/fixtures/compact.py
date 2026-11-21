"""Independent SDK plugin for the engine's compaction hook contract test."""
from pathlib import Path
import sys

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from dial_sdk import Plugin, Reply

plugin = Plugin("compact_fixture")


@plugin.hook("session_before_compact")
def annotate(event):
    return Reply(additional_context=f"reason: {event['reason']}")


if __name__ == "__main__":
    plugin.main()
