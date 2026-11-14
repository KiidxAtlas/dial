#!/usr/bin/env python3
"""Run the same benchmark suites through different coding-agent CLIs and score them identically.

Each trial gets a fresh copy of the case fixture, the same prompt text and the same model through
the agent's own non-interactive CLI. Outcome checks run afterwards, outside the agent, with the
same evaluator for every agent. Checks that only Dial can observe internally (`metric`,
`tool_invoked`) and forced compaction (`compact_before`) are not portable: they are recorded as
such and excluded from every agent's score, never counted as passes or failures.

This measures agents as installed and configured on this machine; it is not a leaderboard score.
"""
import argparse
import collections
import concurrent.futures
import datetime
import hashlib
import json
import os
import random
import signal
import re
import shutil
import statistics
import subprocess
import sys
import tempfile
import time
import tomllib
from pathlib import Path

PORTABLE_CHECKS = {"file_exists", "file_contains", "file_equals", "command", "response_equals"}
CHECK_TIMEOUT_SECS = 120
TOKEN_FIELDS = ("input_total", "uncached_input", "cache_read", "cache_write", "output")
MEASUREMENT_FIELDS = (*TOKEN_FIELDS, "requests", "tool_calls", "tool_errors", "delegations")
# Dial's per-turn `efficiency` object, as emitted by turn_loop.rs. `inference_latency` is the request timer
# snapshot (additive counters plus an `attribution` string); `storage_io` holds inclusive SessionIoMetrics
# counters; `projection_bytes` is the request-attribution Metrics. `requests_by_phase`, `prefix_invalidations`,
# `recovery_attempts` and `backend_operations` are nested inside `projection_bytes`, not at the root.
# Inclusive costs nest inside `duration_ms` and are never summed as wall time.
EFFICIENCY_FIELDS = ("duration_ms", "inference_latency", "storage_io", "projection_bytes")
# Count maps keyed by dynamic labels: a label absent from one turn means a zero count.
DYNAMIC_MAPS = frozenset({"requests_by_phase", "prefix_invalidations", "recovery_attempts", "backend_operations"})
# First-result timestamps are observations relative to each turn's start, not additive counters.
PER_TURN_FIELDS = frozenset({"first_tool_result_ms", "first_workspace_change_result_ms"})
PROJECTION_COUNTERS = ("requests", "requests_before_first_workspace_change_result", "stable_message_bytes",
                       "changed_message_bytes", "observed_reply_text_bytes", "observed_tool_argument_bytes")
UNKNOWN = object()
SKIP = object()
STDERR_EXCERPT_CHARS = 800
REDACTED = "[redacted]"
# Bounded, best-effort redaction for excerpts shown in reports and progress lines. Raw per-turn logs
# are kept unredacted beside the report, so they remain private artifacts.
SECRET_PATTERNS = (
    (re.compile(r"(?i)\b(api[_-]?key|access[_-]?token|auth[_-]?token|token|secret|password|passwd"
                r"|authorization|cookie)(\s*[:=]\s*)(?:bearer\s+)?\S+"), r"\1\2" + REDACTED),
    (re.compile(r"(?i)\bbearer\s+[A-Za-z0-9._~+/=-]+"), "Bearer " + REDACTED),
    (re.compile(r"\b(?:sk|pk|ghp|gho|xox[abp])[-_][A-Za-z0-9._-]{8,}"), REDACTED),
    (re.compile(r"\b[0-9a-fA-F]{32,}\b"), REDACTED),
)


def load_agents(path):
    data = tomllib.loads(Path(path).read_text())
    agents = data.get("agents")
    if not isinstance(agents, list) or not agents:
        raise ValueError("agents file needs at least one [[agents]] table")
    names = set()
    for agent in agents:
        for key in ("name", "first", "follow_up", "output"):
            if key not in agent:
                raise ValueError(f"agent is missing {key!r}")
        if agent["name"] in names:
            raise ValueError(f"duplicate agent: {agent['name']}")
        names.add(agent["name"])
        if agent["output"] not in PARSERS:
            raise ValueError(f"unknown output parser {agent['output']!r}; known: {', '.join(PARSERS)}")
        for key in ("first", "follow_up"):
            if not isinstance(agent[key], list) or not all(isinstance(a, str) for a in agent[key]):
                raise ValueError(f"{agent['name']}.{key} must be an argument list")
            if not any("{prompt}" in a for a in agent[key]):
                raise ValueError(f"{agent['name']}.{key} must contain {{prompt}}")
        env = agent.setdefault("env", {})
        if not isinstance(env, dict) or not all(isinstance(v, str) for v in env.values()):
            raise ValueError(f"{agent['name']}.env must map names to strings")
        tools = agent.setdefault("delegation_tools", ["task"])
        if not isinstance(tools, list) or not all(isinstance(t, str) for t in tools):
            raise ValueError(f"{agent['name']}.delegation_tools must be a list of tool names")
        if agent.setdefault("delegated_usage", "unknown") not in ("included", "unknown"):
            raise ValueError(f"{agent['name']}.delegated_usage must be 'included' or 'unknown'")
        thinking = agent.get("thinking_values", {})
        if (not isinstance(thinking, dict) or any(not isinstance(v, dict) or
                not all(isinstance(k, str) and isinstance(s, str) for k, s in v.items())
                for v in thinking.values())):
            raise ValueError(f"{agent['name']}.thinking_values must map levels to string substitutions")
    return agents


def agent_environment(agent, values):
    """Per-agent variables. `{work_root}` is the run's directory; `{trial_root}` is the trial's own
    directory, so state written there (a `*_HOME`, sessions) cannot leak into another trial. Then `~`
    and `$VAR` expand."""
    environment = dict(os.environ)
    if agent["env"].get("DIAL_HOME", "").startswith("{trial_root}"):
        # A clean benchmark home must not inherit process-level experimental/config overrides.
        environment = {name: value for name, value in environment.items() if not name.startswith("DIAL_")}
    for name, value in agent["env"].items():
        expanded = os.path.expandvars(os.path.expanduser(value.format(**values)))
        environment[name] = expanded
        if name.endswith("_HOME") and value.startswith(("{work_root}", "{trial_root}")):
            Path(expanded).mkdir(parents=True, exist_ok=True)
    return environment


def render(template, values):
    return [argument.format(**values) for argument in template]


def agent_values(agent, values):
    """Translate the shared thinking level to the installed CLI's explicit spelling/settings."""
    choices = agent.get("thinking_values", {})
    level = values.get("thinking", "off")
    if choices and level not in choices:
        raise ValueError(f"{agent['name']} does not support thinking level {level!r}")
    return {**values, **choices.get(level, {})}


def first_prompt(case):
    """Seeded context has no portable CLI form, so every agent receives it identically inline."""
    if not case.get("context_messages"):
        return case["prompt"]
    notes = "\n".join(f"- {message}" for message in case["context_messages"])
    return f"Notes from earlier in this conversation:\n{notes}\n\nRequest:\n{case['prompt']}"


def portable(check):
    return check["kind"] in PORTABLE_CHECKS


def run_check(check, workspace, final_text, *, cancel=None):
    """Return True, False, or None when the check could not be evaluated."""
    kind = check["kind"]
    if kind == "response_equals":
        return None if final_text is None else final_text.strip() == check["content"].strip()
    if kind == "command":
        try:
            result = run_agent_process(["/bin/sh", "-c", check["command"]], cwd=workspace,
                                       text=True, timeout=CHECK_TIMEOUT_SECS,
                                       env=dict(os.environ), stdin=subprocess.DEVNULL, cancel=cancel)
        except subprocess.TimeoutExpired:
            return None
        if result.returncode != check.get("expect_exit", 0):
            return False
        expected = check.get("expect_stdout")
        return expected is None or result.stdout.strip() == expected.strip()
    root = workspace.resolve()
    path = (workspace / check["path"]).resolve()
    if not path.is_relative_to(root) or not path.is_file():
        return False
    if kind == "file_exists":
        return True
    content = path.read_bytes().decode("utf-8", errors="replace")
    if kind == "file_contains":
        return check["needle"] in content
    return content == check["content"]


def empty_usage():
    return {**{field: 0 for field in TOKEN_FIELDS}, "requests": 0, "tool_calls": 0, "tool_errors": 0,
            "tools": {}, "delegations": 0, "models": [], "usage_complete": True}


def count_tool(usage, name):
    usage["tool_calls"] += 1
    usage["tools"][name] = usage["tools"].get(name, 0) + 1


def as_text(value):
    if value is None:
        return ""
    if isinstance(value, bytes):
        return value.decode("utf-8", errors="replace")
    return value


def redact(text):
    for pattern, replacement in SECRET_PATTERNS:
        text = pattern.sub(replacement, text)
    return text


def stderr_excerpt(text):
    """The tail of stderr after redaction, bounded so a report never carries a whole transcript."""
    return redact(as_text(text))[-STDERR_EXCERPT_CHARS:]


def parse_efficiency(result):
    """Dial's `efficiency` object for one turn, kept as emitted. A missing object or field is None (unknown)."""
    value = result.get("efficiency")
    if not isinstance(value, dict):
        return {field: None for field in EFFICIENCY_FIELDS}
    return {field: value.get(field) for field in EFFICIENCY_FIELDS}


def _is_number(value):
    return isinstance(value, (int, float)) and not isinstance(value, bool)


def numeric_tree(value):
    """Additive numeric leaves only. Strings, bools, nulls and per-turn first-result latencies are
    kept per turn, never summed."""
    if isinstance(value, dict):
        out = {}
        for key, child in value.items():
            if key in PER_TURN_FIELDS:
                continue
            part = numeric_tree(child)
            if part is not SKIP:
                out[key] = part
        return out
    return value if _is_number(value) else SKIP


def zero_like(value):
    return {key: zero_like(child) for key, child in value.items()} if isinstance(value, dict) else 0


def add_values(left, right, dynamic=False):
    """Add two numeric trees. A key missing on one side is UNKNOWN for fixed fields, and zero for a dynamic
    count map where absence means the label never occurred. Any UNKNOWN inside a tree makes the tree UNKNOWN."""
    if left is UNKNOWN or right is UNKNOWN:
        return UNKNOWN
    if isinstance(left, dict) and isinstance(right, dict):
        out = {}
        for key in dict.fromkeys([*left, *right]):
            a = left[key] if key in left else (zero_like(right[key]) if dynamic else UNKNOWN)
            b = right[key] if key in right else (zero_like(left[key]) if dynamic else UNKNOWN)
            value = add_values(a, b, dynamic=key in DYNAMIC_MAPS)
            if value is UNKNOWN:
                return UNKNOWN
            out[key] = value
        return out
    if _is_number(left) and _is_number(right):
        return left + right
    return UNKNOWN


def aggregate_efficiency(turns, failed):
    """One trial's efficiency: per field, the sum over turns of its numeric leaves. A field is None when the
    trial failed, a turn lacks the object, or a turn lacks that field. Non-numeric leaves stay per turn only."""
    unknown = {field: None for field in EFFICIENCY_FIELDS}
    if failed or not turns or any(turn.get("efficiency") is None for turn in turns):
        return unknown
    total = {}
    for field in EFFICIENCY_FIELDS:
        values = [turn["efficiency"].get(field) for turn in turns]
        if any(value is None for value in values):
            total[field] = None
            continue
        acc = numeric_tree(values[0])
        for value in values[1:]:
            acc = add_values(acc, numeric_tree(value))
        total[field] = None if acc is UNKNOWN or acc is SKIP else acc
    return total


def efficiency_known(trial):
    """True only when every required Dial efficiency field was reported for the whole trial."""
    efficiency = trial.get("efficiency") or {}
    return all(efficiency.get(field) is not None for field in EFFICIENCY_FIELDS)


def projection_totals(trials):
    """Attribution counters summed over fully known trials; None when none is known or a counter is absent."""
    known = [t["efficiency"]["projection_bytes"] for t in trials if efficiency_known(t)]
    totals = {}
    for name in PROJECTION_COUNTERS:
        values = [projection.get(name) for projection in known]
        totals[name] = sum(values) if known and all(_is_number(v) for v in values) else None
    return totals


def parse_dial_json(stdout):
    """Read the terminal result from `dial -p` JSON or stream-JSON; ignore diagnostic events."""
    usage = empty_usage()
    result = None
    for line in stdout.splitlines():
        line = line.strip()
        if line.startswith("{"):
            try:
                value = json.loads(line)
            except json.JSONDecodeError:
                continue
            if isinstance(value, dict) and value.get("type") == "result":
                result = value
    if result is None:
        usage["usage_complete"] = False
        return None, usage
    total = (((result.get("usage") or {}).get("total") or {}).get("all")) or {}
    if not total:
        usage["usage_complete"] = False
    cache_read, cache_write = total.get("cache_read", 0), total.get("cache_write", 0)
    usage.update(input_total=total.get("input", 0), cache_read=cache_read, cache_write=cache_write,
                 output=total.get("output", 0), requests=total.get("requests", 0))
    # Dial's `input` already includes cached prompt tokens.
    usage["uncached_input"] = max(usage["input_total"] - cache_read - cache_write, 0)
    calls = result.get("tool_calls") or []
    for call in calls:
        count_tool(usage, call.get("name") or "?")
    usage["tool_errors"] = sum(1 for call in calls if call.get("is_error"))
    by_model = (((result.get("usage") or {}).get("total") or {}).get("by_model")) or {}
    usage["models"] = sorted(by_model)
    # Kept beside the usage record so `add_usage` never mixes it into token totals.
    usage["efficiency"] = parse_efficiency(result)
    return result.get("text"), usage


def message_text(message):
    content = message.get("content")
    if isinstance(content, str):
        return content
    return "".join(part.get("text", "") for part in content or [] if part.get("type") == "text")


def parse_omp_json(stdout):
    """`omp -p --mode json`: one JSON event per line; usage is reported per assistant message."""
    usage = empty_usage()
    final_text = None
    models = set()
    for line in stdout.splitlines():
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue
        if not isinstance(event, dict):
            continue
        kind = event.get("type")
        if kind == "message_end" and (event.get("message") or {}).get("role") == "assistant":
            message = event["message"]
            reported = message.get("usage")
            usage["requests"] += 1
            if not reported:
                usage["usage_complete"] = False
            else:
                usage["uncached_input"] += reported.get("input", 0)
                usage["cache_read"] += reported.get("cacheRead", 0)
                usage["cache_write"] += reported.get("cacheWrite", 0)
                usage["output"] += reported.get("output", 0)
            if message.get("provider") and message.get("model"):
                models.add(f"{message['provider']}/{message['model']}")
            text = message_text(message)
            if text.strip():
                final_text = text
        elif kind == "tool_execution_end":
            count_tool(usage, event.get("toolName") or "?")
            usage["tool_errors"] += bool(event.get("isError"))
    usage["input_total"] = usage["uncached_input"] + usage["cache_read"] + usage["cache_write"]
    usage["models"] = sorted(models)
    if usage["requests"] == 0:
        usage["usage_complete"] = False
    return final_text, usage


def parse_claude_json(stdout):
    """`claude -p --output-format stream-json --verbose`: tokens come from the terminal `result`
    event's per-model totals (which include side and subagent calls). Requests are unique streamed
    assistant message IDs; one API message may arrive as several events, one per content block."""
    usage = empty_usage()
    result = None
    message_ids, tool_ids = set(), set()
    for line in stdout.splitlines():
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue
        if not isinstance(event, dict):
            continue
        kind = event.get("type")
        message = event.get("message") if isinstance(event.get("message"), dict) else {}
        if kind == "assistant":
            if message.get("id"):
                message_ids.add(message["id"])
            for part in message.get("content") or []:
                if isinstance(part, dict) and part.get("type") == "tool_use" and part.get("id") not in tool_ids:
                    tool_ids.add(part.get("id"))
                    count_tool(usage, part.get("name") or "?")
                    usage["delegations"] += part.get("name") in ("Task", "Agent")
        elif kind == "user":
            for part in message.get("content") or []:
                if isinstance(part, dict) and part.get("type") == "tool_result":
                    usage["tool_errors"] += part.get("is_error") is True
        elif kind == "result":
            result = event
    usage["requests"] = len(message_ids)
    by_model = (result or {}).get("modelUsage") or {}
    if result is None or not by_model or not message_ids:
        usage["usage_complete"] = False
        return (result or {}).get("result"), usage
    for model, counts in by_model.items():
        usage["uncached_input"] += counts.get("inputTokens", 0)
        usage["cache_read"] += counts.get("cacheReadInputTokens", 0)
        usage["cache_write"] += counts.get("cacheCreationInputTokens", 0)
        usage["output"] += counts.get("outputTokens", 0)
    usage["input_total"] = usage["uncached_input"] + usage["cache_read"] + usage["cache_write"]
    usage["models"] = sorted(by_model)
    return result.get("result"), usage


def parse_hermes_usage(usage_text, session_text=""):
    """`hermes -z --usage-file`: Hermes's `input_tokens` exclude cache read/write tokens.
    Tool calls come from `hermes sessions export` JSONL (OpenAI-style `tool_calls`/`tool` roles)."""
    usage = empty_usage()
    try:
        report = json.loads(usage_text)
    except (json.JSONDecodeError, TypeError):
        report = None
    if not isinstance(report, dict) or not isinstance(report.get("api_calls"), int):
        usage["usage_complete"] = False
        return None, usage
    usage["known_fields"] = ["requests"]
    fields = {"uncached_input": "input_tokens", "cache_read": "cache_read_tokens",
              "cache_write": "cache_write_tokens", "output": "output_tokens"}
    for key, source in fields.items():
        value = report.get(source)
        if isinstance(value, int):
            usage[key] = value
            usage["known_fields"].append(key)
        else:
            usage["usage_complete"] = False
    usage["requests"] = report["api_calls"]
    usage["input_total"] = usage["uncached_input"] + usage["cache_read"] + usage["cache_write"]
    if all(k in usage["known_fields"] for k in ("uncached_input", "cache_read", "cache_write")):
        usage["known_fields"].append("input_total")
    usage["models"] = [f"{report.get('provider')}/{report.get('model')}"] if report.get("model") else []
    messages = []
    for line in (session_text or "").splitlines():
        try:
            value = json.loads(line)
        except json.JSONDecodeError:
            continue
        if isinstance(value, dict):
            messages.extend(value.get("messages") or [] if "messages" in value else [value])
    if not messages:
        usage["usage_complete"] = False
    for message in messages:
        if not isinstance(message, dict):
            continue
        for call in message.get("tool_calls") or [] if message.get("role") == "assistant" else []:
            name = ((call or {}).get("function") or {}).get("name") or "?"
            count_tool(usage, name)
            usage["delegations"] += name == "delegate_task"
    # The export has no structured tool-error flag: unknown, never a measured zero.
    usage["tool_errors"] = None
    if usage["requests"] == 0 or usage["delegations"]:
        usage["known_fields"] = []
        # Delegated subagents run as separate sessions whose usage this report does not prove.
        usage["usage_complete"] = False
    return None, usage


def parse_text(stdout):
    usage = empty_usage()
    usage["usage_complete"] = False
    return stdout, usage


PARSERS = {"dial-json": parse_dial_json, "omp-json": parse_omp_json, "claude-json": parse_claude_json,
           "text": parse_text, "hermes": parse_text}


def add_usage(total, part, agent):
    before = set(total.get("known_fields", MEASUREMENT_FIELDS if total["usage_complete"] else []))
    known = set(part.get("known_fields", MEASUREMENT_FIELDS if part["usage_complete"] else []))
    total["known_fields"] = sorted(before & known)
    for field in (*TOKEN_FIELDS, "requests", "tool_calls", "tool_errors"):
        total[field] = (total[field] + part[field]
                        if total[field] is not None and part[field] is not None else None)
    for name, count in part["tools"].items():
        total["tools"][name] = total["tools"].get(name, 0) + count
    delegations = sum(part["tools"].get(name, 0) for name in agent["delegation_tools"])
    total["delegations"] += delegations
    total["models"] = sorted(set(total["models"]) | set(part["models"]))
    # Usage of delegated workers counts only when the agent is known to report it in this output.
    total["usage_complete"] = (total["usage_complete"] and part["usage_complete"]
                               and not (delegations and agent["delegated_usage"] != "included"))
    if delegations and agent["delegated_usage"] != "included":
        total["known_fields"] = []


def run_agent_process(command, *, timeout, cwd, env, stdin, cancel=None, **_):
    """Stop the entire agent process group before a caller snapshots its files."""
    process = subprocess.Popen(command, cwd=cwd, env=env, stdin=stdin,
                               stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                               text=True, start_new_session=True)
    deadline = time.monotonic() + timeout
    try:
        try:
            while True:
                remaining = deadline - time.monotonic()
                if remaining <= 0 or (cancel is not None and cancel.is_set()):
                    raise subprocess.TimeoutExpired(command, timeout)
                try:
                    stdout, stderr = process.communicate(timeout=min(remaining, .2) if cancel is not None else remaining)
                    break
                except subprocess.TimeoutExpired:
                    if cancel is None:
                        raise
        except subprocess.TimeoutExpired:
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            stdout, stderr = process.communicate()
            raise subprocess.TimeoutExpired(command, timeout, output=stdout, stderr=stderr)
        return subprocess.CompletedProcess(command, process.returncode, stdout, stderr)
    finally:
        # Background descendants must not outlive a successful CLI exit either.
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        process.wait()


def run_trial(agent, case, suite_root, repetition, work_root, values, timeout, keep, logs=None,
              prepare_workspace=None, *, hard_timeout=None, capture_traces=False, cancel=None):
    values = agent_values(agent, values)
    trial_dir = Path(tempfile.mkdtemp(prefix=f"{agent['name']}-{case['id']}-r{repetition}-", dir=work_root))
    workspace = trial_dir / "project"
    source = suite_root / case["project"] if case.get("project") else None
    if source is not None:
        shutil.copytree(source, workspace, ignore=shutil.ignore_patterns("__pycache__", ".git"))
    else:
        workspace.mkdir()
    if prepare_workspace is not None:
        prepare_workspace(workspace)
    log_dir = None
    if logs is not None:
        log_dir = logs / f"{agent['name']}-{case['id']}-r{repetition}"
        log_dir.mkdir(parents=True, exist_ok=True, mode=0o700)
    environment = agent_environment(agent, {**values, "work_root": str(work_root), "trial_root": str(trial_dir)})
    prompts = [first_prompt(case)] + [turn["prompt"] for turn in case.get("turns", [])]
    usage = empty_usage()
    # Session evidence is diagnostic unless the case explicitly opts into trace grading.
    from registry_discovery import capture_trial_trace, discovery_mode, grade_trial_session
    discovery = discovery_mode(case)
    final_session_id = None
    turns = []
    final_text = None
    process_timeout = timeout + 30 if hard_timeout is None else hard_timeout
    agent_error = None
    started = time.monotonic()
    trace_capture_secs = 0
    for index, prompt in enumerate(prompts):
        if cancel is not None and cancel.is_set():
            agent_error = "cancelled"
            usage["usage_complete"] = False
            usage["known_fields"] = []
            break
        template = agent["first"] if index == 0 else agent["follow_up"]
        turn_values = {**values, "prompt": prompt, "timeout": str(timeout),
                       "usage_file": str(trial_dir / f"usage-{index}.json")}
        command = render(template, turn_values)
        stderr_path = str(log_dir / f"turn-{index}.stderr") if log_dir is not None else None
        turn_started = time.monotonic()
        try:
            process = run_agent_process(command, cwd=workspace, capture_output=True, text=True, env=environment,
                                        stdin=subprocess.DEVNULL, timeout=process_timeout, cancel=cancel)
        except subprocess.TimeoutExpired as error:
            stdout, stderr = as_text(error.stdout), as_text(error.stderr)
            if log_dir is not None:
                (log_dir / f"turn-{index}.stdout").write_text(stdout)
                (log_dir / f"turn-{index}.stderr").write_text(stderr)
            usage["usage_complete"] = False
            usage["known_fields"] = []
            agent_error = f"turn {index} exceeded {process_timeout}s"
            turns.append({"exit": None, "elapsed_ms": round((time.monotonic() - turn_started) * 1000),
                          "efficiency": None, "stderr_log": stderr_path, "stderr_tail": stderr_excerpt(stderr)})
            if capture_traces:
                turns[-1]["trace"] = {"status": "unavailable", "reason": "turn_timeout_no_terminal_session"}
            break
        except OSError as error:
            usage["usage_complete"] = False
            usage["known_fields"] = []
            agent_error = f"turn {index} could not start: {error}"
            turns.append({"exit": None, "elapsed_ms": round((time.monotonic() - turn_started) * 1000),
                          "efficiency": None, "stderr_log": None, "stderr_tail": ""})
            if capture_traces:
                turns[-1]["trace"] = {"status": "unavailable", "reason": "agent_start_failed"}
            break
        if log_dir is not None:
            (log_dir / f"turn-{index}.stdout").write_text(process.stdout)
            (log_dir / f"turn-{index}.stderr").write_text(process.stderr)
        if agent["output"] == "hermes":
            usage_path = Path(turn_values["usage_file"])
            _, part = parse_hermes_usage(usage_path.read_text() if usage_path.is_file() else "")
            text = process.stdout
            if log_dir is not None and usage_path.is_file():
                shutil.copyfile(usage_path, log_dir / f"turn-{index}.usage.json")
        else:
            text, part = PARSERS[agent["output"]](process.stdout)
        add_usage(usage, part, agent)
        final_text = text
        final_session_id = None
        if (discovery or capture_traces) and agent["output"] == "dial-json":
            for line in process.stdout.splitlines():
                try:
                    event = json.loads(line)
                except ValueError:
                    continue
                if isinstance(event, dict) and event.get("type") == "result":
                    final_session_id = event.get("session_id")
        # A non-zero exit keeps its exit code, the stderr log path and a redacted excerpt. Successful
        # turns carry no stderr excerpt, so routine warnings never reach the report.
        turns.append({"exit": process.returncode,
                      "elapsed_ms": round((time.monotonic() - turn_started) * 1000),
                      "efficiency": part.get("efficiency"),
                      "usage": {k: v for k, v in part.items() if k != "efficiency"},
                      "stderr_log": stderr_path,
                      "stderr_tail": stderr_excerpt(process.stderr) if process.returncode else "",
                      # Some CLIs (Claude Code) report setup failures only in their final result text.
                      "result_tail": stderr_excerpt(text or "") if process.returncode else ""})
        if capture_traces:
            capture_started = time.monotonic()
            turns[-1]["trace"] = capture_trial_trace(
                agent, environment, final_session_id, trial_dir, log_dir, index)
            capture_secs = time.monotonic() - capture_started
            trace_capture_secs += capture_secs
            turns[-1]["trace"]["capture_ms"] = round(capture_secs * 1000)
        if process.returncode != 0:
            agent_error = f"turn {index} exited {process.returncode}"
            break
    elapsed_ms = round((time.monotonic() - started - trace_capture_secs) * 1000)
    checks = []
    for index, check in enumerate(case.get("checks", [])):
        if not portable(check):
            checks.append({"index": index, "kind": check["kind"], "result": "not_portable"})
            continue
        outcome = run_check(check, workspace, final_text, cancel=cancel)
        checks.append({"index": index, "kind": check["kind"],
                       "result": {True: "passed", False: "failed", None: "unavailable"}[outcome]})
    trace_check = None
    if discovery:
        trace_check = grade_trial_session(agent, case, environment, final_session_id, trial_dir, log_dir)
        checks.append({"index": len(checks), "kind": "registry_discovery_trace",
                       "result": trace_check["result"]})
    scored = [c for c in checks if c["result"] != "not_portable"]
    record = {
        "agent": agent["name"], "case": case["id"], "repetition": repetition,
        "categories": case.get("categories", []),
        "passed": bool(scored) and agent_error is None and all(c["result"] == "passed" for c in scored),
        "scored": bool(scored),
        "agent_error": agent_error, "elapsed_ms": elapsed_ms, "turns": turns,
        "efficiency": aggregate_efficiency(turns, failed=agent_error is not None),
        "checks": checks, "usage": usage,
        "skipped_compactions": sum(1 for turn in case.get("turns", []) if turn.get("compact_before")),
    }
    if agent_error:
        record["error_kind"] = failure_kind("\n".join(t["stderr_tail"] + "\n" + t.get("result_tail", "")
                                                      for t in turns), agent_error)
    if trace_check is not None:
        record["discovery_trace"] = trace_check
    if keep:
        record["workspace"] = str(workspace)
    else:
        shutil.rmtree(trial_dir, ignore_errors=True)
    return record


def median(values):
    return statistics.median(values) if values else None


def percentile95(values):
    """Nearest-rank p95; small samples report their maximum, not an interpolated estimate."""
    ordered = sorted(values)
    return ordered[(95 * len(ordered) + 99) // 100 - 1] if ordered else None


def failure_kind(stderr, error):
    """Classify only explicit setup failures; task/tool failures must not disable later cases."""
    message = (stderr + "\n" + error).lower()
    if any(s in message for s in ("no api key found", "missing api key", "invalid api key", "authentication_error",
                                  "not authenticated", "not logged in", "http 401", "failed to authenticate",
                                  "oauth session expired", "no anthropic credentials found")):
        return "authentication"
    if (("model" in message and any(s in message for s in ("was not found", "model_not_found", "unknown model")))
            or "no model configured" in message):
        return "model"
    if any(s in message for s in ("unexpected argument", "unknown option", "unrecognized arguments", "could not start")):
        return "startup"
    return "execution"


def measurement(trials, field):
    values = [t["usage"][field] for t in trials if field in t["usage"].get(
        "known_fields", MEASUREMENT_FIELDS if t["usage"]["usage_complete"] else [])
        and t["usage"].get(field) is not None]
    return {"total": sum(values) if values else None, "median": median(values),
            "known_trials": len(values), "trials": len(trials)}


def summarize(trials, agents):
    summary = {}
    for agent in agents:
        own = [t for t in trials if t["agent"] == agent["name"] and t["scored"]]
        passed = [t for t in own if t["passed"]]
        complete = [t for t in own if t["usage"]["usage_complete"]]
        summary[agent["name"]] = {
            "scored_trials": len(own),
            "passed_trials": len(passed),
            "pass_rate": len(passed) / len(own) if own else None,
            "agent_errors": sum(1 for t in own if t["agent_error"]),
            "executed_trials": sum(t["agent_error"] is None for t in own),
            "failed_check_trials": sum(t["agent_error"] is None and not t["passed"] for t in own),
            "total_elapsed_ms_all": sum(t["elapsed_ms"] for t in own),
            "measurements": {field: measurement(own, field) for field in MEASUREMENT_FIELDS},
            "median_elapsed_ms_all": median([t["elapsed_ms"] for t in own]),
            "median_elapsed_ms_passed": median([t["elapsed_ms"] for t in passed]),
            "p95_elapsed_ms_all": percentile95([t["elapsed_ms"] for t in own]),
            # Amortize failed attempts too; successful-only medians hide their real cost.
            "elapsed_ms_per_success": sum(t["elapsed_ms"] for t in own) / len(passed) if passed else None,
            "tokens_per_success": (sum(t["usage"]["input_total"] + t["usage"]["output"] for t in own)
                                   / len(passed) if passed and len(complete) == len(own) else None),
            "usage_complete_trials": len(complete),
            "median_input_total_tokens": median([t["usage"]["input_total"] for t in complete]),
            "median_uncached_input_tokens": median([t["usage"]["uncached_input"] for t in complete]),
            "median_output_tokens": median([t["usage"]["output"] for t in complete]),
            "median_requests": median([t["usage"]["requests"] for t in complete]),
            "total_tool_calls": sum(t["usage"]["tool_calls"] for t in own),
            "total_tool_errors": (sum(t["usage"]["tool_errors"] for t in own)
                                  if all(t["usage"]["tool_errors"] is not None for t in own) else None),
            "total_delegations": sum(t["usage"]["delegations"] for t in own),
            "tool_calls_by_name": dict(sorted(sum(
                (collections.Counter(t["usage"]["tools"]) for t in own), collections.Counter()).items())),
            "observed_models": sorted({m for t in own for m in t["usage"]["models"]}),
            # Trials whose every required Dial efficiency field was reported; others are unknown, not zero.
            "efficiency_known_trials": sum(1 for t in own if efficiency_known(t)),
            "efficiency_projection_totals": projection_totals(own),
        }
    by_case = {}
    for trial in trials:
        entry = by_case.setdefault(trial["case"], {}).setdefault(trial["agent"], {"passed": 0, "trials": 0})
        entry["trials"] += 1
        entry["passed"] += trial["passed"]
    return summary, by_case


def table(summary, by_case, agents):
    names = [agent["name"] for agent in agents]
    lines = ["case".ljust(36) + "".join(name.rjust(12) for name in names)]
    for case, results in by_case.items():
        cells = [f"{results[n]['passed']}/{results[n]['trials']}" if n in results else "-" for n in names]
        lines.append(case.ljust(36) + "".join(cell.rjust(12) for cell in cells))
    rows = [("pass rate", "pass_rate", lambda v: f"{v:.0%}"),
            ("agent errors", "agent_errors", str),
            ("median wall s (all)", "median_elapsed_ms_all", lambda v: f"{v / 1000:.1f}"),
            ("median wall s (passed)", "median_elapsed_ms_passed", lambda v: f"{v / 1000:.1f}"),
            ("p95 wall s (all)", "p95_elapsed_ms_all", lambda v: f"{v / 1000:.1f}"),
            ("wall s / success (all attempts)", "elapsed_ms_per_success", lambda v: f"{v / 1000:.1f}"),
            ("tokens / success (all attempts)", "tokens_per_success", lambda v: f"{v:,.0f}"),
            ("median prompt tok (all)", "median_input_total_tokens", lambda v: f"{v:,.0f}"),
            ("median uncached prompt", "median_uncached_input_tokens", lambda v: f"{v:,.0f}"),
            ("median output tok", "median_output_tokens", lambda v: f"{v:,.0f}"),
            ("median requests", "median_requests", lambda v: f"{v:g}"),
            ("tool calls (total)", "total_tool_calls", str),
            ("delegated calls (total)", "total_delegations", str),
            ("tool errors (total)", "total_tool_errors", str),
            ("trials with efficiency", "efficiency_known_trials", str)]
    lines.append("")
    for label, key, fmt in rows:
        cells = [summary[n][key] for n in names]
        lines.append(label.ljust(36) + "".join(("n/a" if v is None else fmt(v)).rjust(12) for v in cells))
    return "\n".join(lines)


def version_of(agent, *, cancel=None):
    command = agent.get("version")
    if not command:
        return None
    try:
        if cancel is not None:
            return run_agent_process(command, cwd=None, env=dict(os.environ), stdin=subprocess.DEVNULL,
                                     timeout=30, cancel=cancel).stdout.strip()
        return subprocess.run(command, capture_output=True, text=True, timeout=30).stdout.strip()
    except (OSError, subprocess.TimeoutExpired) as error:
        return f"unavailable: {error}"


def binary_identity(command):
    """Path and SHA-256 of the agent executable, hashed in chunks; unknown values are None."""
    path = shutil.which(command[0]) if command else None
    if path is None:
        return {"binary_path": None, "binary_sha256": None}
    digest = hashlib.sha256()
    try:
        with open(path, "rb") as stream:
            for chunk in iter(lambda: stream.read(1 << 20), b""):
                digest.update(chunk)
    except OSError as error:
        return {"binary_path": path, "binary_sha256": None, "binary_error": str(error)}
    return {"binary_path": path, "binary_sha256": digest.hexdigest()}


def agent_identity(agent, *, cancel=None):
    """Captured before any trial, so the report states which binary and version produced its trials."""
    return {"version": version_of(agent, cancel=cancel), **binary_identity(agent["first"])}


def trial_plan(agents, cases, repetitions, seed):
    """Seeded, balanced trial order. Each (repetition, case) block lists every agent once; blocks are
    shuffled and each block rotates one seeded agent order by its position. Across the run every agent
    takes each slot equally often when the block count is a multiple of the agent count."""
    rng = random.Random(seed)
    blocks = [(repetition, case) for repetition in range(repetitions) for case in cases]
    rng.shuffle(blocks)
    base = list(agents)
    rng.shuffle(base)
    plan = []
    for position, (repetition, case) in enumerate(blocks):
        shift = position % len(base)
        plan.extend((agent, case, repetition) for agent in base[shift:] + base[:shift])
    return plan


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("suites", nargs="+", type=Path, help="benchmark suite TOML files")
    parser.add_argument("--agents", type=Path, required=True, help="agent command definitions (TOML)")
    parser.add_argument("--only", action="append", help="run only these agent names (repeatable)")
    parser.add_argument("--case", action="append", help="run only these case ids (repeatable)")
    parser.add_argument("--model", required=True, help="model id substituted for {model}")
    parser.add_argument("--thinking", default="off", help="thinking level substituted for {thinking}")
    parser.add_argument("--repetitions", type=int, default=3)
    parser.add_argument("--timeout", type=int, default=600, help="per-turn deadline in seconds")
    parser.add_argument("--jobs", type=int, default=2,
                        help="concurrent trials; use 1 for wall times that compare agents")
    parser.add_argument("--seed", type=int, default=0, help="seed for the balanced trial order")
    parser.add_argument("--root", type=Path, default=Path.cwd(), help="directory case projects are relative to")
    parser.add_argument("--report", type=Path, required=True)
    parser.add_argument("--keep-workspaces", action="store_true")
    parser.add_argument("--capture-traces", action="store_true",
                        help="retain private bounded per-turn Dial session journals from isolated trial homes")
    parser.add_argument("--comparison-mode", choices=("installed", "controlled"), default="installed",
                        help="declared setup: installed defaults or explicitly controlled configuration")
    parser.add_argument("--comparison-notes", default="",
                        help="configuration/permission differences and controls; required for controlled runs")
    args = parser.parse_args()
    try:
        if args.comparison_mode == "controlled" and not args.comparison_notes.strip():
            raise ValueError("controlled comparisons require --comparison-notes describing controls and remaining differences")
        if not 1 <= args.repetitions <= 10 or args.jobs < 1 or args.timeout < 1:
            raise ValueError("repetitions must be 1..=10; jobs and timeout must be positive")
        if args.report.exists() or args.report.with_name(args.report.stem + ".logs").exists():
            raise ValueError(f"refusing to overwrite report or logs for {args.report}")
        agents = load_agents(args.agents)
        if args.only:
            agents = [agent for agent in agents if agent["name"] in args.only]
            if len(agents) != len(set(args.only)):
                raise ValueError("unknown agent in --only")
        suites = [tomllib.loads(path.read_text()) for path in args.suites]
    except (ValueError, OSError, tomllib.TOMLDecodeError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 2
    cases = [case for suite in suites for case in suite["cases"]
             if not args.case or case["id"] in args.case]
    if not cases:
        print("error: no cases selected", file=sys.stderr)
        return 2
    ids = [case["id"] for case in cases]
    if len(set(ids)) != len(ids):
        print("error: selected suites have duplicate case ids", file=sys.stderr)
        return 2
    if args.jobs > 1:
        print("note: jobs > 1 shares the machine between trials; wall times are not comparable", file=sys.stderr)
    args.report.parent.mkdir(parents=True, exist_ok=True)
    values = {"model": args.model, "thinking": args.thinking}
    work_root = Path(tempfile.mkdtemp(prefix="dial-cross-agent-"))
    # Raw agent stdout/stderr per turn, kept beside the report for later diagnosis.
    logs = args.report.with_name(args.report.stem + ".logs")
    identities = {agent["name"]: agent_identity(agent) for agent in agents}
    plan = trial_plan(agents, cases, args.repetitions, args.seed)
    trials = []
    with concurrent.futures.ThreadPoolExecutor(max_workers=args.jobs) as pool:
        futures = [pool.submit(run_trial, agent, case, args.root, rep, work_root, values,
                               args.timeout, args.keep_workspaces, logs,
                               capture_traces=args.capture_traces) for agent, case, rep in plan]
        for future in concurrent.futures.as_completed(futures):
            trial = future.result()
            trials.append(trial)
            state = "PASS" if trial["passed"] else ("ERROR " + trial["agent_error"] if trial["agent_error"] else "FAIL")
            print(f"[{len(trials)}/{len(plan)}] {trial['agent']:>8} {trial['case']} r{trial['repetition']}: "
                  f"{state} ({trial['elapsed_ms'] / 1000:.1f}s)", file=sys.stderr, flush=True)
            failing = trial["turns"][-1] if trial["agent_error"] and trial["turns"] else None
            if failing and failing.get("stderr_log"):
                # Code and path only: the redacted excerpt stays in the report, never on the console.
                print(f"    exit={failing['exit']} stderr_log={failing['stderr_log']}", file=sys.stderr, flush=True)
    trials.sort(key=lambda t: (t["case"], t["repetition"], t["agent"]))
    summary, by_case = summarize(trials, agents)
    not_portable = sorted({f"{case['id']}#{index}:{check['kind']}" for case in cases
                           for index, check in enumerate(case.get("checks", [])) if not portable(check)})
    report = {
        "kind": "cross_agent_benchmark", "version": 1,
        "created": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "suites": [suite["name"] for suite in suites], "model": args.model, "thinking": args.thinking,
        "repetitions": args.repetitions, "timeout_secs": args.timeout, "jobs": args.jobs,
        "timing_comparable": args.jobs == 1, "seed": args.seed,
        "comparison": {"mode": args.comparison_mode, "notes": args.comparison_notes,
                       "basis": "operator declaration, not independently verified configuration equivalence"},
        "trial_order": [f"{agent['name']}/{case['id']}/r{rep}" for agent, case, rep in plan],
        "agents": [{"name": a["name"], "first": a["first"], "follow_up": a["follow_up"], "env": a["env"],
                    "delegation_tools": a["delegation_tools"], "delegated_usage": a["delegated_usage"],
                    **identities[a["name"]]} for a in agents],
        "logs": str(logs),
        "capture_traces": args.capture_traces,
        "excluded_checks": not_portable, "summary": summary, "by_case": by_case, "trials": trials,
    }
    args.report.write_text(json.dumps(report, indent=2) + "\n")
    print(table(summary, by_case, agents))
    if not args.keep_workspaces:
        shutil.rmtree(work_root, ignore_errors=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
