#!/usr/bin/env python3
"""Fresh-session A/B pilot for saved behavior -> owner -> check recall.

Targets (see `make_target`):
  fixture    synthetic `codebase-memory-ab-fixture/src` (offline and stub tests only).
  lexical    `lexical find progress` in an explicit Dial engine checkout (`--dial-engine`). Only three
             files are copied into each trial project: navigation.rs, workflow/investigation.rs and
             run/engine/evidence_tests.rs. Owner `progress_keys`; check
             `lexical_find_rounds_credit_new_evidence_without_withholding_a_paraphrased_result`.

Arms (identical prompt, identical navigate instruction, identical model arguments):
  without  fresh project and HOME; measurement turn only.
  with     fresh project and HOME; one seed turn saves the note with the real CLI. The saved finding
           is checked structurally in `dial understanding --json` (topic, summary lines, sources paths).
Every trial has its own temporary tree with HOME and DIAL_HOME inside it. Measurement turns are fresh
sessions (no --continue/--resume). Workspaces contain only the target's copied files: no README,
answer key, grader, plan or runner.

Usage accounting (real CLI only): `dial stats --json` is read at the start, after the seed turn (with
arm only) and after the measurement turn. Seed cost and measurement cost are deltas of those readings,
so warm reuse is measurable separately from the one-time seed. A delta is null (unknown) when either
reading is missing or a counter is not reported; counters never fall back to zero.

Grading is external: the answer must be exactly two strict lines naming the owner and proving test.
A trial counts as checked only with a finished turn, no recorded errors and a correct answer.
Subcommands: plan | run | grade. `run` makes paid model calls unless the dial binary is a stub.
"""
import argparse
import json
import os
import re
import shutil
import subprocess
import tempfile
from dataclasses import dataclass
from pathlib import Path

HERE = Path(__file__).resolve().parent
FIXTURE = HERE / "codebase-memory-ab-fixture"
ARMS = ("without", "with")
ANSWER_LINE_RE = re.compile(r"^(owner|test): `?([^\s`:]+):(\w+)`?$")
DIAL_TIMEOUT_S = 900
USAGE_KEYS = ("requests", "reported", "input_tokens", "output_tokens")
LEXICAL_FILES = (
    "crates/dial-tools/src/tools/code/navigation.rs",
    "crates/dial-agent/src/agent/workflow/investigation.rs",
    "crates/dial-agent/src/agent/run/engine/evidence_tests.rs",
)


@dataclass(frozen=True)
class Target:
    name: str
    topic: str
    owner_path: str
    owner_fn: str
    check_path: str
    check_fn: str
    sources: tuple
    copies: tuple  # paths relative to `root`; directories are copied recursively
    root: Path
    behavior: str


def make_target(name, dial_engine=None):
    if name == "fixture":
        return Target("fixture", "incoming request route selection",
                      "src/router/mod.rs", "route_request",
                      "src/router/tests.rs", "route_request_prefers_fresh_owner",
                      ("src/router/mod.rs", "src/router/tests.rs"), ("src",), FIXTURE,
                      "owns incoming request route selection")
    if name == "lexical":
        if dial_engine is None:
            raise ValueError("the lexical target needs --dial-engine pointing at a Dial engine checkout")
        root = Path(dial_engine).resolve()
        missing = [rel for rel in LEXICAL_FILES if not (root / rel).is_file()]
        if missing:
            raise ValueError(f"dial-engine checkout lacks: {', '.join(missing)}")
        return Target("lexical", "lexical find progress",
                      "crates/dial-agent/src/agent/workflow/investigation.rs", "progress_keys",
                      "crates/dial-agent/src/agent/run/engine/evidence_tests.rs",
                      "lexical_find_rounds_credit_new_evidence_without_withholding_a_paraphrased_result",
                      LEXICAL_FILES, LEXICAL_FILES, root, "records lexical find progress for investigations")
    raise ValueError(f"unknown target {name!r}")


def prompt_for(target):
    return (
        f"First call the codebase_memory navigate action with query `{target.topic}`. Then answer: which "
        f"function {target.behavior}, and which test proves it? Reply with exactly two lines: "
        "`owner: <project-relative path>:<function>` and `test: <project-relative path>:<test function>`. "
        "No other text."
    )


def seed_prompt_for(target, note):
    return (
        f"Save one repository finding with the codebase_memory remember action: topic `{target.topic}`, "
        f"summary exactly as below, and sources exactly as listed in its Sources line. Then reply DONE.\n{note}"
    )


def build_note(target):
    sources = ", ".join(f"`{s}`" for s in target.sources)
    return (
        f"{target.topic} is decided by `{target.owner_fn}` in `{target.owner_path}`; the proving test is "
        f"`{target.check_fn}` in `{target.check_path}`.\n"
        f"Owner: `{target.owner_fn}`\n"
        f"Check: `{target.check_fn}`\n"
        f"Sources: {sources}\n"
    )


def _declarations(text, name):
    return len(re.findall(rf"\bfn\s+{re.escape(name)}\s*[(<]", text))


def resolve_note(root, target, note):
    """Each Owner/Check identifier must be exactly one declaration across the cited sources."""
    cited = list(target.sources)
    texts = {}
    for rel in cited:
        path = Path(root) / rel
        if not path.is_file():
            return {"resolved": False, "reason": f"cited source missing: {rel}"}
        texts[rel] = path.read_text()
    for label, name in (("owner", target.owner_fn), ("check", target.check_fn)):
        count = sum(_declarations(text, name) for text in texts.values())
        if count != 1:
            return {"resolved": False, "reason": f"{label} `{name}` has {count} declarations"}
    return {"resolved": True, "owner": target.owner_fn, "check": target.check_fn, "cited": cited}


def strict_answer(answer):
    """Exactly two well-formed, distinct `owner:` and `test:` lines; anything else is None."""
    lines = [line.strip() for line in (answer or "").strip().splitlines()]
    if len(lines) != 2 or any(not line for line in lines):
        return None
    found = {}
    for line in lines:
        match = ANSWER_LINE_RE.match(line)
        if not match or match.group(1) in found:
            return None
        found[match.group(1)] = (match.group(2), match.group(3))
    return found if set(found) == {"owner", "test"} else None


def grade_answer(answer, target):
    found = strict_answer(answer)
    return bool(found) and found["owner"] == (target.owner_path, target.owner_fn) \
        and found["test"] == (target.check_path, target.check_fn)


def populate_workspace(target, project):
    """Copy only the target's files. Nothing else from the target root enters the workspace."""
    for rel in target.copies:
        source, dest = Path(target.root, rel), Path(project, rel)
        if source.is_dir():
            shutil.copytree(source, dest)
        else:
            dest.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(source, dest)


def plan(target):
    note = build_note(target)
    return {
        "suite": "codebase-memory-ab-pilot-v3",
        "target": target.name,
        "topic": target.topic,
        "prompt": prompt_for(target),
        "note": note,
        "note_resolution": resolve_note(target.root, target, note),
        "copies": list(target.copies),
        "arms": {arm: {"note_seeded": arm == "with"} for arm in ARMS},
        "measurement": "real CLI: dial -p --output-format json; usage deltas from dial stats --json",
    }


# ---------- usage ------------------------------------------------------------------------------

def _count_or_none(value, name):
    if value is None:
        return None
    if isinstance(value, bool) or not isinstance(value, int) or value < 0:
        raise ValueError(f"{name} must be a nonnegative integer or null, got {value!r}")
    return value


def validate_usage(usage, name):
    if not isinstance(usage, dict) or set(usage) != set(USAGE_KEYS):
        raise ValueError(f"{name} must be an object with exactly {', '.join(USAGE_KEYS)}")
    for key in USAGE_KEYS:
        _count_or_none(usage[key], f"{name}.{key}")
    if usage["requests"] is None and any(usage[k] is not None for k in USAGE_KEYS[1:]):
        raise ValueError(f"{name} has counters without a request count")
    return usage


def usage_delta(after, before, name):
    """Counter difference between two stats readings. Null when unreadable; a decrease is an error."""
    if after is None or before is None:
        return None
    delta = {}
    for key in ("requests", "reported", "input_tokens", "output_tokens"):
        a, b = after.get(key), before.get(key)
        if a is None or b is None:
            delta[key] = None
            continue
        if a < b:
            raise ValueError(f"{name}: stats counter {key} decreased; readings are not from one HOME")
        delta[key] = a - b
    # Tokens are attributable only when every request in the window reported usage.
    if delta["requests"] is None or delta["reported"] != delta["requests"]:
        delta["input_tokens"] = None
        delta["output_tokens"] = None
    return delta


def _sum_or_none(values):
    if any(v is None for v in values):
        return None
    return sum(values)


def _stats_snapshot(stats_json):
    """Four stats counters from `dial stats --json` `overall`; None when unparseable or absent."""
    try:
        overall = json.loads(stats_json)["overall"]
        snap = {"requests": overall["requests"], "reported": overall["reported"],
                "input_tokens": overall["input"], "output_tokens": overall["output"]}
        for key in USAGE_KEYS:
            _count_or_none(snap[key], key)
        return snap
    except (ValueError, KeyError, TypeError):
        return None


# ---------- validation and summary ---------------------------------------------------------

def validate_trials(records):
    if not isinstance(records, list) or not records:
        raise ValueError("report needs a non-empty trials list")
    seen, trials_by_arm = set(), {arm: set() for arm in ARMS}
    for record in records:
        if not isinstance(record, dict):
            raise ValueError("every trial must be an object")
        arm, trial = record.get("arm"), record.get("trial")
        if arm not in ARMS:
            raise ValueError(f"unknown arm {arm!r}")
        if isinstance(trial, bool) or not isinstance(trial, int) or trial < 1:
            raise ValueError(f"trial must be a positive integer, got {trial!r}")
        if (arm, trial) in seen:
            raise ValueError(f"duplicate trial {arm}/{trial}")
        seen.add((arm, trial))
        trials_by_arm[arm].add(trial)
        name = f"{arm}/{trial}"
        if record.get("note_seeded") != (arm == "with"):
            raise ValueError(f"{name} has inconsistent note_seeded")
        if not isinstance(record.get("answer"), str) or not isinstance(record.get("turn_ok"), bool):
            raise ValueError(f"{name} needs a string answer and boolean turn_ok")
        if not isinstance(record.get("errors"), list):
            raise ValueError(f"{name} errors must be a list")
        if arm == "with":
            if not isinstance(record.get("seed"), dict):
                raise ValueError(f"{name} with-arm trial needs seed usage")
            validate_usage(record["seed"].get("usage"), f"{name} seed.usage")
        elif record.get("seed") is not None:
            raise ValueError(f"{name} without-arm trial must not have seed usage")
        validate_usage(record.get("measurement"), f"{name} measurement")
    if trials_by_arm["without"] != trials_by_arm["with"]:
        raise ValueError("both arms must have the same trial numbers")
    return trials_by_arm


def is_checked(record, target):
    return record["turn_ok"] and not record["errors"] and grade_answer(record["answer"], target)


def summarise(records, target):
    trials_by_arm = validate_trials(records)
    out = {}
    for arm in ARMS:
        rows = [r for r in records if r["arm"] == arm]
        checked = [r for r in rows if is_checked(r, target)]
        n = len(checked)
        meas = [r["measurement"] for r in rows]
        meas_requests = _sum_or_none([m["requests"] for m in meas])
        meas_input = _sum_or_none([m["input_tokens"] for m in meas])
        entry = {
            "trials": len(trials_by_arm[arm]),
            "checked": n,
            "measurement_requests_all_trials": meas_requests,
            "measurement_requests_per_checked": meas_requests / n if meas_requests is not None and n else None,
            "measurement_input_tokens_all_trials": meas_input,
            "measurement_input_tokens_per_checked": meas_input / n if meas_input is not None and n else None,
        }
        if arm == "with":
            seeds = [r["seed"]["usage"] for r in rows]
            seed_requests = _sum_or_none([s["requests"] for s in seeds])
            seed_input = _sum_or_none([s["input_tokens"] for s in seeds])
            entry.update({
                "seed_requests_all_trials": seed_requests,
                "seed_input_tokens_all_trials": seed_input,
                "seed_requests_per_checked": seed_requests / n if seed_requests is not None and n else None,
                "total_requests_per_checked": (seed_requests + meas_requests) / n
                if seed_requests is not None and meas_requests is not None and n else None,
                "total_input_tokens_per_checked": (seed_input + meas_input) / n
                if seed_input is not None and meas_input is not None and n else None,
            })
        else:
            entry.update({"seed_requests_all_trials": None, "seed_input_tokens_all_trials": None})
        out[arm] = entry
    return out


# ---------- live / stub execution -------------------------------------------------------------

def _dial_result(stdout):
    for line in reversed(stdout.splitlines()):
        line = line.strip()
        if line.startswith("{"):
            try:
                value = json.loads(line)
            except ValueError:
                continue
            if isinstance(value, dict) and value.get("type") == "result":
                return value
    return None


def trial_env(home):
    """HOME and DIAL_HOME both inside the trial tree; nothing inherited from the user's Dial state."""
    env = {k: v for k, v in os.environ.items() if k not in ("HOME", "DIAL_HOME")}
    env["HOME"] = str(home)
    env["DIAL_HOME"] = str(home / ".dial")
    return env


def _dial(dial, args, project, home, timeout):
    done = subprocess.run([dial, *args], cwd=project, env=trial_env(home), capture_output=True,
                          text=True, timeout=timeout, check=False)
    return done.returncode, done.stdout


def note_state(understanding, target, note):
    """Structural check of `dial understanding --json` findings for this target's topic.

    saved: a finding with exactly this topic whose summary has the exact Owner and Check lines and whose
    structured `sources` paths equal the cited set. partial: a same-topic finding that has the owner or
    check line but not all of this. absent: no same-topic finding carries either line."""
    cited = set(target.sources)
    owner_line, check_line = f"Owner: `{target.owner_fn}`", f"Check: `{target.check_fn}`"
    partial = False
    for item in understanding.get("findings") or []:
        if not isinstance(item, dict) or item.get("topic") != target.topic:
            continue
        summary = item.get("summary") if isinstance(item.get("summary"), str) else ""
        lines = {line.strip() for line in summary.splitlines()}
        sources = item.get("sources")
        paths = {s.get("path") for s in sources if isinstance(s, dict)} if isinstance(sources, list) else set()
        if owner_line in lines and check_line in lines and paths == cited:
            return "saved"
        if owner_line in lines or check_line in lines:
            partial = True
    return "partial" if partial else "absent"


def run_trial(dial, target, arm, trial, model_args, approval="ask", timeout=DIAL_TIMEOUT_S):
    """One fresh trial in its own temporary tree. Never reuses another trial's project or HOME."""
    note = build_note(target)
    record = {"arm": arm, "trial": trial, "note_seeded": arm == "with", "answer": "", "turn_ok": False,
              "seed": None, "note_state": None, "measurement": None, "errors": []}
    with tempfile.TemporaryDirectory(prefix=f"cmab-{arm}-{trial}-") as tmp:
        project, home = Path(tmp, "project"), Path(tmp, "home")
        project.mkdir()
        home.mkdir()
        populate_workspace(target, project)
        start = _stats_snapshot(_dial(dial, ["stats", "--json", "-C", str(project)], project, home, timeout)[1])
        if start is None:
            record["errors"].append("stats --json unreadable before the trial")
        before_measure = start
        if arm == "with":
            code, out = _dial(dial, ["-p", "--output-format", "json", "--approval", approval, *model_args,
                                     "--", seed_prompt_for(target, note)], project, home, timeout)
            seed = _dial_result(out)
            if seed is None or not seed.get("ok"):
                record["errors"].append(f"seed turn did not finish ok (exit {code})")
            before_measure = _stats_snapshot(_dial(dial, ["stats", "--json", "-C", str(project)],
                                                   project, home, timeout)[1])
            record["seed"] = {"usage": usage_delta(before_measure, start, "seed") if start and before_measure
                              else _null_usage(), "ok": seed is not None and bool(seed.get("ok"))}
        code, out = _dial(dial, ["understanding", "--json", "-C", str(project)], project, home, timeout)
        try:
            record["note_state"] = note_state(json.loads(out), target, note)
        except ValueError:
            record["errors"].append("understanding --json was not parseable")
        expected = "saved" if arm == "with" else "absent"
        if record["note_state"] != expected:
            record["errors"].append(f"note state {record['note_state']} but arm expects {expected}")
        code, out = _dial(dial, ["-p", "--output-format", "json", "--approval", approval, *model_args,
                                 "--", prompt_for(target)], project, home, timeout)
        result = _dial_result(out)
        if result is None:
            record["errors"].append(f"measurement turn produced no result (exit {code})")
        else:
            record["turn_ok"] = bool(result.get("ok"))
            record["answer"] = result.get("text") or ""
        after = _stats_snapshot(_dial(dial, ["stats", "--json", "-C", str(project)], project, home, timeout)[1])
        if after is None:
            record["errors"].append("stats --json unreadable after the measurement turn")
        try:
            record["measurement"] = usage_delta(after, before_measure, "measurement") if after and before_measure \
                else _null_usage()
        except ValueError as exc:
            record["errors"].append(str(exc))
            record["measurement"] = _null_usage()
    return record


def _null_usage():
    return {key: None for key in USAGE_KEYS}


def run_suite(dial, target, trials, model_args, approval="ask"):
    records = []
    for trial in range(1, trials + 1):
        for arm in ARMS:
            records.append(run_trial(dial, target, arm, trial, model_args, approval=approval))
    return {"suite": "codebase-memory-ab-pilot-v3", "target": target.name, "trials": records}


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="command", required=True)
    p_plan = sub.add_parser("plan")
    p_plan.add_argument("--target", choices=("fixture", "lexical"), default="lexical")
    p_plan.add_argument("--dial-engine", type=Path)
    p_plan.add_argument("--out", type=Path)
    p_run = sub.add_parser("run")
    p_run.add_argument("--dial", required=True, help="dial binary (or a stub for offline checks)")
    p_run.add_argument("--target", choices=("fixture", "lexical"), default="lexical")
    p_run.add_argument("--dial-engine", type=Path, help="Dial engine checkout supplying the lexical files")
    p_run.add_argument("--trials", type=int, default=1)
    p_run.add_argument("--approval", default="ask")
    p_run.add_argument("--model-arg", action="append", default=[], help="extra argument for both arms")
    p_run.add_argument("--out", type=Path, required=True)
    p_grade = sub.add_parser("grade")
    p_grade.add_argument("report", type=Path)
    p_grade.add_argument("--target", choices=("fixture", "lexical"), default="lexical")
    p_grade.add_argument("--dial-engine", type=Path)
    args = parser.parse_args(argv)
    try:
        if args.command == "plan":
            text = json.dumps(plan(make_target(args.target, args.dial_engine)), indent=2)
            if args.out:
                args.out.write_text(text + "\n")
            else:
                print(text)
            return 0
        target = make_target(args.target, getattr(args, "dial_engine", None))
        if args.command == "run":
            if args.trials < 1:
                parser.error("--trials must be at least 1")
            report = run_suite(args.dial, target, args.trials, args.model_arg, approval=args.approval)
            args.out.write_text(json.dumps(report, indent=2) + "\n")
            print(json.dumps(summarise(report["trials"], target), indent=2))
            return 0
        report = json.loads(args.report.read_text())
        print(json.dumps(summarise(report["trials"], target), indent=2))
        return 0
    except (OSError, ValueError, KeyError, TypeError, subprocess.SubprocessError) as exc:
        parser.error(str(exc))


if __name__ == "__main__":
    raise SystemExit(main())
