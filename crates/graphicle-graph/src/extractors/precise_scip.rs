//! Opt-in (`--precise`) call edges from SCIP indexes.
//!
//! [SCIP](https://github.com/sourcegraph/scip) indexes are written by
//! compiler-grade indexers: scip-python (Pyright), scip-typescript (the
//! TypeScript checker), rust-analyzer (`rust-analyzer scip .`), scip-go,
//! scip-java, scip-clang and others. With `--precise`, the indexes named by
//! `GRAPHICLE_SCIP` (a path list) and `<root>/index.scip` are read. A reference
//! occurrence that the source shows is called (`name(`, `name<T>(`, including
//! `new Name(`) and that lies inside a function or method definition's
//! enclosing range becomes a `calls` edge from that definition's graph node to
//! the referenced symbol's definition node, when both map to graph nodes and
//! the pair has no edge yet. Definitions map to nodes by file, name and line.
//! Edges carry `resolution: "scip"`.
use super::resolution::source_key;
use rustc_hash::{FxHashMap, FxHashSet};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// file key -> name -> [(line, node id)].
type ByName<'g> = FxHashMap<String, FxHashMap<&'g str, Vec<(usize, &'g str)>>>;
const DEFINITION: u64 = 0x1;
const IMPORT: u64 = 0x2;
/// A definition's name and its graph node may start a few lines apart.
const LINE_SLACK: usize = 3;

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or("")
}

/// Index files to read: `GRAPHICLE_SCIP` entries, then `<root>/index.scip`.
pub fn index_paths(root: &Path) -> Vec<PathBuf> {
    let mut out = std::env::var_os("GRAPHICLE_SCIP")
        .map(|v| {
            std::env::split_paths(&v)
                .filter(|p| !p.as_os_str().is_empty())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let default = root.join("index.scip");
    if default.is_file() && !out.contains(&default) {
        out.push(default);
    }
    out
}

/// Minimal protobuf reader for the SCIP messages used here.
struct Pb<'a> {
    buf: &'a [u8],
    pos: usize,
}
enum Field<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
    Other,
}
impl<'a> Pb<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }
    fn varint(&mut self) -> Option<u64> {
        let mut v = 0u64;
        for shift in (0..64).step_by(7) {
            let b = *self.buf.get(self.pos)?;
            self.pos += 1;
            if shift == 63 && b > 1 {
                return None;
            }
            v |= u64::from(b & 0x7f) << shift;
            if b < 0x80 {
                return Some(v);
            }
        }
        None
    }
    fn next(&mut self) -> Option<(u64, Field<'a>)> {
        if self.pos >= self.buf.len() {
            return None;
        }
        let key = self.varint()?;
        let (field, wire) = (key >> 3, key & 7);
        let value = match wire {
            0 => Field::Varint(self.varint()?),
            2 => {
                let n = usize::try_from(self.varint()?).ok()?;
                let end = self.pos.checked_add(n).filter(|e| *e <= self.buf.len())?;
                let bytes = &self.buf[self.pos..end];
                self.pos = end;
                Field::Bytes(bytes)
            }
            1 => {
                self.pos = self.pos.checked_add(8).filter(|e| *e <= self.buf.len())?;
                Field::Other
            }
            5 => {
                self.pos = self.pos.checked_add(4).filter(|e| *e <= self.buf.len())?;
                Field::Other
            }
            _ => return None,
        };
        Some((field, value))
    }
}

fn ints(field: &Field<'_>, out: &mut Vec<i64>) {
    match field {
        Field::Varint(v) => out.push(i64::from(*v as u32 as i32)),
        Field::Bytes(b) => {
            let mut pb = Pb::new(b);
            while let Some(v) = pb.varint() {
                out.push(i64::from(v as u32 as i32));
            }
        }
        Field::Other => {}
    }
}

fn typed_range(bytes: &[u8], multi: bool) -> Vec<i64> {
    let mut v = [0i64; 4];
    let mut pb = Pb::new(bytes);
    while let Some((f, value)) = pb.next() {
        if let (1..=4, Field::Varint(x)) = (f, value) {
            v[(f - 1) as usize] = i64::from(x as u32 as i32);
        }
    }
    if multi { v.to_vec() } else { v[..3].to_vec() }
}

/// `[start_line, start_char, end_line, end_char]` from SCIP's 3- or 4-int form.
fn span(r: &[i64]) -> Option<(usize, usize, usize, usize)> {
    let u = |v: i64| usize::try_from(v).ok();
    let range = match r {
        [l, a, b] => Some((u(*l)?, u(*a)?, u(*l)?, u(*b)?)),
        [l, a, m, b] => Some((u(*l)?, u(*a)?, u(*m)?, u(*b)?)),
        _ => None,
    }?;
    ((range.0, range.1) <= (range.2, range.3)).then_some(range)
}

struct Occurrence {
    range: (usize, usize, usize, usize),
    symbol: String,
    roles: u64,
    enclosing: Option<(usize, usize, usize, usize)>,
}

fn occurrence(bytes: &[u8]) -> Option<Occurrence> {
    let (mut range, mut enclosing) = (vec![], vec![]);
    let (mut symbol, mut roles) = (String::new(), 0);
    let mut pb = Pb::new(bytes);
    while let Some((f, value)) = pb.next() {
        match (f, &value) {
            (1, _) => ints(&value, &mut range),
            (8, Field::Bytes(b)) => range = typed_range(b, false),
            (9, Field::Bytes(b)) => range = typed_range(b, true),
            (2, Field::Bytes(b)) => symbol = String::from_utf8_lossy(b).into_owned(),
            (3, Field::Varint(v)) => roles = *v,
            (7, _) => ints(&value, &mut enclosing),
            (10, Field::Bytes(b)) => enclosing = typed_range(b, false),
            (11, Field::Bytes(b)) => enclosing = typed_range(b, true),
            _ => {}
        }
    }
    Some(Occurrence {
        range: span(&range)?,
        symbol,
        roles,
        enclosing: span(&enclosing),
    })
}

struct Document {
    path: String,
    utf16: bool,
    occurrences: Vec<Occurrence>,
}

/// Documents and the project root (a `file://` URI) of one index.
fn read_index(bytes: &[u8]) -> (Option<PathBuf>, Vec<Document>) {
    let mut project_root = None;
    let mut docs = vec![];
    let mut pb = Pb::new(bytes);
    while let Some((f, value)) = pb.next() {
        match (f, value) {
            (1, Field::Bytes(meta)) => {
                let mut m = Pb::new(meta);
                while let Some((f, value)) = m.next() {
                    if let (3, Field::Bytes(uri)) = (f, value) {
                        let uri = String::from_utf8_lossy(uri);
                        project_root = uri
                            .strip_prefix("file://")
                            .map(|p| PathBuf::from(percent_decode(p)));
                    }
                }
            }
            (2, Field::Bytes(doc)) => {
                let mut d = Document {
                    path: String::new(),
                    // SCIP's default position encoding is ambiguous; the
                    // TypeScript and Python indexers count UTF-16 units.
                    utf16: true,
                    occurrences: vec![],
                };
                let mut p = Pb::new(doc);
                while let Some((f, value)) = p.next() {
                    match (f, value) {
                        (1, Field::Bytes(b)) => d.path = String::from_utf8_lossy(b).into_owned(),
                        (2, Field::Bytes(b)) => d.occurrences.extend(occurrence(b)),
                        (6, Field::Varint(1)) => d.utf16 = false,
                        (6, Field::Varint(3)) => d.utf16 = false,
                        _ => {}
                    }
                }
                docs.push(d);
            }
            _ => {}
        }
    }
    (project_root, docs)
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(v) =
                u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or(""), 16)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The name and kind suffix of a SCIP symbol's last descriptor: `().` for
/// methods and functions, `#` for types, `.` for terms. Local symbols have none.
fn last_descriptor(symbol: &str) -> Option<(String, &'static str)> {
    if symbol.starts_with("local ") {
        return None;
    }
    // Scheme, manager, package name and version precede the descriptors;
    // spaces inside them are escaped as double spaces.
    let mut rest = symbol;
    for _ in 0..4 {
        let mut i = 0;
        let b = rest.as_bytes();
        loop {
            if i >= b.len() {
                return None;
            }
            if b[i] == b' ' {
                if b.get(i + 1) == Some(&b' ') {
                    i += 2;
                    continue;
                }
                break;
            }
            i += 1;
        }
        rest = &rest[i + 1..];
    }
    let mut descriptors = vec![];
    let mut name = String::new();
    let mut chars = rest.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '`' => {
                while let Some(c) = chars.next() {
                    if c == '`' {
                        if chars.peek() == Some(&'`') {
                            chars.next();
                            name.push('`');
                            continue;
                        }
                        break;
                    }
                    name.push(c);
                }
            }
            '/' => descriptors.push((std::mem::take(&mut name), "/")),
            '#' => descriptors.push((std::mem::take(&mut name), "#")),
            '.' => descriptors.push((std::mem::take(&mut name), ".")),
            ':' => descriptors.push((std::mem::take(&mut name), ":")),
            '!' => descriptors.push((std::mem::take(&mut name), "!")),
            '(' => {
                for c in chars.by_ref() {
                    if c == ')' {
                        break;
                    }
                }
                if chars.peek() == Some(&'.') {
                    chars.next();
                }
                descriptors.push((std::mem::take(&mut name), "()."));
            }
            '[' => {
                for c in chars.by_ref() {
                    if c == ']' {
                        break;
                    }
                }
                name.clear();
            }
            c => name.push(c),
        }
    }
    descriptors.pop().filter(|(n, _)| !n.is_empty())
}

/// Byte offset of `units` UTF-16 code units (or bytes) into `line`.
fn byte_offset(line: &str, units: usize, utf16: bool) -> usize {
    if !utf16 {
        return units.min(line.len());
    }
    let mut count = 0;
    for (i, c) in line.char_indices() {
        if count >= units {
            return i;
        }
        count += c.len_utf16();
    }
    line.len()
}

/// Whether the source after the reference shows a call: `(`, or `<...>(`,
/// with type arguments that may run onto the `following` lines.
fn is_call(line: &str, end: usize, following: &[String]) -> bool {
    let rest = line.get(end..).unwrap_or("").trim_start();
    if rest.starts_with('(') {
        return true;
    }
    let Some(generic) = rest.strip_prefix('<') else {
        return false;
    };
    let texts = std::iter::once(generic).chain(following.iter().take(40).map(String::as_str));
    let texts = texts.collect::<Vec<_>>();
    let mut depth = 1;
    for (k, text) in texts.iter().enumerate() {
        for (i, c) in text.char_indices() {
            match c {
                '<' => depth += 1,
                '>' => {
                    depth -= 1;
                    if depth == 0 {
                        let tail = text[i + 1..].trim_start();
                        if !tail.is_empty() {
                            return tail.starts_with('(');
                        }
                        // `>` ends the line: the `(` may open the next one.
                        return texts[k + 1..]
                            .iter()
                            .map(|t| t.trim_start())
                            .find(|t| !t.is_empty())
                            .is_some_and(|t| t.starts_with('('));
                    }
                }
                ';' | '{' | '}' => return false,
                _ => {}
            }
        }
    }
    false
}

/// Graph nodes by file and bare name (`name()`, `.name()` and `Name` alike).
struct Index<'g> {
    by_name: ByName<'g>,
}
impl<'g> Index<'g> {
    fn build(nodes: &[&'g Value], root: &Path) -> Self {
        let mut by_name: ByName<'g> = FxHashMap::default();
        let mut keys: FxHashMap<&str, String> = FxHashMap::default();
        for n in nodes {
            let (id, label, file) = (s(n, "id"), s(n, "label"), s(n, "source_file"));
            let Some(line) = s(n, "source_location")
                .strip_prefix('L')
                .and_then(|l| l.parse::<usize>().ok())
            else {
                continue;
            };
            if id.is_empty() || file.is_empty() || label.is_empty() {
                continue;
            }
            let bare = label.strip_prefix('.').unwrap_or(label);
            let bare = bare.strip_suffix("()").unwrap_or(bare);
            let key = keys
                .entry(file)
                .or_insert_with(|| source_key(file, root))
                .clone();
            by_name
                .entry(key)
                .or_default()
                .entry(bare)
                .or_default()
                .push((line, id));
        }
        Self { by_name }
    }
    fn node(&self, file: &str, name: &str, line: usize) -> Option<&'g str> {
        let candidates = self.by_name.get(file)?.get(name)?;
        let distance = candidates.iter().map(|(l, _)| l.abs_diff(line)).min()?;
        if distance > LINE_SLACK {
            return None;
        }
        let mut closest = candidates
            .iter()
            .filter(|(l, _)| l.abs_diff(line) == distance);
        let (_, target) = closest.next()?;
        closest.all(|(_, id)| id == target).then_some(*target)
    }
}

fn edge(src: &str, tgt: &str, line: usize, file: &str) -> Value {
    json!({"source": src, "target": tgt, "relation": "calls", "context": "call",
        "confidence": "EXTRACTED", "confidence_score": 1.0, "source_file": file,
        "source_location": format!("L{line}"), "weight": 1.0, "_origin": "ast",
        "resolution": "scip"})
}

/// Add `calls` edges from the SCIP indexes of `root` (see the module docs).
/// Only callers in files among `paths` get edges. Returns every (caller,
/// callee) pair an index shows, including pairs the graph already had, so
/// their evidence can be raised (`evidence::annotate`).
pub fn resolve(
    paths: &[PathBuf],
    nodes: &[Value],
    edges: &mut Vec<Value>,
    context_nodes: &[Value],
    context_edges: &[Value],
    root: &Path,
) -> FxHashSet<(String, String)> {
    let mut confirmed = FxHashSet::default();
    let files = index_paths(root);
    if files.is_empty() {
        return confirmed;
    }
    let started = std::time::Instant::now();
    let mut indexes = vec![];
    for path in &files {
        match std::fs::read(path) {
            Ok(bytes) => indexes.push(bytes),
            Err(_) => log::warn!("SCIP index not readable: {}", path.display()),
        }
    }
    if indexes.is_empty() {
        return confirmed;
    }
    let emit = paths
        .iter()
        .map(|p| source_key(&p.to_string_lossy(), root))
        .collect::<FxHashSet<_>>();
    let all_nodes = nodes.iter().chain(context_nodes).collect::<Vec<_>>();
    let index = Index::build(&all_nodes, root);
    let mut pairs: FxHashSet<(String, String)> = FxHashSet::default();
    for e in edges.iter().chain(context_edges) {
        let (a, b) = (s(e, "source"), s(e, "target"));
        let (a, b) = if a <= b { (a, b) } else { (b, a) };
        pairs.insert((a.to_string(), b.to_string()));
    }
    let mut added = vec![];
    let mut sources: FxHashMap<String, Option<Vec<String>>> = FxHashMap::default();
    for bytes in &indexes {
        let (project_root, docs) = read_index(bytes);
        let base = project_root.unwrap_or_else(|| root.to_path_buf());
        let file_key = |rel: &str| source_key(&base.join(rel).to_string_lossy(), root);
        // Symbol definitions: global symbols by name, locals per document.
        let mut defs: FxHashMap<(Option<usize>, String), (usize, usize)> = FxHashMap::default();
        for (di, doc) in docs.iter().enumerate() {
            for occ in &doc.occurrences {
                if occ.roles & DEFINITION != 0 && !occ.symbol.is_empty() {
                    let scope = occ.symbol.starts_with("local ").then_some(di);
                    defs.entry((scope, occ.symbol.clone()))
                        .or_insert((di, occ.range.0));
                    // scip-python names the definitions of a `src/` layout
                    // `src.pkg.mod` but references to them `pkg.mod`.
                    if doc.path.starts_with("src/") && occ.symbol.contains("`src.") {
                        let alias = occ.symbol.replacen("`src.", "`", 1);
                        defs.entry((scope, alias)).or_insert((di, occ.range.0));
                    }
                }
            }
        }
        let keys = docs.iter().map(|d| file_key(&d.path)).collect::<Vec<_>>();
        for (di, doc) in docs.iter().enumerate() {
            if !emit.contains(&keys[di]) {
                continue;
            }
            // Callable definitions with extents: the candidate callers, with
            // the line of their name (decorator lines before it run in the
            // enclosing scope, not in the decorated function).
            let mut callers = doc
                .occurrences
                .iter()
                .filter(|o| o.roles & DEFINITION != 0)
                .filter_map(|o| {
                    let (name, kind) = last_descriptor(&o.symbol)?;
                    (kind == "().").then_some((o.enclosing?, name, o.range.0))
                })
                .collect::<Vec<_>>();
            if callers.is_empty() {
                continue;
            }
            callers
                .sort_by_key(|((sl, sc, el, ec), _, _)| (*sl, *sc, std::cmp::Reverse((*el, *ec))));
            let lines = sources.entry(keys[di].clone()).or_insert_with(|| {
                std::fs::read_to_string(base.join(&doc.path))
                    .ok()
                    .map(|t| t.lines().map(str::to_string).collect())
            });
            let Some(lines) = lines.as_ref() else {
                continue;
            };
            for occ in &doc.occurrences {
                if occ.roles & (DEFINITION | IMPORT) != 0 || occ.symbol.is_empty() {
                    continue;
                }
                let (line, start_char, end_line, end_char) = occ.range;
                let Some(text) = lines.get(end_line) else {
                    continue;
                };
                if !is_call(
                    text,
                    byte_offset(text, end_char, doc.utf16),
                    lines.get(end_line + 1..).unwrap_or_default(),
                ) {
                    continue;
                }
                let Some((target_name, _)) = last_descriptor(&occ.symbol) else {
                    continue;
                };
                // The identifier written at the reference must be the target's
                // name in Python: scip-python 0.6.6 gives every name of a
                // multi-name re-export (`from ._client import AsyncClient,
                // Client`) the first name's symbol when read as `pkg.Client`.
                // In other languages a different identifier is an import
                // alias (`import { f as g }`, `use f as g`): a call of f.
                let python = doc.path.ends_with(".py") || doc.path.ends_with(".pyi");
                if line != end_line
                    || (python
                        && text.get(
                            byte_offset(text, start_char, doc.utf16)
                                ..byte_offset(text, end_char, doc.utf16),
                        ) != Some(target_name.as_str()))
                {
                    continue;
                }
                // Innermost callable whose body contains the reference.
                let pos = (line, occ.range.1);
                let Some((_, caller_name, caller_line)) = callers
                    .iter()
                    .filter(|((sl, sc, el, ec), _, name_line)| {
                        (*sl, *sc) <= pos && pos <= (*el, *ec) && line >= *name_line
                    })
                    .min_by_key(|((sl, sc, el, ec), _, _)| (el - sl, ec.abs_diff(*sc)))
                else {
                    continue;
                };
                let scope = occ.symbol.starts_with("local ").then_some(di);
                let Some(&(target_doc, target_line)) = defs.get(&(scope, occ.symbol.clone()))
                else {
                    continue;
                };
                let (Some(src), Some(tgt)) = (
                    index.node(&keys[di], caller_name, caller_line + 1),
                    index.node(&keys[target_doc], &target_name, target_line + 1),
                ) else {
                    continue;
                };
                if src == tgt {
                    continue;
                }
                confirmed.insert((src.to_string(), tgt.to_string()));
                let pair = if src <= tgt { (src, tgt) } else { (tgt, src) };
                if !pairs.insert((pair.0.to_string(), pair.1.to_string())) {
                    continue;
                }
                added.push(edge(src, tgt, line + 1, &keys[di]));
            }
        }
    }
    if std::env::var("GRAPHICLE_PRECISE_TIMING").is_ok_and(|v| v == "1") {
        eprintln!(
            "[precise scip] indexes={} total={:.0}ms added={}",
            indexes.len(),
            started.elapsed().as_secs_f64() * 1e3,
            added.len()
        );
    }
    edges.extend(added);
    confirmed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptors_and_calls() {
        assert_eq!(
            last_descriptor("scip-python python httpx 0 `httpx._client`/Client#send()."),
            Some(("send".into(), "()."))
        );
        assert_eq!(
            last_descriptor("scip-typescript npm pkg 1.0 src/`a.ts`/Shape#area()."),
            Some(("area".into(), "()."))
        );
        assert_eq!(
            last_descriptor("rust-analyzer cargo my  crate 0.1.0 mod/Type#"),
            Some(("Type".into(), "#"))
        );
        assert_eq!(last_descriptor("local 12"), None);
        assert!(is_call("  foo(1)", 5, &[]));
        assert!(is_call("x = make<Foo<Bar>>(a)", 8, &[]));
        assert!(!is_call("x = foo", 7, &[]));
        // Type arguments over several lines, `(` on the line after `>`.
        let following = ["  A,".to_string(), "  B".to_string(), ">(".to_string()];
        assert!(is_call("const x = register<", 18, &following));
        let following = ["  A".to_string(), ">".to_string(), "  (1)".to_string()];
        assert!(is_call("make<", 4, &following));
        assert!(!is_call(
            "type T = Map<",
            12,
            &["  A,".to_string(), "  B>;".to_string()]
        ));
        assert_eq!(byte_offset("é(x)", 1, true), 2);
    }

    #[test]
    fn malformed_ranges_and_ambiguous_node_matches_are_not_evidence() {
        assert!(span(&[7, 0, 3, 1]).is_none());
        assert!(span(&[3, 9, 4]).is_none());
        assert!(span(&[-1, 0, 1]).is_none());
        let index = Index {
            by_name: FxHashMap::from_iter([(
                "file.py".into(),
                FxHashMap::from_iter([("run", vec![(4, "first"), (6, "second")])]),
            )]),
        };
        assert_eq!(index.node("file.py", "run", 5), None);
        assert_eq!(index.node("file.py", "run", 6), Some("second"));
        let mut bytes = [0x80; 10];
        bytes[9] = 2;
        assert!(Pb::new(&bytes).varint().is_none());
    }

    #[test]
    fn protobuf_occurrence_ranges() {
        // Occurrence: range [3, 4, 9] packed, symbol "s", roles 1, typed
        // multi-line enclosing range 3:0-7:1.
        let mut occ = vec![0x0a, 3, 3, 4, 9, 0x12, 1, b's', 0x18, 1, 0x5a, 8];
        occ.extend([0x08, 3, 0x10, 0, 0x18, 7, 0x20, 1]);
        let o = occurrence(&occ).unwrap();
        assert_eq!(o.range, (3, 4, 3, 9));
        assert_eq!(o.symbol, "s");
        assert_eq!(o.roles, 1);
        assert_eq!(o.enclosing, Some((3, 0, 7, 1)));
    }
}
