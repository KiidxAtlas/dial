"""Versioned declarative registrations; the host validates and owns their execution."""
from __future__ import annotations

import json
import re
from typing import Any


def identifier(value: str) -> str:
    if not isinstance(value, str) or not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_-]{0,63}", value):
        raise ValueError("contribution id must be 1–64 ASCII letters, digits, underscores or hyphens")
    return value


def frozen(value: Any) -> Any:
    return json.loads(json.dumps(value, allow_nan=False))


class Contributions:
    """Shared registration helpers for Extension. No registration grants host permissions."""

    def _init_contributions(self):
        self._contributions = dict(views=[], settings={}, commands=[], agents=[], workflows=[],
                                   context_sources=[], renderers=[], keybindings=[], presets=[])
        self._view_handlers = {}
        self._context_handlers = {}

    def _contribute(self, kind: str, definition: dict):
        definition = frozen(definition)
        name = definition.get("id", definition.get("name"))
        identifier(name)
        values = self._contributions[kind]
        if len(values) >= 64:
            raise ValueError("at most 64 " + kind + " may be registered")
        if any(item.get("id", item.get("name")) == name for item in values):
            raise ValueError("duplicate " + kind + " contribution: " + name)
        values.append(definition)
        return self

    def settings_schema(self, schema: dict):
        """Declare typed properties, defaults and secret fields for user/workspace settings."""
        if not isinstance(schema, dict) or schema.get("type") != "object":
            raise ValueError("settings schema must have type object")
        self._contributions["settings"] = frozen(schema)
        return self

    def register_view(self, view_id: str, title: str, elements: list[dict], *, location: str = "sidebar"):
        if location not in ("sidebar", "tab"):
            raise ValueError("view location must be sidebar or tab")
        return self._contribute("views", dict(id=view_id, title=title, elements=elements, location=location))

    def view(self, view_id: str, title: str, *, elements: list[dict] | None = None, location: str = "sidebar"):
        """Register a persistent view and callback(event) for host-owned UI interaction."""
        self.register_view(view_id, title, elements or [], location=location)
        return self._register(self._view_handlers, view_id)

    def context_source(self, source_id: str, title: str, *, description: str = ""):
        """Explicitly selectable callback(query) returning context, never system instructions."""
        self._contribute("context_sources", dict(id=source_id, title=title, description=description))
        return self._register(self._context_handlers, source_id)

    def agent_profile(self, profile_id: str, title: str, *, model: str | None = None,
                      tools: list[str] | None = None, guidance: str = ""):
        fields = dict(id=profile_id, title=title, tools=tools or [], guidance=guidance)
        if model is not None:
            fields["model"] = model
        return self._contribute("agents", fields)

    def workflow(self, workflow_id: str, title: str, steps: list[dict]):
        """Register visible tool/agent/approval steps run by the existing host controller."""
        return self._contribute("workflows", dict(id=workflow_id, title=title, steps=steps))

    def renderer(self, renderer_id: str, title: str, *, fields: list[dict] | None = None,
                 markdown: str | None = None):
        definition = dict(id=renderer_id, title=title, fields=fields or [])
        if markdown is not None:
            definition["markdown"] = markdown
        return self._contribute("renderers", definition)

    def keybinding(self, key: str, command: str):
        if not isinstance(key, str) or not key or not isinstance(command, str) or not command:
            raise ValueError("keybinding needs a key and command")
        self._contributions["keybindings"].append(dict(key=key, command=command))
        return self

    def preset(self, preset_id: str, title: str, values: dict):
        return self._contribute("presets", dict(id=preset_id, title=title, values=values))
