"""dial IDA worker: one idalib database per process, JSON lines over stdin/stdout.

Request:    {"id": int, "method": str, "params": {...}}
Interrupt:  {"interrupt": int}   raises KeyboardInterrupt inside the running request with that id
Response:   {"id": int, "ok": true, "result": {...}} | {"id": int, "ok": false, "error": {"type", "message"}}
Startup:    {"event": "ready"} | {"event": "error", "error": {"type", "message"}}   (then exit 3)

Stdout is reserved for protocol frames: fd 1 is redirected to stderr so stray prints from IDA
or user code cannot corrupt the stream. EOF on stdin closes the database without saving.
"""

import _thread
import ast
import contextlib
import io
import json
import os
import queue
import sys
import threading
import time
import traceback

_proto = os.fdopen(os.dup(1), "w", encoding="utf-8", buffering=1)
os.dup2(2, 1)


def _send(frame):
    _proto.write(json.dumps(frame, ensure_ascii=False, default=str) + "\n")
    _proto.flush()


def _error(e):
    return {"type": type(e).__name__, "message": str(e) or type(e).__name__}


try:
    import idapro  # noqa: F401  must precede every other ida_* import
    import ida_auto
    import ida_bytes
    import ida_domain
    import ida_funcs
    import ida_hexrays
    import ida_loader
    import ida_name
    import ida_typeinf
    from ida_domain.database import Database, IdaCommandOptions
except Exception as e:  # runtime missing or unusable: report once, then exit
    _send({"event": "error", "error": _error(e)})
    sys.exit(3)

_lock = threading.Lock()
_running = None  # request id currently executing
_interrupted = False  # an interrupt was scheduled for `_running`
_requests = queue.Queue()
_DB = None
_NS = None


def _reader():
    global _interrupted
    for line in sys.stdin:
        try:
            frame = json.loads(line)
        except ValueError:
            frame = None
        if isinstance(frame, dict) and "interrupt" in frame:
            with _lock:
                if _running is not None and _running == frame["interrupt"] and not _interrupted:
                    _interrupted = True
                    _thread.interrupt_main()
            continue
        _requests.put(line)
    _requests.put("")


def _run(req_id, fn, params):
    """Run fn(params); return (ok, value). A scheduled interrupt is always absorbed here, so it can
    never escape into the response writer."""
    global _running, _interrupted
    result = None
    try:
        with _lock:
            _running = req_id
        try:
            result = (True, fn(params))
        except KeyboardInterrupt:
            result = (False, KeyboardInterrupt("interrupted"))
        except Exception as e:
            result = (False, e)
        with _lock:
            _running = None
            pending = _interrupted
            _interrupted = False
        if pending:  # the interrupt was scheduled but not yet delivered: wait for it here
            deadline = time.monotonic() + 0.5
            while time.monotonic() < deadline:
                time.sleep(0.01)
        return result
    except KeyboardInterrupt:  # late delivery during bookkeeping
        with _lock:
            _running = None
            _interrupted = False
        return result if result is not None else (False, KeyboardInterrupt("interrupted"))


def _db():
    if _DB is None:
        raise RuntimeError("no database is open")
    return _DB


def _idb_path():
    return ida_loader.get_path(ida_loader.PATH_TYPE_IDB)


def _open(params):
    global _DB, _NS
    if _DB is not None:
        raise RuntimeError("a database is already open in this worker")
    path = params["path"]
    _DB = Database.open(path, IdaCommandOptions(auto_analysis=True, new_database=False), save_on_close=False)
    ida_auto.auto_wait()
    ida_hexrays.init_hexrays_plugin()
    _NS = {
        "__name__": "__ida_exec__",
        "__builtins__": __builtins__,
        "db": _DB,
        "ida_domain": ida_domain,
        "ida_auto": ida_auto,
        "ida_bytes": ida_bytes,
        "ida_funcs": ida_funcs,
        "ida_hexrays": ida_hexrays,
        "ida_loader": ida_loader,
        "ida_name": ida_name,
        "ida_typeinf": ida_typeinf,
    }
    return {
        "idb": _idb_path(),
        "module": _DB.module,
        "format": _DB.format,
        "arch": _DB.architecture,
        "bitness": _DB.bitness,
    }


def _save(_params):
    p = _idb_path()
    if not ida_loader.save_database(p, 0):
        raise RuntimeError(f"save_database failed for {p}")
    return {"idb": p}


def _exec(params):
    code = params["code"]
    tree = ast.parse(code, mode="exec")
    last = None
    if tree.body and isinstance(tree.body[-1], ast.Expr):
        last = ast.Expression(tree.body.pop().value)
    out = io.StringIO()
    value = None
    error = None
    try:
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(out):
            exec(compile(tree, "<ida>", "exec"), _NS)
            if last is not None:
                value = eval(compile(last, "<ida>", "eval"), _NS)
    except KeyboardInterrupt:
        error = "KeyboardInterrupt: interrupted"
    except BaseException as e:  # user code: report, keep the worker alive
        error = "".join(traceback.format_exception(type(e), e, e.__traceback__)).rstrip()
    return {
        "output": out.getvalue(),
        "value": None if value is None else repr(value),
        "error": error,
    }


def _resolve(target):
    """Hex address (0x...) or symbol name -> effective address."""
    db = _db()
    s = str(target).strip()
    if not s:
        raise ValueError("empty target")
    try:
        ea = int(s, 16) if s.lower().startswith("0x") else None
    except ValueError:
        ea = None
    if ea is None:
        ea = ida_name.get_name_ea(ida_name.BADADDR, s)
        if ea == ida_name.BADADDR:
            ea = ida_name.get_name_ea(ida_name.BADADDR, "_" + s)
    if ea == ida_name.BADADDR:
        raise ValueError(f"unknown target {target!r}")
    if not (db.minimum_ea <= ea < db.maximum_ea):
        raise ValueError(f"address 0x{ea:x} is outside the database")
    return ea


def _rename(params):
    ea = _resolve(params["target"])
    name = params["name"]
    old = ida_name.get_name(ea)
    if not ida_name.set_name(ea, name, ida_name.SN_CHECK | ida_name.SN_NOWARN):
        raise RuntimeError(f"rename to {name!r} failed at 0x{ea:x} (name invalid or in use)")
    return {"ea": f"0x{ea:x}", "old": old, "new": name}


def _comment(params):
    ea = _resolve(params["target"])
    text = params["text"]
    rpt = bool(params.get("repeatable"))
    if not ida_bytes.set_cmt(ea, text, rpt):
        raise RuntimeError(f"set comment failed at 0x{ea:x}")
    func = ida_funcs.get_func(ea)
    if func is not None and func.start_ea == ea:
        ida_funcs.set_func_cmt(func, text, rpt)
    return {"ea": f"0x{ea:x}"}


def _set_type(params):
    ea = _resolve(params["target"])
    decl = params["decl"].strip()
    if not decl.endswith(";"):
        decl += ";"
    tif = ida_typeinf.tinfo_t()
    if ida_typeinf.parse_decl(tif, None, decl, ida_typeinf.PT_SIL) is None:
        raise ValueError(f"cannot parse declaration: {decl}")
    if not ida_typeinf.apply_tinfo(ea, tif, ida_typeinf.TINFO_DEFINITE):
        raise RuntimeError(f"apply type failed at 0x{ea:x}")
    ida_hexrays.mark_cfunc_dirty(ea)
    return {"ea": f"0x{ea:x}", "type": str(tif)}


def _make_function(params):
    ea = _resolve(params["target"])
    if not ida_funcs.add_func(ea):
        raise RuntimeError(f"cannot create function at 0x{ea:x}")
    ida_auto.auto_wait()
    name = ida_funcs.get_func_name(ea)
    return {"ea": f"0x{ea:x}", "name": name}


_METHODS = {
    "open": _open,
    "save": _save,
    "exec": _exec,
    "rename": _rename,
    "comment": _comment,
    "set_type": _set_type,
    "make_function": _make_function,
}


def _handle(line):
    try:
        req = json.loads(line)
    except ValueError as e:
        _send({"id": None, "ok": False, "error": _error(e)})
        return
    req_id = req.get("id")
    method = req.get("method")
    params = req.get("params") or {}
    if method == "close":
        save = bool(params.get("save", True))
        try:
            if _DB is not None:
                _DB.close(save=save)
            _send({"id": req_id, "ok": True, "result": {"saved": save}})
        except Exception as e:
            _send({"id": req_id, "ok": False, "error": _error(e)})
            sys.exit(1)
        sys.exit(0)
    if method == "exec" and _NS is None:
        _send({"id": req_id, "ok": False, "error": {"type": "RuntimeError", "message": "no database is open"}})
        return
    fn = _METHODS.get(method)
    if fn is None:
        _send({"id": req_id, "ok": False, "error": {"type": "ValueError", "message": f"unknown method {method!r}"}})
        return
    ok, value = _run(req_id, fn, params)
    if ok:
        _send({"id": req_id, "ok": True, "result": value})
    else:
        _send({"id": req_id, "ok": False, "error": _error(value)})


def _shutdown_on_eof():
    if _DB is not None:
        with contextlib.suppress(Exception):
            _DB.close(save=False)
    sys.exit(0)


def main():
    _send({"event": "ready"})
    threading.Thread(target=_reader, name="dial-ida-stdin", daemon=True).start()
    while True:
        try:
            line = _requests.get()
        except KeyboardInterrupt:  # stray interrupt while idle
            continue
        if not line:  # EOF: host went away; discard unsaved changes
            _shutdown_on_eof()
        if line.strip():
            _handle(line)


if __name__ == "__main__":
    main()
