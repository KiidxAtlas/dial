"""Generate a catalog of the checked-in preview samples."""
import json
from pathlib import Path


def sample_files():
    return sorted(Path("samples").glob("*/*.json"))


if __name__ == "__main__":
    rows = [f"{p.parent.name}: {json.loads(p.read_text())['label']}\n" for p in sample_files()]
    Path("catalog.txt").write_text("".join(rows))
