import unittest

from ledger import parse_amount, total_cents


class LedgerTests(unittest.TestCase):
    def test_whole_amount(self):
        self.assertEqual(parse_amount("1.00"), 100)

    def test_total_nets_refunds(self):
        entries = [{"kind": "charge", "cents": 500}, {"kind": "refund", "cents": 300}]
        self.assertEqual(total_cents(entries), 200)


if __name__ == "__main__":
    unittest.main()
