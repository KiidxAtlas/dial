"""Freeze an evaluation contract from metadata before opening held-out task instructions.

Execution and usage parsing stay with the existing Harbor/cross-agent owners. This module only
selects a reproducible cohort and records decision criteria; it never executes task code.
"""
import argparse
import hashlib
import json
from pathlib import Path
import tomllib

ORIGINAL_TASKS = ["build-pmars", "constraints-scheduling", "db-wal-recovery",
                  "filter-js-from-html", "log-summary-date-ranges", "openssl-selfsigned-cert",
                  "regex-log", "write-compressor"]


def freeze_contract(dataset, prior_manifests, count=8, seed="dial-omp-gap-v1"):
    dataset = Path(dataset)
    excluded = set(ORIGINAL_TASKS)
    for path in sorted(map(Path, prior_manifests)):
        if path.stat().st_size > 32 * 1024 * 1024:
            raise ValueError("prior manifest exceeds metadata limit")
        manifest = json.loads(path.read_text())
        excluded.update(task.rsplit("/", 1)[-1] for task in manifest.get("tasks", []) if isinstance(task, str))
    candidates = []
    for metadata in sorted(dataset.glob("*/task.toml")):
        name = metadata.parent.name
        if name in excluded:
            continue
        task = tomllib.loads(metadata.read_text())
        if task.get("environment", {}).get("gpus", 0) != 0:
            continue
        candidates.append(name)
    candidates.sort(key=lambda name: hashlib.sha256((seed + "\0" + name).encode()).hexdigest())
    if not 1 <= count <= len(candidates):
        raise ValueError("not enough unmeasured CPU-task candidates")
    selected = candidates[:count]
    inventories = {}
    # Hashing bytes does not expose instructions or graders to the agent. Freeze every source
    # so later task edits cannot silently turn a failed trial into an easier task.
    for name in ORIGINAL_TASKS + selected:
        for file in sorted((dataset / name).rglob("*")):
            if file.is_file() and "__pycache__" not in file.parts:
                inventories[file.relative_to(dataset).as_posix()] = hashlib.sha256(file.read_bytes()).hexdigest()
    return {
        "schema_version": 1, "seed": seed, "original_tasks": ORIGINAL_TASKS,
        "held_out_tasks": selected, "prior_tasks_excluded": sorted(excluded),
        "selection": "Deterministic metadata-only selection of CPU tasks absent from recorded prior manifests; instructions and solutions not opened for selection. Global human/agent familiarity cannot be guaranteed.",
        "repeats": 3, "ordering": "serial counterbalanced order; no concurrent builds or trials; no silent retries or task substitutions",
        "target": {"minimum_pass_rate_relative_to_omp": 1.0, "maximum_request_ratio": 1.3,
                   "maximum_output_token_ratio": 1.3, "aggregation": "per-task medians of three attempts; report each task and cohort totals, including failures; report successful-only ratios separately, never discard failures"},
        "preservation_gate": "filter-js-from-html stays in the original cohort; retain original fixtures and grader",
        "cost": "separate uncached input, cache writes, cache reads and output; price only with applicable model/account rates; subscription accounting is not an invoice",
        "early_measurement": "Run the same frozen pre-integration Dial binary at default and explicit low effort against OMP before attributing gains to other changes. If that binary is unavailable, identify and freeze a replacement baseline; do not relabel it as eight-v1-r2.",
        "defaults_decision": "After the matched cohort, explicitly choose print/benchmark defaults based on quality and efficiency; retain interactive defaults separately. Adaptive effort stays opt-in until its own ablation passes.",
        "ablations": ["effort", "eval.auto_offer", "eval.portable_helpers", "eval.parallel_batches",
                      "efficiency.preserve_task_inputs", "efficiency.small_case_guard",
                      "efficiency.evidence_stall_rounds", "verification.auto_registered_checks",
                      "experimental.adaptive_output_effort", "compaction.argument_projection_tokens"],
        "task_files": inventories,
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--dataset", type=Path, required=True)
    parser.add_argument("--prior-manifests", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--count", type=int, default=8)
    args = parser.parse_args()
    contract = freeze_contract(args.dataset, args.prior_manifests.glob("*-manifest.json"), args.count)
    with args.output.open("x") as stream:
        json.dump(contract, stream, indent=2)
        stream.write("\n")
    print(json.dumps({key:contract[key] for key in ("held_out_tasks", "repeats", "target")}))


if __name__ == "__main__":
    main()
