from ledger import transaction_total


def render_statement(rows):
    return "total=" + str(transaction_total(rows))
