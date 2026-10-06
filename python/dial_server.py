"""Dial's MLX server entry point, embedded by the Rust supervisor.

MLX-LM 0.32.0 assumes tool-parser output always has arguments; malformed model output can raise
KeyError and close the HTTP connection. Preserve such failures as invalid OpenAI tool calls so the
Rust agent can diagnose/retry them. Never synthesize executable arguments for a malformed call.
"""
import json
import logging
import uuid


class ToolCallFormatter:
    def __init__(self, tool_parser, tools, streaming=False):
        self._idx = 0
        self._tool_parser = tool_parser
        self._tools = tools
        self._streaming = streaming

    def _format(self, parsed):
        name = parsed.get("name") if isinstance(parsed, dict) else None
        arguments = parsed.get("arguments") if isinstance(parsed, dict) else None
        valid = isinstance(name, str) and bool(name) and isinstance(arguments, dict)
        if not valid:
            logging.warning("Model emitted an invalid tool-call structure; returning a non-executable call")
        # The Rust client marks JSON null arguments malformed. {} would incorrectly turn a missing
        # argument object into a valid call to a zero-argument tool.
        call = {
            "id": str(uuid.uuid4()),
            "type": "function",
            "function": {
                "name": name if isinstance(name, str) else "",
                "arguments": json.dumps(arguments if valid else None, ensure_ascii=False),
            },
        }
        if self._streaming:
            call["index"] = self._idx
            self._idx += 1
        return call

    def __call__(self, tool_calls):
        result = []
        for text in tool_calls or []:
            try:
                parsed = self._tool_parser(text, self._tools)
            except (ValueError, TypeError, KeyError, IndexError, AttributeError):
                # Invalid model syntax is data, not a server exception. Backend/infrastructure errors
                # outside parsing still propagate rather than being mislabelled as model mistakes.
                parsed = None
            calls = parsed if isinstance(parsed, list) and parsed else [parsed]
            result.extend(self._format(call) for call in calls)
        return result


def main():
    from mlx_lm import server
    # The pinned server has no formatter factory parameter; replace this single protocol boundary.
    server.ToolCallFormatter = ToolCallFormatter
    import os
    from dial_steering import install_server
    specification=os.environ.get("DIAL_STEERING_SPEC", "")
    install_server(server, json.loads(specification) if specification else None)
    server.main()


if __name__ == "__main__":
    main()
