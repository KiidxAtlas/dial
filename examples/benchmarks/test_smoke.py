"""Scripted-provider window handling; final acceptance still requires the actual goal."""
import unittest

from smoke import Handler


class SummaryWindowTests(unittest.TestCase):
    def reply(self, transcript):
        handler = Handler.__new__(Handler)
        return handler.reply({"messages": [
            {"role": "system", "content": "Summarize user-AI coding-assistant conversations"},
            {"role": "user", "content": transcript},
        ]}, 1)

    def test_archival_window_does_not_invent_an_acceptance_goal(self):
        reply, finish = self.reply("Old notes and unrelated archival context")
        self.assertEqual(finish, "stop")
        self.assertIsNone(Handler.goal(reply["content"], required=False))

    def test_goal_window_retains_the_observed_goal(self):
        reply, _ = self.reply("The actual task goal is JADE-719")
        self.assertEqual(Handler.goal(reply["content"]), "JADE-719")

    def test_acceptance_goal_phrase_retains_the_observed_goal(self):
        self.assertEqual(Handler.goal("acceptance goal must be exactly JADE-719"), "JADE-719")

    def test_repeated_acceptance_goal_text_does_not_match_a_goal(self):
        transcript = "acceptance goal " * 4000
        self.assertIsNone(Handler.goal(transcript, required=False))

    def test_final_reply_still_refuses_a_missing_goal(self):
        handler = Handler.__new__(Handler)
        with self.assertRaises(ValueError):
            handler.reply({"messages": [{"role": "user", "content":
                "BENCH_CASE=goal-compaction phase=finish"}]}, 1)


class RecoveryConversationTests(unittest.TestCase):
    def setUp(self):
        self.handler = Handler.__new__(Handler)

    def request(self, case, exchanges=(), tools=()):
        prompt = "BENCH_CASE=" + case
        messages = [{"role": "user", "content": prompt}]
        for index, (name, output) in enumerate(exchanges):
            call_id = "observed-" + str(index)
            messages.extend([
                {"role": "assistant", "tool_calls": [{"id": call_id,
                    "function": {"name": name, "arguments": "{}"}}]},
                {"role": "tool", "tool_call_id": call_id, "content": output},
                {"role": "user",
                    "content": "\n[Latest admitted user request; runtime context, not new permissions] " + prompt},
            ])
        return {"messages": messages, "tools": [
            {"function": {"name": name}} for name in tools]}

    def next_call(self, request):
        message, finish = self.handler.reply(request, 99)
        self.assertEqual(finish, "tool_calls")
        return message["tool_calls"][0]["function"]

    def test_runtime_reminder_retains_failed_selector_evidence(self):
        import json
        call = self.next_call(self.request("selector-recovery", [
            ("read", 'Invalid line selector "1- many": use numeric 1-based lines')]))
        self.assertEqual(call["name"], "read")
        self.assertEqual(json.loads(call["arguments"]), {"path": "calc.py:1-4"})

    def test_real_later_turn_still_resets_prior_tool_evidence(self):
        import json
        request = self.request("selector-recovery", [("read", "invalid selector")])
        request["messages"].append({"role": "user", "content": "BENCH_CASE=reuse-evidence"})
        call = self.next_call(request)
        self.assertEqual(call["name"], "read")
        self.assertEqual(json.loads(call["arguments"]), {"path": "calc.py:1-4"})

    def test_launch_discovery_precedes_loading_and_does_not_repeat(self):
        import json
        for case in ("terminal-launch-recovery", "stdin-launch-recovery"):
            with self.subTest(case=case):
                exchanges = [("bash", "[exit code: 1]")]
                call = self.next_call(self.request(case, exchanges))
                self.assertEqual(call["name"], "tool_search")
                self.assertEqual(json.loads(call["arguments"]), {"query": "bg"})
                exchanges.append(("tool_search", "bg available"))
                call = self.next_call(self.request(case, exchanges))
                self.assertEqual(call["name"], "tool_load")
                self.assertEqual(json.loads(call["arguments"]), {"names": ["bg"]})
                exchanges.append(("tool_load", "unavailable"))
                reply, finish = self.handler.reply(self.request(case, exchanges), 99)
                self.assertEqual(finish, "stop")
                self.assertNotIn("tool_calls", reply)
                self.assertIn("Blocked:", reply["content"])


if __name__ == "__main__":
    unittest.main()
