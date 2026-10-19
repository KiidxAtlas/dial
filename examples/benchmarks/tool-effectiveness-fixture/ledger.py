"""Transaction ledger helpers. Amounts are integer cents."""
from settings import LIMIT_CENTS


def transaction_total(rows):
    return sum(row["cents"] for row in rows)


def bounded_transaction_total(rows):
    return min(transaction_total(rows), LIMIT_CENTS)
