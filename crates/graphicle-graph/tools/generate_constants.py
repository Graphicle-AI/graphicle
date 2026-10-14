#!/usr/bin/env python3
"""Regenerate embedded constant tables from the pinned Python sources.

Each asset under `src/assets/` that mirrors module-level literals is rebuilt
from the pinned module's AST, so literal order is the source order and no
Python module is imported. Extractor functions in dispatch tables become their
registry key (`extract_python` -> `python`).

Usage: generate_constants.py [--check] [--source DIR] [ASSET ...]
With --check nothing is written; differing assets are reported and the exit
status is non-zero.
"""
import argparse
import ast
import json
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]

# asset -> (module, [constant names]); only literal tables are listed here.
TABLES = {
    'extract/constants.json': ('graphify/extract.py', [
        '_LANG_FAMILY_BY_EXT', '_LANGUAGE_BUILTIN_BASE_CLASSES', '_CASE_INSENSITIVE_EXTS',
        '_XAML_NON_EVENT_ATTRS', '_DISPATCH', '_SHEBANG_DISPATCH',
    ]),
}


class Unsupported(ValueError):
    pass


def literal(node, names):
    """Evaluate a constant expression, preserving literal order."""
    if isinstance(node, ast.Constant):
        return node.value
    if isinstance(node, ast.Name):
        if node.id.startswith('extract_'):
            return node.id.removeprefix('extract_')
        if node.id in names:
            return names[node.id]
        raise Unsupported(f'name {node.id}')
    if isinstance(node, (ast.List, ast.Tuple)):
        return [literal(e, names) for e in node.elts]
    if isinstance(node, ast.Set):
        # Sets have no order in Python; the assets store them sorted.
        return sorted(literal(e, names) for e in node.elts)
    if isinstance(node, ast.Dict):
        out = {}
        for k, v in zip(node.keys, node.values):
            if k is None:  # {**other}
                out.update(literal(v, names))
            else:
                out[literal(k, names)] = literal(v, names)
        return out
    if isinstance(node, ast.Call) and isinstance(node.func, ast.Name) and node.func.id in ('frozenset', 'set', 'tuple', 'list') and len(node.args) <= 1 and not node.keywords:
        items = literal(node.args[0], names) if node.args else []
        return sorted(items) if node.func.id in ('frozenset', 'set') else items
    if isinstance(node, ast.BinOp) and isinstance(node.op, ast.BitOr):
        left, right = literal(node.left, names), literal(node.right, names)
        if isinstance(left, list) and isinstance(right, list):
            return sorted(set(left) | set(right))
        if isinstance(left, dict) and isinstance(right, dict):
            return {**left, **right}
    raise Unsupported(ast.dump(node)[:120])


def module_constants(path):
    tree = ast.parse(path.read_text(), filename=str(path))
    names = {}
    for stmt in tree.body:
        targets, value = [], None
        if isinstance(stmt, ast.Assign):
            targets, value = [t for t in stmt.targets if isinstance(t, ast.Name)], stmt.value
        elif isinstance(stmt, ast.AnnAssign) and isinstance(stmt.target, ast.Name) and stmt.value is not None:
            targets, value = [stmt.target], stmt.value
        for target in targets:
            try:
                names[target.id] = literal(value, names)
            except Unsupported:
                pass
    return names


def build(source, asset):
    module, keys = TABLES[asset]
    names = module_constants(source / module)
    missing = [k for k in keys if k not in names]
    if missing:
        raise SystemExit(f'{asset}: {module} has no literal {missing}')
    return {k: names[k] for k in keys}


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('--check', action='store_true')
    parser.add_argument('--source', type=Path)
    parser.add_argument('assets', nargs='*')
    args = parser.parse_args()
    source = args.source or Path(json.loads((ROOT / 'migration/FILES.json').read_text())['source_root'])
    status = 0
    for asset in args.assets or sorted(TABLES):
        path = ROOT / 'src/assets' / asset
        generated = build(source, asset)
        current = json.loads(path.read_text()) if path.exists() else None
        if current == generated:
            print(f'{asset}: unchanged')
            continue
        changed = sorted(k for k in generated if current is None or current.get(k) != generated[k])
        print(f'{asset}: differs in {changed}')
        if args.check:
            status = 1
            continue
        existing = path.read_text() if path.exists() else ''
        indent = 2 if existing.startswith('{\n  ') else None
        text = json.dumps(generated, indent=indent, ensure_ascii=False)
        path.write_text(text + ('\n' if existing.endswith('\n') or indent else ''))
    return status


if __name__ == '__main__':
    sys.exit(main())
