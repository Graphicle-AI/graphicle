"""Qualify the shared exporter palette and the declaration-only package module."""
import ast
from parity import Native, SOURCE
from graphify.exporters.base import COMMUNITY_COLORS

native = Native('exporters_base')
try:
    assert native.result('exporters.base') == COMMUNITY_COLORS
    native.receipt('graphify/exporters/base.py', ['src/exporters/base.rs'],
                   compared_palette_entries=len(COMMUNITY_COLORS),
                   adaptations=['The shared palette is a statically sized Rust constant.'])
    tree = ast.parse((SOURCE / 'graphify/exporters/__init__.py').read_text())
    assert len(tree.body) == 1 and isinstance(tree.body[0], ast.Expr)
    assert isinstance(tree.body[0].value, ast.Constant) and isinstance(tree.body[0].value.value, str)
    styles = native.result('exporters.html', function='_html_styles')
    assert styles['value'] and isinstance(styles['value'], str)
    native.name = 'exporters_package'
    native.receipt('graphify/exporters/__init__.py', ['src/exporters/mod.rs'],
                   original_executable_statements=0,
                   adaptations=['The Python package contains only its docstring. Rust declares the native base, HTML and graph database modules; the palette and HTML namespace are invoked here, with format behavior separately qualified.'])
finally:
    native.close()
