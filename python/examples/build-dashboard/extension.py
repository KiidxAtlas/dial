"""The watcher reports stale results; only an explicit button/command starts a build."""
from dial_extensions import Extension

extension = Extension("build-dashboard", capabilities=["host.tools", "host.context", "views"])
extension.settings_schema({"type": "object", "properties": {
    "build_command": {"type": "string", "default": "cargo check --locked"},
    "test_command": {"type": "string", "default": "cargo test --locked --lib"}
}, "additionalProperties": False})
revision = 1
status = "No build or tests have run."


def elements():
    return [{"type": "text", "text": status},
            {"type": "button", "id": "build", "label": "Build (approval required)"},
            {"type": "button", "id": "test", "label": "Test (approval required)"}]


def refresh():
    global revision
    revision = extension.host.update_view("dashboard", elements(), revision=revision)["revision"]


@extension.view("dashboard", "Build and test", elements=elements())
def view_event(event):
    if event.get("type") == "activate" and event.get("node") in ("build", "test"):
        return run(event["node"])


@extension.command("build-dashboard", description="Run build or test and update the dashboard")
def run(args):
    global status
    kind = args.strip() or "build"
    if kind not in ("build", "test"):
        return "Usage: /build-dashboard [build|test]"
    settings = extension.host.settings()
    extension.host.progress("Starting " + kind)
    result = extension.host.tool("bash", {"command": settings[kind + "_command"], "timeout": 120})
    status = kind + (" failed:\n" if result.get("is_error") else " completed:\n") + result["text"][-8000:]
    refresh()
    return status


@extension.watch(["src/**/*.rs", "crates/**/*.rs", "Cargo.toml"], debounce=0.5)
def source_changed(paths):
    global status
    status = "Source changed; previous build results may be stale: " + ", ".join(paths[:8])
    refresh()


if __name__ == "__main__":
    extension.serve()
