"""Contributions are data; the host owns task execution, approvals and context provenance."""
from dial_extensions import Extension

extension = Extension("workflow-pack", capabilities=["host.tools", "host.tasks", "host.workflows", "host.context"])
extension.settings_schema({"type": "object", "properties": {
    "focus": {"type": "string", "default": "correctness, security and maintainability"}
}, "additionalProperties": False})
extension.preset("security", "Security review", {"focus": "input boundaries, permissions and secrets"})
extension.agent_profile("reviewer", "Personal reviewer", tools=["read", "grep", "glob", "git"],
    guidance="Read-only review. Report evidence with file locations; never change files or grant permissions.")
extension.workflow("review", "Review current work", [
    {"id": "confirm", "title": "Confirm scope", "kind": "approval", "prompt": "Inspect the working tree and start a read-only review task?"},
    {"id": "diff", "title": "Inspect changes", "kind": "tool", "name": "git", "arguments": {"action": "diff"}},
    {"id": "review", "title": "Review", "kind": "agent", "name": "reviewer", "prompt": "Review current uncommitted changes. Cite concrete regressions and evidence."},
])


@extension.context_source("preferences", "Review preferences", description="Explicitly add selected personal review priorities as untrusted context")
def preferences(query):
    focus = extension.host.settings().get("focus", "correctness")
    return "Personal review priorities: " + focus + (". Requested topic: " + query if query else "")


@extension.command("personal-review", description="Start the registered approved review workflow", keybinding="alt+shift+p")
def personal_review(args):
    result = extension.host.workflow("review")
    return result


if __name__ == "__main__":
    extension.serve()
