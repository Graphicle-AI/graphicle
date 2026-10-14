"""Explicit development-oracle adaptations for reviewed fixes beyond the pin.

The pinned checkout is never edited. Selected hunks are applied in memory;
exact context must match once. Rust is still compared independently against
Python, including randomized inputs and complete provider transcripts.

Pinned: Graphify 0.9.88 (a9860f70), the release tag. Fixes merged upstream by
0.9.88 no longer need adaptations; the entries below are fixes upstream has not
merged. The Kotlin `line` hoist this file once carried landed as #3884, the
label-retry salvage of partial labels landed as 18d834f (0.9.73), and the Rust
`Self` type-reference filter (#2963) is subsumed by the prelude filter (#3973,
0.9.74).
"""
from parity import SOURCE

# Upstream merged the fixes these modules were once adapted for; the oracle is
# the pinned source unchanged. build.py: #3707/#3711 (0.9.66). dedup.py: the
# #3775 provenance tiebreak landed as #3786 and #3825 self-loops (0.9.69).
# engine.py: the Kotlin property `line` hoist landed as #3884 (0.9.72 tag).
# llm.py: the #3708 follow-up that keeps the labels a failed retry's siblings
# already parsed landed as 18d834f (0.9.73 tag). rust.py: `Self` joined
# _RUST_BUILTIN_TYPES with the #3973 prelude filter (0.9.74), which skips it
# where the #2963 hunk did. wiki.py: PR 3095 (index rows link to the slug each
# article was written under, #3032/#3033) landed in 0.9.88.
_UNCHANGED = {'graphify/build.py', 'graphify/dedup.py', 'graphify/extractors/engine.py',
              'graphify/extractors/rust.py', 'graphify/extractors/terraform.py', 'graphify/llm.py',
              'graphify/wiki.py'}


def adapted_modules():
    """Pinned modules with reviewed exact-context adaptations, in a stable order."""
    return sorted({'graphify/cache.py', *_EXACT_HUNKS})


def oracle_source(relative):
    text = (SOURCE / relative).read_text()
    if relative in _UNCHANGED:
        return text
    if relative == 'graphify/cache.py':
        before = '            source_path = Path(source)\n'
        after = ('            # Restore the portable path before checking its walked identity.\n'
                 '            source_path = Path(source.removeprefix("$graphify-root$/")\n'
                 '                               if isinstance(source, str) else source)\n')
        assert text.count(before) == 1
        text = text.replace(before, after, 1)
        before = '    from_given = _id_anchor(str(path), rel)\n'
        after = '''    # Keep a walked file symlink distinct from its target. Infer only the
    # enclosing directory root; the final file name belongs to the extractor.
    walked_root = None
    parent = None
    try:
        parent = Path(os.path.abspath(path)).parent
        suffix = parent.resolve().relative_to(root_resolved)
        parts = suffix.parts
        if not parts or parent.parts[-len(parts):] == parts:
            walked_root = parent
            for _ in parts:
                walked_root = walked_root.parent
    except (ValueError, OSError, RuntimeError):
        pass
    if walked_root is None and parent is not None:
        for candidate in (parent, *parent.parents):
            try:
                if candidate.resolve() == root_resolved:
                    walked_root = candidate
                    break
            except (OSError, RuntimeError):
                pass
    from_given = _id_anchor(str(path), rel)
    if not from_given and Path(path).is_absolute() and walked_root is not None:
        from_given = normalize_id(str(walked_root))
'''
        assert text.count(before) == 1
        text = text.replace(before, after, 1)
        before = '    return id_anchors, id_restore, path_anchors, str(root_resolved)\n'
        after = '''    path_restore = str(root_resolved)
    if walked_root is not None:
        path_restore = str(walked_root)
        path_anchors = sorted(set(path_anchors) | {path_restore}, key=len, reverse=True)
    return id_anchors, id_restore, path_anchors, path_restore
'''
        assert text.count(before) == 1
        text = text.replace(before, after, 1)
        before = '''            _absolutize_source_files_in(result, root)
            # Same contract for the ids and remaining paths the entry embeds
            # (#2257): without this a warm hit under a different absolute root
            # replays ids minted from the ORIGINAL root, which extract()'s
            # id-remap cannot fix because they match none of its current-path
            # keys. Order is free — source_file never carries the marker.
            _absolutize_ids_in(result, path, root)
'''
        after = '''            # Restore marked paths first; then anchor ordinary relative paths.
            _absolutize_ids_in(result, path, root)
            # Keep the requested directory spelling for both representations.
            # A physical-root entry has ordinary relative sources; resolving
            # them under a new directory alias would disagree with a cold run.
            source_root = Path(_portability_anchors(path, root)[3])
            for bucket in ("nodes", "edges", "hyperedges", "raw_calls"):
                for item in result.get(bucket, []):
                    if not isinstance(item, dict):
                        continue
                    for key in ("source_file", "definition_file"):
                        source = item.get(key)
                        if source and not Path(source).is_absolute():
                            item[key] = str(source_root / source)
'''
        assert text.count(before) == 1
        text = text.replace(before, after, 1)
    elif relative in _EXACT_HUNKS:
        for before, after in _EXACT_HUNKS[relative]:
            assert text.count(before) == 1, (relative, before)
            text = text.replace(before, after, 1)
    else:
        raise ValueError(f'No reviewed oracle adaptation for {relative}')
    return text


# Reviewed exact-context replacements for pinned modules; each `before` must
# occur exactly once. See migration/upstream-review/adaptations.json.
_EXACT_HUNKS = {
    # Review 2026-10-02: preserve qualified owners and reject ambiguous
    # short names instead of linking to the first declaration in the file.
    'graphify/extractors/vbnet.py': [
        (
            '    owner_by_short: dict[str, str] = {}\n',
            '''    owner_by_short: dict[str, set[str]] = {}
    owners: set[str] = set()

    def qualified_owner(receiver: str, caller: str) -> str | None:
        if receiver.startswith("global."):
            absolute = receiver[len("global."):]
            return absolute if absolute in owners else None
        scope = caller
        while "." in scope:
            scope = scope.rsplit(".", 1)[0]
            candidate = f"{scope}.{receiver}"
            if candidate in owners:
                return candidate
        if receiver in owners:
            return receiver
        if "." in receiver:
            return None
        choices = owner_by_short.get(receiver, set())
        return next(iter(choices)) if len(choices) == 1 else None
''',
        ),
        (
            '        owner_by_short.setdefault(name.casefold(), owner_key)\n',
            '        owner_by_short.setdefault(name.casefold(), set()).add(owner_key)\n'
            '        owners.add(owner_key)\n',
        ),
        (
            '                        target_owner = owner_by_short.get(receiver.split(".")[-1])\n',
            '                        target_owner = qualified_owner(receiver, owner_key)\n',
        ),
    ],
    # Review 2026-10-02: the optional quote in 0.9.73's negative lookahead
    # backtracks, blanking quoted JavaScript; it also matches data-type.
    'graphify/extract.py': [
        (
            'def _astro_mask_non_script(src: str) -> str:\n',
            r'''def _astro_script_is_javascript(attributes: str) -> bool:
    for attribute in re.finditer(
        r"""([^\s=/>"']+)(?:\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s>]+)))?""",
        attributes,
    ):
        if attribute.group(1).lower() == "type":
            value = next((v for v in attribute.groups()[1:] if v is not None), "")
            return value.strip().lower() in ("", "module", "text/javascript", "application/javascript")
    return True


def _astro_mask_non_script(src: str) -> str:
''',
        ),
        (
            '        if not _ASTRO_NON_JS_TYPE_RE.search(m.group(1)):\n',
            '        if _astro_script_is_javascript(m.group(1)):\n',
        ),
        # 0.9.88's EJS masker reads `<script>` types with the same pattern;
        # the native shares the reviewed parser there too.
        (
            '    for m in _EJS_SCRIPT_TAG_RE.finditer(src):\n'
            '        if _NON_JS_SCRIPT_TYPE_RE.search(m.group(1)):\n',
            '    for m in _EJS_SCRIPT_TAG_RE.finditer(src):\n'
            '        if not _astro_script_is_javascript(m.group(1)):\n',
        ),
    ],
    # PR #3020 (head 8ec7b81e): default-schema-qualified references bind to bare definitions.
    'graphify/extractors/sql.py': [
        (
            'from graphify.extractors.base import _file_stem, _make_id, _read_source_bytes\n'
            '\n',
            'from graphify.extractors.base import _file_stem, _make_id, _read_source_bytes\n'
            '\n'
            '_DEFAULT_SCHEMA = "public"\n'
            '\n',
        ),
        # Adapted: the PR appends this pass after the secondary bare-name aliases,
        # so `"public"."t"` also bound to the bare alias of a `"other"."t"`
        # definition (pg_introspect: a `"space name"."order"` table captured a
        # `SELECT * FROM "public"."order"` view). Run it first so only explicit
        # unqualified definitions are bridged; other schemas keep their stub.
        (
            '    _collect_defined_names(root)\n'
            '\n'
            '    # Secondary bare-name aliases: a reference written without a schema\n',
            '    _collect_defined_names(root)\n'
            '\n'
            '    for key in list(table_nids):\n'
            '        if "." in key:\n'
            '            continue\n'
            '        qualified = f"{_DEFAULT_SCHEMA}.{key}"\n'
            '        if qualified not in table_nids:\n'
            '            table_nids[qualified] = table_nids[key]\n'
            '\n'
            '    # Secondary bare-name aliases: a reference written without a schema\n',
        ),
        # Issue #2712 / PR #2716 (head 6c5bfc11): bracket-delimited T-SQL names read verbatim.
        (
            '\n\ndef extract_sql(path: Path, content: str | bytes | None = None) -> dict:\n',
            '\n\n# Issue #2712 / PR #2716: tree-sitter-sql has no T-SQL bracket delimiters, so\n# `[dbo].[Alpha]` parses as ERROR tokens around an object_reference spanning\n# `dbo].[Alpha`. A bracket-touched name is re-read verbatim from the source.\n_BRACKET_IDENT_RE = re.compile(\n    r\'(?:"(?:[^"\\n]|"")+"|\\[(?:[^\\]\\n]|\\]\\])+\\]|`[^`\\n]+`|[\\w$]+)\'\n    r\'(?:\\s*\\.\\s*(?:"(?:[^"\\n]|"")+"|\\[(?:[^\\]\\n]|\\]\\])+\\]|`[^`\\n]+`|[\\w$]+))*\'\n)\n\n\ndef extract_sql(path: Path, content: str | bytes | None = None) -> dict:\n',
        ),
        (
            '    def _read(n) -> str:\n        return source[n.start_byte:n.end_byte].decode("utf-8", errors="replace")\n\n',
            '    def _read(n) -> str:\n        return source[n.start_byte:n.end_byte].decode("utf-8", errors="replace")\n\n    def _ident(n) -> str:\n        text = _read(n)\n        start = n.start_byte\n        if source[start - 1:start] == b"[":\n            start -= 1\n        elif "[" not in text and "]" not in text:\n            return text\n        m = _BRACKET_IDENT_RE.match(source[start:].decode("utf-8", errors="replace"))\n        return m.group(0) if m else text\n\n',
        ),
        (
            '                return _read(c)\n',
            '                return _ident(c)\n',
        ),
        (
            '                                    elif found_ref and cc.type == "object_reference":\n'
            '                                        ref_name = _read(cc)\n',
            '                                    elif found_ref and cc.type == "object_reference":\n'
            '                                        ref_name = _ident(cc)\n',
        ),
        (
            '                                        elif found_ref and cc.type == "object_reference":\n'
            '                                            ref_name = _read(cc)\n',
            '                                        elif found_ref and cc.type == "object_reference":\n'
            '                                            ref_name = _ident(cc)\n',
        ),
        (
            '                                    ref_name = _read(ccc)\n',
            '                                    ref_name = _ident(ccc)\n',
        ),
        (
            '                    trig_name = _read(c)\n',
            '                    trig_name = _ident(c)\n',
        ),
        (
            '                    tbl_name = _read(c)\n',
            '                    tbl_name = _ident(c)\n',
        ),
        (
            '                    index_table = _read(c)\n',
            '                    index_table = _ident(c)\n',
        ),
        (
            '                    own.add(_norm_ident(_read(cc)))\n',
            '                    own.add(_norm_ident(_ident(cc)))\n',
        ),
        (
            '                            tbl = _read(cc)\n',
            '                            tbl = _ident(cc)\n',
        ),
    ],
    # PRs #3278/#3276 (head 07f910d0): qualified/generic heritage and return types.
    'graphify/extractors/apex.py': [
        (
            '    _ANNOTATION = r"(?:\\s*@\\w+(?:\\s*\\([^)]*\\))?\\s*)*"\n'
            '\n'
            '    cls_re = _re.compile(\n'
            '        rf"^{_ANNOTATION}\\s*{_ACCESS}{_SHARING}{_MOD}\\s*class\\s+(\\w+)"\n'
            '        rf"(?:\\s+extends\\s+(\\w+))?(?:\\s+implements\\s+([\\w,\\s]+))?\\s*\\{{?",\n',
            '    _ANNOTATION = r"(?:\\s*@\\w+(?:\\s*\\([^)]*\\))?\\s*)*"\n'
            '    _TYPE = r"[\\w.\\[\\]]+(?:\\s*[<>,]\\s*[\\w.\\[\\]]*)*"\n'
            '\n'
            '    def heritage_names(clause: str) -> list[str]:\n'
            '        parts: list[str] = []\n'
            '        current: list[str] = []\n'
            '        depth = 0\n'
            '        for ch in clause:\n'
            '            if ch == "<":\n'
            '                depth += 1\n'
            '            elif ch == ">":\n'
            '                depth = max(0, depth - 1)\n'
            '            elif ch == "," and depth == 0:\n'
            '                parts.append("".join(current))\n'
            '                current = []\n'
            '                continue\n'
            '            current.append(ch)\n'
            '        parts.append("".join(current))\n'
            '        names: list[str] = []\n'
            '        for raw in parts:\n'
            '            name = raw.split("<", 1)[0].strip().rsplit(".", 1)[-1].strip()\n'
            '            if name:\n'
            '                names.append(name)\n'
            '        return names\n'
            '\n'
            '    cls_re = _re.compile(\n'
            '        rf"^{_ANNOTATION}\\s*{_ACCESS}{_SHARING}{_MOD}\\s*class\\s+(\\w+)"\n'
            '        rf"(?:\\s+extends\\s+({_TYPE}))?(?:\\s+implements\\s+({_TYPE}))?\\s*\\{{?",\n',
        ),
        (
            '        rf"^{_ANNOTATION}\\s*{_ACCESS}{_SHARING}{_MOD}\\s*interface\\s+(\\w+)"\n'
            '        rf"(?:\\s+extends\\s+([\\w,\\s]+))?\\s*\\{{?",\n',
            '        rf"^{_ANNOTATION}\\s*{_ACCESS}{_SHARING}{_MOD}\\s*interface\\s+(\\w+)"\n'
            '        rf"(?:\\s+extends\\s+({_TYPE}))?\\s*\\{{?",\n',
        ),
        (
            '(?:static\\s+)?[\\w<>\\[\\]]+\\s+(\\w+)\\s*\\([^)]*\\)',
            '(?:static\\s+)?{_TYPE}\\s+(\\w+)\\s*\\([^)]*\\)',
        ),
        (
            '            if cm.group(2):\n'
            '                base = cm.group(2).strip()\n'
            '                base_nid = _make_id(stem, base)\n'
            '                if base_nid not in seen_ids:\n'
            '                    base_nid = _make_id(base)\n'
            '                if base_nid not in seen_ids:\n'
            '                    add_node(base_nid, base, lineno)\n'
            '                add_edge(class_nid, base_nid, "extends", lineno, confidence="INFERRED")\n'
            '            if cm.group(3):\n'
            '                for iface in cm.group(3).split(","):\n'
            '                    iface = iface.strip()\n'
            '                    if iface:\n'
            '                        iface_nid = _make_id(stem, iface)\n'
            '                        if iface_nid not in seen_ids:\n'
            '                            iface_nid = _make_id(iface)\n'
            '                        if iface_nid not in seen_ids:\n'
            '                            add_node(iface_nid, iface, lineno)\n'
            '                        add_edge(class_nid, iface_nid, "implements", lineno, confidence="INFERRED")\n',
            '            if cm.group(2):\n'
            '                for base in heritage_names(cm.group(2)):\n'
            '                    base_nid = _make_id(stem, base)\n'
            '                    if base_nid not in seen_ids:\n'
            '                        base_nid = _make_id(base)\n'
            '                    if base_nid not in seen_ids:\n'
            '                        add_node(base_nid, base, lineno)\n'
            '                    add_edge(class_nid, base_nid, "extends", lineno, confidence="INFERRED")\n'
            '            if cm.group(3):\n'
            '                for iface in heritage_names(cm.group(3)):\n'
            '                    iface_nid = _make_id(stem, iface)\n'
            '                    if iface_nid not in seen_ids:\n'
            '                        iface_nid = _make_id(iface)\n'
            '                    if iface_nid not in seen_ids:\n'
            '                        add_node(iface_nid, iface, lineno)\n'
            '                    add_edge(class_nid, iface_nid, "implements", lineno, confidence="INFERRED")\n',
        ),
        (
            '            if im.group(2):\n'
            '                for parent in im.group(2).split(","):\n'
            '                    parent = parent.strip()\n'
            '                    if parent:\n'
            '                        parent_nid = _make_id(stem, parent)\n'
            '                        if parent_nid not in seen_ids:\n'
            '                            parent_nid = _make_id(parent)\n'
            '                        if parent_nid not in seen_ids:\n'
            '                            add_node(parent_nid, parent, lineno)\n'
            '                        add_edge(iface_nid, parent_nid, "extends", lineno, confidence="INFERRED")\n',
            '            if im.group(2):\n'
            '                for parent in heritage_names(im.group(2)):\n'
            '                    parent_nid = _make_id(stem, parent)\n'
            '                    if parent_nid not in seen_ids:\n'
            '                        parent_nid = _make_id(parent)\n'
            '                    if parent_nid not in seen_ids:\n'
            '                        add_node(parent_nid, parent, lineno)\n'
            '                    add_edge(iface_nid, parent_nid, "extends", lineno, confidence="INFERRED")\n',
        ),
    ],
    # PR #3450 (head 207b98c0): same-page `[[#Heading]]` anchors become
    # heading --references--> heading edges. PR #3772 (a table-escaped alias
    # pipe `\|` is not part of the wikilink page) merged in 0.9.72; the
    # pattern below keeps both, with the heading captured for #3450.
    'graphify/extractors/markdown.py': [
        (
            '    root = Path(root)\n'
            '    ignored = ignored_predicate(root)\n',
            '    root = Path(root)\n'
            '    rule_root = root.resolve()\n'
            '    is_ignored = ignored_predicate(rule_root)\n'
            '    def ignored(path):\n'
            '        return is_ignored(rule_root / path.relative_to(root))\n',
        ),  # PR #3826 / issue #3822 merged in 0.9.71; the rules still need the
        # canonical root, or a root reached through a symlink (macOS /var ->
        # /private/var) matches no rule.
        (
            "_MD_WIKILINK_RE = re.compile(r'(?<!\\!)\\[\\[([^\\]|#]+?)(?:\\\\?\\|[^\\]]*|#[^\\]]*)?\\]\\]')\n",
            "_MD_WIKILINK_RE = re.compile(r'(?<!\\!)\\[\\[([^\\]|#]*?)(?:#([^\\]|]*))?(?:\\\\?\\|[^\\]]*)?\\]\\]')\n",
        ),
        (
            '    # Track heading stack for nesting: [(level, nid), ...]\n'
            '    heading_stack: list[tuple[int, str]] = []\n'
            '    in_code_block = False\n',
            '    def is_same_page(page: str) -> bool:\n'
            '        if not page.strip():\n'
            '            return True\n'
            '        resolved = _resolve_markdown_link(page, source_dir, wikilink=True)\n'
            '        return resolved is not None and _make_id(str(resolved)) == file_nid\n'
            '\n'
            '    # Track heading stack for nesting: [(level, nid), ...]\n'
            '    heading_stack: list[tuple[int, str]] = []\n'
            '    # Same-page heading anchors, resolved after the walk: (anchor, line, source nid).\n'
            '    pending_anchors: list[tuple[str, int, str]] = []\n'
            '    in_code_block = False\n',
        ),
        (
            '        for m in _MD_WIKILINK_RE.finditer(line_text):\n'
            '            add_link(m.group(1), line_num, wikilink=True)\n',
            '        for m in _MD_WIKILINK_RE.finditer(line_text):\n'
            '            page = m.group(1) or ""\n'
            '            anchor = (m.group(2) or "").strip()\n'
            '            if not page.strip() and not anchor:\n'
            '                continue\n'
            '            if anchor and is_same_page(page):\n'
            '                src_nid = heading_stack[-1][1] if heading_stack else file_nid\n'
            '                pending_anchors.append((anchor, line_num, src_nid))\n'
            '                continue\n'
            '            add_link(page, line_num, wikilink=True)\n',
        ),
        (
            '            add_mention(owner, m.group(2), line_num)\n'
            '\n'
            '    return {"nodes": nodes, "edges": edges, "raw_calls": raw_calls,\n',
            '            add_mention(owner, m.group(2), line_num)\n'
            '\n'
            '    seen_anchor_pairs: set[tuple[str, str]] = set()\n'
            '    for anchor, line_num, src_nid in pending_anchors:\n'
            '        h_nid = _make_id(stem, anchor)\n'
            '        if h_nid not in seen_ids or h_nid == src_nid:\n'
            '            continue\n'
            '        if (src_nid, h_nid) in seen_anchor_pairs:\n'
            '            continue\n'
            '        seen_anchor_pairs.add((src_nid, h_nid))\n'
            '        add_edge(src_nid, h_nid, "references", line_num)\n'
            '\n'
            '    return {"nodes": nodes, "edges": edges, "raw_calls": raw_calls,\n',
        ),
    ],
    # PR #3544 (head b26396b): define-condition dispatches like defclass, ahead
    # of the data-definer fallback, so condition hierarchies keep `inherits`.
    'graphify/extractors/commonlisp.py': [(
        '        if first_lower == "defclass":\n'
        '            _handle_defclass(top)\n',
        '        if first_lower in ("defclass", "define-condition"):\n'
        '            _handle_defclass(top)\n',
    )],
    # PR #2388 (head 14f8eb3a): the oversized/low-cohesion re-splits honour
    # the caller's resolution instead of _partition's default 1.0.
    'graphify/cluster.py': [
        (
            '            final_communities.extend(_split_community(G, nodes))\n',
            '            final_communities.extend(_split_community(G, nodes, resolution))\n',
        ),
        (
            '            splits = _split_community(G, nodes)\n',
            '            splits = _split_community(G, nodes, resolution)\n',
        ),
        (
            'def _split_community(G: nx.Graph, nodes: list[str]) -> list[list[str]]:\n',
            'def _split_community(G: nx.Graph, nodes: list[str], resolution: float = 1.0) -> list[list[str]]:\n',
        ),
        (
            '        sub_partition = _partition(subgraph)\n',
            '        sub_partition = _partition(subgraph, resolution=resolution)\n',
        ),
    ],
    # PR #3270 (head 00b70a31): solution folders (by project type GUID) are
    # not nodes; the `proj_path == proj_name` heuristic is gone.
    'graphify/extractors/sln.py': [
        (
            'from graphify.extractors.base import _make_id, _read_source_text\n'
            '\n',
            'from graphify.extractors.base import _make_id, _read_source_text\n'
            '\n'
            '_SOLUTION_FOLDER_TYPE_GUID = "2150e333-8fdc-42a3-9474-1a3956d46de8"\n'
            '\n',
        ),
        (
            '        r\'Project\\("[^"]*"\\)\\s*=\\s*"([^"]+)"\\s*,\\s*"([^"]+)"\\s*,\\s*"([^"]*)"\'\n',
            '        r\'Project\\("([^"]*)"\\)\\s*=\\s*"([^"]+)"\\s*,\\s*"([^"]+)"\\s*,\\s*"([^"]*)"\'\n',
        ),
        (
            '        proj_name = m.group(1)\n'
            '        proj_path = m.group(2).replace("\\\\", "/")\n'
            '        proj_guid = m.group(3).strip("{}")\n',
            '        proj_type = m.group(1).strip("{}").lower()\n'
            '        proj_name = m.group(2)\n'
            '        proj_path = m.group(3).replace("\\\\", "/")\n'
            '        proj_guid = m.group(4).strip("{}")\n'
            '        if proj_type == _SOLUTION_FOLDER_TYPE_GUID:\n'
            '            continue\n',
        ),
        (
            '        is_solution_folder = proj_path == proj_name\n'
            '        if is_solution_folder:\n'
            '            abs_proj = proj_name\n'
            '        else:\n'
            '            try:\n'
            '                abs_proj = str((path.parent / proj_path).resolve())\n'
            '            except Exception:\n'
            '                abs_proj = proj_path\n',
            '        try:\n'
            '            abs_proj = str((path.parent / proj_path).resolve())\n'
            '        except Exception:\n'
            '            abs_proj = proj_path\n',
        ),
    ],
}
