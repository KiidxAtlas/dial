#!/bin/sh
set -eu
# Fail closed, including parse errors and an unavailable verifier runtime.
printf '0\n' > /logs/verifier/reward.txt
python3 - <<'PY'
import json
import re
from pathlib import Path

inputs = ['e\u0301x', '_\u0662!', '\u00e9,\u00b2', '\u0301', 'A\u200dB', '']
actual = json.loads(Path('/app/tokens.json').read_text(encoding='utf-8'))
expected = [re.findall(r'\w+', text) for text in inputs]
if actual != expected:
    raise SystemExit(f'expected {expected!r}, got {actual!r}')
PY
printf '1\n' > /logs/verifier/reward.txt
