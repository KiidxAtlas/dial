"""Launch-mode recovery fixture. Run unchanged; success needs actual interactive input."""
import pathlib
import sys


def main():
    mode = sys.argv[1]
    if mode not in ("terminal", "pipe"):
        raise SystemExit("expected terminal or pipe")
    terminal = sys.stdin.isatty() and sys.stdout.isatty()
    if mode == "terminal" and not terminal:
        raise SystemExit("Error: this program requires an interactive terminal (TTY).")
    print("Send confirm on stdin to complete the probe.", flush=True)
    answer = sys.stdin.readline()
    if not answer:
        raise SystemExit("Error: input is required on stdin; received EOF.")
    if answer.strip() != "confirm":
        raise SystemExit("Error: expected confirm on stdin.")
    pathlib.Path(mode + "-receipt.txt").write_text(
        "confirmed;terminal=" + str(terminal).lower() + "\n"
    )
    print("probe-complete", flush=True)


if __name__ == "__main__":
    main()
