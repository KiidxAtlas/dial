# Ledger fixture

ledger.py holds decimal-string amount parsing, netting, export and row loading.
Amounts are integer cents. Visible tests run with:

    python3 -m unittest test_ledger

The visible tests are a subset of the acceptance checks in reliability.toml; passing
them is not sufficient. Each benchmark case gets a disposable copy of this fixture.
