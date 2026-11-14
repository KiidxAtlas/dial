#!/usr/bin/env python3
"""Evidence-backed tool audit orchestration; live execution requires explicit opt-in."""
import argparse
import hashlib
import json
import re
import os
import signal
import subprocess
import sys
import time
from pathlib import Path

from tool_effectiveness import summarize_tools

ROOT = Path(__file__).resolve().parents[2]
MANIFEST = Path("examples/benchmarks/tool-audit.json")
DIMENSIONS = ("contract", "effects", "permissions", "recovery", "cancellation",
              "concurrency", "presentation", "behavior")
TIERS = ("offline", "rust", "platform", "live")
EFFICIENCY_LIMITS = {"tool_calls_median": 1.10, "tool_errors_median": 1.0,
                     "tool_contract_errors_median": 1.0, "observed_tokens_median": 1.10,
                     "elapsed_ms_median": 1.25}


def require(condition, message):
    if not condition:
        raise ValueError(message)


def local(root, name):
    require(isinstance(name, str) and name and not Path(name).is_absolute(),
            f"expected repository-relative path: {name!r}")
    path = (root / name).resolve()
    require(path.is_relative_to(root.resolve()), f"path escapes repository: {name}")
    return path


def parse_json(text):
    def unique(pairs):
        result = {}
        for key, value in pairs:
            require(key not in result, f"duplicate JSON key: {key}")
            result[key] = value
        return result
    return json.loads(text, object_pairs_hook=unique)


def read_json(path):
    return parse_json(path.read_text())


def digest(value):
    return hashlib.sha256(json.dumps(value, sort_keys=True, separators=(",", ":")).encode()).hexdigest()


def declarations(block, pattern):
    block = re.sub(r"//[^\n]*", "", block)
    names = re.findall(pattern, block)
    require(re.sub(pattern, "", block).replace(",", "").strip() == "", "unparsed tool declaration; update roster reader")
    return names


def roster(root):
    """Read the owning configuration declarations, not a second handwritten name list.

    These are configured tool families (including compatibility aliases), not a claim
    to enumerate tools dynamically supplied by MCP, plugins or debug adapters.
    Fail closed when their declaration format changes.
    """
    text = local(root, "crates/dial-llm/src/config/tools.rs").read_text()
    names = []
    for constant in ("BUILTIN", "EXTRA_TOOLS"):
        block = re.search(r"pub const " + constant + r": &\[&str\] = &\[(.*?)\];", text, re.S)
        require(block is not None, f"cannot discover {constant}; update roster reader")
        names.extend(declarations(block[1], r'"([a-z_]+)"'))
    text = local(root, "crates/dial-llm/src/config/tools/integrations.rs").read_text()
    block = re.search(r"\nintegrations!\s*\{(.*?)\n\}", text, re.S)
    require(block is not None, "cannot discover integrations; update roster reader")
    integrations = declarations(block[1], r'\(\s*"([a-z_]+)"\s*,\s*(?:true|false)\s*,\s*(?:true|false)\s*\)')
    require(bool(integrations), "empty integration inventory")
    modes = local(root, "crates/dial-agent/src/agent/run/tool_exposure.rs")
    if modes.exists():
        block = re.search(r"fn mode_tools\(&self\) -> Vec<String>\s*\{\s*\[(.*?)\]", modes.read_text(), re.S)
        require(block is not None, "cannot discover mode tools; update roster reader")
        names.extend(declarations(block[1], r'"([a-z_]+)"'))
    names.extend(integrations)
    require(len(names) == len(set(names)), "duplicate declared tool family")
    return sorted(names)


def snapshot(root):
    """Hash build/test inputs, including untracked source, without reading private state."""
    paths = []
    for scope in ("crates", "src", "tests", "python", "runtimes", "assets", "examples/benchmarks", "scripts", ".github", ".config"):
        base = root / scope
        if base.exists():
            paths.extend(p for p in base.rglob("*") if p.is_file()
                         and not p.is_symlink()
                         and not any(part in {"__pycache__", ".pytest_cache", "node_modules", "target", ".git"}
                                     for part in p.relative_to(base).parts)
                         and p.suffix not in {".pyc", ".pyo"})
    paths.extend(root / name for name in ("Cargo.toml", "Cargo.lock", "build.rs", "Makefile", "rust-toolchain.toml")
                 if (root / name).is_file())
    return digest({p.relative_to(root).as_posix(): hashlib.sha256(p.read_bytes()).hexdigest()
                   for p in sorted(set(paths))})


def validate(root, manifest):
    require(manifest.get("version") == 1, "unsupported audit version")
    groups = manifest.get("groups")
    checks = manifest.get("checks")
    require(isinstance(groups, list) and groups, "groups must be nonempty")
    require(isinstance(checks, dict) and checks, "checks must be nonempty")
    names, ids = [], set()
    for group in groups:
        require(isinstance(group, dict), "group must be an object")
        group_id = group.get("id")
        require(isinstance(group_id, str) and re.fullmatch(r"[a-z][a-z0-9_-]*", group_id) is not None,
                f"invalid group id: {group_id!r}")
        require(group_id not in ids, f"duplicate group: {group_id}")
        ids.add(group_id)
        require(group.get("owner") and group.get("sources"), f"missing owner/sources: {group_id}")
        require(local(root, group["owner"]).is_file(), f"missing owner: {group_id}")
        require(group.get("tools"), f"empty tool group: {group_id}")
        names.extend(group["tools"])
        for source in group["sources"]:
            require(local(root, source).is_file(), f"missing source: {source}")
        require(isinstance(group.get("coverage"), dict) and set(group["coverage"]) == set(DIMENSIONS),
                f"missing/unknown dimensions: {group_id}")
        for dimension, entry in group["coverage"].items():
            require(set(entry) in ({"checks"}, {"gap"}), f"invalid coverage entry: {group['id']}/{dimension}")
            if "gap" in entry:
                require(isinstance(entry["gap"], str) and entry["gap"].strip(), "gap requires a reason")
            else:
                require(isinstance(entry["checks"], list) and entry["checks"], "empty coverage checks")
                for check in entry["checks"]:
                    require(check in checks, f"unknown check: {check}")
                    if dimension == "behavior":
                        require(checks[check].get("adapter") == "behavior" and checks[check].get("tier") == "live",
                                "behavior needs benchmark evidence, not a unit-test mapping")
                        require(set(group["tools"]) <= set(checks[check].get("tools", [])),
                                "behavior check omits group tools")
    require(len(names) == len(set(names)), "tool family assigned more than once")
    actual = set(roster(root))
    require(set(names) == actual,
            f"inventory mismatch: unowned={sorted(actual-set(names))}, retired={sorted(set(names)-actual)}")
    for key, check in checks.items():
        require(re.fullmatch(r"[a-z][a-z0-9_-]*", key) is not None, "invalid check id")
        require(check.get("tier") in TIERS, f"invalid tier: {key}")
        require(check.get("adapter") in ("cargo", "unittest", "node", "behavior"), f"invalid adapter: {key}")
        if check["adapter"] == "behavior":
            require(check["tier"] == "live", "behavior execution must be live tier")
            require(check.get("candidate") and check.get("tasks") and check.get("tools"), "behavior needs candidate/tasks/tools")
            require(set(check["tools"]) <= actual, "unknown behavior tool")
            report = local(root, check["report"])
            require(isinstance(check.get("limits", {}), dict) and set(check.get("limits", {})) <= EFFICIENCY_LIMITS.keys(), "unknown efficiency limit")
            require(all(type(v) in (int, float) and 0 < v < float("inf") for v in check.get("limits", {}).values()), "invalid efficiency limit")
            require(report.is_relative_to(root / "benchmarks"), "behavior report must live under benchmarks/")
        require(type(check.get("minimum_tests")) is int and check["minimum_tests"] > 0, f"invalid minimum_tests: {key}")
        require(type(check.get("timeout")) is int and 1 <= check["timeout"] <= 7200, f"invalid timeout: {key}")
        require(isinstance(check.get("argv"), list) and check["argv"]
                and all(isinstance(arg, str) and arg for arg in check["argv"]), f"invalid argv: {key}")
        require(check.get("sources"), f"check needs test sources: {key}")
        for source in check["sources"]:
            require(local(root, source).is_file(), f"missing test source: {source}")
        require(isinstance(check.get("inputs", check["sources"]), list) and check.get("inputs", check["sources"]), "empty implementation inputs")
        for source in check.get("inputs", check["sources"]):
            require(local(root, source).is_file(), f"missing implementation input: {source}")
    findings = manifest.get("findings", [])
    require(isinstance(findings, list), "findings must be a list")
    seen = set()
    for finding in findings:
        require(finding["id"] not in seen, "duplicate finding id")
        seen.add(finding["id"])
        require(finding["group"] in ids, "finding has unknown group")
        require(finding.get("status") in ("open", "closed"), "invalid finding status")
        for field in ("owner", "evidence", "cause", "next_action"):
            require(isinstance(finding.get(field), str) and finding[field].strip(), f"finding needs {field}")
        require(finding.get("check") in checks, "finding needs a regression check")
        if finding["status"] == "closed":
            for field in ("before", "after", "fix"):
                require(isinstance(finding.get(field), str) and finding[field].strip(), f"closed finding needs {field}")
    return manifest


def passed_count(adapter, text):
    if adapter == "cargo":
        return sum(int(n) for n in re.findall(r"test result: ok\. (\d+) passed;", text))
    if adapter == "unittest":
        matches = re.findall(r"Ran (\d+) tests? in ", text)
        skipped = sum(int(n) for n in re.findall(r"skipped=(\d+)", text))
        return max(0, sum(map(int, matches)) - skipped) if re.search(r"^OK(?:\s|$)", text, re.M) else 0
    return sum(int(n) for n in re.findall(r"(?:# |ℹ )pass (\d+)", text))


def failed_count(adapter, text):
    """Only executed test failures, never compile errors or missing runtimes."""
    if adapter == "cargo":
        return sum(int(n) for n in re.findall(r"test result: FAILED\. \d+ passed; (\d+) failed;", text))
    if adapter == "unittest":
        if "unittest.loader._FailedTest" in text or "Failed to import test module:" in text:
            return 0
        if not re.search(r"Ran [1-9]\d* tests? in ", text):
            return 0
        return sum(int(n) for n in re.findall(r"(?:failures|errors)=(\d+)", text))
    return sum(int(n) for n in re.findall(r"(?:# |ℹ )fail (\d+)", text))


def input_digest(root, check):
    return digest({name: hashlib.sha256(local(root, name).read_bytes()).hexdigest()
                   for name in check.get("inputs", check["sources"])})


def execute(root, key, check, out):
    if check["adapter"] == "behavior":
        require(not local(root, check["report"]).exists(), "refusing stale/overwritten behavior report")
    before = snapshot(root)
    code_before = input_digest(root, check)
    log_path = out / f"{key}.log"
    require(not log_path.exists(), f"refusing to overwrite {log_path}")
    started = time.time_ns()
    timed_out = False
    interrupted = False
    with log_path.open("x") as log:
        process = subprocess.Popen(check["argv"], cwd=root, stdout=log, stderr=subprocess.STDOUT,
                                   start_new_session=os.name == "posix")
        try:
            code = process.wait(timeout=check["timeout"])
        except (subprocess.TimeoutExpired, KeyboardInterrupt) as error:
            interrupted = isinstance(error, KeyboardInterrupt)
            timed_out = not interrupted
            if process.poll() is None:
                if os.name == "posix":
                    os.killpg(process.pid, signal.SIGKILL)
                else:
                    process.kill()
            code = process.wait()
    text = log_path.read_text(errors="replace")
    if check["adapter"] == "behavior":
        report = local(root, check["report"])
        if report.is_file():
            try:
                payload = read_json(report)
            except ValueError:
                payload = None
            if payload is not None:
                with log_path.open("a") as log:
                    log.write("\nTOOL_AUDIT_BEHAVIOR=" + json.dumps(payload, separators=(",", ":")) + "\n")
        text = log_path.read_text(errors="replace")
    count, failures = test_counts(check, text)
    after = snapshot(root)
    return {"check": key, "definition_sha256": digest(check), "source_sha256": before,
            "inputs_sha256": code_before, "interrupted": interrupted,
            "finished_ns": time.time_ns(), "started_ns": started, "exit_code": code,
            "timed_out": timed_out, "passed_tests": count,
            "failed_tests": failures,
            "status": "passed" if code == 0 and not timed_out and not interrupted and before == after
            and count >= check["minimum_tests"] and failures == 0 else "failed",
            "inputs_changed": before != after, "log": log_path.name,
            "log_sha256": hashlib.sha256(log_path.read_bytes()).hexdigest()}


def receipt(root, path, checks, fresh):
    data = read_json(path)
    require(data.get("kind") == "tool-audit-run" and data.get("version") == 1, "not an audit run receipt")
    result = {}
    require(isinstance(data.get("checks"), list) and data["checks"], "empty audit receipt")
    for row in data["checks"]:
        key = row["check"]
        require(key not in result and key in checks, "duplicate or unknown receipt check")
        require(row["definition_sha256"] == digest(checks[key]), f"check definition changed: {key}")
        if fresh:
            require(row["source_sha256"] == fresh and row["inputs_changed"] is False, f"stale evidence: {key}")
            if row["status"] != "unavailable":
                require(row["inputs_sha256"] == input_digest(root, checks[key]), f"changed check inputs: {key}")
        if row["status"] == "unavailable":
            require(bool(row.get("reason")), "unavailable check needs reason")
            result[key] = row
            continue
        log = local(path.parent, row["log"])
        require(hashlib.sha256(log.read_bytes()).hexdigest() == row["log_sha256"], f"changed log: {key}")
        text = log.read_text(errors="replace")
        passed, failed = test_counts(checks[key], text)
        require(row["passed_tests"] == passed, "test count mismatch")
        require(row["failed_tests"] == failed, "failure count mismatch")
        if row["status"] == "passed":
            require(type(row["exit_code"]) is int and row["exit_code"] == 0
                    and row["timed_out"] is False and row["inputs_changed"] is False
                    and row.get("interrupted", False) is False
                    and row["failed_tests"] == 0
                    and row["passed_tests"] >= checks[key]["minimum_tests"], "invalid pass receipt")
        else:
            require(row["status"] == "failed", "invalid receipt status")
        result[key] = row
    return result


def coverage(root, manifest, receipts=()):
    fresh = snapshot(root)
    evidence = {}
    for path in receipts:
        for key, row in receipt(root, path, manifest["checks"], fresh).items():
            require(key not in evidence, f"ambiguous duplicate evidence: {key}")
            evidence[key] = row
    rows = []
    for group in manifest["groups"]:
        for dimension, entry in group["coverage"].items():
            required = entry.get("checks", [])
            ok = required and all(evidence.get(key, {}).get("status") == "passed" for key in required)
            rows.append({"group": group["id"], "tools": group["tools"], "dimension": dimension,
                         "status": "passed" if ok else "gap" if "gap" in entry else "unverified",
                         "reason": entry.get("gap"), "checks": required})
    return rows


def closure(root, manifest, require_receipts=True):
    """Verify closed findings against their before/after receipts.

    Receipts are ignored local evidence. Without `require_receipts`, a closed finding whose
    receipts are absent, or whose passing receipt predates the current source, is reported as
    `closed-unverified`, never `closed`; receipts that are present are still verified in full."""
    fresh = snapshot(root)
    results = []
    for finding in manifest.get("findings", []):
        if finding["status"] != "closed":
            results.append({"id": finding["id"], "status": "open"})
            continue
        paths = [local(root, finding["before"]), local(root, finding["after"])]
        if not require_receipts and not all(path.is_file() for path in paths):
            results.append({"id": finding["id"], "status": "closed-unverified",
                            "reason": "closure receipts are local evidence absent from this checkout"})
            continue
        key = finding["check"]
        before = receipt(root, paths[0], manifest["checks"], None)[key]
        after = receipt(root, paths[1], manifest["checks"], fresh if require_receipts else None)[key]
        require(before["status"] == "failed" and type(before["exit_code"]) is int
                and before["exit_code"] > 0 and before["timed_out"] is False
                and before.get("interrupted", False) is False
                and before["inputs_changed"] is False and before["failed_tests"] > 0,
                f"no observed failing repro: {finding['id']}")
        require(after["status"] == "passed" and before["finished_ns"] < after["started_ns"],
                f"no later passing regression: {finding['id']}")
        require(before["source_sha256"] != after["source_sha256"], "closure needs changed inputs")
        require(before["inputs_sha256"] != after["inputs_sha256"], "closure needs changed implementation/test inputs, not just plan metadata")
        if after["source_sha256"] == fresh and after["inputs_changed"] is False:
            results.append({"id": finding["id"], "status": "closed"})
        else:
            results.append({"id": finding["id"], "status": "closed-unverified",
                            "reason": "the passing receipt predates the current source"})
    return results

def evaluate(payload, candidate, limits=None):
    """Reuse existing outcome/hash/repetition validation; no new grader or model call."""
    summary = summarize_tools(payload)
    require(summary["repetitions"] >= 3, "behavior evaluation needs at least 3 repetitions")
    require(candidate != summary["baseline_configuration"], "candidate must differ from baseline")
    configuration = next((c for c in summary["configurations"] if c["name"] == candidate), None)
    require(configuration is not None, "candidate configuration absent")
    require(configuration["tasks"] and all(t["checked"] for t in configuration["tasks"]),
            "behavior evaluation requires independently checked tasks")
    baseline = next(c for c in summary["configurations"] if c["name"] == summary["baseline_configuration"])
    for observed in (baseline, configuration):
        for task in observed["tasks"]:
            require(task["trace"]["trials_complete"] == task["trials"],
                    f"incomplete behavior trace: {task['id']}")
    comparison = next(c for c in summary["comparisons"] if c["name"] == candidate)
    require(bool(comparison["matched_success_tasks"]), "no matched successful tasks")
    limits = EFFICIENCY_LIMITS | (limits or {})
    before_tasks = {task["id"]: task for task in baseline["tasks"]}
    after_tasks = {task["id"]: task for task in configuration["tasks"]}
    regressions, unknown = [], []
    for task_id in comparison["matched_success_tasks"]:
        for field, limit in limits.items():
            before = before_tasks[task_id]["per_success"][field]
            after = after_tasks[task_id]["per_success"][field]
            if before is None or after is None:
                unknown.append(f"{task_id}/{field}")
            elif after > before * limit:
                regressions.append(f"{task_id}/{field}")
    failed = bool(comparison["observed_regressed_tasks"] or regressions)
    return {"kind": "tool-audit-behavior", "version": 1, "candidate": candidate,
            "status": "failed" if failed else "inconclusive" if unknown else "passed",
            "efficiency_limits": limits, "efficiency_regressions": regressions, "unknown_efficiency": unknown,
            "interpretation": "Observed nonregression only; not proof of improvement or exhaustive coverage.",
            "input_sha256": digest(payload), "summary": summary}


def test_counts(check, text):
    if check["adapter"] != "behavior":
        return passed_count(check["adapter"], text), failed_count(check["adapter"], text)
    lines = [line.split("=", 1)[1] for line in text.splitlines() if line.startswith("TOOL_AUDIT_BEHAVIOR=")]
    if len(lines) != 1:
        return 0, 0
    try:
        payload = parse_json(lines[0])
        result = evaluate(payload, check["candidate"], check.get("limits"))
        if result["status"] == "inconclusive":
            return 0, 0
        configuration = next(c for c in result["summary"]["configurations"] if c["name"] == check["candidate"])
        tasks = {task["id"]: task for task in configuration["tasks"]}
        require(set(check["tasks"]) <= tasks.keys(), "required behavior tasks absent")
        report = payload.get("machine_benchmark", payload)
        entry = next(c for c in report["configurations"] if c["configuration"]["name"] == check["candidate"])
        invoked = set()
        for case in entry["report"]["cases"]:
            if case["id"].split(":", 1)[1] in check["tasks"]:
                invoked.update(name for name, count in case["metrics"]["tool_trace"]["invoked"].items() if count > 0)
        require(set(check["tools"]) <= invoked, "required behavior tools never invoked")
        passed = sum(tasks[key]["checked_successes"] for key in check["tasks"])
        failed = sum(tasks[key]["trials"] - tasks[key]["checked_successes"] for key in check["tasks"])
        # A regression in any independently checked task blocks the whole check.
        return (passed, failed) if result["status"] == "passed" else (0, max(failed, 1))
    except (ValueError, KeyError, TypeError, StopIteration):
        return 0, 0  # Malformed/missing evidence is not a behavioral reproduction.


def save(path, data):
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("x") as output:
        json.dump(data, output, indent=2, sort_keys=True)
        output.write("\n")


def ratchet(previous, current):
    """Do not silently turn committed checks back into gaps or erase findings."""
    def index(manifest):
        return {tool: group["coverage"] for group in manifest["groups"] for tool in group["tools"]}
    old, new = index(previous), index(current)
    for tool, dimensions in old.items():
        require(tool in new, f"audit ownership removed: {tool}")
        for dimension, entry in dimensions.items():
            required = set(entry.get("checks", []))
            require(required <= set(new[tool][dimension].get("checks", [])),
                    f"coverage weakened: {tool}/{dimension}")
            for key in required:
                before, after = previous["checks"][key], current["checks"][key]
                require(before["argv"] == after["argv"] and before["adapter"] == after["adapter"]
                        and before["tier"] == after["tier"]
                        and before["minimum_tests"] <= after["minimum_tests"], f"check weakened: {key}")
                require(set(before.get("inputs", before["sources"])) <= set(after.get("inputs", after["sources"])),
                        f"implementation inputs weakened: {key}")
                if before["adapter"] == "behavior":
                    require(set(before["tools"]) <= set(after["tools"]) and set(before["tasks"]) <= set(after["tasks"]),
                            f"behavior scope weakened: {key}")
                    old_limits = EFFICIENCY_LIMITS | before.get("limits", {})
                    new_limits = EFFICIENCY_LIMITS | after.get("limits", {})
                    require(all(new_limits[field] <= value for field, value in old_limits.items()), f"efficiency budget weakened: {key}")
    findings = {f["id"]: f for f in current.get("findings", [])}
    for finding in previous.get("findings", []):
        require(finding["id"] in findings, f"finding erased: {finding['id']}")
        require(finding["status"] != "closed" or findings[finding["id"]]["status"] == "closed",
                f"closed finding reopened: {finding['id']}")


def git_baseline(root, ref, path):
    require(re.fullmatch(r"[0-9a-f]{40,64}", ref) is not None, "base ref must be a full commit hash")
    commit = subprocess.run(["git", "cat-file", "-t", ref], cwd=root, capture_output=True, text=True, check=True)
    require(commit.stdout.strip() == "commit", "base ref is not a commit")
    files = subprocess.run(["git", "ls-tree", "--name-only", ref, "--", str(path)], cwd=root,
                           capture_output=True, text=True, check=True)
    if not files.stdout.strip():
        return None  # First introduction; subsequent changes must compare against this plan.
    result = subprocess.run(["git", "show", f"{ref}:{path}"], cwd=root, capture_output=True, text=True, check=True)
    return parse_json(result.stdout)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=ROOT)
    parser.add_argument("--manifest", type=Path, default=MANIFEST)
    sub = parser.add_subparsers(dest="action", required=True)
    validation = sub.add_parser("validate")
    validation.add_argument("--base-ref")
    run = sub.add_parser("run")
    run.add_argument("--tier", choices=TIERS, default="offline")
    run.add_argument("--allow-live", action="store_true", help="Explicitly allow configured commands that may consume model/provider quota")
    run.add_argument("--check", action="append", default=[])
    run.add_argument("--out", type=Path, required=True)
    gate = sub.add_parser("gate")
    gate.add_argument("--receipt", type=Path, action="append", default=[])
    gate.add_argument("--require-complete", action="store_true")
    gate.add_argument("--group", action="append", default=[])
    gate.add_argument("--dimension", action="append", choices=DIMENSIONS, default=[])
    behavior = sub.add_parser("evaluate")
    behavior.add_argument("input", type=Path)
    behavior.add_argument("--candidate", required=True)
    behavior.add_argument("--out", type=Path, required=True)
    args = parser.parse_args(argv)
    try:
        root = args.root.resolve()
        manifest = validate(root, read_json(local(root, str(args.manifest))))
        if args.action == "validate":
            if args.base_ref:
                previous = git_baseline(root, args.base_ref, args.manifest)
                if previous is not None:
                    ratchet(previous, manifest)
            findings = closure(root, manifest, require_receipts=False)
            result = {"families": len(roster(root)), "groups": len(manifest["groups"]),
                      "checks": len(manifest["checks"]), "findings": findings, "status": "valid-plan-not-executed"}
        elif args.action == "run":
            selected = args.check or [key for key, check in manifest["checks"].items() if check["tier"] == args.tier]
            require(selected and len(selected) == len(set(selected)), "empty or duplicate check selection")
            require(all(key in manifest["checks"] for key in selected), "unknown check selected")
            require(args.tier != "live" or args.allow_live, "live execution requires --allow-live and reviewed provider configuration")
            require(all(manifest["checks"][key]["tier"] == args.tier for key in selected), "selected check belongs to another tier")
            args.out.mkdir(parents=True, exist_ok=False)
            results = []
            for key in selected:
                try:
                    row = execute(root, key, manifest["checks"][key], args.out)
                except OSError as error:
                    row = {"check": key, "status": "unavailable", "reason": type(error).__name__,
                           "definition_sha256": digest(manifest["checks"][key]),
                           "source_sha256": snapshot(root), "inputs_changed": False}
                results.append(row)
                print(f"{key}: {row['status']}", file=sys.stderr)
                if row.get("interrupted"):
                    break
            result = {"kind": "tool-audit-run", "version": 1, "checks": results}
            save(args.out / "run.json", result)
            return int(any(row["status"] != "passed" for row in results))
        elif args.action == "gate":
            rows = coverage(root, manifest, args.receipt)
            require(set(args.group) <= {g["id"] for g in manifest["groups"]}, "unknown group selected")
            rows = [r for r in rows if (not args.group or r["group"] in args.group)
                    and (not args.dimension or r["dimension"] in args.dimension)]
            findings = closure(root, manifest)
            result = {"kind": "tool-audit-coverage", "coverage": rows, "findings": findings,
                      "complete": all(row["status"] == "passed" for row in rows)
                      and all(row["status"] == "closed" for row in findings)}
            print(json.dumps(result, indent=2))
            return int(args.require_complete and not result["complete"])
        else:
            result = evaluate(read_json(args.input), args.candidate)
            save(args.out, result)
            return int(result["status"] != "passed")
        print(json.dumps(result, indent=2))
        return 0
    except (ValueError, KeyError, TypeError, OSError, subprocess.CalledProcessError) as error:
        print(f"tool audit: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
