"""Ledger fixture: amounts are decimal strings; money is stored as integer cents."""


def parse_amount(text):
    return int(float(text) * 100)


def total_cents(entries):
    return sum(e["cents"] if e["kind"] == "charge" else -e["cents"] for e in entries)


def export_lines(entries):
    return [f"{e['kind']}:{e['cents']}" for e in entries if e["kind"] == "charge"]


def load_rows(lines):
    entries = []
    for line in lines:
        kind, _, amount = line.partition(",")
        try:
            entries.append({"kind": kind, "cents": parse_amount(amount)})
        except ValueError:
            continue
    return entries
