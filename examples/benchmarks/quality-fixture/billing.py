"""Amounts are integer cents; quantities are non-negative integers."""


def invoice_total(items):
    return sum(item["price"] for item in items)


def discounted_total(subtotal, fraction):
    return round(subtotal * (1 - fraction))
