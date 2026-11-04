"""Isolated Dial session evidence shared by cross-agent diagnostics and registry grading.

This is not another runner. Only bounded, fresh, linear Dial journals are accepted;
compaction/branch projection belongs to the storage owner. Captures are private raw
evidence, not an efficiency score or a substitute for independent outcome checks.
"""
import json
import os
import stat
from pathlib import Path

MODES = {"registry_discovery_batch": "batch", "registry_discovery_one_call": "one_call"}
# Source content, not filenames in model-authored arguments, earns inspection credit.
EVIDENCE = {
    "registry": ('COMMANDS: &[(&str, &str)]', '"/compact"'),
    "docs": ("interface-tree-label",),
    "settings": ("smooth_reveal = true", "page_size = 24"),
    "keys": ('TREE_UP: &str = "Ctrl+Up"',),
}
READ_ONLY = {"read", "find", "grep", "ast_search", "glob", "bash"}


def discovery_mode(case):
    modes = [MODES[tag] for tag in case.get("categories", []) if tag in MODES]
    if len(modes) > 1:
        raise ValueError("discovery case has conflicting modes")
    return modes[0] if modes else None


def grade_messages(messages, mode):
    """Grade settled native calls grouped by assistant reply; no raw-call quota.

    Multi-path reads count as useful batching too: independent evidence must actually
    arrive in the same round. Two calls reading the same source earn no batch credit.
    Limits are task-specific round-trip budgets, not general runtime policy.
    """
    if mode not in {"batch", "one_call"}:
        raise ValueError("unknown discovery mode")
    failures, seen, ids = [], set(), set()
    rounds = useful_batches = registry_misses = 0
    pending = {}
    final = False
    for message in messages:
        role = message.get("role")
        if role == "assistant":
            if pending:
                failures.append("unsettled_calls")
                break
            rounds += 1
            calls = message.get("tool_calls") or []
            if not calls:
                final = bool(message.get("content"))
                continue
            final = False
            if mode == "one_call" and len(calls) > 1:
                failures.append("one_call_violation")
            batch_evidence = set()
            for call in calls:
                call_id = call.get("id")
                function = call.get("function") or {}
                arguments = function.get("arguments", {})
                if isinstance(arguments, str):
                    try:
                        arguments = json.loads(arguments)
                    except ValueError:
                        arguments = None
                if not call_id or call_id in ids or not isinstance(arguments, dict):
                    failures.append("malformed_call")
                    continue
                ids.add(call_id)
                pending[call_id] = (function.get("name"), arguments, batch_evidence)
        elif role == "tool":
            item = pending.pop(message.get("tool_call_id"), None)
            if item is None:
                failures.append("orphan_result")
                continue
            name, arguments, batch_evidence = item
            text = message.get("content", "")
            if not isinstance(text, str):
                failures.append("unsupported_result")
                continue
            found = {key for key, markers in EVIDENCE.items() if all(s in text for s in markers)}
            if message.get("is_error") or name not in READ_ONLY:
                found = set()
            # Search-location output may precede a declaration read. Repeated empty
            # registry probes (e.g. guessing name: fields) may not be repaired into a pass.
            if ("registry" not in seen and "command_registry" in json.dumps(arguments)
                    and name in {"grep", "find", "ast_search", "bash"}
                    and text.splitlines()[:1] == ["no matches"]):
                registry_misses += 1
            if "registry" in found:
                if registry_misses >= 2:
                    failures.append("serial_registry_guesses")
                seen.add("registry")
            seen.update(found)
            batch_evidence.update(found)
            if not pending and len(batch_evidence & {"docs", "settings", "keys"}) >= 2:
                useful_batches += 1
    if pending:
        failures.append("unsettled_calls")
    if not final:
        failures.append("missing_final_reply")
    if set(EVIDENCE) - seen:
        failures.append("missing_source_inspection")
    if mode == "batch" and not useful_batches:
        failures.append("no_useful_batch")
    if rounds > {"batch": 5, "one_call": 8}[mode]:
        failures.append("excess_round_trips")
    return {"result": "failed" if failures else "passed", "mode": mode,
            "round_trips": rounds, "useful_batches": useful_batches,
            "inspected_sources": sorted(seen), "failures": sorted(set(failures))}


def load_trial_session(agent, environment, session_id, trial_root):
    """Read only the result-named session in this trial's isolated home, at most 4 MiB."""
    home = environment.get("DIAL_HOME")
    if (agent.get("output") != "dial-json" or not home or not isinstance(session_id, str)
            or not session_id or len(session_id) > 64
            or any(c not in "0123456789abcdef" for c in session_id)):
        return None
    if not Path(home).resolve().is_relative_to(Path(trial_root).resolve()):
        return None
    try:
        candidates, pending, visited = [], [Path(home)], 0
        while pending:
            with os.scandir(pending.pop()) as entries:
                for entry in entries:
                    visited += 1
                    if visited > 10_000:
                        return None
                    if entry.is_dir(follow_symlinks=False):
                        pending.append(Path(entry.path))
                    elif entry.name.endswith(f"_{session_id}.jsonl"):
                        candidates.append(Path(entry.path))
                        if len(candidates) > 1:
                            return None
        if len(candidates) != 1 or not candidates[0].resolve().is_relative_to(Path(home).resolve()):
            return None
        # Nonblocking + fstat bounds special-file attacks even if a pathname is replaced.
        fd = os.open(candidates[0], os.O_RDONLY | os.O_NONBLOCK | os.O_NOFOLLOW)
        with os.fdopen(fd, "rb") as source:
            if not stat.S_ISREG(os.fstat(source.fileno()).st_mode):
                return None
            raw = source.read(4 * 1024 * 1024 + 1)
        if len(raw) > 4 * 1024 * 1024:
            return None
        text = raw.decode("utf-8")
        records = [json.loads(line) for line in text.splitlines() if line.strip()]
        if any(not isinstance(record, dict) for record in records):
            return None
        # Rewinds/compaction need the storage owner's projection, not an ad-hoc
        # replay in the benchmark reader.
        if any(record.get("type") in {"compact", "compaction", "rewind", "branch", "reset_boundary"} for record in records):
            return None
        # A rewind is normally a changed parent link, not a record named "branch".
        # Accept only a linear fresh journal; storage owns projection of other histories.
        previous, ids = None, set()
        for record in records:
            if record.get("type") == "session":
                if ids:
                    return None
                continue
            identity = record.get("id")
            if (record.get("type") not in {"message", "runtime", "custom", "meta", "label"}
                    or not isinstance(identity, str) or identity in ids
                    or record.get("parent") != previous):
                return None
            ids.add(identity)
            previous = identity
        messages = [record["message"] for record in records if record.get("type") == "message"]
        if not messages or any(not isinstance(message, dict) for message in messages):
            return None
        return text, messages
    except (OSError, ValueError, KeyError, TypeError, AttributeError):
        return None


def write_private_trace(path, text):
    """Never overwrite evidence; raw source and arguments must remain owner-only."""
    # O_EXCL also refuses a pre-existing symlink without following it.
    with open(os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600), "w", encoding="utf-8") as target:
        target.write(text)


def capture_trial_trace(agent, environment, session_id, trial_root, log_dir, index):
    """Diagnostic only: missing/unsupported evidence is explicitly unavailable, not empty."""
    unavailable = {"status": "unavailable", "reason": "isolated_linear_session_unavailable"}
    if log_dir is None:
        return dict(unavailable, reason="log_directory_required")
    loaded = load_trial_session(agent, environment, session_id, trial_root)
    if loaded is None:
        return unavailable
    text, _ = loaded
    path = log_dir / f"turn-{index}.session.jsonl"
    try:
        write_private_trace(path, text)
    except OSError:
        return dict(unavailable, reason="trace_write_failed")
    return {"status": "captured", "path": str(path), "bytes": len(text.encode("utf-8"))}


def grade_trial_session(agent, case, environment, final_session_id, trial_root, log_dir=None):
    """Locate exactly the CLI result's isolated session; never inspect operator state."""
    mode = discovery_mode(case)
    unavailable = {"result": "unavailable", "mode": mode, "round_trips": None,
                   "failures": ["isolated_session_trace_unavailable"]}
    loaded = load_trial_session(agent, environment, final_session_id, trial_root)
    if loaded is None:
        return unavailable
    text, messages = loaded
    try:
        policies = []
        for message in messages:
            if message.get("role") == "dial_state":
                state = message.get("state", message.get("patch", {}))
                policy = (state.get("reliability") or {}).get("call_policy")
                if isinstance(policy, dict) and policy.get("mode") in {"batch", "one_call"}:
                    policies.append(policy["mode"])
        if not policies:
            return dict(unavailable, failures=["call_policy_unavailable"])
        if mode == "batch" and "one_call" in policies:
            return dict(unavailable, failures=["runtime_disallows_batch"])
        result = grade_messages(messages, mode)
        result["observed_policy_modes"] = sorted(set(policies))
        if log_dir is not None:
            write_private_trace(log_dir / "discovery-trace.jsonl", text)
        return result
    except (OSError, ValueError, KeyError, TypeError, AttributeError):
        return unavailable
