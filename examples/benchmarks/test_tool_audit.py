"""Adversarial audit gate contracts, not live model or platform evidence."""
import copy
import hashlib
import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
# Also support unittest's module-style invocation used by focused verification.
sys.path.insert(0, str(Path(__file__).resolve().parent))

import tool_audit as audit
from test_tool_effectiveness import machine, tool_trace, trial_case


class AuditTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.put("crates/dial-llm/src/config/tools.rs", 'pub const BUILTIN: &[&str] = &["read"];\npub const EXTRA_TOOLS: &[&str] = &["verify"];')
        self.put("crates/dial-llm/src/config/tools/integrations.rs", '\nintegrations! {\n    ("eval", true, true),\n}\n')
        self.put("src/owner.rs", "// owner\n")
        self.put("tests/test_owner.py", "# regression\n")
        self.check = {"tier": "offline", "adapter": "unittest", "minimum_tests": 1,
                      "timeout": 5, "argv": [sys.executable, "-c", "print('Ran 1 test in 0.1s\\nOK')"],
                      "sources": ["tests/test_owner.py"], "inputs": ["src/owner.rs"]}
        self.manifest = {"version": 1, "checks": {"regression": self.check}, "findings": [], "groups": [
            {"id": "owner", "tools": ["read", "verify", "eval"], "owner": "src/owner.rs",
             "sources": ["src/owner.rs"], "coverage": {
                 dimension: {"checks": ["regression"]} for dimension in audit.DIMENSIONS}}]}
        self.manifest["groups"][0]["coverage"]["behavior"] = {"gap": "Requires live benchmark evidence."}

    def put(self, name, text):
        path = self.root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)
        return path

    def recorded(self, name="run", *, passed=True, source=None, started=10, finished=20):
        directory = self.root / name
        directory.mkdir()
        text = "Ran 1 test in 0.1s\nOK\n" if passed else "Ran 1 test in 0.1s\nFAILED (failures=1)\n"
        log = directory / "regression.log"
        log.write_text(text)
        row = {"check": "regression", "definition_sha256": audit.digest(self.check),
               "source_sha256": source or audit.snapshot(self.root), "started_ns": started,
               "inputs_sha256": "old-implementation" if source else audit.input_digest(self.root, self.check),
               "finished_ns": finished, "exit_code": 0 if passed else 1, "timed_out": False,
               "passed_tests": 1 if passed else 0, "failed_tests": 0 if passed else 1,
               "inputs_changed": False, "status": "passed" if passed else "failed",
               "log": log.name, "log_sha256": hashlib.sha256(log.read_bytes()).hexdigest()}
        path = directory / "run.json"
        audit.save(path, {"kind": "tool-audit-run", "version": 1, "checks": [row]})
        return path

    def test_repository_manifest_is_complete_plan_not_execution(self):
        manifest = audit.validate(audit.ROOT, audit.read_json(audit.ROOT / audit.MANIFEST))
        rows = audit.coverage(audit.ROOT, manifest)
        self.assertTrue(any(row["status"] == "gap" for row in rows))
        self.assertFalse(any(row["status"] == "passed" for row in rows))

    def test_inventory_detects_new_removed_and_duplicate_families(self):
        audit.validate(self.root, self.manifest)
        for names in (["read", "verify"], ["read", "verify", "eval", "removed"], ["read", "verify", "eval", "eval"]):
            with self.subTest(names=names):
                value = copy.deepcopy(self.manifest)
                value["groups"][0]["tools"] = names
                with self.assertRaises(ValueError):
                    audit.validate(self.root, value)

    def test_source_declaration_change_fails_closed(self):
        self.put("crates/dial-llm/src/config/tools.rs", "// declarations moved")
        with self.assertRaisesRegex(ValueError, "cannot discover"):
            audit.roster(self.root)

    def test_missing_dimensions_checks_and_sources_refused(self):
        for field in ("dimension", "check", "source", "minimum", "traversal"):
            value = copy.deepcopy(self.manifest)
            if field == "dimension":
                del value["groups"][0]["coverage"]["effects"]
            elif field == "check":
                value["groups"][0]["coverage"]["effects"] = {"checks": ["missing"]}
            elif field == "source":
                value["groups"][0]["sources"] = ["missing.rs"]
            elif field == "minimum":
                value["checks"]["regression"]["minimum_tests"] = 0
            else:
                value["groups"][0]["sources"] = ["../outside"]
            with self.subTest(field=field), self.assertRaises(ValueError):
                audit.validate(self.root, value)

    def test_duplicate_json_keys_refused(self):
        path = self.put("duplicate.json", '{"version":1,"version":2}')
        with self.assertRaisesRegex(ValueError, "duplicate"):
            audit.read_json(path)

    def test_missing_group_fields_get_named_refusals_not_key_errors(self):
        for field, message in (("id", "invalid group id"), ("owner", "missing owner/sources"),
                               ("coverage", "missing/unknown dimensions")):
            value = copy.deepcopy(self.manifest)
            del value["groups"][0][field]
            with self.subTest(field=field), self.assertRaisesRegex(ValueError, message):
                audit.validate(self.root, value)

    def test_absent_evidence_is_unverified_and_gap_remains_gap(self):
        rows = audit.coverage(self.root, self.manifest)
        self.assertEqual({r["status"] for r in rows}, {"unverified", "gap"})
        self.manifest["groups"][0]["coverage"]["effects"] = {"gap": "not exercised"}
        path = self.recorded()
        rows = audit.coverage(self.root, self.manifest, [path])
        self.assertEqual(next(r for r in rows if r["dimension"] == "effects")["status"], "gap")
        self.assertTrue(any(r["status"] == "passed" for r in rows))

    def test_stale_changed_definition_and_tampered_logs_refused(self):
        path = self.recorded()
        self.put("src/owner.rs", "// changed\n")
        with self.assertRaisesRegex(ValueError, "stale"):
            audit.coverage(self.root, self.manifest, [path])
        self.put("src/owner.rs", "// owner\n")
        self.check["minimum_tests"] = 2
        with self.assertRaisesRegex(ValueError, "definition changed"):
            audit.coverage(self.root, self.manifest, [path])
        self.check["minimum_tests"] = 1
        (path.parent / "regression.log").write_text("altered")
        with self.assertRaisesRegex(ValueError, "changed log"):
            audit.coverage(self.root, self.manifest, [path])

    def test_zero_tests_all_skips_and_nonzero_exit_never_pass(self):
        cases = [("cargo", "test result: ok. 0 passed; 0 failed; 1 ignored;", 0),
                 ("cargo", "test result: ok. 2 passed; 0 failed; 1 ignored;", 2),
                 ("unittest", "Ran 2 tests in 0.1s\nOK (skipped=2)", 0),
                 ("unittest", "Ran 2 tests in 0.1s\nFAILED (failures=1)", 0),
                 ("node", "# tests 3\n# pass 2\n# skipped 1", 2)]
        for adapter, text, expected in cases:
            with self.subTest(adapter=adapter, text=text):
                self.assertEqual(audit.passed_count(adapter, text), expected)
        self.check["argv"] = [sys.executable, "-c", "raise SystemExit(1)"]
        out = self.root / "out"
        out.mkdir()
        result = audit.execute(self.root, "regression", self.check, out)
        self.assertEqual(result["status"], "failed")
        self.assertEqual(result["exit_code"], 1)

    def test_real_runner_success_and_refuses_overwrite(self):
        out = self.root / "out"
        out.mkdir()
        result = audit.execute(self.root, "regression", self.check, out)
        self.assertEqual(result["status"], "passed")
        self.assertEqual(result["passed_tests"], 1)
        with self.assertRaises(ValueError):
            audit.execute(self.root, "regression", self.check, out)

    def test_timeout_and_inputs_changed_never_pass(self):
        out = self.root / "out"
        out.mkdir()
        self.check["timeout"] = 1
        self.check["argv"] = [sys.executable, "-c", "import time; time.sleep(30)"]
        result = audit.execute(self.root, "timeout", self.check, out)
        self.assertTrue(result["timed_out"])
        self.assertEqual(result["status"], "failed")
        self.check["argv"] = [sys.executable, "-c", "from pathlib import Path; Path('src/owner.rs').write_text('changed'); print('Ran 1 test in 0.1s\\nOK')"]
        result = audit.execute(self.root, "changed", self.check, out)
        self.assertTrue(result["inputs_changed"])
        self.assertEqual(result["status"], "failed")

    def test_closed_finding_needs_failure_then_fresh_pass(self):
        before = self.recorded("before", passed=False, source="old-source", started=1, finished=2)
        after = self.recorded("after", started=3, finished=4)
        finding = dict(id="F1", group="owner", owner="maintainer", evidence="observed", cause="defect",
                       next_action="none", check="regression", status="closed", fix="causal change",
                       before=str(before.relative_to(self.root)), after=str(after.relative_to(self.root)))
        self.manifest["findings"] = [finding]
        audit.validate(self.root, self.manifest)
        self.assertEqual(audit.closure(self.root, self.manifest), [{"id": "F1", "status": "closed"}])
        finding["before"] = finding["after"]
        with self.assertRaises(ValueError):
            audit.closure(self.root, self.manifest)

    def test_compile_failure_cannot_close_as_behavioral_reproduction(self):
        before = self.recorded("before", passed=False, source="old-source", started=1, finished=2)
        log = before.parent / "regression.log"
        log.write_text("compiler failed before executing tests")
        data = audit.read_json(before)
        data["checks"][0]["log_sha256"] = hashlib.sha256(log.read_bytes()).hexdigest()
        data["checks"][0]["failed_tests"] = 0
        before.write_text(json.dumps(data))
        after = self.recorded("after", started=3, finished=4)
        self.manifest["findings"] = [dict(id="F1", group="owner", check="regression", status="closed",
            before=str(before.relative_to(self.root)), after=str(after.relative_to(self.root)))]
        with self.assertRaisesRegex(ValueError, "failing repro"):
            audit.closure(self.root, self.manifest)

    def payload(self):
        trace = tool_trace(offered=["read"], invoked={"read": 1})
        return machine({name: [trial_case(i, "task", trace=trace) for i in range(3)]
                        for name in ("baseline", "candidate")})

    def test_behavior_reuses_existing_validators(self):
        result = audit.evaluate(self.payload(), "candidate")
        self.assertEqual(result["status"], "passed")
        self.assertIn("not proof", result["interpretation"])

    def test_behavior_rejects_missing_trace_and_wrong_candidate(self):
        payload = self.payload()
        payload["configurations"][1]["report"]["cases"][0]["metrics"].pop("tool_trace")
        with self.assertRaises(ValueError):
            audit.evaluate(payload, "candidate")
        for name in ("baseline", "missing"):
            with self.assertRaises(ValueError):
                audit.evaluate(self.payload(), name)

    def test_behavior_regression_fails_and_mismatched_contracts_refused(self):
        payload = self.payload()
        case = payload["configurations"][1]["report"]["cases"][0]
        case.update(passed=False, checks_passed=0)
        case["evidence"]["failed_check_indices"] = [0]
        # Keep a second matched task so a correctness regression cannot hide in exclusions.
        for entry in payload["configurations"]:
            entry["report"]["cases"].extend(trial_case(i, "other", trace=tool_trace()) for i in range(3))
        self.assertEqual(audit.evaluate(payload, "candidate")["status"], "failed")
        payload["configurations"][1]["report"]["contract"]["source_sha256"] = "f" * 64
        with self.assertRaises(ValueError):
            audit.evaluate(payload, "candidate")

    def test_cli_validate_gate_and_exclusive_output(self):
        manifest = self.put("manifest.json", json.dumps(self.manifest))
        script = str(audit.ROOT / "examples/benchmarks/tool_audit.py")
        prefix = [sys.executable, script, "--root", str(self.root), "--manifest", manifest.name]
        self.assertEqual(subprocess.run(prefix + ["validate"], capture_output=True).returncode, 0)
        self.assertEqual(subprocess.run(prefix + ["gate", "--require-complete"], capture_output=True).returncode, 1)
        out = self.root / "run"
        self.assertEqual(subprocess.run(prefix + ["run", "--out", str(out)], capture_output=True).returncode, 0)
        self.assertEqual(subprocess.run(prefix + ["run", "--out", str(out)], capture_output=True).returncode, 2)

    def test_ratchet_prevents_coverage_and_finding_erasure(self):
        audit.ratchet(self.manifest, self.manifest)
        for mutation in ("gap", "minimum", "command", "tool", "finding"):
            before, after = copy.deepcopy(self.manifest), copy.deepcopy(self.manifest)
            if mutation == "gap":
                after["groups"][0]["coverage"]["contract"] = {"gap": "removed"}
            elif mutation == "minimum":
                after["checks"]["regression"]["minimum_tests"] = 0
            elif mutation == "command":
                after["checks"]["regression"]["argv"] = ["true"]
            elif mutation == "tool":
                after["groups"][0]["tools"].remove("read")
            else:
                before["findings"] = [{"id": "F1", "status": "open"}]
            with self.subTest(mutation=mutation), self.assertRaises(ValueError):
                audit.ratchet(before, after)

    def test_native_tests_cannot_claim_live_behavior(self):
        self.manifest["groups"][0]["coverage"]["behavior"] = {"checks": ["regression"]}
        with self.assertRaisesRegex(ValueError, "benchmark evidence"):
            audit.validate(self.root, self.manifest)



    def test_loader_failure_is_not_an_executed_behavioral_reproduction(self):
        text = "unittest.loader._FailedTest\nRan 1 test in 0.1s\nFAILED (errors=1)"
        self.assertEqual(audit.failed_count("unittest", text), 0)

    def test_metadata_only_change_cannot_close_finding(self):
        before = self.recorded("before", passed=False, source="old-source", started=1, finished=2)
        data = audit.read_json(before)
        data["checks"][0]["inputs_sha256"] = audit.input_digest(self.root, self.check)
        before.write_text(json.dumps(data))
        after = self.recorded("after", started=3, finished=4)
        self.manifest["findings"] = [dict(id="F1", group="owner", check="regression", status="closed",
            before=str(before.relative_to(self.root)), after=str(after.relative_to(self.root)))]
        with self.assertRaisesRegex(ValueError, "implementation/test inputs"):
            audit.closure(self.root, self.manifest)

    def test_missing_runtime_is_unavailable_not_a_pass(self):
        self.check["argv"] = ["/nonexistent/dial-audit-runtime"]
        manifest = self.put("manifest.json", json.dumps(self.manifest))
        out = self.root / "out"
        result = subprocess.run([sys.executable, str(audit.ROOT / "examples/benchmarks/tool_audit.py"),
                                "--root", str(self.root), "--manifest", manifest.name, "run", "--out", str(out)],
                                capture_output=True, text=True)
        self.assertEqual(result.returncode, 1, result.stderr)
        receipt = audit.receipt(self.root, out / "run.json", self.manifest["checks"], audit.snapshot(self.root))
        self.assertEqual(receipt["regression"]["status"], "unavailable")
        self.assertFalse(any(row["status"] == "passed" for row in audit.coverage(self.root, self.manifest, [out / "run.json"])))


class BehaviorAuditTests(AuditTests):
    """Synthetic reports test the adapter only; they are not live-model evidence."""

    def behavior_check(self):
        return dict(tier="live", adapter="behavior", minimum_tests=3, timeout=5,
                    argv=[sys.executable, "-c", "raise SystemExit(0)"],
                    sources=["tests/test_owner.py"], candidate="candidate", tasks=["task"],
                    tools=["read", "verify", "eval"], report="benchmarks/report.json")

    def test_behavior_execution_receipt_roundtrip_and_live_opt_in(self):
        check = self.behavior_check()
        payload = self.payload()
        for case in payload["configurations"][1]["report"]["cases"]:
            case["metrics"]["tool_trace"]["invoked"] = {"read": 1, "verify": 1, "eval": 1}
        program = "from pathlib import Path; p=Path('benchmarks/report.json'); p.parent.mkdir(); p.write_text(" + repr(json.dumps(payload)) + ")"
        check["argv"] = [sys.executable, "-c", program]
        self.manifest["checks"] = {"regression": check}
        self.manifest["groups"][0]["coverage"] = {d: {"checks": ["regression"]} for d in audit.DIMENSIONS}
        audit.validate(self.root, self.manifest)
        manifest = self.put("manifest.json", json.dumps(self.manifest))
        prefix = [sys.executable, str(audit.ROOT / "examples/benchmarks/tool_audit.py"),
                  "--root", str(self.root), "--manifest", manifest.name]
        out = self.root / "out"
        command = prefix + ["run", "--tier", "live", "--out", str(out)]
        result = subprocess.run(command, capture_output=True, text=True)
        self.assertEqual(result.returncode, 2, result.stderr)
        self.assertFalse(out.exists())
        result = subprocess.run(command + ["--allow-live"], capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        rows = audit.coverage(self.root, self.manifest, [out / "run.json"])
        self.assertEqual({row["status"] for row in rows}, {"passed"})
        with self.assertRaisesRegex(ValueError, "stale/overwritten"):
            audit.execute(self.root, "again", check, out)

    def test_behavior_requires_tools_tasks_and_unambiguous_payload(self):
        check = self.behavior_check()
        payload = self.payload()
        line = "TOOL_AUDIT_BEHAVIOR=" + json.dumps(payload)
        self.assertEqual(audit.test_counts(check, line), (0, 0))
        check["tools"] = ["read"]
        self.assertEqual(audit.test_counts(check, line), (3, 0))
        self.assertEqual(audit.test_counts(check, line + "\n" + line), (0, 0))
        self.assertEqual(audit.test_counts(check, "TOOL_AUDIT_BEHAVIOR={broken}"), (0, 0))
        check["tasks"] = ["absent"]
        self.assertEqual(audit.test_counts(check, line), (0, 0))

    def test_efficiency_budgets_block_regressions_and_unknown_is_not_free(self):
        payload = self.payload()
        for case in payload["configurations"][1]["report"]["cases"]:
            case["metrics"]["tool_calls"] = 30
        result = audit.evaluate(payload, "candidate")
        self.assertEqual(result["status"], "failed")
        self.assertIn("task/tool_calls_median", result["efficiency_regressions"])
        payload = self.payload()
        for case in payload["configurations"][1]["report"]["cases"]:
            case["metrics"]["token_usage_observed"] = False
            case["metrics"]["token_usage_incomplete"] = True
        self.assertEqual(audit.evaluate(payload, "candidate")["status"], "inconclusive")

    def test_mixed_tier_selection_cannot_bypass_live_opt_in(self):
        self.manifest["checks"]["live"] = self.behavior_check()
        manifest = self.put("manifest.json", json.dumps(self.manifest))
        result = subprocess.run([sys.executable, str(audit.ROOT / "examples/benchmarks/tool_audit.py"),
                                "--root", str(self.root), "--manifest", manifest.name,
                                "run", "--tier", "offline", "--check", "live", "--out", str(self.root / "out")],
                                capture_output=True, text=True)
        self.assertEqual(result.returncode, 2, result.stderr)
        self.assertFalse((self.root / "out").exists())

    def test_inventory_rejects_unparsed_partial_new_declaration(self):
        self.put("crates/dial-llm/src/config/tools/integrations.rs",
                 '\nintegrations! {\n ("eval", true, true),\n ("future-tool", true, true),\n}\n')
        with self.assertRaisesRegex(ValueError, "unparsed"):
            audit.roster(self.root)


if __name__ == "__main__":
    unittest.main()
