"""Offline acceptance for the codebase-memory A/B pilot. The `dial` used here is a STUB that mimics the
CLI JSON shapes it depends on (`-p --output-format json`, `understanding --json`, `stats --json`). It proves
runner plumbing, isolation, validation and grading only. It is not a measurement of Dial or any model."""
import json
import os
import shutil
import stat
import sys
import tempfile
import unittest
from pathlib import Path

import codebase_memory_ab as ab

STUB = r'''#!{python}
import json, os, re, sys
from pathlib import Path
home = Path(os.environ["HOME"]) / ".stub"
home.mkdir(exist_ok=True)
findings_path, stats_path = home / "findings.json", home / "stats.json"
findings = json.loads(findings_path.read_text()) if findings_path.exists() else []
stats = json.loads(stats_path.read_text()) if stats_path.exists() else {{"requests": 0, "reported": 0, "input": 0, "output": 0}}
args = sys.argv[1:]
project = Path(args[args.index("-C") + 1]) if "-C" in args else Path.cwd()
with open(os.environ["STUB_LOG"], "a") as log:
    log.write(json.dumps({{"argv": args, "HOME": os.environ.get("HOME"), "DIAL_HOME": os.environ.get("DIAL_HOME"),
                          "files": sorted(str(p.relative_to(project)) for p in project.rglob("*") if p.is_file())}}) + "\n")
if args[0] == "understanding":
    print(json.dumps({{"findings": findings}}))
elif args[0] == "stats":
    print(json.dumps({{"overall": stats}}))
else:
    prompt = args[-1]
    if "remember action" in prompt:
        if os.environ.get("STUB_FAIL_SEED"):
            print(json.dumps({{"type": "result", "ok": False, "text": ""}}))
            sys.exit(0)
        body = prompt.split("\n", 1)[1]
        topic = re.search(r"topic `([^`]+)`", prompt).group(1)
        sources = [{{"path": p, "digest": "stub"}} for p in re.findall(r"`([^`]+)`", re.search(r"^Sources: (.+)$", body, re.M).group(1))]
        findings.append({{"topic": topic, "summary": body.split("\nSources:")[0] + "\n", "sources": sources}})
        text = "DONE"
    else:
        owner_seen = any("Owner: `" in f["summary"] for f in findings)
        text = os.environ["STUB_CORRECT"] if owner_seen else os.environ["STUB_WRONG"]
    stats["requests"] += 1
    stats["input"] += 100
    stats["output"] += 10
    if not os.environ.get("STUB_UNREPORTED"):
        stats["reported"] += 1
    print(json.dumps({{"type": "result", "ok": True, "text": text, "session_id": "stub"}}))
findings_path.write_text(json.dumps(findings))
stats_path.write_text(json.dumps(stats))
'''

FIXTURE_CORRECT = ("owner: src/router/mod.rs:route_request\n"
                   "test: src/router/tests.rs:route_request_prefers_fresh_owner")
FIXTURE_WRONG = ("owner: src/legacy/mod.rs:route_request_legacy\n"
                 "test: src/router/tests.rs:route_request_prefers_fresh_owner")
LEX_TEST = "lexical_find_rounds_credit_new_evidence_without_withholding_a_paraphrased_result"
LEX_CORRECT = (f"owner: crates/dial-agent/src/agent/workflow/investigation.rs:progress_keys\n"
               f"test: crates/dial-agent/src/agent/run/engine/evidence_tests.rs:{LEX_TEST}")
LEX_WRONG = (f"owner: crates/dial-tools/src/tools/code/navigation.rs:progress_keys\n"
             f"test: crates/dial-agent/src/agent/run/engine/evidence_tests.rs:{LEX_TEST}")


def make_stub(directory):
    path = Path(directory, "dial")
    path.write_text(STUB.format(python=sys.executable))
    path.chmod(path.stat().st_mode | stat.S_IXUSR)
    return str(path)


def make_engine(directory):
    """A synthetic engine checkout with the three lexical files; declarations match the real names."""
    root = Path(directory, "engine")
    files = {
        "crates/dial-tools/src/tools/code/navigation.rs": "pub fn lexical_terms() {}\n",
        "crates/dial-agent/src/agent/workflow/investigation.rs": "pub(in crate::agent) fn progress_keys(x: u8) -> u8 { x }\n",
        "crates/dial-agent/src/agent/run/engine/evidence_tests.rs": f"#[test]\nfn {LEX_TEST}() {{}}\n",
        "crates/unrelated/secret.rs": "fn secret() {}\n",
    }
    for rel, text in files.items():
        Path(root, rel).parent.mkdir(parents=True, exist_ok=True)
        Path(root, rel).write_text(text)
    return root


class AnswerAndResolutionTest(unittest.TestCase):
    def setUp(self):
        self.fixture = ab.make_target("fixture")

    def test_grader_accepts_exactly_the_right_two_lines(self):
        self.assertTrue(ab.grade_answer(FIXTURE_CORRECT, self.fixture))
        self.assertTrue(ab.grade_answer("owner: `src/router/mod.rs:route_request`\n"
                                        "test: src/router/tests.rs:route_request_prefers_fresh_owner\n", self.fixture))

    def test_grader_rejects_wrong_extra_or_contradictory_lines(self):
        cases = {
            "wrong owner": FIXTURE_WRONG,
            "wrong test": "owner: src/router/mod.rs:route_request\ntest: src/router/mod.rs:route_request",
            "extra line": FIXTURE_CORRECT + "\nowner: src/router/mod.rs:route_request",
            "prose": "The owner is route_request.\n" + FIXTURE_CORRECT,
            "duplicate owner": "owner: src/router/mod.rs:route_request\nowner: src/legacy/mod.rs:route_request_legacy\n"
                               "test: src/router/tests.rs:route_request_prefers_fresh_owner",
            "missing test": "owner: src/router/mod.rs:route_request",
            "blank": "",
        }
        for name, answer in cases.items():
            with self.subTest(name):
                self.assertFalse(ab.grade_answer(answer, self.fixture))

    def test_fixture_note_resolves_and_decoy_declares_no_owner(self):
        self.assertTrue(ab.resolve_note(ab.FIXTURE, self.fixture, ab.build_note(self.fixture))["resolved"])
        legacy = (ab.FIXTURE / "src/legacy/mod.rs").read_text()
        self.assertEqual(ab._declarations(legacy, self.fixture.owner_fn), 0)

    def test_lexical_target_needs_an_engine_checkout_with_all_three_files(self):
        with self.assertRaises(ValueError):
            ab.make_target("lexical")
        with tempfile.TemporaryDirectory() as tmp:
            engine = make_engine(tmp)
            Path(engine, ab.LEXICAL_FILES[2]).unlink()
            with self.assertRaises(ValueError):
                ab.make_target("lexical", engine)

    def test_lexical_note_resolves_only_within_its_cited_files(self):
        with tempfile.TemporaryDirectory() as tmp:
            engine = make_engine(tmp)
            target = ab.make_target("lexical", engine)
            self.assertTrue(ab.resolve_note(engine, target, ab.build_note(target))["resolved"])
            Path(engine, ab.LEXICAL_FILES[1]).write_text("fn progress_keys(){}\nfn progress_keys(x: u8){}\n")
            self.assertFalse(ab.resolve_note(engine, target, ab.build_note(target))["resolved"])

    def test_lexical_grader_checks_the_real_paths(self):
        target = ab.make_target("lexical", make_engine(tempfile.mkdtemp()))
        self.assertTrue(ab.grade_answer(LEX_CORRECT, target))
        self.assertFalse(ab.grade_answer(LEX_WRONG, target))

    def test_plan_prompt_and_note_are_target_scoped_and_shared_by_both_arms(self):
        plan = ab.plan(self.fixture)
        self.assertIn("navigate action with query `incoming request route selection`", plan["prompt"])
        self.assertTrue(plan["note_resolution"]["resolved"])
        self.assertEqual(plan["arms"], {"without": {"note_seeded": False}, "with": {"note_seeded": True}})


class StructuredNoteStateTest(unittest.TestCase):
    def setUp(self):
        self.target = ab.make_target("fixture")
        self.note = ab.build_note(self.target)

    def finding(self, **overrides):
        base = {"topic": self.target.topic, "summary": self.note.split("\nSources:")[0] + "\n",
                "sources": [{"path": p, "digest": "d"} for p in self.target.sources]}
        base.update(overrides)
        return base

    def test_exact_structured_finding_is_saved(self):
        self.assertEqual(ab.note_state({"findings": [self.finding()]}, self.target, self.note), "saved")

    def test_sources_pasted_into_summary_do_not_count_as_citations(self):
        fake = self.finding(sources=[], summary=self.note)
        self.assertEqual(ab.note_state({"findings": [fake]}, self.target, self.note), "partial")

    def test_wrong_topic_or_missing_source_is_not_saved(self):
        table = {
            "topic differs": self.finding(topic="route selection"),
            "one citation missing": self.finding(sources=[{"path": "src/router/mod.rs", "digest": "d"}]),
            "owner line inside prose": self.finding(summary="Owner: route_request is mentioned in prose\n"),
        }
        for name, finding in table.items():
            with self.subTest(name):
                self.assertNotEqual(ab.note_state({"findings": [finding]}, self.target, self.note), "saved")

    def test_no_same_topic_finding_is_absent(self):
        self.assertEqual(ab.note_state({"findings": []}, self.target, self.note), "absent")


class UsageAndValidationTest(unittest.TestCase):
    def usage(self, requests=1, reported=None, input_tokens=100, output_tokens=10):
        return {"requests": requests, "reported": requests if reported is None else reported,
                "input_tokens": input_tokens, "output_tokens": output_tokens}

    def test_unreported_requests_make_tokens_unknown_not_zero(self):
        after = {"requests": 3, "reported": 2, "input_tokens": 200, "output_tokens": 20}
        before = {"requests": 1, "reported": 1, "input_tokens": 100, "output_tokens": 10}
        delta = ab.usage_delta(after, before, "t")
        self.assertEqual(delta["requests"], 2)
        self.assertIsNone(delta["input_tokens"])
        self.assertIsNone(delta["output_tokens"])

    def test_missing_reading_or_decreasing_counter_is_refused_or_unknown(self):
        self.assertIsNone(ab.usage_delta(None, {"requests": 0, "reported": 0, "input_tokens": 0, "output_tokens": 0}, "t"))
        before = {"requests": 2, "reported": 2, "input_tokens": 5, "output_tokens": 1}
        after = {"requests": 1, "reported": 1, "input_tokens": 5, "output_tokens": 1}
        with self.assertRaises(ValueError):
            ab.usage_delta(after, before, "t")

    def test_invalid_counts_are_rejected(self):
        bad = {"negative": -1, "float": 1.5, "bool": True, "string": "3"}
        for name, value in bad.items():
            with self.subTest(name):
                record = self.record("without", 1, measurement=self.usage(input_tokens=value))
                with self.assertRaises(ValueError):
                    ab.validate_trials([record, self.record("with", 1, seed={"usage": self.usage()})])

    def record(self, arm, trial, measurement=None, seed=None, **kw):
        base = {"arm": arm, "trial": trial, "note_seeded": arm == "with", "answer": FIXTURE_CORRECT,
                "turn_ok": True, "errors": [], "measurement": measurement or self.usage(),
                "seed": seed if arm == "with" else None}
        base.update(kw)
        return base

    def test_arm_trial_pairing_and_shapes_are_checked(self):
        table = {
            "unknown arm": [self.record("other", 1)],
            "duplicate pair": [self.record("with", 1, seed={"usage": self.usage()}), self.record("with", 1, seed={"usage": self.usage()}), self.record("without", 1)],
            "note flag contradicts arm": [self.record("with", 1, seed={"usage": self.usage()}, note_seeded=False), self.record("without", 1)],
            "unpaired trial": [self.record("with", 1, seed={"usage": self.usage()}), self.record("without", 1), self.record("without", 2)],
            "with arm lacks seed": [self.record("with", 1), self.record("without", 1)],
            "without arm has seed": [self.record("with", 1, seed={"usage": self.usage()}),
                                     dict(self.record("without", 1), seed={"usage": self.usage()})],
            "zero trial": [self.record("with", 0, seed={"usage": self.usage()}), self.record("without", 0)],
            "empty": [],
        }
        for name, records in table.items():
            with self.subTest(name):
                with self.assertRaises(ValueError):
                    ab.validate_trials(records)

    def test_failed_setup_with_correct_looking_answer_is_not_checked(self):
        target = ab.make_target("fixture")
        records = [self.record("without", 1), self.record("with", 1, seed={"usage": self.usage()}, errors=["seed failed"]),
                   self.record("without", 2, turn_ok=False), self.record("with", 2, seed={"usage": self.usage()})]
        summary = ab.summarise(records, target)
        self.assertEqual(summary["with"]["checked"], 1)
        self.assertEqual(summary["without"]["checked"], 1)

    def test_totals_separate_seed_from_measurement_and_divide_by_checked(self):
        target = ab.make_target("fixture")
        records = [
            self.record("without", 1, measurement=self.usage(requests=2, input_tokens=300, output_tokens=5)),
            self.record("without", 2, answer="wrong", measurement=self.usage(requests=3, input_tokens=300, output_tokens=9)),
            self.record("with", 1, seed={"usage": self.usage(requests=1, input_tokens=400)},
                        measurement=self.usage(requests=1, input_tokens=100)),
            self.record("with", 2, seed={"usage": self.usage(requests=1, input_tokens=400)},
                        measurement=self.usage(requests=1, input_tokens=100)),
        ]
        summary = ab.summarise(records, target)
        self.assertEqual(summary["without"]["measurement_requests_all_trials"], 5)
        self.assertEqual(summary["without"]["measurement_input_tokens_per_checked"], 600)
        self.assertIsNone(summary["without"]["seed_requests_all_trials"])
        self.assertEqual(summary["with"]["seed_requests_all_trials"], 2)
        self.assertEqual(summary["with"]["measurement_input_tokens_per_checked"], 100)
        self.assertEqual(summary["with"]["total_requests_per_checked"], 2)
        self.assertEqual(summary["with"]["total_input_tokens_per_checked"], 500)

    def test_unknown_measurement_tokens_stay_none_in_every_total(self):
        target = ab.make_target("fixture")
        unknown = self.usage(requests=2, reported=1, input_tokens=None, output_tokens=None)
        records = [self.record("without", 1, measurement=unknown), self.record("with", 1, seed={"usage": self.usage()}, measurement=self.usage())]
        summary = ab.summarise(records, target)
        self.assertIsNone(summary["without"]["measurement_input_tokens_all_trials"])
        self.assertIsNone(summary["without"]["measurement_input_tokens_per_checked"])
        self.assertEqual(summary["without"]["measurement_requests_all_trials"], 2)


class StubEndToEndTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp(prefix="cmab-stub-")
        self.dial = make_stub(self.tmp)
        self.log = Path(self.tmp, "calls.log")
        self._env = dict(os.environ)
        os.environ.update(STUB_LOG=str(self.log), STUB_CORRECT=FIXTURE_CORRECT, STUB_WRONG=FIXTURE_WRONG)
        for key in ("STUB_UNREPORTED", "STUB_FAIL_SEED"):
            os.environ.pop(key, None)

    def tearDown(self):
        os.environ.clear()
        os.environ.update(self._env)
        shutil.rmtree(self.tmp, ignore_errors=True)

    def calls(self):
        return [json.loads(line) for line in self.log.read_text().splitlines()]

    def test_fixture_suite_pairs_arms_isolates_state_and_uses_identical_measurement_prompts(self):
        target = ab.make_target("fixture")
        report = ab.run_suite(self.dial, target, trials=2, model_args=[])
        self.assertEqual(len(report["trials"]), 4)
        for record in report["trials"]:
            self.assertEqual(record["errors"], [], record)
        summary = ab.summarise(report["trials"], target)
        self.assertEqual(summary["with"]["checked"], 2)
        self.assertEqual(summary["without"]["checked"], 0)
        self.assertEqual(summary["with"]["seed_requests_all_trials"], 2)
        self.assertEqual(summary["with"]["measurement_requests_per_checked"], 1.0)
        measure_calls = [c for c in self.calls() if "--" in c["argv"] and "navigate action" in c["argv"][-1]]
        self.assertEqual(len(measure_calls), 4)
        self.assertEqual({c["argv"][-1] for c in measure_calls}, {ab.prompt_for(target)})
        self.assertTrue(all("--approval" in c["argv"] and c["argv"][c["argv"].index("--approval") + 1] == "ask" for c in measure_calls))
        for call in self.calls():
            self.assertTrue(call["HOME"] and call["DIAL_HOME"].startswith(call["HOME"]))
            self.assertEqual(call["files"], sorted(call["files"]))
            self.assertTrue(all(f == "src" or f.startswith("src/") for f in call["files"]), call["files"])
            self.assertFalse(any("README" in f or "test_codebase" in f for f in call["files"]))

    def test_lexical_suite_copies_only_the_three_files_and_grades_their_paths(self):
        with tempfile.TemporaryDirectory() as tmp:
            engine = make_engine(tmp)
            os.environ.update(STUB_CORRECT=LEX_CORRECT, STUB_WRONG=LEX_WRONG)
            target = ab.make_target("lexical", engine)
            report = ab.run_suite(self.dial, target, trials=1, model_args=[])
            summary = ab.summarise(report["trials"], target)
            self.assertEqual(summary["with"]["checked"], 1)
            self.assertEqual(summary["without"]["checked"], 0)
            for call in self.calls():
                self.assertEqual(sorted(call["files"]), sorted(ab.LEXICAL_FILES))

    def test_seed_failure_is_recorded_and_excluded_from_checked(self):
        os.environ["STUB_FAIL_SEED"] = "1"
        target = ab.make_target("fixture")
        report = ab.run_suite(self.dial, target, trials=1, model_args=[])
        with_record = next(r for r in report["trials"] if r["arm"] == "with")
        self.assertTrue(with_record["errors"])
        self.assertEqual(ab.summarise(report["trials"], target)["with"]["checked"], 0)

    def test_unreported_stub_usage_leaves_token_totals_unknown(self):
        os.environ["STUB_UNREPORTED"] = "1"
        target = ab.make_target("fixture")
        report = ab.run_suite(self.dial, target, trials=1, model_args=[])
        summary = ab.summarise(report["trials"], target)
        self.assertEqual(summary["with"]["checked"], 1)
        self.assertIsNone(summary["with"]["measurement_input_tokens_per_checked"])
        self.assertIsNone(summary["with"]["total_input_tokens_per_checked"])


if __name__ == "__main__":
    unittest.main()
