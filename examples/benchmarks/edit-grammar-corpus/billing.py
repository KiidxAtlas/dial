"""Billing helpers."""

TAX_RATE = 0.2


def apply_tax(amount):
    if amount < 0:
        raise ValueError("negative amount")
    return round(amount * (1 + TAX_RATE), 2)


def invoice_total(items):
    total = 0
    for item in items:
        if item < 0:
            raise ValueError("negative amount")
        total += apply_tax(item)
    return total


def summarize(items):
    count = 0
    for item in items:
        if item < 0:
            raise ValueError("negative amount")
        count += 1
    return count, invoice_total(items)


def largest(items):
    best = None
    for item in items:
        if best is None or item > best:
            best = item
    return best
