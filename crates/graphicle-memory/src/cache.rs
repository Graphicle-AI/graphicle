//! Exact bounded feature reuse, adapted from MemOS CachingEmbedder's scoped
//! reuse/deduplication design. Stores token features, not canonical memory.
//! Changes: explicit scope invalidation, byte admission, no background worker.
use crate::{Result, text};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Distinct words of one text with their counts, stored compactly: the words
/// sorted and concatenated, with end offsets and counts beside them. About a
/// fifth of the memory of a `BTreeMap<String, usize>`, so a warm process can
/// keep the features of a whole passage catalogue instead of recomputing them.
#[derive(Clone, Default)]
pub struct Words {
    text: Box<str>,
    ends: Box<[u32]>,
    counts: Box<[u32]>,
}

impl Words {
    fn from_counts(counts: BTreeMap<String, usize>) -> Self {
        let mut text = String::new();
        let mut ends = Vec::with_capacity(counts.len());
        let mut values = Vec::with_capacity(counts.len());
        for (word, n) in counts {
            text.push_str(&word);
            ends.push(text.len() as u32);
            values.push(n.min(u32::MAX as usize) as u32);
        }
        Self {
            text: text.into_boxed_str(),
            ends: ends.into_boxed_slice(),
            counts: values.into_boxed_slice(),
        }
    }
    fn word(&self, i: usize) -> &str {
        let start = if i == 0 { 0 } else { self.ends[i - 1] as usize };
        &self.text[start..self.ends[i] as usize]
    }
    fn find(&self, word: &str) -> Option<usize> {
        let (mut low, mut high) = (0, self.ends.len());
        while low < high {
            let mid = (low + high) / 2;
            match self.word(mid).cmp(word) {
                std::cmp::Ordering::Less => low = mid + 1,
                std::cmp::Ordering::Greater => high = mid,
                std::cmp::Ordering::Equal => return Some(mid),
            }
        }
        None
    }
    pub fn contains_key(&self, word: &str) -> bool {
        self.find(word).is_some()
    }
    /// How often `word` occurs (0 when it does not).
    pub fn get(&self, word: &str) -> usize {
        self.find(word).map_or(0, |i| self.counts[i] as usize)
    }
    /// Number of distinct words.
    pub fn len(&self) -> usize {
        self.ends.len()
    }
    pub fn is_empty(&self) -> bool {
        self.ends.is_empty()
    }
    pub fn keys(&self) -> impl Iterator<Item = &str> {
        (0..self.len()).map(|i| self.word(i))
    }
    /// The same counts with every word passed through `f`; words `f` maps
    /// together have their counts summed (stemming "plan", "plans", "planned").
    pub fn map(&self, mut f: impl FnMut(&str) -> String) -> Words {
        let mut counts = BTreeMap::new();
        for i in 0..self.len() {
            *counts.entry(f(self.word(i))).or_default() += self.counts[i] as usize;
        }
        Self::from_counts(counts)
    }
    /// Distinct words both texts contain (a merge of the two sorted lists).
    pub fn common(&self, other: &Words) -> usize {
        let (mut i, mut j, mut n) = (0, 0, 0);
        while i < self.len() && j < other.len() {
            match self.word(i).cmp(other.word(j)) {
                std::cmp::Ordering::Less => i += 1,
                std::cmp::Ordering::Greater => j += 1,
                std::cmp::Ordering::Equal => {
                    n += 1;
                    i += 1;
                    j += 1;
                }
            }
        }
        n
    }
    fn bytes(&self) -> usize {
        self.text.len() + 8 * self.ends.len()
    }
}

#[derive(Clone)]
pub struct Features {
    pub words: Words,
    pub length: usize,
}
struct Entry {
    value: Arc<Features>,
    bytes: usize,
    tick: u64,
    at: Instant,
}
#[derive(Default)]
struct Cache {
    entries: HashMap<(String, String), Entry>,
    order: BTreeMap<u64, (String, String)>,
    bytes: usize,
    tick: u64,
    hits: u64,
    misses: u64,
}
fn cache() -> &'static Mutex<Cache> {
    static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(Cache::default()))
}
/// Holds the features of the whole bounded recall catalogue (20,000 passages
/// of a few kilobytes plus reviewed memories) so repeated queries in one
/// process keep hitting instead of recomputing.
const MAX_BYTES: usize = 128 * 1024 * 1024;
// Match the largest ranked row set, with plain and stemmed features of each
// row. Byte caps remain authoritative; eviction uses the ordered tick index
// instead of scanning the entire cache.
const MAX_ENTRIES: usize = 2 * crate::retrieval::MAX_ROWS;
/// Entries unused this long are recomputed. Keys are content hashes, so an
/// entry is never stale; forgetting clears its repository scope at once.
const TTL: Duration = Duration::from_secs(1800);
/// A full cache never evicts an entry used this recently; the new value is
/// returned uncached instead. Plain LRU under repeated queries over a working
/// set even slightly larger than the cache evicts each entry just before its
/// next use and misses on every lookup; this keeps a stable part cached.
const PROTECT: Duration = Duration::from_secs(120);

pub fn features(scope: &str, content: &str) -> Result<Arc<Features>> {
    let key = (scope.to_owned(), text::sha256(content.as_bytes()));
    if let Some(value) = lookup(&key)? {
        return Ok(value);
    }
    let mut counts = BTreeMap::new();
    let mut length = 0;
    for word in text::words(content) {
        *counts.entry(word).or_insert(0) += 1;
        length += 1;
    }
    let words = Words::from_counts(counts);
    store(key, Features { words, length })
}
/// [`features`] with every word Porter-stemmed, cached beside the plain
/// features (`retrieval::rank` with `stem`). Lengths are unchanged.
pub fn stemmed_features(
    scope: &str,
    content: &str,
    stemmer: &mut crate::stem::Stemmer,
) -> Result<Arc<Features>> {
    let key = (
        scope.to_owned(),
        format!("{}:porter", text::sha256(content.as_bytes())),
    );
    if let Some(value) = lookup(&key)? {
        return Ok(value);
    }
    let plain = features(scope, content)?;
    let words = plain.words.map(|w| stemmer.stem(w));
    store(
        key,
        Features {
            words,
            length: plain.length,
        },
    )
}
/// A live cached value, refreshed as most recently used; a miss is counted.
fn lookup(key: &(String, String)) -> Result<Option<Arc<Features>>> {
    let mut state = cache()
        .lock()
        .map_err(|_| "Feature cache lock unavailable")?;
    state.tick += 1;
    let tick = state.tick;
    if let Some(entry) = state.entries.get_mut(key)
        && entry.at.elapsed() < TTL
    {
        let previous = entry.tick;
        entry.tick = tick;
        entry.at = Instant::now();
        let value = entry.value.clone();
        state.order.remove(&previous);
        state.order.insert(tick, key.clone());
        state.hits += 1;
        return Ok(Some(value));
    }
    if let Some(old) = state.entries.remove(key) {
        state.bytes -= old.bytes;
        state.order.remove(&old.tick);
    }
    state.misses += 1;
    Ok(None)
}
fn store(key: (String, String), features: Features) -> Result<Arc<Features>> {
    let mut state = cache()
        .lock()
        .map_err(|_| "Feature cache lock unavailable")?;
    // Another caller may have stored the same content meanwhile.
    if let Some(old) = state.entries.remove(&key) {
        state.bytes -= old.bytes;
        state.order.remove(&old.tick);
    }
    state.tick += 1;
    let tick = state.tick;
    let bytes = features.words.bytes() + 2 * key.0.len() + 2 * key.1.len() + 160;
    let value = Arc::new(features);
    if bytes <= MAX_BYTES {
        while state.bytes + bytes > MAX_BYTES || state.entries.len() >= MAX_ENTRIES {
            let Some((&oldest, key)) = state.order.first_key_value() else {
                break;
            };
            if state
                .entries
                .get(key)
                .is_some_and(|e| e.at.elapsed() < PROTECT)
            {
                // Every remaining entry was used more recently still.
                return Ok(value);
            }
            let key = state.order.remove(&oldest).unwrap();
            if let Some(entry) = state.entries.remove(&key) {
                state.bytes -= entry.bytes;
            }
        }
        state.bytes += bytes;
        state.order.insert(tick, key.clone());
        state.entries.insert(
            key,
            Entry {
                value: value.clone(),
                bytes,
                tick,
                at: Instant::now(),
            },
        );
    }
    Ok(value)
}
pub fn clear(scope: &str) -> Result<()> {
    let mut state = cache()
        .lock()
        .map_err(|_| "Feature cache lock unavailable")?;
    state.entries.retain(|(s, _), _| s != scope);
    state.order.retain(|_, (s, _)| s.as_str() != scope);
    state.bytes = state.entries.values().map(|e| e.bytes).sum();
    let mut layouts = layout_cache()
        .lock()
        .map_err(|_| "Layout cache lock unavailable")?;
    layouts.entries.retain(|(s, _, _, _), _| s != scope);
    layouts.order.retain(|_, (s, _, _, _)| s.as_str() != scope);
    layouts.bytes = layouts.entries.values().map(|e| e.bytes).sum();
    Ok(())
}
pub fn stats() -> Result<Value> {
    let s = cache()
        .lock()
        .map_err(|_| "Feature cache lock unavailable")?;
    let l = layout_cache()
        .lock()
        .map_err(|_| "Layout cache lock unavailable")?;
    Ok(
        json!({"entries":s.entries.len(),"bytes":s.bytes,"max_bytes":MAX_BYTES,"max_entries":MAX_ENTRIES,"hits":s.hits,"misses":s.misses,
        "layouts":{"entries":l.entries.len(),"bytes":l.bytes,"max_bytes":LAYOUT_BYTES,"max_entries":LAYOUT_ENTRIES,"hits":l.hits,"misses":l.misses},"persistent":false}),
    )
}

#[derive(Clone)]
pub struct Span {
    pub start: usize,
    pub end: usize,
    pub line: usize,
    pub end_line: usize,
}
struct LayoutEntry {
    value: Arc<Vec<Span>>,
    bytes: usize,
    tick: u64,
    at: Instant,
}
type LayoutKey = (String, String, usize, usize);
#[derive(Default)]
struct LayoutCache {
    entries: HashMap<LayoutKey, LayoutEntry>,
    order: BTreeMap<u64, LayoutKey>,
    bytes: usize,
    tick: u64,
    hits: u64,
    misses: u64,
}
const LAYOUT_BYTES: usize = 32 * 1024 * 1024;
/// Two chunk settings over the largest passage catalogue: every document
/// that fills `max_passages` has a layout, and jobs chunk with other settings.
const LAYOUT_ENTRIES: usize = 2 * crate::ingest::MAX_PASSAGES;
fn layout_cache() -> &'static Mutex<LayoutCache> {
    static CACHE: OnceLock<Mutex<LayoutCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(LayoutCache::default()))
}
pub fn layout(
    scope: &str,
    sha: &str,
    characters: usize,
    tokens: usize,
    build: impl FnOnce() -> Result<Vec<Span>>,
) -> Result<Arc<Vec<Span>>> {
    let key = (scope.to_owned(), sha.to_owned(), characters, tokens);
    {
        let mut state = layout_cache()
            .lock()
            .map_err(|_| "Layout cache lock unavailable")?;
        state.tick += 1;
        let tick = state.tick;
        if let Some(entry) = state.entries.get_mut(&key)
            && entry.at.elapsed() < TTL
        {
            let previous = entry.tick;
            entry.tick = tick;
            entry.at = Instant::now();
            let value = entry.value.clone();
            state.order.remove(&previous);
            state.order.insert(tick, key);
            state.hits += 1;
            return Ok(value);
        }
        if let Some(old) = state.entries.remove(&key) {
            state.bytes -= old.bytes;
            state.order.remove(&old.tick);
        }
        state.misses += 1;
    }
    // Built without the lock, so documents are laid out side by side
    // (`ingest::chunks`); a layout depends only on the content and settings.
    let value = Arc::new(build()?);
    let mut state = layout_cache()
        .lock()
        .map_err(|_| "Layout cache lock unavailable")?;
    // Another caller may have stored the same layout meanwhile.
    if let Some(old) = state.entries.remove(&key) {
        state.bytes -= old.bytes;
        state.order.remove(&old.tick);
    }
    state.tick += 1;
    let tick = state.tick;
    let bytes = value.len() * std::mem::size_of::<Span>() + 2 * scope.len() + 2 * sha.len() + 384;
    if bytes <= LAYOUT_BYTES {
        while state.bytes + bytes > LAYOUT_BYTES || state.entries.len() >= LAYOUT_ENTRIES {
            let Some((&oldest, key)) = state.order.first_key_value() else {
                break;
            };
            if state
                .entries
                .get(key)
                .is_some_and(|e| e.at.elapsed() < PROTECT)
            {
                return Ok(value);
            }
            let key = state.order.remove(&oldest).unwrap();
            if let Some(entry) = state.entries.remove(&key) {
                state.bytes -= entry.bytes;
            }
        }
        state.order.insert(tick, key.clone());
        state.entries.insert(
            key,
            LayoutEntry {
                value: value.clone(),
                bytes,
                tick,
                at: Instant::now(),
            },
        );
        state.bytes += bytes;
    }
    Ok(value)
}

/// Most threads [`par_map`] uses, the caller's included. A command-line
/// process starts with empty caches, so each query lays out every document
/// and computes the features of every passage; both depend only on the
/// text, so they are computed side by side.
const MAX_THREADS: usize = 16;
/// Lists shorter than this are mapped on the calling thread.
const PARALLEL_FROM: usize = 64;

/// `f` over `items` in input order, on the calling thread and up to
/// [`MAX_THREADS`] - 1 scoped threads, each with its own `init()` state (a
/// stemmer, whose results do not depend on its memo). The result is what one
/// thread mapping the items in order would return; a panic in `f` is resumed
/// on the calling thread.
pub fn par_map<T: Sync, S, R: Send>(
    items: &[T],
    init: impl Fn() -> S + Sync,
    f: impl Fn(&mut S, &T) -> R + Sync,
) -> Vec<R> {
    let threads = std::thread::available_parallelism()
        .map_or(1, |n| n.get())
        .min(MAX_THREADS)
        .min(items.len() / (PARALLEL_FROM / 4).max(1));
    if items.len() < PARALLEL_FROM || threads < 2 {
        let mut state = init();
        return items.iter().map(|item| f(&mut state, item)).collect();
    }
    let next = std::sync::atomic::AtomicUsize::new(0);
    let work = || {
        let mut state = init();
        let mut done = Vec::new();
        loop {
            let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let Some(item) = items.get(i) else {
                return done;
            };
            done.push((i, f(&mut state, item)));
        }
    };
    let parts: Vec<Vec<(usize, R)>> = std::thread::scope(|scope| {
        // The calling thread works too, so a thread the system refuses to
        // start only leaves its share to the others.
        let workers: Vec<_> = (1..threads)
            .filter_map(|_| std::thread::Builder::new().spawn_scoped(scope, work).ok())
            .collect();
        let mut parts = vec![work()];
        for worker in workers {
            parts.push(
                worker
                    .join()
                    .unwrap_or_else(|panic| std::panic::resume_unwind(panic)),
            );
        }
        parts
    });
    let mut slots: Vec<Option<R>> = std::iter::repeat_with(|| None).take(items.len()).collect();
    for (i, value) in parts.into_iter().flatten() {
        slots[i] = Some(value);
    }
    slots
        .into_iter()
        .map(|value| value.expect("every item is mapped once"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::par_map;

    #[test]
    fn par_map_returns_the_ordered_map_with_per_thread_state() {
        let items: Vec<usize> = (0..5000).collect();
        let mapped = par_map(&items, Vec::new, |seen: &mut Vec<usize>, &i| {
            seen.push(i);
            (i * 7 % 13, seen.len() <= items.len())
        });
        let expected: Vec<_> = items.iter().map(|&i| (i * 7 % 13, true)).collect();
        assert_eq!(mapped, expected);
        // Short lists stay on the calling thread with one state.
        let short = par_map(&items[..10], Vec::new, |seen: &mut Vec<usize>, &i| {
            seen.push(i);
            seen.len()
        });
        assert_eq!(short, (1..=10).collect::<Vec<_>>());
    }

    #[test]
    #[should_panic(expected = "item 4000")]
    fn par_map_resumes_a_worker_panic() {
        let items: Vec<usize> = (0..5000).collect();
        par_map(&items, || (), |_, &i| assert_ne!(i, 4000, "item {i}"));
    }
}
