# Dial changed-line coverage collector, injected through PYTHONPATH for one verification check.
# Records which lines of DIAL_COVERAGE_FILES ran and writes them, with each file's executable
# lines, to DIAL_COVERAGE_DIR/py-<pid>.json at exit. Any project sitecustomize still runs.
import os as _os
import sys as _sys


def _dial_coverage():
    import atexit
    import json

    targets = {}
    for path in _os.environ.get("DIAL_COVERAGE_FILES", "").split(_os.pathsep):
        if path:
            targets[_os.path.realpath(path)] = set()
    out = _os.environ.get("DIAL_COVERAGE_DIR")
    if not targets or not out:
        return
    names = {}

    def hits(filename):
        found = names.get(filename, 0)
        if found == 0:
            try:
                found = targets.get(_os.path.realpath(filename))
            except (OSError, ValueError):
                found = None
            names[filename] = found
        return found

    monitoring = getattr(_sys, "monitoring", None)
    tool = None
    if monitoring is not None:
        for candidate in (monitoring.COVERAGE_ID, 4, 5):
            try:
                monitoring.use_tool_id(candidate, "dial-coverage")
                tool = candidate
                break
            except ValueError:
                continue
    if tool is not None:
        def line(code, number):
            found = hits(code.co_filename)
            if found is not None:
                found.add(number)
            return monitoring.DISABLE

        monitoring.register_callback(tool, monitoring.events.LINE, line)
        monitoring.set_events(tool, monitoring.events.LINE)
    else:
        import threading

        def trace(frame, event, arg):
            found = hits(frame.f_code.co_filename)
            if found is None:
                return None

            def local(frame, event, arg):
                if event == "line":
                    found.add(frame.f_lineno)
                return local

            found.add(frame.f_lineno)
            return local

        _sys.settrace(trace)
        threading.settrace(trace)

    def executable(path):
        try:
            with open(path, "rb") as handle:
                code = compile(handle.read(), path, "exec")
        except Exception:
            return None
        lines, stack = set(), [code]
        while stack:
            code = stack.pop()
            if hasattr(code, "co_lines"):
                lines.update(number for _, _, number in code.co_lines() if number)
            else:
                import dis
                lines.update(number for _, number in dis.findlinestarts(code))
            stack.extend(const for const in code.co_consts if hasattr(const, "co_code"))
        return sorted(lines)

    def write():
        report = {
            path: {"executed": sorted(found), "executable": executable(path)}
            for path, found in targets.items()
        }
        try:
            _os.makedirs(out, exist_ok=True)
            with open(_os.path.join(out, "py-%d.json" % _os.getpid()), "w") as handle:
                json.dump(report, handle)
        except OSError:
            pass

    atexit.register(write)


def _dial_chain():
    here = _os.path.dirname(_os.path.abspath(__file__))
    for entry in list(_sys.path):
        directory = _os.path.abspath(entry or ".")
        if directory == here:
            continue
        candidate = _os.path.join(directory, "sitecustomize.py")
        if _os.path.isfile(candidate):
            with open(candidate, "rb") as handle:
                source = handle.read()
            exec(compile(source, candidate, "exec"), {"__name__": "sitecustomize", "__file__": candidate})
            return


try:
    _dial_coverage()
finally:
    _dial_chain()
