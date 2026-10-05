"""Exercise the Dial service contract without loading weights or starting a server."""
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

with tempfile.TemporaryDirectory() as home:
    with patch.dict(os.environ, {"DIAL_LAYA_HOME": home}):
        import dial_laya_server as server


class FakeAgent:
    def predict(self, state, questions):
        return {"answers": {key: {"type": "choice", "choice": "none"} for key in questions}}


class ServiceTests(unittest.TestCase):
    def test_identity_and_loading_are_dial_owned(self):
        with patch.object(server, "agent", None):
            health = server.health()
        self.assertEqual(health["service"], "dial-laya")
        self.assertEqual(health["protocol"], 1)
        self.assertEqual(health["status"], "starting")
        self.assertTrue(health["checkpoint"].endswith("/model"))

    def test_native_question_packet_and_prediction(self):
        packet = server.Prediction(state={"input": "hello"}, questions={"retrieval": {"type": "choice"}})
        with patch.object(server, "agent", FakeAgent()):
            self.assertEqual(server.predict(packet)["answers"]["retrieval"]["choice"], "none")

    def test_loading_invalid_questions_and_large_evidence_are_rejected(self):
        packet = server.Prediction(state={"input": "hello"}, questions={"retrieval": {"type": "choice"}})
        with patch.object(server, "agent", None):
            with self.assertRaises(server.HTTPException) as error:
                server.predict(packet)
            self.assertEqual(error.exception.status_code, 503)
        with patch.object(server, "agent", FakeAgent()):
            for state, questions in [({"input": "x" * 12001}, {"q": {"type": "noul"}}), ({"input": "hi"}, {"q": {"type": "execute"}})]:
                with self.assertRaises(server.HTTPException) as error:
                    server.predict(server.Prediction(state=state, questions=questions))
                self.assertEqual(error.exception.status_code, 422)


if __name__ == "__main__":
    unittest.main()
