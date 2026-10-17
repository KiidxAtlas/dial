"""Schema-constrained tool-call decoding for Dial's MLX-LM server.

Inside a tool-call span, every sampled token must keep the call text a viable prefix of a call
that the loaded model's tool parser reads back. Supported parser formats:

- `json_tools`: `{"name": <declared tool>, "arguments": {...}}`
- `gemma4`: `call:NAME{key:value,...}`; strings delimited by `<|"|>`
- `function_gemma`: `call:NAME{key:value,...}`; strings delimited by `<escape>`
- `glm47`: `NAME<arg_key>k</arg_key><arg_value>v</arg_value>...`
- `mistral`: a `[TOOL_CALLS]` JSON array of calls, or `NAME[ARGS]{...}`. Open-ended: the parser
  reads no closing marker, so the constraint ends once the call's JSON is complete.

Any other parser stays unconstrained. Outside tool calls and inside thinking spans, sampling is
untouched.

Enforced: JSON syntax (including string escapes and control characters), tool names, declared
argument keys (unless `additionalProperties` permits extras), required keys before `}`, value
types, string enums/consts, array item schemas and nested objects. Combinators (`anyOf`,
`oneOf`, `allOf`, `$ref`, `not`, `if`) and numeric/string bounds remain syntax-only; Rust-side
validation stays authoritative. Plain (unquoted) gemma and function_gemma values must be JSON
literals. A dead end releases the constraint for the rest of that call, so the existing
malformed-call handling sees the output instead of the decoder stalling.
"""
import codecs
import copy
import json
import logging

_WS = " \t\n\r"
_MAX_WS = 64  # Consecutive structural whitespace; stops newline/indent loops.
_ATTEMPTS = 4  # Rejection-sampling tries before scanning ranked candidates.
_RANKED = 256
_VIABLE = 16
_COMBINATORS = ("anyOf", "oneOf", "allOf", "$ref", "not", "if")
_TYPES = frozenset(("object", "array", "string", "integer", "number", "boolean", "null"))

ARGUMENTS = object()  # Wrapper "arguments": resolved from the "name" already emitted.


class _Spec:
    __slots__ = ("closed", "enum", "items", "props", "required", "types")

    def __init__(self, types=None, enum=None, props=None, required=frozenset(), closed=False,
                 items=None):
        self.types = types
        self.enum = enum
        self.props = props or {}
        self.required = required
        self.closed = closed
        self.items = items


_ANY = _Spec()


def _json_type(value):
    if value is None:
        return {"null"}
    if isinstance(value, bool):
        return {"boolean"}
    if isinstance(value, int):
        return {"integer", "number"}
    if isinstance(value, float):
        return {"number"}
    if isinstance(value, str):
        return {"string"}
    return {"array"} if isinstance(value, list) else {"object"}


def spec(schema):
    if not isinstance(schema, dict) or any(key in schema for key in _COMBINATORS):
        return _ANY
    declared = schema.get("type")
    if isinstance(declared, str):
        types = {declared}
    elif isinstance(declared, list) and declared:
        types = set(declared)
    elif isinstance(schema.get("properties"), dict):
        types = {"object"}
    elif isinstance(schema.get("items"), dict):
        types = {"array"}
    else:
        types = None
    if types is not None and not types <= _TYPES:
        types = None
    values = schema["enum"] if isinstance(schema.get("enum"), list) else (
        [schema["const"]] if "const" in schema else None)
    enum = None
    if values:
        if all(isinstance(v, str) for v in values):
            enum = frozenset(values)
            types = {"string"}
        else:
            inferred = set().union(*(_json_type(v) for v in values))
            types = inferred if types is None else (types & inferred) or inferred
    props = schema.get("properties") if isinstance(schema.get("properties"), dict) else {}
    required = frozenset(r for r in schema.get("required", ()) if isinstance(r, str)) \
        if isinstance(schema.get("required"), list) else frozenset()
    extra = schema.get("additionalProperties")
    closed = (bool(props) or extra is False) and extra is not True and not isinstance(extra, dict)
    if closed:
        # A required key the schema forgot to declare must still be writable.
        props = {**{key: {} for key in required if key not in props}, **props}
    items = schema.get("items") if isinstance(schema.get("items"), dict) else None
    return _Spec(frozenset(types) if types else None, enum, props, required, closed, items)


def _allows(s, kind):
    return s.types is None or kind in s.types


class _Value:
    __slots__ = ("spec",)

    def __init__(self, s):
        self.spec = s

    def clone(self):
        return self  # Immutable: replaced, never mutated.


class _Object:
    __slots__ = ("key", "phase", "seen", "spec", "values")

    def __init__(self, s):
        self.spec, self.phase, self.seen, self.key, self.values = s, "first", set(), None, {}

    def clone(self):
        other = _Object.__new__(_Object)
        other.spec, other.phase, other.key = self.spec, self.phase, self.key
        other.seen, other.values = set(self.seen), dict(self.values)
        return other

    def remaining(self):
        return [key for key in self.spec.props if key not in self.seen]


class _Array:
    __slots__ = ("phase", "spec")

    def __init__(self, s):
        self.spec, self.phase = s, "first"

    def clone(self):
        other = _Array(self.spec)
        other.phase = self.phase
        return other


class _String:
    __slots__ = ("allowed", "buf", "esc", "key")

    def __init__(self, key, allowed, track):
        self.key, self.allowed, self.esc = key, allowed, 0
        self.buf = "" if track else None

    def clone(self):
        other = _String.__new__(_String)
        other.key, other.allowed, other.buf, other.esc = self.key, self.allowed, self.buf, self.esc
        return other


_NUMBER = {
    "sign": {"0": "zero", "d": "int"},
    "zero": {".": "frac0", "e": "exp0"},
    "int": {"d": "int", ".": "frac0", "e": "exp0"},
    "frac0": {"d": "frac"},
    "frac": {"d": "frac", "e": "exp0"},
    "exp0": {"+": "expsign", "d": "exp"},
    "expsign": {"d": "exp"},
    "exp": {"d": "exp"},
}
_NUMBER_END = frozenset(("zero", "int", "frac", "exp"))


class _Number:
    __slots__ = ("integer", "state")

    def __init__(self, integer, state):
        self.integer, self.state = integer, state

    def clone(self):
        return _Number(self.integer, self.state)


class _Literal:
    __slots__ = ("rest",)

    def __init__(self, rest):
        self.rest = rest

    def clone(self):
        return _Literal(self.rest)


class JsonMatcher:
    """Incremental, cloneable recognizer for one JSON value under a (subset) JSON Schema."""
    __slots__ = ("stack", "tools", "ws")

    def __init__(self, root, tools=None):
        self.stack = [_Value(root)]
        self.ws = 0
        self.tools = tools or {}

    def clone(self):
        other = JsonMatcher.__new__(JsonMatcher)
        other.stack = [frame.clone() for frame in self.stack]
        other.ws, other.tools = self.ws, self.tools
        return other

    @property
    def complete(self):
        return not self.stack

    can_end = complete

    def feed(self, text):
        return all(self._char(ch) for ch in text)

    def _space(self):
        self.ws += 1
        return self.ws <= _MAX_WS

    def _finish(self, value=None):
        self.stack.pop()
        if self.stack:
            parent = self.stack[-1]
            if isinstance(parent, _Object) and value is not None:
                parent.values[parent.key] = value
            parent.phase = "next"

    def _child(self, obj):
        raw = obj.spec.props.get(obj.key)
        if raw is ARGUMENTS:
            s = spec(self.tools.get(obj.values.get("name"), {"type": "object"}))
            if not s.types or "object" not in s.types:
                s = _Spec(frozenset(("object",)), None, s.props, s.required, s.closed, None)
            return s
        return _ANY if raw is None else spec(raw)

    def _char(self, ch):
        while True:
            if not self.stack:
                return ch in _WS and self._space()
            frame = self.stack[-1]
            if isinstance(frame, _String):
                self.ws = 0
                return self._string(frame, ch)
            if isinstance(frame, _Number):
                kind = "0" if ch == "0" else "d" if ch.isdigit() and ch.isascii() else \
                    "e" if ch in "eE" else "+" if ch in "+-" else ch
                if kind in (".", "e") and frame.integer:
                    nxt = None
                else:
                    table = _NUMBER[frame.state]
                    nxt = table.get(kind) or (table.get("d") if kind == "0" else None)
                if nxt is not None:
                    self.ws = 0
                    frame.state = nxt
                    return True
                if frame.state not in _NUMBER_END:
                    return False
                self._finish()
                continue  # The terminator belongs to the parent.
            if ch in _WS:
                return self._space()
            self.ws = 0
            if isinstance(frame, _Literal):
                if not frame.rest.startswith(ch):
                    return False
                frame.rest = frame.rest[1:]
                if not frame.rest:
                    self._finish()
                return True
            if isinstance(frame, _Value):
                return self._value(frame.spec, ch)
            if isinstance(frame, _Object):
                return self._object(frame, ch)
            return self._array(frame, ch)

    def _value(self, s, ch):
        top = len(self.stack) - 1
        if ch == "{" and _allows(s, "object"):
            self.stack[top] = _Object(s)
        elif ch == "[" and _allows(s, "array"):
            self.stack[top] = _Array(s)
        elif ch == '"' and _allows(s, "string"):
            self.stack[top] = _String(False, s.enum, s.enum is not None)
        elif (ch == "-" or ch.isdigit() and ch.isascii()) and (
                _allows(s, "number") or _allows(s, "integer")):
            integer = s.types is not None and "number" not in s.types
            self.stack[top] = _Number(integer, "sign" if ch == "-" else "zero" if ch == "0" else "int")
        elif ch in "tf" and _allows(s, "boolean"):
            self.stack[top] = _Literal("rue" if ch == "t" else "alse")
        elif ch == "n" and _allows(s, "null"):
            self.stack[top] = _Literal("ull")
        else:
            return False
        return True

    def _object(self, obj, ch):
        phase = obj.phase
        if ch == "}" and phase in ("first", "next"):
            if not obj.spec.required <= obj.seen:
                return False
            self._finish()
            return True
        if ch == '"' and phase in ("first", "key"):
            allowed = frozenset(obj.remaining()) if obj.spec.closed else None
            if allowed is not None and not allowed:
                return False
            self.stack.append(_String(True, allowed, True))
            return True
        if ch == ":" and phase == "colon":
            obj.phase = "value"
            self.stack.append(_Value(self._child(obj)))
            return True
        if ch == "," and phase == "next":
            if obj.spec.closed and not obj.remaining():
                return False
            obj.phase = "key"
            return True
        return False

    def _array(self, arr, ch):
        if ch == "]" and arr.phase in ("first", "next"):
            self._finish()
            return True
        if ch == "," and arr.phase == "next":
            arr.phase = "item"
            self.stack.append(_Value(spec(arr.spec.items) if arr.spec.items else _ANY))
            return True
        if arr.phase == "first":
            arr.phase = "item"
            self.stack.append(_Value(spec(arr.spec.items) if arr.spec.items else _ANY))
            return self._char(ch)
        return False

    def _string(self, frame, ch):
        if frame.esc == 1:
            if ch == "u":
                frame.esc = 2
                return True
            decoded = {'"': '"', "\\": "\\", "/": "/", "b": "\b", "f": "\f", "n": "\n",
                       "r": "\r", "t": "\t"}.get(ch)
            if decoded is None:
                return False
            frame.esc = 0
            if frame.buf is not None:
                frame.buf += decoded
            return True
        if frame.esc:
            if ch not in "0123456789abcdefABCDEF":
                return False
            frame.esc = 0 if frame.esc == 5 else frame.esc + 1
            return True
        if ch == '"':
            if frame.allowed is not None and frame.buf not in frame.allowed:
                return False
            if frame.key:
                self.stack.pop()
                obj = self.stack[-1]
                if frame.buf in obj.seen:
                    return False
                obj.seen.add(frame.buf)
                obj.key, obj.phase = frame.buf, "colon"
            else:
                self._finish(frame.buf)
            return True
        if ch == "\\":
            if frame.allowed is not None:
                return False  # Names and enum members are plain text.
            frame.esc = 1
            return True
        if ord(ch) < 0x20:
            return False  # Raw control characters are invalid JSON.
        if frame.buf is not None:
            buf = frame.buf + ch
            if frame.allowed is not None and not any(a.startswith(buf) for a in frame.allowed):
                return False
            frame.buf = buf
        return True


def tool_schemas(tools):
    """OpenAI tool list → {name: parameters schema}."""
    result = {}
    for tool in tools or ():
        function = tool.get("function") if isinstance(tool, dict) else None
        name = function.get("name") if isinstance(function, dict) else None
        if isinstance(name, str) and name:
            result[name] = function.get("parameters") or {"type": "object"}
    return result


def call_matcher(schemas):
    root = _Spec(frozenset(("object",)), None,
                 {"name": {"enum": sorted(schemas)}, "arguments": ARGUMENTS},
                 frozenset(("name", "arguments")), True)
    return JsonMatcher(root, schemas)


def _scan(tags, pending, text):
    """Split `pending + text` into ("text", char) and ("tag", tag) events.

    A tail that could still grow into one of `tags` stays pending until the next chunk.
    """
    data = pending + text
    events = []
    i = 0
    while i < len(data):
        for tag in tags:
            if data.startswith(tag, i):
                events.append(("tag", tag))
                i += len(tag)
                break
        else:
            rest = data[i:]
            if any(tag.startswith(rest) for tag in tags):
                return rest, events
            events.append(("text", data[i]))
            i += 1
    return "", events


def _escape(text):
    """`text` as the body of a JSON string literal."""
    return json.dumps(text, ensure_ascii=False)[1:-1]


def _identifier(ch):
    return ch.isalnum() or ch == "_"


def _calls_schema(schemas):
    """Schema of one `{"name", "arguments"}` call, for array-style calls."""
    return {"type": "object",
            "properties": {"name": {"enum": sorted(schemas)}, "arguments": ARGUMENTS},
            "required": ["name", "arguments"]}


class _Stateful:
    """Cloning for matchers that keep plain attributes plus an optional JsonMatcher."""

    __slots__ = ()

    def clone(self):
        other = copy.copy(self)
        if self.j is not None:
            other.j = self.j.clone()
        return other


class _Tagged(_Stateful):
    """`call:NAME{key:value,...}` with tag-delimited strings (gemma4, function_gemma).

    Bare keys and string tags are rewritten to JSON as they arrive, so arguments face the same
    schema checks as `json_tools`. Plain values must be JSON literals.
    """

    __slots__ = ("buf", "forbid", "j", "mode", "pend", "quote", "quote_keys", "schemas", "stage")
    HEAD = "call:"

    def __init__(self, schemas, quote, quote_keys, forbid):
        self.schemas, self.quote, self.quote_keys, self.forbid = schemas, quote, quote_keys, forbid
        self.stage, self.buf, self.j, self.mode, self.pend = "head", "", None, None, ""

    def feed(self, text):
        self.pend, events = _scan((self.quote,), self.pend, text)
        return all(self._event(kind, value) for kind, value in events)

    @property
    def complete(self):
        return self.stage == "args" and self.mode is None and not self.pend and self.j.complete

    can_end = complete

    def _event(self, kind, value):
        if self.stage == "head":
            if kind != "text" or not self.HEAD.startswith(self.buf + value):
                return False
            self.buf += value
            if self.buf == self.HEAD:
                self.stage, self.buf = "name", ""
            return True
        if self.stage == "name":
            if kind != "text":
                return False
            if value == "{":
                schema = self.schemas.get(self.buf)
                if schema is None:
                    return False
                self.j = JsonMatcher(spec(schema), self.schemas)
                self.stage = "args"
                return self.j.feed("{")
            buf = self.buf + value
            if not any(name.startswith(buf) for name in self.schemas):
                return False
            self.buf = buf
            return True
        return self._argument(kind, value)

    def _argument(self, kind, value):
        j = self.j
        if self.mode == "quote":
            if kind == "tag":
                self.mode = None
                return j.feed('"')
            return not (self.forbid and value in self.forbid) and j.feed(_escape(value))
        if self.mode == "bare":
            if kind == "tag":
                return False
            if value == ":":
                self.mode = None
                return j.feed('":')
            return _identifier(value) and j.feed(value)
        top = j.stack[-1] if j.stack else None
        if kind == "tag":
            if isinstance(top, _Object) and not self.quote_keys:
                return False
            if not j.feed('"'):
                return False
            self.mode = "quote"
            return True
        if isinstance(top, _Object) and top.phase in ("first", "key") and _identifier(value):
            self.mode = "bare"
            return j.feed('"' + value)
        return j.feed(value)


_GLM_TAGS = ("<arg_key>", "</arg_key>", "<arg_value>", "</arg_value>")


class _Glm(_Stateful):
    """`NAME<arg_key>k</arg_key><arg_value>v</arg_value>...` (glm47).

    The parser keeps string-typed values raw and reads other values as JSON when they parse. Here a
    `string` value is raw text (enum prefix-checked), an untyped value is free text, and any other
    type must be a JSON prefix of that type. The call ends only at the closing marker.
    """

    __slots__ = ("buf", "enum", "j", "key", "mode", "pend", "schemas", "seen", "spec", "stage")

    def __init__(self, schemas):
        self.schemas, self.stage, self.buf = schemas, "name", ""
        self.spec, self.key, self.seen = None, "", set()
        self.mode, self.enum, self.j, self.pend = None, None, None, ""

    def clone(self):
        other = super().clone()
        other.seen = set(self.seen)
        return other

    def feed(self, text):
        self.pend, events = _scan(_GLM_TAGS, self.pend, text)
        return all(self._event(kind, value) for kind, value in events)

    @property
    def complete(self):
        return False

    @property
    def can_end(self):
        if self.pend:
            return False
        if self.stage == "name":
            schema = self.schemas.get(self.buf)
            return schema is not None and not spec(schema).required
        return self.stage == "after" and self.spec.required <= self.seen

    def _event(self, kind, value):
        stage = self.stage
        if stage == "name":
            if kind == "tag":
                if value != "<arg_key>" or self.buf not in self.schemas:
                    return False
                self.spec = spec(self.schemas[self.buf])
                self.stage, self.buf = "key", ""
                return True
            buf = self.buf + value
            if not any(name.startswith(buf) for name in self.schemas):
                return False
            self.buf = buf
            return True
        if stage == "key":
            if kind == "text":
                buf = self.buf + value
                if self.spec.closed and not any(key.startswith(buf) for key in self.spec.props):
                    return False
                self.buf = buf
                return True
            key = self.buf
            if value != "</arg_key>" or not key or key in self.seen:
                return False
            if self.spec.closed and key not in self.spec.props:
                return False
            self.seen.add(key)
            self.key, self.buf, self.stage = key, "", "vgap"
            return True
        if stage == "vgap":
            if kind == "text":
                return value in _WS
            if value != "<arg_value>":
                return False
            self._open_value()
            return True
        if stage == "value":
            if kind == "tag":
                return value == "</arg_value>" and self._close_value()
            if self.mode == "json":
                return self.j.feed(value)
            if self.enum is not None and not any(
                    member.startswith((self.buf + value).lstrip()) for member in self.enum):
                return False
            self.buf += value
            return True
        if kind == "text":
            return value in _WS
        if value != "<arg_key>":
            return False
        self.stage, self.buf = "key", ""
        return True

    def _open_value(self):
        raw = self.spec.props.get(self.key)
        kind = spec(raw) if isinstance(raw, dict) else _ANY
        self.stage, self.buf = "value", ""
        if kind.types is None:
            self.mode, self.enum = "any", None
        elif kind.types == frozenset(("string",)):
            self.mode, self.enum = "str", kind.enum
        else:
            self.mode, self.enum, self.j = "json", None, JsonMatcher(kind, self.schemas)

    def _close_value(self):
        if self.mode == "json":
            done = self.j.clone()
            if not (done.feed(" ") and done.complete):
                return False
        elif self.enum is not None and self.buf.strip() not in self.enum:
            return False
        self.stage, self.buf, self.j = "after", "", None
        return True


class _Mistral(_Stateful):
    """`[TOOL_CALLS]` then a JSON array of calls, or `NAME[ARGS]{...}` (mistral).

    Open-ended: the parser reads no closing marker, so the call ends once its JSON is complete.
    """

    __slots__ = ("buf", "j", "pend", "schemas", "stage")
    ARGS = "[ARGS]"

    def __init__(self, schemas):
        self.schemas, self.stage, self.buf, self.j, self.pend = schemas, "start", "", None, ""

    def feed(self, text):
        self.pend, events = _scan((self.ARGS,), self.pend, text)
        return all(self._event(kind, value) for kind, value in events)

    @property
    def complete(self):
        return self.stage in ("array", "args") and not self.pend and self.j.complete

    can_end = complete

    def _event(self, kind, value):
        if self.stage == "start":
            if kind == "text" and value == "[":
                self.stage = "array"
                self.j = JsonMatcher(spec({"type": "array", "items": _calls_schema(self.schemas)}),
                                     self.schemas)
                return self.j.feed("[")
            self.stage = "name"
        if self.stage == "name":
            if kind == "tag":
                schema = self.schemas.get(self.buf)
                if value != self.ARGS or schema is None:
                    return False
                self.stage, self.j = "args", JsonMatcher(spec(schema), self.schemas)
                return True
            buf = self.buf + value
            if not any(name.startswith(buf) for name in self.schemas):
                return False
            self.buf = buf
            return True
        return kind == "text" and self.j.feed(value)


class _Format:
    """How one mlx-lm tool-parser format is constrained."""

    __slots__ = ("literals", "make", "open_ended")

    def __init__(self, make, literals=(), open_ended=False):
        self.make, self.literals, self.open_ended = make, tuple(literals), open_ended


_JSON = _Format(call_matcher)
_FORMATS = {
    "json_tools": _JSON,
    "gemma4": _Format(lambda schemas: _Tagged(schemas, '<|"|>', True, ""), ('<|"|>',)),
    "function_gemma": _Format(lambda schemas: _Tagged(schemas, "<escape>", False, "}"),
                              ("<escape>",)),
    "glm47": _Format(_Glm, _GLM_TAGS),
    "mistral": _Format(_Mistral, ("[ARGS]",), open_ended=True),
}


def format_for(tokenizer):
    """The constrained format for this tokenizer's tool parser, or None if it is unsupported."""
    parser = getattr(tokenizer, "tool_parser", None)
    module = getattr(parser, "__module__", "") or ""
    fmt = _FORMATS.get(module.rsplit(".", 1)[-1])
    if fmt is None or not getattr(tokenizer, "tool_call_start_tokens", None):
        return None
    if not fmt.open_ended and not getattr(tokenizer, "tool_call_end_tokens", None):
        return None
    return fmt
class TokenBytes:
    """Exact UTF-8 bytes each token contributes; None for special/added tokens."""

    def __init__(self, tokenizer):
        hf = getattr(tokenizer, "_tokenizer", tokenizer)
        self.tokenizer = tokenizer
        fmt = format_for(tokenizer)
        self.literals = fmt.literals if fmt else ()
        self.special = set(getattr(hf, "all_special_ids", None) or ())
        self.special.update(getattr(hf, "added_tokens_decoder", None) or ())
        detokenizer = getattr(tokenizer, "_detokenizer", None)
        self.tokenmap = getattr(detokenizer, "tokenmap", None)
        self.size = len(self.tokenmap) if self.tokenmap is not None else len(hf)
        self.cache = {}
        self.byte_decoder = None
        if self.tokenmap is not None and self.tokenmap and isinstance(self.tokenmap[0], str):
            from mlx_lm.tokenizer_utils import _byte_decoder
            self.byte_decoder = _byte_decoder()

    def __call__(self, token):
        if token in self.cache:
            return self.cache[token]
        if token in self.special:
            value = self._literal(token)
        elif self.tokenmap is None:
            value = self.tokenizer.decode([token]).encode("utf-8")
        elif token >= len(self.tokenmap):
            value = None
        elif self.byte_decoder is not None:
            value = bytearray()
            for c in self.tokenmap[token]:
                b = self.byte_decoder.get(c)
                if b is None:
                    value.extend(c.encode("utf-8"))
                else:
                    value.append(b)
            value = bytes(value)
        else:
            value = self.tokenmap[token].replace("\u2581".encode("utf-8"), b" ")
        self.cache[token] = value
        return value

    def _literal(self, token):
        """Special tokens that the tool format spells as literal text, e.g. `<|"|>`."""
        if not self.literals:
            return None
        text = self.tokenizer.decode([token])
        return text.encode("utf-8") if text in self.literals else None


def _utf8(pending, data):
    decoder = codecs.getincrementaldecoder("utf-8")()
    decoder.setstate((pending, 0))
    try:
        text = decoder.decode(data)
    except UnicodeDecodeError:
        return None
    return text, decoder.getstate()[0]


class ToolCallSampler:
    """Wrap a sampler; constrain only tokens inside a tool-call span.

    Outside a call the sampled array is returned untouched and read one step later (it is then
    already evaluated), so free text keeps MLX's asynchronous decode pipeline.
    """

    def __init__(self, sampler, tokenizer, schemas, initial_state, ops, token_bytes=None):
        self.sampler = sampler
        self.schemas = schemas
        self.ops = ops
        self.bytes = token_bytes or TokenBytes(tokenizer)
        self.start = tuple(tokenizer.tool_call_start_tokens)
        self.end = tuple(tokenizer.tool_call_end_tokens)
        self.fmt = format_for(tokenizer) or _JSON
        thinking = getattr(tokenizer, "has_thinking", False)
        self.think_start = tuple(tokenizer.think_start_tokens or ()) if thinking else ()
        self.think_end = tuple(tokenizer.think_end_tokens or ()) if thinking else ()
        self.thinking = initial_state == "reasoning" and bool(self.think_end)
        self.history = []
        self.pending = None
        self.matcher = None
        self.utf8 = b""
        self.closing = []
        self.trailing = 0
        self.corrections = 0
        self.calls = 0

    def observe(self, token):
        """A wrapping sampler committed `token` without calling this sampler."""
        self._flush()
        self._settle()
        if self.matcher is not None:
            self._release("an outer sampler forced a token inside the call")
        self._commit_free(token)

    def __call__(self, logprobs):
        self._flush()
        if self.closing:
            token = self.closing.pop(0)
            if not self.closing:
                self._finish_call()
            return self.ops.force(logprobs, token)
        self._settle()
        if self.matcher is None:
            sampled = self.sampler(logprobs)
            self.pending = sampled
            return sampled
        return self._constrained(logprobs)

    def _flush(self):
        if self.pending is not None:
            token = int(self.pending.item())
            self.pending = None
            self._commit_free(token)

    def _commit_free(self, token):
        history = self.history
        history.append(token)
        del history[:-64]
        if self.thinking:
            if self.think_end and tuple(history[-len(self.think_end):]) == self.think_end:
                self.thinking = False
                history.clear()
        elif self.think_start and tuple(history[-len(self.think_start):]) == self.think_start:
            self.thinking = True
            history.clear()
        elif self.start and tuple(history[-len(self.start):]) == self.start:
            self.matcher = self.fmt.make(self.schemas)
            self.calls += 1
            history.clear()

    def _check(self, token):
        matcher = self.matcher
        if self.end and token == self.end[0] and matcher.can_end:
            return "end"
        if matcher.complete:
            data = self.bytes(token)
            if data and self.trailing < 2 and not data.strip(b" \t\r\n"):
                return "space"
            return None
        data = self.bytes(token)
        if not data:
            return None
        decoded = _utf8(self.utf8, data)
        if decoded is None:
            return None
        text, pending = decoded
        candidate = matcher.clone()
        if not candidate.feed(text):
            return None
        return candidate, pending

    def _commit(self, result):
        if result == "end":
            self.closing = list(self.end[1:])
            if not self.closing:
                self._finish_call()
        elif result == "space":
            self.trailing += 1
        else:
            self.matcher, self.utf8 = result

    def _constrained(self, logprobs):
        rejected = []
        for _ in range(_ATTEMPTS):
            masked = self.ops.exclude(logprobs, rejected) if rejected else logprobs
            sampled = self.sampler(masked)
            token = int(sampled.item())
            result = self._check(token)
            if result is not None:
                self._commit(result)
                return sampled
            rejected.append(token)
            if self.matcher.complete:
                break  # Only the closing marker remains; force it below.
        self.corrections += 1
        if self.matcher.complete:
            if not self.end:
                self._finish_call()
                sampled = self.sampler(logprobs)
                self.pending = sampled
                return sampled
            self._commit("end")
            return self.ops.force(logprobs, self.end[0])
        excluded = set(rejected)
        viable = {}
        for token in self.ops.ranked(logprobs, _RANKED):
            if token in excluded:
                continue
            result = self._check(token)
            if result is not None:
                viable[token] = result
                if len(viable) >= _VIABLE:
                    break
        if len(viable) > 1:
            sampled = self.sampler(self.ops.only(logprobs, list(viable)))
            token = int(sampled.item())
            if token in viable:
                self._commit(viable[token])
                return sampled
        if viable:
            token, result = next(iter(viable.items()))
            self._commit(result)
            return self.ops.force(logprobs, token)
        for token in range(self.bytes.size):
            if token in excluded:
                continue
            result = self._check(token)
            if result is not None:
                self._commit(result)
                return self.ops.force(logprobs, token)
        self._release("no token continues a valid call")
        sampled = self.sampler(logprobs)
        self.pending = sampled
        return sampled

    def _settle(self):
        """Open-ended formats end their call as soon as its body is complete."""
        if self.matcher is not None and not self.end and self.matcher.complete:
            self._finish_call()

    def _finish_call(self):
        if self.corrections:
            logging.info("Constrained tool call: corrected %d token(s)", self.corrections)
        self.matcher = None
        self.utf8 = b""
        self.trailing = 0
        self.corrections = 0
        self.history.clear()

    def _release(self, reason):
        logging.warning("Releasing tool-call constraint: %s", reason)
        self.matcher = None
        self.utf8 = b""
        self.trailing = 0
        self.corrections = 0
        self.closing = []
        self.history.clear()


def supported(tokenizer):
    """True when `format_for` recognises this tokenizer's tool parser and call markers."""
    return format_for(tokenizer) is not None


class MlxOps:
    """Logit operations on MLX arrays (one row per request)."""

    def __init__(self, mx):
        self.mx = mx

    def force(self, logprobs, token):
        return self.mx.full(logprobs.shape[:-1], token, dtype=self.mx.uint32)

    def exclude(self, logprobs, tokens):
        mx = self.mx
        penalty = mx.zeros((logprobs.shape[-1],), dtype=logprobs.dtype)
        penalty[mx.array(tokens)] = -mx.inf
        return logprobs + penalty

    def only(self, logprobs, tokens):
        mx = self.mx
        mask = mx.full((logprobs.shape[-1],), -mx.inf, dtype=logprobs.dtype)
        mask[mx.array(tokens)] = 0
        return logprobs + mask

    def ranked(self, logprobs, count):
        mx = self.mx
        row = logprobs.reshape(-1, logprobs.shape[-1])[0]
        count = min(count, row.shape[0])
        top = mx.argpartition(-row, kth=count - 1)[:count]
        order = mx.argsort(-row[top])
        return top[order].tolist()
