#!/usr/bin/env python3
"""Compare complete solution extraction, including virtual folders and symlinks."""
import random
import tempfile
import types
from pathlib import Path
from parity import Native, SOURCE, upstream_tests
from upstream_fixes import oracle_source
import graphify.extractors.sln as reference

# Pinned extractor with the reviewed post-pin fix (PR #3270) applied in memory;
# the pinned module object stays in place for upstream test patching.
oracle = types.ModuleType("graphify_oracle_sln")
oracle.__package__ = "graphify.extractors"
exec(compile(oracle_source("graphify/extractors/sln.py"), "<pinned sln.py with reviewed fixes>", "exec"), oracle.__dict__)

native = Native("sln")
original = oracle.extract_sln
checked = 0


def extract(path):
    global checked
    checked += 1
    expected = original(path)
    actual = native.result("extract.sln", path=str(path))
    if actual != expected:
        native.fail(path=str(path), source=path.read_text(errors="replace") if path.is_file() else None,
                    expected=expected, actual=actual)
    return actual


try:
    reference.extract_sln = extract
    # test_sln_solution_folder_ids_are_relative asserts the pre-#3270 folder node
    # and is replaced upstream by the two cases reproduced deterministically below.
    tests = upstream_tests(*["tests/test_dotnet.py::"+name for name in [
        "test_sln_extracts_projects", "test_sln_contains_edges", "test_sln_project_dependency"]],
        *["tests/test_languages.py::"+name for name in ["test_sln_no_error", "test_sln_finds_projects", "test_sln_contains_edges", "test_sln_project_dependency_edges"]])
    for path in (SOURCE / "tests/fixtures").rglob("*.sln"):
        extract(path)
    rng = random.Random(20260923)
    with tempfile.TemporaryDirectory(prefix="graphify-sln-parity-") as directory:
        root = Path(directory)
        (root / "real").mkdir()
        (root / "alias").symlink_to(root / "real", target_is_directory=True)
        extract(root / "missing.sln")
        extract(root)
        # PR #3270 (upstream tests test_sln_solution_folder_is_not_a_node and
        # test_sln_project_named_like_its_path_is_still_a_node): the type GUID,
        # not `path == name`, decides what is a virtual folder.
        folder = root / "App.sln"
        folder.write_text('Microsoft Visual Studio Solution File, Format Version 12.00\n'
            'Project("{2150E333-8FDC-42A3-9474-1A3956D46DE8}") = "Plugins", "Plugins", "{11111111-1111-1111-1111-111111111111}"\n'
            '    ProjectSection(ProjectDependencies) = postProject\n        {22222222-2222-2222-2222-222222222222} = {22222222-2222-2222-2222-222222222222}\n    EndProjectSection\nEndProject\n'
            'Project("{FAE04EC0-301F-11D3-BF4B-00C04F79EFBC}") = "App", "App\\App.csproj", "{22222222-2222-2222-2222-222222222222}"\n'
            '    ProjectSection(ProjectDependencies) = postProject\n        {11111111-1111-1111-1111-111111111111} = {11111111-1111-1111-1111-111111111111}\n    EndProjectSection\nEndProject\n'
            'Project("{FAE04EC0-301F-11D3-BF4B-00C04F79EFBC}") = "Tools", "Tools", "{33333333-3333-3333-3333-333333333333}"\nEndProject\n', encoding="utf-8")
        result = extract(folder)
        labels = [n["label"] for n in result["nodes"]]
        assert labels == ["App.sln", "App", "Tools"] and all(str(root) not in n["id"] for n in result["nodes"] if n["label"] == "Plugins"), result
        assert not any(e["relation"] == "imports" for e in result["edges"]), result["edges"]
        for case in range(80):
            text = []
            count = rng.randrange(1, 15)
            for i in range(count):
                name = rng.choice([f"Project{i}",f"Folder{i}", f"É日本語{i}"])
                project = name if name.startswith("Folder") else f"alias\\sub\\..\\{name}.csproj"
                kind = rng.choice(["{aa-bb}", "{2150E333-8FDC-42A3-9474-1A3956D46DE8}", "{2150e333-8fdc-42a3-9474-1a3956d46de8}", "2150E333-8FDC-42A3-9474-1A3956D46DE8", ""])
                text.append(f'Project("{kind}") = "{name}", "{project}", "{{{i:08X}-01}}"')
                text.append("    ProjectSection(ProjectDependencies) = postProject")
                for target in rng.sample(range(count), rng.randrange(count + 1)):
                    text.append(f'        {{{target:08x}-01}} = {{{target:08x}-01}}')
                text.extend(["    EndProjectSection", "EndProject"])
            separator = rng.choice(["\n", "\r\n", "\r", "\u2028"])
            path = root / f"solution-{case}.sln"
            path.write_bytes(separator.join(text).encode() + b"\xff\xfe")
            extract(path)
finally:
    native.close()

native.receipt("graphify/extractors/sln.py", ["src/extractors/sln.rs", "src/fsutil.rs"],
               extractions_checked=checked, upstream_tests=tests,
               comparisons="Complete ordered nodes/edges, skipped virtual folders (by type GUID) and dependencies on them, same-named real projects, project GUIDs, missing files, CRLF, Unicode, invalid UTF-8, symlink resolution.")
