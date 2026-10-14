// Adapted from Graphify 0.9.88 (a9860f70), Apache-2.0; see NOTICE.md.
// Upstream: graphify/extract.py
//! Complete native AST corpus extraction and cross-file assembly.
use crate::{
    Result,
    cache::{self, CacheOptions, ExtractionCache},
    extract_languages as languages, extract_members as members, extract_merge as merge,
    extract_normalize as norm, extract_projects as projects, extract_python,
    extractors::{
        self,
        base::is_language_builtin,
        resolution::{self, field, resolved, s},
        resolution_types as types,
    },
    rcfile::effective_suffix,
    resolver_registry::{self, LanguageResolver},
};
use rustc_hash::{FxHashMap, FxHashSet};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeSet, HashMap},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
};
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ExtractOptions {
    pub root: Option<PathBuf>,
    pub cache_root: Option<PathBuf>,
    pub parallel: bool,
    pub max_workers: Option<usize>,
    /// Read-only nodes and edges of the unchanged corpus (#2406, #2437); a
    /// JSON `null` is the reference's `None` (watch passes `context or None`).
    #[serde(deserialize_with = "null_as_empty")]
    pub resolution_context_nodes: Vec<Value>,
    #[serde(deserialize_with = "null_as_empty")]
    pub resolution_context_edges: Vec<Value>,
    pub use_cache: bool,
    pub include_intermediate: bool,
    /// Opt-in receiver-aware method resolution for Python, TypeScript/JavaScript
    /// and Go (`extractors::receiver_resolution`). Adds `calls` edges tagged
    /// `receiver_evidence` and an `unresolved_calls` list; the parity default
    /// (`false`) leaves the output byte-identical.
    pub receiver_resolution: bool,
    /// Add exact literal Jest/Vitest test registrations and attribute callback
    /// calls to them. Dynamic/duplicate names stay unresolved. Parity is off.
    pub test_identities: bool,
    /// Precise-mode passes beyond the upstream port's (`extractors::precise_python`).
    /// Defaults to `--precise` / `GRAPHICLE_PRECISE=1`; parity is off.
    pub precise: bool,
}
/// Keys of an extraction result that only steer the current run (#2543,
/// #3411). They hold the absolute paths of this run's inputs, so a writer that
/// dumps the raw extraction as graph.json (`--no-cluster`) leaves them out:
/// they put the checkout path and user name into the graph and differ between
/// a first build and a rebuild of the same tree (Graphify 0.9.80).
pub const RUN_ONLY_EXTRACTION_KEYS: [&str; 2] = ["extracted_sources", "failed_sources"];
/// Opt-in precise mode (`--precise` / `GRAPHICLE_PRECISE=1`): resolution passes
/// beyond the upstream port's, which change the graph and so are off by default.
pub static PRECISE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
pub fn precise() -> bool {
    PRECISE.load(std::sync::atomic::Ordering::Relaxed)
        || std::env::var("GRAPHICLE_PRECISE").is_ok_and(|v| v == "1")
}
impl Default for ExtractOptions {
    fn default() -> Self {
        Self {
            root: None,
            cache_root: None,
            parallel: true,
            max_workers: None,
            resolution_context_nodes: vec![],
            resolution_context_edges: vec![],
            use_cache: true,
            include_intermediate: false,
            receiver_resolution: precise(),
            test_identities: false,
            precise: precise(),
        }
    }
}
/// A context list given as JSON `null` is empty.
fn null_as_empty<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Vec<Value>, D::Error> {
    Ok(Option::<Vec<Value>>::deserialize(deserializer)?.unwrap_or_default())
}
pub fn get_extractor(path: &Path) -> Option<String> {
    let name = path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_lowercase();
    if name.ends_with(".blade.php") {
        return Some("blade".into());
    }
    if crate::mcp_ingest::is_mcp_config_path(path) {
        return Some("mcp_config".into());
    }
    if crate::manifest_ingest::is_package_manifest_path(path) {
        return Some("package_manifest".into());
    }
    // A project-declared remap (`language.inc=php` in `.graphiclerc`, #2961)
    // wins over every sniff below: a remapped `.h`/`.m` skips the C++/ObjC
    // probes too.
    let real = path
        .file_name()
        .map(|n| crate::path_syntax::suffix(&n.to_string_lossy()).to_string())
        .unwrap_or_default();
    let remapped = effective_suffix(path);
    if remapped != real {
        let dispatch = &merge::CONSTANTS["_DISPATCH"];
        return dispatch[&remapped]
            .as_str()
            .or_else(|| dispatch[remapped.to_lowercase()].as_str())
            .map(str::to_string);
    }
    let ext = languages::suffix(path);
    if ext == ".h" {
        if header_kind(path, true) {
            return Some("objc".into());
        }
        if header_kind(path, false) {
            return Some("cpp".into());
        }
    }
    if ext == ".m" && !header_kind(path, true) {
        return None;
    }
    if ext.is_empty()
        && let Some(interp) = crate::detect_types::shebang_interpreter(path)
    {
        return merge::CONSTANTS["_SHEBANG_DISPATCH"][interp]
            .as_str()
            .map(str::to_string);
    }
    // Opt-in beyond the upstream port: reStructuredText pages (`--docs`).
    if extractors::rst::enabled()
        && (ext == ".rst" || (ext == ".txt" && extractors::rst::is_sphinx_txt(path)))
    {
        return Some("rst".into());
    }
    merge::CONSTANTS["_DISPATCH"][ext]
        .as_str()
        .map(str::to_string)
}
pub fn header_kind(path: &Path, objc: bool) -> bool {
    let Ok(bytes) = extractors::parse_cache::read(path) else {
        return false;
    };
    let source = String::from_utf8_lossy(&bytes[..bytes.len().min(256 * 1024)]);
    let markers: &[&str] = if objc {
        &[
            "@interface",
            "@protocol",
            "@implementation",
            "@import",
            "#import",
        ]
    } else {
        &[
            "class ",
            "namespace ",
            "template",
            "::",
            "public:",
            "private:",
            "protected:",
        ]
    };
    markers.iter().any(|m| source.contains(m))
}
pub fn extract_file(path: &Path, root: Option<&Path>) -> Value {
    let _markdown_root = extractors::markdown::enter_scan_root(root);
    let _python_root = extractors::engine::enter_scan_root(root);
    let _handoff = extractors::parse_cache::HandoffScope::begin();
    let Some(kind) = get_extractor(path) else {
        return json!({"nodes":[],"edges":[]});
    };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match kind.as_str() {
        "mcp_config" => Ok(crate::mcp_ingest::extract_mcp_config(path)),
        "package_manifest" => crate::manifest_ingest::extract_package_manifest(path),
        "robot" => Ok(extractors::robot::extract_robot(path)),
        "rst" => extractors::rst::extract_rst(path),
        "lazarus_package" | "slnx" | "csproj" | "xaml" => {
            Ok(projects::extract_project(path, &kind, root))
        }
        _ => {
            if extractors::LANGUAGE_EXTRACTORS.contains_key(kind.as_str()) {
                extractors::extract(&kind, path)
            } else {
                Ok(languages::extract_language(path, &kind))
            }
        }
    }));
    match result {
        Ok(Ok(value)) => value,
        Ok(Err(e)) => {
            log::warn!("skipped {}: {e}", path.display());
            json!({"nodes":[],"edges":[],"error":e})
        }
        Err(_) => json!({"nodes":[],"edges":[],"error":"native extraction failed"}),
    }
}
pub fn collect_files(
    target: &Path,
    follow_symlinks: bool,
    root: Option<&Path>,
) -> Result<Vec<PathBuf>> {
    let containment = resolved(root.unwrap_or(target));
    let inside = |p: &Path| resolved(p).starts_with(&containment);
    if target.is_file() {
        return Ok(if inside(target) {
            vec![target.to_path_buf()]
        } else {
            vec![]
        });
    }
    let root = root.unwrap_or(target);
    let mut warnings = crate::detect_ignore::EncodingWarnings::default();
    let patterns = crate::detect_ignore::load_ignore_file(root, true, &mut warnings)?;
    let negation = patterns.iter().any(|(_, p)| p.starts_with('!'));
    let mut cache = HashMap::new();
    let mut dirs = vec![(target.to_path_buf(), BTreeSet::new())];
    let mut files = vec![];
    if !follow_symlinks
        && target
            .components()
            .any(|p| crate::detect_types::is_noise_dir(&p.as_os_str().to_string_lossy(), None))
    {
        return Ok(files);
    }
    while let Some((dir, mut ancestors)) = dirs.pop() {
        let real = resolved(&dir);
        if !ancestors.insert(real.clone()) {
            continue;
        }
        if dir.is_symlink() {
            let parent = resolved(dir.parent().unwrap_or(Path::new(".")));
            if parent == real || parent.starts_with(&real) {
                continue;
            }
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if path.is_dir() {
                if crate::detect_types::is_noise_dir(&name, Some(&dir)) {
                    continue;
                }
                if path.is_symlink() && (!follow_symlinks || !inside(&path)) {
                    continue;
                }
                if !follow_symlinks
                    && !negation
                    && crate::detect_ignore::is_ignored(&path, root, &patterns, &mut cache)
                {
                    continue;
                }
                dirs.push((path, ancestors.clone()));
            } else if merge::CONSTANTS["_DISPATCH"]
                .get(languages::suffix(&path))
                .is_some()
                && !crate::detect_ignore::is_ignored(&path, root, &patterns, &mut cache)
                && !crate::detect_types::is_installed_rule_file(&path)
                && inside(&path)
            {
                files.push(path);
            }
        }
    }
    files.sort();
    Ok(files)
}
fn default_root(paths: &[PathBuf]) -> PathBuf {
    if paths.len() == 1 {
        return paths[0].parent().unwrap_or(Path::new(".")).to_path_buf();
    }
    let Some(first) = paths.first() else {
        return PathBuf::from(".");
    };
    let parts = first.components().collect::<Vec<_>>();
    let n = (0..parts.len())
        .take_while(|i| {
            paths
                .iter()
                .all(|p| p.components().nth(*i) == Some(parts[*i]))
        })
        .count();
    if n == 0 {
        PathBuf::from(".")
    } else {
        parts[..n].iter().collect()
    }
}
fn run_defaults(
    paths: &[PathBuf],
    per: &mut Vec<Value>,
    nodes: &mut Vec<Value>,
    edges: &mut Vec<Value>,
) {
    register_resolvers();
    for warning in resolver_registry::run_language_resolvers(paths, per, nodes, edges, None) {
        log::warn!("{}", warning.message);
    }
}
pub fn register_resolvers() {
    static REGISTERED: OnceLock<()> = OnceLock::new();
    REGISTERED.get_or_init(|| {
        let add = |name: &str, ext: &[&str], f: Arc<resolver_registry::Resolve>| {
            resolver_registry::register(LanguageResolver {
                name: name.into(),
                suffixes: ext.iter().map(|s| s.to_string()).collect(),
                resolve: f,
            });
        };
        add(
            "terraform_modules",
            &[".tf"],
            Arc::new(|p, n, e| {
                extractors::terraform::resolve_terraform_modules(p, n, e);
                Ok(())
            }),
        );
        for (name, lang, ext) in [
            ("swift_member_calls", "swift", vec![".swift"]),
            ("python_member_calls", "python", vec![".py"]),
        ] {
            add(
                name,
                &ext,
                Arc::new(move |p, n, e| {
                    members::resolve_members(lang, p, n, e);
                    Ok(())
                }),
            );
        }
        add(
            "ruby_member_calls",
            &[".rb", ".rake"],
            Arc::new(|p, n, e| crate::ruby_resolution::resolve_ruby_member_calls(p, n, e)),
        );
        for (name, lang, ext) in [
            (
                "typescript_member_calls",
                "typescript",
                vec![".ts", ".tsx", ".mts", ".cts", ".js", ".jsx"],
            ),
            (
                "cpp_member_calls",
                "cpp",
                vec![".cpp", ".cc", ".cxx", ".hpp", ".cu", ".cuh", ".metal", ".h"],
            ),
            ("objc_member_calls", "objc", vec![".m", ".mm", ".h"]),
            ("csharp_member_calls", "csharp", vec![".cs"]),
            ("java_member_calls", "java", vec![".java"]),
            ("php_member_calls", "php", vec![".php"]),
        ] {
            add(
                name,
                &ext,
                Arc::new(move |p, n, e| {
                    members::resolve_members(lang, p, n, e);
                    Ok(())
                }),
            );
        }
        add(
            "rust_self_member_calls",
            &[".rs"],
            Arc::new(|p, n, e| {
                members::rust_self_calls(p, n, e);
                Ok(())
            }),
        );
        add(
            "vbnet_partial_calls",
            &[".vb"],
            Arc::new(|p, n, e| {
                extractors::vbnet::resolve_vbnet_partial_calls(p, n, e);
                Ok(())
            }),
        );
        add(
            "r_sourced_calls",
            &[".r", ".R"],
            Arc::new(|p, n, e| {
                extractors::r::resolve_r_sourced_calls(p, n, e);
                Ok(())
            }),
        );
        add(
            "solidity_type_references",
            &[".sol"],
            Arc::new(|p, n, e| {
                extractors::solidity::resolve_solidity_type_references(p, n, e);
                Ok(())
            }),
        );
        add(
            "erlang_remote_calls",
            &[".erl", ".hrl", ".escript"],
            Arc::new(|p, n, e| {
                extractors::erlang::resolve_erlang_remote_calls(p, n, e);
                Ok(())
            }),
        );
        add(
            "elixir_import_targets",
            &[".ex", ".exs"],
            Arc::new(|_, n, e| {
                members::elixir_imports(n, e);
                Ok(())
            }),
        );
        add(
            "elixir_qualified_calls",
            &[".ex", ".exs"],
            Arc::new(|p, n, e| {
                members::elixir_qualified_calls(p, n, e);
                Ok(())
            }),
        );
        add(
            "pascal_inherited_calls",
            &[".pas", ".pp", ".dpr", ".dpk", ".inc"],
            Arc::new(|p, n, e| crate::pascal_resolution::resolve_pascal_inherited_calls(p, n, e)),
        );
        add(
            "kotlin_qualified_calls",
            &[".kt", ".kts"],
            Arc::new(|p, n, e| {
                members::qualified_calls("kotlin", p, n, e);
                Ok(())
            }),
        );
        add(
            "kotlin_member_calls",
            &[".kt", ".kts"],
            Arc::new(|p, n, e| {
                members::kotlin_members(p, n, e);
                Ok(())
            }),
        );
        add(
            "csharp_qualified_calls",
            &[".cs"],
            Arc::new(|p, n, e| {
                members::qualified_calls("csharp", p, n, e);
                Ok(())
            }),
        );
        add(
            "csharp_interface_dispatch",
            &[".cs"],
            Arc::new(|p, n, e| crate::csharp_dispatch::resolve_csharp_interface_dispatch(p, n, e)),
        );
        add(
            "swift_protocol_dispatch",
            &[".swift"],
            Arc::new(|p, n, e| crate::swift_dispatch::resolve_swift_protocol_dispatch(p, n, e)),
        );
        add(
            "markdown_mentions",
            &[".md", ".mdx", ".qmd", ".skill", ".rst", ".txt"],
            Arc::new(|p, n, e| {
                crate::markdown_resolution::resolve_markdown_mentions(p, n, e);
                Ok(())
            }),
        );
    });
}
/// Which inputs an [`extract_with_report`] run actually re-parsed.
///
/// Incremental builds are sound by construction: unchanged files come from
/// the extraction cache as the per-file records they produced when last
/// parsed, changed files are re-extracted, and the whole set goes through the
/// same cross-file `assemble` as a cold build. The result therefore equals a
/// full rebuild; the report only says how much parsing that took.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct ExtractReport {
    /// Files re-extracted this run (cache miss, cache disabled or bypassed).
    pub changed: Vec<PathBuf>,
    /// Files whose per-file records were served from the extraction cache.
    pub unchanged: usize,
    /// Files with no extractor.
    pub unsupported: usize,
}

pub fn extract(paths: &[PathBuf], options: &ExtractOptions) -> Result<Value> {
    extract_with_report(paths, options).map(|(value, _)| value)
}

/// Rebuild every attribute map of a per-file record at its exact size.
///
/// Records arrive from `json!` builders and the JSON parser, whose maps grow
/// by doubling; with hundreds of thousands of small records the slack is a
/// sizeable share of the resident set. Rebuilding preserves key order.
pub(crate) fn tighten(value: &mut Value) {
    match value {
        Value::Object(map) => {
            let mut tight = serde_json::Map::with_capacity(map.len());
            for (key, mut inner) in std::mem::take(map) {
                tighten(&mut inner);
                tight.insert(key, inner);
            }
            *map = tight;
        }
        Value::Array(items) => {
            items.iter_mut().for_each(tighten);
            items.shrink_to_fit();
        }
        _ => {}
    }
}
/// [`extract`] plus the [`ExtractReport`] describing the cache traffic.
pub fn extract_with_report(
    paths: &[PathBuf],
    options: &ExtractOptions,
) -> Result<(Value, ExtractReport)> {
    let _sources = extractors::parse_cache::RunScope::begin();
    extractors::resolution_js::clear();
    extractors::markdown::clear_link_index_cache();
    let root = resolved(
        options
            .root
            .as_deref()
            .or(options.cache_root.as_deref())
            .unwrap_or(&default_root(paths)),
    );
    let started = std::time::Instant::now();
    let (per, report) = records_with_report(paths, options, paths)?;
    if std::env::var("GRAPHICLE_WATCH_TIMING").is_ok_and(|v| v == "1")
        && !options.resolution_context_nodes.is_empty()
    {
        eprintln!(
            "[extract timing] records={:.1}ms",
            started.elapsed().as_secs_f64() * 1000.0
        );
    }
    Ok((assemble(paths, per, &root, options)?, report))
}

/// Parse/cache the immutable per-file inputs without performing cross-file resolution.
/// The caller owns the parse-cache run and serializes extractor-global state.
pub(crate) fn records_with_report(
    paths: &[PathBuf],
    options: &ExtractOptions,
    corpus_paths: &[PathBuf],
) -> Result<(Vec<Value>, ExtractReport)> {
    let root = resolved(
        options
            .root
            .as_deref()
            .or(options.cache_root.as_deref())
            .unwrap_or(&default_root(paths)),
    );
    // Project-level extension -> language declarations (#2961), active for
    // this run (and handed to its workers below).
    let overrides = crate::rcfile::activate_language_overrides(&root);
    if !overrides.is_empty() {
        log::info!(
            "language overrides ({}): {}",
            crate::rcfile::RC_FILENAME,
            overrides
                .iter()
                .map(|(ext, target)| format!("{ext} -> {target}"))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    let cache_root = resolved(options.cache_root.as_deref().unwrap_or(Path::new(".")));
    let co = CacheOptions {
        root: root.clone(),
        cache_root: Some(cache_root),
        ..CacheOptions::default()
    };
    let mut cache = ExtractionCache::default();
    let mut per = vec![Value::Null; paths.len()];
    let mut work = vec![];
    let mut report = ExtractReport::default();
    let worker_count = |items: usize| {
        if options.parallel {
            crate::par::workers(options.max_workers).min(items.max(1))
        } else {
            1
        }
    };
    let bypass = |path: &Path| {
        extractors::models::JS_CACHE_BYPASS_SUFFIXES.contains(&effective_suffix(path).as_str())
    };
    // Cache entries are located and read back in parallel (hashing every
    // source is most of a warm run); the stat index and counters are updated
    // in path order afterwards, as a sequential loop would.
    let mut candidates = vec![];
    for (i, path) in paths.iter().enumerate() {
        if get_extractor(path).is_none() {
            per[i] = json!({"nodes":[],"edges":[]});
            report.unsupported += 1;
            continue;
        }
        if options.use_cache && !bypass(path) {
            candidates.push(i);
        } else {
            work.push((i, path));
        }
    }
    // A Python file's syntax facts are kept beside its cached record, so a
    // cache hit is not parsed again by the cross-file passes either.
    let syntax_dir = options.use_cache.then(|| cache.syntax_dir(&co)).flatten();
    if !candidates.is_empty() {
        let _profiled = crate::profile::span("extract.records.cache_load");
        let candidate_paths = candidates
            .iter()
            .map(|i| paths[*i].as_path())
            .collect::<Vec<_>>();
        // A file the project remaps to another language keys its entry with
        // the declaration (`cache_salt`), so the same bytes cached under the
        // old extractor are a miss.
        let loaded = cache.load_many_salted(
            &candidate_paths,
            &co,
            worker_count(candidates.len()),
            |path: &Path| crate::rcfile::cache_salt(path),
            |path, entry, value| {
                tighten(value);
                if let Some(dir) = &syntax_dir {
                    keep_cached_syntax(dir, path, entry);
                }
            },
        );
        for (i, loaded) in candidates.into_iter().zip(loaded) {
            match loaded {
                Ok(Some(value)) => {
                    per[i] = value;
                    report.unchanged += 1;
                }
                Ok(None) => work.push((i, &paths[i])),
                Err(e) => {
                    log::warn!("AST cache load failed: {e}");
                    work.push((i, &paths[i]));
                }
            }
        }
        work.sort_by_key(|(i, _)| *i);
    }
    report.changed = work.iter().map(|(_, p)| (*p).clone()).collect();
    // Declared before any parse so the run never retains a corpus worth of
    // trees; the cross-file resolvers parse on demand across these workers.
    if options.parallel {
        extractors::parse_cache::set_workers(worker_count(usize::MAX));
    }
    // Whether a freshly extracted record is written back to the cache. The
    // encoding and the write run on the extracting worker, outside any lock
    // (the stat-index changes the hashes imply are applied afterwards, in
    // path order), which keeps memory flat and the workers busy.
    // An anomalous zero-node record is not cached (#1666), but an intentional
    // decline such as a virtual workspace root (`skipped`) is, so repeat runs
    // neither re-extract nor re-report it (0.9.73, #3910).
    let cacheable = |path: &Path, value: &Value| {
        options.use_cache
            && !bypass(path)
            && value.get("error").is_none()
            && (value["nodes"].as_array().is_some_and(|a| !a.is_empty())
                || crate::build::truthy(&value["skipped"]))
    };
    // The service's source-aware profile resolves workspace entries against
    // the detected corpus. An ignored local dist/ build must not steal an
    // import from a source entry. Incremental callers pass the whole corpus,
    // including unchanged providers outside the current parsing subset.
    let corpus = (options.receiver_resolution
        && paths.iter().any(|path| {
            [
                ".ts", ".tsx", ".mts", ".cts", ".js", ".jsx", ".mjs", ".cjs", ".vue", ".svelte",
            ]
            .contains(&languages::suffix(path).as_str())
        }))
    .then(|| extractors::resolution_js::source_corpus(corpus_paths));
    // Once locating has prepared the entry directory, workers save through a
    // shared view of the cache and hand back the stat-index changes; before
    // that (nothing was located), each save takes the cache exclusively.
    let shared_dir = cache.shared_ast_dir(&co);
    let memo = cache::HashMemo::shared(cache.granularity());
    let cache = std::sync::RwLock::new(cache);
    let deferred = Mutex::new(Vec::new());
    let extract_one = |index: usize, path: &Path| {
        let profiled = crate::profile::enabled().then(std::time::Instant::now);
        let _corpus = extractors::resolution_js::enter_source_corpus(corpus.clone());
        let mut value = extract_file(path, Some(&root));
        tighten(&mut value);
        if cacheable(path, &value) {
            let salted;
            let co = match crate::rcfile::cache_salt(path) {
                Some(salt) => {
                    salted = CacheOptions {
                        salt: Some(salt),
                        ..co.clone()
                    };
                    &salted
                }
                None => &co,
            };
            let syntax = syntax_dir.as_ref().and_then(|_| encode_kept_syntax(path));
            let saved = ExtractionCache::encode_entry(path, &value, co).and_then(|bytes| {
                let Some(dir) = &shared_dir else {
                    return cache
                        .write()
                        .unwrap_or_else(|e| e.into_inner())
                        .save_encoded(path, &bytes, co, syntax.as_deref());
                };
                let (probe, written) = cache
                    .read()
                    .unwrap_or_else(|e| e.into_inner())
                    .save_encoded_shared(
                        path,
                        &bytes,
                        co,
                        dir,
                        &memo,
                        syntax_dir.as_deref().zip(syntax.as_deref()),
                    );
                if let Some(probe) = probe {
                    deferred
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push((index, probe));
                }
                written
            });
            if let Err(e) = saved {
                log::warn!("AST cache save failed: {e}");
            }
        }
        if let Some(started) = profiled {
            crate::profile::file_done(path, started.elapsed());
        }
        value
    };
    let workers = worker_count(work.len());
    let parse_span = crate::profile::span("extract.records.parse");
    if options.parallel && work.len() >= 20 && workers > 1 {
        let next = AtomicUsize::new(0);
        let results = Mutex::new(vec![]);
        // Workers see this run's language overrides (`_worker_init`).
        let overrides = crate::rcfile::snapshot();
        std::thread::scope(|scope| {
            for _ in 0..workers {
                let (next, results, work, extract_one, overrides) =
                    (&next, &results, &work, &extract_one, &overrides);
                std::thread::Builder::new()
                    .stack_size(32 * 1024 * 1024)
                    .spawn_scoped(scope, move || {
                        overrides.install();
                        loop {
                            let i = next.fetch_add(1, Ordering::Relaxed);
                            let Some((index, path)) = work.get(i) else {
                                break;
                            };
                            let value = extract_one(*index, path);
                            results
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .push((*index, value));
                        }
                    })
                    .map_err(crate::Error::from)?;
            }
            Ok::<_, crate::Error>(())
        })?;
        for (i, value) in results.into_inner().unwrap_or_else(|e| e.into_inner()) {
            per[i] = value;
        }
    }
    for (i, path) in &work {
        if per[*i].is_null() {
            per[*i] = extract_one(*i, path);
        }
    }
    drop(parse_span);
    crate::profile::report_files("extract.records.files");
    let flush_span = crate::profile::span("extract.records.cache_flush");
    let mut cache = cache.into_inner().unwrap_or_else(|e| e.into_inner());
    let mut deferred = deferred.into_inner().unwrap_or_else(|e| e.into_inner());
    deferred.sort_by_key(|(i, _)| *i);
    for (_, probe) in deferred {
        let _ = cache.apply_hash(probe);
    }
    cache.flush();
    drop(flush_span);
    if options.test_identities {
        for (path, record) in paths.iter().zip(&mut per) {
            extractors::test_identities::augment(path, record, &root);
        }
    }
    Ok((per, report))
}

/// Which kept syntax facts the AST cache stores beside a file's record.
#[derive(Clone, Copy)]
enum StoredSyntax {
    Python,
    Java,
    Php,
}
fn stored_syntax(path: &Path) -> Option<StoredSyntax> {
    match path.extension()?.to_str()? {
        "py" => Some(StoredSyntax::Python),
        "java" => Some(StoredSyntax::Java),
        "php" | "phtml" | "php3" | "php4" | "php5" | "php7" | "phps" => Some(StoredSyntax::Php),
        _ => None,
    }
}
/// The bytes the AST cache keeps for the syntax facts this run's extraction
/// of `path` recorded (they stay kept for the run's own passes).
fn encode_kept_syntax(path: &Path) -> Option<Vec<u8>> {
    use extractors::{parse_cache, resolution_facts as facts, resolution_types as types};
    fn reencode<T: Send + 'static>(
        kind: &'static str,
        path: &Path,
        encode: impl FnOnce(&T) -> Option<Vec<u8>>,
    ) -> Option<Vec<u8>> {
        let kept = parse_cache::take_facts(kind, path)?;
        let (bytes, kept) = match kept.downcast::<T>() {
            Ok(syntax) => (encode(&syntax), syntax as parse_cache::Facts),
            Err(other) => (None, other),
        };
        parse_cache::keep_facts(kind, path, kept);
        bytes
    }
    match stored_syntax(path)? {
        StoredSyntax::Python => reencode(facts::PYTHON_SYNTAX, path, |s: &facts::PythonSyntax| {
            s.encode(path)
        }),
        StoredSyntax::Java => reencode(types::JAVA_SYNTAX, path, types::JavaSyntax::encode),
        StoredSyntax::Php => reencode(types::PHP_SYNTAX, path, types::PhpSyntax::encode),
    }
}
/// Keep the syntax facts stored beside the cached record `entry` of `path`
/// for this run's cross-file passes, when there are any for it.
fn keep_cached_syntax(dir: &Path, path: &Path, entry: &Path) {
    use extractors::{parse_cache, resolution_facts as facts, resolution_types as types};
    let Some(kind) = stored_syntax(path) else {
        return;
    };
    let Some(hash) = entry.file_stem() else {
        return;
    };
    let mut stored = dir.join(hash);
    stored.set_extension("json");
    let Ok(bytes) = std::fs::read(&stored) else {
        return;
    };
    let _ = match kind {
        StoredSyntax::Python => facts::PythonSyntax::decode(path, &bytes).map(|s| {
            parse_cache::keep_facts(facts::PYTHON_SYNTAX, path, Box::new(s));
        }),
        StoredSyntax::Java => types::JavaSyntax::decode(&bytes).map(|s| {
            parse_cache::keep_facts(types::JAVA_SYNTAX, path, Box::new(s));
        }),
        StoredSyntax::Php => types::PhpSyntax::decode(&bytes).map(|s| {
            parse_cache::keep_facts(types::PHP_SYNTAX, path, Box::new(s));
        }),
    };
}

/// Graphify #3946: a code file that parses cleanly but yields nothing beyond
/// its own file node is often a plain data literal the extractor has nothing
/// to model; without this it enters the graph as a silent, zero-edge node.
/// Exactly one node, so the zero-node case stays with the failure report;
/// files with parse errors (warned above), extractor errors, intentional
/// skips, no extractor, or only whitespace are not reported.
fn warn_symbolless_files(per: &[Value], paths: &[PathBuf], root: &Path) {
    let truthy = crate::build::truthy;
    let mut files = vec![];
    for (r, p) in per.iter().zip(paths) {
        if truthy(&r["parse_errors"]) || truthy(&r["error"]) || truthy(&r["skipped"]) {
            continue;
        }
        if get_extractor(p).is_none() || r["nodes"].as_array().is_none_or(|n| n.len() != 1) {
            continue;
        }
        let size = match std::fs::read(p) {
            Ok(bytes) => {
                let blank = bytes
                    .utf8_chunks()
                    .all(|chunk| chunk.valid().chars().all(crate::detect_types::py_space));
                if blank {
                    continue;
                }
                bytes.len()
            }
            Err(_) => 0,
        };
        let rel = p.strip_prefix(root).unwrap_or(p);
        files.push((rel.to_string_lossy().replace('\\', "/"), size));
    }
    if files.is_empty() {
        return;
    }
    let total_mb = files.iter().map(|(_, size)| *size as f64).sum::<f64>() / (1024.0 * 1024.0);
    let shown = files
        .iter()
        .take(5)
        .map(|(rel, _)| rel.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let more = if files.len() > 5 {
        format!(" (+{} more)", files.len() - 5)
    } else {
        String::new()
    };
    log::warn!(
        "{} code file(s) yielded no symbols ({total_mb:.2} MB) — a data file, or source this extractor models no symbols for: {shown}{more}",
        files.len()
    );
}

pub fn assemble(
    paths: &[PathBuf],
    per: Vec<Value>,
    root: &Path,
    options: &ExtractOptions,
) -> Result<Value> {
    assemble_with_corpus(paths, per, root, options, paths)
}

pub(crate) fn assemble_with_corpus(
    paths: &[PathBuf],
    mut per: Vec<Value>,
    root: &Path,
    options: &ExtractOptions,
    corpus_paths: &[PathBuf],
) -> Result<Value> {
    let assemble_started = std::time::Instant::now();
    let _assemble_span = crate::profile::span("extract.assemble");
    let collect_span = crate::profile::span("assemble.collect");
    // Binding raw calls and default cross-file resolvers consult package entries
    // too. Keep the same source boundary as the parser workers, including when
    // an incremental call resolves only a dependency closure.
    let corpus = (options.receiver_resolution
        && corpus_paths.iter().any(|p| {
            [
                ".ts", ".tsx", ".mts", ".cts", ".js", ".jsx", ".mjs", ".cjs", ".vue", ".svelte",
            ]
            .contains(&languages::suffix(p).as_str())
        }))
    .then(|| extractors::resolution_js::source_corpus(corpus_paths));
    let _corpus = extractors::resolution_js::enter_source_corpus(corpus);
    let mut failed = vec![];
    let mut failed_seen = BTreeSet::new();
    for (r, p) in per.iter().zip(paths) {
        let empty = r["nodes"].as_array().is_none_or(Vec::is_empty);
        if crate::build::truthy(&r["error"])
            || (empty && !crate::build::truthy(&r["skipped"]) && get_extractor(p).is_some())
        {
            let sf = p.to_string_lossy().into_owned();
            if failed_seen.insert(sf.clone()) {
                failed.push(sf);
            }
        }
        if let Some(errors) = r.get("parse_errors")
            && (r["nodes"].as_array().is_none_or(|a| a.len() <= 1)
                || errors["multiline_error"] == true)
        {
            log::warn!(
                "{} has syntax errors; extracted symbols may be incomplete",
                p.display()
            );
        }
    }
    warn_symbolless_files(&per, paths, root);
    for (r, p) in per.iter_mut().zip(paths) {
        if [".tf", ".tfvars", ".hcl"].contains(&languages::suffix(p).as_str()) {
            extractors::terraform::prepare_terraform(r, p, root)?;
        }
    }
    let count = |key: &str| {
        per.iter()
            .map(|r| r[key].as_array().map_or(0, Vec::len))
            .sum::<usize>()
    };
    let mut nodes = Vec::with_capacity(count("nodes"));
    let mut edges = Vec::with_capacity(count("edges"));
    // Raw call records are shared with the per-file results in the reference
    // model: they are remapped in place there and moved out below once the
    // cross-file passes that touch them have run, so no copy is taken here.
    let mut raw = Vec::with_capacity(count("raw_calls"));
    for r in &mut per {
        nodes.extend(r["nodes"].as_array().into_iter().flatten().cloned());
        // After this point nothing reads a per-file record's edges except the
        // `intermediate` output, so the records are moved rather than copied
        // unless that output is requested.
        match r.get_mut("edges") {
            Some(Value::Array(es)) if !options.include_intermediate => {
                edges.extend(std::mem::take(es));
            }
            Some(Value::Array(es)) => edges.extend(es.iter().cloned()),
            _ => {}
        }
    }
    // #3729: absolute module names that more than one scanned file answers to
    // stay unresolved. The corpus, not an incremental subset, decides.
    let phase = |name: &str, since: std::time::Instant| {
        if !options.resolution_context_nodes.is_empty()
            && std::env::var("GRAPHICLE_WATCH_TIMING").is_ok_and(|v| v == "1")
        {
            eprintln!(
                "[extract timing] {name}={:.1}ms",
                since.elapsed().as_secs_f64() * 1000.0
            );
        }
    };
    let t = std::time::Instant::now();
    drop(collect_span);
    let profiled = crate::profile::span("assemble.python_modules");
    let (module_files, root_modules) = extract_python::absolute_import_alias_files(
        &extract_python::scan_paths(corpus_paths, &options.resolution_context_nodes, root),
        root,
    );
    let ambiguous = extract_python::ambiguous_modules(&module_files, &root_modules);
    extract_python::suppress_ambiguous_imports(
        &mut edges,
        &nodes,
        &mut per,
        &ambiguous,
        &module_files,
        root,
    );
    drop(module_files);
    drop(profiled);
    phase("python_modules", t);
    let t = std::time::Instant::now();
    let profiled = crate::profile::span("assemble.symbol_resolution");
    resolution::augment_symbol_resolution_edges(
        paths,
        &mut nodes,
        &mut edges,
        root,
        &ambiguous,
        &options.resolution_context_nodes,
    );
    drop(profiled);
    phase("symbol_resolution", t);
    let profiled = crate::profile::span("assemble.merge_decl_def");
    types::merge_decl_def_classes(&mut nodes, &mut edges);
    drop(profiled);
    let profiled = crate::profile::span("assemble.normalize_ids");
    norm::normalize_ids(paths, &mut nodes, &mut edges, &mut raw, &mut per, root);
    drop(profiled);
    let profiled = crate::profile::span("assemble.repoint_python_imports");
    merge::repoint_python_imports(paths, &nodes, &mut edges, root);
    drop(profiled);
    let profiled = crate::profile::span("assemble.swift_csharp_merge");
    merge::merge_swift_extensions(&per, &mut nodes, &mut edges);
    merge::merge_csharp_partials(&mut per, &mut nodes, &mut edges, paths, root);
    drop(profiled);
    let profiled = crate::profile::span("assemble.disambiguate");
    // Raw call records are shared with per-file results in the reference model:
    // move them out for disambiguation and back again afterwards.
    raw.clear();
    for r in &mut per {
        if let Some(rs) = r.get_mut("raw_calls").and_then(Value::as_array_mut) {
            raw.extend(rs.iter_mut().map(std::mem::take));
        }
    }
    let before = nodes
        .iter()
        .map(|n| (s(n, "id").to_string(), s(n, "source_file").to_string()))
        .collect::<Vec<_>>();
    resolution::disambiguate_colliding_node_ids(&mut nodes, &mut edges, &mut raw, root);
    norm::sync_per_nodes(&mut per, &before, &nodes);
    drop(before);
    // Moved back rather than copied: `bind_raw_calls` below reads the records
    // through `per`, in this same order.
    let mut raw = raw.into_iter();
    for r in &mut per {
        if let Some(rs) = r.get_mut("raw_calls").and_then(Value::as_array_mut) {
            for rc in rs {
                *rc = raw.next().unwrap_or_default();
            }
        }
    }
    drop(raw);
    merge::canonicalize_namespaces(&mut nodes, &mut edges);
    drop(profiled);
    // Cross-file passes read the records of one language at a time; lend them
    // out of `per` instead of copying every record of that language. Files
    // are selected by their effective suffix (a `.graphiclerc` declaration
    // counts, #2961): exactly for Java, Go and Python, case-folded for PHP and
    // .NET, as upstream compares them.
    let select = |per: &mut [Value], wanted: &dyn Fn(&Path) -> bool| {
        let mut idx = vec![];
        let mut rs = vec![];
        let mut ps = vec![];
        for (i, (r, p)) in per.iter_mut().zip(paths).enumerate() {
            if wanted(p) {
                idx.push(i);
                rs.push(std::mem::take(r));
                ps.push(p.clone());
            }
        }
        (idx, rs, ps)
    };
    let exact = |ext: &'static str| move |p: &Path| effective_suffix(p) == ext;
    let php = |p: &Path| {
        [
            ".php", ".phtml", ".php3", ".php4", ".php5", ".php7", ".phps",
        ]
        .contains(&effective_suffix(p).to_lowercase().as_str())
            && !p.to_string_lossy().to_lowercase().ends_with(".blade.php")
    };
    let dotnet = |p: &Path| {
        [".cs", ".razor", ".cshtml"].contains(&effective_suffix(p).to_lowercase().as_str())
    };
    let restore = |per: &mut [Value], idx: Vec<usize>, rs: Vec<Value>| {
        for (i, r) in idx.into_iter().zip(rs) {
            per[i] = r;
        }
    };
    let profiled = crate::profile::span("assemble.php_java_go_types");
    let (idx, php_records, pp) = select(&mut per, &php);
    if !pp.is_empty() {
        types::resolve_php_type_references(&php_records, &pp, &mut nodes, &mut edges);
    }
    restore(&mut per, idx, php_records);
    let (idx, java, jp) = select(&mut per, &exact(".java"));
    if !jp.is_empty() {
        types::resolve_java_type_references(&java, &jp, &mut nodes, &mut edges);
    }
    restore(&mut per, idx, java);
    let (idx, go, gp) = select(&mut per, &exact(".go"));
    if !gp.is_empty() {
        types::resolve_go_type_references(
            &go,
            &gp,
            &mut nodes,
            &mut edges,
            root,
            &options.resolution_context_nodes,
            &options.resolution_context_edges,
        );
    }
    restore(&mut per, idx, go);
    drop(profiled);
    let profiled = crate::profile::span("assemble.python_imports");
    let (idx, py, pyp) = select(&mut per, &exact(".py"));
    if !pyp.is_empty() {
        let added = types::cross_file_python_imports(&py, &pyp, &mut nodes, &mut edges, &ambiguous);
        edges.extend(added);
    }
    restore(&mut per, idx, py);
    drop(profiled);
    let profiled = crate::profile::span("assemble.rewire_unique_stubs");
    merge::rewire_unique_stubs(&mut nodes, &mut edges);
    drop(profiled);
    let profiled = crate::profile::span("assemble.java_csharp_bash_kotlin");
    if !jp.is_empty() {
        let (idx, java, _) = select(&mut per, &exact(".java"));
        edges.extend(types::cross_file_java_imports(&java, &jp));
        restore(&mut per, idx, java);
    }
    if paths.iter().any(|p| dotnet(p)) {
        extractors::csharp::resolve_csharp_type_references(&mut nodes, &mut edges)?;
        extractors::csharp::resolve_cross_file_csharp_imports(&mut nodes, &mut edges);
        merge::drop_shadowed_csharp_partial_bases(&nodes, &mut edges);
    }
    let sh_idx = per
        .iter()
        .zip(paths)
        .enumerate()
        .filter(|(_, (r, p))| {
            [".sh", ".bash"].contains(&languages::suffix(p).as_str())
                || r["nodes"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|n| n["metadata"]["language"] == "bash")
        })
        .map(|(i, _)| i)
        .collect::<Vec<_>>();
    if !sh_idx.is_empty() {
        let sh = sh_idx
            .iter()
            .map(|i| std::mem::take(&mut per[*i]))
            .collect::<Vec<_>>();
        let sp = sh_idx.iter().map(|i| paths[*i].clone()).collect::<Vec<_>>();
        let added = crate::symbol_resolution::resolve_bash_source_edges(&sh, &sp, root, &edges);
        restore(&mut per, sh_idx, sh);
        edges.extend(added?);
    }
    members::kotlin_imports(&per, &mut edges);
    // Quoted C/C++/ObjC includes outside the including file's directory, to
    // their header file nodes (#4354): after the id remaps, before the
    // import-evidence index of the bare-name pass reads the edges.
    {
        let mut resolution_nodes = nodes.iter().collect::<Vec<_>>();
        if !options.resolution_context_nodes.is_empty() {
            let fresh = nodes.iter().map(|n| s(n, "id")).collect::<FxHashSet<_>>();
            resolution_nodes.extend(
                options
                    .resolution_context_nodes
                    .iter()
                    .filter(|n| !s(n, "id").is_empty() && !fresh.contains(s(n, "id"))),
            );
        }
        merge::resolve_c_include_edges(&mut edges, &resolution_nodes, Some(root));
    }
    drop(profiled);
    let raw = per
        .iter()
        .flat_map(|r| r["raw_calls"].as_array().into_iter().flatten())
        .collect::<Vec<_>>();
    if options.resolution_context_nodes.is_empty() && options.resolution_context_edges.is_empty() {
        let profiled = crate::profile::span("assemble.bind_raw_calls");
        bind_raw_calls(&raw, &nodes, &mut edges, root);
        drop(profiled);
        let _profiled = crate::profile::span("assemble.run_defaults");
        run_defaults(paths, &mut per, &mut nodes, &mut edges);
    } else {
        let _profiled = crate::profile::span("assemble.context_resolve");
        // GRAPHICLE_WATCH_TIMING=1: where a live update's extract time goes.
        let timing = std::env::var("GRAPHICLE_WATCH_TIMING").is_ok_and(|v| v == "1");
        if timing {
            eprintln!(
                "[extract timing] cross_file={:.1}ms",
                assemble_started.elapsed().as_secs_f64() * 1000.0
            );
        }
        let mut last = std::time::Instant::now();
        let mut mark = |phase: &str| {
            if timing {
                eprintln!(
                    "[extract timing] {phase}={:.1}ms",
                    last.elapsed().as_secs_f64() * 1000.0
                );
                last = std::time::Instant::now();
            }
        };
        let ids = nodes.iter().map(|n| s(n, "id")).collect::<BTreeSet<_>>();
        let mut resolution_nodes = nodes.clone();
        resolution_nodes.extend(
            options
                .resolution_context_nodes
                .iter()
                .filter(|n| !s(n, "id").is_empty() && !ids.contains(s(n, "id")))
                .cloned(),
        );
        drop(ids);
        mark("context_nodes");
        bind_raw_calls(&raw, &resolution_nodes, &mut edges, root);
        mark("bind_raw_calls");
        let mut combined = edges.clone();
        combined.extend(options.resolution_context_edges.iter().cloned());
        let (n0, e0) = (resolution_nodes.len(), combined.len());
        mark("context_edges");
        run_defaults(paths, &mut per, &mut resolution_nodes, &mut combined);
        mark("resolvers");
        for n in nodes.iter_mut() {
            if let Some(new) = resolution_nodes.iter().find(|v| v["id"] == n["id"]) {
                *n = new.clone();
            }
        }
        for (i, e) in edges.iter_mut().enumerate() {
            *e = combined[i].clone();
        }
        nodes.extend(resolution_nodes[n0..].iter().cloned());
        edges.extend(combined[e0..].iter().cloned());
        mark("copy_back");
    }
    // A Rust `use` group keeps one edge per binding; the group edge only
    // served the passes above (#4135).
    edges.retain_mut(|e| {
        let group = e
            .as_object_mut()
            .and_then(|m| m.shift_remove("_rust_use_group"));
        if let Some(m) = e.as_object_mut() {
            m.shift_remove("_rust_use_binding");
        }
        !group.is_some_and(|g| crate::build::truthy(&g))
    });
    let profiled = crate::profile::span("assemble.receiver_precise_routes");
    let unresolved_calls = options
        .receiver_resolution
        .then(|| extractors::receiver_resolution::resolve(paths, &per, &nodes, &mut edges, root));
    if options.precise {
        extractors::precise_python::resolve(
            paths,
            &nodes,
            &mut edges,
            &options.resolution_context_nodes,
            &options.resolution_context_edges,
            root,
        );
        extractors::precise_dart::resolve(
            &mut nodes,
            &mut edges,
            &options.resolution_context_nodes,
            root,
        );
        #[cfg(feature = "precise-js")]
        extractors::precise_js::resolve(
            paths,
            &nodes,
            &mut edges,
            &options.resolution_context_nodes,
            &options.resolution_context_edges,
            root,
        );
        let indexer = extractors::precise_scip::resolve(
            paths,
            &nodes,
            &mut edges,
            &options.resolution_context_nodes,
            &options.resolution_context_edges,
            root,
        );
        // How strongly each call edge is supported (`evidence`), raised by
        // SCIP indexes and, with GRAPHICLE_TRACE, by runtime traces.
        let traces = extractors::evidence::trace_paths();
        let runtime = if traces.is_empty() {
            Default::default()
        } else {
            let all = nodes
                .iter()
                .chain(&options.resolution_context_nodes)
                .collect::<Vec<_>>();
            extractors::evidence::runtime_pairs(&all, root, &traces)
        };
        extractors::evidence::annotate(&mut edges, &indexer, &runtime);
    }
    if extractors::routes::enabled() {
        extractors::routes::resolve(
            paths,
            &mut nodes,
            &mut edges,
            &options.resolution_context_nodes,
            root,
        );
    }
    drop(profiled);
    let profiled = crate::profile::span("assemble.finish");
    norm::finish(&mut nodes, &mut edges, root);
    drop(profiled);
    let mut result = serde_json::Map::new();
    result.insert("nodes".into(), Value::Array(nodes));
    result.insert("edges".into(), Value::Array(edges));
    result.insert("input_tokens".into(), json!(0));
    result.insert("output_tokens".into(), json!(0));
    result.insert("failed_sources".into(), json!(failed));
    result.insert("extracted_sources".into(), json!(paths));
    let mut result = Value::Object(result);
    if let Some(mut unresolved) = unresolved_calls {
        for u in &mut unresolved {
            if let Some(sf) = u["source_file"].as_str() {
                u["source_file"] = json!(resolution::source_key(sf, root));
            }
        }
        result["unresolved_calls"] = json!(unresolved);
    }
    if options.include_intermediate {
        result["intermediate"] = json!(per);
    }
    crate::par::drop_later(per);
    Ok(result)
}
pub fn bind_raw_calls(raw: &[&Value], nodes: &[Value], edges: &mut Vec<Value>, root: &Path) {
    // Every table here is lookup-only, so hashing replaces ordered maps.
    let mut labels: FxHashMap<&str, Vec<String>> = FxHashMap::default();
    let mut ci: FxHashMap<String, Vec<String>> = FxHashMap::default();
    let mut callable = FxHashSet::default();
    let mut classes = FxHashSet::default();
    let mut sf_file: FxHashMap<&str, String> = FxHashMap::default();
    // A Rust call onto a bare-name node (struct/enum) constructs a value.
    let mut id_label: FxHashMap<&str, &str> =
        FxHashMap::with_capacity_and_hasher(nodes.len(), Default::default());
    for n in nodes {
        if !s(n, "id").is_empty() {
            id_label.insert(s(n, "id"), s(n, "label"));
        }
        if s(n, "file_type") != "rationale" && s(n, "type") != "namespace" {
            let name = s(n, "label")
                .trim_matches(['(', ')'])
                .trim_start_matches('.');
            if !name.is_empty() {
                labels.entry(name).or_default().push(s(n, "id").into());
                if merge::case_insensitive(s(n, "source_file")) {
                    ci.entry(name.to_lowercase())
                        .or_default()
                        .push(s(n, "id").into());
                }
            }
        }
        if field(n, "_callable") == Some(&Value::Bool(true)) {
            callable.insert(s(n, "id"));
        }
        if field(n, "_callable_class") == Some(&Value::Bool(true)) {
            classes.insert(s(n, "id"));
        }
        let sf = s(n, "source_file");
        if !sf.is_empty()
            && Path::new(sf).file_name().and_then(|s| s.to_str()) == Some(s(n, "label"))
        {
            sf_file.entry(sf).or_insert(s(n, "id").to_string());
        }
    }
    let mut id_file: FxHashMap<&str, String> =
        FxHashMap::with_capacity_and_hasher(nodes.len(), Default::default());
    let mut id_source: FxHashMap<&str, &str> =
        FxHashMap::with_capacity_and_hasher(nodes.len(), Default::default());
    // A file's node id, derived once per source file rather than per node.
    let mut file_ids: FxHashMap<&str, String> = FxHashMap::default();
    for n in nodes {
        let sf = s(n, "source_file");
        if sf.is_empty() {
            continue;
        }
        id_source.insert(s(n, "id"), sf);
        let file = match sf_file.get(sf) {
            Some(file) => file.clone(),
            None => file_ids
                .entry(sf)
                .or_insert_with(|| {
                    let p = Path::new(sf);
                    merge::file_node_id(p.strip_prefix(root).unwrap_or(p))
                })
                .clone(),
        };
        id_file.insert(s(n, "id"), file);
    }
    let mut symbols: FxHashMap<&str, FxHashSet<&str>> = FxHashMap::default();
    let mut modules: FxHashMap<&str, FxHashSet<&str>> = FxHashMap::default();
    let mut existing = FxHashSet::with_capacity_and_hasher(edges.len(), Default::default());
    let mut call_pairs = FxHashSet::default();
    for e in edges.iter() {
        let (source, target, relation) = (s(e, "source"), s(e, "target"), s(e, "relation"));
        // A Rust `use` binding edge is not evidence that the caller's file
        // imported the callee's module (#4135).
        if let Some(map) = match relation {
            "imports" => Some(&mut symbols),
            "imports_from" if !field(e, "_rust_use_binding").is_some_and(crate::build::truthy) => {
                Some(&mut modules)
            }
            _ => None,
        } {
            map.entry(source).or_default().insert(target);
        }
        existing.insert((source.to_string(), target.to_string()));
        if ["calls", "indirect_call"].contains(&relation) {
            call_pairs.insert((source.to_string(), target.to_string()));
        }
    }
    // Enum members (`case_of`) and fields/properties (`defines`) are never what
    // a C# `new X()` constructs (#3815, #3888).
    let member_ids: FxHashSet<&str> = edges
        .iter()
        .filter(|e| matches!(s(e, "relation"), "case_of" | "defines"))
        .map(|e| s(e, "target"))
        .collect();
    // Top-level Elixir module names per file, for scoping unqualified Elixir
    // calls; read from the resolution nodes so an unchanged file's modules stay
    // visible on an incremental rebuild.
    let mut elixir_modules: FxHashMap<String, FxHashSet<String>> = FxHashMap::default();
    for n in nodes {
        if field(n, "_elixir_module").is_some_and(crate::build::truthy)
            && field(n, "source_file").is_some_and(crate::build::truthy)
        {
            elixir_modules
                .entry(crate::python_compat::string(&n["source_file"]))
                .or_default()
                .insert(
                    n.get("label")
                        .map(crate::python_compat::string)
                        .unwrap_or_default(),
                );
        }
    }
    let mut stub_resolver: Option<extractors::csharp::CsharpNameResolver> = None;
    // method id -> owning type id, built on first use.
    let mut method_owner: Option<FxHashMap<&str, &str>> = None;
    // The tables borrow from `edges`; new edges are appended after the loop.
    let mut added = vec![];
    for rc in raw {
        // Bound by an ambiguous absolute import (#3729): never guess a target.
        if field(rc, "_ambiguous_python_import").is_some_and(crate::build::truthy) {
            continue;
        }
        let callee = s(rc, "callee");
        let sf = s(rc, "source_file");
        if callee.is_empty()
            || is_language_builtin(callee)
            || field(rc, "is_member_call") == Some(&Value::Bool(true))
            || field(rc, "is_mixin") == Some(&Value::Bool(true))
            || ["bash", "markdown"].contains(&s(rc, "language"))
            || (s(rc, "language") == "go" && extractors::go::PREDECLARED_FUNCS.contains(&callee))
        {
            continue;
        }
        let mut cs = labels.get(callee).cloned().unwrap_or_default();
        if cs.is_empty() && merge::case_insensitive(sf) {
            cs = ci.get(&callee.to_lowercase()).cloned().unwrap_or_default();
        }
        // An enum member or a field/property is never a C# call target, nor
        // what `new X()` constructs (#3815, #3888, #4245).
        let csharp_new = field(rc, "csharp_new").is_some_and(crate::build::truthy);
        if csharp_new || s(rc, "lang") == "csharp" {
            cs.retain(|c| !member_ids.contains(c.as_str()));
        }
        // A call rescued from an annotation stub (#3888) binds to a same-named
        // class elsewhere only with evidence: an import, or the C# scope.
        let via_stub = field(rc, "via_stub") == Some(&Value::Bool(true));
        let mut stub_scope_hit = false;
        if via_stub && s(rc, "lang") == "csharp" && !cs.is_empty() {
            let resolver = stub_resolver
                .get_or_insert_with(|| extractors::csharp::CsharpNameResolver::new(nodes, edges));
            if let Some(caller_node) = resolver.node_by_id.get(s(rc, "caller_nid"))
                && let Some(scoped) = resolver.resolve_label(callee, caller_node, sf)
                && cs.contains(&scoped)
            {
                cs = vec![scoped];
                stub_scope_hit = true;
            }
        }
        if let Some(f) = merge::family(sf) {
            cs.retain(|c| {
                merge::family(id_source.get(c.as_str()).copied().unwrap_or(""))
                    .is_none_or(|g| g == f)
            });
        }
        let go_exact = s(rc, "language") == "go" && !s(rc, "import_path").is_empty();
        if go_exact {
            cs.retain(|c| {
                types::go_import_path_for_file(
                    Path::new(id_source.get(c.as_str()).copied().unwrap_or("")),
                    root,
                )
                .is_some_and(|p| p == s(rc, "import_path"))
            });
        }
        // An unqualified Elixir call can only reach the caller's own module,
        // Kernel, or a module imported or used in the caller's lexical scope
        // (Graphify c96a743, 48de615, #4001).
        if let Some(scope) = field(rc, "elixir_call_scope").filter(|v| !v.is_null()) {
            let scope: FxHashSet<String> = scope
                .as_array()
                .into_iter()
                .flatten()
                .map(crate::python_compat::string)
                .collect();
            cs.retain(|c| {
                elixir_modules
                    .get(id_source.get(c.as_str()).copied().unwrap_or(""))
                    .is_some_and(|mods| mods.iter().any(|m| scope.contains(m)))
            });
            if cs.is_empty() {
                continue;
            }
        }
        if cs.is_empty() {
            continue;
        }
        let caller = s(rc, "caller_nid");
        let cf = sf_file.get(sf).or_else(|| id_file.get(caller));
        let syms = cf.and_then(|f| symbols.get(f.as_str()));
        let mods = cf.and_then(|f| modules.get(f.as_str()));
        let has_symbol = |id: &String| syms.is_some_and(|s| s.contains(id.as_str()));
        let has_module = |id: &String| {
            id_file
                .get(id.as_str())
                .is_some_and(|f| mods.is_some_and(|m| m.contains(f.as_str())))
        };
        // C#: an unqualified call binds to a member of the caller's own type
        // first; a partial type's halves live in different files, so after
        // the merge a same-named method elsewhere must not hide the other
        // half's member. Exactly one own-type candidate binds.
        let mut own_type = vec![];
        if cs.len() > 1 && s(rc, "lang") == "csharp" && !csharp_new {
            let owners = method_owner.get_or_insert_with(|| {
                edges
                    .iter()
                    .filter(|e| s(e, "relation") == "method")
                    .map(|e| (s(e, "target"), s(e, "source")))
                    .collect::<FxHashMap<_, _>>()
            });
            if let Some(owner) = owners.get(caller) {
                own_type = cs
                    .iter()
                    .filter(|c| owners.get(c.as_str()) == Some(owner))
                    .cloned()
                    .collect();
            }
        }
        let (target, evidence) = if cs.len() == 1 {
            (
                cs[0].clone(),
                go_exact || has_symbol(&cs[0]) || has_module(&cs[0]),
            )
        } else if own_type.len() == 1 {
            (own_type[0].clone(), true)
        } else {
            let exact = cs.iter().filter(|c| has_symbol(c)).collect::<Vec<_>>();
            if exact.len() == 1 {
                (exact[0].clone(), true)
            } else {
                let module = cs.iter().filter(|c| has_module(c)).collect::<Vec<_>>();
                if module.len() == 1 {
                    (module[0].clone(), true)
                } else {
                    let files = cs
                        .iter()
                        .map(|c| {
                            (
                                c.clone(),
                                json!(id_source.get(c.as_str()).copied().unwrap_or_default()),
                            )
                        })
                        .collect();
                    let Some(target) =
                        crate::paths::disambiguate_ambiguous_candidates(&cs, &files, sf)
                    else {
                        continue;
                    };
                    (target, false)
                }
            }
        };
        if id_source
            .get(target.as_str())
            .is_none_or(|sf| sf.is_empty())
            || target == caller
        {
            continue;
        }
        let indirect = field(rc, "indirect") == Some(&Value::Bool(true));
        if indirect {
            if !callable.contains(target.as_str())
                || classes.contains(target.as_str())
                || !call_pairs.insert((caller.into(), target.clone()))
            {
                continue;
            }
        } else {
            if !evidence
                && [".ts", ".tsx", ".mts", ".cts", ".js", ".jsx", ".mjs", ".cjs"]
                    .contains(&effective_suffix(sf).as_str())
            {
                continue;
            }
            if via_stub && !(evidence || stub_scope_hit) {
                continue;
            }
            if !existing.insert((caller.into(), target.clone())) {
                continue;
            }
        }
        let label = id_label.get(target.as_str()).copied().unwrap_or("");
        let constructor =
            !indirect && sf.ends_with(".rs") && !label.is_empty() && !label.ends_with(')');
        let mut edge = json!({"source":caller,"target":target,"relation":if indirect{"indirect_call"}else if constructor{"references"}else{"calls"},"context":if indirect{rc["context"].as_str().unwrap_or("argument")}else if constructor{"constructor"}else{"call"},"confidence":if !indirect&&evidence{"EXTRACTED"}else{"INFERRED"},"confidence_score":if !indirect&&evidence{1.0}else{0.85},"source_file":sf,"source_location":rc.get("source_location").cloned().unwrap_or(Value::Null),"weight":1.0});
        tighten(&mut edge);
        added.push(edge);
    }
    edges.extend(added);
    repoint_go_imports(&sf_file, edges, root, &mut existing);
}

/// #3746: repoint intra-module Go `imports_from` edges from the
/// synthetic `go_pkg_<import path>` sink onto the imported package's file
/// nodes. Stdlib and external imports have no file in the corpus and stay
/// sinks. Mirrors the block that follows qualified-call binding in
/// upstream `extract.py`.
fn repoint_go_imports(
    sf_file: &FxHashMap<&str, String>,
    edges: &mut Vec<Value>,
    root: &Path,
    existing: &mut FxHashSet<(String, String)>,
) {
    let mut go_pkg_files: FxHashMap<String, Vec<String>> = FxHashMap::default();
    let mut ordered: Vec<(&str, &String)> = sf_file
        .iter()
        .filter(|(sf, _)| sf.ends_with(".go"))
        .map(|(sf, id)| (*sf, id))
        .collect();
    // `sf_file` hashes its keys; sort so per-package file order is stable.
    ordered.sort();
    for (sf, fnid) in ordered {
        if let Some(import_path) = types::go_import_path_for_file(Path::new(sf), root) {
            go_pkg_files
                .entry(import_path)
                .or_default()
                .push(fnid.clone());
        }
    }
    if go_pkg_files.is_empty() {
        return;
    }
    let sinks: FxHashMap<String, &String> = go_pkg_files
        .keys()
        .map(|ip| (extractors::base::make_id(["go", "pkg", ip.as_str()]), ip))
        .collect();
    let mut rewritten = Vec::with_capacity(edges.len());
    for e in edges.drain(..) {
        let import_path = if s(&e, "relation") == "imports_from" {
            sinks.get(s(&e, "target")).copied()
        } else {
            None
        };
        let Some(import_path) = import_path else {
            rewritten.push(e);
            continue;
        };
        let source = s(&e, "source").to_string();
        for fnid in &go_pkg_files[import_path] {
            if *fnid == source || existing.contains(&(source.clone(), fnid.clone())) {
                continue;
            }
            let mut repointed = e.clone();
            repointed["target"] = json!(fnid);
            rewritten.push(repointed);
            existing.insert((source.clone(), fnid.clone()));
        }
    }
    *edges = rewritten;
}
pub fn dispatch(r: &Value) -> Result<Value> {
    let action = r["action"].as_str().unwrap_or("extract");
    let path = Path::new(r["path"].as_str().unwrap_or("."));
    let paths =
        serde_json::from_value::<Vec<PathBuf>>(r.get("paths").cloned().unwrap_or(json!([])))
            .map_err(crate::Error::from)?;
    let options: ExtractOptions = serde_json::from_value(r.clone()).map_err(crate::Error::from)?;
    Ok(match action {
        "extractor" => json!(get_extractor(path)),
        "header_kind" => json!(header_kind(path, r["objc"] == true)),
        "file" => extract_file(path, options.root.as_deref()),
        "collect" => json!(collect_files(
            path,
            r["follow_symlinks"] == true,
            options.root.as_deref()
        )?),
        "assemble" => assemble(
            &paths,
            r["per_file"].as_array().cloned().unwrap_or_default(),
            &resolved(options.root.as_deref().unwrap_or(Path::new("."))),
            &options,
        )?,
        "incremental" => {
            let (extraction, report) = extract_with_report(&paths, &options)?;
            json!({"extraction": extraction, "report": report})
        }
        _ => extract(&paths, &options)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn relocated_cache_preserves_walked_roots_and_file_symlinks() {
        let _serial = extractors::parse_cache::test_serial();
        let temp = tempfile::tempdir().unwrap();
        // Start on the physical path as well as testing explicit aliases below.
        // macOS's implicit /var -> /private/var alias otherwise makes every
        // cached source a root marker and misses ordinary relative sources.
        let physical = temp.path().canonicalize().unwrap();
        let mut roots = Vec::new();
        for name in ["original", "relocated"] {
            let root = physical.join(name);
            std::fs::create_dir_all(root.join("sub")).unwrap();
            std::fs::write(root.join("real.py"), "def value():\n    return 1\n").unwrap();
            std::os::unix::fs::symlink(root.join("real.py"), root.join("sub/link.py")).unwrap();
            let alias = physical.join(format!("{name}-alias"));
            std::os::unix::fs::symlink(&root, &alias).unwrap();
            roots.extend([root, alias]);
        }
        for (i, source) in roots.iter().enumerate() {
            for (j, target) in roots.iter().enumerate() {
                let cache_root = temp.path().join(format!("cache-{i}-{j}"));
                let run = |root: &Path, use_cache| {
                    extract(
                        &[root.join("real.py"), root.join("sub/link.py")],
                        &ExtractOptions {
                            root: Some(root.into()),
                            cache_root: Some(cache_root.clone()),
                            use_cache,
                            parallel: false,
                            ..Default::default()
                        },
                    )
                    .unwrap()
                };
                run(source, true);
                assert_eq!(
                    run(target, false),
                    run(target, true),
                    "cache moved from {source:?} to {target:?}"
                );
            }
        }
    }

    #[test]
    fn workspace_build_outputs_do_not_change_source_graphs_or_call_evidence() {
        let _serial = extractors::parse_cache::test_serial();
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let package = root.join("packages/lib");
        std::fs::create_dir_all(package.join("src")).unwrap();
        std::fs::create_dir(root.join("app")).unwrap();
        std::fs::write(
            root.join("package.json"),
            r#"{"private":true,"workspaces":["packages/*"]}"#,
        )
        .unwrap();
        std::fs::write(
            package.join("package.json"),
            r#"{"name":"@fixture/lib","main":"./dist/index.js","types":"./dist/index.d.ts"}"#,
        )
        .unwrap();
        let paths = vec![package.join("src/index.ts"), root.join("app/main.ts")];
        std::fs::write(&paths[0], "export function perform() { return 42; }\n").unwrap();
        std::fs::write(
            &paths[1],
            "import { perform } from '@fixture/lib';\nexport function run() { return perform(); }\n",
        )
        .unwrap();
        let options = ExtractOptions {
            root: Some(root.clone()),
            use_cache: false,
            receiver_resolution: true,
            parallel: false,
            ..Default::default()
        };
        let clean = extract(&paths, &options).unwrap();
        assert!(clean["edges"].as_array().unwrap().iter().any(|edge| {
            edge["source"] == "app_main_run"
                && edge["target"] == "packages_lib_src_index_perform"
                && edge["relation"] == "calls"
        }));
        std::fs::create_dir(package.join("dist")).unwrap();
        std::fs::write(
            package.join("dist/index.js"),
            "export function perform() { return 42; }\n",
        )
        .unwrap();
        std::fs::write(
            package.join("dist/index.d.ts"),
            "export declare function perform(): number;\n",
        )
        .unwrap();
        for parallel in [false, true] {
            let options = ExtractOptions {
                parallel,
                max_workers: Some(2),
                ..options.clone()
            };
            assert_eq!(extract(&paths, &options).unwrap(), clean);
        }
    }
}
