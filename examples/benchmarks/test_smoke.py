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

    def test_final_reply_still_refuses_a_missing_goal(self):
        handler = Handler.__new__(Handler)
        with self.assertRaises(ValueError):
            handler.reply({"messages": [{"role": "user", "content":
                "BENCH_CASE=goal-compaction phase=finish"}]}, 1)


if __name__ == "__main__":
    unittest.main()
