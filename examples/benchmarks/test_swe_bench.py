"""Offline SWE-bench pilot regressions: no Docker, model, network or dataset download."""
import hashlib
import os
import json
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
import contextlib
import io
import types
from unittest import mock

ROOT = Path(__file__).resolve().parent
sys.path.insert(0, str(ROOT))
import cross_agent  # noqa: E402
import swe_bench  # noqa: E402

GOLD = "GOLD_PATCH_SENTINEL_7f3a"
TEST_PATCH = "TEST_PATCH_SENTINEL_91bc"
HINT = "HINTS_SENTINEL_c20d"
FAIL_TO_PASS = "FAIL_TO_PASS_SENTINEL_e4a1"
PASS_TO_PASS = "PASS_TO_PASS_SENTINEL_5d08"


def row(instance_id, problem="Fix the parser so it accepts empty input."):
    return {"instance_id": instance_id, "repo": "example/project", "base_commit": "0" * 40,
            "problem_statement": problem, "patch": GOLD, "test_patch": TEST_PATCH,
            "hints_text": HINT, "FAIL_TO_PASS": FAIL_TO_PASS, "PASS_TO_PASS": PASS_TO_PASS}


def snapshot(root):
    """Relative path -> bytes for every file outside .git."""
    files = {}
    for path in root.rglob("*"):
        if path.is_file() and ".git" not in path.relative_to(root).parts:
            files[path.relative_to(root).as_posix()] = path.read_bytes()
    return files


class SelectionTests(unittest.TestCase):
    def test_selection_is_seeded_and_independent_of_input_order(self):
        rows = [row(f"repo__case-{i:02d}") for i in range(8)]
        first = swe_bench.select_instances(rows, 4, seed=5)
        self.assertEqual(first, swe_bench.select_instances(list(reversed(rows)), 4, seed=5))
        self.assertEqual(len(first), 4)
        self.assertEqual(len({r["instance_id"] for r in first}), 4)
        self.assertNotEqual([r["instance_id"] for r in first],
                            [r["instance_id"] for r in swe_bench.select_instances(rows, 4, seed=6)])

    def test_invalid_selections_are_rejected(self):
        rows = [row("a"), row("b")]
        for name, data, count in (
            ("duplicate ids", [row("a"), row("a")], 1),
            ("zero count", rows, 0),
            ("count above population", rows, 3),
        ):
            with self.subTest(name), self.assertRaises(ValueError):
                swe_bench.select_instances(data, count, seed=0)


class PromptTests(unittest.TestCase):
    def test_prompt_contains_the_issue_but_no_gold_or_test_metadata(self):
        prompt = swe_bench.prompt_for(row("repo__case-1"), Path("/tmp/helper.json"))
        self.assertIn("Fix the parser so it accepts empty input.", prompt)
        for sentinel in (GOLD, TEST_PATCH, HINT, FAIL_TO_PASS, PASS_TO_PASS):
            self.assertNotIn(sentinel, prompt)

    def test_prompt_points_only_to_the_helper_descriptor_not_dataset_answers(self):
        helper = Path(tempfile.gettempdir()) / "pilot-helper.json"
        prompt = swe_bench.prompt_for(row("repo__case-2"), helper)
        self.assertIn(str(helper), prompt)
        self.assertNotIn("dataset.json", prompt)


class WorkspacePatchTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        self.workspace = self.root / "work"
        self.workspace.mkdir()
        (self.workspace / "edit.txt").write_text("original\n")
        (self.workspace / "remove.txt").write_text("doomed\n")
        (self.workspace / "keep.txt").write_text("untouched\n")

    def tearDown(self):
        self.temporary.cleanup()

    def apply_to_pristine(self, patch):
        pristine = self.root / "pristine"
        shutil.copytree(self.base_copy, pristine)
        subprocess.run(["git", "apply", "-"], cwd=pristine, input=patch.encode(), check=True,
                       capture_output=True)
        return pristine

    def test_clean_workspace_yields_an_empty_patch(self):
        swe_bench.initialize_workspace(self.workspace)
        self.assertEqual(swe_bench.working_patch(self.workspace), "")

    def test_patch_covers_every_agent_change_and_applies_to_the_pristine_base(self):
        swe_bench.initialize_workspace(self.workspace)
        self.base_copy = self.root / "base-copy"
        shutil.copytree(self.workspace, self.base_copy)
        expected_base = snapshot(self.base_copy)

        # Unstaged edit, deletion, untracked new file (including binary bytes).
        (self.workspace / "edit.txt").write_text("changed\n")
        (self.workspace / "remove.txt").unlink()
        (self.workspace / "new.txt").write_text("fresh\n")
        (self.workspace / "blob.bin").write_bytes(b"\x00\x01\xff binary")
        # Staged new file.
        (self.workspace / "staged.txt").write_text("staged\n")
        swe_bench.git(self.workspace, "add", "staged.txt")
        # Committed change to an existing file and a new commit.
        (self.workspace / "keep.txt").write_text("committed edit\n")
        swe_bench.git(self.workspace, "add", "keep.txt")
        swe_bench.git(self.workspace, "-c", "user.name=agent", "-c", "user.email=agent@localhost",
                      "commit", "-qm", "agent commit")

        patch = swe_bench.working_patch(self.workspace)
        for name in ("edit.txt", "remove.txt", "new.txt", "blob.bin", "staged.txt", "keep.txt"):
            self.assertIn(f" b/{name}" if "remove" not in name else f"a/{name}", patch)
        self.assertIn("+committed edit", patch)

        pristine = self.apply_to_pristine(patch)
        self.assertEqual(snapshot(pristine), snapshot(self.workspace))
        self.assertNotEqual(snapshot(pristine), expected_base)

    def test_patch_is_read_only_with_respect_to_the_agent_history(self):
        swe_bench.initialize_workspace(self.workspace)
        head = swe_bench.git(self.workspace, "rev-parse", "HEAD").decode().strip()
        (self.workspace / "staged.txt").write_text("staged\n")
        swe_bench.git(self.workspace, "add", "staged.txt")
        (self.workspace / "new.txt").write_text("x\n")
        index = self.workspace / ".git" / "index"
        index_before = index.read_bytes()
        swe_bench.working_patch(self.workspace)
        # Sandboxes may protect .git: rendering the patch must not rewrite the index or HEAD.
        self.assertEqual(index.read_bytes(), index_before)
        self.assertEqual(swe_bench.git(self.workspace, "rev-parse", "HEAD").decode().strip(), head)

    def test_patch_renders_mode_changes_symlinks_and_executables_that_apply(self):
        swe_bench.initialize_workspace(self.workspace)
        self.base_copy = self.root / "base-copy"
        shutil.copytree(self.workspace, self.base_copy)
        (self.workspace / "keep.txt").chmod(0o755)
        script = self.workspace / "run.sh"
        script.write_text("#!/bin/sh\necho ok\n")
        script.chmod(0o755)
        os.symlink("edit.txt", self.workspace / "alias.txt")
        patch = swe_bench.working_patch(self.workspace)
        self.assertIn("new file mode 100755", patch)
        self.assertIn("new file mode 120000", patch)
        self.assertIn("old mode 100644", patch)
        pristine = self.apply_to_pristine(patch)
        self.assertTrue(os.access(pristine / "run.sh", os.X_OK))
        self.assertTrue(os.access(pristine / "keep.txt", os.X_OK))
        self.assertEqual(os.readlink(pristine / "alias.txt"), "edit.txt")
        self.assertEqual(snapshot(pristine), snapshot(self.workspace))


class ComparisonAgentTests(unittest.TestCase):
    def test_real_config_gets_matching_isolation_flags_and_renders(self):
        # The shared default catalog also lists dashboard-only agents; prepare selects the pair.
        agents = {a["name"]: a for a in swe_bench.comparison_agents(ROOT / "cross-agent.toml")}
        self.assertEqual(set(agents), {"dial", "omp"})
        for template in ("first", "follow_up"):
            values = {"model": "m", "thinking": "off", "timeout": "5", "prompt": "p"}
            for agent in agents.values():
                rendered = cross_agent.render(agent[template], values)
                self.assertIn("p", rendered)
                self.assertIn("anthropic/m", rendered)
            omp = agents["omp"][template]
            self.assertIn("--no-extensions", omp)
            self.assertIn("--plan", omp)
            self.assertNotIn("--no-extensions", agents["dial"][template])

    def test_explicit_pair_selection_still_requires_both_agents(self):
        body = 'name = "{name}"\noutput = "dial-json"\nfirst = ["x", "{{prompt}}"]\nfollow_up = ["x", "{{prompt}}"]\n'
        for names in (("dial", "omp", "other"), ("dial", "other")):
            with self.subTest(names=names), tempfile.TemporaryDirectory() as directory:
                path = Path(directory) / "agents.toml"
                path.write_text("".join(f"[[agents]]\n{body.format(name=name)}" for name in names))
                if "omp" not in names:
                    with self.assertRaises(ValueError):
                        swe_bench.comparison_agents(path, select_pair=True)
                else:
                    self.assertEqual([a["name"] for a in swe_bench.comparison_agents(path, select_pair=True)],
                                     ["dial", "omp"])

    def test_wrong_agent_set_is_rejected(self):
        body = 'name = "{name}"\noutput = "dial-json"\nfirst = ["x", "{{prompt}}"]\nfollow_up = ["x", "{{prompt}}"]\n'
        cases = {
            "missing omp": f'[[agents]]\n{body.format(name="dial")}',
            "extra agent": "".join(f"[[agents]]\n{body.format(name=n)}" for n in ("dial", "omp", "other")),
        }
        for label, text in cases.items():
            with self.subTest(label), tempfile.TemporaryDirectory() as directory:
                path = Path(directory) / "agents.toml"
                path.write_text(text)
                with self.assertRaises(ValueError):
                    swe_bench.comparison_agents(path)


class SummaryTests(unittest.TestCase):
    AGENTS = ["dial", "omp"]

    def test_missing_and_unscored_grades_are_unavailable_not_dropped_or_solved(self):
        grades = [
            {"agent": "dial", "instance_id": "a", "resolved": True},
            {"agent": "dial", "instance_id": "b", "resolved": False},
            {"agent": "omp", "instance_id": "a", "resolved": None},
        ]
        summary = swe_bench.summarize(grades, self.AGENTS, total=3)
        self.assertEqual(summary["dial"], {
            "selected_tasks": 3, "recorded_tasks": 2, "resolved": 1, "unresolved": 1,
            "unavailable": 1, "resolved_fraction_selected": 1 / 3, "complete": False})
        self.assertEqual(summary["omp"], {
            "selected_tasks": 3, "recorded_tasks": 1, "resolved": 0, "unresolved": 0,
            "unavailable": 3, "resolved_fraction_selected": 0.0, "complete": False})

    def test_complete_grading_has_no_unavailable_tasks(self):
        grades = [{"agent": name, "instance_id": i, "resolved": True}
                  for name in self.AGENTS for i in ("a", "b")]
        summary = swe_bench.summarize(grades, self.AGENTS, total=2)
        for name in self.AGENTS:
            self.assertEqual((summary[name]["unavailable"], summary[name]["complete"]), (0, True))
            self.assertEqual(summary[name]["resolved_fraction_selected"], 1.0)


class FrozenContractTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)

    def tearDown(self):
        self.temporary.cleanup()

    def write_run(self, **overrides):
        dataset = json.dumps([row("a")]).encode()
        (self.root / "dataset.json").write_bytes(dataset)
        contract = {"dataset_sha256": hashlib.sha256(dataset).hexdigest(), "seed": 0,
                    "runner_sha256": hashlib.sha256(Path(swe_bench.__file__).read_bytes()).hexdigest(),
                    "cross_agent_sha256": hashlib.sha256(Path(cross_agent.__file__).read_bytes()).hexdigest(),
                    **overrides}
        (self.root / "contract.json").write_text(json.dumps(contract))
        self.reseal()

    def reseal(self):
        digest = hashlib.sha256((self.root / "contract.json").read_bytes()).hexdigest()
        (self.root / "prepared.json").write_text(json.dumps({"contract_sha256": digest}))

    def test_untouched_frozen_run_loads(self):
        self.write_run()
        self.assertEqual(swe_bench.load_contract(self.root)["seed"], 0)

    def test_each_frozen_input_mutation_is_rejected(self):
        mutations = {
            "dataset edited": lambda: (self.root / "dataset.json").write_text("[]"),
            "contract edited without re-preparation": lambda: (self.root / "contract.json").write_text(
                json.dumps({**json.loads((self.root / "contract.json").read_text()), "seed": 9})),
            "runner changed": lambda: self.rewrite_contract(runner_sha256="0" * 64),
            "cross-agent runner changed": lambda: self.rewrite_contract(cross_agent_sha256="0" * 64),
        }
        for label, mutate in mutations.items():
            with self.subTest(label):
                self.temporary.cleanup()
                self.temporary = tempfile.TemporaryDirectory()
                self.root = Path(self.temporary.name)
                self.write_run()
                mutate()
                if label == "contract edited without re-preparation":
                    (self.root / "prepared.json").write_text(json.dumps({"contract_sha256": "stale"}))
                with self.assertRaises(ValueError):
                    swe_bench.load_contract(self.root)

    def rewrite_contract(self, **changes):
        contract = {**json.loads((self.root / "contract.json").read_text()), **changes}
        (self.root / "contract.json").write_text(json.dumps(contract))
        self.reseal()


class PrepareWorkspaceCallbackTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        (self.root / "fixture").mkdir()
        (self.root / "fixture" / "seed.txt").write_text("seed")
        # Records whether the callback's marker existed when the CLI started.
        probe = ("import os, sys; open('seen.txt', 'w').write(str(os.path.exists('prepared.txt')))")
        self.agent = {"name": "stub", "output": "dial-json",
                      "first": [sys.executable, "-c", probe, "{prompt}"],
                      "follow_up": [sys.executable, "-c", probe, "{prompt}"],
                      "env": {}, "delegation_tools": ["task"], "delegated_usage": "unknown"}

    def tearDown(self):
        self.temporary.cleanup()

    def test_callback_runs_on_the_copied_workspace_before_the_cli_starts(self):
        calls = []

        def prepare(workspace):
            calls.append(workspace)
            (workspace / "prepared.txt").write_text("x")

        record = cross_agent.run_trial(self.agent, {"id": "p", "project": "fixture", "prompt": "go",
                                                    "checks": []},
                                       self.root, 0, self.root, {"model": "m"}, 30, True, None, prepare)
        workspace = Path(record["workspace"])
        self.assertEqual(calls, [workspace])
        self.assertEqual((workspace / "seen.txt").read_text(), "True")
        self.assertEqual((workspace / "seed.txt").read_text(), "seed")

    def test_without_a_callback_the_cli_sees_no_marker(self):
        record = cross_agent.run_trial(self.agent, {"id": "n", "project": "fixture", "prompt": "go",
                                                    "checks": []},
                                       self.root, 0, self.root, {"model": "m"}, 30, True, None)
        self.assertEqual((Path(record["workspace"]) / "seen.txt").read_text(), "False")


class DialSandboxFlagTests(unittest.TestCase):
    CONFIG = ROOT / "cross-agent.toml"

    def test_default_comparison_adds_no_sandbox_flag(self):
        for agent in swe_bench.comparison_agents(self.CONFIG, select_pair=True):
            for template in ("first", "follow_up"):
                with self.subTest(agent=agent["name"], template=template):
                    self.assertNotIn("--sandbox", agent[template])

    def test_explicit_opt_out_inserts_sandbox_off_into_dial_templates_only(self):
        default = {a["name"]: a for a in swe_bench.comparison_agents(self.CONFIG, select_pair=True)}
        opted = {a["name"]: a for a in swe_bench.comparison_agents(self.CONFIG, dial_sandbox_off=True, select_pair=True)}
        for template in ("first", "follow_up"):
            flags = opted["dial"][template]
            index = flags.index("--sandbox")
            self.assertEqual(flags[index + 1], "off")
            self.assertEqual(flags.count("--sandbox"), 1)
            # Only the two sandbox tokens differ from the default Dial command.
            self.assertEqual(flags[:index] + flags[index + 2:], default["dial"][template])
            self.assertEqual(opted["omp"][template], default["omp"][template])
            self.assertNotIn("--sandbox", opted["omp"][template])


class ContainerHelperTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.workspace = self.root / "work"
        self.workspace.mkdir()
        (self.workspace / "edit.txt").write_text("original\n")
        swe_bench.initialize_workspace(self.workspace)
        self.helper = self.root / "helper.json"
        self.helper.write_text(json.dumps({"workspace": str(self.workspace), "image": "img/test:1"}))
        self.real_run = subprocess.run
        self.docker_calls = []
        self.run_error = None
        self.run_returncode = 0
        patcher = mock.patch.object(subprocess, "run", side_effect=self.fake_run)
        patcher.start()
        self.addCleanup(patcher.stop)

    def fake_run(self, argv, *args, **kwargs):
        # Only docker is faked; git runs for real against the local temporary workspace.
        if argv[0] != "docker":
            return self.real_run(argv, *args, **kwargs)
        self.docker_calls.append((list(argv), kwargs))
        if argv[1] == "run":
            if self.run_error is not None:
                raise self.run_error
            return subprocess.CompletedProcess(argv, self.run_returncode)
        if argv[1] == "ps":
            # Simulate the label lookup: report every container this test started.
            names = "".join(a[a.index("--name") + 1] + "\n" for a, _ in self.runs())
            return subprocess.CompletedProcess(argv, 0, stdout=names if kwargs.get("text") else names.encode())
        return subprocess.CompletedProcess(argv, 0)

    def run_helper(self, command):
        return swe_bench.test(types.SimpleNamespace(helper=self.helper, command=["--", *command]))

    def runs(self):
        return [(argv, kwargs) for argv, kwargs in self.docker_calls if argv[1] == "run"]

    def removals(self):
        return [argv for argv, _ in self.docker_calls if argv[1:3] == ["rm", "-f"]]

    def test_nonempty_patch_is_piped_into_an_isolated_container(self):
        (self.workspace / "edit.txt").write_text("changed\n")
        command = ["python", "-m", "pytest", "tests/test_x.py"]
        self.assertEqual(self.run_helper(command), 0)
        (argv, kwargs), = self.runs()
        self.assertIn(b"+changed", kwargs["input"])
        self.assertIn(b"edit.txt", kwargs["input"])
        self.assertEqual(argv[argv.index("--network") + 1], "none")
        self.assertIn("-i", argv)
        self.assertIn("img/test:1", argv)
        self.assertEqual(argv[argv.index("--label") + 1], f"dial.swebench.helper={self.helper.resolve()}")
        self.assertEqual(argv[-len(command):], command)
        # No host mount, volume, privileged mode, Docker socket or host networking.
        for forbidden in ("-v", "--volume", "--mount", "--privileged", "--network=host"):
            self.assertNotIn(forbidden, argv)
        self.assertFalse(any("docker.sock" in arg or arg.startswith(("--mount", "--volume")) for arg in argv))

    def test_clean_workspace_still_runs_with_an_empty_patch(self):
        self.assertEqual(self.run_helper(["pytest"]), 0)
        (argv, kwargs), = self.runs()
        self.assertEqual(kwargs["input"], b"")

    def test_success_returns_the_container_status_and_removes_it(self):
        self.run_returncode = 3
        self.assertEqual(self.run_helper(["pytest"]), 3)
        (argv, _), = self.runs()
        name = argv[argv.index("--name") + 1]
        self.assertTrue(name.startswith("swebench-test-"))
        self.assertEqual(self.removals(), [["docker", "rm", "-f", name]])

    def test_timeout_still_force_removes_the_named_container(self):
        self.run_error = subprocess.TimeoutExpired(cmd=["docker", "run"], timeout=600)
        with self.assertRaises(subprocess.TimeoutExpired):
            self.run_helper(["pytest"])
        (argv, _), = self.runs()
        name = argv[argv.index("--name") + 1]
        self.assertEqual(self.removals(), [["docker", "rm", "-f", name]])


class ContainerCleanupTests(unittest.TestCase):
    def test_cleanup_removes_only_the_containers_listed_for_this_helper(self):
        helper = Path(tempfile.gettempdir()) / "cleanup-helper.json"
        with mock.patch.object(subprocess, "check_output", return_value="c1\nc2\n") as listing, \
                mock.patch.object(subprocess, "run") as removal:
            swe_bench.cleanup_test_containers(helper)
        self.assertIn(f"label=dial.swebench.helper={helper.resolve()}", listing.call_args[0][0])
        removal.assert_called_once()
        self.assertEqual(removal.call_args[0][0], ["docker", "rm", "-f", "c1", "c2"])

    def test_cleanup_with_no_matching_container_removes_nothing(self):
        with mock.patch.object(subprocess, "check_output", return_value=""), \
                mock.patch.object(subprocess, "run") as removal:
            swe_bench.cleanup_test_containers(Path(tempfile.gettempdir()) / "empty-helper.json")
        removal.assert_not_called()


class GradeReportTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.output = Path(temporary.name).resolve()
        self.rows = [row("a"), row("b"), row("c")]
        self.local_ids = {f"img/{r['instance_id']}": f"sha256:{r['instance_id']}" for r in self.rows}
        self.outcomes = {}
        self.run_calls = []
        self.write_frozen_run()
        self.install_fake_modules()

    def write_frozen_run(self):
        dataset = json.dumps(self.rows).encode()
        (self.output / "dataset.json").write_bytes(dataset)
        (self.output / "manifest.json").write_text(json.dumps({"instances": [
            {"instance_id": r["instance_id"], "base_commit": r["base_commit"]} for r in self.rows]}))
        images = {r["instance_id"]: {"tag": f"img/{r['instance_id']}",
                                     "id": self.local_ids[f"img/{r['instance_id']}"],
                                     "source_sha256": "unused"} for r in self.rows}
        contract = {"kind": "swebench_cross_agent_pilot", "version": 1, "seed": 0, "model": "m",
                    "thinking": "off", "timeout_secs": 30,
                    "dataset_sha256": hashlib.sha256(dataset).hexdigest(),
                    "runner_sha256": hashlib.sha256(Path(swe_bench.__file__).read_bytes()).hexdigest(),
                    "cross_agent_sha256": hashlib.sha256(Path(cross_agent.__file__).read_bytes()).hexdigest(),
                    "agents": [{"name": "dial"}, {"name": "omp"}], "identities": {}, "images": images}
        (self.output / "contract.json").write_text(json.dumps(contract))
        digest = hashlib.sha256((self.output / "contract.json").read_bytes()).hexdigest()
        (self.output / "prepared.json").write_text(json.dumps({"contract_sha256": digest}))
        (self.output / "trials").mkdir()

    def install_fake_modules(self):
        outer = self

        class DockerException(Exception):
            pass

        class Images:
            def get(self, key):
                return types.SimpleNamespace(id=outer.local_ids[key])

        client = types.SimpleNamespace(images=Images())
        self.DockerException = DockerException
        docker = types.ModuleType("docker")
        docker.from_env = lambda: client
        docker.errors = types.SimpleNamespace(DockerException=DockerException)
        run_evaluation = types.ModuleType("swebench.harness.run_evaluation")
        run_evaluation.run_instance = self.run_instance
        test_spec = types.ModuleType("swebench.harness.test_spec.test_spec")
        test_spec.make_test_spec = lambda row, arch, namespace: types.SimpleNamespace(
            instance_image_key=f"img/{row['instance_id']}")
        modules = {"docker": docker, "swebench": types.ModuleType("swebench"),
                   "swebench.harness": types.ModuleType("swebench.harness"),
                   "swebench.harness.run_evaluation": run_evaluation,
                   "swebench.harness.test_spec": types.ModuleType("swebench.harness.test_spec"),
                   "swebench.harness.test_spec.test_spec": test_spec}
        patchers = [mock.patch.dict(sys.modules, modules),
                    mock.patch("importlib.metadata.version", return_value=swe_bench.HARNESS_VERSION)]
        for patcher in patchers:
            patcher.start()
            self.addCleanup(patcher.stop)

    def run_instance(self, spec, prediction, rm, force, client, run_id, timeout):
        # Mimics the official harness: writes report.json relative to the grading cwd.
        instance = prediction["instance_id"]
        agent = run_id.removeprefix("pilot-")
        self.run_calls.append((run_id, instance, timeout))
        outcome = self.outcomes.get((agent, instance), "true")
        if outcome == "docker":
            raise self.DockerException("daemon unavailable")
        if outcome == "incomplete":
            return {"completed": False}
        report = (Path("logs/run_evaluation") / run_id /
                  prediction["model_name_or_path"].replace("/", "__") / instance / "report.json")
        report.parent.mkdir(parents=True, exist_ok=True)
        report.write_text(json.dumps({instance: {"resolved": outcome == "true"}}))
        return {"completed": True}

    def write_attempt(self, agent, instance, patch="diff --git a/edit.txt b/edit.txt\n", elapsed_ms=1234):
        trials = self.output / "trials"
        usage = {**cross_agent.empty_usage(), "requests": 2, "tool_calls": 3}
        trial = {"agent": agent, "case": instance, "repetition": 0, "categories": [], "passed": False,
                 "scored": True, "agent_error": None, "elapsed_ms": elapsed_ms, "turns": [],
                 "efficiency": None, "usage": usage, "skipped_compactions": 0}
        (trials / f"{agent}-{instance}.json").write_text(json.dumps(trial))
        prediction = {"instance_id": instance, "model_name_or_path": f"{agent}-m", "model_patch": patch}
        (trials / f"{agent}-{instance}.prediction.json").write_text(json.dumps(prediction))

    def run_grade(self):
        with contextlib.redirect_stdout(io.StringIO()):
            return swe_bench.grade(types.SimpleNamespace(output=self.output, test_timeout=900))

    def report(self):
        return json.loads((self.output / "report.json").read_text())

    def test_final_report_separates_true_false_and_unavailable_with_trial_metadata(self):
        self.write_attempt("dial", "a", elapsed_ms=1111)
        self.write_attempt("dial", "b", elapsed_ms=2222)
        self.outcomes[("dial", "b")] = "false"
        self.write_attempt("dial", "c", patch="", elapsed_ms=3333)
        self.write_attempt("omp", "a", elapsed_ms=4444)
        self.outcomes[("omp", "a")] = "incomplete"
        self.write_attempt("omp", "c", patch="", elapsed_ms=5555)
        # omp/b has neither a trial record nor a prediction: it must stay unavailable.

        self.assertEqual(self.run_grade(), 1)
        report = self.report()
        grades = {(g["agent"], g["instance_id"]): g for g in report["grades"]}
        expected = {
            ("dial", "a"): (True, None),
            ("dial", "b"): (False, None),
            ("dial", "c"): (False, "empty_patch"),
            ("omp", "a"): (None, "official_report_unavailable; inspect run_instance.log"),
            ("omp", "c"): (False, "empty_patch"),
        }
        self.assertEqual({key: (g["resolved"], g["error"]) for key, g in grades.items()}, expected)
        self.assertEqual(report["summary"]["dial"], {
            "selected_tasks": 3, "recorded_tasks": 3, "resolved": 1, "unresolved": 2, "unavailable": 0,
            "resolved_fraction_selected": 1 / 3, "complete": True})
        self.assertEqual(report["summary"]["omp"], {
            "selected_tasks": 3, "recorded_tasks": 2, "resolved": 0, "unresolved": 1, "unavailable": 2,
            "resolved_fraction_selected": 0.0, "complete": False})
        trials = {(t["agent"], t["case"]): t for t in report["trials"]}
        self.assertEqual({key: t["passed"] for key, t in trials.items()}, {
            ("dial", "a"): True, ("dial", "b"): False, ("dial", "c"): False,
            ("omp", "a"): False, ("omp", "c"): False})
        self.assertFalse(trials[("omp", "a")]["scored"])
        self.assertTrue(trials[("dial", "b")]["scored"])
        self.assertEqual(trials[("dial", "a")]["elapsed_ms"], 1111)
        self.assertEqual(trials[("dial", "a")]["usage"]["tool_calls"], 3)
        self.assertEqual(report["by_case"], {
            "a": {"dial": {"passed": 1, "trials": 1, "resolved": True},
                  "omp": {"passed": 0, "trials": 1, "resolved": None}},
            "b": {"dial": {"passed": 0, "trials": 1, "resolved": False}},
            "c": {"dial": {"passed": 0, "trials": 1, "resolved": False},
                  "omp": {"passed": 0, "trials": 1, "resolved": False}}})
        self.assertEqual(report["execution"]["dial"]["scored_trials"], 3)
        self.assertEqual(report["execution"]["dial"]["passed_trials"], 1)
        self.assertEqual(report["execution"]["omp"]["scored_trials"], 1)
        self.assertEqual(self.run_calls, [("pilot-dial", "a", 900), ("pilot-dial", "b", 900),
                                          ("pilot-omp", "a", 900)])

    def test_existing_grade_record_is_reused_without_rerunning_the_grader(self):
        self.write_attempt("dial", "a")
        cached = {"agent": "dial", "instance_id": "a", "resolved": True, "error": None,
                  "completed": True, "report": "cached"}
        (self.output / "trials" / "dial-a.grade.json").write_text(json.dumps(cached))
        self.outcomes[("dial", "a")] = "false"  # would be wrong if the grader ran again
        self.run_grade()
        self.assertEqual(self.run_calls, [])
        self.assertEqual(self.report()["summary"]["dial"]["resolved"], 1)

    def test_docker_evaluation_error_is_unavailable_not_unresolved(self):
        self.write_attempt("dial", "a")
        self.outcomes[("dial", "a")] = "docker"
        self.run_grade()
        grade = self.report()["grades"][0]
        self.assertIsNone(grade["resolved"])
        self.assertEqual(grade["error"], "Docker evaluation error: DockerException")
        self.assertEqual(self.report()["summary"]["dial"]["unavailable"], 3)
        self.assertEqual(self.report()["summary"]["dial"]["unresolved"], 0)

    def test_changed_local_image_refuses_to_grade(self):
        self.write_attempt("dial", "a")
        self.local_ids["img/a"] = "sha256:other"
        with self.assertRaises(ValueError):
            self.run_grade()
        self.assertEqual(self.run_calls, [])


if __name__ == "__main__":
    unittest.main()
