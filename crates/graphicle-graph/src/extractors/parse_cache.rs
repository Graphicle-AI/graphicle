// Adapted from Graphify 0.9.88 (a9860f70), Apache-2.0; see NOTICE.md.
// Upstream: graphify/extract.py
//! Run-scoped cache of raw source bytes and parsed syntax trees.
//!
//! Extraction reads and parses each source file several times: once per file
//! in the per-file stage, again for rationale comments, and again for every
//! cross-file resolution pass in `assemble`. While an extraction run is active
//! this cache serves repeat reads and repeat parses of the same path, bounded
//! by byte budgets so large repositories never accumulate every tree. Trees
//! are only retained while the run is sequential: a parallel run re-parses on
//! demand across its workers instead (see [`set_workers`]), which keeps the
//! resident set bounded by the trees in flight rather than by the corpus.
//! Within one file's extraction, the extractor hands its tree to the file's
//! rationale pass on the same thread ([`hand_off`]) instead of it parsing again.
//! Outside a run every call falls through to the filesystem, so nothing can go
//! stale across `watch` iterations or ad-hoc single-file extraction.
use super::engine_parser::Parsed;
use std::{
    collections::HashMap,
    io,
    path::{Path, PathBuf},
    sync::{Arc, LazyLock, Mutex, MutexGuard},
};
use tree_sitter::Language;

/// Shared raw file contents.
pub type Source = Arc<[u8]>;
/// Shared parse result; `Parsed` is immutable once built.
pub type Tree = Arc<Parsed>;

/// Total raw bytes retained across cached sources.
const SOURCE_BUDGET: usize = 64 << 20;
/// Total source bytes whose trees are retained by a sequential run (trees cost
/// several times their source size, so this is deliberately smaller).
const TREE_BUDGET: usize = 16 << 20;

#[derive(Default)]
struct Cache {
    /// Number of extraction runs currently in progress; the cache serves
    /// requests while this is non-zero.
    active: usize,
    /// Worker threads the cross-file resolvers may use for per-file work
    /// while a run is active; 1 (the default) keeps them sequential.
    workers: usize,
    source_bytes: usize,
    tree_bytes: usize,
    sources: HashMap<PathBuf, Source>,
    trees: HashMap<&'static str, HashMap<PathBuf, Tree>>,
    /// Facts one pass derived from a file's tree for a later pass of the
    /// same run (see [`keep_facts`]), by kind and path.
    facts: HashMap<(&'static str, PathBuf), Facts>,
}
/// A later pass's facts about one file (see [`keep_facts`]).
pub type Facts = Box<dyn std::any::Any + Send>;

thread_local! {
    /// The tree this thread's extractor just built, for the same file's next
    /// parse under the same grammar on this thread (see [`hand_off`]).
    static HANDOFF: std::cell::RefCell<Option<(PathBuf, &'static str, Tree)>> =
        const { std::cell::RefCell::new(None) };
}

/// Keep `parsed`, the tree an extractor built from `path`'s own bytes under
/// `tag`'s grammar, for the next [`parse`] of that path and grammar on this
/// thread: the file's rationale pass then walks it instead of parsing again.
/// Only one tree is kept per thread, and only within a [`HandoffScope`].
pub fn hand_off(path: &Path, tag: &'static str, parsed: Tree) {
    HANDOFF.with(|slot| *slot.borrow_mut() = Some((path.to_path_buf(), tag, parsed)));
}

/// One file's extraction: a tree handed off inside it is dropped when it ends
/// (including by panic), so nothing outlives the file it came from.
pub struct HandoffScope(());

impl HandoffScope {
    pub fn begin() -> Self {
        HANDOFF.with(|slot| slot.borrow_mut().take());
        HandoffScope(())
    }
}

impl Drop for HandoffScope {
    fn drop(&mut self) {
        HANDOFF.with(|slot| slot.borrow_mut().take());
    }
}

static CACHE: LazyLock<Mutex<Cache>> = LazyLock::new(Mutex::default);

fn lock() -> MutexGuard<'static, Cache> {
    CACHE.lock().unwrap_or_else(|e| e.into_inner())
}

/// Guard that keeps the cache active for one extraction run and clears it when
/// the last overlapping run ends, including on early return or panic.
pub struct RunScope(());

impl RunScope {
    pub fn begin() -> Self {
        let mut cache = lock();
        if cache.active == 0 {
            *cache = Cache::default();
        }
        cache.active += 1;
        RunScope(())
    }
}

impl Drop for RunScope {
    fn drop(&mut self) {
        let mut cache = lock();
        cache.active = cache.active.saturating_sub(1);
        if cache.active == 0 {
            // Releasing a corpus worth of trees is slow; the cache is reset
            // immediately and the old contents are freed off the caller's thread.
            // (A failed spawn drops the closure, freeing them inline instead.)
            let retired = std::mem::take(&mut *cache);
            drop(cache);
            let _ = std::thread::Builder::new()
                .name("parse-cache-release".into())
                .spawn(move || drop(retired));
        }
    }
}

fn active(cache: &Cache) -> bool {
    cache.active > 0
}

/// Read a file, serving repeat reads from the run cache when one is active.
pub fn read(path: &Path) -> io::Result<Source> {
    {
        let cache = lock();
        if !active(&cache) {
            drop(cache);
            return std::fs::read(path).map(Source::from);
        }
        if let Some(source) = cache.sources.get(path) {
            return Ok(source.clone());
        }
    }
    let source = Source::from(std::fs::read(path)?);
    let mut cache = lock();
    if active(&cache) && cache.source_bytes + source.len() <= SOURCE_BUDGET {
        cache.source_bytes += source.len();
        cache.sources.insert(path.to_path_buf(), source.clone());
    }
    Ok(source)
}

/// Parse `path` with `language`, serving repeat parses from the run cache.
/// `tag` names the grammar so one path may be cached under several grammars.
pub fn parse(path: &Path, tag: &'static str, language: Language) -> Option<Tree> {
    let handed = HANDOFF.with(|slot| {
        let mut slot = slot.borrow_mut();
        match slot.as_ref() {
            Some((p, t, _)) if *t == tag && p == path => slot.take().map(|(_, _, tree)| tree),
            _ => None,
        }
    });
    if handed.is_some() {
        return handed;
    }
    // One look at the cache for both the tree and the bytes: every parse
    // of a parallel pass comes through here.
    let (tree, source, retains) = {
        let cache = lock();
        let tree = cache.trees.get(tag).and_then(|m| m.get(path)).cloned();
        let source = tree
            .is_none()
            .then(|| cache.sources.get(path).cloned())
            .flatten();
        (tree, source, active(&cache) && cache.workers <= 1)
    };
    if tree.is_some() {
        return tree;
    }
    let source = match source {
        Some(source) => source,
        None => read(path).ok()?,
    };
    // Every upstream parse site reads through `_read_source_bytes`, so a tree
    // is built from the decoded source (the same bytes for valid UTF-8).
    let decoded = super::base::decode_source(&source).0.into_owned();
    let parsed = Tree::new(parse_bytes(language, decoded)?);
    if !retains {
        return Some(parsed);
    }
    let mut cache = lock();
    if active(&cache) && cache.workers <= 1 && cache.tree_bytes + source.len() <= TREE_BUDGET {
        cache.tree_bytes += source.len();
        cache
            .trees
            .entry(tag)
            .or_default()
            .insert(path.to_path_buf(), parsed.clone());
    }
    Some(parsed)
}

/// Keep `facts` a pass derived from `path`'s tree for a later pass of the
/// current run that would otherwise parse the file again; they are dropped
/// with the run. Outside a run nothing is kept.
pub fn keep_facts(kind: &'static str, path: &Path, facts: Facts) {
    let mut cache = lock();
    if active(&cache) {
        cache.facts.insert((kind, path.to_path_buf()), facts);
    }
}

/// Take the facts [`keep_facts`] kept for `path`, if any.
pub fn take_facts(kind: &'static str, path: &Path) -> Option<Facts> {
    let mut cache = lock();
    if cache.facts.is_empty() {
        return None;
    }
    cache.facts.remove(&(kind, path.to_path_buf()))
}

/// A copy of the facts [`keep_facts`] kept for `path`, which stay kept for a
/// later pass that takes them.
pub fn kept_clone<T: Clone + 'static>(kind: &'static str, path: &Path) -> Option<T> {
    let cache = lock();
    cache
        .facts
        .get(&(kind, path.to_path_buf()))?
        .downcast_ref::<T>()
        .cloned()
}

/// Allow the cross-file resolvers to spread per-file work over `workers`
/// threads for the duration of the current run. With more than one worker the
/// run stops retaining trees: every pass parses on demand in parallel and
/// frees each tree as soon as its walk is done.
pub fn set_workers(workers: usize) {
    let mut cache = lock();
    if active(&cache) {
        cache.workers = workers.max(1);
    }
}

/// Apply `f` to every item, in parallel when the current run allows it, and
/// return the results in item order. Sequential (and allocation-free beyond
/// the output) when no run is active, `workers` is 1 or the input is small.
pub fn par_map<T: Sync, R: Send>(items: &[T], f: impl Fn(&T) -> R + Sync) -> Vec<R> {
    let workers = lock().workers.clamp(1, items.len().max(1));
    if workers == 1 || items.len() < 8 {
        return items.iter().map(f).collect();
    }
    let next = std::sync::atomic::AtomicUsize::new(0);
    let slots = Mutex::new(Vec::with_capacity(items.len()));
    std::thread::scope(|scope| {
        for _ in 0..workers {
            let (next, slots, f) = (&next, &slots, &f);
            scope.spawn(move || {
                loop {
                    let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some(item) = items.get(i) else {
                        break;
                    };
                    let out = f(item);
                    slots
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push((i, out));
                }
            });
        }
    });
    let mut slots = slots.into_inner().unwrap_or_else(|e| e.into_inner());
    slots.sort_by_key(|(i, _)| *i);
    slots.into_iter().map(|(_, out)| out).collect()
}

/// Apply `f` to every item in place, splitting the slice across the workers
/// the current run allows; sequential under the same conditions as [`par_map`].
pub fn par_for_each_mut<T: Send>(items: &mut [T], f: impl Fn(&mut T) + Sync) {
    let workers = lock().workers.clamp(1, items.len().max(1));
    if workers == 1 || items.len() < 8 {
        items.iter_mut().for_each(f);
        return;
    }
    let chunk = items.len().div_ceil(workers);
    std::thread::scope(|scope| {
        for part in items.chunks_mut(chunk) {
            let f = &f;
            scope.spawn(move || part.iter_mut().for_each(f));
        }
    });
}

/// Whether an extraction run is in progress (so [`keep_facts`] keeps).
pub fn run_active() -> bool {
    active(&lock())
}

/// Worker threads the current run allows for per-file work (1 outside a run).
pub fn workers() -> usize {
    let cache = lock();
    if active(&cache) {
        cache.workers.max(1)
    } else {
        1
    }
}

/// Parse already-loaded bytes without touching the cache.
pub fn parse_bytes(language: Language, source: Vec<u8>) -> Option<Parsed> {
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&language).ok()?;
    let tree = parser.parse(&source, None)?;
    Some(Parsed { tree, source })
}

/// The cache is process-wide, so unit tests that depend on it being inactive
/// or freshly active must not overlap with each other or with tests that run
/// a whole [`crate::extract::extract`] (which opens a [`RunScope`]).
#[cfg(test)]
static TEST_SERIAL: Mutex<()> = Mutex::new(());

#[cfg(test)]
pub(crate) fn test_serial() -> MutexGuard<'static, ()> {
    TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn serial() -> MutexGuard<'static, ()> {
        test_serial()
    }

    #[test]
    fn inactive_cache_reads_fresh_bytes() {
        let _serial = serial();
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.py");
        std::fs::write(&file, b"x = 1\n").unwrap();
        assert_eq!(&*read(&file).unwrap(), b"x = 1\n");
        std::fs::write(&file, b"x = 2\n").unwrap();
        assert_eq!(&*read(&file).unwrap(), b"x = 2\n");
    }

    #[test]
    fn active_run_shares_sources_and_trees_then_clears() {
        let _serial = serial();
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.py");
        std::fs::write(&file, b"def f():\n    pass\n").unwrap();
        {
            let _run = RunScope::begin();
            let first = read(&file).unwrap();
            let second = read(&file).unwrap();
            assert!(Arc::ptr_eq(&first, &second));
            let python = || tree_sitter_python::LANGUAGE.into();
            let t1 = parse(&file, "python", python()).unwrap();
            let t2 = parse(&file, "python", python()).unwrap();
            assert!(Arc::ptr_eq(&t1, &t2));
            assert_eq!(t1.tree.root_node().kind(), "module");
        }
        std::fs::write(&file, b"y = 3\n").unwrap();
        assert_eq!(&*read(&file).unwrap(), b"y = 3\n");
        let fresh = parse(&file, "python", tree_sitter_python::LANGUAGE.into()).unwrap();
        assert_eq!(fresh.source.as_slice(), b"y = 3\n");
    }

    #[test]
    fn overlapping_runs_keep_the_cache_until_the_last_one_ends() {
        let _serial = serial();
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.py");
        std::fs::write(&file, b"x = 1\n").unwrap();
        let outer = RunScope::begin();
        let first = read(&file).unwrap();
        {
            let _inner = RunScope::begin();
            assert!(Arc::ptr_eq(&first, &read(&file).unwrap()));
        }
        assert!(Arc::ptr_eq(&first, &read(&file).unwrap()));
        drop(outer);
        assert!(!active(&lock()));
        assert!(lock().sources.is_empty());
    }

    #[test]
    fn a_handed_off_tree_serves_the_next_parse_on_its_thread_only() {
        let _serial = serial();
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.py");
        let bytes = b"import os\n\ndef f(x):\n    return os.path.join(x)\n";
        std::fs::write(&file, bytes).unwrap();
        let python = || -> Language { tree_sitter_python::LANGUAGE.into() };
        let _run = RunScope::begin();
        set_workers(8);
        let fresh = parse(&file, "python", python()).unwrap();
        {
            let _scope = HandoffScope::begin();
            let handed = Tree::new(parse_bytes(python(), bytes.to_vec()).unwrap());
            hand_off(&file, "python", handed.clone());
            // Another grammar or another thread does not take it.
            assert!(!Arc::ptr_eq(
                &handed,
                &parse(&file, "javascript", tree_sitter_javascript::LANGUAGE.into()).unwrap()
            ));
            let other = std::thread::scope(|s| {
                s.spawn(|| parse(&file, "python", python()).unwrap())
                    .join()
                    .unwrap()
            });
            assert!(!Arc::ptr_eq(&handed, &other));
            // The file's next parse on this thread walks it, once.
            let served = parse(&file, "python", python()).unwrap();
            assert!(Arc::ptr_eq(&handed, &served));
            assert_eq!(
                served.tree.root_node().to_sexp(),
                fresh.tree.root_node().to_sexp()
            );
            assert!(!Arc::ptr_eq(
                &handed,
                &parse(&file, "python", python()).unwrap()
            ));
            hand_off(&file, "python", handed.clone());
        }
        // The scope drops an unused hand-off.
        assert!(HANDOFF.with(|slot| slot.borrow().is_none()));
    }
}
