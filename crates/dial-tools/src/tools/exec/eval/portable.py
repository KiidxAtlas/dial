# Portable helpers use the same admitted tools as JavaScript. Python calls are synchronous.
import base64 as _dial_base64
import json as _dial_json
import re as _dial_re
import struct as _dial_struct


def _dial_format(format):
    if not isinstance(format, str) or len(format) > 256 or not _dial_re.fullmatch(r"[<>](?:[1-9][0-9]*[bBhHiIqQfdx]|[bBhHiIqQfdx])+", format):
        raise ValueError("format needs explicit < or > byte order and bBhHiIqQfdx fields")
    if sum(int(count or 1) for count, _ in _dial_re.findall(r"([1-9][0-9]*)?([bBhHiIqQfdx])", format[1:])) > 256:
        raise ValueError("at most 256 binary fields")
    return format


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
