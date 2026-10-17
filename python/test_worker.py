import json
import math
import tempfile
import unittest
from pathlib import Path

from dial_worker import (
    PREFERENCE_PAIRS_PER_BATCH,
    RequestError,
    collect_preference_pairs,
    completion_mask,
    layer_scores,
    pack_preference_batch,
    preference_iterations,
    preference_objective,
    read_datasets,
    read_records,
    sensitivity_request,
    tokenize_preference_pairs,
    trajectory_group,
)


class ProvenanceTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)

    def record(self, answer="4"):
        return {"messages": [{"role": "user", "content": "What is 2 + 2?"},
                             {"role": "assistant", "content": answer}],
                "source": "external arithmetic reference", "verified": True}

    def dataset(self, name, records):
        path = self.root / name
        path.write_text("".join(json.dumps(record) + "\n" for record in records))
        return path

    def test_backend_receives_only_verified_messages(self):
        record = self.record()
        result, _ = read_records(self.dataset("train.jsonl", [record]))
        self.assertEqual(result, [{"messages": record["messages"]}])

    def test_provenance_is_required_and_boolean_true(self):
        for field, value in [("source", ""), ("source", None),
                             ("verified", False), ("verified", 1), ("verified", "true")]:
            with self.subTest(field=field, value=value):
                record = self.record()
                record[field] = value
                with self.assertRaises(RequestError):
                    read_records(self.dataset("train.jsonl", [record]))
        record = self.record()
        del record["source"]
        with self.assertRaises(RequestError):
            read_records(self.dataset("train.jsonl", [record]))

    def test_agent_selected_command_exit_is_not_a_verified_outcome(self):
        for source in ["check:exit0", "dial:verified:12:check:exit0"]:
            with self.subTest(source=source):
                record = self.record()
                record["source"] = source
                with self.assertRaises(RequestError):
                    read_records(self.dataset("train.jsonl", [record]))

    def test_split_overlap_ignores_provenance_and_key_order(self):
        record = self.record()
        duplicate = self.record()
        duplicate["source"] = "a different external reference"
        duplicate["messages"] = [dict(reversed(list(m.items()))) for m in record["messages"]]
        with self.assertRaisesRegex(RequestError, "overlap"):
            read_datasets(self.dataset("train.jsonl", [record]),
                          self.dataset("valid.jsonl", [duplicate]))

    def test_disjoint_verified_splits_are_accepted(self):
        first, second = self.record(), self.record("Five.")
        second["messages"][0]["content"] = "What is 2 + 3?"
        train, valid = read_datasets(self.dataset("train.jsonl", [first]),
                                     self.dataset("valid.jsonl", [second]))
        self.assertEqual(train, [{"messages": first["messages"]}])
        self.assertEqual(valid, [{"messages": second["messages"]}])

    def test_same_prompt_with_different_answer_cannot_cross_splits(self):
        with self.assertRaisesRegex(RequestError, "prompts overlap"):
            read_datasets(self.dataset("train.jsonl", [self.record()]),
                          self.dataset("valid.jsonl", [self.record("Four.")]))

    def test_conversation_prefix_and_later_turn_cannot_cross_splits(self):
        first = self.record()
        later = self.record()
        later["messages"][0]["content"] = "  What is 2 + 2? \n"
        later["messages"].extend([{"role": "user", "content": "And 2 + 3?"},
                                  {"role": "assistant", "content": "5"}])
        with self.assertRaisesRegex(RequestError, "prompts overlap"):
            read_datasets(self.dataset("train.jsonl", [first]),
                          self.dataset("valid.jsonl", [later]))

    def test_compacted_trajectory_cannot_cross_splits_under_different_prompts(self):
        first, later = self.record(), self.record("5")
        first["source"] = "dial:tool-recovery:12"
        later["source"] = "dial:verified:12:verifier:arithmetic"
        later["messages"][0]["content"] = "And 2 + 3?"
        with self.assertRaisesRegex(RequestError, "trajectories overlap"):
            read_datasets(self.dataset("train.jsonl", [first]),
                          self.dataset("valid.jsonl", [later]))

    def test_shared_trajectory_provenance_golden_cases(self):
        fixture = Path(__file__).resolve().parents[1] / "tests/fixtures/trajectory_provenance.json"
        for case in json.loads(fixture.read_text()):
            with self.subTest(source=case["source"]):
                self.assertEqual(trajectory_group(case["source"]), case["group"])

    def test_oversized_trajectory_id_does_not_raise_or_lose_leading_zeroes(self):
        self.assertIsNone(trajectory_group("dial:verified:" + "9" * 5000))
        self.assertEqual(trajectory_group("dial:verified:" + "0" * 5000 + "7"), "trajectory:7")

    def test_learned_trajectories_are_origin_qualified_across_splits(self):
        first, later = self.record(), self.record("5")
        first["source"] = "dial:learned:aaaa:12:tool-recovery"
        later["messages"][0]["content"] = "And 2 + 3?"
        for source, overlaps in [("dial:learned:aaaa:12:verifier:test", True),
                                 ("dial:learned:bbbb:12:verifier:test", False),
                                 ("dial:verified:12:verifier:test", False)]:
            later["source"] = source
            paths = (self.dataset("train.jsonl", [first]), self.dataset("valid.jsonl", [later]))
            with self.subTest(source=source):
                if overlaps:
                    with self.assertRaisesRegex(RequestError, "trajectories overlap"):
                        read_datasets(*paths)
                else:
                    read_datasets(*paths)

    def test_empty_and_unanswered_data_are_rejected(self):
        with self.assertRaises(RequestError):
            read_records(self.dataset("train.jsonl", []))
        record = self.record()
        record["messages"].pop()
        with self.assertRaises(RequestError):
            read_records(self.dataset("train.jsonl", [record]))
        record = self.record()
        record["messages"][-1]["content"] = " "
        with self.assertRaises(RequestError):
            read_records(self.dataset("train.jsonl", [record]))


class ToolCallRecordTests(ProvenanceTests):
    def tool_record(self, arguments=None):
        call = {"id": "c1", "type": "function",
                "function": {"name": "read", "arguments": {"path": "a.txt"} if arguments is None else arguments}}
        tools = [{"type": "function", "function": {"name": "read", "parameters": {"type": "object"}}}]
        return {"messages": [{"role": "system", "content": "s"}, {"role": "user", "content": "read a.txt"},
                             {"role": "assistant", "content": None, "tool_calls": [call]}],
                "tools": tools, "source": "dial:tool-recovery:1", "verified": True}

    def test_tool_call_turns_and_tool_schemas_reach_the_backend(self):
        record = self.tool_record()
        result, _ = read_records(self.dataset("train.jsonl", [record]))
        self.assertEqual(result, [{"messages": record["messages"], "tools": record["tools"]}])

    def test_tool_call_arguments_must_be_objects_not_strings(self):
        with self.assertRaises(RequestError):
            read_records(self.dataset("train.jsonl", [self.tool_record("{\"path\": \"a.txt\"}")]))

    def test_tool_calls_are_only_accepted_on_assistant_turns(self):
        record = self.tool_record()
        record["messages"][1]["tool_calls"] = record["messages"][2]["tool_calls"]
        with self.assertRaises(RequestError):
            read_records(self.dataset("train.jsonl", [record]))

    def test_provenance_still_required_for_tool_records(self):
        record = self.tool_record()
        record["verified"] = False
        with self.assertRaises(RequestError):
            read_records(self.dataset("train.jsonl", [record]))



class PreferencePairTests(ProvenanceTests):
    def pair(self):
        context = [{"role": "user", "content": "Read the file"}]
        tools = [{"type": "function", "function": {
            "name": "read", "parameters": {"type": "object"}}}]
        call = {"role": "assistant", "content": None, "tool_calls": [{
            "id": "c1", "type": "function",
            "function": {"name": "read", "arguments": {"path": "a"}}}]}
        return {"context": context, "tools": tools, "chosen": call,
                "rejected": {"role": "assistant", "content": "malformed attempt"}}

    def test_verified_pairs_are_read_and_invalid_pairs_rejected(self):
        record = self.record()
        record["source"] = "dial:tool-recovery:1"
        record["preference_pairs"] = [self.pair()]
        entries, _ = read_records(self.dataset("pairs.jsonl", [record]))
        self.assertEqual(entries[0]["preference_pairs"], [self.pair()])
        record["preference_pairs"][0]["rejected"] = {"role": "tool", "content": "bad", "tool_call_id": "x"}
        with self.assertRaises(RequestError):
            read_records(self.dataset("bad-pairs.jsonl", [record]))

    def test_pair_tokenization_preserves_completion_only_offsets(self):
        class Tokenizer:
            def apply_chat_template(self, conversation, tools, add_generation_prompt, tokenize):
                if add_generation_prompt:
                    return [1, 2]
                answer = conversation[-1]["content"] or "tool-call"
                return [1, 2, 3, 4 if answer == "chosen" else 5]

        pair = self.pair()
        pair["chosen"] = {"role": "assistant", "content": "chosen"}
        pair["rejected"] = {"role": "assistant", "content": "rejected"}
        tokenized = tokenize_preference_pairs([pair], Tokenizer(), 8)
        self.assertEqual(tokenized, [(([1, 2, 3, 4], 2), ([1, 2, 3, 5], 2))])
        self.assertEqual(completion_mask(4, 2), [False, False, True, True])
        with self.assertRaises(RequestError):
            completion_mask(4, 4)
        with self.assertRaisesRegex(RequestError, "exceeds max_seq_length"):
            tokenize_preference_pairs([pair], Tokenizer(), 3)

    def test_deterministic_logits_favor_chosen_completion(self):
        def selected_logp(logits, target):
            peak = max(logits)
            log_normalizer = peak + math.log(sum(math.exp(value - peak) for value in logits))
            return logits[target] - log_normalizer

        chosen_logp = selected_logp([3.0, 0.0], 0)
        rejected_logp = selected_logp([0.0, 3.0], 0)
        chosen_favored = preference_objective(0.1, chosen_logp, rejected_logp)
        reversed_preference = preference_objective(0.1, rejected_logp, chosen_logp)
        self.assertLess(chosen_favored, reversed_preference)

    def test_pair_and_iteration_caps_and_empty_pair_noop(self):
        pairs = [self.pair() for _ in range(40)]
        records = [{"preference_pairs": pairs}]
        self.assertEqual(len(collect_preference_pairs(records)), 32)
        self.assertEqual(preference_iterations(2, 32), 2)
        self.assertEqual(preference_iterations(100, 3), 1)
        self.assertEqual(preference_iterations(100, 32), 8)
        pair = (([1, 2, 3, 4], 2), ([1, 2, 3, 5], 2))
        packed, lengths = pack_preference_batch([pair] * PREFERENCE_PAIRS_PER_BATCH, 8)
        self.assertEqual(len(packed), 2 * PREFERENCE_PAIRS_PER_BATCH)
        self.assertEqual(len(lengths), len(packed))
        self.assertEqual(collect_preference_pairs([{"messages": self.record()["messages"]}]), [])


class SensitivityHelperTests(unittest.TestCase):
    def request(self, **overrides):
        request = {"op": "sensitivity", "model": "m", "train_data": "t.jsonl",
                   "rank": 8, "samples": 4, "max_seq_length": 512}
        request.update(overrides)
        return request

    def test_request_bounds(self):
        self.assertEqual(sensitivity_request(self.request()), ("m", "t.jsonl", 8, 4, 512))
        self.assertEqual(sensitivity_request(self.request(rank=128))[2], 128)
        for key, value in [("rank", 0), ("rank", 129), ("samples", 0), ("samples", 65),
                           ("max_seq_length", 31), ("model", " "), ("train_data", None)]:
            with self.subTest(key=key, value=value):
                with self.assertRaises(RequestError):
                    sensitivity_request(self.request(**{key: value}))

    def test_scores_are_per_layer_norms(self):
        squares = {"layers.0.self_attn.q_proj.lora_b": 9.0, "layers.0.self_attn.q_proj.lora_a": 0.0,
                   "layers.2.self_attn.q_proj.lora_b": 16.0}
        self.assertEqual(layer_scores(squares, 3), [3.0, 0.0, 4.0])

    def test_scores_reject_foreign_or_nonfinite_parameters(self):
        with self.assertRaises(RequestError):
            layer_scores({"embed.lora_b": 1.0}, 2)
        with self.assertRaises(RequestError):
            layer_scores({"layers.2.q_proj.lora_b": 1.0}, 2)
        with self.assertRaises(RequestError):
            layer_scores({"layers.0.q_proj.lora_b": float("inf")}, 2)
        with self.assertRaises(RequestError):
            layer_scores({"layers.0.q_proj.lora_b": -1.0}, 2)


if __name__ == "__main__":
    unittest.main()
