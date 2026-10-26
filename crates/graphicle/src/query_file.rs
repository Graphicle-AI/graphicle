//! Saved query files: what `context`'s code-graph arm, identifier arm and
//! identifier route read of a code graph episode, stored so that a question
//! reads only what it needs (GitHub issue #8).
//!
//! Every question used to read the episode's whole saved graph
//! ([`crate::query_graph`]) before its arms ran: on a kubernetes checkout
//! (260,000 nodes, 1.5 million edges) that took 1.2 to 2.0 seconds and 3.5 GB
//! of memory whatever the question, and the arms then walked every node again
//! for the symbol start lines and the identifier symbols. This file keeps,
//! in a layout read in place (memory-mapped where the platform allows):
//! - per node, the strings the lexical scoring reads ([`TextRef`]), its
//!   degree, whether it is a document node, its attributes and its edges by
//!   neighbour (both directions on a directed graph);
//! - per edge, what the arms read of it and where a reference was read
//!   ([`EdgeRecord`]);
//! - the trigram postings of the engine's search index;
//! - the symbol start lines of every file and the code symbols with their
//!   start lines, by the last segment of their names;
//! - the node ids in order.
//!
//! A question scans the scoring strings its words need (every node's
//! `norm_lower` for the IDF weights, and the nodes its trigrams leave), and
//! decodes the postings of its own trigrams and the records of the nodes it
//! cites. The arms score and pick seeds with the engine's own code over this
//! file ([`graphicle::serve_search::score_on`],
//! [`graphicle::serve_search::pick_seeds_on`]) and expand, cite and follow
//! edges as over the resident graph ([`crate::code_graph_arm::plan`],
//! [`crate::identifier_route`]), so a packet is the one the resident graph
//! gives, byte for byte.
//!
//! Integrity: the body is cut into pages of [`PAGE`] bytes, each with a
//! checksum in the header (whose own checksum is checked on open, with the
//! section table and the page count against the file's length: a file with
//! this build's key whose header does not hold, truncated or damaged, is
//! removed on open, and [`current`] does not count it, so the next sync
//! writes it again). A page is checked the first time a read touches it, so
//! a question checks what it reads. A record that does not decode, a value
//! out of range or a page that does not match marks the file corrupt
//! ([`QueryFile::corrupt`]): the question then reads the saved graph
//! instead, with the same packet, and the file is removed so that the next
//! sync writes it again. A file is removed only while it is still the one
//! read (another sync may have replaced it since). The checksums
//! find damage, not a file made to pass them, so the reader also trusts no
//! count in the file: every count must fit in what is left of its section
//! (each item takes a byte at least) or in the table it indexes, a list
//! reserves room for at most [`RESERVE`] items before they decode (a nested
//! one for none), and a value nests at most [`MAX_DEPTH`] levels deep (the
//! writer refuses a graph with a deeper one). Such a file is read around
//! like a damaged one, without large allocations or deep recursion.
//!
//! The file is written beside the saved graph after each code graph build
//! ([`crate::query_graph::ensure`]) and keyed by its [`FORMAT`], the build of
//! the engine, of the memory library and of this program (hashes of their
//! sources and, for the program, of its Cargo.lock, which pins the
//! dependencies that shape the search index's strings), the program's
//! version, the project and the episode: a file written by another build is
//! never read. Change [`FORMAT`] whenever what is
//! written or how it is read changes. A missing, stale, truncated or
//! malformed file is never an error: the arms then read the saved graph as
//! before. `GRAPHICLE_QUERY_FILE=0` (or `GRAPHICLE_SAVED_GRAPH=0`) makes them
//! do so always.
use graphicle::graph::Graph;
use graphicle::serve_search::{ScoreSource, SearchIndex, TextRef};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

/// Changes when what this module writes, or how it reads it, changes.
pub const FORMAT: &str = "graphicle.query_file.v3";
const MAGIC: &[u8; 8] = b"GQFILE\0\x03";
/// Bytes of the body each checksum covers.
pub const PAGE: usize = 4096;
/// Section indices; their offsets and lengths follow the key.
const META: usize = 0;
const STR_OFF: usize = 1;
const STR_BLOB: usize = 2;
const NODES: usize = 3;
const NORM_BLOB: usize = 4;
const NORM_OFF: usize = 5;
const GRAM_KEYS: usize = 6;
const GRAM_POS: usize = 7;
const POSTINGS: usize = 8;
const ATTRS: usize = 9;
const ADJ: usize = 10;
const EDGE_OFF: usize = 11;
const EDGES: usize = 12;
const STARTS_TAB: usize = 13;
const STARTS: usize = 14;
const SYMBOLS: usize = 15;
const LAST_TAB: usize = 16;
const LAST_POS: usize = 17;
const IDS: usize = 18;
/// The number of sections.
pub const SECTIONS: usize = 19;
/// Sections a test may damage: the postings, the code symbols and the node
/// ids in order.
pub const POSTINGS_SECTION: usize = POSTINGS;
pub const SYMBOLS_SECTION: usize = SYMBOLS;
pub const IDS_SECTION: usize = IDS;
/// u32 fields of a node row: ten text string ids, the label length, the
/// degree, the attribute and adjacency record offsets, and the flags.
const ROW: usize = 15;
const FLAG_DOCUMENT: u32 = 1;
/// The edge attributes the arms read, in record order.
const EDGE_KEYS: [&str; 3] = ["context", "relation", "derivation"];
/// Most items a decoded list reserves room for before it reads them.
const RESERVE: usize = 1 << 12;
/// Deepest nesting of an attribute value.
const MAX_DEPTH: usize = 64;

/// Whether questions read query files: not with `GRAPHICLE_QUERY_FILE=0`,
/// nor when saved graphs are off (`GRAPHICLE_SAVED_GRAPH=0`).
pub fn enabled() -> bool {
    crate::query_graph::enabled()
        && !std::env::var("GRAPHICLE_QUERY_FILE").is_ok_and(|v| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "0" | "false" | "no" | "off"
            )
        })
}

/// The key a query file of `episode` must carry to be read by this build:
/// the format, the sources of the engine, the memory library and this
/// program (with this program's locked dependencies), the version, the
/// project and the episode.
pub fn key(repository: &str, episode: &str) -> Vec<u8> {
    format!(
        "{FORMAT}\0{}\0{}\0{}\0{}\0{repository}\0{episode}",
        graphicle::snapshot::build_code(),
        graphicle_memory::build_code(),
        env!("GRAPHICLE_RUNTIME_CODE"),
        env!("CARGO_PKG_VERSION")
    )
    .into_bytes()
}

/// A 64-bit checksum of `bytes`: four lanes of xor, multiply and rotate,
/// then a final mix. Every step is a bijection of its lane, so a change
/// within one 8-byte word always changes the sum.
fn sum(bytes: &[u8]) -> u64 {
    const K: [u64; 4] = [
        0x9E37_79B9_7F4A_7C15,
        0xC2B2_AE3D_27D4_EB4F,
        0x1656_67B1_9E37_79F9,
        0x85EB_CA77_C2B2_AE63,
    ];
    let word = |b: &[u8]| u64::from_le_bytes(b.try_into().unwrap_or([0; 8]));
    let mut lanes = [
        0x243F_6A88_85A3_08D3u64,
        0x1319_8A2E_0370_7344,
        0xA409_3822_299F_31D0,
        0x082E_FA98_EC4E_6C89,
    ];
    let mut chunks = bytes.chunks_exact(32);
    for chunk in &mut chunks {
        lanes[0] = (lanes[0] ^ word(&chunk[0..8]))
            .wrapping_mul(K[0])
            .rotate_left(31);
        lanes[1] = (lanes[1] ^ word(&chunk[8..16]))
            .wrapping_mul(K[1])
            .rotate_left(31);
        lanes[2] = (lanes[2] ^ word(&chunk[16..24]))
            .wrapping_mul(K[2])
            .rotate_left(31);
        lanes[3] = (lanes[3] ^ word(&chunk[24..32]))
            .wrapping_mul(K[3])
            .rotate_left(31);
    }
    let mut h = (bytes.len() as u64).wrapping_mul(K[0]);
    for (i, lane) in lanes.iter().enumerate() {
        h = (h ^ lane.rotate_left(i as u32 * 16))
            .wrapping_mul(K[1])
            .rotate_left(27);
    }
    for &byte in chunks.remainder() {
        h = (h ^ u64::from(byte)).wrapping_mul(K[2]).rotate_left(23);
    }
    h ^= h >> 33;
    h = h.wrapping_mul(0xFF51_AFD7_ED55_8CCD);
    h ^= h >> 33;
    h = h.wrapping_mul(0xC4CE_B9FE_1A85_EC53);
    h ^ (h >> 33)
}

// ---------------------------------------------------------------------------
// Writing.

fn put_var(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

/// The section offset `v` as a u32, or an error when a section outgrew it:
/// of kind `Unsupported`, as the same code graph always outgrows it.
fn small(v: usize) -> std::io::Result<u32> {
    u32::try_from(v).map_err(|_| unsupported("a query file section would pass 4 GiB".to_string()))
}

/// The checksum of the query file's pages ([`sum`]), for the other files
/// beside it ([`crate::query_graph`]).
pub(crate) fn checksum(bytes: &[u8]) -> u64 {
    sum(bytes)
}

#[derive(Default)]
struct Strings {
    ids: HashMap<String, u32>,
    offsets: Vec<u32>,
    blob: Vec<u8>,
}

impl Strings {
    fn id(&mut self, text: &str) -> std::io::Result<u32> {
        if let Some(&id) = self.ids.get(text) {
            return Ok(id);
        }
        let id = small(self.offsets.len())?;
        self.offsets.push(small(self.blob.len())?);
        self.blob.extend_from_slice(text.as_bytes());
        self.ids.insert(text.to_string(), id);
        Ok(id)
    }
}

/// Why a graph cannot have a query file: what it holds that the reader
/// would refuse. The sync records it as a write that failed
/// ([`crate::query_graph::ensure`]), so the same code graph is not tried
/// again.
fn unsupported(why: String) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::Unsupported, why)
}

/// `value`, nested `depth` levels below an attribute, in the record
/// encoding: a tag, then the string id (strings and numbers, a number by its
/// exact JSON text) or the items. A value nested deeper than the reader
/// reads ([`MAX_DEPTH`]) is refused.
fn put_value(
    out: &mut Vec<u8>,
    strings: &mut Strings,
    value: &Value,
    depth: usize,
) -> std::io::Result<()> {
    if depth > MAX_DEPTH {
        return Err(unsupported(format!(
            "an attribute value is nested more than {MAX_DEPTH} levels deep"
        )));
    }
    match value {
        Value::Null => out.push(0),
        Value::Bool(false) => out.push(1),
        Value::Bool(true) => out.push(2),
        Value::Number(number) => {
            out.push(3);
            put_var(out, u64::from(strings.id(&number.to_string())?));
        }
        Value::String(text) => {
            out.push(4);
            put_var(out, u64::from(strings.id(text)?));
        }
        Value::Array(items) => {
            out.push(5);
            put_var(out, items.len() as u64);
            for item in items {
                put_value(out, strings, item, depth + 1)?;
            }
        }
        Value::Object(fields) => {
            out.push(6);
            put_var(out, fields.len() as u64);
            for (name, item) in fields {
                put_var(out, u64::from(strings.id(name)?));
                put_value(out, strings, item, depth + 1)?;
            }
        }
    }
    Ok(())
}

/// An optional string as 0, or 1 plus its id.
fn put_text(out: &mut Vec<u8>, strings: &mut Strings, text: Option<&str>) -> std::io::Result<()> {
    match text {
        None => put_var(out, 0),
        Some(text) => put_var(out, 1 + u64::from(strings.id(text)?)),
    }
    Ok(())
}

/// Three chars packed into 63 bits, as the engine keys its trigram postings.
fn pack3(w: &[char]) -> u64 {
    (u64::from(u32::from(w[0])) << 42)
        | (u64::from(u32::from(w[1])) << 21)
        | u64::from(u32::from(w[2]))
}

/// A needle's distinct trigram keys, or the whole text when it is shorter
/// than three chars (the engine's `node_grams`).
fn grams_of(text: &str) -> (Vec<u64>, bool) {
    let chars = text.chars().collect::<Vec<_>>();
    if chars.len() < 3 {
        return (Vec::new(), !text.is_empty());
    }
    let mut keys = chars.windows(3).map(pack3).collect::<Vec<_>>();
    keys.sort_unstable();
    keys.dedup();
    (keys, false)
}

/// The query file of `graph` and its search `index`, keyed by `key`. A
/// graph with a node whose id is not a string, or with an attribute value
/// nested more than [`MAX_DEPTH`] levels deep, has none (an error of kind
/// `Unsupported`): the arms then read the saved graph.
pub fn encode(key: &[u8], graph: &Graph, index: &SearchIndex) -> std::io::Result<Vec<u8>> {
    let nodes = graph.nodes();
    let n = nodes.len();
    let mut ids = Vec::with_capacity(n);
    for node in nodes {
        match &node.id {
            Value::String(id) => ids.push(id.as_str()),
            _ => return Err(unsupported("a node id is not a string".to_string())),
        }
    }
    let mut sections: Vec<Vec<u8>> = vec![Vec::new(); SECTIONS];
    let mut strings = Strings::default();
    let degrees = graphicle::serve_search::degrees(graph);
    // Edges get record ids in the order the adjacency rows first list them.
    let mut edge_ids: HashMap<usize, u32> = HashMap::new();
    let mut edge_offsets: Vec<u32> = Vec::new();
    let mut edges_blob: Vec<u8> = Vec::new();
    let mut rows: Vec<u8> = Vec::with_capacity(n * ROW * 4);
    let mut norm_blob: Vec<u8> = Vec::new();
    let mut norm_offsets: Vec<u32> = Vec::with_capacity(n + 1);
    let mut attrs: Vec<u8> = Vec::new();
    let mut adjacency: Vec<u8> = Vec::new();
    for (i, node) in nodes.iter().enumerate() {
        let text = index.text(i).fields();
        for field in [
            text.norm,
            text.norm_lower,
            text.label,
            text.label_tokens,
            text.src_lower,
            text.rationale,
            text.attr_norm,
            text.attr_tokens,
            text.id,
            text.id_lower,
        ] {
            put_u32(&mut rows, strings.id(field)?);
        }
        put_u32(&mut rows, small(text.len)?);
        put_u32(&mut rows, small(degrees[i])?);
        put_u32(&mut rows, small(attrs.len())?);
        put_u32(&mut rows, small(adjacency.len())?);
        let mut flags = 0;
        if crate::code_graph_arm::document_node(&node.attributes) {
            flags |= FLAG_DOCUMENT;
        }
        put_u32(&mut rows, flags);
        norm_offsets.push(small(norm_blob.len())?);
        norm_blob.extend_from_slice(text.norm_lower.as_bytes());
        norm_blob.push(0);
        // The attributes, in their order.
        put_var(&mut attrs, node.attributes.len() as u64);
        for (name, value) in node.attributes.iter() {
            put_var(&mut attrs, u64::from(strings.id(name)?));
            put_value(&mut attrs, &mut strings, value, 0)?;
        }
        // The edges to each neighbour (`edges_between(node, v)`) and, on a
        // directed graph, from each one (`edges_between(v, node)`).
        let outgoing: Vec<usize> = graph.neighbor_indices(i, false).collect();
        let incoming: Vec<usize> = if graph.is_directed() {
            graph.neighbor_indices(i, true).collect()
        } else {
            Vec::new()
        };
        for (incoming, neighbours) in [(false, outgoing), (true, incoming)] {
            put_var(&mut adjacency, neighbours.len() as u64);
            for v in neighbours {
                let list = if incoming {
                    graph.edges_between(&nodes[v].id, &node.id)
                } else {
                    graph.edges_between(&node.id, &nodes[v].id)
                };
                put_var(&mut adjacency, v as u64);
                put_var(&mut adjacency, list.len() as u64);
                for edge in list {
                    let next = small(edge_offsets.len())?;
                    let id = *edge_ids
                        .entry(std::ptr::from_ref(edge) as usize)
                        .or_insert(next);
                    if id == next {
                        edge_offsets.push(small(edges_blob.len())?);
                        for name in EDGE_KEYS {
                            match edge.attributes.get(name) {
                                None => edges_blob.push(0),
                                Some(value) => {
                                    edges_blob.push(1);
                                    put_value(&mut edges_blob, &mut strings, value, 0)?;
                                }
                            }
                        }
                        let files = crate::code_graph_arm::edge_files(&edge.attributes);
                        put_var(&mut edges_blob, files.len() as u64);
                        for file in files {
                            put_var(&mut edges_blob, u64::from(strings.id(&file)?));
                        }
                        // The identifier route's reading of the edge.
                        let (source, target) = crate::identifier_route::reference_ends(graph, edge);
                        put_var(&mut edges_blob, source as u64);
                        put_var(&mut edges_blob, target as u64);
                        let (file, line) =
                            crate::identifier_route::reference_site(&edge.attributes);
                        put_text(&mut edges_blob, &mut strings, file.as_deref())?;
                        put_var(&mut edges_blob, line.map_or(0, |line| line as u64 + 1));
                    }
                    put_var(&mut adjacency, u64::from(id));
                }
            }
        }
    }
    norm_offsets.push(small(norm_blob.len())?);
    edge_offsets.push(small(edges_blob.len())?);
    // The engine's trigram postings (texts shorter than three chars are not
    // trigrams: no needle looks them up).
    let mut grams: Vec<(u64, Vec<usize>)> = index
        .postings()
        .into_iter()
        .filter_map(|(gram, list)| {
            let chars = gram.chars().collect::<Vec<_>>();
            (chars.len() == 3).then(|| (pack3(&chars), list))
        })
        .collect();
    grams.sort_unstable_by_key(|(key, _)| *key);
    for (key, list) in &grams {
        sections[GRAM_KEYS].extend_from_slice(&key.to_le_bytes());
        put_u32(&mut sections[GRAM_POS], small(list.len())?);
        let at = small(sections[POSTINGS].len())?;
        put_u32(&mut sections[GRAM_POS], at);
        let mut previous = 0usize;
        for &i in list {
            put_var(&mut sections[POSTINGS], (i - previous) as u64);
            previous = i;
        }
    }
    // Symbol start lines by file, sorted by path.
    let starts = crate::code_graph_arm::symbol_starts(graph);
    let mut paths: Vec<(&String, &Vec<usize>)> = starts.iter().collect();
    paths.sort_by(|a, b| a.0.cmp(b.0));
    for (path, lines) in &paths {
        put_u32(&mut sections[STARTS_TAB], strings.id(path)?);
        let at = small(sections[STARTS].len())?;
        put_u32(&mut sections[STARTS_TAB], at);
        put_var(&mut sections[STARTS], lines.len() as u64);
        for &line in lines.iter() {
            put_var(&mut sections[STARTS], line as u64);
        }
    }
    // The code symbols, and their positions by last segment.
    let symbols = crate::identifier_arm::symbols_of(graph);
    let mut by_last: BTreeMap<String, Vec<u32>> = BTreeMap::new();
    for (position, (index, name, relpath, start)) in symbols.iter().enumerate() {
        put_u32(&mut sections[SYMBOLS], small(*index)?);
        put_u32(&mut sections[SYMBOLS], strings.id(name)?);
        put_u32(&mut sections[SYMBOLS], strings.id(relpath)?);
        put_u32(
            &mut sections[SYMBOLS],
            small(start.map_or(0, |line| line + 1))?,
        );
        let last = name.rsplit('.').next().unwrap_or(name);
        by_last
            .entry(last.to_string())
            .or_default()
            .push(small(position)?);
    }
    for (last, positions) in &by_last {
        put_u32(&mut sections[LAST_TAB], strings.id(last)?);
        let at = small(sections[LAST_POS].len() / 4)?;
        put_u32(&mut sections[LAST_TAB], at);
        put_u32(&mut sections[LAST_TAB], small(positions.len())?);
        for &position in positions {
            put_u32(&mut sections[LAST_POS], position);
        }
    }
    // Node indices by id.
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by(|&a, &b| ids[a].cmp(ids[b]));
    for i in order {
        put_u32(&mut sections[IDS], small(i)?);
    }
    strings.offsets.push(small(strings.blob.len())?);
    for value in [
        u32::from(graph.is_directed()),
        u32::from(graph.is_multigraph()),
        small(n)?,
        small(strings.offsets.len() - 1)?,
        small(grams.len())?,
        small(edge_offsets.len() - 1)?,
        small(paths.len())?,
        small(symbols.len())?,
        small(by_last.len())?,
    ] {
        put_u32(&mut sections[META], value);
    }
    for offset in &strings.offsets {
        put_u32(&mut sections[STR_OFF], *offset);
    }
    sections[STR_BLOB] = strings.blob;
    sections[NODES] = rows;
    sections[NORM_BLOB] = norm_blob;
    for offset in norm_offsets {
        put_u32(&mut sections[NORM_OFF], offset);
    }
    sections[ATTRS] = attrs;
    sections[ADJ] = adjacency;
    for offset in edge_offsets {
        put_u32(&mut sections[EDGE_OFF], offset);
    }
    sections[EDGES] = edges_blob;
    // The header: magic, key, section table, the body's start, the page
    // checksums and the header's own checksum; then the body.
    let body_len: usize = sections.iter().map(Vec::len).sum();
    let pages = body_len.div_ceil(PAGE);
    let header_len = MAGIC.len() + 4 + key.len() + SECTIONS * 16 + 16 + pages * 8 + 8;
    let mut out = Vec::with_capacity(header_len + body_len);
    out.extend_from_slice(MAGIC);
    put_u32(&mut out, small(key.len())?);
    out.extend_from_slice(key);
    let mut at = header_len;
    for section in &sections {
        out.extend_from_slice(&(at as u64).to_le_bytes());
        out.extend_from_slice(&(section.len() as u64).to_le_bytes());
        at += section.len();
    }
    out.extend_from_slice(&(header_len as u64).to_le_bytes());
    out.extend_from_slice(&(pages as u64).to_le_bytes());
    let mut body = Vec::with_capacity(body_len);
    for section in &sections {
        body.extend_from_slice(section);
    }
    for page in body.chunks(PAGE) {
        out.extend_from_slice(&sum(page).to_le_bytes());
    }
    let header_sum = sum(&out);
    out.extend_from_slice(&header_sum.to_le_bytes());
    debug_assert_eq!(out.len(), header_len);
    out.extend_from_slice(&body);
    Ok(out)
}

/// Writes the page checksums and the header's checksum of a query file's
/// bytes again, in place, as [`encode`] does: for tests that give a file
/// content a damaged page would not show (a hostile file). False when the
/// bytes hold no whole header.
#[doc(hidden)]
pub fn reseal(bytes: &mut [u8]) -> bool {
    let Some(length) = bytes
        .get(MAGIC.len()..MAGIC.len() + 4)
        .map(|raw| u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]) as usize)
    else {
        return false;
    };
    let after = MAGIC.len() + 4 + length + SECTIONS * 16;
    let read = |bytes: &[u8], at: usize| -> Option<usize> {
        let raw = bytes.get(at..at.checked_add(8)?)?;
        usize::try_from(u64::from_le_bytes(raw.try_into().ok()?)).ok()
    };
    let (Some(body), Some(pages)) = (read(bytes, after), read(bytes, after + 8)) else {
        return false;
    };
    let sums = after + 16;
    let Some(header_sum) = pages.checked_mul(8).and_then(|len| len.checked_add(sums)) else {
        return false;
    };
    if header_sum.checked_add(8) != Some(body) || body > bytes.len() {
        return false;
    }
    for page in 0..pages {
        let from = body + page * PAGE;
        let to = (from + PAGE).min(bytes.len());
        let Some(content) = bytes.get(from..to) else {
            return false;
        };
        let page_sum = sum(content).to_le_bytes();
        bytes[sums + page * 8..sums + page * 8 + 8].copy_from_slice(&page_sum);
    }
    let header = sum(&bytes[..header_sum]).to_le_bytes();
    bytes[header_sum..header_sum + 8].copy_from_slice(&header);
    true
}

// ---------------------------------------------------------------------------
// Reading.

/// The bytes of a file, memory-mapped where the platform allows.
enum Bytes {
    Owned(Vec<u8>),
    #[cfg(unix)]
    Mapped(*const u8, usize),
}

// The mapping is read-only and lives as long as the value.
unsafe impl Send for Bytes {}
unsafe impl Sync for Bytes {}

impl Bytes {
    /// The bytes of the file at `path`, and the identity of the file read
    /// ([`crate::query_graph::identity`]).
    fn read(path: &Path) -> std::io::Result<(Self, (u64, u64))> {
        let file = std::fs::File::open(path)?;
        let metadata = file.metadata()?;
        let identity = crate::query_graph::identity(&metadata);
        let len = metadata.len() as usize;
        #[cfg(unix)]
        {
            if len > 0 {
                use std::os::unix::io::AsRawFd;
                // SAFETY: a private read-only mapping of a file that is only
                // ever replaced whole (`write_atomic`), never changed in place;
                // it is unmapped when the value drops.
                let pointer = unsafe {
                    libc::mmap(
                        std::ptr::null_mut(),
                        len,
                        libc::PROT_READ,
                        libc::MAP_PRIVATE,
                        file.as_raw_fd(),
                        0,
                    )
                };
                if pointer != libc::MAP_FAILED {
                    return Ok((Self::Mapped(pointer as *const u8, len), identity));
                }
            }
        }
        let mut bytes = Vec::with_capacity(len);
        std::io::Read::read_to_end(&mut std::io::BufReader::new(file), &mut bytes)?;
        Ok((Self::Owned(bytes), identity))
    }
    fn get(&self) -> &[u8] {
        match self {
            Self::Owned(bytes) => bytes,
            // SAFETY: the mapping of `len` bytes lives until the value drops.
            #[cfg(unix)]
            Self::Mapped(pointer, len) => unsafe { std::slice::from_raw_parts(*pointer, *len) },
        }
    }
}

impl Drop for Bytes {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            if let Self::Mapped(pointer, len) = self {
                // SAFETY: unmaps the mapping `read` made, once.
                unsafe {
                    libc::munmap(*pointer as *mut libc::c_void, *len);
                }
            }
        }
    }
}

/// A read that found the file truncated, malformed or damaged.
#[derive(Debug)]
pub struct Corrupt;

/// One edge as the arms and the identifier route read it.
#[derive(Clone, Debug, PartialEq)]
pub struct EdgeRecord {
    pub context: Option<Value>,
    pub relation: Option<Value>,
    pub derivation: Option<Value>,
    /// [`crate::code_graph_arm::edge_files`] of the edge.
    pub files: Vec<String>,
    /// The relation's ends as the extractor read them
    /// ([`crate::identifier_route::reference_ends`]).
    pub source: usize,
    pub target: usize,
    /// Where the relation was read ([`crate::identifier_route::reference_site`]).
    pub site_file: Option<String>,
    pub site_line: Option<usize>,
}

/// One node's edges, as edge record ids by neighbour.
#[derive(Clone, Debug, Default)]
pub struct Adjacency {
    directed: bool,
    /// `edges_between(node, v)` by neighbour `v`.
    out: HashMap<usize, Vec<u32>>,
    /// `edges_between(v, node)` by neighbour `v` (directed graphs).
    incoming: HashMap<usize, Vec<u32>>,
}

impl Adjacency {
    /// `graph.neighbor_indices(node, false)` and, on a directed graph,
    /// `graph.neighbor_indices(node, true)`, as one set.
    pub fn neighbours(&self) -> BTreeSet<usize> {
        let mut all: BTreeSet<usize> = self.out.keys().copied().collect();
        all.extend(self.incoming.keys().copied());
        all
    }
    /// `graph.edges_between(node, v)`.
    pub fn out_to(&self, v: usize) -> &[u32] {
        self.out.get(&v).map_or(&[], Vec::as_slice)
    }
    /// `graph.edges_between(v, node)`: on an undirected graph the same
    /// edges, in the same order, as [`Adjacency::out_to`].
    pub fn in_from(&self, v: usize) -> &[u32] {
        if self.directed {
            self.incoming.get(&v).map_or(&[], Vec::as_slice)
        } else {
            self.out_to(v)
        }
    }
    /// Every edge of the node, each once: `out_edges` and, on a directed
    /// graph, `in_edges`.
    pub fn all(&self) -> BTreeSet<u32> {
        self.out
            .values()
            .chain(self.incoming.values())
            .flatten()
            .copied()
            .collect()
    }
}

/// An open query file.
pub struct QueryFile {
    bytes: Bytes,
    path: PathBuf,
    /// The file read ([`crate::query_graph::identity`]): a damaged file is
    /// removed only while it is still the one at `path`.
    identity: (u64, u64),
    sections: [(usize, usize); SECTIONS],
    /// Where the body starts, and where the page checksums are.
    body: usize,
    sums: usize,
    pages: usize,
    /// Pages checked so far, one bit each.
    checked: Box<[AtomicU64]>,
    corrupt: AtomicBool,
    /// Whether the norms blob is UTF-8, once checked.
    norms_valid: std::sync::OnceLock<bool>,
    pub directed: bool,
    pub multigraph: bool,
    n: usize,
    strings: usize,
    grams: usize,
    edges: usize,
    paths: usize,
    symbols: usize,
    lasts: usize,
}

/// A reader of a section's varints, which checks each page it enters.
struct Cursor<'a> {
    file: &'a QueryFile,
    s: usize,
    at: usize,
    /// Positions below this are in checked pages.
    ok: usize,
}

impl Cursor<'_> {
    fn byte(&mut self) -> Result<u8, Corrupt> {
        let (offset, len) = self.file.sections[self.s];
        if self.at >= len {
            return Err(self.file.mark());
        }
        if self.at >= self.ok {
            let absolute = offset + self.at;
            self.file.check_pages(absolute, absolute + 1)?;
            let page_end = self.file.body + ((absolute - self.file.body) / PAGE + 1) * PAGE;
            self.ok = (page_end - offset).min(len);
        }
        let byte = self.file.bytes.get()[offset + self.at];
        self.at += 1;
        Ok(byte)
    }
    fn var(&mut self) -> Result<u64, Corrupt> {
        let mut value = 0u64;
        let mut shift = 0;
        loop {
            let byte = self.byte()?;
            value |= u64::from(byte & 0x7f) << shift;
            if byte < 0x80 {
                return Ok(value);
            }
            shift += 7;
            if shift > 63 {
                return Err(self.file.mark());
            }
        }
    }
    /// A varint below `bound`.
    fn below(&mut self, bound: usize) -> Result<usize, Corrupt> {
        let value = self.var()?;
        if value >= bound as u64 {
            return Err(self.file.mark());
        }
        Ok(value as usize)
    }
    /// A count of items that each take at least one byte of what is left.
    fn count(&mut self) -> Result<usize, Corrupt> {
        let value = self.var()?;
        let left = self.file.sections[self.s].1.saturating_sub(self.at);
        if value > left as u64 {
            return Err(self.file.mark());
        }
        Ok(value as usize)
    }
    /// An optional string: 0, or 1 plus its id.
    fn text(&mut self) -> Result<Option<String>, Corrupt> {
        match self.var()? {
            0 => Ok(None),
            id => Ok(Some(self.file.string(id - 1)?.to_string())),
        }
    }
    /// A value nested `depth` levels below an attribute. Only an
    /// attribute's own list reserves room before its items decode (at most
    /// [`RESERVE`]): a nested one grows as its items do, so a short hostile
    /// record cannot make every level of a deep value reserve.
    fn value(&mut self, depth: usize) -> Result<Value, Corrupt> {
        if depth > MAX_DEPTH {
            return Err(self.file.mark());
        }
        let reserve = |count: usize| if depth == 0 { count.min(RESERVE) } else { 0 };
        Ok(match self.byte()? {
            0 => Value::Null,
            1 => Value::Bool(false),
            2 => Value::Bool(true),
            3 => {
                let id = self.var()?;
                let text = self.file.string(id)?;
                Value::Number(
                    text.parse::<serde_json::Number>()
                        .map_err(|_| self.file.mark())?,
                )
            }
            4 => {
                let id = self.var()?;
                Value::String(self.file.string(id)?.to_string())
            }
            5 => {
                let count = self.count()?;
                let mut items = Vec::with_capacity(reserve(count));
                for _ in 0..count {
                    items.push(self.value(depth + 1)?);
                }
                Value::Array(items)
            }
            6 => {
                let count = self.count()?;
                let mut fields = Map::with_capacity(reserve(count));
                for _ in 0..count {
                    let id = self.var()?;
                    let name = self.file.string(id)?.to_string();
                    let item = self.value(depth + 1)?;
                    fields.insert(name, item);
                }
                Value::Object(fields)
            }
            _ => return Err(self.file.mark()),
        })
    }
}

/// Where a query file's sections lie and where its page checksums are.
struct Header {
    sections: [(usize, usize); SECTIONS],
    body: usize,
    sums: usize,
    pages: usize,
}

/// Why [`header`] found no whole header at the start of a file.
enum NotWhole {
    /// Not a query file this build wrote for this key (another build's,
    /// another episode's, or none at all): it is not read, and the next
    /// sync writes the file for this key in its place.
    Other,
    /// This build's key, but a header that does not hold: truncated or
    /// damaged.
    Damaged,
}

/// The header of a query file of `len` bytes whose first bytes are `data`
/// (the header at least, or the whole file): its key, then its section
/// table, its page count and the header's own checksum, measured against
/// `len`.
fn header(data: &[u8], len: usize, key: &[u8]) -> Result<Header, NotWhole> {
    if !ours(data, key) {
        return Err(NotWhole::Other);
    }
    whole_header(data, len, key.len()).ok_or(NotWhole::Damaged)
}

/// Whether `data` starts as a query file this build wrote for `key`: the
/// magic, the key's length and the key.
fn ours(data: &[u8], key: &[u8]) -> bool {
    data.strip_prefix(MAGIC.as_slice()).is_some_and(|rest| {
        rest.get(..4)
            .is_some_and(|raw| raw == (key.len() as u32).to_le_bytes())
            && rest.get(4..4 + key.len()) == Some(key)
    })
}

/// [`header`] past the key, or `None` when anything does not hold.
fn whole_header(data: &[u8], len: usize, key_len: usize) -> Option<Header> {
    let raw_u64 = |at: usize| -> Option<u64> {
        let raw = data.get(at..at.checked_add(8)?)?;
        Some(u64::from_le_bytes(raw.try_into().ok()?))
    };
    let read_u64 = |at: usize| raw_u64(at).and_then(|v| usize::try_from(v).ok());
    let table = MAGIC.len() + 4 + key_len;
    let mut sections = [(0usize, 0usize); SECTIONS];
    for (s, slot) in sections.iter_mut().enumerate() {
        *slot = (read_u64(table + s * 16)?, read_u64(table + s * 16 + 8)?);
    }
    let after = table + SECTIONS * 16;
    let (body, pages) = (read_u64(after)?, read_u64(after + 8)?);
    let sums = after + 16;
    let header_sum = sums.checked_add(pages.checked_mul(8)?)?;
    if header_sum.checked_add(8)? != body || body > len || body > data.len() {
        return None;
    }
    if (len - body).div_ceil(PAGE) != pages || raw_u64(header_sum)? != sum(&data[..header_sum]) {
        return None;
    }
    // Sections lie in the body, in order, each after the last, to the end.
    let mut next = body;
    for &(offset, length) in &sections {
        if offset != next {
            return None;
        }
        next = offset.checked_add(length)?;
    }
    (next == len).then_some(Header {
        sections,
        body,
        sums,
        pages,
    })
}

impl QueryFile {
    /// The file at `path` when it carries `key` and its header is whole:
    /// the header's checksum, the section table and the page count. The
    /// pages are checked as reads reach them. A file with this key whose
    /// header or tables do not hold (truncated or damaged) is removed, so
    /// that the next sync writes it again.
    pub fn open(path: &Path, key: &[u8]) -> Option<Self> {
        let (bytes, identity) = Bytes::read(path).ok()?;
        let data = bytes.get();
        let Header {
            sections,
            body,
            sums,
            pages,
        } = match header(data, data.len(), key) {
            Ok(header) => header,
            Err(NotWhole::Other) => return None,
            Err(NotWhole::Damaged) => {
                crate::query_graph::remove_damaged(path, identity);
                return None;
            }
        };
        let file = Self {
            checked: (0..pages.div_ceil(64)).map(|_| AtomicU64::new(0)).collect(),
            bytes,
            path: path.to_path_buf(),
            identity,
            sections,
            body,
            sums,
            pages,
            corrupt: AtomicBool::new(false),
            norms_valid: std::sync::OnceLock::new(),
            directed: false,
            multigraph: false,
            n: 0,
            strings: 0,
            grams: 0,
            edges: 0,
            paths: 0,
            symbols: 0,
            lasts: 0,
        };
        // A read that fails here marks the file corrupt, which removes it.
        let mut meta = [0usize; 9];
        for (i, slot) in meta.iter_mut().enumerate() {
            *slot = file.u32_in(META, i).ok()? as usize;
        }
        let [
            directed,
            multigraph,
            n,
            strings,
            grams,
            edges,
            paths,
            symbols,
            lasts,
        ] = meta;
        // Each table's length follows from its count; a count no length
        // can hold is refused too.
        let table =
            |count: usize, extra: usize, width: usize| count.checked_add(extra)?.checked_mul(width);
        let sized = [
            (META, Some(9 * 4)),
            (STR_OFF, table(strings, 1, 4)),
            (NODES, table(n, 0, ROW * 4)),
            (NORM_OFF, table(n, 1, 4)),
            (GRAM_KEYS, table(grams, 0, 8)),
            (GRAM_POS, table(grams, 0, 8)),
            (EDGE_OFF, table(edges, 1, 4)),
            (STARTS_TAB, table(paths, 0, 8)),
            (SYMBOLS, table(symbols, 0, 16)),
            (LAST_TAB, table(lasts, 0, 12)),
            (IDS, table(n, 0, 4)),
        ];
        if sized
            .iter()
            .any(|&(s, len)| len != Some(file.sections[s].1))
        {
            file.mark();
            return None;
        }
        Some(Self {
            directed: directed == 1,
            multigraph: multigraph == 1,
            n,
            strings,
            grams,
            edges,
            paths,
            symbols,
            lasts,
            ..file
        })
    }

    /// Whether a read found the file truncated, malformed or damaged.
    pub fn corrupt(&self) -> bool {
        self.corrupt.load(Ordering::Relaxed)
    }

    /// The pages reads have checked so far (each page is checked by the
    /// first read that reaches it), and the pages of the file.
    pub fn pages_read(&self) -> (usize, usize) {
        let read = self
            .checked
            .iter()
            .map(|word| word.load(Ordering::Relaxed).count_ones() as usize)
            .sum();
        (read, self.pages)
    }

    /// With `GRAPHICLE_PHASE_TIMINGS`, logs as phase `name` what reads of
    /// the file found so far: the pages read, the file's pages, and whether
    /// a read found the file corrupt (the saved graph then answers).
    pub fn note(&self, name: &str) {
        if crate::phase::enabled() {
            let (read, pages) = self.pages_read();
            crate::phase::note(
                name,
                &format!(
                    "pages={read} of={pages} corrupt={}",
                    u8::from(self.corrupt())
                ),
            );
        }
    }

    /// Marks the file corrupt; the first time, removes it (while it is still
    /// the file at its path), so that the next sync writes it again.
    fn mark(&self) -> Corrupt {
        if !self.corrupt.swap(true, Ordering::Relaxed) {
            crate::query_graph::remove_damaged(&self.path, self.identity);
        }
        Corrupt
    }

    /// Checks the pages under the absolute byte range `start..end`.
    #[inline]
    fn check_pages(&self, start: usize, end: usize) -> Result<(), Corrupt> {
        // Most reads stay within one page that an earlier read checked.
        if end > start {
            let first = (start - self.body) / PAGE;
            if (end - 1 - self.body) / PAGE == first
                && self
                    .checked
                    .get(first / 64)
                    .is_some_and(|word| word.load(Ordering::Relaxed) & (1u64 << (first % 64)) != 0)
                && !self.corrupt()
            {
                return Ok(());
            }
        }
        self.check_pages_slowly(start, end)
    }

    /// [`QueryFile::check_pages`] for pages not checked yet.
    fn check_pages_slowly(&self, start: usize, end: usize) -> Result<(), Corrupt> {
        if self.corrupt() {
            return Err(Corrupt);
        }
        if end <= start {
            return Ok(());
        }
        let data = self.bytes.get();
        if start < self.body {
            return Err(self.mark());
        }
        let (first, last) = ((start - self.body) / PAGE, (end - 1 - self.body) / PAGE);
        for page in first..=last {
            if page >= self.pages {
                return Err(self.mark());
            }
            let (word, bit) = (page / 64, 1u64 << (page % 64));
            if self.checked[word].load(Ordering::Relaxed) & bit != 0 {
                continue;
            }
            let from = self.body + page * PAGE;
            let to = (from + PAGE).min(data.len());
            let at = self.sums + page * 8;
            let expected = u64::from_le_bytes([
                data[at],
                data[at + 1],
                data[at + 2],
                data[at + 3],
                data[at + 4],
                data[at + 5],
                data[at + 6],
                data[at + 7],
            ]);
            if sum(&data[from..to]) != expected {
                return Err(self.mark());
            }
            self.checked[word].fetch_or(bit, Ordering::Relaxed);
        }
        Ok(())
    }

    /// `len` bytes of section `s` from `at`, checked.
    fn read(&self, s: usize, at: usize, len: usize) -> Result<&[u8], Corrupt> {
        let (offset, size) = self.sections[s];
        let end = at.checked_add(len).ok_or_else(|| self.mark())?;
        if end > size {
            return Err(self.mark());
        }
        self.check_pages(offset + at, offset + end)?;
        Ok(&self.bytes.get()[offset + at..offset + end])
    }

    fn u32_in(&self, s: usize, i: usize) -> Result<u32, Corrupt> {
        let at = i.checked_mul(4).ok_or_else(|| self.mark())?;
        let raw = self.read(s, at, 4)?;
        Ok(u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]))
    }

    fn cursor(&self, s: usize, at: usize) -> Cursor<'_> {
        Cursor {
            file: self,
            s,
            at,
            ok: at,
        }
    }

    /// String `id`.
    fn string(&self, id: u64) -> Result<&str, Corrupt> {
        let id = usize::try_from(id).map_err(|_| self.mark())?;
        if id >= self.strings {
            return Err(self.mark());
        }
        // The string's start and end, side by side.
        let raw = self.read(STR_OFF, id * 4, 8)?;
        let start = u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]) as usize;
        let end = u32::from_le_bytes([raw[4], raw[5], raw[6], raw[7]]) as usize;
        if end < start {
            return Err(self.mark());
        }
        let raw = self.read(STR_BLOB, start, end - start)?;
        std::str::from_utf8(raw).map_err(|_| self.mark())
    }

    /// Field `field` of node `i`'s row.
    fn row(&self, i: usize, field: usize) -> Result<u32, Corrupt> {
        if i >= self.n {
            return Err(self.mark());
        }
        self.u32_in(NODES, i * ROW + field)
    }

    /// String field `field` of node `i`'s row.
    fn row_text(&self, i: usize, field: usize) -> Result<&str, Corrupt> {
        self.string(u64::from(self.row(i, field)?))
    }

    /// Node `i`'s whole row, in one read.
    fn row_all(&self, i: usize) -> Result<[u32; ROW], Corrupt> {
        if i >= self.n {
            return Err(self.mark());
        }
        let raw = self.read(NODES, i * ROW * 4, ROW * 4)?;
        let mut row = [0u32; ROW];
        for (field, value) in row.iter_mut().enumerate() {
            let at = field * 4;
            *value = u32::from_le_bytes([raw[at], raw[at + 1], raw[at + 2], raw[at + 3]]);
        }
        Ok(row)
    }

    /// Node `i`'s scoring strings ([`ScoreSource::text`]).
    fn texts(&self, i: usize) -> Result<TextRef<'_>, Corrupt> {
        let row = self.row_all(i)?;
        let field = |f: usize| self.string(u64::from(row[f]));
        Ok(TextRef {
            norm: field(0)?,
            norm_lower: field(1)?,
            label: field(2)?,
            label_tokens: field(3)?,
            src_lower: field(4)?,
            rationale: field(5)?,
            attr_norm: field(6)?,
            attr_tokens: field(7)?,
            id: field(8)?,
            id_lower: field(9)?,
            len: row[10] as usize,
        })
    }

    pub fn len(&self) -> usize {
        self.n
    }

    pub fn is_empty(&self) -> bool {
        self.n == 0
    }

    /// Node `i`'s id.
    pub fn id(&self, i: usize) -> Result<&str, Corrupt> {
        self.row_text(i, 8)
    }

    /// The node with id `id`.
    pub fn node_index(&self, id: &str) -> Result<Option<usize>, Corrupt> {
        let (mut low, mut high) = (0usize, self.n);
        while low < high {
            let middle = (low + high) / 2;
            let index = self.u32_in(IDS, middle)? as usize;
            match self.id(index)?.cmp(id) {
                std::cmp::Ordering::Less => low = middle + 1,
                std::cmp::Ordering::Greater => high = middle,
                std::cmp::Ordering::Equal => return Ok(Some(index)),
            }
        }
        Ok(None)
    }

    /// Whether node `i` is a document node
    /// ([`crate::code_graph_arm::document_node`]).
    pub fn is_document(&self, i: usize) -> Result<bool, Corrupt> {
        Ok(self.row(i, 14)? & FLAG_DOCUMENT != 0)
    }

    /// Node `i`'s attributes, as `Attributes::to_map` gives them.
    pub fn attributes(&self, i: usize) -> Result<Map<String, Value>, Corrupt> {
        let mut cursor = self.cursor(ATTRS, self.row(i, 12)? as usize);
        let count = cursor.count()?;
        let mut fields = Map::with_capacity(count.min(RESERVE));
        for _ in 0..count {
            let id = cursor.var()?;
            let name = self.string(id)?.to_string();
            let value = cursor.value(0)?;
            fields.insert(name, value);
        }
        Ok(fields)
    }

    /// Node `i`'s adjacency record.
    pub fn adjacency(&self, i: usize) -> Result<Adjacency, Corrupt> {
        let mut cursor = self.cursor(ADJ, self.row(i, 13)? as usize);
        let mut rows: [HashMap<usize, Vec<u32>>; 2] = [HashMap::new(), HashMap::new()];
        for row in &mut rows {
            let count = cursor.count()?;
            row.reserve(count.min(RESERVE));
            for _ in 0..count {
                let neighbour = cursor.below(self.n)?;
                let k = cursor.count()?;
                let mut edges = Vec::with_capacity(k.min(RESERVE));
                for _ in 0..k {
                    edges.push(cursor.below(self.edges)? as u32);
                }
                row.insert(neighbour, edges);
            }
        }
        let [out, incoming] = rows;
        Ok(Adjacency {
            directed: self.directed,
            out,
            incoming,
        })
    }

    /// Edge record `id`.
    pub fn edge(&self, id: u32) -> Result<EdgeRecord, Corrupt> {
        if id as usize >= self.edges {
            return Err(self.mark());
        }
        let mut cursor = self.cursor(EDGES, self.u32_in(EDGE_OFF, id as usize)? as usize);
        let mut values: [Option<Value>; 3] = [None, None, None];
        for slot in &mut values {
            match cursor.byte()? {
                0 => {}
                1 => *slot = Some(cursor.value(0)?),
                _ => return Err(self.mark()),
            }
        }
        let count = cursor.count()?;
        let mut files = Vec::with_capacity(count.min(RESERVE));
        for _ in 0..count {
            let id = cursor.var()?;
            files.push(self.string(id)?.to_string());
        }
        let source = cursor.below(self.n)?;
        let target = cursor.below(self.n)?;
        let site_file = cursor.text()?;
        let site_line = match cursor.var()? {
            0 => None,
            line => Some(usize::try_from(line - 1).map_err(|_| self.mark())?),
        };
        let [context, relation, derivation] = values;
        Ok(EdgeRecord {
            context,
            relation,
            derivation,
            files,
            source,
            target,
            site_file,
            site_line,
        })
    }

    /// The edge records `ids`.
    pub fn edges(&self, ids: &[u32]) -> Result<Vec<EdgeRecord>, Corrupt> {
        ids.iter().map(|&id| self.edge(id)).collect()
    }

    /// The symbol start lines of `path`
    /// ([`crate::code_graph_arm::symbol_starts`]).
    pub fn starts(&self, path: &str) -> Result<Option<Vec<usize>>, Corrupt> {
        let (mut low, mut high) = (0usize, self.paths);
        while low < high {
            let middle = (low + high) / 2;
            let name = self.string(u64::from(self.u32_in(STARTS_TAB, middle * 2)?))?;
            match name.cmp(path) {
                std::cmp::Ordering::Less => low = middle + 1,
                std::cmp::Ordering::Greater => high = middle,
                std::cmp::Ordering::Equal => {
                    let at = self.u32_in(STARTS_TAB, middle * 2 + 1)? as usize;
                    let mut cursor = self.cursor(STARTS, at);
                    let count = cursor.count()?;
                    let mut lines = Vec::with_capacity(count.min(RESERVE));
                    for _ in 0..count {
                        lines.push(usize::try_from(cursor.var()?).map_err(|_| self.mark())?);
                    }
                    return Ok(Some(lines));
                }
            }
        }
        Ok(None)
    }

    /// Code symbol `position`: (node index, dotted name, file, start line).
    pub fn symbol(&self, position: usize) -> Result<(usize, &str, &str, Option<usize>), Corrupt> {
        if position >= self.symbols {
            return Err(self.mark());
        }
        let index = self.u32_in(SYMBOLS, position * 4)? as usize;
        if index >= self.n {
            return Err(self.mark());
        }
        let name = self.string(u64::from(self.u32_in(SYMBOLS, position * 4 + 1)?))?;
        let relpath = self.string(u64::from(self.u32_in(SYMBOLS, position * 4 + 2)?))?;
        let start = self.u32_in(SYMBOLS, position * 4 + 3)?;
        Ok((
            index,
            name,
            relpath,
            start.checked_sub(1).map(|line| line as usize),
        ))
    }

    /// The positions of the code symbols whose dotted name ends in segment
    /// `last`, in symbol order.
    pub fn symbols_ending(&self, last: &str) -> Result<Vec<usize>, Corrupt> {
        let (mut low, mut high) = (0usize, self.lasts);
        while low < high {
            let middle = (low + high) / 2;
            let name = self.string(u64::from(self.u32_in(LAST_TAB, middle * 3)?))?;
            match name.cmp(last) {
                std::cmp::Ordering::Less => low = middle + 1,
                std::cmp::Ordering::Greater => high = middle,
                std::cmp::Ordering::Equal => {
                    let first = self.u32_in(LAST_TAB, middle * 3 + 1)? as usize;
                    let count = self.u32_in(LAST_TAB, middle * 3 + 2)? as usize;
                    // The positions lie in the table, after `first`.
                    let entries = self.sections[LAST_POS].1 / 4;
                    if first > entries || count > entries - first {
                        return Err(self.mark());
                    }
                    let mut positions = Vec::with_capacity(count.min(RESERVE));
                    for p in first..first.saturating_add(count) {
                        let position = self.u32_in(LAST_POS, p)? as usize;
                        if position >= self.symbols {
                            return Err(self.mark());
                        }
                        positions.push(position);
                    }
                    return Ok(positions);
                }
            }
        }
        Ok(Vec::new())
    }

    /// The postings of gram `key`: its node count and record offset.
    fn gram(&self, key: u64) -> Result<Option<(usize, usize)>, Corrupt> {
        let (mut low, mut high) = (0usize, self.grams);
        while low < high {
            let middle = (low + high) / 2;
            let raw = self.read(GRAM_KEYS, middle * 8, 8)?;
            let found = u64::from_le_bytes(raw.try_into().map_err(|_| self.mark())?);
            match found.cmp(&key) {
                std::cmp::Ordering::Less => low = middle + 1,
                std::cmp::Ordering::Greater => high = middle,
                std::cmp::Ordering::Equal => {
                    return Ok(Some((
                        self.u32_in(GRAM_POS, middle * 2)? as usize,
                        self.u32_in(GRAM_POS, middle * 2 + 1)? as usize,
                    )));
                }
            }
        }
        Ok(None)
    }

    /// The nodes of a gram's postings: ascending, each below the node count.
    fn postings(&self, (count, offset): (usize, usize)) -> Result<Vec<usize>, Corrupt> {
        let mut cursor = self.cursor(POSTINGS, offset);
        let left = self.sections[POSTINGS].1.saturating_sub(offset);
        if count > left || count > self.n {
            return Err(self.mark());
        }
        let mut list = Vec::with_capacity(count.min(RESERVE));
        let mut previous = 0usize;
        for k in 0..count {
            let delta = usize::try_from(cursor.var()?).map_err(|_| self.mark())?;
            let node = previous.checked_add(delta).ok_or_else(|| self.mark())?;
            if node >= self.n || (k > 0 && delta == 0) {
                return Err(self.mark());
            }
            list.push(node);
            previous = node;
        }
        Ok(list)
    }

    /// The norms blob: every node's `norm_lower`, each followed by a NUL;
    /// its pages checked and its UTF-8 validated once.
    fn norms(&self) -> Result<&str, Corrupt> {
        let (_, size) = self.sections[NORM_BLOB];
        let raw = self.read(NORM_BLOB, 0, size)?;
        if !*self
            .norms_valid
            .get_or_init(|| std::str::from_utf8(raw).is_ok())
        {
            return Err(self.mark());
        }
        // SAFETY: `from_utf8` validated these very bytes above (once per
        // open file), and they do not change while the file is open: a
        // private read-only mapping of a file that is only ever replaced
        // whole, or the bytes read into memory.
        Ok(unsafe { std::str::from_utf8_unchecked(raw) })
    }

    /// Where node `i`'s `norm_lower` starts in the norms blob; `i == n`
    /// gives the blob's end.
    fn norm_start(&self, i: usize) -> Result<usize, Corrupt> {
        if i > self.n {
            return Err(self.mark());
        }
        let at = self.u32_in(NORM_OFF, i)? as usize;
        if at > self.sections[NORM_BLOB].1 {
            return Err(self.mark());
        }
        Ok(at)
    }

    /// The nodes whose `norm_lower` contains `term`.
    fn df(&self, term: &str) -> Result<usize, Corrupt> {
        if term.is_empty() {
            return Ok(self.n);
        }
        let text = self.norms()?;
        // Each node's string is followed by a NUL.
        if term.contains('\0') {
            let mut count = 0;
            for i in 0..self.n {
                let (a, b) = (self.norm_start(i)?, self.norm_start(i + 1)?);
                let norm = text
                    .get(a..b.saturating_sub(1).max(a))
                    .ok_or_else(|| self.mark())?;
                count += usize::from(norm.contains(term));
            }
            return Ok(count);
        }
        // A term without a NUL cannot match across two nodes' strings, so
        // one scan of the blob finds every node that holds the term: each
        // node's first match is never skipped.
        let mut count = 0usize;
        let mut node = 0usize;
        let mut next = self.norm_start(1.min(self.n))?;
        let mut last = None;
        for (position, _) in text.match_indices(term) {
            while next <= position {
                node += 1;
                next = self.norm_start(node + 1)?;
            }
            if position + term.len() < next && last != Some(node) {
                count += 1;
                last = Some(node);
            }
        }
        Ok(count)
    }

    /// `result`'s value, or `fallback` when the read failed (the file is
    /// then marked corrupt, and the caller reads the saved graph instead).
    fn or<T>(&self, result: Result<T, Corrupt>, fallback: T) -> T {
        result.unwrap_or(fallback)
    }
}

impl ScoreSource for QueryFile {
    fn size(&self) -> usize {
        self.n
    }
    fn text(&self, i: usize) -> TextRef<'_> {
        self.or(
            self.texts(i),
            TextRef {
                norm: "",
                norm_lower: "",
                label: "",
                label_tokens: "",
                src_lower: "",
                rationale: "",
                attr_norm: "",
                attr_tokens: "",
                id: "",
                id_lower: "",
                len: 0,
            },
        )
    }
    fn idf(&self, terms: &[String]) -> BTreeMap<String, f64> {
        let n = self.n.max(1) as f64;
        let mut weights = BTreeMap::new();
        for term in terms {
            if !weights.contains_key(term) {
                let df = self.or(self.df(term), 0);
                weights.insert(term.clone(), (1.0 + n / (1.0 + df as f64)).ln());
            }
        }
        weights
    }
    fn candidates(&self, needles: &[String], guard: f64) -> Option<Vec<usize>> {
        if self.n == 0 {
            return Some(Vec::new());
        }
        let needles = needles.iter().filter(|s| !s.is_empty()).collect::<Vec<_>>();
        let threshold = (self.n as f64 * guard).trunc();
        let mut keyed = Vec::with_capacity(needles.len());
        for needle in &needles {
            let (keys, short) = grams_of(needle);
            if short || keys.is_empty() {
                return None;
            }
            if keys
                .iter()
                .filter_map(|&key| self.or(self.gram(key), None).map(|(count, _)| count))
                .min()
                .is_some_and(|count| count as f64 > threshold)
            {
                return None;
            }
            keyed.push(keys);
        }
        let mut candidates = BTreeSet::new();
        for keys in keyed {
            let Some(mut sets) = keys
                .iter()
                .map(|&key| self.or(self.gram(key), None))
                .collect::<Option<Vec<_>>>()
            else {
                continue;
            };
            sets.sort_by_key(|(count, _)| *count);
            let Some(&first) = sets.first() else {
                continue;
            };
            let mut hits = self
                .or(self.postings(first), Vec::new())
                .into_iter()
                .collect::<BTreeSet<_>>();
            for &other in sets.iter().skip(1) {
                let list = self.or(self.postings(other), Vec::new());
                hits.retain(|i| list.binary_search(i).is_ok());
            }
            candidates.extend(hits);
        }
        Some(candidates.into_iter().collect())
    }
    fn degree(&self, i: usize) -> usize {
        self.or(self.row(i, 11), 0) as usize
    }
}

/// The query file of `episode`: `gf-cache/<project>/query/<episode>.query`.
pub fn path(db: &rusqlite::Connection, repository: &str, episode: &str) -> Option<PathBuf> {
    crate::query_graph::file_of(db, repository, episode, "query")
}

/// Whether `path` holds a query file this build reads for `key`, whole by
/// its header: the key, the header's checksum, and the section table and
/// page count against the file's length. It reads the header only, not the
/// pages (a damaged page is found by the question that reads it).
pub fn current(path: &Path, key: &[u8]) -> bool {
    use std::io::Read;
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    let Ok(len) = file.metadata().map(|m| m.len()) else {
        return false;
    };
    let Ok(len) = usize::try_from(len) else {
        return false;
    };
    let mut reader = std::io::BufReader::new(file);
    // The fixed part first (magic, key, section table, body offset and page
    // count), then the page checksums and the header's checksum, which end
    // where the body starts.
    let fixed = MAGIC.len() + 4 + key.len() + SECTIONS * 16 + 16;
    let mut head = Vec::with_capacity(fixed);
    if (&mut reader)
        .take(fixed as u64)
        .read_to_end(&mut head)
        .is_err()
        || head.len() != fixed
        || !ours(&head, key)
    {
        return false;
    }
    let body = u64::from_le_bytes(head[fixed - 16..fixed - 8].try_into().unwrap_or([0xff; 8]));
    let Some(body) = usize::try_from(body)
        .ok()
        .filter(|&body| body > fixed && body <= len)
    else {
        return false;
    };
    if reader
        .take((body - fixed) as u64)
        .read_to_end(&mut head)
        .is_err()
        || head.len() != body
    {
        return false;
    }
    header(&head, len, key).is_ok()
}

struct Cached {
    stamp: (u64, Option<std::time::SystemTime>),
    file: Arc<QueryFile>,
}

static OPEN: Mutex<Option<HashMap<PathBuf, Cached>>> = Mutex::new(None);

/// The query file of `episode` when this build wrote one, it is whole and
/// enabled; a process keeps it open (a server's later questions reuse it)
/// until a read finds it corrupt.
pub fn open(db: &rusqlite::Connection, repository: &str, episode: &str) -> Option<Arc<QueryFile>> {
    if !enabled() {
        return None;
    }
    let path = path(db, repository, episode)?;
    let metadata = std::fs::metadata(&path).ok()?;
    let stamp = (metadata.len(), metadata.modified().ok());
    {
        let open = OPEN.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(cached) = open.as_ref().and_then(|open| open.get(&path))
            && cached.stamp == stamp
            && !cached.file.corrupt()
        {
            return Some(cached.file.clone());
        }
    }
    let file = Arc::new(QueryFile::open(&path, &key(repository, episode))?);
    let mut open = OPEN.lock().unwrap_or_else(PoisonError::into_inner);
    let open = open.get_or_insert_with(HashMap::new);
    // One file per project is current; the others are dropped.
    open.retain(|other, _| other.parent() != path.parent());
    open.insert(
        path,
        Cached {
            stamp,
            file: file.clone(),
        },
    );
    Some(file)
}

/// Write the query file of `graph` (the episode's graph, with its search
/// `index`) to `path`, and return its size; an error of kind `Unsupported`
/// when the graph cannot have one ([`encode`]).
pub fn write(
    path: &Path,
    repository: &str,
    episode: &str,
    graph: &Graph,
    index: &SearchIndex,
) -> std::io::Result<usize> {
    let bytes = encode(&key(repository, episode), graph, index)?;
    crate::query_graph::write_atomic(path, &bytes)?;
    Ok(bytes.len())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;

    /// A directed multigraph shaped like a projected episode: files,
    /// symbols with lines, a document page, parallel and opposite edges
    /// with contexts and provenance, and edges whose extractor direction
    /// (`_src`, `_tgt`) and line (`source_location`, `span`) the identifier
    /// route reads.
    pub(crate) fn sample() -> Graph {
        sample_of(true)
    }

    /// [`sample`], directed or not.
    pub(crate) fn sample_of(directed: bool) -> Graph {
        let mut nodes = vec![
            json!({"id":"src_store_rs","label":"store.rs","file_type":"code","kind":"file","source_file":"src/store.rs","relpath":"src/store.rs","source_location":"L1"}),
            json!({"id":"src_query_rs","label":"query.rs","file_type":"code","kind":"file","source_file":"src/query.rs","relpath":"src/query.rs","source_location":"L1"}),
            json!({"id":"docs_guide_md","label":"Store guide","file_type":"document","kind":"document","source_file":"docs/guide.md","relpath":"docs/guide.md","source_location":"L1"}),
            json!({"id":"store_open","label":"Store.open()","file_type":"code","kind":"method","qualified_name":"store.Store.open","source_file":"src/store.rs","relpath":"src/store.rs","source_location":"L10","stable_id":"s1","revision":"r1"}),
            json!({"id":"store_close","label":"Store.close()","file_type":"code","kind":"method","qualified_name":"store.Store.close","source_file":"src/store.rs","relpath":"src/store.rs","source_location":"L40","rationale":"Closing flushes the write ahead log"}),
            json!({"id":"store","label":"Store","file_type":"code","kind":"class","qualified_name":"store.Store","source_file":"src/store.rs","relpath":"src/store.rs","source_location":"L5-L60","attributes":{"pub":true,"lines":[5,60]}}),
            json!({"id":"query_run","label":"run_query()","file_type":"code","kind":"function","qualified_name":"query.run_query","source_file":"src/query.rs","relpath":"src/query.rs","source_location":"L3","confidence_score":0.75}),
            json!({"id":"query_plan","label":"plan","file_type":"code","kind":"function","source_file":"src/query.rs","relpath":"src/query.rs","source_location":"L30","norm_label":"plan"}),
            json!({"id":"concept_wal","label":"write ahead log","file_type":"concept","kind":"concept"}),
            json!({"id":"日本_語","label":"Ünïcode Störe","file_type":"code","kind":"function","source_file":"src/ünï.rs","relpath":"src/ünï.rs","source_location":"L2","number":1.0}),
        ];
        for i in 0..40 {
            nodes.push(json!({"id":format!("helper_{i}"),"label":format!("helper_{i}()"),"file_type":"code","kind":"function",
                "source_file":format!("src/helpers/h{}.rs", i % 7),"relpath":format!("src/helpers/h{}.rs", i % 7),
                "source_location":format!("L{}", 3 + i * 2)}));
        }
        let mut links = vec![
            json!({"source":"src_store_rs","target":"store","relation":"contains","confidence":"EXTRACTED","context":"code"}),
            json!({"source":"store","target":"store_open","relation":"method","confidence":"EXTRACTED","context":"code","derivation":"ast"}),
            json!({"source":"store","target":"store_close","relation":"method","confidence":"EXTRACTED","context":"code"}),
            json!({"source":"query_run","target":"store_open","relation":"calls","confidence":"INFERRED","context":"code","derivation":"call_resolution","provenance":[{"source_file":"src/query.rs","line":4}],"source_file":"src/query.rs","source_location":"L4"}),
            json!({"source":"query_run","target":"store_open","relation":"calls","confidence":"EXTRACTED","context":"code","source_file":"src/query.rs","span":{"start_line":6}}),
            json!({"source":"store_open","target":"query_run","relation":"references","confidence":"AMBIGUOUS","context":"docs"}),
            json!({"source":"docs_guide_md","target":"store","relation":"references","confidence":"EXTRACTED","context":"docs","derivation":"doc_link"}),
            json!({"source":"store_close","target":"concept_wal","relation":"rationale_for","confidence":"INFERRED"}),
            json!({"source":"query_plan","target":"query_run","relation":"calls","confidence":"EXTRACTED","context":"code","weight":2}),
            json!({"source":"src_query_rs","target":"query_run","relation":"contains","confidence":"EXTRACTED","context":"code"}),
            json!({"source":"src_query_rs","target":"query_plan","relation":"contains","confidence":"EXTRACTED","context":"code"}),
            json!({"source":"日本_語","target":"store","relation":"uses","confidence":"EXTRACTED","context":null}),
            // Stored the other way round: the extractor's direction is kept.
            json!({"source":"store_close","target":"query_plan","relation":"calls","confidence":"EXTRACTED","context":"code","_src":"query_plan","_tgt":"store_close","source_file":"src\\query.rs","source_location":"L31"}),
        ];
        for i in 0..40 {
            links.push(json!({"source":format!("helper_{i}"),"target":if i % 3 == 0 {"store_open"} else {"query_run"},
                "relation":"calls","confidence":"EXTRACTED","context":if i % 5 == 0 {"tests"} else {"code"}}));
        }
        Graph::from_node_link(
            &json!({"directed":directed,"multigraph":true,"graph":{},"nodes":nodes,"links":links}),
            "links",
        )
        .unwrap()
    }

    pub(crate) fn written(graph: &Graph, dir: &Path) -> QueryFile {
        let index = SearchIndex::new(graph);
        let path = dir.join("e.query");
        write(&path, "p", "e", graph, &index).unwrap();
        assert!(current(&path, &key("p", "e")));
        assert!(!current(&path, &key("p", "other")));
        QueryFile::open(&path, &key("p", "e")).unwrap()
    }

    #[test]
    fn scoring_and_seeds_match_the_resident_graph() {
        use graphicle::serve_search::{pick_seeds_in_order, pick_seeds_on, score_on, score_query};
        let dir = tempfile::TempDir::new().unwrap();
        let graph = sample();
        let index = SearchIndex::new(&graph);
        let file = written(&graph, dir.path());
        assert_eq!(file.len(), graph.nodes().len());
        for i in 0..graph.nodes().len() {
            assert_eq!(file.text(i), index.text(i).fields(), "node {i}");
        }
        for question in [
            "Where does Store.open run?",
            "store open close",
            "run_query plan",
            "helper_12 helper",
            "write ahead log",
            "ünïcode störe",
            "s",
            "zz qq",
            "the store of the query",
            "helper",
            "",
        ] {
            let terms = graphicle::query_terms::query_terms(question);
            for per_term in [true, false] {
                let resident = score_query(&graph, &index, &terms, per_term);
                let saved = score_on(&file, &terms, per_term);
                assert_eq!(resident.ranked, saved.ranked, "{question}");
                assert_eq!(
                    resident.best_seed_by_term, saved.best_seed_by_term,
                    "{question}"
                );
                assert_eq!(resident.term_order, saved.term_order, "{question}");
                for order in [&resident.term_order[..], &[]] {
                    assert_eq!(
                        pick_seeds_in_order(
                            Some(&graph),
                            &resident.ranked,
                            3,
                            0.2,
                            &resident.best_seed_by_term,
                            order
                        ),
                        pick_seeds_on(
                            &file,
                            &saved.ranked,
                            3,
                            0.2,
                            &saved.best_seed_by_term,
                            order
                        ),
                        "{question}"
                    );
                }
            }
        }
        assert!(!file.corrupt());
    }

    #[test]
    fn records_match_the_resident_graph() {
        for directed in [true, false] {
            records_match(&sample_of(directed));
        }
    }

    /// An edge as the arms and the identifier route read it.
    type Record = (
        Option<Value>,
        Option<Value>,
        Option<Value>,
        Vec<String>,
        (usize, usize),
        (Option<String>, Option<usize>),
    );

    fn records_match(graph: &Graph) {
        let dir = tempfile::TempDir::new().unwrap();
        let file = written(graph, dir.path());
        let nodes = graph.nodes();
        let records = |edges: Vec<&graphicle::graph::Edge>| -> Vec<Record> {
            edges
                .into_iter()
                .map(|edge| {
                    (
                        edge.attributes.get("context").cloned(),
                        edge.attributes.get("relation").cloned(),
                        edge.attributes.get("derivation").cloned(),
                        crate::code_graph_arm::edge_files(&edge.attributes),
                        crate::identifier_route::reference_ends(graph, edge),
                        crate::identifier_route::reference_site(&edge.attributes),
                    )
                })
                .collect()
        };
        let read = |ids: &[u32]| -> Vec<Record> {
            file.edges(ids)
                .unwrap()
                .into_iter()
                .map(|e| {
                    (
                        e.context,
                        e.relation,
                        e.derivation,
                        e.files,
                        (e.source, e.target),
                        (e.site_file, e.site_line),
                    )
                })
                .collect()
        };
        for (i, node) in nodes.iter().enumerate() {
            let id = node.id.as_str().unwrap();
            assert_eq!(file.id(i).unwrap(), id);
            assert_eq!(file.node_index(id).unwrap(), Some(i));
            assert_eq!(
                file.attributes(i).unwrap(),
                node.attributes.to_map(),
                "{id}"
            );
            assert_eq!(
                file.is_document(i).unwrap(),
                crate::code_graph_arm::document_node(&node.attributes)
            );
            let adjacency = file.adjacency(i).unwrap();
            let mut neighbours: BTreeSet<usize> = graph.neighbor_indices(i, false).collect();
            if graph.is_directed() {
                neighbours.extend(graph.neighbor_indices(i, true));
            }
            assert_eq!(adjacency.neighbours(), neighbours, "{id}");
            for (j, other) in nodes.iter().enumerate() {
                assert_eq!(
                    read(adjacency.out_to(j)),
                    records(graph.edges_between(&node.id, &other.id)),
                    "{id} -> {}",
                    other.id
                );
                assert_eq!(
                    read(adjacency.in_from(j)),
                    records(graph.edges_between(&other.id, &node.id)),
                    "{} -> {id}",
                    other.id
                );
            }
            // Every edge of the node, as `out_edges` and `in_edges` list them.
            let mut incident: Vec<Record> = records(
                graph
                    .out_edges(i)
                    .into_iter()
                    .chain(if graph.is_directed() {
                        graph.in_edges(i)
                    } else {
                        Vec::new()
                    })
                    .map(|(.., edge)| edge)
                    .collect(),
            );
            let all: Vec<u32> = adjacency.all().into_iter().collect();
            let mut found = read(&all);
            let key = |r: &Record| format!("{r:?}");
            incident.sort_by_key(key);
            incident.dedup();
            found.sort_by_key(key);
            found.dedup();
            assert_eq!(found, incident, "{id}");
        }
        assert_eq!(file.node_index("missing").unwrap(), None);
        let starts = crate::code_graph_arm::symbol_starts(graph);
        for (path, lines) in &starts {
            assert_eq!(file.starts(path).unwrap().as_ref(), Some(lines), "{path}");
        }
        assert_eq!(file.starts("src/none.rs").unwrap(), None);
        let symbols = crate::identifier_arm::symbols_of(graph);
        for (position, (index, name, relpath, start)) in symbols.iter().enumerate() {
            assert_eq!(
                file.symbol(position).unwrap(),
                (*index, name.as_str(), relpath.as_str(), *start)
            );
            let last = name.rsplit('.').next().unwrap_or(name);
            assert!(file.symbols_ending(last).unwrap().contains(&position));
        }
        assert!(file.symbols_ending("nothing_ends_so").unwrap().is_empty());
        assert!(!file.corrupt());
    }

    /// The bytes of section `s` of a written query file, as (offset, len).
    pub(crate) fn section_of(bytes: &[u8], s: usize) -> (usize, usize) {
        let length = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;
        let at = 12 + length + s * 16;
        let read = |at: usize| u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap()) as usize;
        (read(at), read(at + 8))
    }

    /// `bytes` with one byte flipped in every page of section `s`.
    pub(crate) fn damaged(bytes: &[u8], s: usize) -> Vec<u8> {
        let (offset, len) = section_of(bytes, s);
        let mut out = bytes.to_vec();
        let mut at = offset;
        while at < offset + len {
            out[at] ^= 0x5a;
            at += PAGE;
        }
        out
    }

    /// A file with another key is not read and left alone (another build's,
    /// or another episode's). A file with this key cut short is not read
    /// either, and once the cut is past the key it is removed, so that the
    /// next sync writes it again; [`current`] counts none of them.
    #[test]
    fn another_key_or_a_truncated_file_is_not_read() {
        let dir = tempfile::TempDir::new().unwrap();
        let graph = sample();
        let file = written(&graph, dir.path());
        drop(file);
        let path = dir.path().join("e.query");
        let key = key("p", "e");
        assert!(QueryFile::open(&path, &crate::query_file::key("p", "other")).is_none());
        assert!(path.exists());
        let bytes = std::fs::read(&path).unwrap();
        let past_key = MAGIC.len() + 4 + key.len();
        for cut in [
            0,
            7,
            9,
            past_key - 1,
            past_key + 1,
            bytes.len() / 2,
            bytes.len() - 1,
        ] {
            std::fs::write(&path, &bytes[..cut]).unwrap();
            assert!(!current(&path, &key), "cut {cut}");
            assert!(QueryFile::open(&path, &key).is_none(), "cut {cut}");
            assert_eq!(path.exists(), cut < past_key, "cut {cut}");
        }
        // A byte of the key changed: another key, left alone.
        let mut other = bytes.clone();
        other[12 + 3] ^= 1;
        std::fs::write(&path, &other).unwrap();
        assert!(!current(&path, &key));
        assert!(QueryFile::open(&path, &key).is_none());
        assert!(path.exists());
    }

    /// A header damaged past the key (the section table, the page
    /// checksums, the header's own checksum, the body offset) is not
    /// current, and the open that finds it removes the file. A damaged page
    /// leaves the header current: the read that reaches it finds it.
    #[test]
    fn a_damaged_header_is_not_current_and_is_removed() {
        let dir = tempfile::TempDir::new().unwrap();
        let graph = sample();
        drop(written(&graph, dir.path()));
        let path = dir.path().join("e.query");
        let key = key("p", "e");
        let bytes = std::fs::read(&path).unwrap();
        assert!(current(&path, &key));
        let table = MAGIC.len() + 4 + key.len();
        let after = table + SECTIONS * 16;
        let body = u64::from_le_bytes(bytes[after..after + 8].try_into().unwrap()) as usize;
        for (what, at) in [
            ("section table", table + 5 * 16 + 1),
            ("body offset", after),
            ("page count", after + 8),
            ("page checksums", after + 16),
            ("header checksum", body - 8),
        ] {
            let mut damaged = bytes.clone();
            damaged[at] ^= 0x5a;
            std::fs::write(&path, &damaged).unwrap();
            assert!(!current(&path, &key), "{what}");
            assert!(QueryFile::open(&path, &key).is_none(), "{what}");
            assert!(!path.exists(), "{what}: not removed");
        }
        // A file another write put in place since the open read it is not
        // removed: only the damaged file that was read.
        let mut damaged = bytes.clone();
        damaged[after + 16] ^= 0x5a;
        std::fs::write(&path, &damaged).unwrap();
        let read = crate::query_graph::identity(&std::fs::metadata(&path).unwrap());
        crate::query_graph::write_atomic(&path, &bytes).unwrap();
        crate::query_graph::remove_damaged(&path, read);
        assert!(path.exists() && current(&path, &key));
        // A damaged page: the header is current, the page is found later.
        let mut page = bytes.clone();
        page[body + 1] ^= 0x5a;
        std::fs::write(&path, &page).unwrap();
        assert!(current(&path, &key));
        assert!(QueryFile::open(&path, &key).is_some() || !path.exists());
    }

    /// The writer refuses what the reader would refuse, so that a file it
    /// writes is never found corrupt and written again at every sync: a node
    /// id that is not a string, and an attribute value nested more than
    /// [`MAX_DEPTH`] levels deep. A value nested exactly that deep is
    /// written and read back.
    #[test]
    fn the_writer_refuses_what_the_reader_would() {
        let numbered = Graph::from_node_link(
            &json!({"directed":false,"multigraph":false,"graph":{},"nodes":[{"id":1,"label":"one"}],"links":[]}),
            "links",
        )
        .unwrap();
        let index = SearchIndex::new(&numbered);
        let refused = encode(b"k", &numbered, &index).unwrap_err();
        assert_eq!(refused.kind(), std::io::ErrorKind::Unsupported);
        let nested = |levels: usize| {
            let mut value = json!("bottom");
            for _ in 0..levels {
                value = json!([value]);
            }
            value
        };
        let with = |value: Value| {
            Graph::from_node_link(
                &json!({"directed":false,"multigraph":false,"graph":{},
                    "nodes":[{"id":"a","label":"a","deep":value},{"id":"b","label":"b"}],
                    "links":[{"source":"a","target":"b","relation":"calls","context":value}]}),
                "links",
            )
            .unwrap()
        };
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("e.query");
        let deepest = with(nested(MAX_DEPTH));
        write(&path, "p", "e", &deepest, &SearchIndex::new(&deepest)).unwrap();
        let file = QueryFile::open(&path, &key("p", "e")).unwrap();
        assert_eq!(file.attributes(0).unwrap()["deep"], nested(MAX_DEPTH));
        let edges: Vec<u32> = file.adjacency(0).unwrap().all().into_iter().collect();
        assert_eq!(
            file.edges(&edges).unwrap()[0].context,
            Some(nested(MAX_DEPTH))
        );
        assert!(!file.corrupt());
        let deeper = with(nested(MAX_DEPTH + 1));
        let refused = encode(b"k", &deeper, &SearchIndex::new(&deeper)).unwrap_err();
        assert_eq!(refused.kind(), std::io::ErrorKind::Unsupported);
        assert!(refused.to_string().contains("nested more than 64 levels"));
    }

    /// The checksums find damage, not a file made to pass them: a file
    /// whose checksums are whole but whose counts claim more than what is
    /// left, whose values nest too deep or whose indices name nothing is
    /// corrupt to the read that reaches it, with no panic, no large
    /// allocation and no deep recursion.
    #[test]
    fn hostile_content_with_whole_checksums_is_corrupt() {
        let dir = tempfile::TempDir::new().unwrap();
        let graph = sample();
        drop(written(&graph, dir.path()));
        let path = dir.path().join("e.query");
        let pristine = std::fs::read(&path).unwrap();
        let u32_at =
            |at: usize| u32::from_le_bytes(pristine[at..at + 4].try_into().unwrap()) as usize;
        let start = |s: usize| section_of(&pristine, s).0;
        let row = |i: usize, field: usize| u32_at(start(NODES) + (i * ROW + field) * 4);
        let n = u32_at(start(META) + 2 * 4);
        assert_eq!(n, graph.nodes().len());
        let var = |value: u64| {
            let mut out = Vec::new();
            put_var(&mut out, value);
            out
        };
        // `bytes` over section `s` from `at`, with the checksums written
        // again: the file opens.
        let hostile = |s: usize, at: usize, bytes: &[u8]| -> QueryFile {
            let mut out = pristine.clone();
            let (offset, len) = section_of(&out, s);
            assert!(at + bytes.len() <= len, "section {s}");
            out[offset + at..offset + at + bytes.len()].copy_from_slice(bytes);
            assert!(reseal(&mut out));
            std::fs::write(&path, &out).unwrap();
            QueryFile::open(&path, &key("p", "e")).expect("whole checksums")
        };
        let corrupt = |file: &QueryFile, read: bool, what: &str| {
            assert!(read, "{what}: the read succeeded");
            assert!(file.corrupt(), "{what}: not marked");
            assert!(!path.exists(), "{what}: not removed");
        };
        // Counts beyond what is left of their section.
        let file = hostile(ATTRS, row(0, 12), &var(1 << 40));
        corrupt(&file, file.attributes(0).is_err(), "attribute count");
        let file = hostile(ADJ, row(0, 13), &var(1 << 40));
        corrupt(&file, file.adjacency(0).is_err(), "neighbour count");
        let first_edge = u32_at(start(EDGE_OFF));
        let mut record = vec![0, 0, 0];
        record.extend(var(1 << 40));
        let file = hostile(EDGES, first_edge, &record);
        corrupt(&file, file.edge(0).is_err(), "edge file count");
        let mut paths: Vec<String> = crate::code_graph_arm::symbol_starts(&graph)
            .into_keys()
            .collect();
        paths.sort();
        let file = hostile(STARTS, u32_at(start(STARTS_TAB) + 4), &var(1 << 40));
        corrupt(&file, file.starts(&paths[0]).is_err(), "start line count");
        // A count that fits what is left, whose items (two bytes each at
        // least) run out at the end of the section.
        let last = row(n - 1, 12);
        let left = section_of(&pristine, ATTRS).1 - last;
        let file = hostile(ATTRS, last, &var((left / 2) as u64));
        corrupt(&file, file.attributes(n - 1).is_err(), "items run out");
        // A varint longer than 64 bits.
        let file = hostile(ADJ, row(0, 13), &[0xff; 10]);
        corrupt(&file, file.adjacency(0).is_err(), "long varint");
        // A value nested deeper than the reader reads: one attribute, then
        // lists of one item each.
        let mut deep = var(1);
        deep.extend(var(0));
        for _ in 0..MAX_DEPTH + 2 {
            deep.extend([5, 1]);
        }
        deep.push(0);
        let file = hostile(ATTRS, row(0, 12), &deep);
        corrupt(&file, file.attributes(0).is_err(), "nesting");
        // A gram whose postings claim more nodes than the graph has, and
        // postings that do not ascend.
        let first_gram = u64::from_le_bytes(
            pristine[start(GRAM_KEYS)..start(GRAM_KEYS) + 8]
                .try_into()
                .unwrap(),
        );
        let file = hostile(GRAM_POS, 0, &(n as u32 + 1).to_le_bytes());
        let found = file.gram(first_gram).unwrap().unwrap();
        corrupt(&file, file.postings(found).is_err(), "postings count");
        let file = hostile(POSTINGS, 0, &[1, 0]);
        corrupt(&file, file.postings((2, 0)).is_err(), "postings order");
        // A bad entry in the node ids in order: the first one a lookup
        // reads (the middle one) names no node.
        let file = hostile(IDS, (n / 2) * 4, &(n as u32).to_le_bytes());
        corrupt(&file, file.node_index("store").is_err(), "node id entry");
        // A symbol bucket larger than the positions table.
        let file = hostile(LAST_TAB, 8, &u32::MAX.to_le_bytes());
        let name = file
            .string(u32_at(start(LAST_TAB)) as u64)
            .unwrap()
            .to_string();
        corrupt(&file, file.symbols_ending(&name).is_err(), "symbol bucket");
        // A symbol whose node is out of range.
        let file = hostile(SYMBOLS, 0, &(n as u32).to_le_bytes());
        corrupt(&file, file.symbol(0).is_err(), "symbol node");
        // A string that ends before it starts, and one past the blob.
        let file = hostile(STR_OFF, 0, &u32::MAX.to_le_bytes());
        corrupt(&file, file.string(0).is_err(), "string start");
        let strings = u32_at(start(META) + 3 * 4);
        let file = hostile(STR_OFF, strings * 4, &u32::MAX.to_le_bytes());
        corrupt(
            &file,
            file.string(strings as u64 - 1).is_err(),
            "string end",
        );
        // An edge whose end is no node: no values, no files, then the end.
        let mut record = vec![0, 0, 0, 0];
        record.extend(var(n as u64));
        let file = hostile(EDGES, first_edge, &record);
        corrupt(&file, file.edge(0).is_err(), "edge end");
        // A node count no table holds: the file does not open.
        let mut out = pristine.clone();
        let meta = start(META);
        out[meta + 8..meta + 12].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(reseal(&mut out));
        std::fs::write(&path, &out).unwrap();
        assert!(QueryFile::open(&path, &key("p", "e")).is_none());
        // The pristine bytes, resealed, read cleanly.
        let mut out = pristine.clone();
        assert!(reseal(&mut out));
        assert_eq!(out, pristine);
    }

    /// A damaged page is found by the first read that reaches it: the file
    /// is marked corrupt and removed, so the next sync writes it again.
    #[test]
    fn a_damaged_page_marks_the_file_corrupt_and_removes_it() {
        let dir = tempfile::TempDir::new().unwrap();
        let graph = sample();
        drop(written(&graph, dir.path()));
        let path = dir.path().join("e.query");
        let bytes = std::fs::read(&path).unwrap();
        let reopen = |s: usize| {
            std::fs::write(&path, damaged(&bytes, s)).unwrap();
            QueryFile::open(&path, &key("p", "e")).unwrap()
        };
        // The postings: a needle's rare trigrams read them.
        let file = reopen(POSTINGS_SECTION);
        let _ = file.candidates(&["plan".to_string()], 0.1);
        assert!(file.corrupt());
        assert!(!path.exists());
        // The code symbols.
        let file = reopen(SYMBOLS_SECTION);
        assert!(file.symbol(0).is_err());
        assert!(file.corrupt());
        assert!(!path.exists());
        // The node ids in order.
        let file = reopen(IDS_SECTION);
        assert!(file.node_index("store").is_err());
        assert!(file.corrupt() && !path.exists());
        // Once marked, every read fails.
        assert!(file.attributes(0).is_err());
        // An intact file reads every page cleanly.
        std::fs::write(&path, &bytes).unwrap();
        let file = QueryFile::open(&path, &key("p", "e")).unwrap();
        for i in 0..file.len() {
            file.attributes(i).unwrap();
            file.adjacency(i).unwrap();
        }
        assert!(!file.corrupt() && path.exists());
    }

    /// A count or an index out of range is refused before anything is
    /// reserved for it.
    #[test]
    fn counts_and_indices_out_of_range_are_corrupt() {
        let dir = tempfile::TempDir::new().unwrap();
        let graph = sample();
        let file = written(&graph, dir.path());
        assert!(file.symbol(usize::MAX).is_err());
        let file = written(&graph, dir.path());
        assert!(file.edge(u32::MAX).is_err());
        let file = written(&graph, dir.path());
        assert!(file.attributes(file.len()).is_err());
        let file = written(&graph, dir.path());
        assert!(file.postings((usize::MAX / 2, 0)).is_err());
        let file = written(&graph, dir.path());
        assert!(file.string(u64::MAX).is_err());
        assert!(file.corrupt());
    }

    #[test]
    fn a_change_within_one_word_always_changes_the_sum() {
        let page: Vec<u8> = (0..PAGE as u32).map(|i| (i * 7 + 3) as u8).collect();
        let base = sum(&page);
        for at in [0, 1, 7, 8, 31, 32, 1000, PAGE - 1] {
            for bit in [0x01u8, 0x80, 0xff] {
                let mut changed = page.clone();
                changed[at] ^= bit;
                assert_ne!(sum(&changed), base, "{at} {bit}");
            }
        }
        assert_ne!(sum(&page[..PAGE - 1]), base);
    }
}
