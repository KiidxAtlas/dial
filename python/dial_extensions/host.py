"""Controller-mediated host requests. Capability declarations never grant approvals."""
from __future__ import annotations

from contextvars import ContextVar
import json
from typing import Any, Callable

MAX_FRAME = 1024 * 1024
_binding: ContextVar[Any] = ContextVar("dial_extension_host", default=None)


class HostError(RuntimeError):
    """A host operation was unavailable, declined, or failed."""


class Host:
    """The active callback's host. Not usable from detached/background threads.

    Tools, tasks and model requests are admitted by Dial's execution controller;
    this client has no direct registry, credential or approval access.
    """

    def __init__(self, exchange: Callable[[str, dict], Any] | None = None):
        self._exchange = exchange
        self._sequence = 0

    def request(self, method: str, params: dict | None = None) -> Any:
        params = {} if params is None else params
        if not isinstance(method, str) or not method or not isinstance(params, dict):
            raise TypeError("host requests require a method and object parameters")
        if self._exchange is not None:
            return self._exchange(method, params)
        binding = _binding.get()
        if binding is None:
            raise HostError("host API requires an active framed callback; queue background updates with extension.publish()")
        source, output, operation = binding
        self._sequence += 1
        identity = self._sequence
        frame = dict(callback=dict(method=method, params=params), operation_id=operation, callback_id=identity)
        encoded = json.dumps(frame, ensure_ascii=False, allow_nan=False)
        if len(encoded.encode("utf-8")) > MAX_FRAME:
            raise ValueError("host request exceeds 1 MiB")
        output.write(encoded + "\n")
        output.flush()
        raw = source.readline(MAX_FRAME + 2)
        if not raw or len(raw.encode("utf-8")) > MAX_FRAME or not raw.endswith("\n"):
            raise HostError("host response missing, incomplete, or too large")
        reply = json.loads(raw)
        if (not isinstance(reply, dict) or reply.get("operation_id") != operation
                or reply.get("callback_id") != identity):
            raise HostError("host response belongs to a different callback")
        if "error" in reply:
            raise HostError(str(reply["error"]))
        if "result" not in reply:
            raise HostError("host response has no result")
        return reply["result"]

    def context(self, *, entries: list[str] | None = None) -> dict:
        """Inspect metadata; selected conversation entries require separate admission."""
        return self.request("context.get", {} if entries is None else {"entries": entries})

    def tools(self) -> list[dict]:
        return self.request("tools.list")

    def tool(self, name: str, arguments: dict | None = None) -> dict:
        """Call an available tool through normal schema, approval and effect handling."""
        return self.request("tools.call", dict(name=name, arguments=arguments or {}))

    def start_task(self, task: str, *, agent: str = "worker", solution_space: str = "",
                   profile: str | None = None) -> dict:
        params = dict(task=task, agent=agent, solution_space=solution_space)
        if profile is not None:
            params["profile"] = profile
        return self.request("tasks.start", params)

    def task_status(self, task_id: str) -> dict:
        return self.request("tasks.status", {"id": task_id})

    def cancel_task(self, task_id: str) -> dict:
        return self.request("tasks.cancel", {"id": task_id})

    def model(self, prompt: str, *, schema: dict | None = None, model: str | None = None,
              max_tokens: int = 2048) -> dict:
        """Request bounded, tool-free inference; Dial owns credentials and cost approval."""
        params = dict(prompt=prompt, max_tokens=max_tokens)
        if schema is not None:
            params["schema"] = schema
        if model is not None:
            params["model"] = model
        return self.request("models.call", params)

    def update_view(self, view_id: str, elements: list[dict], *, revision: int) -> dict:
        return self.request("views.update", dict(id=view_id, elements=elements, revision=revision))

    def remove_view(self, view_id: str) -> dict:
        return self.request("views.remove", {"id": view_id})

    def progress(self, text: str, *, name: str = "", value: float | None = None,
                 total: float | None = None) -> Any:
        return self.request("progress", dict(text=text, name=name, value=value, total=total))

    def notify(self, text: str, *, level: str = "info") -> Any:
        return self.request("events.emit", dict(kind="notification", name=level, text=text))

    def workflow(self, workflow_id: str, arguments: dict | None = None) -> dict:
        return self.request("workflows.run", dict(id=workflow_id, arguments=arguments or {}))

    def settings(self) -> dict:
        return self.request("settings.get")

    def secret(self, name: str) -> str | None:
        """Resolve an explicitly configured secret through the host credential owner."""
        return self.request("secrets.get", {"name": name})
