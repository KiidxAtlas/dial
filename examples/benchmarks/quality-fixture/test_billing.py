import unittest

from billing import invoice_total


class BillingTests(unittest.TestCase):
    def test_quantities(self):
        self.assertEqual(invoice_total([{"price": 125, "quantity": 3}]), 375)

    def test_empty_invoice(self):
        self.assertEqual(invoice_total([]), 0)


if __name__ == "__main__":
    unittest.main()
