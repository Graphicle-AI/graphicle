#!/usr/bin/env python3
"""Route oracle for FastAPI apps: import every module under DIR and list the
routes its FastAPI apps and APIRouters really serve, with the endpoint each
calls (file, function name, definition line).

Usage: python fastapi_routes.py REPO DIR OUT.json   (with FastAPI installed)

A module is imported on its own; one that fails (a package module with
relative imports) is imported again by its dotted name below the first
directory without `__init__.py`. Modules that still fail (missing optional
packages, deliberate errors in tutorial files) are listed under "skipped".

Served paths: a router or sub-application whose every route is also served
by another app or router (through `include_router` or `mount`, under the same
or a longer path) is mounted, so its own paths are not served; the paths of
every app or router that is not mounted are. Sub-applications are followed
through `Mount` with the mount path as prefix.

A route is [relative file of the endpoint, endpoint name, def line, METHOD,
path]; websocket routes use the method WEBSOCKET.
"""
import importlib, importlib.util, inspect, json, sys, warnings
from pathlib import Path


def load(path, repo):
    """The module at `path`, or (None, error)."""
    name = "oracle_" + "_".join(path.relative_to(repo).with_suffix("").parts)
    try:
        spec = importlib.util.spec_from_file_location(name, path)
        module = importlib.util.module_from_spec(spec)
        sys.path.insert(0, str(path.parent))
        try:
            spec.loader.exec_module(module)
        finally:
            sys.path.pop(0)
        return module, None
    except BaseException as e:  # noqa: BLE001 - tutorial files may exit or raise anything
        first = e
    parts = [] if path.name == "__init__.py" else [path.stem]
    d = path.parent
    while (d / "__init__.py").exists() and d != repo:
        parts.insert(0, d.name)
        d = d.parent
    if len(parts) < 2:
        return None, first
    top = parts[0]
    for k in [k for k in sys.modules if k == top or k.startswith(top + ".")]:
        del sys.modules[k]
    sys.path.insert(0, str(d))
    try:
        return importlib.import_module(".".join(parts)), None
    except BaseException as e:  # noqa: BLE001
        return None, e
    finally:
        sys.path.pop(0)


def endpoint_key(endpoint, repo, sub):
    code = endpoint.__code__
    try:
        rel = str(Path(code.co_filename).resolve().relative_to(repo))
    except ValueError:
        return None
    if not rel.startswith(sub):
        return None  # FastAPI's own docs/openapi endpoints, shared by every app
    # co_firstlineno is the first decorator; use the `def` line.
    try:
        lines, start = inspect.getsourcelines(endpoint)
        offset = next(i for i, l in enumerate(lines) if l.lstrip().startswith(("def ", "async def ")))
        line = start + offset
    except (OSError, StopIteration, TypeError):
        line = code.co_firstlineno
    return rel, endpoint.__name__, line


def walk(routes, prefix, repo, sub, out, depth=0):
    """(endpoint key, METHOD, path) of `routes`, following included routers
    and mounts."""
    from starlette.routing import Mount
    try:
        # FastAPI >= 0.13x keeps included routers lazily (`_IncludedRouter`);
        # this yields their effective routes, as routing and OpenAPI see them.
        from fastapi.routing import iter_route_contexts
        routes = list(iter_route_contexts(list(routes)))
    except ImportError:
        routes = list(routes)
    for r in routes:
        original = getattr(r, "original_route", r)
        if isinstance(original, Mount):
            if depth < 8:
                walk(getattr(original, "routes", None) or [], prefix + (r.path or ""), repo, sub, out, depth + 1)
            continue
        endpoint = getattr(r, "endpoint", None)
        if endpoint is None or not hasattr(endpoint, "__code__"):
            continue
        key = endpoint_key(endpoint, repo, sub)
        if key is None:
            continue
        methods = sorted(getattr(r, "methods", None) or ["WEBSOCKET"])
        for m in methods:
            if m == "HEAD" and "GET" in methods:
                continue
            out.append((key, m, prefix + r.path))


def main():
    repo, sub, out = Path(sys.argv[1]).resolve(), sys.argv[2], Path(sys.argv[3])
    skipped = []
    warnings.simplefilter("ignore")
    # Tutorial modules print while importing.
    sys.stdout = sys.stderr
    containers = {}
    for path in sorted((repo / sub).rglob("*.py")):
        module, error = load(path, repo)
        if module is None:
            skipped.append([str(path.relative_to(repo)), type(error).__name__])
            continue
        from fastapi import FastAPI, APIRouter
        for value in list(vars(module).values()):
            if isinstance(value, (FastAPI, APIRouter)) and id(value) not in containers:
                routes = []
                walk(value.routes, "", repo, sub, routes)
                containers[id(value)] = (value, routes)
    # A container is mounted when another serves each of its routes, at least
    # one under a longer path or from an app (an app and a router serving the
    # same paths: the app is served).
    from fastapi import FastAPI
    items = list(containers.values())
    served = set()
    for i, (value, routes) in enumerate(items):
        mounted, strict = bool(routes), False
        for key, method, path in routes:
            covers = [(j, p) for j, (_, other) in enumerate(items) if j != i
                      for k, m, p in other if k == key and m == method and p.endswith(path)]
            if not covers:
                mounted = False
                break
            strict |= any(len(p) > len(path)
                          or (isinstance(items[j][0], FastAPI) and not isinstance(value, FastAPI))
                          for j, p in covers)
        if not (mounted and strict):
            served.update((k[0], k[1], k[2], m, p) for k, m, p in routes)
    out.write_text(json.dumps({"routes": sorted(served), "skipped": skipped}, indent=0) + "\n")


if __name__ == "__main__":
    main()
