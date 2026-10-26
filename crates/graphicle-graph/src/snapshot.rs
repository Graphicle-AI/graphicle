//! Saved query snapshots: a loaded graph and its search index, fully built,
//! stored next to graph.json.
//!
//! `query`, `explain`, `path` and the MCP server spend nearly all of their
//! time parsing graph.json, building the graph and building the search index,
//! not answering. The first load writes the built structures to
//! `<graph dir>/cache/query/<graph file>.<variant>.snap`; later loads read that
//! file instead while graph.json keeps the same size, modification/change time
//! and file identity, and the binary was built from the same source. Any
//! mismatch, decode error or I/O error falls back to graph.json and rewrites
//! the snapshot, so answers are always those of graph.json. Inputs other than
//! graph.json (the server's learning overlay) are applied after loading, never
//! stored. Snapshots are only written when `<graph dir>/cache` already exists
//! (every extract creates it). `GRAPHICLE_QUERY_SNAPSHOT=0` disables both
//! reading and writing. A `--min-evidence` load has snapshots of its own
//! (`<variant>-e<level>`): the filtered graph is a different built graph.
//!
//! The format is private to one build: every snapshot records a hash of the
//! crate source it was written by and is ignored by any other build. Records
//! are stored in fixed-size chunks that decode in parallel; strings are stored
//! once and referenced by number.
use crate::{Result, graph::Graph, record::Field, serve_search::SearchIndex};
use serde_json::{Map, Number, Value};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicUsize, Ordering},
    time::Instant,
};

const MAGIC: &[u8; 8] = b"GFSNAP\0\x02";
/// Hash of the crate source (build.rs); a snapshot from another build is stale.
const CODE: &str = env!("GRAPHICLE_SNAPSHOT_CODE");
const MAX_DEPTH: usize = 256;
/// Records per chunk; each chunk decodes independently.
pub(crate) const CHUNK: usize = 2048;

/// How the graph was loaded; each variant is a different built graph.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Variant {
    /// `query`: graph.json as stored.
    Query,
    /// `explain`, `affected`, `god-nodes`: loaded as a directed graph.
    Explain,
    /// `path`: loaded as a directed multigraph.
    Path,
    /// The MCP server's loader (directed; the learning overlay is applied
    /// after loading).
    Serve,
}
impl Variant {
    fn tag(self) -> &'static str {
        match self {
            Self::Query => "query",
            Self::Explain => "explain",
            Self::Path => "path",
            Self::Serve => "serve",
        }
    }
    /// The variant's name in snapshot files, with the `--min-evidence` level
    /// the graph was filtered at, if any.
    fn name(self) -> String {
        match crate::extractors::evidence::min_level() {
            0 => self.tag().to_string(),
            level => format!("{}-e{level}", self.tag()),
        }
    }
}

/// The hash of the engine source this build was made from, which every
/// snapshot it writes carries: a host that stores data derived from the
/// engine's graphs can key it the same way.
pub fn build_code() -> &'static str {
    CODE
}

pub fn enabled() -> bool {
    !std::env::var("GRAPHICLE_QUERY_SNAPSHOT").is_ok_and(|v| {
        matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "no" | "off"
        )
    })
}

pub(crate) fn timing() -> bool {
    std::env::var_os("GRAPHICLE_QUERY_TIMING").is_some()
}

/// `<graph dir>/cache/query/<file>.<variant>.snap`, when the cache dir exists.
pub fn path_for(graph: &Path, variant: Variant) -> Option<PathBuf> {
    let cache = graph.parent()?.join("cache");
    if !cache.is_dir() {
        return None;
    }
    let name = graph.file_name()?.to_string_lossy();
    Some(
        cache
            .join("query")
            .join(format!("{name}.{}.snap", variant.name())),
    )
}

/// Size, modification/change time and file identity of an input file.
fn stamp(path: &Path, w: &mut Vec<u8>) -> std::io::Result<()> {
    let meta = fs::metadata(path)?;
    put_uvar(w, meta.len());
    let nanos = |t: std::io::Result<std::time::SystemTime>| {
        t.ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_nanos() as u64)
    };
    put_uvar(w, nanos(meta.modified()));
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        put_uvar(w, meta.dev());
        put_uvar(w, meta.ino());
        put_uvar(w, meta.ctime() as u64);
        put_uvar(w, meta.ctime_nsec() as u64);
    }
    #[cfg(not(unix))]
    put_uvar(w, nanos(meta.created()));
    Ok(())
}

/// The key a snapshot must carry to be current for this graph file.
fn header(graph: &Path, variant: Variant) -> std::io::Result<Vec<u8>> {
    let mut w = Vec::with_capacity(96);
    put_bytes(&mut w, CODE.as_bytes());
    put_bytes(&mut w, variant.name().as_bytes());
    stamp(graph, &mut w)?;
    Ok(w)
}

/// Loads through the snapshot when it is current, otherwise runs `build` and
/// saves its result. `build` must read nothing but `graph`.
pub(crate) fn load(
    graph: &Path,
    variant: Variant,
    build: impl FnOnce() -> Result<(Graph, SearchIndex)>,
) -> Result<(Graph, SearchIndex)> {
    let started = Instant::now();
    let Some(snap) = enabled().then(|| path_for(graph, variant)).flatten() else {
        return build();
    };
    let Ok(key) = header(graph, variant) else {
        return build();
    };
    if let Ok(bytes) = fs::read(&snap)
        && let Some(body) = bytes
            .strip_prefix(MAGIC.as_slice())
            .and_then(|rest| rest.strip_prefix(key.as_slice()))
    {
        match decode(body) {
            Ok(parts) => {
                if timing() {
                    eprintln!(
                        "[graphicle timing] snapshot load {:.1} ms ({} bytes)",
                        started.elapsed().as_secs_f64() * 1e3,
                        bytes.len()
                    );
                }
                return Ok(parts);
            }
            Err(e) if timing() => eprintln!("[graphicle timing] snapshot ignored: {e}"),
            Err(_) => {}
        }
    }
    let (graph_value, index) = build()?;
    let built = started.elapsed();
    // Only save what was built from the file the key describes.
    if header(graph, variant).is_ok_and(|now| now == key) {
        let saved = save(&snap, &key, &graph_value, &index);
        if timing() {
            eprintln!(
                "[graphicle timing] build {:.1} ms, snapshot save {:.1} ms{}",
                built.as_secs_f64() * 1e3,
                (started.elapsed() - built).as_secs_f64() * 1e3,
                saved
                    .err()
                    .map(|e| format!(" (failed: {e})"))
                    .unwrap_or_default()
            );
        }
    }
    Ok((graph_value, index))
}

fn save(path: &Path, key: &[u8], graph: &Graph, index: &SearchIndex) -> std::io::Result<()> {
    let bytes = encode(key, graph, index);
    let dir = path
        .parent()
        .ok_or_else(|| std::io::Error::other("snapshot path has no parent"))?;
    fs::create_dir_all(dir)?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(&bytes)?;
    tmp.flush()?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

/// The complete snapshot file for `graph` and its `index`.
pub(crate) fn encode(key: &[u8], graph: &Graph, index: &SearchIndex) -> Vec<u8> {
    let mut w = Writer::default();
    graph.write_snapshot(&mut w);
    index.write_snapshot(&mut w);
    w.end_chunk();
    let chunks = w.chunks.iter().map(Vec::len).sum::<usize>();
    let mut out = Vec::with_capacity(MAGIC.len() + key.len() + w.bytes + chunks + 16);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(key);
    for table in [&w.strings, &w.keys] {
        put_uvar(&mut out, table.len() as u64);
        for s in table.iter() {
            put_uvar(&mut out, s.len() as u64);
        }
        for s in table.iter() {
            out.extend_from_slice(s.as_bytes());
        }
    }
    put_uvar(&mut out, w.chunks.len() as u64);
    for chunk in &w.chunks {
        put_uvar(&mut out, chunk.len() as u64);
    }
    for chunk in &w.chunks {
        out.extend_from_slice(chunk);
    }
    out
}

/// A snapshot of `graph` and `index` that its caller keys and stores itself
/// (a host's projected graphs): [`from_bytes`] reads it back with the same
/// `key`, in this build only.
pub fn to_bytes(key: &[u8], graph: &Graph, index: &SearchIndex) -> Vec<u8> {
    encode(&caller_key(key), graph, index)
}

/// The graph and index of a [`to_bytes`] snapshot written with `key` by this
/// build; `None` when it carries another key or was written by another build.
pub fn from_bytes(bytes: &[u8], key: &[u8]) -> Option<Result<(Graph, SearchIndex)>> {
    let key = caller_key(key);
    let body = bytes
        .strip_prefix(MAGIC.as_slice())?
        .strip_prefix(key.as_slice())?;
    Some(decode(body))
}

fn caller_key(key: &[u8]) -> Vec<u8> {
    let mut w = Vec::with_capacity(key.len() + 80);
    put_bytes(&mut w, CODE.as_bytes());
    put_bytes(&mut w, b"caller");
    put_bytes(&mut w, key);
    w
}

/// Graph and index from a snapshot body (the bytes after the key).
pub(crate) fn decode(body: &[u8]) -> Result<(Graph, SearchIndex)> {
    let mut d = Decoder::new(body)?;
    let graph = Graph::read_snapshot(&mut d)?;
    let index = SearchIndex::read_snapshot(&mut d, graph.node_count())?;
    if d.next != d.chunks.len() {
        return Err(corrupt("trailing chunks"));
    }
    Ok((graph, index))
}

pub(crate) fn corrupt(what: &str) -> crate::Error {
    format!("corrupt query snapshot: {what}").into()
}

fn put_uvar(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    put_uvar(out, bytes.len() as u64);
    out.extend_from_slice(bytes);
}

type Interner<'a> = hashbrown::HashMap<&'a str, u32, crate::graph::FastState>;

fn intern<'a>(ids: &mut Interner<'a>, table: &mut Vec<&'a str>, s: &'a str) -> u32 {
    let next = table.len() as u32;
    *ids.entry(s).or_insert_with(|| {
        table.push(s);
        next
    })
}

/// Snapshot encoder: a sequence of chunks sharing one string table and one
/// attribute-name table.
#[derive(Default)]
pub(crate) struct Writer<'a> {
    body: Vec<u8>,
    chunks: Vec<Vec<u8>>,
    ids: Interner<'a>,
    strings: Vec<&'a str>,
    key_ids: Interner<'a>,
    keys: Vec<&'a str>,
    bytes: usize,
}
impl<'a> Writer<'a> {
    /// Closes the current chunk; the reader sees chunks in the same order.
    pub(crate) fn end_chunk(&mut self) {
        let chunk = std::mem::take(&mut self.body);
        self.chunks.push(chunk);
    }
    pub(crate) fn u8(&mut self, v: u8) {
        self.body.push(v);
    }
    pub(crate) fn uvar(&mut self, v: u64) {
        put_uvar(&mut self.body, v);
    }
    pub(crate) fn usize(&mut self, v: usize) {
        self.uvar(v as u64);
    }
    pub(crate) fn str(&mut self, s: &'a str) {
        let before = self.strings.len();
        let id = intern(&mut self.ids, &mut self.strings, s);
        if self.strings.len() > before {
            self.bytes += s.len() + 2;
        }
        self.uvar(u64::from(id));
    }
    /// An attribute name, from the (small) name table.
    pub(crate) fn key(&mut self, s: &'a str) {
        let id = intern(&mut self.key_ids, &mut self.keys, s);
        self.uvar(u64::from(id));
    }
    /// Ascending lists as deltas, anything else verbatim.
    pub(crate) fn indices(&mut self, list: &[usize]) {
        self.usize(list.len());
        if list.windows(2).all(|w| w[0] <= w[1]) {
            self.u8(0);
            let mut previous = 0;
            for &i in list {
                self.usize(i - previous);
                previous = i;
            }
        } else {
            self.u8(1);
            for &i in list {
                self.usize(i);
            }
        }
    }
    pub(crate) fn value(&mut self, v: &'a Value) {
        match v {
            Value::Null => self.u8(0),
            Value::Bool(false) => self.u8(1),
            Value::Bool(true) => self.u8(2),
            Value::Number(n) => {
                self.u8(3);
                self.str(n.as_str());
            }
            Value::String(s) => {
                self.u8(4);
                self.str(s);
            }
            Value::Array(items) => {
                self.u8(5);
                self.usize(items.len());
                for item in items {
                    self.value(item);
                }
            }
            Value::Object(map) => {
                self.u8(6);
                self.usize(map.len());
                for (k, item) in map {
                    self.str(k);
                    self.value(item);
                }
            }
        }
    }
}

fn read_table<'a>(r: &mut Reader<'_, 'a>) -> Result<Vec<&'a str>> {
    let count = r.len(1)?;
    let mut lengths = Vec::with_capacity(count);
    let mut total = 0usize;
    for _ in 0..count {
        let n = r.usize()?;
        total = total
            .checked_add(n)
            .ok_or_else(|| corrupt("string table"))?;
        lengths.push(n);
    }
    let blob = r.take(total)?;
    let blob = std::str::from_utf8(blob).map_err(|_| corrupt("string table UTF-8"))?;
    let mut strings = Vec::with_capacity(count);
    let mut start = 0;
    for n in lengths {
        strings.push(
            blob.get(start..start + n)
                .ok_or_else(|| corrupt("string boundary"))?,
        );
        start += n;
    }
    Ok(strings)
}

/// The chunks of a snapshot body and the tables they reference.
pub(crate) struct Decoder<'a> {
    chunks: Vec<&'a [u8]>,
    next: usize,
    strings: Vec<&'a str>,
    keys: Vec<Field>,
}
impl<'a> Decoder<'a> {
    fn new(body: &'a [u8]) -> Result<Self> {
        let mut r = Reader {
            buf: body,
            pos: 0,
            strings: &[],
            keys: &[],
        };
        let strings = read_table(&mut r)?;
        let keys = read_table(&mut r)?
            .into_iter()
            .map(Field::borrowed)
            .collect();
        let count = r.len(1)?;
        let mut lengths = Vec::with_capacity(count);
        for _ in 0..count {
            lengths.push(r.usize()?);
        }
        let mut chunks = Vec::with_capacity(count);
        for n in lengths {
            chunks.push(r.take(n)?);
        }
        if r.pos != body.len() {
            return Err(corrupt("trailing bytes"));
        }
        Ok(Self {
            chunks,
            next: 0,
            strings,
            keys,
        })
    }
    /// Decodes the next chunk with `f`, which must consume all of it.
    pub(crate) fn chunk<T>(
        &mut self,
        f: impl FnOnce(&mut Reader<'_, 'a>) -> Result<T>,
    ) -> Result<T> {
        let buf = *self
            .chunks
            .get(self.next)
            .ok_or_else(|| corrupt("missing chunk"))?;
        self.next += 1;
        let mut r = Reader {
            buf,
            pos: 0,
            strings: &self.strings,
            keys: &self.keys,
        };
        let out = f(&mut r)?;
        if r.pos != buf.len() {
            return Err(corrupt("chunk length"));
        }
        Ok(out)
    }
    /// Decodes the next `count` chunks with `f` on all cores; results in order.
    pub(crate) fn chunks<T: Send>(
        &mut self,
        count: usize,
        f: impl Fn(&mut Reader<'_, 'a>, usize) -> Result<T> + Sync,
    ) -> Result<Vec<T>> {
        let start = self.next;
        let end = start
            .checked_add(count)
            .filter(|end| *end <= self.chunks.len())
            .ok_or_else(|| corrupt("missing chunk"))?;
        self.next = end;
        let (chunks, strings, keys) = (&self.chunks[start..end], &self.strings, &self.keys);
        let decode = |i: usize| -> Result<T> {
            let mut r = Reader {
                buf: chunks[i],
                pos: 0,
                strings,
                keys,
            };
            let out = f(&mut r, i)?;
            if r.pos != chunks[i].len() {
                return Err(corrupt("chunk length"));
            }
            Ok(out)
        };
        let workers = std::thread::available_parallelism()
            .map_or(1, usize::from)
            .min(count);
        if workers <= 1 {
            return (0..count).map(decode).collect();
        }
        let next = AtomicUsize::new(0);
        let mut slots = std::thread::scope(|scope| {
            let handles = (0..workers)
                .map(|_| {
                    scope.spawn(|| {
                        let mut done = vec![];
                        loop {
                            let i = next.fetch_add(1, Ordering::Relaxed);
                            if i >= count {
                                break done;
                            }
                            done.push((i, decode(i)));
                        }
                    })
                })
                .collect::<Vec<_>>();
            handles
                .into_iter()
                .flat_map(|h| h.join().expect("snapshot decoder panicked"))
                .collect::<Vec<_>>()
        });
        slots.sort_unstable_by_key(|(i, _)| *i);
        slots.into_iter().map(|(_, r)| r).collect()
    }
}

/// Decoder for one chunk, with bounds checks on every read: malformed input
/// is an error, never a panic or an unbounded allocation.
pub(crate) struct Reader<'s, 'a> {
    buf: &'a [u8],
    pos: usize,
    strings: &'s [&'a str],
    keys: &'s [Field],
}
impl<'a> Reader<'_, 'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self
            .pos
            .checked_add(n)
            .filter(|end| *end <= self.buf.len())
            .ok_or_else(|| corrupt("truncated"))?;
        let out = &self.buf[self.pos..end];
        self.pos = end;
        Ok(out)
    }
    pub(crate) fn u8(&mut self) -> Result<u8> {
        let b = *self.buf.get(self.pos).ok_or_else(|| corrupt("truncated"))?;
        self.pos += 1;
        Ok(b)
    }
    pub(crate) fn uvar(&mut self) -> Result<u64> {
        let mut v = 0u64;
        for shift in (0..64).step_by(7) {
            let b = self.u8()?;
            if shift == 63 && b > 1 {
                return Err(corrupt("varint overflow"));
            }
            v |= u64::from(b & 0x7f) << shift;
            if b < 0x80 {
                return Ok(v);
            }
        }
        Err(corrupt("varint"))
    }
    pub(crate) fn usize(&mut self) -> Result<usize> {
        usize::try_from(self.uvar()?).map_err(|_| corrupt("size"))
    }
    /// A count of items that each take at least `min` bytes.
    pub(crate) fn len(&mut self, min: usize) -> Result<usize> {
        let n = self.usize()?;
        if n.saturating_mul(min.max(1)) > self.buf.len() - self.pos {
            return Err(corrupt("count exceeds data"));
        }
        Ok(n)
    }
    /// An index below `bound`.
    pub(crate) fn index(&mut self, bound: usize) -> Result<usize> {
        let i = self.usize()?;
        if i >= bound {
            return Err(corrupt("index out of range"));
        }
        Ok(i)
    }
    pub(crate) fn str(&mut self) -> Result<&'a str> {
        let id = self.index(self.strings.len())?;
        Ok(self.strings[id])
    }
    pub(crate) fn string(&mut self) -> Result<String> {
        self.str().map(str::to_owned)
    }
    pub(crate) fn key(&mut self) -> Result<Field> {
        let id = self.index(self.keys.len())?;
        Ok(self.keys[id].clone())
    }
    pub(crate) fn indices(&mut self, bound: usize) -> Result<Vec<usize>> {
        let n = self.len(1)?;
        let delta = match self.u8()? {
            0 => true,
            1 => false,
            _ => return Err(corrupt("index list")),
        };
        let mut out = Vec::with_capacity(n);
        let mut previous = 0usize;
        for _ in 0..n {
            let v = self.usize()?;
            let i = if delta {
                previous
                    .checked_add(v)
                    .ok_or_else(|| corrupt("index list"))?
            } else {
                v
            };
            if i >= bound {
                return Err(corrupt("index out of range"));
            }
            out.push(i);
            previous = i;
        }
        Ok(out)
    }
    pub(crate) fn value(&mut self) -> Result<Value> {
        self.value_at(0)
    }
    fn value_at(&mut self, depth: usize) -> Result<Value> {
        if depth > MAX_DEPTH {
            return Err(corrupt("nesting"));
        }
        Ok(match self.u8()? {
            0 => Value::Null,
            1 => Value::Bool(false),
            2 => Value::Bool(true),
            3 => Value::Number(
                self.str()?
                    .parse::<Number>()
                    .map_err(|_| corrupt("number"))?,
            ),
            4 => Value::String(self.string()?),
            5 => {
                let n = self.len(1)?;
                let mut items = Vec::with_capacity(n);
                for _ in 0..n {
                    items.push(self.value_at(depth + 1)?);
                }
                Value::Array(items)
            }
            6 => {
                let n = self.len(2)?;
                let mut map = Map::with_capacity(n);
                for _ in 0..n {
                    let k = self.string()?;
                    let v = self.value_at(depth + 1)?;
                    map.insert(k, v);
                }
                Value::Object(map)
            }
            _ => return Err(corrupt("value tag")),
        })
    }
}

/// Number of `CHUNK`-sized chunks holding `n` records.
pub(crate) fn chunk_count(n: usize) -> usize {
    n.div_ceil(CHUNK)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn encoded(v: &Value) -> Vec<u8> {
        let mut w = Writer::default();
        w.value(v);
        w.end_chunk();
        let mut out = vec![];
        for table in [&w.strings, &w.keys] {
            put_uvar(&mut out, table.len() as u64);
            for s in table.iter() {
                put_uvar(&mut out, s.len() as u64);
            }
            for s in table.iter() {
                out.extend_from_slice(s.as_bytes());
            }
        }
        put_uvar(&mut out, w.chunks.len() as u64);
        for chunk in &w.chunks {
            put_uvar(&mut out, chunk.len() as u64);
        }
        for chunk in &w.chunks {
            out.extend_from_slice(chunk);
        }
        out
    }

    fn decoded(bytes: &[u8]) -> Result<Value> {
        Decoder::new(bytes)?.chunk(|r| r.value())
    }

    #[test]
    fn values_roundtrip_with_exact_number_text_and_key_order() {
        let text = r#"{"z":1.0,"a":[null,true,false,-0,1e5,12345678901234567890123,0.1],
            "é":{"b":"x","a":"\u0000y"},"n":"ünïcødé","e":{},"l":[]}"#;
        let v: Value = serde_json::from_str(text).unwrap();
        let back = decoded(&encoded(&v)).unwrap();
        assert_eq!(
            serde_json::to_string(&back).unwrap(),
            serde_json::to_string(&v).unwrap()
        );
    }

    #[test]
    fn malformed_input_is_an_error_not_a_panic() {
        let bytes = encoded(&json!({"k": ["a", 1, {"x": [true]}]}));
        for cut in 0..bytes.len() {
            assert!(decoded(&bytes[..cut]).is_err(), "cut {cut}");
        }
        let mut flipped = bytes.clone();
        for i in 0..flipped.len() {
            for bit in [0x01, 0x80, 0xff] {
                flipped[i] ^= bit;
                let _ = decoded(&flipped);
                flipped[i] ^= bit;
            }
        }
        assert!(decoded(&[0xff; 12]).is_err());
    }

    #[test]
    fn malformed_numeric_records_are_rejected() {
        for number in ["NaN", "true", "01", "1e", "", "0 trailing"] {
            let mut r = Reader {
                buf: &[3, 0],
                pos: 0,
                strings: &[number],
                keys: &[],
            };
            assert!(
                r.value().is_err(),
                "accepted invalid JSON number {number:?}"
            );
        }
        let mut bytes = [0x80; 10];
        bytes[9] = 2;
        let mut r = Reader {
            buf: &bytes,
            pos: 0,
            strings: &[],
            keys: &[],
        };
        assert!(r.uvar().is_err(), "accepted an overflowing 64-bit varint");
    }
}
