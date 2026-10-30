"""Deterministic in-process SDK harness. This is not live host/approval verification."""
from __future__ import annotations

from contextlib import contextmanager
from copy import deepcopy
from typing import Any, Callable

from .host import Host, HostError


class FakeHost:
    """Explicit handlers and approval decisions; unknown operations fail closed.

    Example::

        host = FakeHost()
        host.handle('tools.list', lambda params: [])
        with host.attach(extension):
            result = extension.dispatch('command', {'name': 'hello', 'args': ''})
    """

    def __init__(self):
        self.calls: list[tuple[str, dict]] = []
        self._handlers: dict[str, Callable] = {}
        self._allowed: set[str] = set()

    def handle(self, method: str, callback: Callable[[dict], Any], *, approved: bool = False):
        self._handlers[method] = callback
        if approved:
            self._allowed.add(method)
        else:
            self._allowed.discard(method)
        return self

    def request(self, method: str, params: dict):
        self.calls.append((method, deepcopy(params)))
        if method not in self._handlers:
            raise HostError("fake host has no handler for " + method)
        if method in ("tools.call", "tasks.start", "models.call", "secrets.get", "workflows.run") and method not in self._allowed:
            raise HostError("fake host approval declined for " + method)
        return deepcopy(self._handlers[method](deepcopy(params)))

    @contextmanager
    def attach(self, extension):
        previous = extension.host
        extension.host = Host(self.request)
        try:
            yield self
        finally:
            extension.host = previous
