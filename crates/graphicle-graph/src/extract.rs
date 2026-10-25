// Adapted from Graphify 0.9.95 (92741d16), Apache-2.0; see NOTICE.md.
// Upstream: graphify/extract.py
//! Complete native AST corpus extraction and cross-file assembly.
pub mod per_file;
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
    record::Attributes,
    resolver_registry::{self, EdgeList, LanguageResolver, Rec, rs},
};
use per_file::{FileRecords, PerFile, PerList, per_dispatch};
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
pub const RUN_ONLY_EXTRACTION_KEYS: [&str; 3] =
    ["extracted_sources", "failed_sources", "skipped_sources"];
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
        // `extern "C"` (0.9.93, #4526): a C-API header guarded by
        // `#ifdef __cplusplus`, whose linkage block the C grammar rejects.
        &[
            "class ",
            "namespace ",
            "template",
            "::",
            "public:",
            "private:",
            "protected:",
            "extern \"C\"",
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
        "yaml" => Ok(extractors::yaml_config::extract_yaml(path)),
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
    per: &mut PerList,
    nodes: &mut Vec<Value>,
    edges: &mut EdgeList,
) {
    register_resolvers();
    for warning in resolver_registry::run_language_resolvers_on(paths, per, nodes, edges, None) {
        log::warn!("{}", warning.message);
    }
}
/// Register a member pass written once over [`PerFile`] results and [`Rec`]
/// edges together with its record-native form: the JSON entry runs `$body`
/// on JSON lists, the native one on the lists as assembly holds them
/// (`$per` is the per-file slice, `$nodes` the nodes, `$edges` the edges).
macro_rules! member_pass {
    ($name:expr, $ext:expr, |$per:ident, $nodes:ident, $edges:ident| $body:expr) => {{
        let resolve: Arc<resolver_registry::Resolve> = Arc::new(
            move |per: &mut Vec<Value>, $nodes: &mut Vec<Value>, $edges: &mut Vec<Value>| {
                let $per = &per[..];
                $body;
                Ok(())
            },
        );
        let records: Arc<resolver_registry::ResolveRecords> = Arc::new(
            move |per: &mut PerList, $nodes: &mut Vec<Value>, $edges: &mut Vec<Attributes>| {
                #[cfg(test)]
                crate::extract_members::NATIVE_RUNS.with(|runs| runs.borrow_mut().push($name));
                per_dispatch!(per, per => {
                    let $per = &per[..];
                    $body;
                });
                Ok(())
            },
        );
        resolver_registry::register_native(
            LanguageResolver {
                name: $name.into(),
                suffixes: $ext.iter().map(|s| s.to_string()).collect(),
                resolve,
            },
            records,
        );
    }};
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
        // The member passes read the per-file results and the edges in
        // either form, so none of them turns assembly's lists back to JSON.
        for (name, lang, ext) in [
            ("swift_member_calls", "swift", vec![".swift"]),
            ("python_member_calls", "python", vec![".py"]),
        ] {
            member_pass!(name, ext, |p, n, e| members::resolve_members(lang, p, n, e));
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
                // Every suffix the TS/JS extractors claim, so a pure-ESM
                // (`.mjs`) repo still activates the resolver.
                vec![".ts", ".tsx", ".mts", ".cts", ".js", ".jsx", ".mjs", ".cjs"],
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
            member_pass!(name, ext, |p, n, e| members::resolve_members(lang, p, n, e));
        }
        member_pass!("rust_self_member_calls", [".rs"], |p, n, e| {
            members::rust_self_calls(p, n, e)
        });
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
        // After the type pass: it walks the `inherits` edges that pass binds
        // (0.9.92, #4379).
        add(
            "solidity_inherited_calls",
            &[".sol"],
            Arc::new(|p, n, e| {
                extractors::solidity::resolve_solidity_inherited_calls(p, n, e);
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
        member_pass!("elixir_import_targets", [".ex", ".exs"], |_p, n, e| {
            members::elixir_imports(n, e)
        });
        member_pass!("elixir_qualified_calls", [".ex", ".exs"], |p, n, e| {
            members::elixir_qualified_calls(p, n, e)
        });
        add(
            "pascal_inherited_calls",
            &[".pas", ".pp", ".dpr", ".dpk", ".inc"],
            Arc::new(|p, n, e| crate::pascal_resolution::resolve_pascal_inherited_calls(p, n, e)),
        );
        member_pass!("kotlin_qualified_calls", [".kt", ".kts"], |p, n, e| {
            members::qualified_calls("kotlin", p, n, e)
        });
        member_pass!("kotlin_member_calls", [".kt", ".kts"], |p, n, e| {
            members::kotlin_members(p, n, e)
        });
        // Go receiver-typed member calls (`g.Greet()` on a type declared in
        // another file); the shared pass skips member calls.
        member_pass!("go_member_calls", [".go"], |p, n, e| {
            members::go_members(p, n, e)
        });
        member_pass!("csharp_qualified_calls", [".cs"], |p, n, e| {
            members::qualified_calls("csharp", p, n, e)
        });
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
        // These passes also run on the per-file results and the edges in
        // whichever form assembly holds them, so neither list converts for
        // them; each is the JSON pass above, written once over both forms.
        for (name, any) in [
            (
                "terraform_modules",
                resolver_registry::any_form!(|p, n, e| {
                    extractors::terraform::resolve_terraform_modules_any(p, n, e);
                    Ok(())
                }),
            ),
            (
                "ruby_member_calls",
                resolver_registry::any_form!(|p, n, e| {
                    crate::ruby_resolution::resolve_ruby_member_calls_any(p, n, e)
                }),
            ),
            (
                "vbnet_partial_calls",
                resolver_registry::any_form!(|p, n, e| {
                    extractors::vbnet::resolve_vbnet_partial_calls_any(p, n, e);
                    Ok(())
                }),
            ),
            (
                "r_sourced_calls",
                resolver_registry::any_form!(|p, n, e| {
                    extractors::r::resolve_r_sourced_calls_any(p, n, e);
                    Ok(())
                }),
            ),
            (
                "solidity_type_references",
                resolver_registry::any_form!(|p, n, e| {
                    extractors::solidity::resolve_solidity_type_references_any(p, n, e);
                    Ok(())
                }),
            ),
            (
                "solidity_inherited_calls",
                resolver_registry::any_form!(|p, n, e| {
                    extractors::solidity::resolve_solidity_inherited_calls_any(p, n, e);
                    Ok(())
                }),
            ),
            (
                "erlang_remote_calls",
                resolver_registry::any_form!(|p, n, e| {
                    extractors::erlang::resolve_erlang_remote_calls_any(p, n, e);
                    Ok(())
                }),
            ),
            (
                "pascal_inherited_calls",
                resolver_registry::any_form!(|p, n, e| {
                    crate::pascal_resolution::resolve_pascal_inherited_calls_any(p, n, e)
                }),
            ),
            (
                "csharp_interface_dispatch",
                resolver_registry::any_form!(|p, n, e| {
                    crate::csharp_dispatch::resolve_csharp_interface_dispatch_any(p, n, e)
                }),
            ),
            (
                "swift_protocol_dispatch",
                resolver_registry::any_form!(|p, n, e| {
                    crate::swift_dispatch::resolve_swift_protocol_dispatch_any(p, n, e)
                }),
            ),
            (
                "markdown_mentions",
                resolver_registry::any_form!(|p, n, e| {
                    crate::markdown_resolution::resolve_markdown_mentions_any(p, n, e);
                    Ok(())
                }),
            ),
        ] {
            resolver_registry::attach_any_form(name, any);
        }
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
    let (per, report) = records_list(paths, options, paths)?;
    if std::env::var("GRAPHICLE_WATCH_TIMING").is_ok_and(|v| v == "1")
        && !options.resolution_context_nodes.is_empty()
    {
        eprintln!(
            "[extract timing] records={:.1}ms",
            started.elapsed().as_secs_f64() * 1000.0
        );
    }
    Ok((assemble_list(paths, per, &root, options, paths)?, report))
}

/// [`extract`] for a caller that builds the graph itself (the extract
/// command): the assembled edges stay beside the extraction as compact
/// records ([`Assembled`]), which go to the build as they are instead of being
/// rebuilt as JSON first. A list that a JSON-only pass left as JSON is
/// converted here, on the run's workers, so the build holds no edge maps.
pub(crate) fn extract_assembled(paths: &[PathBuf], options: &ExtractOptions) -> Result<Assembled> {
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
    let (per, _) = records_list(paths, options, paths)?;
    if std::env::var("GRAPHICLE_WATCH_TIMING").is_ok_and(|v| v == "1")
        && !options.resolution_context_nodes.is_empty()
    {
        eprintln!(
            "[extract timing] records={:.1}ms",
            started.elapsed().as_secs_f64() * 1000.0
        );
    }
    let mut assembled = assemble_records(paths, per, &root, options, paths)?;
    // Lossless, and a list holding an edge that is no object stays JSON.
    let _ = assembled.edges.records();
    Ok(assembled)
}

/// Parse/cache the immutable per-file inputs without performing cross-file resolution.
/// The caller owns the parse-cache run and serializes extractor-global state.
pub(crate) fn records_with_report(
    paths: &[PathBuf],
    options: &ExtractOptions,
    corpus_paths: &[PathBuf],
) -> Result<(Vec<Value>, ExtractReport)> {
    let (per, report) = records_in(paths, options, corpus_paths, false)?;
    Ok((per.into_values(), report))
}

/// [`records_with_report`] with every result in compact form where it
/// converts ([`FileRecords`]). A parsed file converts on the worker that
/// extracted it, after its AST cache entry was written from the extractor's
/// own JSON, so the cached bytes are unchanged; cache hits convert after
/// loading, on the run's workers.
fn records_list(
    paths: &[PathBuf],
    options: &ExtractOptions,
    corpus_paths: &[PathBuf],
) -> Result<(PerList, ExtractReport)> {
    records_in(
        paths,
        options,
        corpus_paths,
        !resolver_registry::json_only(),
    )
}

/// One file's result as a worker hands it back.
enum Parsed {
    Json(Value),
    Records(FileRecords),
}

fn records_in(
    paths: &[PathBuf],
    options: &ExtractOptions,
    corpus_paths: &[PathBuf],
    compact: bool,
) -> Result<(PerList, ExtractReport)> {
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
    // The results that converted to compact form on their worker.
    let mut compacted: Vec<Option<FileRecords>> = if compact {
        std::iter::repeat_with(|| None).take(paths.len()).collect()
    } else {
        Vec::new()
    };
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
        // Compact results: a hit converts on the worker that read it, so the
        // corpus never sits in memory as JSON (a warm run). The converted
        // record waits in its candidate's slot; the loader hands back a
        // placeholder. A path listed twice keeps the conversion for later.
        let position = candidate_paths
            .iter()
            .enumerate()
            .map(|(k, path)| (*path, k))
            .collect::<FxHashMap<_, _>>();
        let early = (compact && position.len() == candidate_paths.len()).then(|| {
            std::iter::repeat_with(OnceLock::<FileRecords>::new)
                .take(candidate_paths.len())
                .collect::<Vec<_>>()
        });
        // A file the project remaps to another language keys its entry with
        // the declaration (`cache_salt`), so the same bytes cached under the
        // old extractor are a miss.
        let loaded = cache.load_many_salted(
            &candidate_paths,
            &co,
            worker_count(candidates.len()),
            |path: &Path| crate::rcfile::cache_salt(path),
            |path, entry, value| {
                // A compact result tightens what stays JSON when it converts.
                if !compact {
                    tighten(value);
                }
                if let Some(dir) = &syntax_dir {
                    keep_cached_syntax(dir, path, entry);
                }
                if let (Some(slots), Some(&k)) = (&early, position.get(path)) {
                    match FileRecords::from_value(std::mem::take(value)) {
                        Ok(records) => {
                            let _ = slots[k].set(records);
                        }
                        Err(original) => *value = original,
                    }
                }
            },
        );
        let mut early = early.map(|slots| slots.into_iter().map(OnceLock::into_inner));
        for (i, loaded) in candidates.into_iter().zip(loaded) {
            let converted = early.as_mut().and_then(|slots| slots.next().flatten());
            match loaded {
                Ok(Some(value)) => {
                    match converted {
                        Some(records) => compacted[i] = Some(records),
                        None => per[i] = value,
                    }
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
        if !compact {
            tighten(&mut value);
        }
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
        if !compact {
            return Parsed::Json(value);
        }
        match FileRecords::from_value(value) {
            Ok(records) => Parsed::Records(records),
            Err(mut value) => {
                tighten(&mut value);
                Parsed::Json(value)
            }
        }
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
        for (i, parsed) in results.into_inner().unwrap_or_else(|e| e.into_inner()) {
            match parsed {
                Parsed::Json(value) => per[i] = value,
                Parsed::Records(records) => compacted[i] = Some(records),
            }
        }
    }
    for (i, path) in &work {
        if per[*i].is_null() && compacted.get(*i).is_none_or(Option::is_none) {
            match extract_one(*i, path) {
                Parsed::Json(value) => per[*i] = value,
                Parsed::Records(records) => compacted[*i] = Some(records),
            }
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
    let mut per = if compact {
        compact_list(per, compacted)
    } else {
        PerList::Values(per)
    };
    if options.test_identities {
        for (path, record) in paths.iter().zip(per.values()) {
            extractors::test_identities::augment(path, record, &root);
        }
        if compact {
            per.records();
        }
    }
    Ok((per, report))
}

/// The results as one compact list: those still JSON (cache hits, files
/// without an extractor, results that did not convert) convert on the run's
/// workers. If one of them does not convert, the whole list stays JSON.
fn compact_list(mut per: Vec<Value>, compacted: Vec<Option<FileRecords>>) -> PerList {
    let pending = compacted
        .iter()
        .enumerate()
        .filter(|(_, c)| c.is_none())
        .map(|(i, _)| i)
        .collect::<Vec<_>>();
    let converted = per_file::convert(
        pending
            .iter()
            .map(|&i| std::mem::take(&mut per[i]))
            .collect(),
        FileRecords::from_value,
    );
    if converted.iter().all(std::result::Result::is_ok) {
        let mut slots = compacted;
        for (i, records) in pending.into_iter().zip(converted) {
            slots[i] = records.ok();
        }
        return PerList::Records(
            slots
                .into_iter()
                .map(|records| records.expect("every result converted"))
                .collect(),
        );
    }
    for (i, records) in pending.into_iter().zip(converted) {
        per[i] = match records {
            Ok(records) => records.into_value(),
            Err(mut value) => {
                tighten(&mut value);
                value
            }
        };
    }
    for (i, records) in compacted.into_iter().enumerate() {
        if let Some(records) = records {
            per[i] = records.into_value();
        }
    }
    PerList::Values(per)
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
fn warn_symbolless_files<P: PerFile>(per: &[P], paths: &[PathBuf], root: &Path) {
    let truthy = crate::build::truthy;
    let mut files = vec![];
    for (r, p) in per.iter().zip(paths) {
        if truthy(r.at("parse_errors")) || truthy(r.at("error")) || truthy(r.at("skipped")) {
            continue;
        }
        if get_extractor(p).is_none() || r.node_count().is_none_or(|n| n != 1) {
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
        // `root` is resolved (never `\\?\` on Windows); compare `p` the same way.
        let p = crate::fsutil::plain(p);
        let rel = p.strip_prefix(root).unwrap_or(&p);
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
    per: Vec<Value>,
    root: &Path,
    options: &ExtractOptions,
    corpus_paths: &[PathBuf],
) -> Result<Value> {
    assemble_list(paths, PerList::Values(per), root, options, corpus_paths)
}

/// The failed sources of a run (and a warning per file with syntax errors).
/// The sources that failed (an error, or zero nodes from a wired extractor)
/// and those the extractor declined by design (`skipped`, Graphify 0.9.94,
/// #2879): stamped, yet with zero nodes, which the manifest marks `ast_empty`
/// so an incremental build stops re-queuing them.
fn failed_sources<P: PerFile>(per: &[P], paths: &[PathBuf]) -> (Vec<String>, Vec<String>) {
    let mut failed = vec![];
    let mut failed_seen = BTreeSet::new();
    let mut skipped = vec![];
    let mut skipped_seen = BTreeSet::new();
    for (r, p) in per.iter().zip(paths) {
        let empty = r.nodes().is_none_or(<[P::R]>::is_empty);
        if crate::build::truthy(r.at("error"))
            || (empty && !crate::build::truthy(r.at("skipped")) && get_extractor(p).is_some())
        {
            let sf = p.to_string_lossy().into_owned();
            if failed_seen.insert(sf.clone()) {
                failed.push(sf);
            }
        } else if crate::build::truthy(r.at("skipped")) {
            let sf = p.to_string_lossy().into_owned();
            if skipped_seen.insert(sf.clone()) {
                skipped.push(sf);
            }
        }
        if let Some(errors) = r.key("parse_errors")
            && (r.node_count().is_none_or(|n| n <= 1) || errors["multiline_error"] == true)
        {
            log::warn!(
                "{} has syntax errors; extracted symbols may be incomplete",
                p.display()
            );
        }
    }
    (failed, skipped)
}

/// Copy every per-file node into `nodes` and move (with the intermediate
/// output: copy) every per-file edge into `edges`.
fn collect_records<P: PerFile>(
    per: &mut [P],
    include_intermediate: bool,
    nodes: &mut Vec<Value>,
    edges: &mut Vec<Value>,
) {
    nodes.reserve_exact(per.iter().filter_map(PerFile::node_count).sum());
    edges.reserve_exact(
        per.iter()
            .map(|r| r.at("edges").as_array().map_or(0, Vec::len))
            .sum(),
    );
    for r in per {
        nodes.extend(r.nodes().into_iter().flatten().map(Rec::to_value));
        // After this point nothing reads a per-file record's edges except the
        // `intermediate` output, so the records are moved rather than copied
        // unless that output is requested.
        match r.key_mut("edges") {
            Some(Value::Array(es)) if !include_intermediate => {
                edges.extend(std::mem::take(es));
            }
            Some(Value::Array(es)) => edges.extend(es.iter().cloned()),
            _ => {}
        }
    }
}

/// Disambiguate colliding node ids. Raw call records are shared with the
/// per-file results in the reference model: they are moved out for the pass
/// and back again afterwards, and the per-file nodes follow the corpus nodes.
fn disambiguate<P: PerFile>(per: &mut [P], nodes: &mut [Value], edges: &mut [Value], root: &Path) {
    let mut raw = Vec::with_capacity(
        per.iter()
            .map(|r| r.raw_calls().map_or(0, <[P::R]>::len))
            .sum(),
    );
    for r in per.iter_mut() {
        if let Some(rs) = r.raw_calls_mut() {
            raw.extend(rs.iter_mut().map(std::mem::take));
        }
    }
    let before = nodes
        .iter()
        .map(|n| (s(n, "id").to_string(), s(n, "source_file").to_string()))
        .collect::<Vec<_>>();
    resolution::disambiguate_colliding_node_ids(nodes, edges, &mut raw[..], root);
    norm::sync_per_nodes(per, &before, nodes);
    drop(before);
    // Moved back rather than copied: `bind_raw_calls` below reads the records
    // through `per`, in this same order.
    let mut raw = raw.into_iter();
    for r in per.iter_mut() {
        if let Some(rs) = r.raw_calls_mut() {
            for rc in rs {
                *rc = raw.next().unwrap_or_default();
            }
        }
    }
}

/// The cross-file passes that read the per-file results of one language at a
/// time (PHP, Java, Go and Python types and imports, Bash sources, Kotlin
/// imports), with the passes between them.
fn language_passes<P: PerFile>(
    per: &mut [P],
    paths: &[PathBuf],
    nodes: &mut Vec<Value>,
    edges: &mut Vec<Value>,
    root: &Path,
    options: &ExtractOptions,
    ambiguous: &extractors::resolution_facts::AmbiguousModules,
) -> Result<()> {
    // Cross-file passes read the records of one language at a time; lend them
    // out of `per` instead of copying every record of that language. Files
    // are selected by their effective suffix (a `.graphiclerc` declaration
    // counts, #2961): exactly for Java, Go and Python, case-folded for PHP and
    // .NET, as upstream compares them. A pass that reads JSON gets the
    // selected results as JSON (`json_slice`: borrowed when they are); the
    // Java and Go passes read them in place, in either form.
    let select = |per: &mut [P], wanted: &dyn Fn(&Path) -> bool| {
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
    let restore = |per: &mut [P], idx: Vec<usize>, rs: Vec<P>| {
        for (i, r) in idx.into_iter().zip(rs) {
            per[i] = r;
        }
    };
    let profiled = crate::profile::span("assemble.php_java_go_types");
    let (idx, php_records, pp) = select(per, &php);
    if !pp.is_empty() {
        types::resolve_php_type_references(&P::json_slice(&php_records), &pp, nodes, edges);
    }
    restore(per, idx, php_records);
    let (idx, java, jp) = select(per, &exact(".java"));
    if !jp.is_empty() {
        types::resolve_java_type_references(&java[..], &jp, nodes, edges);
    }
    restore(per, idx, java);
    let (idx, go, gp) = select(per, &exact(".go"));
    if !gp.is_empty() {
        types::resolve_go_type_references(
            &go[..],
            &gp,
            nodes,
            edges,
            root,
            &options.resolution_context_nodes,
            &options.resolution_context_edges,
        );
    }
    restore(per, idx, go);
    drop(profiled);
    let profiled = crate::profile::span("assemble.python_imports");
    let (idx, py, pyp) = select(per, &exact(".py"));
    if !pyp.is_empty() {
        let added =
            types::cross_file_python_imports(&P::json_slice(&py), &pyp, nodes, edges, ambiguous);
        edges.extend(added);
    }
    restore(per, idx, py);
    drop(profiled);
    let profiled = crate::profile::span("assemble.rewire_unique_stubs");
    {
        // An incremental build re-extracts only the changed files, so the
        // rewire also takes the unchanged ones as candidates. #4439: a Rust
        // `use` reached its declaration only when a type annotation had left
        // a sourceless node under the same id; the rewire is lent one for each
        // imported name that has none. Neither is kept, so an unresolved
        // import dangles as before.
        let fresh = nodes.iter().map(|n| s(n, "id")).collect::<FxHashSet<_>>();
        let mut use_stubs: Vec<Value> = vec![];
        let mut use_stub_index: FxHashMap<String, usize> = FxHashMap::default();
        for e in edges.iter() {
            let Some(name) = e.get("_rust_use_name").and_then(Value::as_str) else {
                continue;
            };
            let target = s(e, "target");
            if !extract_python::is_identifier(name) || fresh.contains(target) {
                continue;
            }
            let stub = json!({"id":target,"label":name,"file_type":"code","source_file":""});
            match use_stub_index.get(target) {
                Some(&i) => use_stubs[i] = stub,
                None => {
                    use_stub_index.insert(target.to_string(), use_stubs.len());
                    use_stubs.push(stub);
                }
            }
        }
        let context = options
            .resolution_context_nodes
            .iter()
            .filter(|n| !fresh.contains(s(n, "id")))
            .collect::<Vec<_>>();
        drop(fresh);
        let lent = use_stubs.iter().chain(context).collect::<Vec<_>>();
        merge::rewire_unique_stubs_with(nodes, &lent, edges);
    }
    drop(profiled);
    let profiled = crate::profile::span("assemble.java_csharp_bash_kotlin");
    if !jp.is_empty() {
        let (idx, java, _) = select(per, &exact(".java"));
        edges.extend(types::cross_file_java_imports(&java[..], &jp));
        restore(per, idx, java);
    }
    if paths.iter().any(|p| dotnet(p)) {
        extractors::csharp::resolve_csharp_type_references(nodes, edges)?;
        extractors::csharp::resolve_cross_file_csharp_imports(nodes, edges);
        merge::drop_shadowed_csharp_partial_bases(nodes, edges);
    }
    let sh_idx = per
        .iter()
        .zip(paths)
        .enumerate()
        .filter(|(_, (r, p))| {
            [".sh", ".bash"].contains(&languages::suffix(p).as_str())
                || r.nodes().into_iter().flatten().any(|n| {
                    n.rec_get("metadata")
                        .is_some_and(|m| m["language"] == "bash")
                })
        })
        .map(|(i, _)| i)
        .collect::<Vec<_>>();
    if !sh_idx.is_empty() {
        let sh = sh_idx
            .iter()
            .map(|i| std::mem::take(&mut per[*i]))
            .collect::<Vec<_>>();
        let sp = sh_idx.iter().map(|i| paths[*i].clone()).collect::<Vec<_>>();
        let added = crate::symbol_resolution::resolve_bash_source_edges(
            &P::json_slice(&sh),
            &sp,
            root,
            edges,
        );
        restore(per, sh_idx, sh);
        edges.extend(added?);
    }
    members::kotlin_imports(&*per, edges);
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
        merge::resolve_c_include_edges(edges, &resolution_nodes, Some(root));
    }
    drop(profiled);
    Ok(())
}

/// Every raw call record of the per-file results, in file order.
fn raw_calls_of<P: PerFile>(per: &[P]) -> Vec<&P::R> {
    per.iter()
        .flat_map(|r| r.raw_calls().into_iter().flatten())
        .collect()
}

/// `_stamp_python_fixture_paths` (Graphify 0.9.92, #4332): repo-relative
/// locations for the pytest-fixture arm of the Python member resolver. A
/// re-extracted file carries absolute paths while an unchanged file's context
/// node is repo-relative, and the arm walks directories, so it gets one form.
/// In memory only: the results were cached before resolution.
fn stamp_python_fixture_paths(per: &mut PerList, root: &Path) {
    let rel = |source: &str| -> String {
        let path = Path::new(source);
        if !path.is_absolute() {
            return crate::fsutil::rel_string(path);
        }
        match resolved(path).strip_prefix(root) {
            Ok(relative) => crate::fsutil::rel_string(relative),
            Err(_) => crate::fsutil::rel_string(path),
        }
    };
    let truthy = |value: Option<&Value>| value.is_some_and(crate::build::truthy);
    let indices: Vec<usize> = per_dispatch!(&*per, p => p
        .iter()
        .enumerate()
        .filter(|(_, r)| {
            r.raw_calls().into_iter().flatten().any(|rc| truthy(rc.rec_get("_python_fixture_param")))
                || r.nodes().into_iter().flatten().any(|n| truthy(n.rec_get("_py_fixture")))
        })
        .map(|(i, _)| i)
        .collect());
    for i in indices {
        per.edit_json(i, |result| {
            for call in result["raw_calls"].as_array_mut().into_iter().flatten() {
                if truthy(call.get("_python_fixture_param")) && truthy(call.get("source_file")) {
                    let source = crate::python_compat::string(&call["source_file"]);
                    call["_python_test_file"] = json!(rel(&source));
                }
            }
            let mut located = serde_json::Map::new();
            for node in result["nodes"].as_array().into_iter().flatten() {
                if truthy(node.get("_py_fixture")) && truthy(node.get("source_file")) {
                    let source = crate::python_compat::string(&node["source_file"]);
                    located.insert(
                        crate::python_compat::string(&node["id"]),
                        json!(rel(&source)),
                    );
                }
            }
            if !located.is_empty() {
                result["_python_fixture_files"] = Value::Object(located);
            }
        });
    }
}

/// [`bind_raw_calls`] over the edge list in record form when it converts.
fn bind_per_file<P: PerFile>(per: &[P], nodes: &[Value], edges: &mut EdgeList, root: &Path) {
    let raw = raw_calls_of(per);
    match edges.records() {
        Some(records) => bind_raw_calls(&raw[..], nodes, records, &[], root),
        None => bind_raw_calls(&raw[..], nodes, edges.values(), &[], root),
    }
}

/// An assembled extraction whose edge list is held beside it, in the form
/// assembly left it: compact records unless a JSON-only pass turned the list
/// back into JSON. The extract command hands both to the build as they are
/// ([`crate::build::build_owned_records`]), so the records are not rebuilt as
/// one JSON map each (Kubernetes: 1.4M edges, about 1.3 GB of maps beside the
/// records); [`Assembled::into_value`] gives the extraction everyone else reads.
pub(crate) struct Assembled {
    /// The extraction object, holding an empty `edges` array in its place.
    extraction: Value,
    edges: EdgeList,
}
impl Assembled {
    /// The extraction with its edges in place as JSON objects, converted on
    /// this thread: at the end of assembly the pages of the per-file records
    /// it just freed are this thread's, and the rebuilt maps reuse them.
    pub(crate) fn into_value(self) -> Value {
        let Self {
            mut extraction,
            edges,
        } = self;
        extraction["edges"] = Value::Array(edges.into_values());
        extraction
    }
    /// The extraction and the compact edge records that come before the edges
    /// it holds: the records with an empty `edges` array when the list is in
    /// record form, else no records and the JSON edges in place.
    pub(crate) fn into_parts(self) -> (Value, Vec<Attributes>) {
        match self.edges {
            EdgeList::Records(records) => (self.extraction, records),
            edges => (
                Self {
                    extraction: self.extraction,
                    edges,
                }
                .into_value(),
                Vec::new(),
            ),
        }
    }
}

/// [`assemble_records`] with the edges in place as JSON.
fn assemble_list(
    paths: &[PathBuf],
    per: PerList,
    root: &Path,
    options: &ExtractOptions,
    corpus_paths: &[PathBuf],
) -> Result<Value> {
    assemble_records(paths, per, root, options, corpus_paths).map(Assembled::into_value)
}

/// Assembly over the per-file results in either form ([`PerList`]): the
/// passes read them through [`PerFile`], and a pass that reads only JSON gets
/// them converted for its run. The edges come back beside the extraction in
/// the form the passes left them ([`Assembled`]).
fn assemble_records(
    paths: &[PathBuf],
    mut per: PerList,
    root: &Path,
    options: &ExtractOptions,
    corpus_paths: &[PathBuf],
) -> Result<Assembled> {
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
    let (failed, skipped) = per_dispatch!(&per, p => failed_sources(&p[..], paths));
    per_dispatch!(&per, p => warn_symbolless_files(&p[..], paths, root));
    for (i, p) in paths.iter().enumerate() {
        if [".tf", ".tfvars", ".hcl"].contains(&languages::suffix(p).as_str()) {
            per.edit_json(i, |r| extractors::terraform::prepare_terraform(r, p, root))?;
        }
    }
    let mut nodes = Vec::new();
    let mut edges = Vec::new();
    per_dispatch!(&mut per, p => collect_records(
        &mut p[..],
        options.include_intermediate,
        &mut nodes,
        &mut edges,
    ));
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
    per_dispatch!(&mut per, p => extract_python::suppress_ambiguous_imports(
        &mut edges,
        &nodes,
        &mut p[..],
        &ambiguous,
        &module_files,
        root,
    ));
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
    // The same fold for a Go package's type, which every file declaring a
    // method on it mints again under one id (#3399).
    types::merge_go_package_types(&mut nodes, &edges);
    drop(profiled);
    let profiled = crate::profile::span("assemble.normalize_ids");
    per_dispatch!(&mut per, p => norm::normalize_ids(paths, &mut nodes, &mut edges, &mut p[..], root));
    drop(profiled);
    let profiled = crate::profile::span("assemble.repoint_python_imports");
    merge::repoint_python_imports(paths, &nodes, &mut edges, root);
    drop(profiled);
    let profiled = crate::profile::span("assemble.swift_csharp_merge");
    per_dispatch!(&mut per, p => merge::merge_swift_extensions(&p[..], &mut nodes, &mut edges));
    drop(profiled);
    let profiled = crate::profile::span("assemble.disambiguate");
    per_dispatch!(&mut per, p => {
        disambiguate(&mut p[..], &mut nodes, &mut edges, root);
        // The C# partial merge runs after the colliding ids are salted apart
        // (0.9.91, #4411, #4428): it rewires every half's edges onto the
        // canonical half, and a canonical id that collides with another file's
        // node (`Spec.cs` next to `_Spec.cs`) would otherwise be split by its
        // edges' own source files, leaving them on a dead id. The halves are
        // joined by type path, whatever their (salted) ids.
        merge::merge_csharp_partials_of(&mut p[..], &mut nodes, &mut edges, paths, root);
    });
    merge::canonicalize_namespaces(&mut nodes, &mut edges);
    drop(profiled);
    per_dispatch!(&mut per, p => language_passes(&mut p[..], paths, &mut nodes, &mut edges, root, options, &ambiguous))?;
    // From here on the edges are compact records (eight bytes per field,
    // values shared through the value table) instead of one serde map each:
    // the call edges bound below and the ~10^5-10^6 repointed Go imports are
    // cloned and added in that form. Passes that read only JSON get it back
    // losslessly for their run (`EdgeList`).
    let mut edges = EdgeList::Values(edges);
    stamp_python_fixture_paths(&mut per, root);
    if options.resolution_context_nodes.is_empty() && options.resolution_context_edges.is_empty() {
        let profiled = crate::profile::span("assemble.bind_raw_calls");
        per_dispatch!(&per, p => bind_per_file(&p[..], &nodes, &mut edges, root));
        drop(profiled);
        let _profiled = crate::profile::span("assemble.run_defaults");
        run_defaults(paths, &mut per, &mut nodes, &mut edges);
    } else {
        let edges = edges.values();
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
        per_dispatch!(&per, p => bind_raw_calls(
            &raw_calls_of(&p[..])[..],
            &resolution_nodes,
            edges,
            &options.resolution_context_edges,
            root,
        ));
        mark("bind_raw_calls");
        let mut combined = edges.clone();
        combined.extend(options.resolution_context_edges.iter().cloned());
        let (n0, e0) = (resolution_nodes.len(), combined.len());
        mark("context_edges");
        let mut combined = EdgeList::Values(combined);
        run_defaults(paths, &mut per, &mut resolution_nodes, &mut combined);
        let combined = combined.into_values();
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
    // Past the resolvers, only receiver resolution and the intermediate output
    // read the per-file records. Without those, free the records here rather
    // than after `finish`: their raw calls and node copies (Kubernetes: about
    // 1.5 GB as JSON) would otherwise sit beside every new call and import
    // edge until assembly ends. Freed on this thread so the passes below reuse
    // the pages.
    let mut per = if options.receiver_resolution || options.include_intermediate {
        Some(per)
    } else {
        let _profiled = crate::profile::span("assemble.release_records");
        drop(per);
        None
    };
    // A Rust `use` group keeps one edge per binding; the group edge only
    // served the passes above (#4135).
    match &mut edges {
        EdgeList::Values(values) => drop_rust_use_groups(values),
        EdgeList::Records(records) => drop_rust_use_groups(records),
    }
    let profiled = crate::profile::span("assemble.receiver_precise_routes");
    let unresolved_calls = if options.receiver_resolution {
        let per: &[Value] = match per.as_mut() {
            Some(per) => per.values(),
            None => &[],
        };
        Some(extractors::receiver_resolution::resolve(
            paths,
            per,
            &nodes,
            edges.values(),
            root,
        ))
    } else {
        None
    };
    if options.precise {
        extractors::precise_python::resolve(
            paths,
            &nodes,
            edges.values(),
            &options.resolution_context_nodes,
            &options.resolution_context_edges,
            root,
        );
        extractors::precise_dart::resolve(
            &mut nodes,
            edges.values(),
            &options.resolution_context_nodes,
            root,
        );
        #[cfg(feature = "precise-js")]
        extractors::precise_js::resolve(
            paths,
            &nodes,
            edges.values(),
            &options.resolution_context_nodes,
            &options.resolution_context_edges,
            root,
        );
        let indexer = extractors::precise_scip::resolve(
            paths,
            &nodes,
            edges.values(),
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
        extractors::evidence::annotate(edges.values(), &indexer, &runtime);
    }
    if extractors::routes::enabled() {
        extractors::routes::resolve(
            paths,
            &mut nodes,
            edges.values(),
            &options.resolution_context_nodes,
            root,
        );
    }
    drop(profiled);
    let profiled = crate::profile::span("assemble.finish");
    match &mut edges {
        EdgeList::Values(values) => norm::finish(&mut nodes, values, root),
        EdgeList::Records(records) => norm::finish(&mut nodes, records, root),
    }
    drop(profiled);
    let mut result = serde_json::Map::new();
    result.insert("nodes".into(), Value::Array(nodes));
    // The edges stay beside the result ([`Assembled`]); this keeps the key's
    // place.
    result.insert("edges".into(), Value::Array(Vec::new()));
    result.insert("input_tokens".into(), json!(0));
    result.insert("output_tokens".into(), json!(0));
    result.insert("failed_sources".into(), json!(failed));
    result.insert("skipped_sources".into(), json!(skipped));
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
        let per: &[Value] = match per.as_mut() {
            Some(per) => per.values(),
            None => &[],
        };
        result["intermediate"] = json!(per);
    }
    if let Some(per) = per {
        crate::par::drop_later(per);
    }
    Ok(Assembled {
        extraction: result,
        edges,
    })
}

/// Drop the Rust `use` group edges and the binding markers (#4135).
fn drop_rust_use_groups<E: Rec>(edges: &mut Vec<E>) {
    edges.retain_mut(|e| {
        let group = e.rec_shift_remove("_rust_use_group");
        e.rec_shift_remove("_rust_use_binding");
        e.rec_shift_remove("_rust_use_name");
        !group.is_some_and(|g| crate::build::truthy(&g))
    });
}
/// The nodes by id and the type of each C# method with an arity, built once
/// per binding pass when a C# call first meets several candidates.
type OverloadIndex<'a> = (FxHashMap<&'a str, &'a Value>, FxHashMap<String, String>);
/// `_csharp_collapse_overloads` (Graphify 0.9.95, #4584): the overloads of
/// one C# method among `candidates` fold into one. Overloads of a name on one
/// type are separate nodes, so a bare-name lookup saw rival definitions that,
/// all in one file, no tie-breaker could choose between: the argument count
/// picks the overload when exactly one fits, else the first declared one
/// stands for the name. `owner` maps a method with an arity to its type.
fn csharp_collapse_overloads(
    candidates: Vec<String>,
    argc: Option<i64>,
    owner: &FxHashMap<String, String>,
    node_by_id: &FxHashMap<&str, &Value>,
) -> Vec<String> {
    let mut groups: Vec<(Option<&str>, Vec<&str>)> = vec![];
    for c in &candidates {
        let key = owner.get(c.as_str()).map(String::as_str);
        match groups.iter_mut().find(|(k, members)| {
            (key.is_some() && *k == key)
                || (key.is_none() && k.is_none() && members[0] == c.as_str())
        }) {
            Some((_, members)) => members.push(c.as_str()),
            None => groups.push((key, vec![c.as_str()])),
        }
    }
    if groups.len() == candidates.len() {
        return candidates;
    }
    let null = Value::Null;
    let mut keep: FxHashSet<String> = FxHashSet::default();
    for (_, members) in &groups {
        if members.len() == 1 {
            keep.insert(members[0].to_string());
            continue;
        }
        let picked = crate::extractors::engine_types::csharp_pick_overload(
            members
                .iter()
                .map(|m| (*m, node_by_id.get(m).map_or(&null, |n| &n["metadata"]))),
            argc,
        );
        keep.insert(picked.unwrap_or(members[0]).to_string());
    }
    candidates
        .into_iter()
        .filter(|c| keep.contains(c))
        .collect()
}
pub fn bind_raw_calls<E: Rec, R: Rec>(
    raw: &[&R],
    nodes: &[Value],
    edges: &mut Vec<E>,
    context_edges: &[Value],
    root: &Path,
) {
    // C# overload index (0.9.95, #4584), built on first use.
    let mut overload_index: Option<OverloadIndex<'_>> = None;
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
    let mut pair_ids = PairIds::default();
    let mut existing: FxHashSet<(u32, u32)> =
        FxHashSet::with_capacity_and_hasher(edges.len(), Default::default());
    let mut call_pairs: FxHashSet<(u32, u32)> = FxHashSet::default();
    for e in edges.iter() {
        let (source, target, relation) = (rs(e, "source"), rs(e, "target"), rs(e, "relation"));
        // A Rust `use` binding edge is not evidence that the caller's file
        // imported the callee's module (#4135).
        if let Some(map) = match relation {
            "imports" => Some(&mut symbols),
            "imports_from"
                if !e
                    .rec_get("_rust_use_binding")
                    .is_some_and(crate::build::truthy) =>
            {
                Some(&mut modules)
            }
            _ => None,
        } {
            map.entry(source).or_default().insert(target);
        }
        let pair = pair_ids.pair(source, target);
        existing.insert(pair);
        if ["calls", "indirect_call"].contains(&relation) {
            call_pairs.insert(pair);
        }
    }
    // Enum members (`case_of`) and fields/properties (`defines`) are never what
    // a C# `new X()` constructs (#3815, #3888).
    let member_ids: FxHashSet<&str> = edges
        .iter()
        .filter(|e| matches!(rs(*e, "relation"), "case_of" | "defines"))
        .map(|e| rs(e, "target"))
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
    // nested type id -> its outer type's label, built on first use.
    let mut outer_type: Option<FxHashMap<&str, &str>> = None;
    // The Go package of every candidate's file, read from disk once per pass.
    let mut go_modules = types::GoModules::new(root);
    // The tables borrow from `edges`; new edges are appended after the loop.
    let mut added = vec![];
    for &rc in raw {
        // Bound by an ambiguous absolute import (#3729): never guess a target.
        if rc
            .rec_get("_ambiguous_python_import")
            .is_some_and(crate::build::truthy)
        {
            continue;
        }
        let callee = rs(rc, "callee");
        let sf = rs(rc, "source_file");
        if callee.is_empty()
            || is_language_builtin(callee)
            || rc.rec_get("is_member_call") == Some(&Value::Bool(true))
            || rc.rec_get("is_mixin") == Some(&Value::Bool(true))
            || ["bash", "markdown"].contains(&rs(rc, "language"))
            // A Solidity name is visible only through the caller contract's own
            // bases; `resolve_solidity_inherited_calls` binds those (0.9.92,
            // #4379). A corpus-wide match would link `_transfer()` to any
            // contract's `_transfer`.
            || rc.rec_get("solidity_call").is_some_and(crate::build::truthy)
            || (rs(rc, "language") == "go" && extractors::go::PREDECLARED_FUNCS.contains(&callee))
        {
            continue;
        }
        let mut cs = labels.get(callee).cloned().unwrap_or_default();
        if cs.is_empty() && merge::case_insensitive(sf) {
            cs = ci.get(&callee.to_lowercase()).cloned().unwrap_or_default();
        }
        // An enum member or a field/property is never a C# call target, nor
        // what `new X()` constructs (#3815, #3888, #4245).
        let csharp_new = rc.rec_get("csharp_new").is_some_and(crate::build::truthy);
        if rs(rc, "lang") == "csharp" && !csharp_new && cs.len() > 1 {
            let (node_by_id, owner) = overload_index.get_or_insert_with(|| {
                let node_by_id = nodes
                    .iter()
                    .map(|n| (s(n, "id"), n))
                    .collect::<FxHashMap<_, _>>();
                let mut owner = FxHashMap::default();
                let has_arity = |id: &str| {
                    node_by_id
                        .get(id)
                        .is_some_and(|n| n["metadata"].get("arity").is_some())
                };
                for e in edges.iter() {
                    if rs(e, "relation") == "method" && has_arity(rs(e, "target")) {
                        owner.insert(rs(e, "target").to_string(), rs(e, "source").to_string());
                    }
                }
                for e in context_edges {
                    if s(e, "relation") == "method" && has_arity(s(e, "target")) {
                        owner.insert(s(e, "target").to_string(), s(e, "source").to_string());
                    }
                }
                (node_by_id, owner)
            });
            cs = csharp_collapse_overloads(
                std::mem::take(&mut cs),
                rc.rec_get("arity").and_then(Value::as_i64),
                owner,
                node_by_id,
            );
        }
        if csharp_new || rs(rc, "lang") == "csharp" {
            cs.retain(|c| !member_ids.contains(c.as_str()));
        }
        if csharp_new {
            // Nor does `new X()` construct a method that shares X's name:
            // every `new List<T>()` would bind to a `List()` method (0.9.90).
            let owners = method_owner.get_or_insert_with(|| {
                edges
                    .iter()
                    .filter(|e| rs(*e, "relation") == "method")
                    .map(|e| (rs(e, "target"), rs(e, "source")))
                    .collect::<FxHashMap<_, _>>()
            });
            cs.retain(|c| !owners.contains_key(c.as_str()));
        }
        // A call rescued from an annotation stub (#3888) binds to a same-named
        // class elsewhere only with evidence: an import, or the C# scope.
        let via_stub = rc.rec_get("via_stub") == Some(&Value::Bool(true));
        let mut stub_scope_hit = false;
        if via_stub && rs(rc, "lang") == "csharp" && !cs.is_empty() {
            let resolver = stub_resolver.get_or_insert_with(|| {
                extractors::csharp::CsharpNameResolver::new(nodes, &E::json_slice(edges))
            });
            if let Some(caller_node) = resolver.node_by_id.get(rs(rc, "caller_nid"))
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
        let go_exact = rs(rc, "language") == "go" && !rs(rc, "import_path").is_empty();
        if go_exact {
            cs.retain(|c| {
                go_modules
                    .import_path(Path::new(id_source.get(c.as_str()).copied().unwrap_or("")))
                    .is_some_and(|p| p == rs(rc, "import_path"))
            });
        }
        // An unqualified Elixir call can only reach the caller's own module,
        // Kernel, or a module imported or used in the caller's lexical scope
        // (Graphify c96a743, 48de615, #4001).
        if let Some(scope) = rc.rec_get("elixir_call_scope").filter(|v| !v.is_null()) {
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
        let caller = rs(rc, "caller_nid");
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
        if cs.len() > 1 && rs(rc, "lang") == "csharp" && !csharp_new {
            let owners = method_owner.get_or_insert_with(|| {
                edges
                    .iter()
                    .filter(|e| rs(*e, "relation") == "method")
                    .map(|e| (rs(e, "target"), rs(e, "source")))
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
                    // A module import only proves the caller pulled in the
                    // callee's file, not this symbol: the edge stays INFERRED
                    // (0.9.95, #2945).
                    (module[0].clone(), false)
                } else if csharp_new && !rs(rc, "qualified_prefix").is_empty() {
                    // A qualified construction (`new Outer.Slot()`) names
                    // which Slot it means: the one nested in a type called
                    // Outer, or no edge; a proximity guess over the bare name
                    // would ignore it (0.9.90). A namespace qualifier matches
                    // no outer type and is left to the qualified-call pass.
                    let outers = outer_type.get_or_insert_with(|| {
                        edges
                            .iter()
                            .filter(|e| rs(*e, "relation") == "contains")
                            .map(|e| {
                                (
                                    rs(e, "target"),
                                    id_label.get(rs(e, "source")).copied().unwrap_or(""),
                                )
                            })
                            .collect::<FxHashMap<_, _>>()
                    });
                    let outer = rs(rc, "qualified_prefix").rsplit('.').next().unwrap_or("");
                    let nested = cs
                        .iter()
                        .filter(|c| outers.get(c.as_str()) == Some(&outer))
                        .collect::<Vec<_>>();
                    if nested.len() != 1 {
                        continue;
                    }
                    (nested[0].clone(), false)
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
        let indirect = rc.rec_get("indirect") == Some(&Value::Bool(true));
        if indirect {
            if !callable.contains(target.as_str())
                || classes.contains(target.as_str())
                || !call_pairs.insert(pair_ids.pair(caller, &target))
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
            if !existing.insert(pair_ids.pair(caller, &target)) {
                continue;
            }
        }
        let label = id_label.get(target.as_str()).copied().unwrap_or("");
        let constructor =
            !indirect && sf.ends_with(".rs") && !label.is_empty() && !label.ends_with(')');
        let mut edge = json!({"source":caller,"target":target,"relation":if indirect{"indirect_call"}else if constructor{"references"}else{"calls"},"context":if indirect{rc.rec_get("context").and_then(Value::as_str).unwrap_or("argument")}else if constructor{"constructor"}else{"call"},"confidence":if !indirect&&evidence{"EXTRACTED"}else{"INFERRED"},"confidence_score":if !indirect&&evidence{1.0}else{0.85},"source_file":sf,"source_location":rc.rec_get("source_location").cloned().unwrap_or(Value::Null),"weight":1.0});
        tighten(&mut edge);
        let Value::Object(edge) = edge else {
            unreachable!("an edge literal is an object")
        };
        added.push(E::from_object(edge));
    }
    edges.reserve_exact(added.len());
    edges.extend(added);
    repoint_go_imports(
        &sf_file,
        edges,
        &mut go_modules,
        &mut existing,
        &mut pair_ids,
    );
}

/// Id texts numbered once for the `(source, target)` pair tables of
/// [`bind_raw_calls`]: a pair is two integers rather than two owned copies of
/// its id strings (Kubernetes holds about 1.6 million pairs at once). Two
/// texts get the same number exactly when they are equal, so every lookup
/// answers as it would on the strings.
#[derive(Default)]
struct PairIds(FxHashMap<Box<str>, u32>);
impl PairIds {
    fn id(&mut self, text: &str) -> u32 {
        if let Some(&id) = self.0.get(text) {
            return id;
        }
        let id = u32::try_from(self.0.len()).expect("fewer than 2^32 distinct ids");
        self.0.insert(text.into(), id);
        id
    }
    fn pair(&mut self, source: &str, target: &str) -> (u32, u32) {
        (self.id(source), self.id(target))
    }
}

/// #3746: repoint intra-module Go `imports_from` edges from the
/// synthetic `go_pkg_<import path>` sink onto the imported package's file
/// nodes. Stdlib and external imports have no file in the corpus and stay
/// sinks. Mirrors the block that follows qualified-call binding in
/// upstream `extract.py`.
fn repoint_go_imports<E: Rec>(
    sf_file: &FxHashMap<&str, String>,
    edges: &mut Vec<E>,
    go_modules: &mut types::GoModules<'_>,
    existing: &mut FxHashSet<(u32, u32)>,
    pair_ids: &mut PairIds,
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
        if let Some(import_path) = go_modules.import_path(Path::new(sf)) {
            go_pkg_files
                .entry(import_path.to_owned())
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
    let sink_of = |e: &E| {
        if rs(e, "relation") == "imports_from" {
            sinks.get(rs(e, "target")).copied()
        } else {
            None
        }
    };
    // Decide every repointed copy first, in edge order and with the same pair
    // checks, so the rewritten list is allocated once at its final length
    // (Kubernetes: about 750,000 copies) instead of doubling beside the old.
    let mut counts = vec![];
    let mut targets: Vec<&String> = vec![];
    for e in edges.iter() {
        let Some(import_path) = sink_of(e) else {
            continue;
        };
        let source = rs(e, "source");
        let before = targets.len();
        for fnid in &go_pkg_files[import_path] {
            if fnid.as_str() == source || !existing.insert(pair_ids.pair(source, fnid)) {
                continue;
            }
            targets.push(fnid);
        }
        counts.push(targets.len() - before);
    }
    let mut rewritten = Vec::with_capacity(edges.len() - counts.len() + targets.len());
    let (mut counts, mut targets) = (counts.into_iter(), targets.into_iter());
    for e in edges.drain(..) {
        if sink_of(&e).is_none() {
            rewritten.push(e);
            continue;
        }
        for fnid in targets.by_ref().take(counts.next().unwrap_or(0)) {
            let mut repointed = e.clone();
            repointed.rec_set_str("target", fnid);
            rewritten.push(repointed);
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

    /// Assembly's record path (edges as compact records from raw-call binding
    /// on: bound calls, repointed Go imports, the Python member pass, the Rust
    /// `use` group drop and `finish`) gives byte for byte the extraction of
    /// the JSON path, cold and from the cache, and with the intermediate output.
    #[test]
    fn record_edges_extract_exactly_like_json_edges() {
        let _serial = extractors::parse_cache::test_serial();
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let write = |rel: &str, text: &str| {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, text).unwrap();
            path
        };
        write("go.mod", "module example.com/demo\n\ngo 1.22\n");
        let mut paths = vec![
            write(
                "a/a.go",
                "package a\n\nfunc Alpha() int { return 1 }\n\ntype Thing struct{}\n\nfunc (t *Thing) Do() int { return Alpha() }\n",
            ),
            write(
                "a/a2.go",
                "package a\n\nfunc Beta() int { return Alpha() }\n",
            ),
            write(
                "b/b.go",
                "package b\n\nimport (\n\t\"fmt\"\n\t\"example.com/demo/a\"\n)\n\nfunc Bravo() { fmt.Println(a.Alpha()); Charlie() }\n\nfunc Charlie() {}\n",
            ),
            write(
                "c/c.go",
                "package c\n\nimport \"example.com/demo/a\"\n\nfunc Cee() int { return a.Beta() + Helper() }\n\nfunc Helper() int { return 2 }\n",
            ),
            write(
                "py/models.py",
                "class Base:\n    def run(self):\n        return 1\n\n\nclass Child(Base):\n    def go(self):\n        return self.run()\n",
            ),
            write(
                "py/app.py",
                "import models\nfrom models import Child\n\n\ndef main():\n    c = Child()\n    models.Base().run()\n    return c.go()\n",
            ),
            write(
                "src/lib.rs",
                "use std::collections::{HashMap, HashSet};\n\npub struct S;\n\nimpl S {\n    pub fn new() -> Self { S }\n    pub fn f(&self) -> usize { self.g() }\n    fn g(&self) -> usize { 1 }\n}\n\npub fn make() -> S { S::new() }\n",
            ),
        ];
        paths.sort();
        let run = |json_only: bool, use_cache: bool, include_intermediate: bool| {
            crate::resolver_registry::JSON_ONLY.with(|f| f.set(json_only));
            let out = extract(
                &paths,
                &ExtractOptions {
                    root: Some(root.clone()),
                    cache_root: Some(root.join("cache")),
                    use_cache,
                    include_intermediate,
                    parallel: false,
                    ..Default::default()
                },
            );
            crate::resolver_registry::JSON_ONLY.with(|f| f.set(false));
            serde_json::to_string(&out.unwrap()).unwrap()
        };
        for (use_cache, intermediate) in
            [(false, false), (true, false), (true, false), (false, true)]
        {
            let json = run(true, use_cache, intermediate);
            let records = run(false, use_cache, intermediate);
            assert_eq!(
                records, json,
                "cache {use_cache}, intermediate {intermediate}"
            );
        }
        // The fixture reaches the passes the record path runs.
        let out: Value = serde_json::from_str(&run(false, false, false)).unwrap();
        let edges = out["edges"].as_array().unwrap();
        assert!(edges.iter().any(|e| e["relation"] == "calls"));
        assert!(edges.iter().any(|e| {
            e["relation"] == "imports_from"
                && e["target"]
                    .as_str()
                    .is_some_and(|t| !t.starts_with("go_pkg"))
        }));
        assert!(edges.iter().all(|e| e["_origin"] == "ast"));
    }

    /// Assembly over compact per-file results (node and raw-call records as
    /// `Attributes`) gives byte for byte the extraction over JSON results, for
    /// every language pass that reads them, cold, from the cache (all hits)
    /// and with the intermediate output; the cache entries are the same bytes.
    #[test]
    fn per_file_records_extract_exactly_like_json_results() {
        let _serial = extractors::parse_cache::test_serial();
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let write = |rel: &str, text: &str| {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, text).unwrap();
            path
        };
        write("go.mod", "module example.com/demo\n\ngo 1.22\n");
        let mut paths = vec![
            write(
                "a/a.go",
                "package a\n\nimport \"example.com/demo/b\"\n\ntype Thing struct{ B b.Item }\n\nfunc Alpha() int { return b.Make().N }\n",
            ),
            write(
                "b/b.go",
                "package b\n\ntype Item struct{ N int }\n\nfunc Make() Item { return Item{} }\n",
            ),
            write("py/pkg/__init__.py", ""),
            write(
                "py/pkg/models.py",
                "class Base:\n    def run(self):\n        return 1\n\n\nclass Child(Base):\n    def go(self):\n        return self.run()\n",
            ),
            write(
                "py/app.py",
                "from pkg import models\nfrom pkg.models import Child\n\n\ndef main():\n    models.Base().run()\n    return Child().go()\n",
            ),
            write("scripts/lib.sh", "#!/bin/bash\nhelper() {\n  echo hi\n}\n"),
            write(
                "scripts/main.sh",
                "#!/bin/bash\nsource ./lib.sh\nmain() {\n  helper\n}\nmain\n",
            ),
            write(
                "k/A.kt",
                "package demo.k\n\nclass A {\n    fun f(): Int = 1\n}\n",
            ),
            write(
                "k/B.kt",
                "package demo.other\n\nimport demo.k.A\n\nfun g(): Int = A().f()\n",
            ),
            write(
                "cs/P1.cs",
                "namespace N {\n  public partial class P {\n    public void A() { B(); }\n  }\n}\n",
            ),
            write(
                "cs/P2.cs",
                "namespace N {\n  public partial class P {\n    public void B() { }\n  }\n}\n",
            ),
            write("sw/S.swift", "struct S {\n    func a() {}\n}\n"),
            write("sw/E.swift", "extension S {\n    func f() { a() }\n}\n"),
            write("tf/main.tf", "module \"m\" {\n  source = \"./mod\"\n}\n"),
            write("tf/mod/main.tf", "variable \"x\" {}\n"),
            write(
                "j/p/A.java",
                "package p;\n\npublic class A {\n    public void f() {}\n}\n",
            ),
            write(
                "j/q/B.java",
                "package q;\n\nimport p.A;\n\nclass B {\n    void g() { new A().f(); }\n}\n",
            ),
            write(
                "ph/A.php",
                "<?php\nnamespace P;\n\nclass A {\n    public function f() {}\n}\n",
            ),
            write(
                "ph/B.php",
                "<?php\nnamespace Q;\n\nuse P\\A;\n\nclass B {\n    public function g() { (new A())->f(); }\n}\n",
            ),
            write(
                "src/lib.rs",
                "pub struct S;\n\nimpl S {\n    pub fn f(&self) -> usize { self.g() }\n    fn g(&self) -> usize { 1 }\n}\n",
            ),
        ];
        paths.sort();
        let cache_files = |dir: &Path| {
            let mut files = std::collections::BTreeMap::new();
            let mut stack = vec![dir.to_path_buf()];
            while let Some(d) = stack.pop() {
                for entry in std::fs::read_dir(&d).into_iter().flatten().flatten() {
                    let path = entry.path();
                    if path.is_dir() {
                        stack.push(path);
                    } else if path.extension().is_some_and(|e| e == "json")
                        && !path.ends_with("stat-index.json")
                    {
                        files.insert(
                            path.strip_prefix(dir).unwrap().to_path_buf(),
                            std::fs::read(&path).unwrap(),
                        );
                    }
                }
            }
            files
        };
        let run = |json_only: bool, cache: &str, use_cache: bool, include_intermediate: bool| {
            crate::resolver_registry::JSON_ONLY.with(|f| f.set(json_only));
            let out = extract(
                &paths,
                &ExtractOptions {
                    root: Some(root.clone()),
                    cache_root: Some(root.join(cache)),
                    use_cache,
                    include_intermediate,
                    parallel: false,
                    ..Default::default()
                },
            );
            crate::resolver_registry::JSON_ONLY.with(|f| f.set(false));
            serde_json::to_string(&out.unwrap()).unwrap()
        };
        for (use_cache, intermediate) in [(true, false), (true, false), (true, true), (false, true)]
        {
            let json = run(true, "cache-json", use_cache, intermediate);
            let records = run(false, "cache-records", use_cache, intermediate);
            assert_eq!(
                records, json,
                "cache {use_cache}, intermediate {intermediate}"
            );
            assert_eq!(
                cache_files(&root.join("cache-records")),
                cache_files(&root.join("cache-json"))
            );
        }
        // The fixture reaches the per-file passes: Go type references, Bash
        // sources, Kotlin imports, the C# partial merge and bound calls.
        let out: Value = serde_json::from_str(&run(false, "cache-check", false, false)).unwrap();
        let edges = out["edges"].as_array().unwrap();
        assert!(edges.iter().any(|e| e["relation"] == "calls"));
        assert!(edges.iter().any(|e| {
            e["source_file"]
                .as_str()
                .is_some_and(|f| f.ends_with(".sh"))
                && e["relation"] != "contains"
        }));
        assert!(
            !cache_files(&root.join("cache-records")).is_empty(),
            "the cache holds entries"
        );
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
