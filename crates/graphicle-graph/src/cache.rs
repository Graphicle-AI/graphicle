// Adapted from Graphify 0.9.88 (a9860f70), Apache-2.0; see NOTICE.md.
// Upstream: graphify/cache.py, graphify/extract.py
//! Port of graphify/cache.py: owned per-run cache state with explicit flush.
//! The native release has its own AST version; semantic entries retain their
//! original content/prompt namespaces so compatible extraction is reusable.
pub use crate::cache_paths::*;
use crate::{Result, paths, python_compat, python_json};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeSet, HashSet},
    fs,
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
/// Bump when AST cache-key semantics change independently of the package
/// version; 8 carries the per-file record fields of Graphify 0.9.78-0.9.88
/// (upstream schemas 6-12: Python typed receivers, `self.X` bindings, opaque
/// bases, `super(args)` and property reads, C# `type_path`, receiver chains and
/// return tables, Java receiver chains and `_java_ret`, Rust `use` groups,
/// quoted-include provenance) and records the target files an entry's edges
/// resolved to; 7 invalidated records whose source paths leaked a walked
/// directory alias when the extraction root was canonicalized (6 added
/// receiver shadows).
pub const AST_CACHE_SCHEMA: i64 = 8;
/// Where an AST entry lists the `target_file` stamps that named an existing
/// file when it was written; a hit whose target is gone is a miss (#4185).
pub const CACHED_TARGETS_KEY: &str = "_cached_target_files";
/// `_salted_key`: the on-disk key of an entry whose validity depends on more
/// than the file's bytes (a file the project remaps to another language,
/// #2961); without a salt, the content hash itself.
pub fn salted_key(hash: &str, salt: Option<&str>) -> String {
    match salt.filter(|s| !s.is_empty()) {
        None => hash.to_string(),
        Some(salt) => format!("{:x}", Sha256::digest(format!("{hash}:{salt}").as_bytes())),
    }
}
/// `_grammar_fingerprint` over `(name, version)` pairs: the first 12 hex
/// digits of the SHA-256 of the sorted `name==version` list, `"none"` for an
/// empty set.
pub fn grammar_fingerprint_of(parts: impl IntoIterator<Item = (String, String)>) -> String {
    let mut parts = parts
        .into_iter()
        .map(|(name, version)| format!("{}=={version}", name.to_lowercase()))
        .collect::<Vec<_>>();
    if parts.is_empty() {
        return "none".into();
    }
    parts.sort();
    format!("{:x}", Sha256::digest(parts.join(";").as_bytes()))[..12].into()
}
/// The tree-sitter runtime and grammar crates the engine pins, from its own
/// `Cargo.lock` (vendored `graphicle-tree-sitter-*` crates included).
/// Upstream fingerprints the installed `tree-sitter-*` distributions; the
/// native links its grammars statically, so the versions it is built with are
/// its grammar set (a grammar pack must match the pinned grammar exactly, see
/// `grammar_packs`). A workspace that embeds the engine resolves its own lock
/// (the `graphicle` program links tree-sitter 0.27), which this list does not
/// see.
pub fn compiled_grammars() -> Vec<(String, String)> {
    const LOCK: &str = include_str!("../Cargo.lock");
    let grammar = |name: &str| {
        let name = name.strip_prefix("graphicle-").unwrap_or(name);
        name == "tree-sitter" || name.starts_with("tree-sitter-") || name.starts_with("tree_sitter")
    };
    let mut out = vec![];
    let mut name = None;
    for line in LOCK.lines().map(str::trim) {
        if line == "[[package]]" {
            name = None;
        } else if let Some(value) = line.strip_prefix("name = ") {
            name = Some(value.trim_matches('"'));
        } else if let Some(value) = line.strip_prefix("version = ")
            && let Some(package) = name.take()
            && grammar(package)
        {
            out.push((package.to_string(), value.trim_matches('"').to_string()));
        }
    }
    out
}
/// The grammar set this build's AST entries depend on (#4236), memoised.
pub fn grammar_fingerprint() -> &'static str {
    static FINGERPRINT: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    FINGERPRINT.get_or_init(|| grammar_fingerprint_of(compiled_grammars()))
}
/// `_target_file_present`: `target` (relative ones under `root`) is a file.
pub fn target_file_present(target: &str, root: &Path) -> bool {
    let candidate = Path::new(target);
    if candidate.is_absolute() {
        candidate.is_file()
    } else {
        root.join(candidate).is_file()
    }
}
/// `_present_target_files`: the edges' `target_file` stamps that name an
/// existing file, sorted and distinct.
pub fn present_target_files(result: &Value, root: &Path) -> Vec<String> {
    let mut present = BTreeSet::new();
    for edge in ar(&result["edges"]) {
        if let Some(target) = edge.get("target_file").and_then(Value::as_str)
            && !target.is_empty()
            && !present.contains(target)
            && target_file_present(target, root)
        {
            present.insert(target.to_string());
        }
    }
    present.into_iter().collect()
}
pub const MTIME_GRANULARITY_NS: i64 = 2_000_000_000;
pub fn clock_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos().min(i64::MAX as u128) as i64)
        .unwrap_or(0)
}
pub fn body_content(bytes: &[u8]) -> Vec<u8> {
    static FENCE: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"(?m)^---[ \t]*\r?$").unwrap());
    let text = String::from_utf8_lossy(bytes);
    let re = &*FENCE;
    let Some(open) = re.find(&text).filter(|m| m.start() == 0) else {
        return bytes.into();
    };
    let Some(close) = re.find_at(&text, open.end()) else {
        return bytes.into();
    };
    text[close.start() + 3..].as_bytes().into()
}
pub fn prompt_fingerprint(text: &str) -> String {
    let normalized = text
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .split('\n')
        .map(|l| l.trim_end_matches(|c: char| c.is_whitespace() || matches!(c, '\x1c'..='\x1f')))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "{:x}",
        Sha256::digest(
            normalized
                .trim_matches(|c: char| c.is_whitespace() || matches!(c, '\x1c'..='\x1f'))
                .as_bytes()
        )
    )[..12]
        .into()
}
pub fn mtime_granularity(raw: Option<&str>) -> i64 {
    let parsed = raw.unwrap_or("").trim().parse::<f64>();
    match parsed {
        Ok(n) if n >= 0.0 && n.is_finite() => (n * 1_000_000.0) as i64,
        _ => MTIME_GRANULARITY_NS,
    }
}
#[derive(Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct Stat {
    pub size: u64,
    pub mtime_ns: i64,
}
impl Stat {
    pub fn read(path: &Path) -> std::io::Result<Self> {
        let m = fs::metadata(path)?;
        #[cfg(unix)]
        let mtime_ns = {
            use std::os::unix::fs::MetadataExt;
            m.mtime()
                .saturating_mul(1_000_000_000)
                .saturating_add(m.mtime_nsec())
        };
        #[cfg(not(unix))]
        let mtime_ns = match m.modified()?.duration_since(UNIX_EPOCH) {
            Ok(d) => d.as_nanos() as i64,
            Err(e) => -(e.duration().as_nanos() as i64),
        };
        Ok(Self {
            size: m.len(),
            mtime_ns,
        })
    }
}
pub fn stat_fresh(entry: &Value, st: Stat, granularity: i64) -> bool {
    entry.is_object()
        && entry["size"] == json!(st.size)
        && entry["mtime_ns"] == json!(st.mtime_ns)
        && entry["indexed_at_ns"]
            .as_i64()
            .is_some_and(|at| st.mtime_ns as i128 + granularity as i128 <= at as i128)
}
/// A content hash taken like [`ExtractionCache::file_hash`] without touching
/// the cache, so workers can hash in parallel; [`ExtractionCache::apply_hash`]
/// records it.
pub struct HashProbe {
    result: Result<String>,
    update: Option<StatUpdate>,
    read_source: bool,
}
impl HashProbe {
    fn failed(error: crate::Error) -> Self {
        Self {
            result: Err(error),
            update: None,
            read_source: false,
        }
    }
    pub fn hash(&self) -> Option<&str> {
        self.result.as_deref().ok()
    }
}
/// The stat-index entry a freshly computed hash refreshes.
struct StatUpdate {
    key: String,
    salt: String,
    stat: Stat,
    at: i64,
}
/// Resolutions shared by the probes of one batch: the root and each source
/// directory resolve once instead of once per file (every component of a
/// resolved path costs an `lstat`). Resolving a directory and then the file
/// name from it walks the same components as resolving the whole path.
#[derive(Default)]
pub struct HashMemo {
    shared: bool,
    granularity: Option<i64>,
    root: std::sync::OnceLock<PathBuf>,
    dirs: std::sync::RwLock<std::collections::HashMap<PathBuf, PathBuf>>,
}
impl HashMemo {
    pub fn shared(granularity: i64) -> Self {
        Self {
            shared: true,
            granularity: Some(granularity),
            ..Self::default()
        }
    }
    fn granularity(&self, cache: &ExtractionCache) -> i64 {
        self.granularity.unwrap_or_else(|| cache.granularity())
    }
    fn resolve_root(&self, root: &Path) -> Result<PathBuf> {
        if !self.shared {
            return resolve(root);
        }
        if let Some(r) = self.root.get() {
            return Ok(r.clone());
        }
        let r = resolve(root)?;
        Ok(self.root.get_or_init(|| r).clone())
    }
    fn resolve_file(&self, p: &Path) -> Result<PathBuf> {
        let (Some(parent), Some(name)) = (p.parent(), p.file_name()) else {
            return resolve(p);
        };
        if !self.shared || !p.is_absolute() {
            return resolve(p);
        }
        let known = self
            .dirs
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(parent)
            .cloned();
        let dir = match known {
            Some(dir) => dir,
            None => {
                let dir = resolve(parent)?;
                self.dirs
                    .write()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(parent.to_path_buf(), dir.clone());
                dir
            }
        };
        crate::fsutil::resolve_missing_from(&dir, Path::new(name)).map_err(crate::Error::from)
    }
}
#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum Prompt {
    Text(String),
    File(PathBuf),
}
#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct CacheOptions {
    pub root: PathBuf,
    pub kind: String,
    pub cache_root: Option<PathBuf>,
    pub prompt: Option<Prompt>,
    pub prompt_file: Option<PathBuf>,
    pub allow_legacy: bool,
    pub allow_partial: bool,
    /// Extra key material (`load_cached`/`save_cached` `salt`): the entry key
    /// becomes [`salted_key`] of the content hash and this.
    pub salt: Option<String>,
}
impl Default for CacheOptions {
    fn default() -> Self {
        Self {
            root: PathBuf::from("."),
            kind: "ast".into(),
            cache_root: None,
            prompt: None,
            prompt_file: None,
            allow_legacy: true,
            allow_partial: false,
            salt: None,
        }
    }
}
#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct ExtractionCache {
    pub output: String,
    pub extractor_version: String,
    pub schema: i64,
    pub cleaned_ast_dirs: HashSet<String>,
    pub legacy_semantic_hits: usize,
    pub corrupt_cache_entries: usize,
    pub prompt_fp_cache: Map<String, Value>,
    pub stat_index: Map<String, Value>,
    pub stat_index_root: Option<PathBuf>,
    pub stat_index_anchor: Option<PathBuf>,
    pub stat_index_dirty: bool,
    /// The on-disk index file of the bound root, captured at bind time so a
    /// root switch flushes the outgoing root to its own file (#3989).
    pub stat_index_path: Option<PathBuf>,
    pub warnings: Vec<String>,
    pub source_reads: usize,
    pub computed_word_counts: usize,
    /// A supplied clock/granularity is useful for embedded deterministic callers.
    pub now_ns: Option<i64>,
    pub granularity_ns: Option<i64>,
    /// The grammar fingerprint of the AST directory; `None` is this build's
    /// [`grammar_fingerprint`] (a comparison may supply the reference's).
    pub grammar_fingerprint: Option<String>,
}
impl Default for ExtractionCache {
    fn default() -> Self {
        Self {
            output: paths::OUT_DIR.to_string(),
            extractor_version: format!(
                "{}-graphicle.{}",
                crate::UPSTREAM_VERSION,
                env!("CARGO_PKG_VERSION")
            ),
            schema: AST_CACHE_SCHEMA,
            cleaned_ast_dirs: HashSet::new(),
            legacy_semantic_hits: 0,
            corrupt_cache_entries: 0,
            prompt_fp_cache: Map::new(),
            stat_index: Map::new(),
            stat_index_root: None,
            stat_index_anchor: None,
            stat_index_dirty: false,
            stat_index_path: None,
            warnings: vec![],
            source_reads: 0,
            computed_word_counts: 0,
            now_ns: None,
            granularity_ns: None,
            grammar_fingerprint: None,
        }
    }
}
impl ExtractionCache {
    /// `v{version}-s{schema}-g{grammars}`: the versioned AST directory name.
    pub fn ast_version(&self) -> String {
        format!(
            "v{}-s{}-g{}",
            self.extractor_version,
            self.schema,
            self.grammar_fingerprint
                .as_deref()
                .unwrap_or_else(|| grammar_fingerprint())
        )
    }
    pub fn now(&self) -> i64 {
        self.now_ns.unwrap_or_else(clock_ns)
    }
    pub fn granularity(&self) -> i64 {
        self.granularity_ns.unwrap_or_else(|| {
            mtime_granularity(
                std::env::var("GRAPHICLE_MTIME_GRANULARITY_MS")
                    .ok()
                    .as_deref(),
            )
        })
    }
    pub fn base(&self, root: &Path) -> Result<PathBuf> {
        let out = Path::new(&self.output);
        Ok(if out.is_absolute() {
            out.into()
        } else {
            resolve(root)?.join(out)
        })
    }
    pub fn stat_file(&self, root: &Path) -> Result<PathBuf> {
        Ok(self.base(root)?.join("cache/stat-index.json"))
    }
    pub fn cleanup_stale(&mut self, base: &Path, current: &Path) {
        if !self.cleaned_ast_dirs.insert(ps(current)) || !base.is_dir() {
            return;
        }
        let Ok(children) = fs::read_dir(base) else {
            return;
        };
        for child in children.flatten() {
            let p = child.path();
            if p == current {
                continue;
            }
            if p.is_dir() && child.file_name().to_string_lossy().starts_with('v') {
                if !fs::symlink_metadata(&p).is_ok_and(|m| m.file_type().is_symlink()) {
                    let _ = fs::remove_dir_all(p);
                }
            } else if p.extension().is_some_and(|s| s == "json") {
                let _ = fs::remove_file(p);
            }
        }
    }
    pub fn cache_dir(&mut self, root: &Path, kind: &str, fp: Option<&str>) -> Result<PathBuf> {
        let mut d = self.base(root)?.join("cache").join(kind);
        if kind == "ast" {
            let base = d;
            d = base.join(self.ast_version());
            self.cleanup_stale(&base, &d);
        } else if let Some(fp) = fp.filter(|s| !s.is_empty()) {
            d = d.join(format!("p{fp}"));
        }
        fs::create_dir_all(&d).map_err(crate::Error::from)?;
        Ok(d)
    }
    /// Where the parser-derived facts of files cached for plain AST `options`
    /// are kept (`cache/syntax/v…-s…/<entry hash>.json`, keyed like the AST
    /// entries and versioned with them), so a run that reads a record from
    /// the cache does not parse the file for its cross-file passes either.
    /// Stale versions are cleared once; the directory is created on write.
    pub fn syntax_dir(&mut self, options: &CacheOptions) -> Option<PathBuf> {
        if !Self::plain_ast(options) {
            return None;
        }
        let location = options.cache_root.as_deref().unwrap_or(&options.root);
        let base = self.base(location).ok()?.join("cache").join("syntax");
        let d = base.join(self.ast_version());
        self.cleanup_stale(&base, &d);
        Some(d)
    }
    pub fn resolve_prompt(
        &mut self,
        prompt: Option<&Prompt>,
        file: Option<&Path>,
    ) -> Option<String> {
        let prompt = file
            .map(|p| Prompt::File(p.into()))
            .or_else(|| prompt.cloned())?;
        let memo_key = file.and_then(|p| {
            Stat::read(p)
                .ok()
                .map(|st| json!([ps(p), st.size, st.mtime_ns]).to_string())
        });
        if let Some(key) = &memo_key
            && let Some(v) = self.prompt_fp_cache.get(key).and_then(Value::as_str)
        {
            return Some(v.into());
        }
        let content = match &prompt {
            Prompt::Text(s) => Ok(s.clone()),
            Prompt::File(p) => fs::read(p).map(|b| String::from_utf8_lossy(&b).into_owned()),
        };
        match content {
            Ok(s) => {
                let fp = prompt_fingerprint(&s);
                if let Some(k) = memo_key {
                    self.prompt_fp_cache.insert(k, json!(fp));
                }
                Some(fp)
            }
            Err(e) => {
                let p = match prompt {
                    Prompt::Text(s) => s,
                    Prompt::File(p) => ps(&p),
                };
                self.warnings.push(format!("could not read extraction prompt {} ({}); semantic cache entries cannot be attributed to a prompt version and fall back to the unversioned layout, so this run may replay entries from an older extraction prompt (#1939).",python_compat::repr(&json!(p)),crate::io_compat::read_error(&e,Path::new(&p))));
                None
            }
        }
    }
    pub fn ensure_index(&mut self, root: &Path, cache_root: Option<&Path>) -> Result<()> {
        let new_root = resolve(cache_root.unwrap_or(root))?;
        if let Some(current) = self.stat_index_root.clone() {
            if current == new_root {
                return Ok(());
            }
            // A later call naming a different cache root rebinds (#3989): the
            // outgoing root's pending entries go to its own file first, so no
            // root's stat-index.json ever holds another project's paths.
            self.warnings.push(format!(
                "stat index switched from cache root {} to {} within one process; each root's stat-index.json now only reflects its own files, but library callers crossing project roots in one process should still use a fresh process per root to avoid losing the fastpath (#3989).",
                python_compat::repr(&json!(ps(&current))),
                python_compat::repr(&json!(ps(&new_root)))
            ));
            self.flush();
        }
        let anchor = resolve(root)?;
        let p = self.stat_file(&new_root)?;
        self.stat_index_root = Some(new_root);
        self.stat_index_anchor = Some(anchor.clone());
        self.stat_index_path = Some(p.clone());
        self.stat_index.clear();
        if let Ok(s) = fs::read_to_string(&p)
            && let Ok(Value::Object(raw)) = serde_json::from_str(&s)
        {
            for (k, v) in raw {
                if Path::new(&k).is_absolute() {
                    self.stat_index.entry(k).or_insert(v);
                } else {
                    self.stat_index.insert(stat_absolute(&k, &anchor), v);
                }
            }
        }
        self.stat_index_dirty = false;
        Ok(())
    }
    pub fn flush(&mut self) {
        if !self.stat_index_dirty {
            return;
        }
        let Some(root) = self.stat_index_root.as_ref() else {
            return;
        };
        if !root.is_dir() {
            self.stat_index_dirty = false;
            return;
        }
        let Some(path) = self
            .stat_index_path
            .clone()
            .or_else(|| self.stat_file(root).ok())
        else {
            self.stat_index_dirty = false;
            return;
        };
        let mut on_disk = Map::new();
        for (k, v) in &self.stat_index {
            if Path::new(k).exists() {
                let key = self
                    .stat_index_anchor
                    .as_ref()
                    .map(|a| stat_relative(k, a))
                    .unwrap_or(k.clone());
                on_disk.insert(key, v.clone());
            }
        }
        let result = (|| -> Result<()> {
            fs::create_dir_all(path.parent().unwrap()).map_err(crate::Error::from)?;
            let mut data = serde_json::to_vec(&on_disk).map_err(crate::Error::from)?; // The disk layout uses ASCII escaping like json.dumps.
            if data.iter().any(|b| *b >= 128) {
                data = compact_ascii(&Value::Object(on_disk))?;
            }
            atomic_write(&path, &data, "stat-index.")
        })();
        let _ = result;
        self.stat_index_dirty = false;
    }
    pub fn stat_entry(&mut self, key: &str, st: Stat, at: i64) -> &mut Value {
        let e = self
            .stat_index
            .entry(key.to_string())
            .or_insert(Value::Null);
        if !e.is_object() || e["size"] != json!(st.size) || e["mtime_ns"] != json!(st.mtime_ns) {
            *e = json!({"size":st.size,"mtime_ns":st.mtime_ns});
        }
        e["indexed_at_ns"] = json!(at);
        e
    }
    pub fn file_hash(
        &mut self,
        path: &Path,
        root: &Path,
        cache_root: Option<&Path>,
    ) -> Result<String> {
        let p = normalize(path);
        let root = normalize(root);
        if !p.is_file() {
            return Err(format!("file_hash requires a file, got: {}", ps(&p)).into());
        }
        self.ensure_index(&root, cache_root)?;
        let probe = self.probe_hash(&p, &root, &HashMemo::default());
        self.apply_hash(probe)
    }
    /// [`Self::file_hash`] for many files: the stats, reads and SHA-256 run on
    /// up to `workers` threads, then the stat-index updates are applied in
    /// `paths` order, so the index holds what hashing the files one by one
    /// would leave (up to the observation timestamps).
    pub fn file_hashes(
        &mut self,
        paths: &[&Path],
        root: &Path,
        cache_root: Option<&Path>,
        workers: usize,
    ) -> Vec<Result<String>> {
        let mut out = Vec::with_capacity(paths.len());
        // One by one until a file has loaded the index, so the first
        // `ensure_index` sees exactly what sequential hashing would.
        let mut next = 0;
        while next < paths.len() && self.stat_index_root.is_none() {
            out.push(self.file_hash(paths[next], root, cache_root));
            next += 1;
        }
        let root = normalize(root);
        let memo = HashMemo::shared(self.granularity());
        let this = &*self;
        let probes = crate::par::map(&paths[next..], workers, |path| {
            let p = normalize(path);
            if !p.is_file() {
                return HashProbe::failed(
                    format!("file_hash requires a file, got: {}", ps(&p)).into(),
                );
            }
            this.probe_hash(&p, &root, &memo)
        });
        out.extend(probes.into_iter().map(|probe| self.apply_hash(probe)));
        out
    }
    /// The read-only part of [`Self::file_hash`] (after its file check and
    /// index load): the digest, plus the stat-index change it implies, which
    /// [`Self::apply_hash`] records.
    pub fn probe_hash(&self, p: &Path, root: &Path, memo: &HashMemo) -> HashProbe {
        let probe = || -> Result<HashProbe> {
            let resolved = memo.resolve_file(p)?;
            let abs_key = ps(&resolved);
            let resolved_root = memo.resolve_root(root)?;
            let salt = if let Ok(rel) = resolved.strip_prefix(&resolved_root) {
                let walked = absolute(p)?;
                let walked_root = absolute(root)?;
                let rel = if let Ok(r) = walked.strip_prefix(&walked_root) {
                    r.into()
                } else {
                    walked
                        .ancestors()
                        .skip(1)
                        .find(|parent| resolve(parent).is_ok_and(|p| p == resolved_root))
                        .and_then(|parent| walked.strip_prefix(parent).ok())
                        .unwrap_or(rel)
                        .to_path_buf()
                };
                ps(&rel)
                    .replace(std::path::MAIN_SEPARATOR, "/")
                    .to_lowercase()
            } else {
                abs_key
                    .replace(std::path::MAIN_SEPARATOR, "/")
                    .to_lowercase()
            };
            let st = Stat::read(p).ok();
            if let Some(st) = st
                && let Some(entry) = self.stat_index.get(&abs_key)
                && stat_fresh(entry, st, memo.granularity(self))
                && let Some(hash) = entry
                    .get("hashes")
                    .and_then(|v| v.get(&salt))
                    .and_then(Value::as_str)
            {
                return Ok(HashProbe {
                    result: Ok(hash.into()),
                    update: None,
                    read_source: false,
                });
            }
            let at = self.now();
            let raw = fs::read(p).map_err(|e| crate::io_compat::read_error(&e, p))?;
            let content = if p
                .extension()
                .is_some_and(|s| s.to_string_lossy().to_lowercase() == "md")
            {
                body_content(&raw)
            } else {
                raw
            };
            let mut h = Sha256::new();
            h.update(content);
            h.update([0]);
            h.update(salt.as_bytes());
            let digest = format!("{:x}", h.finalize());
            Ok(HashProbe {
                result: Ok(digest),
                update: st.map(|stat| StatUpdate {
                    key: abs_key,
                    salt,
                    stat,
                    at,
                }),
                read_source: true,
            })
        };
        probe().unwrap_or_else(HashProbe::failed)
    }
    /// Record a [`HashProbe`] in the stat index and counters; yields its digest.
    pub fn apply_hash(&mut self, probe: HashProbe) -> Result<String> {
        if probe.read_source {
            self.source_reads += 1;
        }
        let digest = probe.result?;
        if let Some(u) = probe.update {
            let entry = self.stat_entry(&u.key, u.stat, u.at);
            if !entry["hashes"].is_object() {
                entry["hashes"] = json!({});
            }
            entry["hashes"][u.salt] = json!(digest);
            entry.as_object_mut().unwrap().shift_remove("hash");
            self.stat_index_dirty = true;
        }
        Ok(digest)
    }
    pub fn cached_word_count(
        &mut self,
        path: &Path,
        root: &Path,
        compute: impl FnOnce(&Path) -> Result<Value>,
        cache_root: Option<&Path>,
    ) -> Result<Value> {
        let lookup = self.word_count_lookup(path, root, cache_root)?;
        match lookup {
            WordCountLookup::Hit { value } => Ok(value),
            WordCountLookup::Missing { ticket } => {
                let n = compute(path)?;
                self.complete_word_count(ticket, n)
            }
        }
    }
    pub fn word_count_lookup(
        &mut self,
        path: &Path,
        root: &Path,
        cache_root: Option<&Path>,
    ) -> Result<WordCountLookup> {
        let p = normalize(path);
        let root = normalize(root);
        self.ensure_index(&root, cache_root)?;
        let key = ps(&resolve(&p)?);
        let st = Stat::read(&p).ok();
        if let Some(st) = st
            && let Some(e) = self.stat_index.get(&key)
            && stat_fresh(e, st, self.granularity())
            && let Some(n) = e.get("word_count")
        {
            return Ok(WordCountLookup::Hit { value: n.clone() });
        }
        Ok(WordCountLookup::Missing {
            ticket: WordCountTicket {
                key,
                st,
                observed_at_ns: self.now(),
            },
        })
    }
    pub fn complete_word_count(&mut self, ticket: WordCountTicket, n: Value) -> Result<Value> {
        self.computed_word_counts += 1;
        if let Some(st) = ticket.st {
            self.stat_entry(&ticket.key, st, ticket.observed_at_ns)["word_count"] = n.clone();
            self.stat_index_dirty = true;
        }
        Ok(n)
    }
    /// [`Self::cached_word_count`] for many files: lookups and counting run on
    /// up to `workers` threads, then the index updates are applied in `paths`
    /// order, stopping at (and returning) the first failed lookup, as a
    /// sequential loop would.
    pub fn cached_word_counts(
        &mut self,
        paths: &[&Path],
        root: &Path,
        cache_root: Option<&Path>,
        workers: usize,
        compute: impl Fn(&Path) -> Value + Sync,
    ) -> Result<Vec<Value>> {
        if paths.is_empty() {
            return Ok(vec![]);
        }
        self.ensure_index(&normalize(root), cache_root)?;
        let memo = HashMemo::shared(self.granularity());
        let this = &*self;
        let looked = crate::par::map(paths, workers, |path| -> Result<WordCount> {
            let p = normalize(path);
            let key = ps(&memo.resolve_file(&p)?);
            let st = Stat::read(&p).ok();
            if let Some(st) = st
                && let Some(e) = this.stat_index.get(&key)
                && stat_fresh(e, st, memo.granularity(this))
                && let Some(n) = e.get("word_count")
            {
                return Ok(WordCount::Hit(n.clone()));
            }
            let ticket = WordCountTicket {
                key,
                st,
                observed_at_ns: this.now(),
            };
            Ok(WordCount::Counted(ticket, compute(path)))
        });
        let mut out = Vec::with_capacity(paths.len());
        for looked in looked {
            out.push(match looked? {
                WordCount::Hit(n) => n,
                WordCount::Counted(ticket, n) => self.complete_word_count(ticket, n)?,
            });
        }
        Ok(out)
    }
    pub fn load(&mut self, path: &Path, options: &CacheOptions) -> Result<Option<Value>> {
        let Some(location) = self.locate(path, options)? else {
            return Ok(None);
        };
        let parsed = Self::parse_located(&location, path, options)?;
        Ok(self.record_located(&location, parsed))
    }
    /// Where the cache entry for `path` lives, if one exists. This is the
    /// part of [`Self::load`] that touches cache state (content hash index,
    /// prompt fingerprint, directory layout); reading the entry itself is the
    /// pure [`Self::parse_located`], so many entries can be parsed in parallel
    /// after locating them one by one.
    pub fn locate(
        &mut self,
        path: &Path,
        options: &CacheOptions,
    ) -> Result<Option<CacheEntryLocation>> {
        let Ok(hash) = self.file_hash(path, &options.root, options.cache_root.as_deref()) else {
            return Ok(None);
        };
        let hash = salted_key(&hash, options.salt.as_deref());
        let fp = self.resolve_prompt(options.prompt.as_ref(), options.prompt_file.as_deref());
        let location = options.cache_root.as_deref().unwrap_or(&options.root);
        let mut entry = self
            .cache_dir(location, &options.kind, fp.as_deref())?
            .join(format!("{hash}.json"));
        let mut legacy = false;
        if fp.is_some() && !entry.exists() && options.allow_legacy {
            let p = self
                .cache_dir(location, &options.kind, None)?
                .join(format!("{hash}.json"));
            if p.exists() {
                entry = p;
                legacy = true;
            }
        }
        if !entry.exists() {
            return Ok(None);
        }
        Ok(Some(CacheEntryLocation { entry, legacy }))
    }
    /// Read and validate a located entry without touching cache state.
    pub fn parse_located(
        location: &CacheEntryLocation,
        path: &Path,
        options: &CacheOptions,
    ) -> Result<LocatedEntry> {
        let data = match fs::read_to_string(&location.entry) {
            Ok(d) => d,
            Err(e) if e.kind() == std::io::ErrorKind::InvalidData => return Err(e.into()),
            Err(_) => return Ok(LocatedEntry::Miss),
        };
        let mut result: Value = match serde_json::from_str(&data) {
            Ok(d) => d,
            Err(_) => return Ok(LocatedEntry::Corrupt),
        };
        drop(data);
        if !options.allow_partial && result.is_object() {
            if truth(&result["partial"]) {
                return Ok(LocatedEntry::Miss);
            }
            if options.kind.starts_with("semantic")
                && !truth(&result["nodes"])
                && !truth(&result["hyperedges"])
            {
                return Ok(LocatedEntry::Miss);
            }
        }
        if options.kind.starts_with("semantic")
            && result.is_object()
            && !semantic_matches(&result, path, &options.root)?
        {
            return Ok(LocatedEntry::Miss);
        }
        if result.is_object() {
            // A source under a walked directory alias can carry ROOT_MARKER
            // too. Restore it before treating ordinary relative paths as such.
            restore_entry_paths(&mut result, path, &options.root);
            // A recorded target file that is gone makes the replayed edges
            // name a path the run cannot remap: re-extract instead (#4185).
            let targets = result
                .as_object_mut()
                .and_then(|m| m.shift_remove(CACHED_TARGETS_KEY));
            if let Some(Value::Array(targets)) = targets
                && !targets
                    .iter()
                    .filter_map(Value::as_str)
                    .all(|t| target_file_present(t, &options.root))
            {
                return Ok(LocatedEntry::Miss);
            }
        }
        Ok(LocatedEntry::Hit(result))
    }
    /// Account a parsed entry in this cache's counters and yield the hit.
    pub fn record_located(
        &mut self,
        location: &CacheEntryLocation,
        parsed: LocatedEntry,
    ) -> Option<Value> {
        match parsed {
            LocatedEntry::Hit(value) => {
                if location.legacy {
                    self.legacy_semantic_hits += 1;
                }
                Some(value)
            }
            LocatedEntry::Corrupt => {
                self.corrupt_cache_entries += 1;
                None
            }
            LocatedEntry::Miss => None,
        }
    }
    /// Remove only the current content's entries in this prompt/mode namespace.
    /// Rejected extraction results must not become permanent cache hits on retry.
    pub fn invalidate(&mut self, path: &Path, options: &CacheOptions) -> Result<()> {
        let hash = self.file_hash(path, &options.root, options.cache_root.as_deref())?;
        let fp = self.resolve_prompt(options.prompt.as_ref(), options.prompt_file.as_deref());
        let location = options.cache_root.as_deref().unwrap_or(&options.root);
        let mut directories = vec![self.cache_dir(location, &options.kind, fp.as_deref())?];
        if fp.is_some() && options.allow_legacy {
            directories.push(self.cache_dir(location, &options.kind, None)?);
        }
        for directory in directories {
            match fs::remove_file(directory.join(format!("{hash}.json"))) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }

    pub fn save(&mut self, path: &Path, result: &Value, options: &CacheOptions) -> Result<()> {
        if !path.is_file() {
            return Ok(());
        }
        let bytes = Self::encode_entry(path, result, options)?;
        self.save_encoded(path, &bytes, options, None)
    }
    /// The on-disk bytes [`Self::save`] writes for `result`: the per-file
    /// record with sources and ids made relative to the root. Pure, so
    /// extraction workers can encode their own records.
    pub fn encode_entry(path: &Path, result: &Value, options: &CacheOptions) -> Result<Vec<u8>> {
        let mut on_disk = result.clone();
        if on_disk.is_object() {
            if options.kind == "ast" {
                let targets = present_target_files(&on_disk, &options.root);
                if !targets.is_empty() {
                    on_disk[CACHED_TARGETS_KEY] = json!(targets);
                }
            }
            relativize_sources(&mut on_disk, &options.root);
            relativize_ids(&mut on_disk, path, &options.root);
        }
        let mut bytes = vec![];
        python_json::write(&mut bytes, &on_disk, None, true).map_err(crate::Error::from)?;
        Ok(bytes)
    }
    /// Write bytes from [`Self::encode_entry`] as the entry for `path`.
    pub fn save_encoded(
        &mut self,
        path: &Path,
        bytes: &[u8],
        options: &CacheOptions,
        syntax: Option<&[u8]>,
    ) -> Result<()> {
        if !path.is_file() {
            return Ok(());
        }
        let hash = salted_key(
            &self.file_hash(path, &options.root, options.cache_root.as_deref())?,
            options.salt.as_deref(),
        );
        let fp = self.resolve_prompt(options.prompt.as_ref(), options.prompt_file.as_deref());
        let dir = self.cache_dir(
            options.cache_root.as_deref().unwrap_or(&options.root),
            &options.kind,
            fp.as_deref(),
        )?;
        let entry = dir.join(format!("{hash}.json"));
        atomic_write(&entry, bytes, &format!("{hash}."))?;
        if let Some(syntax) = syntax
            && let Some(dir) = self.syntax_dir(options)
        {
            write_syntax(&dir, &hash, syntax);
        }
        Ok(())
    }
    /// Whether AST entries for `options` can be located and saved without the
    /// prompt/legacy handling: no prompt, so the entry directory is the one
    /// versioned AST directory.
    fn plain_ast(options: &CacheOptions) -> bool {
        options.kind == "ast" && options.prompt.is_none() && options.prompt_file.is_none()
    }
    /// The AST entry directory [`Self::save_encoded`] would write to, once
    /// this cache has loaded its stat index and run [`Self::cache_dir`] for
    /// it (which clears stale versions once per cache). Until then, `None`:
    /// saving must go through [`Self::save_encoded`].
    pub fn shared_ast_dir(&self, options: &CacheOptions) -> Option<PathBuf> {
        if !Self::plain_ast(options) || self.stat_index_root.is_none() {
            return None;
        }
        let location = options.cache_root.as_deref().unwrap_or(&options.root);
        let dir = self
            .base(location)
            .ok()?
            .join("cache")
            .join("ast")
            .join(self.ast_version());
        self.cleaned_ast_dirs.contains(&ps(&dir)).then_some(dir)
    }
    /// [`Self::save_encoded`] from a worker thread, for a `dir` from
    /// [`Self::shared_ast_dir`]: the same file check, hash, directory and
    /// bytes. The stat-index change the hash implies comes back as a probe
    /// for [`Self::apply_hash`] (applied even when the write fails, as the
    /// sequential save records the hash before writing).
    pub fn save_encoded_shared(
        &self,
        path: &Path,
        bytes: &[u8],
        options: &CacheOptions,
        dir: &Path,
        memo: &HashMemo,
        syntax: Option<(&Path, &[u8])>,
    ) -> (Option<HashProbe>, Result<()>) {
        if !path.is_file() {
            return (None, Ok(()));
        }
        let p = normalize(path);
        if !p.is_file() {
            return (
                None,
                Err(format!("file_hash requires a file, got: {}", ps(&p)).into()),
            );
        }
        let mut probe = self.probe_hash(&p, &normalize(&options.root), memo);
        let hash = match std::mem::replace(&mut probe.result, Ok(String::new())) {
            Ok(hash) => {
                probe.result = Ok(hash.clone());
                salted_key(&hash, options.salt.as_deref())
            }
            Err(e) => return (None, Err(e)),
        };
        let written = fs::create_dir_all(dir)
            .map_err(crate::Error::from)
            .and_then(|()| {
                atomic_write(
                    &dir.join(format!("{hash}.json")),
                    bytes,
                    &format!("{hash}."),
                )
            });
        if written.is_ok()
            && let Some((syntax_dir, syntax)) = syntax
        {
            write_syntax(syntax_dir, &hash, syntax);
        }
        (Some(probe), written)
    }
    /// [`Self::load`] for many AST entries with `post` applied to each hit:
    /// the hashing and entry reads run on up to `workers` threads, and the
    /// stat-index updates and counters are applied in `paths` order, as a
    /// sequential loop would. `Ok(None)` is a miss.
    pub fn load_many(
        &mut self,
        paths: &[&Path],
        options: &CacheOptions,
        workers: usize,
        post: impl Fn(&Path, &Path, &mut Value) + Sync,
    ) -> Vec<Result<Option<Value>>> {
        self.load_many_salted(paths, options, workers, |_| options.salt.clone(), post)
    }
    /// [`Self::load_many`] with each path's own key `salt` (`cache_salt`),
    /// computed on the calling thread.
    pub fn load_many_salted(
        &mut self,
        paths: &[&Path],
        options: &CacheOptions,
        workers: usize,
        salt: impl Fn(&Path) -> Option<String>,
        post: impl Fn(&Path, &Path, &mut Value) + Sync,
    ) -> Vec<Result<Option<Value>>> {
        let salts = paths.iter().map(|p| salt(p)).collect::<Vec<_>>();
        if !Self::plain_ast(options) {
            // Prompted or semantic entries: locate one by one (prompt
            // fingerprints, legacy layout), then parse in parallel.
            let located = paths
                .iter()
                .zip(&salts)
                .map(|(p, salt)| {
                    let options = CacheOptions {
                        salt: salt.clone(),
                        ..options.clone()
                    };
                    self.locate(p, &options)
                })
                .collect::<Vec<_>>();
            let parsed = crate::par::map(
                &located.iter().zip(paths).collect::<Vec<_>>(),
                workers,
                |(location, path)| match location {
                    Ok(Some(location)) => Some(Self::parse_located(location, path, options).map(
                        |mut entry| {
                            if let LocatedEntry::Hit(value) = &mut entry {
                                post(path, &location.entry, value);
                            }
                            entry
                        },
                    )),
                    _ => None,
                },
            );
            return located
                .into_iter()
                .zip(parsed)
                .map(|(location, parsed)| match (location, parsed) {
                    (Ok(Some(location)), Some(parsed)) => {
                        parsed.map(|entry| self.record_located(&location, entry))
                    }
                    (Ok(_), _) => Ok(None),
                    (Err(e), _) => Err(e),
                })
                .collect();
        }
        let hashes = self.file_hashes(paths, &options.root, options.cache_root.as_deref(), workers);
        let location = options.cache_root.as_deref().unwrap_or(&options.root);
        // `locate` resolves the entry directory after each successful hash;
        // it is the same directory every time, created (and stale versions
        // cleared) by the first call.
        let dir = hashes
            .iter()
            .any(Result::is_ok)
            .then(|| self.cache_dir(location, &options.kind, None));
        let entries = hashes
            .into_iter()
            .zip(&salts)
            .map(|(hash, salt)| match (hash, &dir) {
                (Err(_), _) | (_, None) => Ok(None),
                (Ok(hash), Some(Ok(dir))) => Ok(Some(
                    dir.join(format!("{}.json", salted_key(&hash, salt.as_deref()))),
                )),
                (Ok(_), Some(Err(e))) => Err(crate::Error::Internal(e.to_string())),
            })
            .collect::<Vec<_>>();
        let parsed = crate::par::map(
            &entries.iter().zip(paths).collect::<Vec<_>>(),
            workers,
            |(entry, path)| match entry {
                Ok(Some(entry)) if entry.exists() => {
                    let location = CacheEntryLocation {
                        entry: entry.clone(),
                        legacy: false,
                    };
                    Some(
                        Self::parse_located(&location, path, options).map(|mut entry| {
                            if let LocatedEntry::Hit(value) = &mut entry {
                                post(path, &location.entry, value);
                            }
                            (location, entry)
                        }),
                    )
                }
                _ => None,
            },
        );
        entries
            .into_iter()
            .zip(parsed)
            .map(|(entry, parsed)| {
                entry?;
                match parsed {
                    Some(Ok((location, entry))) => Ok(self.record_located(&location, entry)),
                    Some(Err(e)) => Err(e),
                    None => Ok(None),
                }
            })
            .collect()
    }
    fn cache_entries(&self, root: &Path, kinds: &[&str], flat: bool) -> Result<Vec<PathBuf>> {
        let base = self.base(root)?.join("cache");
        let mut entries = vec![];
        if flat {
            json_files(&base, false, &mut entries)
        }
        for kind in kinds {
            json_files(&base.join(kind), true, &mut entries);
        }
        Ok(entries)
    }
    pub fn cached_files(&self, root: &Path) -> Result<BTreeSet<String>> {
        Ok(self
            .cache_entries(root, &["ast", "semantic", "semantic-deep"], true)?
            .iter()
            .filter_map(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned()))
            .collect())
    }
    pub fn clear(&self, root: &Path) -> Result<()> {
        for f in self.cache_entries(root, &["ast", "semantic", "semantic-deep"], true)? {
            fs::remove_file(f).map_err(crate::Error::from)?;
        }
        Ok(())
    }
    pub fn prune(&self, root: &Path, live: &HashSet<String>) -> Result<usize> {
        let mut pruned = 0;
        for p in self.cache_entries(root, &["semantic", "semantic-deep"], false)? {
            let stem = p.file_stem().unwrap_or_default().to_string_lossy();
            if !live.contains(stem.as_ref()) && fs::remove_file(p).is_ok() {
                pruned += 1
            }
        }
        Ok(pruned)
    }
    pub fn check_semantic(&mut self, files: &[String], options: &SemanticOptions) -> Result<Value> {
        let opt = options.cache_options();
        let mut nodes = vec![];
        let mut edges = vec![];
        let mut hyper = vec![];
        let mut uncached = vec![];
        let before = self.legacy_semantic_hits;
        let corrupt = self.corrupt_cache_entries;
        for f in files {
            let p = Path::new(f);
            let p = if p.is_absolute() {
                p.into()
            } else {
                options.root.join(p)
            };
            if let Some(result) = self.load(&p, &opt)? {
                nodes.extend_from_slice(ar(&result["nodes"]));
                edges.extend_from_slice(ar(&result["edges"]));
                hyper.extend_from_slice(ar(&result["hyperedges"]));
            } else {
                uncached.push(f.clone())
            }
        }
        let n = self.legacy_semantic_hits - before;
        if n > 0 {
            self.warnings.push(format!("{n} semantic cache entr{} predate extraction-prompt fingerprinting and were written by an unknown prompt version; they were replayed as-is, so this graph may mix extraction vintages. Re-run with --force (or GRAPHICLE_FORCE=1) to re-extract them with the current prompt (#1939).",if n==1{"y"}else{"ies"}));
        }
        let n = self.corrupt_cache_entries - corrupt;
        if n > 0 {
            self.warnings.push(format!("{n} semantic cache entr{} could not be parsed as JSON and were treated as misses, so those files were re-extracted. A corrupt entry stays on disk and fails again every run; run with --force (or GRAPHICLE_FORCE=1) to rewrite them, or clear the cache to stop paying for the re-extraction (#2405).",if n==1{"y"}else{"ies"}));
        }
        Ok(json!([nodes, edges, hyper, uncached]))
    }
    pub fn save_semantic(
        &mut self,
        nodes: &[Value],
        edges: &[Value],
        hyper: &[Value],
        options: &SemanticOptions,
    ) -> Result<usize> {
        let matcher = SourceMatcher::new(&options.root)?;
        let mut groups = Map::<String, Value>::new();
        for (bucket, items) in [("nodes", nodes), ("edges", edges), ("hyperedges", hyper)] {
            for item in items {
                let Some(src) = item
                    .get("source_file")
                    .filter(|s| truth(s))
                    .and_then(Value::as_str)
                else {
                    continue;
                };
                let norm = matcher.normalized(src);
                let mut item = item.clone();
                if norm != src {
                    item["source_file"] = json!(norm);
                }
                groups.entry(norm).or_insert_with(empty_group)[bucket]
                    .as_array_mut()
                    .unwrap()
                    .push(item);
            }
        }
        let allowed = options
            .allowed_source_files
            .as_ref()
            .map(|p| {
                p.iter()
                    .map(|p| matcher.identity(p))
                    .collect::<Result<BTreeSet<_>>>()
            })
            .transpose()?;
        let partial = options
            .partial_source_files
            .as_ref()
            .map(|p| {
                p.iter()
                    .map(|p| matcher.identity(p))
                    .collect::<Result<BTreeSet<_>>>()
            })
            .transpose()?;
        let recover = |f: &str| -> Result<(PathBuf, PathBuf)> {
            let mut path = matcher.identity(f)?;
            let mut resolved = resolve(&path).or_else(|_| absolute(&path))?;
            if !resolved.is_file()
                && let Some(allowed) = &allowed
            {
                let matches = allowed
                    .iter()
                    .filter(|p| p.file_name() == path.file_name())
                    .collect::<Vec<_>>();
                if matches.len() == 1 {
                    path = matches[0].clone();
                    resolved = path.clone();
                }
            }
            let normalized = normalize(&path);
            Ok((normalized, resolved))
        };
        if let Some(partial) = &partial {
            let present = groups
                .keys()
                .map(|k| matcher.identity(k))
                .collect::<Result<BTreeSet<_>>>()?;
            for path in partial {
                if !present.contains(path) {
                    groups.entry(ps(path)).or_insert_with(empty_group);
                }
            }
        }
        let skipped = |f: &str| -> Result<bool> {
            let (path, resolved) = recover(f)?;
            Ok(!resolved.is_file()
                || allowed
                    .as_ref()
                    .is_some_and(|allowed| !allowed.contains(&path)))
        };
        if allowed.is_some() {
            let mut dropped = BTreeSet::new();
            let mut written = BTreeSet::new();
            for (f, result) in &groups {
                let target = if skipped(f)? {
                    &mut dropped
                } else {
                    &mut written
                };
                for n in ar(&result["nodes"]) {
                    if !n["id"].is_null()
                        && let Some(k) = python_compat::scalar_key(&n["id"])
                    {
                        target.insert(k);
                    }
                }
            }
            dropped.retain(|id| !written.contains(id));
            if !dropped.is_empty() {
                for (f, result) in &mut groups {
                    if !skipped(f)? {
                        prune_dangling(result, &dropped);
                    }
                }
            }
        }
        let mut saved = 0;
        let mut not_file = 0;
        let group_count = groups.len();
        for (f, mut result) in groups {
            let (path, p) = recover(&f)?;
            if !p.is_file() {
                not_file += 1;
                continue;
            }
            if path != matcher.identity(&f)? {
                let corrected = matcher.normalized(&ps(&path));
                for bucket in ["nodes", "edges", "hyperedges"] {
                    for item in result[bucket].as_array_mut().unwrap() {
                        item["source_file"] = json!(corrected);
                    }
                }
            }
            if allowed
                .as_ref()
                .is_some_and(|allowed| !allowed.contains(&path))
            {
                self.warnings.push(format!("semantic cache skipped out-of-scope source_file {}; the file was not dispatched for extraction",python_compat::repr(&json!(f))));
                continue;
            }
            let mut prev_partial = false;
            let mut opt = options.cache_options();
            if options.merge_existing {
                opt.allow_legacy = false;
                opt.allow_partial = true;
                if let Some(prev) = self.load(&path, &opt)?.filter(truth) {
                    prev_partial = truth(&prev["partial"]);
                    for bucket in ["nodes", "edges", "hyperedges"] {
                        let mut items = ar(&prev[bucket]).to_vec();
                        items.extend_from_slice(ar(&result[bucket]));
                        result[bucket] = json!(items);
                    }
                }
            }
            let partial = partial.as_ref().is_some_and(|p| p.contains(&path))
                || group_partial(&result)
                || prev_partial;
            if partial {
                result["partial"] = json!(true)
            }
            if !partial && !truth(&result["nodes"]) && !truth(&result["hyperedges"]) {
                continue;
            }
            self.save(&path, &result, &opt)?;
            saved += 1;
        }
        if not_file > 0 && not_file == group_count {
            self.warnings.push(format!("save_semantic_cache: all {not_file} source_file group(s) were skipped because their paths do not resolve to real files. This usually means ``root`` is anchored to the wrong directory (e.g. the --out directory instead of the corpus root). Pass the corpus directory as ``root`` and the output directory as ``cache_root`` (#1991)."));
        }
        Ok(saved)
    }
}
/// On-disk position of one cache entry, from [`ExtractionCache::locate`].
#[derive(Clone, Debug)]
pub struct CacheEntryLocation {
    pub entry: PathBuf,
    /// Found under the unversioned prompt layout of an older extraction.
    pub legacy: bool,
}
/// Outcome of parsing a located entry, from [`ExtractionCache::parse_located`].
#[derive(Debug)]
pub enum LocatedEntry {
    Hit(Value),
    Corrupt,
    Miss,
}
/// Own this guard for an embedded run; dropping it flushes the stat index.
/// The standalone application flushes its process-wide instance at shutdown.
#[derive(Default)]
pub struct CacheSession(pub ExtractionCache);
impl std::ops::Deref for CacheSession {
    type Target = ExtractionCache;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl std::ops::DerefMut for CacheSession {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}
impl Drop for CacheSession {
    fn drop(&mut self) {
        self.0.flush()
    }
}
#[derive(serde::Serialize, serde::Deserialize)]
pub struct WordCountTicket {
    pub key: String,
    pub st: Option<Stat>,
    pub observed_at_ns: i64,
}
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum WordCountLookup {
    Hit { value: Value },
    Missing { ticket: WordCountTicket },
}
/// One file's outcome in [`ExtractionCache::cached_word_counts`].
enum WordCount {
    Hit(Value),
    Counted(WordCountTicket, Value),
}
fn empty_group() -> Value {
    json!({"nodes":[],"edges":[],"hyperedges":[]})
}
pub fn group_partial(v: &Value) -> bool {
    ["nodes", "edges", "hyperedges"].iter().any(|k| {
        ar(&v[*k])
            .iter()
            .any(|v| v.is_object() && truth(&v["_partial"]))
    })
}
#[derive(Default, Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct SemanticOptions {
    pub root: PathBuf,
    pub cache_root: Option<PathBuf>,
    pub mode: Option<String>,
    pub prompt: Option<Prompt>,
    pub prompt_file: Option<PathBuf>,
    pub merge_existing: bool,
    pub allowed_source_files: Option<Vec<String>>,
    pub partial_source_files: Option<Vec<String>>,
}
impl SemanticOptions {
    pub fn cache_options(&self) -> CacheOptions {
        CacheOptions {
            root: self.root.clone(),
            kind: self
                .mode
                .as_ref()
                .map(|s| format!("semantic-{s}"))
                .unwrap_or("semantic".into()),
            cache_root: self.cache_root.clone(),
            prompt: self.prompt.clone(),
            prompt_file: self.prompt_file.clone(),
            ..Default::default()
        }
    }
}
fn compact_ascii(v: &Value) -> Result<Vec<u8>> {
    let mut full = vec![];
    python_json::write(&mut full, v, None, true).map_err(crate::Error::from)?;
    let mut in_string = false;
    let mut escape = false;
    let mut out = vec![];
    for b in full {
        if b == b'"' && !escape {
            in_string = !in_string;
        }
        if in_string || b != b' ' {
            out.push(b);
        }
        escape = b == b'\\' && !escape;
    }
    Ok(out)
}
/// Write a file's syntax facts beside its AST entry (see
/// [`ExtractionCache::syntax_dir`]). They are an optimisation: a failed
/// write only means the next run parses the file.
fn write_syntax(dir: &Path, hash: &str, bytes: &[u8]) {
    let _ = fs::create_dir_all(dir)
        .map_err(crate::Error::from)
        .and_then(|()| {
            atomic_write(
                &dir.join(format!("{hash}.json")),
                bytes,
                &format!("{hash}."),
            )
        });
}
fn atomic_write(path: &Path, bytes: &[u8], prefix: &str) -> Result<()> {
    let parent = path.parent().ok_or("cache entry has no parent")?;
    let mut f = tempfile::Builder::new()
        .prefix(prefix)
        .suffix(".tmp")
        .tempfile_in(parent)
        .map_err(crate::Error::from)?;
    f.write_all(bytes).map_err(crate::Error::from)?;
    let temp = f.into_temp_path();
    paths::os_replace_with_fallback(&temp, path).map_err(crate::Error::from)?;
    Ok(())
}
fn json_files(dir: &Path, recursive: bool, files: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.extension().is_some_and(|s| s == "json") {
            files.push(p.clone())
        }
        if recursive
            && p.is_dir()
            && !fs::symlink_metadata(&p).is_ok_and(|s| s.file_type().is_symlink())
        {
            json_files(&p, true, files)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn embedded_session_flushes_when_its_owner_leaves_scope() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("a.txt");
        fs::write(&file, "text").unwrap();
        let index = root.path().join("cache-output/cache/stat-index.json");
        {
            let mut session = CacheSession::default();
            session.output = "cache-output".into();
            session.file_hash(&file, root.path(), None).unwrap();
            assert!(!index.exists());
        }
        let stored: Value = serde_json::from_slice(&fs::read(index).unwrap()).unwrap();
        assert!(stored.get("a.txt").is_some());
    }

    /// A tree with plain files, a Markdown front-matter file, a symlinked
    /// directory, a duplicate path, a missing path and a directory.
    fn corpus() -> (tempfile::TempDir, Vec<PathBuf>) {
        let root = tempfile::tempdir().unwrap();
        let src = root.path().join("src");
        fs::create_dir_all(src.join("deep/er")).unwrap();
        let mut paths = vec![];
        for i in 0..40 {
            let p = src
                .join(if i % 3 == 0 { "deep/er" } else { "." })
                .join(format!("f{i}.py"));
            fs::write(&p, format!("x = {i}\n")).unwrap();
            paths.push(p);
        }
        fs::write(
            src.join("notes.md"),
            "---\ntitle: t\n---\nbody words here\n",
        )
        .unwrap();
        paths.push(src.join("notes.md"));
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(src.join("deep"), root.path().join("linked")).unwrap();
            paths.push(root.path().join("linked/er/f0.py"));
        }
        paths.push(paths[1].clone());
        paths.push(src.join("missing.py"));
        paths.push(src.join("deep"));
        (root, paths)
    }

    /// The stat index as written, without the observation clock.
    fn index_without_clock(cache: &ExtractionCache) -> Value {
        let mut index = Value::Object(cache.stat_index.clone());
        for entry in index.as_object_mut().unwrap().values_mut() {
            entry.as_object_mut().unwrap().shift_remove("indexed_at_ns");
        }
        index
    }

    #[test]
    fn batched_hashes_match_hashing_one_by_one() {
        let (root, paths) = corpus();
        let refs = paths.iter().map(PathBuf::as_path).collect::<Vec<_>>();
        // Old enough that the second pass is served from the index.
        let now = clock_ns() + 10 * MTIME_GRANULARITY_NS;
        for pass in 0..2 {
            let mut serial = ExtractionCache {
                now_ns: Some(now),
                ..Default::default()
            };
            let mut batched = ExtractionCache {
                now_ns: Some(now),
                ..Default::default()
            };
            if pass == 1 {
                for cache in [&mut serial, &mut batched] {
                    cache.ensure_index(root.path(), None).unwrap();
                    for p in &refs {
                        let _ = cache.file_hash(p, root.path(), None);
                    }
                }
            }
            let one = refs
                .iter()
                .map(|p| {
                    serial
                        .file_hash(p, root.path(), None)
                        .map_err(|e| e.to_string())
                })
                .collect::<Vec<_>>();
            let many = batched
                .file_hashes(&refs, root.path(), None, 8)
                .into_iter()
                .map(|r| r.map_err(|e| e.to_string()))
                .collect::<Vec<_>>();
            assert_eq!(one, many, "pass {pass}");
            assert!(one.iter().filter(|r| r.is_err()).count() == 2);
            assert_eq!(index_without_clock(&serial), index_without_clock(&batched));
            assert_eq!(
                serial.stat_index.keys().collect::<Vec<_>>(),
                batched.stat_index.keys().collect::<Vec<_>>()
            );
            // One by one, the repeated path is served by the entry its first
            // occurrence just wrote; a batch hashes both occurrences.
            assert_eq!(
                serial.source_reads + usize::from(pass == 0),
                batched.source_reads,
                "pass {pass}"
            );
        }
    }

    #[test]
    fn batched_word_counts_match_counting_one_by_one() {
        let (root, paths) = corpus();
        let files = paths
            .iter()
            .filter(|p| p.is_file())
            .map(PathBuf::as_path)
            .collect::<Vec<_>>();
        let count = |p: &Path| json!(fs::read(p).map(|b| b.len()).unwrap_or(0));
        let mut serial = ExtractionCache {
            now_ns: Some(clock_ns()),
            ..Default::default()
        };
        let one = files
            .iter()
            .map(|p| {
                serial
                    .cached_word_count(p, root.path(), |p| Ok(count(p)), None)
                    .unwrap()
            })
            .collect::<Vec<_>>();
        let mut batched = ExtractionCache {
            now_ns: serial.now_ns,
            ..Default::default()
        };
        let many = batched
            .cached_word_counts(&files, root.path(), None, 8, count)
            .unwrap();
        assert_eq!(one, many);
        assert_eq!(index_without_clock(&serial), index_without_clock(&batched));
        assert_eq!(serial.computed_word_counts, batched.computed_word_counts);
    }

    #[test]
    fn shared_saves_write_the_entry_the_sequential_save_writes() {
        let (root, paths) = corpus();
        let options = CacheOptions {
            root: root.path().into(),
            cache_root: Some(root.path().into()),
            ..Default::default()
        };
        let record = json!({"nodes":[{"id":"a","source_file":"src/f1.py"}],"edges":[]});
        let path = &paths[1];
        let bytes = ExtractionCache::encode_entry(path, &record, &options).unwrap();
        let mut sequential = ExtractionCache {
            output: "seq-out".into(),
            ..Default::default()
        };
        sequential
            .save_encoded(path, &bytes, &options, None)
            .unwrap();
        let mut shared = ExtractionCache {
            output: "shared-out".into(),
            ..Default::default()
        };
        assert!(shared.shared_ast_dir(&options).is_none());
        // Locating prepares the directory the shared save needs.
        assert!(shared.locate(path, &options).unwrap().is_none());
        let dir = shared.shared_ast_dir(&options).unwrap();
        let memo = HashMemo::shared(shared.granularity());
        let (probe, written) =
            shared.save_encoded_shared(path, &bytes, &options, &dir, &memo, None);
        written.unwrap();
        shared.apply_hash(probe.unwrap()).unwrap();
        let entries = |out: &str| {
            let mut found = vec![];
            json_files(&root.path().join(out).join("cache"), true, &mut found);
            found
                .into_iter()
                .filter(|p| p.file_name().unwrap() != "stat-index.json")
                .map(|p| {
                    (
                        p.strip_prefix(root.path().join(out)).unwrap().to_path_buf(),
                        fs::read(&p).unwrap(),
                    )
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(entries("seq-out"), entries("shared-out"));
        assert_eq!(entries("seq-out").len(), 1);
        assert_eq!(
            index_without_clock(&sequential),
            index_without_clock(&shared)
        );
    }

    #[test]
    fn a_later_root_rebinds_the_stat_index_to_its_own_file() {
        // Graphify 985a017 (#3989): the index bound to the first root forever.
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        fs::write(a.path().join("a.py"), "x = 1\n").unwrap();
        fs::write(b.path().join("b.py"), "y = 2\n").unwrap();
        let now = clock_ns() + 10 * MTIME_GRANULARITY_NS;
        let mut cache = ExtractionCache {
            now_ns: Some(now),
            ..Default::default()
        };
        cache
            .file_hash(&a.path().join("a.py"), a.path(), None)
            .unwrap();
        cache
            .file_hash(&b.path().join("b.py"), b.path(), None)
            .unwrap();
        cache.flush();
        let keys = |root: &Path| {
            let file = cache.stat_file(&resolve(root).unwrap()).unwrap();
            let raw: Map<String, Value> =
                serde_json::from_str(&fs::read_to_string(file).unwrap()).unwrap();
            raw.keys().cloned().collect::<Vec<_>>()
        };
        assert_eq!(keys(a.path()), ["a.py"]);
        assert_eq!(keys(b.path()), ["b.py"]);
        assert!(cache.warnings.iter().any(|w| w.contains("#3989")));
        // Same root again: no rebind and no further warning.
        let warned = cache.warnings.len();
        cache.ensure_index(b.path(), None).unwrap();
        assert_eq!(cache.warnings.len(), warned);
    }
}
