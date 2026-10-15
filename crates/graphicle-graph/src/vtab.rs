//! Process-wide hash-consed table of scalar JSON values.
//!
//! Graph records repeat their values heavily: a Kubernetes graph holds about
//! 12.8 million string values but only about 510 thousand distinct ones. Each
//! distinct scalar (`null`, a boolean, a number by its exact text, a string)
//! is stored here once and records carry its four-byte [`VId`].
//!
//! - Values are keyed by kind and exact text, so number `1` and `1.0`, `-0.0`
//!   and `0`, and string `"1.0"` against number `1.0` all stay distinct: a
//!   record always writes back the very bytes it was given.
//! - [`VId`] has no `Ord`, so the (thread-timing dependent) numbering can never
//!   order anything that reaches an output.
//! - Storage is append-only and never freed: [`resolve`] hands out
//!   `&'static Value` with no lock. The table grows only with new distinct
//!   values, so an unchanged repository adds nothing on repeated updates.
//! - Each thread remembers the ids it interned last in a small direct-mapped
//!   cache, so the repeated values of consecutive records (relation,
//!   confidence, source file, endpoint) are found without the shard lock.
use hashbrown::HashTable;
use serde_json::Value;
use std::{
    cell::Cell,
    hash::{BuildHasher, Hasher},
    mem::MaybeUninit,
    sync::{
        Mutex, PoisonError,
        atomic::{AtomicPtr, AtomicU64, Ordering},
    },
};

/// The id of one interned scalar. Copy, Eq and Hash, deliberately not Ord.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct VId(u32);

impl VId {
    pub const NULL: Self = Self(0);
    pub const FALSE: Self = Self(1);
    pub const TRUE: Self = Self(2);
    /// Ids stay below this bound, so a record may use the top bit as a tag.
    pub(crate) const LIMIT: u32 = 1 << 31;
    pub(crate) fn raw(self) -> u32 {
        self.0
    }
    pub(crate) fn from_raw(raw: u32) -> Self {
        debug_assert!(raw < Self::LIMIT);
        Self(raw)
    }
    /// The value this id stands for.
    pub fn value(self) -> &'static Value {
        resolve(self)
    }
}

const RESERVED: u32 = 3;
const SHARD_BITS: u32 = 6;
const SHARDS: usize = 1 << SHARD_BITS;
/// The first chunk of a shard holds `1 << BASE_BITS` values; chunk `c` holds
/// `1 << (BASE_BITS + c)`, so a shard of `n` values allocates under `2n`.
const BASE_BITS: u32 = 6;
/// Per-shard capacity: local index < 2^25, so an id stays below 2^31.
const LOCAL_LIMIT: u32 = (VId::LIMIT - RESERVED) >> SHARD_BITS;
const CHUNKS: usize = 20;

static NULL: Value = Value::Null;
static FALSE: Value = Value::Bool(false);
static TRUE: Value = Value::Bool(true);

const TAG_STRING: u8 = 1;
const TAG_NUMBER: u8 = 2;

struct Inner {
    index: HashTable<u32>,
    len: u32,
    text_bytes: u64,
}

struct Shard {
    inner: Mutex<Inner>,
    chunks: [AtomicPtr<Value>; CHUNKS],
}

impl Shard {
    const fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                index: HashTable::new(),
                len: 0,
                text_bytes: 0,
            }),
            chunks: [const { AtomicPtr::new(std::ptr::null_mut()) }; CHUNKS],
        }
    }
    /// The value at `local`, which must have been published (its id handed out).
    fn get(&self, local: u32) -> &'static Value {
        let (chunk, offset) = locate(local);
        let base = self.chunks[chunk].load(Ordering::Acquire);
        debug_assert!(!base.is_null());
        // SAFETY: an id is handed out only after its slot was written under the
        // shard lock and its chunk pointer published (Release). Chunks are
        // leaked, never moved or freed, and a written slot is never written
        // again, so the reference stays valid and immutable for the process.
        unsafe { &*base.add(offset) }
    }
    /// Write `value` at `local`. The caller holds the shard lock and `local` is
    /// the shard's next free index.
    fn put(&self, local: u32, value: Value) {
        let (chunk, offset) = locate(local);
        let mut base = self.chunks[chunk].load(Ordering::Acquire);
        if base.is_null() {
            let len = 1usize << (BASE_BITS as usize + chunk);
            let block: Box<[MaybeUninit<Value>]> = Box::new_uninit_slice(len);
            base = Box::leak(block).as_mut_ptr().cast::<Value>();
            self.chunks[chunk].store(base, Ordering::Release);
        }
        // SAFETY: `offset` is inside the chunk, the slot is unwritten (indices
        // are handed out once, under the lock) and no reader can hold its id yet.
        unsafe { base.add(offset).write(value) }
    }
}

static TABLE: [Shard; SHARDS] = [const { Shard::new() }; SHARDS];

static WRITE_BACKS: AtomicU64 = AtomicU64::new(0);
static SPILLS: AtomicU64 = AtomicU64::new(0);

fn locate(local: u32) -> (usize, usize) {
    let j = local as u64 + (1u64 << BASE_BITS);
    let chunk = (63 - j.leading_zeros() - BASE_BITS) as usize;
    let offset = (j - (1u64 << (BASE_BITS as usize + chunk))) as usize;
    (chunk, offset)
}

fn hash(tag: u8, text: &str) -> u64 {
    let mut h = foldhash::fast::FixedState::with_seed(0x6772_6170_6869_636c).build_hasher();
    h.write_u8(tag);
    h.write(text.as_bytes());
    h.finish()
}

/// Kind and exact text of an interned value (only strings and numbers are
/// stored in shards).
fn key_of(value: &Value) -> (u8, &str) {
    match value {
        Value::String(s) => (TAG_STRING, s),
        Value::Number(n) => (TAG_NUMBER, n.as_str()),
        _ => unreachable!("only strings and numbers live in shards"),
    }
}

/// The shard of a hash. hashbrown takes a table's bucket from the low bits
/// and its 7-bit control tag from the top bits, so the shard comes from the
/// middle: a shard's entries keep the full tag entropy (one false candidate
/// in 128 per probe instead of about one in two).
fn shard_of(h: u64) -> usize {
    ((h >> 32) as usize) & (SHARDS - 1)
}

fn encode(shard: usize, local: u32) -> VId {
    VId(RESERVED + ((local << SHARD_BITS) | shard as u32))
}

fn lock(shard: &Shard) -> std::sync::MutexGuard<'_, Inner> {
    shard.inner.lock().unwrap_or_else(PoisonError::into_inner)
}

fn find(inner: &Inner, shard: &Shard, h: u64, tag: u8, text: &str) -> Option<u32> {
    inner
        .index
        .find(h, |&local| key_of(shard.get(local)) == (tag, text))
        .copied()
}

/// Store a value that `find` just missed, under the same lock.
fn insert_new(inner: &mut Inner, s: usize, h: u64, value: Value) -> VId {
    let shard = &TABLE[s];
    let local = inner.len;
    assert!(local < LOCAL_LIMIT, "value table full");
    inner.text_bytes += key_of(&value).1.len() as u64;
    shard.put(local, value);
    inner.len += 1;
    inner.index.insert_unique(h, local, |&l| {
        let (t, x) = key_of(shard.get(l));
        hash(t, x)
    });
    encode(s, local)
}

/// Slots of the per-thread cache of recently interned ids.
const RECENT: usize = 1024;
const EMPTY: u32 = u32::MAX;

thread_local! {
    /// Ids this thread interned or found last, by hash. Only published string
    /// and number ids are stored, and a hit is checked by kind and text, so it
    /// returns exactly the id the shard would.
    static RECENT_IDS: [Cell<u32>; RECENT] = const { [const { Cell::new(EMPTY) }; RECENT] };
}

fn recent_slot(h: u64) -> usize {
    ((h >> 40) as usize) & (RECENT - 1)
}

fn recent(h: u64, tag: u8, text: &str) -> Option<VId> {
    RECENT_IDS.with(|ids| {
        let raw = ids[recent_slot(h)].get();
        (raw != EMPTY && key_of(resolve(VId(raw))) == (tag, text)).then_some(VId(raw))
    })
}

fn remember(h: u64, id: VId) {
    RECENT_IDS.with(|ids| ids[recent_slot(h)].set(id.0));
}

/// Intern a string or number, moving it in only when it is new.
fn intern_owned(value: Value) -> VId {
    let (tag, text) = key_of(&value);
    let h = hash(tag, text);
    if let Some(id) = recent(h, tag, text) {
        return id;
    }
    let s = shard_of(h);
    let id = {
        let mut inner = lock(&TABLE[s]);
        match find(&inner, &TABLE[s], h, tag, text) {
            Some(local) => encode(s, local),
            None => insert_new(&mut inner, s, h, value),
        }
    };
    remember(h, id);
    id
}

/// Intern a string or number by its kind and text, building it only when new.
fn intern_with(tag: u8, text: &str, make: impl FnOnce() -> Value) -> VId {
    let h = hash(tag, text);
    if let Some(id) = recent(h, tag, text) {
        return id;
    }
    let s = shard_of(h);
    let id = {
        let mut inner = lock(&TABLE[s]);
        match find(&inner, &TABLE[s], h, tag, text) {
            Some(local) => encode(s, local),
            None => insert_new(&mut inner, s, h, make()),
        }
    };
    remember(h, id);
    id
}

/// Whether `value` is a scalar the table stores (everything but arrays and
/// objects).
pub fn is_scalar(value: &Value) -> bool {
    !matches!(value, Value::Array(_) | Value::Object(_))
}

/// The id of a scalar `value`, moving it in only when it is new; arrays and
/// objects come back unchanged.
pub fn intern(value: Value) -> Result<VId, Value> {
    match &value {
        Value::Null => Ok(VId::NULL),
        Value::Bool(false) => Ok(VId::FALSE),
        Value::Bool(true) => Ok(VId::TRUE),
        Value::String(_) | Value::Number(_) => Ok(intern_owned(value)),
        Value::Array(_) | Value::Object(_) => Err(value),
    }
}

/// The id of a scalar `value`, cloning it only when it is new.
pub fn intern_ref(value: &Value) -> Option<VId> {
    match value {
        Value::Null => Some(VId::NULL),
        Value::Bool(false) => Some(VId::FALSE),
        Value::Bool(true) => Some(VId::TRUE),
        Value::String(s) => Some(intern_with(TAG_STRING, s, || value.clone())),
        Value::Number(n) => Some(intern_with(TAG_NUMBER, n.as_str(), || value.clone())),
        Value::Array(_) | Value::Object(_) => None,
    }
}

/// The id of the string `text`; allocates nothing when it is already present.
pub fn intern_str(text: &str) -> VId {
    intern_with(TAG_STRING, text, || Value::String(text.to_owned()))
}

/// The value an id stands for. Takes no lock.
pub fn resolve(id: VId) -> &'static Value {
    match id.0 {
        0 => &NULL,
        1 => &FALSE,
        2 => &TRUE,
        raw => {
            let r = raw - RESERVED;
            TABLE[(r as usize) & (SHARDS - 1)].get(r >> SHARD_BITS)
        }
    }
}

/// Count one write-back through a record's mutable guard (profile only).
pub(crate) fn note_write_back() {
    if crate::profile::enabled() {
        WRITE_BACKS.fetch_add(1, Ordering::Relaxed);
    }
}

/// Count one array or object stored beside a record's slots (profile only).
pub(crate) fn note_spill() {
    if crate::profile::enabled() {
        SPILLS.fetch_add(1, Ordering::Relaxed);
    }
}

/// Table size and record counters, for `GRAPHICLE_PROFILE`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Distinct interned strings and numbers.
    pub values: u64,
    /// Their total text bytes.
    pub text_bytes: u64,
    /// Bytes reserved for the value slots (chunks), touched or not.
    pub slot_bytes: u64,
    /// Guard write-backs (`get_mut`, `entry`) since start (profile only).
    pub write_backs: u64,
    /// Arrays and objects stored beside record slots since start (profile only).
    pub spills: u64,
}

pub fn stats() -> Stats {
    let mut out = Stats {
        write_backs: WRITE_BACKS.load(Ordering::Relaxed),
        spills: SPILLS.load(Ordering::Relaxed),
        ..Stats::default()
    };
    for shard in &TABLE {
        let inner = lock(shard);
        out.values += u64::from(inner.len);
        out.text_bytes += inner.text_bytes;
        let chunks = shard
            .chunks
            .iter()
            .take_while(|c| !c.load(Ordering::Acquire).is_null())
            .count();
        let slots = (1u64 << (BASE_BITS as u64 + chunks as u64)) - (1u64 << BASE_BITS);
        out.slot_bytes += slots * std::mem::size_of::<Value>() as u64;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn chunk_layout_covers_every_index_once() {
        let mut expected = (0usize, 0usize);
        for local in 0..100_000u32 {
            let got = locate(local);
            assert_eq!(got, expected, "local {local}");
            expected.1 += 1;
            if expected.1 == 1 << (BASE_BITS as usize + expected.0) {
                expected = (expected.0 + 1, 0);
            }
        }
        assert!(locate(LOCAL_LIMIT - 1).0 < CHUNKS);
    }

    #[test]
    fn kinds_and_exact_texts_stay_distinct() {
        let values: Vec<Value> = [
            "1",
            "1.0",
            "-0.0",
            "0",
            "1e-7",
            "1E-7",
            "0.55",
            "100000000000000000000000",
        ]
        .iter()
        .map(|t| serde_json::from_str::<Value>(t).unwrap())
        .chain(
            ["1", "1.0", "-0.0", "", "é", "日本"]
                .iter()
                .map(|s| json!(s)),
        )
        .chain([
            json!(null),
            json!(true),
            json!(false),
            json!(0.55),
            json!(-0.0),
        ])
        .collect();
        let ids: Vec<VId> = values.iter().map(|v| intern(v.clone()).unwrap()).collect();
        for (i, (a, ia)) in values.iter().zip(&ids).enumerate() {
            assert_eq!(
                serde_json::to_string(resolve(*ia)).unwrap(),
                serde_json::to_string(a).unwrap()
            );
            for (b, ib) in values.iter().zip(&ids).skip(i + 1) {
                let same = serde_json::to_string(a).unwrap() == serde_json::to_string(b).unwrap();
                assert_eq!(same, ia == ib, "{a} vs {b}");
            }
        }
        assert_eq!(intern_str("é"), intern(json!("é")).unwrap());
        assert_eq!(intern_ref(&json!("日本")), Some(intern_str("日本")));
        assert!(intern(json!([1])).is_err());
        assert_eq!(intern_ref(&json!({"a": 1})), None);
    }

    #[test]
    fn recent_cache_collisions_return_the_table_ids() {
        // Texts whose hashes share one cache slot, as strings and as numbers.
        let target = recent_slot(hash(TAG_STRING, "vtab-recent-0"));
        let texts: Vec<String> = (0u64..)
            .map(|i| format!("{i}"))
            .filter(|t| {
                recent_slot(hash(TAG_STRING, &format!("vtab-recent-{t}"))) == target
                    || recent_slot(hash(TAG_NUMBER, t)) == target
            })
            .take(6)
            .collect();
        let values: Vec<Value> = texts
            .iter()
            .flat_map(|t| {
                [
                    json!(format!("vtab-recent-{t}")),
                    serde_json::from_str::<Value>(t).unwrap(),
                    json!(t),
                ]
            })
            .collect();
        let first: Vec<VId> = values.iter().map(|v| intern(v.clone()).unwrap()).collect();
        for round in 0..3 {
            for (v, id) in values.iter().zip(&first).rev() {
                let again = match round {
                    0 => intern(v.clone()).unwrap(),
                    1 => intern_ref(v).unwrap(),
                    _ => v
                        .as_str()
                        .map_or_else(|| intern_ref(v).unwrap(), intern_str),
                };
                assert_eq!(again, *id, "{v}");
                assert_eq!(resolve(again), v);
            }
        }
        // A number and a string with the same text never share an id.
        for t in &texts {
            assert_ne!(
                intern_str(t),
                intern(serde_json::from_str::<Value>(t).unwrap()).unwrap()
            );
        }
    }

    #[test]
    fn concurrent_interning_agrees() {
        let threads: Vec<_> = (0..8)
            .map(|t| {
                std::thread::spawn(move || {
                    (0..5000)
                        .map(|i| {
                            let text = format!("vtab-concurrent-{}", (i * 7 + t) % 3000);
                            (text.clone(), intern_str(&text))
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        let mut seen = std::collections::HashMap::new();
        for thread in threads {
            for (text, id) in thread.join().unwrap() {
                assert_eq!(resolve(id).as_str(), Some(text.as_str()));
                assert_eq!(*seen.entry(text).or_insert(id), id);
            }
        }
        assert_eq!(seen.len(), 3000);
    }
}
