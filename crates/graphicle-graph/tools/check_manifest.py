#!/usr/bin/env python3
"""Qualify the compatibility facade against its original discovery exports."""
import tempfile
from pathlib import Path
from parity import Native
from naming import to_upstream
import graphify.manifest as manifest
import graphify.detect as detect

native = Native('manifest')
try:
    assert all(getattr(manifest, name) is getattr(detect, name) for name in manifest.__all__)
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        (root / 'a.md').write_text('# Original source\nEvidence remains on disk.\n')
        (root / 'b.py').write_text('def example():\n    return 42\n')
        path = root / 'graphify-out' / 'manifest.json'
        path.parent.mkdir()
        def call(function, **kwargs):
            # Renamed result fields (e.g. ignore_patterns) map back to the reference's.
            return to_upstream(native.result('manifest', function=function, root=str(root), manifest_path=str(path), **kwargs)['value'])
        assert call('load_manifest') == manifest.load_manifest(str(path), root=root) == {}
        full = detect.detect(root)
        call('save_manifest', files=full['files'], kind='both', now=1720000000.25)
        assert call('load_manifest') == manifest.load_manifest(str(path), root=root)
        assert call('detect_incremental') == manifest.detect_incremental(root, str(path))
        (root / 'a.md').write_text('# Rewritten source\nNew evidence.\n')
        (root / 'b.py').unlink()
        expected = manifest.detect_incremental(root, str(path))
        actual = call('detect_incremental')
        if actual != expected:
            native.fail(expected=expected, actual=actual)
        assert actual['new_total'] == 1 and actual['deleted_files'] == [str(root.resolve() / 'b.py')], actual
    native.receipt('graphify/manifest.py', ['src/manifest.rs'], compatibility_exports=manifest.__all__, actual_io=['save', 'load', 'incremental', 'rewrite', 'deletion'], adaptations=['The Rust incremental entry point accepts an explicit Detector to retain its cache across calls. The discovery dispatcher uses this compatibility entry point.'])
finally:
    native.close()
