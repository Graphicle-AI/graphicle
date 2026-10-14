"""pytest plugin: record Python-level calls between functions under a root.

Usage:
  TRACE_ROOT=/repo TRACE_OUT=calls.json python -m pytest -p trace_calls ...

Records each distinct (caller, callee) pair of code objects whose files lie
under TRACE_ROOT, as [file, qualname, firstlineno] triples (file relative to
the root). Calls reached through C code (map(), sorted(key=...)) are
attributed to the nearest Python caller. Generator and coroutine resumptions
are skipped: a frame counts when it starts (`RESUME 0`), not when resumed.
"""
import dis
import json
import os
import sys
import threading

ROOT = os.path.realpath(os.environ.get("TRACE_ROOT", os.getcwd())) + os.sep
OUT = os.environ.get("TRACE_OUT", "calls.json")
RESUME = dis.opmap.get("RESUME")
_edges = set()
_inside = {}


def _local(code):
    hit = _inside.get(code)
    if hit is None:
        name = code.co_filename
        path = os.path.realpath(name)
        hit = (not name.startswith("<") and path.startswith(ROOT)
               and "/site-packages/" not in path)
        _inside[code] = hit
    return hit


def _key(code):
    rel = os.path.relpath(os.path.realpath(code.co_filename), ROOT)
    return (rel, getattr(code, "co_qualname", code.co_name), code.co_firstlineno)


def _resumed(frame):
    code = frame.f_code
    if RESUME is None or not code.co_flags & 0x3A0:  # generator/coroutine flags
        return False
    raw, at = code.co_code, frame.f_lasti
    return 0 <= at < len(raw) - 1 and raw[at] == RESUME and raw[at + 1] != 0


def _profile(frame, event, arg):
    if event != "call":
        return
    code = frame.f_code
    if not _local(code) or _resumed(frame):
        return
    caller = frame.f_back
    while caller is not None and not _local(caller.f_code):
        caller = caller.f_back
    if caller is not None:
        _edges.add((_key(caller.f_code), _key(code)))


def pytest_configure(config):
    sys.setprofile(_profile)
    threading.setprofile(_profile)


def pytest_unconfigure(config):
    sys.setprofile(None)
    threading.setprofile(None)
    with open(OUT, "w") as f:
        json.dump(sorted([list(a), list(b)] for a, b in _edges), f)
