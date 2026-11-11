# Portable helpers use the same admitted tools as JavaScript. Python calls are synchronous.
import base64 as _dial_base64
import json as _dial_json
import re as _dial_re
import struct as _dial_struct
import random as _dial_random
import time as _dial_time


def _dial_format(format):
    if not isinstance(format, str) or len(format) > 256 or not _dial_re.fullmatch(r"[<>](?:[1-9][0-9]*[bBhHiIqQfdx]|[bBhHiIqQfdx])+", format):
        raise ValueError("format needs explicit < or > byte order and bBhHiIqQfdx fields")
    if sum(int(count or 1) for count, _ in _dial_re.findall(r"([1-9][0-9]*)?([bBhHiIqQfdx])", format[1:])) > 256:
        raise ValueError("at most 256 binary fields")
    return format


def _dial_unique(values, current):
    seen, out = {repr(current)}, []
    for value in values:
        key = repr(value)
        if key not in seen:
            seen.add(key)
            out.append(value)
    return out


class _DialGen:
    """One generator: draw(rng, size), shrink(value) -> smaller candidates, edges (tried first)."""

    def __init__(self, draw, shrink=None, edges=()):
        self.draw, self.shrink, self.edges = draw, shrink or (lambda value: []), list(edges)

    def map(self, f):
        return _DialGen(lambda r, size: f(self.draw(r, size)), None, [f(e) for e in self.edges])

    def filter(self, keep):
        def draw(r, size):
            for _ in range(100):
                value = self.draw(r, size)
                if keep(value):
                    return value
            raise ValueError("gen.filter rejected 100 draws")
        return _DialGen(draw, lambda v: [s for s in self.shrink(v) if keep(s)], [e for e in self.edges if keep(e)])


class _DialGens:
    def int(self, lo=-1000, hi=1000):
        if not isinstance(lo, int) or not isinstance(hi, int) or lo > hi:
            raise TypeError("gen.int expects integers lo <= hi")
        target = min(max(0, lo), hi)

        def draw(r, size):
            span = min(hi - lo, max(1, size * size))
            start = min(max(target - span // 2, lo), hi - span)
            return start + r.randint(0, span) if r.random() < 0.8 else r.randint(lo, hi)

        def shrink(v):
            step = 0 if v == target else (1 if v > target else -1)
            return _dial_unique([x for x in (target, v - int((v - target) / 2), v - step) if lo <= x <= hi], v)

        return _DialGen(draw, shrink, _dial_unique([x for x in (0, 1, -1, lo, hi, lo + 1, hi - 1) if lo <= x <= hi], None))

    def float(self, lo=-1e6, hi=1e6):
        inside = lambda x: lo <= x <= hi
        return _DialGen(
            lambda r, size: (r.random() * 2 - 1) * size if r.random() < 0.5 else r.uniform(lo, hi),
            lambda v: _dial_unique([x for x in (0.0, float(int(v)), v / 2) if inside(x)], v),
            _dial_unique([x for x in (0.0, 0.5, -0.5, lo, hi) if inside(x)], None))

    def array(self, item, min=0, max=20):
        def draw(r, size):
            return [item.draw(r, size) for _ in range(r.randint(min, _dial_min(max, min + size)))]

        def shrink(xs):
            out = []
            if len(xs) > min:
                half = _dial_max(min, len(xs) // 2)
                out += [xs[:half], xs[len(xs) - half:]]
                out += [xs[:i] + xs[i + 1:] for i in range(_dial_min(len(xs), 32))]
            for i in range(_dial_min(len(xs), 32)):
                out += [xs[:i] + [s] + xs[i + 1:] for s in item.shrink(xs[i])[:4]]
            return _dial_unique(out, xs)

        edges = ([[]] if min == 0 else []) + [[e] * _dial_max(1, min) for e in item.edges[:6]]
        return _DialGen(draw, shrink, [e for e in edges if len(e) <= max])

    def string(self, alphabet="".join(chr(c) for c in range(32, 127)), min=0, max=20):
        chars = list(alphabet)
        if not chars:
            raise ValueError("gen.string needs a nonempty alphabet")
        ch = _DialGen(lambda r, size: r.choice(chars), lambda c: [] if c == chars[0] else [chars[0]], chars[:3])
        items = self.array(ch, min=min, max=max)
        edges = ([""] if min == 0 else []) + [c * _dial_max(1, min) for c in chars[:3]]
        return _DialGen(lambda r, size: "".join(items.draw(r, size)),
                        lambda s: ["".join(xs) for xs in items.shrink(list(s))],
                        _dial_unique([e for e in edges if len(e) <= max], None))

    def tuple(self, *items):
        def shrink(xs):
            return _dial_unique([xs[:i] + (s,) + xs[i + 1:] for i, item in enumerate(items) for s in item.shrink(xs[i])[:8]], xs)
        edges = []
        if all(item.edges for item in items):
            rest = tuple(item.edges[0] for item in items[1:])
            edges = [(e,) + rest for e in items[0].edges]
        return _DialGen(lambda r, size: tuple(item.draw(r, size) for item in items), shrink, edges)

    def record(self, **shape):
        keys = list(shape)
        inner = self.tuple(*(shape[k] for k in keys))
        build = lambda values: dict(zip(keys, values))
        return _DialGen(lambda r, size: build(inner.draw(r, size)),
                        lambda d: [build(v) for v in inner.shrink(tuple(d[k] for k in keys))],
                        [build(e) for e in inner.edges])

    def pick(self, values):
        values = list(values)
        if not values:
            raise ValueError("gen.pick expects a nonempty sequence")
        return _DialGen(lambda r, size: r.choice(values), lambda v: values[:values.index(v)][:4] if v in values else [], values[:1])

    def oneOf(self, *items):
        return _DialGen(lambda r, size: r.choice(items).draw(r, size), None, [e for item in items for e in item.edges[:3]])

    one_of = oneOf

    def bool(self):
        return self.pick([False, True])

    def constant(self, value):
        return _DialGen(lambda r, size: value, None, [value])


_dial_min, _dial_max = min, max


def _dial_show(value):
    text = repr(value)
    return text if len(text) <= 400 else text[:400] + "…"


def _dial_verdict(prop, value):
    try:
        return "property returned False" if prop(value) is False else None
    except Exception as error:
        return type(error).__name__ + ": " + str(error)

class _Dial:
    def pack(self, format, values):
        return _dial_struct.pack(_dial_format(format), *values)

    def unpack(self, format, value):
        return list(_dial_struct.unpack(_dial_format(format), bytes(value)))

    def hex(self, value, offset=0, length=256):
        data = bytes(value)
        if not isinstance(offset, int) or not 0 <= offset <= len(data) or not isinstance(length, int) or not 0 <= length <= 1024:
            raise ValueError("hex offset must be in range; length is 0–1024")
        rows = []
        for pos in range(offset, min(len(data), offset + length), 16):
            chunk = data[pos:min(pos+16, offset+length)]
            rows.append(f"{pos:08x}  " + " ".join(f"{n:02x}" for n in chunk).ljust(47) + "  |" + "".join(chr(n) if 32 <= n < 127 else "." for n in chunk) + "|")
        return "\n".join(rows)

    def read(self, path, *, encoding="text", retain=False):
        return tool.read(path=path, format=encoding, retain=retain)["data"]

    def readText(self, path):
        return self.read(path)["content"]

    def readLines(self, path, offset=1, limit=None):
        """One page of a text file of any size: {content, lines, offset, next_offset}; the page is
        at most 1 MiB, and next_offset is None at the end of the file."""
        args = {"path": path, "format": "text", "offset": offset}
        if limit is not None:
            args["limit"] = limit
        return tool.read(**args)["data"]

    def lines(self, path, offset=1):
        """Every line of a text file of any size from `offset`, without line endings, paged 1 MiB at a time."""
        while offset is not None:
            page = self.readLines(path, offset)
            content = page["content"]
            if content:
                for line in (content[:-1] if content.endswith("\n") else content).split("\n"):
                    yield line[:-1] if line.endswith("\r") else line
            offset = page["next_offset"]

    def readJson(self, path):
        return _dial_json.loads(self.readText(path))

    def readBytes(self, path):
        return _dial_base64.b64decode(self.read(path, encoding="base64")["content"], validate=True)

    def writeText(self, path, content):
        return tool.write(path=path, content=content)

    def writeJson(self, path, value):
        return self.writeText(path, _dial_json.dumps(value, ensure_ascii=False, allow_nan=False, indent=2) + "\n")

    def writeBytes(self, path, value):
        return tool.write(path=path, content=_dial_base64.b64encode(bytes(value)).decode("ascii"), encoding="base64")

    def run(self, command, **options):
        return tool.bash(command=command, **options)

    def check(self, name, passed):
        return __dial_check__(name, passed)

    gen = _DialGens()

    def search(self, prop, gen, *, runs=200, seed=0x5EED, cases=(), timeout=10.0, shrinks=400):
        """Search for an input where prop returns False or raises: edge cases, then seeded random
        inputs growing with the run index; a failure is shrunk toward a minimal input."""
        if not callable(prop) or not isinstance(gen, _DialGen):
            raise TypeError("dial.search(prop, gen, runs=, seed=, cases=, timeout=) needs a callable and a dial.gen generator")
        r, deadline = _dial_random.Random(seed), _dial_time.monotonic() + timeout

        def inputs():
            yield from cases
            yield from gen.edges
            for i in range(runs):
                yield gen.draw(r, 1 + i * 100 // _dial_max(1, runs))

        tried = 0
        for candidate in inputs():
            if _dial_time.monotonic() > deadline:
                return {"passed": True, "runs": tried, "seed": seed, "stopped": "timeout", "note": "No counterexample within the time budget; this is evidence, not proof."}
            tried += 1
            failure = _dial_verdict(prop, candidate)
            if failure is None:
                continue
            smallest, steps, improved = candidate, 0, True
            while improved and steps < shrinks and _dial_time.monotonic() <= deadline:
                improved = False
                for smaller in gen.shrink(smallest):
                    steps += 1
                    if steps > shrinks:
                        break
                    why = _dial_verdict(prop, smaller)
                    if why is not None:
                        smallest, failure, improved = smaller, why, True
                        break
            return {"passed": False, "runs": tried, "seed": seed, "counterexample": smallest, "failure": failure,
                    "original": candidate, "shrinkSteps": steps, "shown": _dial_show(smallest)}
        return {"passed": True, "runs": tried, "seed": seed, "note": "No counterexample found; this is evidence, not proof."}

    def compare(self, left, right, gen, *, equal=None, **options):
        """Search for an input where two implementations disagree (both raising counts as agreement)."""
        def outcome(f, x):
            try:
                return ("value", f(x))
            except Exception as error:
                return ("raised", type(error).__name__)

        def same(x):
            a, b = outcome(left, x), outcome(right, x)
            if a[0] == "raised" or b[0] == "raised":
                agree = a[0] == b[0]
            else:
                agree = equal(a[1], b[1]) if equal else a[1] == b[1]
            if not agree:
                show = lambda o: "raised " + o[1] if o[0] == "raised" else _dial_show(o[1])
                raise AssertionError("left " + show(a) + " vs right " + show(b))

        return self.search(same, gen, **options)

    def batch(self, entries, *, collect=False):
        if not isinstance(collect, bool):
            raise TypeError("collect must be a boolean")
        results = tool.batch(entries)
        if not collect and any(result["is_error"] or result.get("effects") == "unknown" for result in results):
            error = RuntimeError("Batch prerequisites failed; completed effects remain. Inspect error.results before retrying.")
            error.results = results
            error.result = {"is_error":True,"text":str(error),"data":{"results":results}}
            raise error
        return results

    def fresh(self, receipt):
        return self.read(receipt["path"], encoding="base64")["sha256"] == receipt["sha256"]

    def restore(self, receipt, path=None):
        original = self.read(receipt["artifact"], encoding="base64")
        if original["sha256"] != receipt["sha256"]:
            raise ValueError("Retained bytes do not match the source receipt")
        return tool.write(path=receipt["path"] if path is None else path, content=original["content"], encoding="base64")

    def snapshot(self, paths):
        if not isinstance(paths, (list, tuple)) or not 1 <= len(paths) <= 16:
            raise ValueError("snapshot expects 1–16 local paths")
        reads = self.batch([{"name":"read", "args":{"path":path,"format":"base64","retain":True}} for path in paths])
        manifest = [{key:value for key,value in result["data"].items() if key != "content"} for result in reads]
        for receipt in manifest:
            if not self.fresh(receipt):
                raise RuntimeError("Snapshot source changed: " + receipt["path"] + "; retained artifacts remain available.")
        return {"files":manifest,"consistency":"Individual byte captures rechecked; not an atomic live-database snapshot."}


dial = _Dial()
