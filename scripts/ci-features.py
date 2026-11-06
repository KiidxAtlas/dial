#!/usr/bin/env python3
"""Emit the representative or exhaustive Linux feature matrix for GitHub Actions."""
import argparse
import itertools
import json

FEATURES = ("aws", "voice", "embedded-shell", "desktop", "wayland-pipewire")


def matrix(exhaustive=False):
    if exhaustive:
        selections = [
            combination
            for size in range(len(FEATURES) + 1)
            for combination in itertools.combinations(FEATURES, size)
        ]
    else:
        # PipeWire already enables desktop, so a second desktop+PipeWire row is redundant.
        selections = [(), *((feature,) for feature in FEATURES), FEATURES[:-1], FEATURES]
    return {"include": [
        {
            "name": " + ".join(features) or "minimal",
            "features": " ".join(features),
            # libspa needs newer PipeWire headers than Ubuntu 22.04 provides.
            "os": "ubuntu-24.04" if "wayland-pipewire" in features else "ubuntu-22.04",
            # Only the largest selection on each runner image writes its shared cache.
            "save-cache": features in (FEATURES[:-1], FEATURES),
        }
        for features in selections
    ]}


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--exhaustive", action="store_true")
    args = parser.parse_args()
    print(json.dumps(matrix(args.exhaustive), separators=(",", ":")))
