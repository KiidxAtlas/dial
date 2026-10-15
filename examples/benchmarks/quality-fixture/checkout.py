from billing import discounted_total
from settings import DISCOUNT_FRACTION


def amount_due(subtotal):
    return discounted_total(subtotal, DISCOUNT_FRACTION / 100)
