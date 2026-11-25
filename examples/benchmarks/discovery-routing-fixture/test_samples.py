"""Each sample smoke test leaves its own receipt in the disposable fixture workspace."""
import json
import unittest
from pathlib import Path


class SampleTests(unittest.TestCase):
    def check_sample(self, platform, layout):
        sample, = Path("samples", platform).glob("*.json")
        data = json.loads(sample.read_text())
        self.assertEqual(data["layout"], layout)
        self.assertTrue(data["label"])
        Path(f"{platform}.checked").write_text(data["label"] + "\n")

    def test_desktop(self):
        self.check_sample("desktop", "wide")

    def test_mobile(self):
        self.check_sample("mobile", "narrow")
