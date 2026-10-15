# Billing fixture

Production invoice arithmetic lives in billing.py. legacy_billing.py is archival,
not the implementation imported by the application. Amounts are integer cents;
quantities are non-negative integers. Run visible tests with:

    python3 -m unittest test_billing

The environment-specific pagination check requires its fixture environment:

    BILLING_TEST_ENV=fixture python3 check_environment.py

Current discount policy is declared in settings.py as a fraction, not a percent.
Historical notes about a percentage representation are stale. These independent
benchmark tasks intentionally start with defects; each gets a disposable copy.
