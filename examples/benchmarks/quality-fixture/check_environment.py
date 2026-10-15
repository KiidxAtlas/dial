"""Fixture runner: requires an explicitly selected test environment."""
import os

if os.environ.get("BILLING_TEST_ENV") != "fixture":
    raise SystemExit("test environment unavailable: set BILLING_TEST_ENV=fixture")

from reports import page_count

assert page_count(list(range(26))) == 2
print("environment check passed")
