"""Managed launcher, copied beside sdk/ by Dial. Not a security sandbox."""
from contextlib import redirect_stdout
from pathlib import Path
import runpy
import sys


def main():
    if sys.version_info < (3, 11):
        raise SystemExit("Dial Python extensions require Python 3.11 or newer; update the interpreter and reinstall")
    if len(sys.argv) != 2:
        raise SystemExit("usage: launcher.py ABSOLUTE_EXTENSION_ENTRY")
    entry = Path(sys.argv[1])
    if not entry.is_absolute() or not entry.is_file():
        raise SystemExit("extension entry must be an absolute file path")
    # -I ignores PYTHONPATH; add only the managed SDK and trusted entry directory.
    sys.path.insert(0, str(Path(__file__).resolve().parent / "sdk"))
    sys.path.insert(1, str(entry.parent))
    import dial_extensions  # noqa: F401 -- reserve the managed SDK before user imports
    sys.argv = [str(entry)]
    with redirect_stdout(sys.stderr):
        runpy.run_path(str(entry), run_name="__main__")


if __name__ == "__main__":
    main()
