#!/usr/bin/env python3
"""Offline contracts against an installed/package executable outside the checkout."""
import argparse
import hashlib
import json
import os
import subprocess
import tempfile
from pathlib import Path


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--dial", type=Path, required=True)
    args = parser.parse_args()
    executable = args.dial.resolve(strict=True)
    with tempfile.TemporaryDirectory(prefix="dial-artifact-") as temporary:
        root = Path(temporary)
        workspace = root / "workspace"
        workspace.mkdir()
        home = root / "dial-home"
        env = os.environ.copy()
        env["DIAL_HOME"] = str(home)
        env.pop("DIAL_EMBEDDED_SHELL_HOST", None)

        def run(*arguments, cwd=workspace):
            result = subprocess.run([str(executable), *map(str, arguments)], cwd=cwd,
                                    env=env, capture_output=True, text=True, timeout=30)
            if result.returncode:
                raise RuntimeError(f"artifact command {arguments!r} failed: {result.stderr[-2048:]}")
            return result.stdout

        assert run("--version").startswith("dial ")
        assert "annotate" in run("--help")
        reference = json.loads(run("config", "--json", "reference"))
        assert reference["schema_version"] == 1
        settings = {entry["path"]: entry for entry in reference["settings"]}
        assert settings["harness.max_steps"]["default"] == 0
        assert "ui.pinned_workers" in settings and "memory.backend" in settings
        for shell in ("bash", "zsh", "fish", "powershell", "elvish"):
            assert "dial" in run("completions", shell)

        # Exercises the actual re-executed shell host and bundled utility registrations.
        script = root / "dial-shell-artifact.sh"
        script.write_text("printf 'alpha\\nbeta\\n' | head -n 1\n", encoding="utf-8")
        assert run("__embedded-shell", script).strip() == "alpha"
        assert not script.exists()

        # Re-executed hosts retain only completed, validated cwd/export state.
        state = root / "shell-state.json"
        seed = {"schema_version": 1, "completed": False, "cwd": str(workspace), "exported": {}}
        state.write_text(json.dumps(seed), encoding="utf-8")
        state.chmod(0o600)
        script.write_text("mkdir nested; cd nested; export HARNESS_FIXTURE_VALUE=retained\n", encoding="utf-8")
        run("__embedded-shell", script, state)
        saved = json.loads(state.read_text())
        assert saved["completed"] and Path(saved["cwd"]).resolve() == (workspace / "nested").resolve()
        assert saved["exported"]["HARNESS_FIXTURE_VALUE"] == "retained"
        script.write_text('printf "%s\\n" "$HARNESS_FIXTURE_VALUE"; pwd\n', encoding="utf-8")
        assert run("__embedded-shell", script, state, cwd=Path(saved["cwd"])).splitlines() == ["retained", saved["cwd"]]
        assert not script.exists()

        # Discover the actual project state path through the CLI, then annotate immutable output.
        # Config listing does not need model credentials and preserves the zero-step convention.
        run("config", "set", "harness.max_steps", "0", "--project")
        states = list((home / "projects").iterdir())
        assert len(states) == 1
        artifacts = states[0] / "artifacts"
        artifacts.mkdir(mode=0o700)
        source = b"first\nsecond\n"
        identity = hashlib.sha256(source).hexdigest()
        artifact = artifacts / identity
        artifact.write_bytes(source)
        artifact.chmod(0o600)
        uri = f"artifact://{identity}"
        note = json.loads(run("annotate", uri, "--note", "Check the second row", "--start", "2"))
        assert note["sha256"] == identity and note["start"] == note["end"] == 2
        notes = json.loads(run("annotate", uri, "--list"))
        assert len(notes["annotations"]) == 1 and notes["annotations"][0]["origin"] == "human_cli"
        assert artifact.read_bytes() == source
        print(json.dumps({"schema_version": 1, "executable": str(executable),
                          "executable_sha256": hashlib.sha256(executable.read_bytes()).hexdigest(),
                          "checks": ["entry", "metadata", "completions", "shell-host", "shell-state", "annotation"]}))


if __name__ == "__main__":
    main()
