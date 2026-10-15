"""Archived implementation; production code does not import this module."""


def invoice_total(items):
    return sum(item["price"] * item["quantity"] for item in items)
