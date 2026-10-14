"""Minimal SCIP protobuf reader (stdlib only).

Reads the fields of scip.proto that call-graph evaluation needs: document
paths, occurrences (range, symbol, roles, enclosing range) and the project
root. See https://github.com/sourcegraph/scip/blob/main/scip.proto.
"""

DEFINITION = 0x1
IMPORT = 0x2


def _varint(buf, pos):
    value = shift = 0
    while True:
        byte = buf[pos]
        pos += 1
        value |= (byte & 0x7F) << shift
        if byte < 0x80:
            return value, pos
        shift += 7


def _fields(buf):
    pos, end = 0, len(buf)
    while pos < end:
        key, pos = _varint(buf, pos)
        field, wire = key >> 3, key & 7
        if wire == 0:
            value, pos = _varint(buf, pos)
        elif wire == 2:
            length, pos = _varint(buf, pos)
            value = buf[pos:pos + length]
            pos += length
        elif wire == 5:
            value, pos = None, pos + 4
        elif wire == 1:
            value, pos = None, pos + 8
        else:
            raise ValueError(f"unsupported protobuf wire type {wire}")
        yield field, wire, value


def _int32(value):
    return value - (1 << 32) if value >= 1 << 31 else value


def _ints(value, wire):
    if wire == 0:
        return [_int32(value)]
    out, pos = [], 0
    while pos < len(value):
        item, pos = _varint(value, pos)
        out.append(_int32(item))
    return out


def _typed_range(buf, multi):
    values = {f: v for f, w, v in _fields(buf) if w == 0}
    keys = (1, 2, 3, 4) if multi else (1, 2, 3)
    return [_int32(values.get(k, 0)) for k in keys]


def span(r):
    """(start_line, start_char, end_line, end_char) from SCIP's 3/4-int form."""
    if len(r) == 3:
        return (r[0], r[1], r[0], r[2])
    if len(r) == 4:
        return tuple(r)
    return None


def _occurrence(buf):
    rng, enclosing, symbol, roles = [], [], "", 0
    for f, w, v in _fields(buf):
        if f == 1:
            rng += _ints(v, w)
        elif f == 8:
            rng = _typed_range(v, False)
        elif f == 9:
            rng = _typed_range(v, True)
        elif f == 2:
            symbol = v.decode("utf-8", "replace")
        elif f == 3:
            roles = v
        elif f == 7:
            enclosing += _ints(v, w)
        elif f == 10:
            enclosing = _typed_range(v, False)
        elif f == 11:
            enclosing = _typed_range(v, True)
    return {"range": span(rng), "symbol": symbol, "roles": roles, "enclosing": span(enclosing)}


def read_index(path):
    """(project_root or None, [documents]) of a SCIP index file."""
    data = open(path, "rb").read()
    root, docs = None, []
    for f, _, v in _fields(data):
        if f == 1:
            for f2, _, v2 in _fields(v):
                if f2 == 3:
                    uri = v2.decode()
                    if uri.startswith("file://"):
                        from urllib.parse import unquote
                        root = unquote(uri[len("file://"):])
        elif f == 2:
            doc = {"path": "", "occurrences": [], "utf16": True}
            for f2, w2, v2 in _fields(v):
                if f2 == 1:
                    doc["path"] = v2.decode()
                elif f2 == 2:
                    occ = _occurrence(v2)
                    if occ["range"]:
                        doc["occurrences"].append(occ)
                elif f2 == 6 and w2 == 0 and v2 in (1, 3):
                    doc["utf16"] = False
            docs.append(doc)
    return root, docs


def last_descriptor(symbol):
    """(name, suffix) of a global symbol's last descriptor; None for locals."""
    found = descriptors(symbol)
    if not found or not found[-1][0]:
        return None
    return found[-1]


def descriptors(symbol):
    """[(name, suffix)] of a global symbol's descriptors; [] for locals."""
    if symbol.startswith("local "):
        return []
    rest = symbol
    for _ in range(4):  # scheme, manager, package name, version
        i = 0
        while i < len(rest):
            if rest[i] == " ":
                if rest[i + 1:i + 2] == " ":
                    i += 2
                    continue
                break
            i += 1
        rest = rest[i + 1:]
    out, name, i = [], "", 0
    while i < len(rest):
        c = rest[i]
        if c == "`":
            j = i + 1
            while j < len(rest):
                if rest[j] == "`":
                    if rest[j + 1:j + 2] == "`":
                        name += "`"
                        j += 2
                        continue
                    break
                name += rest[j]
                j += 1
            i = j + 1
            continue
        if c in "/#.:!":
            out.append((name, c))
            name = ""
        elif c == "(":
            j = rest.find(")", i)
            i = len(rest) if j < 0 else j
            if rest[i + 1:i + 2] == ".":
                i += 1
            out.append((name, "()."))
            name = ""
        elif c == "[":
            j = rest.find("]", i)
            i = len(rest) if j < 0 else j
            name = ""
        else:
            name += c
        i += 1
    return out


def qualname(symbol):
    """Dotted name inside its module (`Class.method`, `outer.inner`)."""
    parts = [n for n, s in descriptors(symbol) if s in ("#", "().", ".")]
    return ".".join(parts) if parts else None


def parent_symbol(symbol):
    """The symbol with its last descriptor removed (a method's class)."""
    desc = last_descriptor(symbol)
    if not desc:
        return None
    name, suffix = desc
    tail = (f"`{name}`" if not name.replace("_", "a").isalnum() else name) + suffix
    for candidate in (tail, f"`{name}`{suffix}"):
        if symbol.endswith(candidate):
            return symbol[: -len(candidate)]
    idx = symbol.rfind(name)
    return symbol[:idx].rstrip("`") if idx > 0 else None


def utf16_to_byte(line, units):
    count = 0
    for i, ch in enumerate(line):
        if count >= units:
            return i
        count += 2 if ord(ch) > 0xFFFF else 1
    return len(line)
