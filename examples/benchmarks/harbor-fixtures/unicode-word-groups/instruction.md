Write `/app/tokens.json`, a JSON array of arrays of strings. For each input below, in order, produce exactly the matches returned by Python 3's `re.findall(r"\w+", text)` with default flags. Decode the JSON escapes first; do not normalize the input text.

```json
["e\u0301x", "_\u0662!", "\u00e9,\u00b2", "\u0301", "A\u200dB", ""]
```
