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
import json
import os
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
    return agents


def agent_environment(agent, values):
    """Per-agent variables: `{work_root}` placeholders, then `~` and `$VAR` expansion."""
    environment = dict(os.environ)
    for name, value in agent["env"].items():
        expanded = os.path.expandvars(os.path.expanduser(value.format(**values)))
        environment[name] = expanded
        if name.endswith("_HOME") and value.startswith("{work_root}"):
            Path(expanded).mkdir(parents=True, exist_ok=True)
    return environment


def render(template, values):
    return [argument.format(**values) for argument in template]


def first_prompt(case):
    """Seeded context has no portable CLI form, so every agent receives it identically inline."""
    if not case.get("context_messages"):
        return case["prompt"]
    notes = "\n".join(f"- {message}" for message in case["context_messages"])
    return f"Notes from earlier in this conversation:\n{notes}\n\nRequest:\n{case['prompt']}"


def portable(check):
    return check["kind"] in PORTABLE_CHECKS


def run_check(check, workspace, final_text):
    """Return True, False, or None when the check could not be evaluated."""
    kind = check["kind"]
    if kind == "response_equals":
        return None if final_text is None else final_text.strip() == check["content"].strip()
    if kind == "command":
        try:
            result = subprocess.run(check["command"], cwd=workspace, shell=True, capture_output=True,
                                    text=True, timeout=CHECK_TIMEOUT_SECS)
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


def parse_dial_json(stdout):
    """`dial -p --output-format json`: one result object per invocation."""
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


def parse_text(stdout):
    usage = empty_usage()
    usage["usage_complete"] = False
    return stdout, usage


PARSERS = {"dial-json": parse_dial_json, "omp-json": parse_omp_json, "text": parse_text}


def add_usage(total, part, agent):
    for field in (*TOKEN_FIELDS, "requests", "tool_calls", "tool_errors"):
        total[field] += part[field]
    for name, count in part["tools"].items():
        total["tools"][name] = total["tools"].get(name, 0) + count
    delegations = sum(part["tools"].get(name, 0) for name in agent["delegation_tools"])
    total["delegations"] += delegations
    total["models"] = sorted(set(total["models"]) | set(part["models"]))
    # Usage of delegated workers counts only when the agent is known to report it in this output.
    total["usage_complete"] = (total["usage_complete"] and part["usage_complete"]
                               and not (delegations and agent["delegated_usage"] != "included"))


def run_trial(agent, case, suite_root, repetition, work_root, values, timeout, keep, logs=None):
    trial_dir = Path(tempfile.mkdtemp(prefix=f"{agent['name']}-{case['id']}-r{repetition}-", dir=work_root))
    workspace = trial_dir / "project"
    source = suite_root / case["project"] if case.get("project") else None
    if source is not None:
        shutil.copytree(source, workspace, ignore=shutil.ignore_patterns("__pycache__", ".git"))
    else:
        workspace.mkdir()
    log_dir = None
    if logs is not None:
        log_dir = logs / f"{agent['name']}-{case['id']}-r{repetition}"
        log_dir.mkdir(parents=True, exist_ok=True)
    environment = agent_environment(agent, {**values, "work_root": str(work_root)})
    prompts = [first_prompt(case)] + [turn["prompt"] for turn in case.get("turns", [])]
    usage = empty_usage()
    turns = []
    final_text = None
    agent_error = None
    started = time.monotonic()
    for index, prompt in enumerate(prompts):
        template = agent["first"] if index == 0 else agent["follow_up"]
        command = render(template, {**values, "prompt": prompt, "timeout": str(timeout)})
        turn_started = time.monotonic()
        try:
            process = subprocess.run(command, cwd=workspace, capture_output=True, text=True, env=environment,
                                     stdin=subprocess.DEVNULL, timeout=timeout + 30)
        except subprocess.TimeoutExpired as error:
            if log_dir is not None:
                for stream in ("stdout", "stderr"):
                    captured = getattr(error, stream) or b""
                    if isinstance(captured, bytes):
                        captured = captured.decode("utf-8", errors="replace")
                    (log_dir / f"turn-{index}.{stream}").write_text(captured)
            usage["usage_complete"] = False
            agent_error = f"turn {index} exceeded {timeout + 30}s"
            turns.append({"exit": None, "elapsed_ms": round((time.monotonic() - turn_started) * 1000)})
            break
        except OSError as error:
            usage["usage_complete"] = False
            agent_error = f"turn {index} could not start: {error}"
            break
        if log_dir is not None:
            (log_dir / f"turn-{index}.stdout").write_text(process.stdout)
            (log_dir / f"turn-{index}.stderr").write_text(process.stderr)
        text, part = PARSERS[agent["output"]](process.stdout)
        add_usage(usage, part, agent)
        final_text = text
        turns.append({"exit": process.returncode,
                      "elapsed_ms": round((time.monotonic() - turn_started) * 1000),
                      "stderr_tail": process.stderr[-400:] if process.returncode else ""})
        if process.returncode != 0:
            agent_error = f"turn {index} exited {process.returncode}"
            break
    elapsed_ms = round((time.monotonic() - started) * 1000)
    checks = []
    for index, check in enumerate(case.get("checks", [])):
        if not portable(check):
            checks.append({"index": index, "kind": check["kind"], "result": "not_portable"})
            continue
        outcome = run_check(check, workspace, final_text)
        checks.append({"index": index, "kind": check["kind"],
                       "result": {True: "passed", False: "failed", None: "unavailable"}[outcome]})
    scored = [c for c in checks if c["result"] != "not_portable"]
    record = {
        "agent": agent["name"], "case": case["id"], "repetition": repetition,
        "categories": case.get("categories", []),
        "passed": bool(scored) and agent_error is None and all(c["result"] == "passed" for c in scored),
        "scored": bool(scored),
        "agent_error": agent_error, "elapsed_ms": elapsed_ms, "turns": turns,
        "checks": checks, "usage": usage,
        "skipped_compactions": sum(1 for turn in case.get("turns", []) if turn.get("compact_before")),
    }
    if keep:
        record["workspace"] = str(workspace)
    else:
        shutil.rmtree(trial_dir, ignore_errors=True)
    return record


def median(values):
    return statistics.median(values) if values else None


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
            "median_elapsed_ms_all": median([t["elapsed_ms"] for t in own]),
            "median_elapsed_ms_passed": median([t["elapsed_ms"] for t in passed]),
            "usage_complete_trials": len(complete),
            "median_input_total_tokens": median([t["usage"]["input_total"] for t in complete]),
            "median_uncached_input_tokens": median([t["usage"]["uncached_input"] for t in complete]),
            "median_output_tokens": median([t["usage"]["output"] for t in complete]),
            "median_requests": median([t["usage"]["requests"] for t in complete]),
            "total_tool_calls": sum(t["usage"]["tool_calls"] for t in own),
            "total_tool_errors": sum(t["usage"]["tool_errors"] for t in own),
            "total_delegations": sum(t["usage"]["delegations"] for t in own),
            "tool_calls_by_name": dict(sorted(sum(
                (collections.Counter(t["usage"]["tools"]) for t in own), collections.Counter()).items())),
            "observed_models": sorted({m for t in own for m in t["usage"]["models"]}),
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
            ("median prompt tok (all)", "median_input_total_tokens", lambda v: f"{v:,.0f}"),
            ("median uncached prompt", "median_uncached_input_tokens", lambda v: f"{v:,.0f}"),
            ("median output tok", "median_output_tokens", lambda v: f"{v:,.0f}"),
            ("median requests", "median_requests", lambda v: f"{v:g}"),
            ("tool calls (total)", "total_tool_calls", str),
            ("delegated calls (total)", "total_delegations", str),
            ("tool errors (total)", "total_tool_errors", str)]
    lines.append("")
    for label, key, fmt in rows:
        cells = [summary[n][key] for n in names]
        lines.append(label.ljust(36) + "".join(("n/a" if v is None else fmt(v)).rjust(12) for v in cells))
    return "\n".join(lines)


def version_of(agent):
    command = agent.get("version")
    if not command:
        return None
    try:
        return subprocess.run(command, capture_output=True, text=True, timeout=30).stdout.strip()
    except (OSError, subprocess.TimeoutExpired) as error:
        return f"unavailable: {error}"


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
    parser.add_argument("--jobs", type=int, default=2, help="concurrent trials")
    parser.add_argument("--root", type=Path, default=Path.cwd(), help="directory case projects are relative to")
    parser.add_argument("--report", type=Path, required=True)
    parser.add_argument("--keep-workspaces", action="store_true")
    args = parser.parse_args()
    try:
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
    args.report.parent.mkdir(parents=True, exist_ok=True)
    values = {"model": args.model, "thinking": args.thinking}
    work_root = Path(tempfile.mkdtemp(prefix="dial-cross-agent-"))
    # Raw agent stdout/stderr per turn, kept beside the report for later diagnosis.
    logs = args.report.with_name(args.report.stem + ".logs")
    # Agents alternate within each case and repetition so drift affects them evenly.
    plan = [(agent, case, rep) for rep in range(args.repetitions) for case in cases for agent in agents]
    trials = []
    with concurrent.futures.ThreadPoolExecutor(max_workers=args.jobs) as pool:
        futures = [pool.submit(run_trial, agent, case, args.root, rep, work_root, values,
                               args.timeout, args.keep_workspaces, logs) for agent, case, rep in plan]
        for future in concurrent.futures.as_completed(futures):
            trial = future.result()
            trials.append(trial)
            state = "PASS" if trial["passed"] else ("ERROR " + trial["agent_error"] if trial["agent_error"] else "FAIL")
            print(f"[{len(trials)}/{len(plan)}] {trial['agent']:>8} {trial['case']} r{trial['repetition']}: "
                  f"{state} ({trial['elapsed_ms'] / 1000:.1f}s)", file=sys.stderr, flush=True)
    trials.sort(key=lambda t: (t["case"], t["repetition"], t["agent"]))
    summary, by_case = summarize(trials, agents)
    not_portable = sorted({f"{case['id']}#{index}:{check['kind']}" for case in cases
                           for index, check in enumerate(case.get("checks", [])) if not portable(check)})
    report = {
        "kind": "cross_agent_benchmark", "version": 1,
        "created": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "suites": [suite["name"] for suite in suites], "model": args.model, "thinking": args.thinking,
        "repetitions": args.repetitions, "timeout_secs": args.timeout, "jobs": args.jobs,
        "agents": [{"name": a["name"], "first": a["first"], "follow_up": a["follow_up"], "env": a["env"],
                    "delegation_tools": a["delegation_tools"], "delegated_usage": a["delegated_usage"],
                    "version": version_of(a)} for a in agents],
        "logs": str(logs),
        "excluded_checks": not_portable, "summary": summary, "by_case": by_case, "trials": trials,
    }
    args.report.write_text(json.dumps(report, indent=2) + "\n")
    print(table(summary, by_case, agents))
    if not args.keep_workspaces:
        shutil.rmtree(work_root, ignore_errors=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
