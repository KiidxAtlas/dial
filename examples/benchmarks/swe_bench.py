#!/usr/bin/env python3
"""Local SWE-bench comparison: shared CLI execution, official Docker patch grading.

Requires swebench==4.1.0 for the legacy Verified dataset schema. Model calls happen
only in `run`; `prepare` downloads images and `grade` executes the official grader.
Raw logs and dataset answers are private evaluation artifacts, not agent inputs.
"""
import argparse
import contextlib
import hashlib
import importlib.metadata
import json
import os
from pathlib import Path
import random
import shlex
import subprocess
import sys
import tarfile
import tempfile
import uuid

import cross_agent

HARNESS_VERSION = "4.1.0"
PLATFORM = "linux/amd64"


def save(path, value):
    temporary = path.with_suffix(path.suffix + ".tmp")
    temporary.write_text(json.dumps(value, indent=2) + "\n")
    temporary.replace(path)


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def source_digest(root):
    state = hashlib.sha256()
    for path in sorted(root.rglob("*")):
        if path.is_symlink():
            payload = b"symlink\0" + os.fsencode(os.readlink(path))
        elif path.is_file():
            payload = str(path.stat().st_mode & 0o777).encode() + b"\0" + path.read_bytes()
        else:
            continue
        state.update(os.fsencode(str(path.relative_to(root))) + b"\0")
        state.update(hashlib.sha256(payload).digest())
    return state.hexdigest()


def git(workspace, *arguments, diff_exit=False):
    result = subprocess.run(
        ["git", "-c", "core.hooksPath=/dev/null", "-c", "commit.gpgsign=false", *arguments],
        cwd=workspace, env={**os.environ, "GIT_CONFIG_GLOBAL": os.devnull,
                            "GIT_CONFIG_NOSYSTEM": "1", "GIT_OPTIONAL_LOCKS": "0"},
        capture_output=True, timeout=120,
    )
    if result.returncode not in ({0, 1} if diff_exit else {0}):
        raise subprocess.CalledProcessError(result.returncode, result.args,
                                            output=result.stdout, stderr=result.stderr)
    return result.stdout


def initialize_workspace(workspace):
    git(workspace, "init", "-q")
    git(workspace, "add", "--force", "-A")
    git(workspace, "-c", "user.name=SWE-bench pilot", "-c", "user.email=pilot@localhost",
        "commit", "-qm", "Frozen task base")


def working_patch(workspace):
    # Read-only: shell sandboxes may protect .git, including its index. Diff the
    # original commit even if the agent staged or committed its repair, then append
    # nonignored untracked files using Git's own binary/mode/symlink patch renderer.
    base = git(workspace, "rev-list", "--max-parents=0", "HEAD").decode().strip()
    patch = git(workspace, "diff", "--binary", base)
    untracked = git(workspace, "ls-files", "--others", "--exclude-standard", "-z")
    for path in untracked.split(b"\0"):
        if path:
            patch += git(workspace, "diff", "--no-index", "--binary", "--", "/dev/null",
                         os.fsdecode(path), diff_exit=True)
    return patch.decode()


def select_instances(rows, count, seed):
    ordered = sorted(rows, key=lambda row: row["instance_id"])
    ids = [row["instance_id"] for row in ordered]
    if len(ids) != len(set(ids)) or not 1 <= count <= len(rows):
        raise ValueError("selection needs unique instance IDs and a valid count")
    return random.Random(seed).sample(ordered, count)


def prompt_for(row, helper):
    # Deliberately exclude hints, reference patch, test patch and acceptance metadata.
    return (
        "Resolve this repository issue. Make the smallest correct source change and verify it. "
        "Do not look up the issue, pull request or solution online, or inspect benchmark "
        "datasets, reference answers or other trials. Your repository is at the task's base "
        "commit; do not change its Git history.\n\n"
        + row["problem_statement"]
        + "\n\nThe host may not have this historical project's dependencies. Run tests in its "
        "prepared Docker environment with:\n"
        + shlex.join([sys.executable, str(Path(__file__).resolve()), "test",
                      "--helper", str(helper), "--"])
        + " python -m pytest <test paths>\n"
        "For Django use: ... python tests/runtests.py <test labels> --settings=test_sqlite. "
        "The helper applies your current Git diff, including new nonignored files, in a fresh "
        "container. It runs only tests you request, not hidden acceptance tests. "
        "Leave the final changes in the working tree."
    )


def require_harness():
    version = importlib.metadata.version("swebench")
    if version != HARNESS_VERSION:
        raise ValueError(f"requires swebench=={HARNESS_VERSION}, found {version}")
    return version


def comparison_agents(path, *, dial_sandbox_off=False):
    agents = cross_agent.load_agents(path)
    if len(agents) != 2 or {a["name"] for a in agents} != {"dial", "omp"}:
        raise ValueError("this comparison requires exactly dial and omp")
    # Keep installed authentication in place; never copy grants into trial roots.
    for agent in agents:
        for template in ("first", "follow_up"):
            if agent["name"] == "omp":
                agent[template][1:1] = ["--no-extensions", "--no-skills", "--no-rules",
                                        "--no-context-files", "--no-title", "--no-prewalk",
                                        "--plan", "anthropic/{model}"]
            elif dial_sandbox_off:
                agent[template][1:1] = ["--sandbox", "off"]
            agent[template][1:1] = ["--smol", "anthropic/{model}",
                                    "--slow", "anthropic/{model}"]
    return agents


def prepare(args):
    require_harness()
    from swebench.harness.test_spec.test_spec import make_test_spec

    rows = json.loads(args.dataset.read_text())
    manifest = json.loads(args.manifest.read_text())
    expected = [(r["instance_id"], r["base_commit"]) for r in manifest["instances"]]
    if expected != [(r["instance_id"], r["base_commit"]) for r in rows]:
        raise ValueError("dataset rows disagree with frozen selection manifest")
    args.output.mkdir(parents=True, exist_ok=False, mode=0o700)
    save(args.output / "dataset.json", rows)
    save(args.output / "manifest.json", manifest)
    agents = comparison_agents(args.agents, dial_sandbox_off=args.dial_sandbox_off)
    contract = {
        "kind": "swebench_cross_agent_pilot", "version": 1,
        "dataset_sha256": digest(args.output / "dataset.json"),
        "harness_version": HARNESS_VERSION, "platform": PLATFORM,
        "model": args.model, "thinking": "off", "timeout_secs": args.timeout,
        "seed": args.seed, "repetitions": 1, "jobs": 1,
        "runner_sha256": digest(Path(__file__)),
        "cross_agent_sha256": digest(Path(cross_agent.__file__)),
        "agents": agents,
        "identities": {a["name"]: cross_agent.agent_identity(a) for a in agents},
        "limitations": [
            "Host-native agents, Docker test helper, official Docker final grading; not a leaderboard submission.",
            "AMD64 containers may be emulated on the host; serial timings are local observations only.",
            "Dial has fresh per-trial configuration; omp retains installed configuration/authentication, "
            "with ambient extensions, skills, rules, context files and prewalk disabled. This is not "
            "a configuration-matched default-vs-default comparison.",
            "One trial per task: no repeated-trial reliability or statistical superiority claim.",
            "Host-native CLIs are not adversarially isolated from the host filesystem or network; "
            "grader artifacts live outside trial roots and are forbidden by the common prompt.",
        ],
        "images": {},
    }
    save(args.output / "contract.json", contract)
    for row in rows:
        instance = row["instance_id"]
        source = args.output / "sources" / instance
        source.mkdir(parents=True)
        image = make_test_spec(row, arch="x86_64", namespace="swebench").instance_image_key
        subprocess.run(["docker", "pull", "--platform", PLATFORM, image], check=True, timeout=1800)
        info = json.loads(subprocess.check_output(["docker", "image", "inspect", image]))[0]
        contract["images"][instance] = {"tag": image, "id": info["Id"],
                                         "digests": info.get("RepoDigests", [])}
        archive = args.output / f"{instance}.tar"
        with archive.open("wb") as stream:
            subprocess.run(["docker", "run", "--rm", "--platform", PLATFORM, image,
                            "git", "-C", "/testbed", "archive", row["base_commit"]],
                           stdout=stream, check=True, timeout=180)
        with tarfile.open(archive) as stream:
            stream.extractall(source, filter="data")
        contract["images"][instance]["source_sha256"] = source_digest(source)
        archive.unlink()
        save(args.output / "contract.json", contract)
        print(f"prepared {instance}", flush=True)
    save(args.output / "prepared.json", {"contract_sha256": digest(args.output / "contract.json")})


def load_contract(root):
    contract = json.loads((root / "contract.json").read_text())
    prepared = json.loads((root / "prepared.json").read_text())
    if prepared["contract_sha256"] != digest(root / "contract.json"):
        raise ValueError("prepared contract changed")
    if contract["dataset_sha256"] != digest(root / "dataset.json"):
        raise ValueError("frozen dataset changed")
    if contract["runner_sha256"] != digest(Path(__file__)) or contract["cross_agent_sha256"] != digest(Path(cross_agent.__file__)):
        raise ValueError("runner changed since preparation; use a new run directory")
    return contract


def run(args):
    contract = load_contract(args.output)
    agents = contract["agents"]
    for agent in agents:
        if cross_agent.agent_identity(agent) != contract["identities"][agent["name"]]:
            raise ValueError(f"{agent['name']} executable changed since preparation")
    rows = json.loads((args.output / "dataset.json").read_text())
    cases = [{"id": row["instance_id"], "project": "sources/" + row["instance_id"]} for row in rows]
    by_id = {row["instance_id"]: row for row in rows}
    work_record = args.output / "work-root.json"
    if work_record.exists():
        work = Path(json.loads(work_record.read_text())["path"])
    else:
        work = Path(tempfile.mkdtemp(prefix="swebench-work-"))
        save(work_record, {"path": str(work)})
    work = work.resolve()
    if work.is_relative_to(args.output) or args.output.is_relative_to(work):
        raise ValueError("trial work and grader artifacts must have separate roots")
    records = args.output / "trials"
    records.mkdir(exist_ok=True)
    for agent, case, repetition in cross_agent.trial_plan(agents, cases, 1, contract["seed"]):
        key = f"{agent['name']}-{case['id']}"
        record_path = records / (key + ".json")
        if record_path.exists():
            continue
        started = records / (key + ".started")
        if started.exists():
            raise ValueError(f"interrupted trial {key}; refusing an unrecorded second attempt")
        started.touch(exist_ok=False)
        # The helper descriptor has no issue/answer metadata and is outside the copied repo.
        helper = work / (key + ".helper.json")
        image = contract["images"][case["id"]]["id"]
        if source_digest(args.output / case["project"]) != contract["images"][case["id"]]["source_sha256"]:
            raise ValueError(f"source changed for {case['id']}")

        def prepare_trial(workspace):
            initialize_workspace(workspace)
            save(helper, {"workspace": str(workspace), "image": image})

        case = {**case, "prompt": prompt_for(by_id[case["id"]], helper)}
        try:
            record = cross_agent.run_trial(
                agent, case, args.output, repetition, work,
                {"model": contract["model"], "thinking": contract["thinking"]},
                contract["timeout_secs"], True, args.output / "logs", prepare_trial,
                hard_timeout=contract["timeout_secs"],
            )
        finally:
            # A killed helper cannot execute its own finally block. Clean up only this
            # trial's labeled containers before grading its patch or starting another.
            cleanup_test_containers(helper)
        workspace = Path(record["workspace"])
        prediction = {"instance_id": case["id"],
                      "model_name_or_path": agent["name"] + "-" + contract["model"],
                      "model_patch": working_patch(workspace)}
        save(records / (key + ".prediction.json"), prediction)
        save(record_path, record)
        print(f"finished {key}: agent_error={record['agent_error']}, "
              f"patch_bytes={len(prediction['model_patch'].encode())}, "
              f"elapsed={record['elapsed_ms']/1000:.1f}s", flush=True)
    for agent in agents:
        predictions = [json.loads((records / f"{agent['name']}-{case['id']}.prediction.json").read_text())
                       for case in cases]
        path = args.output / (agent["name"] + "-predictions.jsonl")
        path.write_text("".join(json.dumps(p) + "\n" for p in predictions))


def cleanup_test_containers(helper):
    label = f"dial.swebench.helper={helper.resolve()}"
    containers = subprocess.check_output(
        ["docker", "ps", "-aq", "--filter", f"label={label}"], text=True, timeout=30).split()
    if containers:
        subprocess.run(["docker", "rm", "-f", *containers], check=True,
                       capture_output=True, timeout=30)


def test(args):
    descriptor = json.loads(args.helper.read_text())
    command = args.command[1:] if args.command[:1] == ["--"] else args.command
    if not command:
        raise ValueError("a test command is required after --")
    patch = working_patch(Path(descriptor["workspace"]))
    # No host mount or socket is exposed inside the test container. Its stdin is the patch.
    script = ('set -e; source /opt/miniconda3/etc/profile.d/conda.sh; conda activate testbed; '
              'cd /testbed; cat > /tmp/swebench-candidate.patch; '
              'if [ -s /tmp/swebench-candidate.patch ]; then git apply /tmp/swebench-candidate.patch; fi; '
              'exec "$@"')
    container = "swebench-test-" + uuid.uuid4().hex
    try:
        result = subprocess.run(["docker", "run", "--rm", "-i", "--network", "none",
                                 "--name", container, "--platform", PLATFORM,
                                 "--label", f"dial.swebench.helper={args.helper.resolve()}",
                                 descriptor["image"], "/bin/bash", "-c", script, "test", *command],
                                input=patch.encode(), timeout=600)
        return result.returncode
    finally:
        subprocess.run(["docker", "rm", "-f", container], capture_output=True, timeout=30)


def summarize(grades, agents, total):
    result = {}
    for name in agents:
        own = [g for g in grades if g["agent"] == name]
        result[name] = {
            "selected_tasks": total, "recorded_tasks": len(own),
            "resolved": sum(g["resolved"] is True for g in own),
            "unresolved": sum(g["resolved"] is False for g in own),
            "unavailable": total - sum(g["resolved"] is not None for g in own),
            "resolved_fraction_selected": sum(g["resolved"] is True for g in own) / total,
            "complete": len(own) == total,
        }
    return result


def grade(args):
    require_harness()
    contract = load_contract(args.output)
    import docker
    from swebench.harness.run_evaluation import run_instance
    from swebench.harness.test_spec.test_spec import make_test_spec

    rows = json.loads((args.output / "dataset.json").read_text())
    client = docker.from_env()
    grades = []
    # Official relative log paths remain private under the run directory.
    with contextlib.chdir(args.output):
        for agent in contract["agents"]:
            for row in rows:
                key = f"{agent['name']}-{row['instance_id']}"
                prediction_path = args.output / "trials" / (key + ".prediction.json")
                grade_path = args.output / "trials" / (key + ".grade.json")
                if grade_path.exists():
                    grades.append(json.loads(grade_path.read_text()))
                    continue
                if not prediction_path.exists():
                    continue
                prediction = json.loads(prediction_path.read_text())
                result = {"agent": agent["name"], "instance_id": row["instance_id"],
                          "resolved": None, "error": None}
                if not prediction["model_patch"]:
                    result.update(resolved=False, error="empty_patch")
                else:
                    spec = make_test_spec(row, arch="x86_64", namespace="swebench")
                    # Pin the already pulled image contents, not a moving registry tag.
                    local = client.images.get(spec.instance_image_key)
                    if local.id != contract["images"][row["instance_id"]]["id"]:
                        raise ValueError(f"image changed for {row['instance_id']}")
                    run_id = "pilot-" + agent["name"]
                    try:
                        status = run_instance(spec, prediction, False, False, client, run_id,
                                              timeout=args.test_timeout)
                    except docker.errors.DockerException as error:
                        status = {"completed": False}
                        result["error"] = f"Docker evaluation error: {type(error).__name__}"
                    result["completed"] = status.get("completed", False)
                    log = (Path("logs/run_evaluation") / run_id /
                           prediction["model_name_or_path"].replace("/", "__") /
                           row["instance_id"] / "report.json")
                    result["report"] = str(log)
                    if log.exists() and result["completed"]:
                        official = json.loads(log.read_text())[row["instance_id"]]
                        result["resolved"] = official["resolved"]
                    elif result["error"] is None:
                        result["error"] = "official_report_unavailable; inspect run_instance.log"
                save(grade_path, result)
                grades.append(result)
                print(f"graded {key}: resolved={result['resolved']}", flush=True)
    trials = []
    for item in grades:
        path = args.output / "trials" / f"{item['agent']}-{item['instance_id']}.json"
        if path.exists():
            trial = json.loads(path.read_text())
            trials.append({**trial, "scored": item["resolved"] is not None,
                           "passed": item["resolved"] is True, "resolved": item["resolved"]})
    execution, by_case = cross_agent.summarize(trials, contract["agents"])
    for item in grades:
        if item["instance_id"] in by_case and item["agent"] in by_case[item["instance_id"]]:
            by_case[item["instance_id"]][item["agent"]]["resolved"] = item["resolved"]
    report = {"contract": contract, "manifest": json.loads((args.output / "manifest.json").read_text()),
              "grades": grades, "trials": trials, "execution": execution, "by_case": by_case,
              "summary": summarize(grades, [a["name"] for a in contract["agents"]], len(rows))}
    save(args.output / "report.json", report)
    print(json.dumps(report["summary"], indent=2))
    return 0 if all(s["unavailable"] == 0 for s in report["summary"].values()) else 1


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="action", required=True)
    prep = commands.add_parser("prepare")
    prep.add_argument("--dataset", type=Path, required=True, help="frozen selected rows, including grader metadata")
    prep.add_argument("--manifest", type=Path, required=True)
    prep.add_argument("--agents", type=Path, default=Path(__file__).with_name("cross-agent.toml"))
    prep.add_argument("--model", default="claude-sonnet-4-6")
    prep.add_argument("--timeout", type=int, default=900)
    prep.add_argument("--seed", type=int, default=0)
    for name in ("prepare", "run", "grade"):
        sub = prep if name == "prepare" else commands.add_parser(name)
        sub.add_argument("--output", type=Path, required=True)
        if name == "grade":
            sub.add_argument("--test-timeout", type=int, default=900)
    tests = commands.add_parser("test")
    tests.add_argument("--helper", type=Path, required=True)
    tests.add_argument("command", nargs=argparse.REMAINDER)
    prep.add_argument("--dial-sandbox-off", action="store_true",
                      help="explicitly approved unrestricted child shells for Docker access")
    args = parser.parse_args()
    if hasattr(args, "output"):
        args.output = args.output.resolve()
    try:
        if getattr(args, "timeout", 1) < 1 or getattr(args, "test_timeout", 1) < 1:
            raise ValueError("deadlines must be positive")
        return globals()[args.action](args) or 0
    except (ValueError, OSError, subprocess.SubprocessError) as error:
        print(f"error: {error}", file=sys.stderr)
        if isinstance(error, subprocess.CalledProcessError) and error.stderr:
            print(cross_agent.stderr_excerpt(error.stderr), file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
