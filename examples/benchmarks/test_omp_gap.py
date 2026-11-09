"""Evaluation selection and predeclared targets; no model calls or task execution."""
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch
import omp_gap


class ContractTests(unittest.TestCase):
    def test_prior_tasks_are_excluded_and_selection_never_reads_instructions(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            for name in ["old", "a", "b", "gpu"]:
                task = root / name
                task.mkdir()
                (task / "task.toml").write_text("[environment]\ngpus=" + ("1" if name == "gpu" else "0"))
                (task / "instruction.md").write_text("unexamined task text")
            prior = root / "prior.json"
            prior.write_text(json.dumps({"tasks":["terminal-bench/old"]}))
            original = Path.read_text
            def metadata_only(path, *args, **kwargs):
                if path.name == "instruction.md":
                    raise AssertionError("selection must not inspect task instructions")
                return original(path, *args, **kwargs)
            with patch.object(Path, "read_text", metadata_only):
                contract = omp_gap.freeze_contract(root, [prior], 2)
            self.assertEqual(set(contract["held_out_tasks"]), {"a", "b"})
            self.assertEqual(contract["repeats"], 3)
            self.assertEqual(contract["target"]["maximum_output_token_ratio"], 1.3)
            self.assertIn("filter-js-from-html", contract["original_tasks"])
            self.assertIn("a/instruction.md", contract["task_files"])
