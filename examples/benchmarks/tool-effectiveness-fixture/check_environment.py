"""Fixture runner: needs an explicitly selected environment, not an application fix."""
import os

if os.environ.get("LEDGER_TEST_ENV") != "fixture":
    raise SystemExit("test environment unavailable: set LEDGER_TEST_ENV=fixture")

from ledger import bounded_transaction_total

assert bounded_transaction_total([{"cents": 1000}]) == 1000
print("environment check passed")
