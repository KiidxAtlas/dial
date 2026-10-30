"""Bounded session-worker timers, debounced file subscriptions and queued display updates.

Dial polls these at safe boundaries. No callback runs merely because a repository
contains an extension; background code cannot independently acquire a host dispatcher.
"""
from __future__ import annotations

from itertools import islice
from pathlib import Path
from queue import Queue, Empty, Full
import time
from typing import Callable


class Scheduler:
    def _init_scheduler(self):
        self._clock = time.monotonic
        self._timers = []
        self._watches = []
        self._published = Queue(maxsize=128)
        self._shutdown_handlers = []

    def every(self, seconds: float):
        """Register callback() for a host-polled interval; missed ticks coalesce."""
        if isinstance(seconds, bool) or not isinstance(seconds, (float, int)) or not 0.1 <= seconds <= 86400:
            raise ValueError("timer interval must be between 0.1 seconds and one day")
        if len(self._timers) >= 64:
            raise ValueError("at most 64 timers may be registered")
        def register(callback: Callable):
            self._timers.append(dict(interval=float(seconds), due=self._clock() + seconds, callback=callback))
            return callback
        return register

    def watch(self, paths: list[str], *, debounce: float = 0.25):
        """Register callback(changed_paths) for up to 256 matching workspace files.

        Paths are relative to the host workspace and may contain glob patterns.
        Subscriptions inspect metadata only; polling never executes source files.
        """
        if (not isinstance(paths, list) or not 1 <= len(paths) <= 64
                or any(not isinstance(path, str) or not path or Path(path).is_absolute()
                       or ".." in Path(path).parts for path in paths)
                or isinstance(debounce, bool) or not isinstance(debounce, (int, float))
                or not 0 <= debounce <= 60):
            raise ValueError("watch requires relative paths and a 0–60 second debounce")
        if len(self._watches) >= 64:
            raise ValueError("at most 64 watches may be registered")
        def register(callback: Callable):
            self._watches.append(dict(paths=list(paths), debounce=float(debounce), snapshot=None,
                                      pending=set(), due=None, callback=callback))
            return callback
        return register

    def on_shutdown(self, callback: Callable):
        """Register bounded synchronous/async cleanup when the host requests shutdown."""
        self._shutdown_handlers.append(callback)
        return callback

    def publish(self, method: str, params: dict):
        """Queue a display-only host request from any thread; drained on the next poll.

        A full queue raises rather than silently dropping an update. Tools, model
        calls and other effectful operations cannot be queued this way.
        """
        if method not in ("views.update", "views.remove", "progress", "events.emit"):
            raise ValueError("only display updates may be queued")
        from .contributions import frozen
        try:
            self._published.put_nowait((method, frozen(params)))
        except Full:
            raise RuntimeError("extension display-update queue is full") from None

    @staticmethod
    def _snapshot(root: Path, patterns: list[str]):
        snapshot = {}
        truncated = False
        for pattern in patterns:
            for path in islice(root.glob(pattern), 257):
                resolved = path.resolve()
                if not resolved.is_relative_to(root) or not path.is_file():
                    continue
                relative = path.relative_to(root).as_posix()
                if relative not in snapshot and len(snapshot) == 256:
                    truncated = True
                    break
                try:
                    metadata = path.stat()
                except FileNotFoundError:
                    continue
                snapshot[relative] = (metadata.st_mtime_ns, metadata.st_size)
            if truncated:
                break
        return snapshot, truncated

    def _poll(self, params: dict):
        now = self._clock()
        results = []
        for timer in self._timers:
            if now >= timer["due"]:
                timer["due"] = now + timer["interval"]
                results.append(self._reply(self._invoke(timer["callback"])))
        workspace = params.get("workspace")
        truncated = False
        if self._watches and workspace:
            root = Path(workspace).resolve(strict=True)
            for watch in self._watches:
                snapshot, capped = self._snapshot(root, watch["paths"])
                truncated |= capped
                previous = watch["snapshot"]
                if previous is not None:
                    changed = {path for path in previous.keys() | snapshot.keys()
                               if previous.get(path) != snapshot.get(path)}
                    if changed:
                        watch["pending"].update(changed)
                        watch["due"] = now + watch["debounce"]
                watch["snapshot"] = snapshot
                if watch["pending"] and now >= watch["due"]:
                    changed = sorted(watch["pending"])
                    watch["pending"].clear()
                    watch["due"] = None
                    results.append(self._reply(self._invoke(watch["callback"], changed)))
        for _ in range(128):
            try:
                method, arguments = self._published.get_nowait()
            except Empty:
                break
            self.host.request(method, arguments)
        actions = [action for reply in results for action in reply.get("actions", [])]
        return dict(actions=actions, watch_truncated=truncated)
