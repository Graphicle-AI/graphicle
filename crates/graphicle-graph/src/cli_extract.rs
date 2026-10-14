// Adapted from Graphify 0.9.88 (a9860f70), Apache-2.0; see NOTICE.md.
// Upstream: graphify/__main__.py, graphify/cli.py
//! Headless AST/semantic extraction, cache checkpoints, merge and manifest commit.
use crate::{
    Result, build,
    cache::{ExtractionCache, Prompt, SemanticOptions},
    cli::{Args, Output, expand},
    cli_state::{self as state, ar, strings},
    detect, extract, llm, paths,
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeSet, HashSet},
    fs,
    path::{Path, PathBuf},
};
fn empty() -> Value {
    json!({"nodes":[],"edges":[],"hyperedges":[],"input_tokens":0,"output_tokens":0})
}

/// `extract --dry-run` (opt-in, beyond the upstream port): what a full extract would
/// send to the LLM, without calling it, writing a graph or persisting
/// `--docs`/`--routes`/`--exclude` (detection still records its file-stat
/// cache). Token counts are the packer's estimate
/// (characters / 4, plus the prompt of every call); output tokens are not
/// estimated.
#[allow(clippy::too_many_arguments)]
fn dry_run(
    a: &Args,
    out: &mut Output,
    code: &[PathBuf],
    semantic: &[PathBuf],
    cfg: &crate::llm_config::Configuration,
    d: &mut build::Diagnostics,
    budget: i64,
    deep: bool,
    force: bool,
    target: &Path,
    out_root: &Path,
) -> Result<()> {
    let paths = semantic
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    let uncached = if force || paths.is_empty() {
        paths.clone()
    } else {
        let options = SemanticOptions {
            root: target.to_path_buf(),
            cache_root: Some(out_root.to_path_buf()),
            mode: deep.then(|| "deep".into()),
            prompt: Some(Prompt::Text(llm::extraction_system(deep))),
            ..Default::default()
        };
        strings(&ExtractionCache::default().check_semantic(&paths, &options)?[3])
    };
    let mut packer = crate::llm_source::TokenPacker::default();
    let units = packer.expand(
        &uncached
            .iter()
            .map(|p| crate::file_slice::Unit::Path(PathBuf::from(p)))
            .collect::<Vec<_>>(),
    )?;
    let calls = if units.is_empty() {
        0
    } else {
        packer.pack(&units, budget, false)?.len()
    };
    let content = units.iter().map(|u| packer.estimate(u, false)).sum::<i64>();
    let prompt = (llm::extraction_system(deep).chars().count() as i64 + 3) / 4;
    let input = content + prompt * calls as i64;
    out.line(format!(
        "[graphicle extract] dry run: {} files for the AST pass (no LLM); {} of {} semantic files need the LLM ({} cached) in {calls} calls, about {input} input tokens",
        code.len(),
        uncached.len(),
        semantic.len(),
        semantic.len() - uncached.len(),
    ));
    let backend = a
        .get("backend")
        .map(str::to_string)
        .or_else(|| cfg.detect_backend(d).ok().flatten());
    match backend {
        Some(b) => out.line(format!(
            "[graphicle extract] input at {b} prices: about ${:.2} (output tokens not estimated); nothing was sent",
            cfg.estimate_cost(&b, input as f64, 0.0)
        )),
        None => out.line(
            "[graphicle extract] no LLM backend configured: pass --backend NAME for a price; nothing was sent",
        ),
    }
    Ok(())
}
fn paths_for(data: &Value, key: &str) -> Vec<PathBuf> {
    strings(&data[key]).into_iter().map(PathBuf::from).collect()
}
fn tokens(v: &Value, key: &str) -> i64 {
    v[key].as_i64().unwrap_or(0)
}
fn context(
    existing: &Value,
    root: &Path,
    files: &Value,
    code: &[PathBuf],
) -> (Vec<Value>, Vec<Value>) {
    let identity = |s: &str| crate::cache_paths::normalize(&root.join(s));
    let mut live = files
        .as_object()
        .into_iter()
        .flatten()
        .flat_map(|(_, v)| strings(v))
        .map(|s| identity(&s))
        .collect::<BTreeSet<_>>();
    for p in code {
        live.remove(&identity(&p.to_string_lossy()));
    }
    let mut ns = vec![];
    let mut es = vec![];
    for n in ar(&existing["nodes"]) {
        if !build::truthy(&n["id"])
            || !build::is_ast_tier(n)
            || !n["source_file"]
                .as_str()
                .is_some_and(|s| live.contains(&identity(s)))
        {
            continue;
        }
        let mut c = json!({});
        for k in ["id", "label", "source_file", "file_type", "type"] {
            c[k] = n[k].clone();
        }
        // `--routes`: requests in re-extracted files reach stored routes.
        if n["node_kind"] == "route" {
            for k in ["node_kind", "route_method", "route_path"] {
                c[k] = n[k].clone();
            }
        }
        // Keep bounded resolver identity for unchanged nodes; these markers
        // cannot be reconstructed from labels.
        for k in [
            "_callable",
            "_callable_class",
            "_elixir_module",
            "_rust_impl_key",
            "_rust_declaration_count",
            "_ts_public_twin",
            "_py_property",
            "_java_ret",
        ] {
            if build::truthy(&n[k]) {
                c[k] = n[k].clone();
            }
        }
        let mut metadata = json!({});
        for k in [
            "ruby_resolution_schema",
            "ruby_method_kind",
            "ruby_lookup_unsafe",
            "python_opaque_bases",
            "ruby_reopened",
            "ruby_external_method_owners",
            // Erlang remote-call resolution keys (#3714, 0.9.66): an unchanged
            // callee module keeps module/name/arity on incremental rebuilds.
            "language",
            "kind",
            "module",
            "name",
            "arity",
        ] {
            if let Some(v) = n["metadata"].get(k) {
                metadata[k] = v.clone();
            }
        }
        if !metadata.as_object().unwrap().is_empty() {
            c["metadata"] = metadata;
        }
        ns.push(c);
    }
    for e in ar(existing.get("links").unwrap_or(&existing["edges"])) {
        if !["contains", "method", "inherits"].contains(&e["relation"].as_str().unwrap_or(""))
            || !build::is_ast_tier(e)
            || !e["source_file"]
                .as_str()
                .is_some_and(|s| live.contains(&identity(s)))
        {
            continue;
        }
        let mut c = json!({});
        for k in ["source", "target", "relation", "source_file"] {
            c[k] = e[k].clone();
        }
        if e["metadata"]["ruby_superclass_ref"].is_string() {
            c["metadata"] = json!({"ruby_superclass_ref":e["metadata"]["ruby_superclass_ref"]});
            if e["metadata"]["ruby_lexical_scopes"]
                .as_array()
                .is_some_and(|a| a.iter().all(Value::is_string))
            {
                c["metadata"]["ruby_lexical_scopes"] = e["metadata"]["ruby_lexical_scopes"].clone();
            }
        }
        es.push(c);
    }
    (ns, es)
}
/// The warning for a semantic pass that returned fewer files than it was sent
/// (#3105). When none of its chunks was truncated or hollow, the model simply
/// did not address every file, and re-sending the same chunk reproduces the
/// omission, so the warning says it is a coverage gap at this chunk size
/// (Graphify 0.9.85, #4101).
fn incomplete_semantic_warning(omitted: usize, truncated: usize) -> String {
    let note = if omitted > 0 && truncated == 0 {
        " None of this run's chunks came back truncated — the model returned complete, well-formed responses that simply did not address every file. Retrying with the same --token-budget will likely omit the same files again; try a smaller --token-budget to split the corpus into more, smaller chunks."
    } else {
        ""
    };
    format!(
        "[graphicle extract] semantic extraction is incomplete: {omitted} dispatched file(s) produced no nodes and {truncated} came back truncated or hollow.{note} The shrink guard stays armed for this write; pass --allow-partial to overwrite a larger existing graph anyway."
    )
}
pub fn run(a: &Args, out: &mut Output) -> Result<()> {
    let has_path = a.positional(0).is_some();
    if !has_path && a.get("postgres").is_none() {
        return Err("must specify a path to scan or a --postgres DSN".into());
    }
    let target = state::resolve(&expand(a.positional(0).unwrap_or(".")));
    if !target.exists() {
        return Err(format!("path not found: {}", target.display()).into());
    }
    if a.flag("no-dedup") && a.flag("dedup-llm") {
        return Err("--no-dedup and --dedup-llm are mutually exclusive".into());
    }
    if a.get("mode").is_some_and(|s| s != "deep") {
        return Err(format!(
            "unknown --mode '{}'. Available: deep",
            a.get("mode").unwrap()
        )
        .into());
    }
    let workers = a
        .get("max-workers")
        .map(|_| a.positive("max-workers", 1))
        .transpose()?;
    let budget = a.positive("token-budget", 60000)?;
    let concurrency = a.positive("max-concurrency", 4)?;
    let timeout = a.positive_float("api-timeout", 600.0)?;
    let resolution = a.positive_float("resolution", 1.0)?;
    let exclude_hubs = a
        .get("exclude-hubs")
        .map(|_| a.float("exclude-hubs", 0.0))
        .transpose()?;
    let force = crate::cli::force(a);
    let deep = a.get("mode") == Some("deep");
    let code_only = a.flag("code-only");
    let allow_partial = a.flag("allow-partial");
    // Write a graph that is smaller because dedup merged nodes; a deleted
    // file never needs this flag (#3774).
    let allow_dedup_shrink = a.flag("allow-dedup-shrink");
    let out_root = state::resolve(&a.path("out", a.path("output", &target)));
    let dir = out_root.join(paths::OUT_DIR.as_str());
    fs::create_dir_all(&dir).map_err(crate::Error::from)?;
    let effective_excludes = if a.all("exclude").is_empty() {
        crate::watch::build_excludes(&dir)
    } else {
        a.all("exclude")
    };
    let gitignore = !a.flag("no-gitignore") && crate::watch::build_gitignore(&dir);
    // A dry run leaves nothing a later build would read.
    let dry_run_only = a.flag("dry-run");
    if !dry_run_only {
        crate::watch::write_config(
            &dir,
            &a.all("exclude"),
            a.flag("no-gitignore").then_some(false),
        );
    }
    let docs = a.flag("docs") || crate::watch::build_docs(&dir);
    if a.flag("docs") && !dry_run_only {
        crate::watch::write_flag_config(&dir, "docs");
    }
    if docs {
        crate::extractors::rst::ENABLED.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    if a.flag("routes") && !dry_run_only {
        crate::watch::write_flag_config(&dir, "routes");
    }
    if a.flag("routes") || crate::watch::build_flag(&dir, "routes") {
        crate::extractors::routes::ENABLED.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    let graph_path = dir.join("graph.json");
    let manifest_path = dir.join("manifest.json");
    let incremental = has_path && graph_path.exists() && !force;
    let merge_existing = incremental || (code_only && graph_path.exists());
    let mut d = build::Diagnostics::default();
    let mut stages = state::StageTimer::new(a.flag("timing"));
    let opts = detect::DetectOptions {
        google_workspace: a.flag("google-workspace").then_some(true),
        extra_excludes: effective_excludes,
        gitignore,
        cache_root: Some(out_root.clone()),
        ..Default::default()
    };
    let mut detector = detect::Detector::default();
    // A stored graph is parsed while the corpus is detected (the stale-source
    // check below reads it); neither depends on the other.
    let (detection, stored) = std::thread::scope(|scope| {
        let stored = (has_path && graph_path.exists())
            .then(|| scope.spawn(|| state::read_graph(&graph_path)));
        let detection = if !has_path {
            Ok(json!({}))
        } else if incremental {
            detector.detect_incremental(&target, &manifest_path, "both", &opts)
        } else {
            detector.detect(&target, &opts)
        };
        let stored = stored.map(|h| {
            h.join()
                .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
        });
        (detection, stored)
    });
    let detection = detection?;
    let files = detection.get("files").cloned().unwrap_or(json!({}));
    let selection = if incremental {
        &detection["new_files"]
    } else {
        &files
    };
    let mut code = paths_for(selection, "code");
    let mut semantic = ["document", "paper", "image"]
        .iter()
        .flat_map(|k| paths_for(selection, k))
        .collect::<Vec<_>>();
    let mut prune = strings(&detection["deleted_files"]);
    prune.extend(strings(&detection["excluded_files"]));
    let mut seen = files
        .as_object()
        .into_iter()
        .flatten()
        .flat_map(|(_, v)| strings(v))
        .collect::<BTreeSet<_>>();
    seen.extend(strings(&detection["unclassified"]));
    // `--routes` (see `extractors::routes`): what the stored routes' paths
    // depend on outside their own files.
    let mut route_mounts = None;
    if graph_path.exists() {
        let stored = stored.unwrap_or_else(|| state::read_graph(&graph_path));
        if incremental
            && crate::extractors::routes::enabled()
            && let Ok(graph) = &stored
        {
            route_mounts = Some(crate::extractors::routes::stored_mounts(graph, |s| {
                target.join(s)
            }));
        }
        let (stale, kept) =
            state::stale_sources_in(stored, &graph_path, &target, &seen, &detection);
        prune.extend(stale);
        if kept > 0 {
            out.warn(format!("[graphicle] fail-closed: kept node(s) from {kept} source file(s) that left the scan corpus but still exist on disk"));
        }
    }
    if incremental {
        code.extend(
            state::zero_sources(
                &graph_path,
                &target,
                &strings(&detection["unchanged_files"]["code"]),
                false,
            )
            .into_iter()
            .map(PathBuf::from),
        );
        let unchanged = ["document", "paper", "image"]
            .iter()
            .flat_map(|k| strings(&detection["unchanged_files"][*k]))
            .collect::<Vec<_>>();
        semantic.extend(
            state::zero_sources(&graph_path, &target, &unchanged, true)
                .into_iter()
                .map(PathBuf::from),
        );
        code = crate::extractors::terraform::refresh_terraform_paths(
            &code,
            &paths_for(&files, "code"),
            &prune.iter().map(PathBuf::from).collect::<Vec<_>>(),
        );
        // A change that can move routes declared in unchanged files
        // re-extracts every code file, so their paths match a full extract.
        if let Some(mounts) = &route_mounts {
            let changed = code
                .iter()
                .cloned()
                .chain(
                    prune
                        .iter()
                        .flat_map(|p| [PathBuf::from(p), target.join(p)]),
                )
                .collect::<Vec<_>>();
            if mounts.moved_by(&changed) {
                code = paths_for(&files, "code");
                out.line("[graphicle extract] route mounts changed: re-extracting every code file");
            }
        }
    }
    if code_only {
        semantic.clear();
    } else if deep && incremental {
        semantic = ["document", "paper", "image"]
            .iter()
            .flat_map(|k| paths_for(&files, k))
            .collect();
    }
    // Opt-in beyond the upstream port (`--docs`, kept for `update`/`watch`): Markdown
    // and reStructuredText pages are extracted without an LLM and their
    // mentions linked to code, as `update` already does for Markdown. In a
    // full extract they are no longer sent to the LLM; PDFs, images and
    // other documents still are.
    if code_only && docs {
        let pages = paths_for(selection, "document")
            .into_iter()
            .filter(|p| extract::get_extractor(p).is_some())
            .collect::<Vec<_>>();
        code.extend(pages);
    } else if docs {
        let (pages, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut semantic)
            .into_iter()
            .partition(|p| extract::get_extractor(p).is_some());
        semantic = rest;
        code.extend(pages);
    }
    out.line(format!(
        "[graphicle extract] {} code and {} semantic files{}",
        code.len(),
        semantic.len(),
        if incremental {
            " changed (incremental)"
        } else {
            " found"
        }
    ));
    stages.mark("detect", &mut d.stderr);
    let mut cfg = crate::llm_config::Configuration::from_environment(&mut d);
    if a.flag("dry-run") {
        return dry_run(
            a, out, &code, &semantic, &cfg, &mut d, budget, deep, force, &target, &out_root,
        );
    }
    if a.get("api-timeout").is_some() {
        cfg.env
            .insert("GRAPHICLE_API_TIMEOUT".into(), timeout.to_string());
    }
    let needs_llm = !semantic.is_empty() || a.flag("dedup-llm");
    let backend = a.get("backend").map(str::to_string).or_else(|| {
        needs_llm
            .then(|| cfg.detect_backend(&mut d).ok().flatten())
            .flatten()
    });
    if let Some(name) = &backend {
        cfg.backend(name)?;
    }
    if needs_llm && backend.is_none() {
        return Err("no LLM backend configured; set an API key or pass --code-only for local AST extraction".into());
    }
    let mut runtime = llm::Runtime::new(cfg);
    let mut incomplete = !ar(&detection["walk_errors"]).is_empty();
    let mut ast = empty();
    if !code.is_empty() {
        let existing = if incremental {
            state::read_graph(&graph_path)?
        } else {
            Value::Null
        };
        let (ns, es) = context(&existing, &target, &detection["unchanged_files"], &code);
        match extract::extract(
            &code,
            &extract::ExtractOptions {
                root: Some(target.clone()),
                cache_root: Some(out_root.clone()),
                max_workers: workers.map(|n| n as usize),
                resolution_context_nodes: ns,
                resolution_context_edges: es,
                ..Default::default()
            },
        ) {
            Ok(v) => ast = v,
            Err(e) => {
                if !allow_partial {
                    return Err(format!("AST extraction failed: {e}").into());
                }
                out.warn(format!("AST extraction failed: {e}"));
                incomplete = true;
            }
        }
    }
    let failed = strings(&ast["failed_sources"]);
    incomplete |= !failed.is_empty();
    stages.mark("AST extract", &mut d.stderr);
    let mut cache = ExtractionCache::default();
    let mut sem = empty();
    let mut partial = vec![];
    let sem_options = SemanticOptions {
        root: target.clone(),
        cache_root: Some(out_root.clone()),
        mode: deep.then(|| "deep".into()),
        prompt: Some(Prompt::Text(llm::extraction_system(deep))),
        ..Default::default()
    };
    let semantic_paths = semantic
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    let mut hits = 0;
    if !semantic.is_empty() {
        let uncached = if force {
            semantic_paths.clone()
        } else {
            let cached = cache.check_semantic(&semantic_paths, &sem_options)?;
            sem["nodes"] = cached[0].clone();
            sem["edges"] = cached[1].clone();
            sem["hyperedges"] = cached[2].clone();
            strings(&cached[3])
        };
        hits = semantic.len() - uncached.len();
        if !uncached.is_empty() {
            let files = uncached
                .iter()
                .map(|p| crate::file_slice::Unit::Path(PathBuf::from(p)))
                .collect::<Vec<_>>();
            let mut succeeded = 0;
            let mut total = 0;
            let options = crate::llm_extract::CorpusOptions {
                extract: crate::llm_extract::ExtractOptions {
                    backend: backend.clone(),
                    model: a.get("model").map(str::to_string),
                    root: target.clone(),
                    deep_mode: deep,
                    ..Default::default()
                },
                token_budget: Some(budget),
                max_concurrency: concurrency,
                cache_root: Some(out_root.clone()),
                ..Default::default()
            };
            let mut fresh = match runtime.extract_corpus(
                &files,
                &options,
                &mut cache,
                &mut d,
                &mut |progress| {
                    total = progress.total;
                    succeeded += 1;
                },
            ) {
                Ok(v) => v,
                Err(e) => {
                    out.warn(format!("semantic extraction failed: {e}"));
                    incomplete = true;
                    empty()
                }
            };
            if succeeded == 0 {
                return Err(format!(
                    "all semantic chunks failed for backend '{}' ({} uncached files)",
                    backend.as_deref().unwrap_or(""),
                    uncached.len()
                )
                .into());
            }
            incomplete |= succeeded < total;
            let (dropped, n) = crate::cache_paths::scope(&mut fresh, &target, Some(&uncached))?;
            if !dropped.is_empty() {
                out.warn(format!(
                    "Dropped {n} out-of-scope items attributed to {} undispatched files",
                    dropped.len()
                ));
            }
            partial = crate::llm_parse::partial_source_files(&fresh);
            // A chunk that came back hollow, unparseable or omitting some of
            // its files counted as succeeded, so it arms the shrink guard like
            // a crashed chunk (#3105).
            let omitted = ar(&fresh["uncovered_files"]).len();
            if omitted > 0 || !partial.is_empty() {
                incomplete = true;
                let truncated = partial.iter().collect::<BTreeSet<_>>().len();
                out.warn(incomplete_semantic_warning(omitted, truncated));
            }
            let mut save = sem_options.clone();
            save.allowed_source_files = Some(uncached);
            save.partial_source_files = Some(partial.clone());
            if let Err(e) = cache.save_semantic(
                ar(&fresh["nodes"]),
                ar(&fresh["edges"]),
                ar(&fresh["hyperedges"]),
                &save,
            ) {
                out.warn(format!("could not write semantic cache: {e}"));
            }
            crate::llm_parse::strip_partial_markers(&mut fresh);
            for k in ["nodes", "edges", "hyperedges"] {
                sem[k]
                    .as_array_mut()
                    .unwrap()
                    .extend_from_slice(ar(&fresh[k]));
            }
            for k in ["input_tokens", "output_tokens"] {
                sem[k] = json!(tokens(&sem, k) + tokens(&fresh, k));
            }
        }
    }
    if has_path {
        let mut live = HashSet::new();
        let pages = ["document", "paper", "image"]
            .iter()
            .flat_map(|kind| paths_for(&files, kind))
            .map(|p| target.join(p))
            .filter(|p| p.is_file())
            .collect::<Vec<_>>();
        let pages = pages.iter().map(PathBuf::as_path).collect::<Vec<_>>();
        let workers = crate::par::workers(workers.map(|n| n as usize));
        for hash in cache
            .file_hashes(&pages, &target, Some(&out_root), workers)
            .into_iter()
            .flatten()
        {
            live.insert(hash);
        }
        if let Err(e) = cache.prune(&out_root, &live) {
            out.warn(format!("could not prune semantic cache: {e}"));
        }
    }
    cache.flush();
    for warning in std::mem::take(&mut cache.warnings) {
        out.warn(warning);
    }
    stages.mark("semantic extract", &mut d.stderr);
    let pg = if let Some(dsn) = a.get("postgres") {
        crate::pg_introspect::introspect_postgres(Some(dsn))?
    } else {
        empty()
    };
    let cargo = if a.flag("cargo") {
        let manifest_missing = !target.join("Cargo.toml").is_file()
            && crate::cargo_introspect::discover_root_manifest(&target).is_none();
        if manifest_missing {
            // #3677: a missing manifest degrades instead of aborting.
            out.line(
                "[graphicle extract] --cargo: no Cargo.toml at scan root, skipping crate edges",
            );
            empty()
        } else {
            crate::cargo_introspect::introspect_cargo(&target)?
        }
    } else {
        empty()
    };
    // Everything in `sem` is semantic tier, fresh results and cache hits
    // alike, so it is stamped before the merge (#2843): an unstamped item is
    // classified by the shape of its source_location, and a document node at
    // `L12` would read as AST and be deleted by the next incremental rebuild.
    llm::stamp_semantic_origin(&mut sem);
    let mut merged = empty();
    for key in ["nodes", "edges"] {
        // `ast` is not read again (only its counters below), so its records
        // move instead of being copied; the others are still read later.
        let mut all = match ast.get_mut(key).map(Value::take) {
            Some(Value::Array(items)) => items,
            _ => vec![],
        };
        all.extend(
            [&sem, &pg, &cargo]
                .iter()
                .flat_map(|r| ar(&r[key]).iter().cloned()),
        );
        merged[key] = Value::Array(all);
    }
    merged["hyperedges"] = sem["hyperedges"].clone();
    merged["extracted_sources"] = ast.get("extracted_sources").cloned().unwrap_or(json!([]));
    for k in ["input_tokens", "output_tokens"] {
        merged[k] = json!(tokens(&ast, k) + tokens(&sem, k));
    }
    let mut manifest = state::stamped(&files, &sem, &target, &partial, &failed, &[]);
    let mut handle_shrink = |shrink: &Value, out: &mut Output| -> Result<()> {
        if let Some(m) = shrink.as_object().filter(|m| !m.is_empty()) {
            out.warn(format!("semantic extraction is incomplete: unverified semantic shrink for {}. Use --allow-partial to accept this result.",m.keys().cloned().collect::<Vec<_>>().join(", ")));
            if !allow_partial {
                incomplete = true;
                for source in m.keys() {
                    cache.invalidate(&target.join(source), &sem_options.cache_options())?;
                }
                manifest = state::stamped(
                    &files,
                    &sem,
                    &target,
                    &partial,
                    &failed,
                    &m.keys().cloned().collect::<Vec<_>>(),
                );
            }
        }
        Ok(())
    };
    let no_changes = incremental
        && code.is_empty()
        && semantic.is_empty()
        && ar(&detection["deleted_files"]).is_empty()
        && ar(&pg["nodes"]).is_empty()
        && ar(&pg["edges"]).is_empty()
        && ar(&cargo["nodes"]).is_empty()
        && ar(&cargo["edges"]).is_empty();
    if a.flag("no-cluster") {
        if no_changes {
            if !prune.is_empty() {
                state::prune_sources(&graph_path, &prune)?;
            }
            out.line("[graphicle extract] no incremental changes detected (--no-cluster); outputs left untouched.");
        } else {
            if merge_existing {
                build::merge_raw_extraction(&mut merged, &graph_path, &prune, Some(&target), None)?;
                handle_shrink(&merged["_unverified_semantic_shrink"], out)?;
            }
            merged["nodes"] = json!(build::dedupe_nodes(ar(&merged["nodes"]))?);
            merged["edges"] = json!(build::dedupe_edges(ar(&merged["edges"]))?);
            build::disambiguate_file_labels_in_nodes(merged["nodes"].as_array_mut().unwrap());
            // The endpoint rules of the clustered build (#2873): an import of an
            // external module gets a typed stub node, and any other edge
            // without two declared endpoints is dropped instead of shipping a
            // phantom (Graphify 0.9.80, #4189).
            build::finalize_raw_graph_endpoints(&mut merged);
            let source = ar(&merged["nodes"])
                .iter()
                .map(|n| (n["id"].to_string(), n["source_file"].clone()))
                .collect::<std::collections::BTreeMap<_, _>>();
            for e in merged["edges"].as_array_mut().unwrap() {
                if !build::truthy(&e["source_file"]) {
                    e["source_file"] = source
                        .get(&e["source"].to_string())
                        .filter(|v| build::truthy(v))
                        .or_else(|| source.get(&e["target"].to_string()))
                        .cloned()
                        .unwrap_or(json!(""));
                }
            }
            if incomplete
                && !allow_partial
                && matches!(
                    crate::export::existing_graph_node_count(&graph_path),
                    crate::export::ExistingGraph::Malformed
                )
            {
                return Err("extraction was incomplete and existing graph is corrupt; refusing to overwrite".into());
            }
            if incomplete
                && !allow_partial
                && let crate::export::ExistingGraph::Count(old) =
                    crate::export::existing_graph_node_count(&graph_path)
                && ar(&merged["nodes"]).len() < old
            {
                return Err("extraction was incomplete and graph shrank; refusing to overwrite a complete graph with a partial one".into());
            }
            crate::export::backup_if_protected(&dir, &mut d);
            // merge_raw_extraction has consumed the run-only keys; graph.json
            // must not carry this run's absolute input paths (#4189).
            state::write_json_compact(
                &graph_path,
                &state::Without(&merged, &extract::RUN_ONLY_EXTRACTION_KEYS),
            )
            .map_err(crate::Error::from)?;
            out.line(format!(
                "[graphicle extract] wrote {} — {} nodes, {} edges (no clustering)",
                graph_path.display(),
                ar(&merged["nodes"]).len(),
                ar(&merged["edges"]).len()
            ));
        }
    } else {
        let options = build::BuildOptions {
            dedup: !a.flag("no-dedup"),
            dedup_llm_backend: a.flag("dedup-llm").then(|| backend.clone()).flatten(),
            root: Some(target.clone()),
            ..Default::default()
        };
        // Only the token counters of `merged` are read after the build, so the
        // build takes the extraction itself instead of two copies of it.
        let counters = json!({"input_tokens": merged["input_tokens"], "output_tokens": merged["output_tokens"]});
        let extraction = std::mem::replace(&mut merged, counters);
        let mut g = if merge_existing {
            build::build_merge_owned(
                vec![extraction],
                &build::MergeOptions {
                    graph_path: Some(graph_path.clone()),
                    prune_sources: prune,
                    root: Some(target.clone()),
                    dedup: options.dedup,
                    dedup_llm_backend: options.dedup_llm_backend.clone(),
                    ..Default::default()
                },
                &mut d,
                Some(&mut runtime),
            )?
        } else {
            build::build_owned(vec![extraction], &options, &mut d, Some(&mut runtime))?
        };
        handle_shrink(
            g.attributes
                .get("_unverified_semantic_shrink")
                .unwrap_or(&Value::Null),
            out,
        )?;
        if g.nodes.is_empty() {
            return Err("graph is empty — extraction produced no nodes".into());
        }
        stages.mark("build", &mut d.stderr);
        let comm = crate::cluster::cluster(&g, resolution, exclude_hubs, "auto")?;
        stages.mark("cluster", &mut d.stderr);
        crate::export::backup_if_protected(&dir, &mut d);
        // A complete run force-writes, so to_json's node-count guard never sees
        // a graph dedup made smaller: refuse that unless the user accepts it,
        // excusing only this run's deletions (#3774). --allow-partial does not
        // accept that shrink.
        let (dedup_collapsed, pruned_nodes) = build::take_shrink_accounting(&mut g);
        if !incomplete
            && options.dedup
            && !allow_dedup_shrink
            && dedup_collapsed > 0
            && let crate::export::ExistingGraph::Count(old) =
                crate::export::existing_graph_node_count(&graph_path)
            && (g.nodes.len() as u64) < (old as u64).saturating_sub(pruned_nodes)
        {
            out.diagnostics(d);
            out.warn(format!(
                "[graphicle extract] error: dedup merged {dedup_collapsed} node(s); writing would shrink the graph from {old} nodes to {} nodes. Refusing to overwrite {}. Pass --allow-dedup-shrink to write the smaller graph.",
                g.nodes.len(),
                graph_path.display()
            ));
            out.exit_code = 1;
            return Ok(());
        }
        let profiled = crate::profile::span("write.graph_json");
        if !crate::export::to_json(
            &g,
            &comm,
            &graph_path,
            &crate::export::JsonOptions {
                force: allow_partial || !incomplete,
                built_at_commit: crate::export::git_head(Some(&target)),
                ..Default::default()
            },
            &mut d,
        )? {
            out.diagnostics(d);
            return Err("extraction was incomplete; refusing to overwrite a complete graph with a smaller partial result".into());
        }
        drop(profiled);
        let profiled = crate::profile::span("write.analysis");
        // The analyses only read the graph; they run side by side.
        let (cohesion, gods, surprises) = std::thread::scope(|scope| {
            let cohesion = scope.spawn(|| crate::cluster::score_all(&g, &comm));
            let gods = scope.spawn(|| crate::analyze::god_nodes(&g, 10, exclude_hubs));
            let surprises =
                crate::analyze::surprising_connections(&g, &comm, crate::analyze::SURPRISES_TOP_N);
            let join = |h: std::thread::ScopedJoinHandle<'_, Value>| {
                h.join()
                    .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
            };
            let gods = gods
                .join()
                .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
            (join(cohesion), gods, surprises)
        });
        let analysis = json!({"communities":crate::cluster::communities_to_json(&comm),"cohesion":cohesion,"gods":gods.unwrap_or_default(),"surprises":surprises.unwrap_or_default(),"tokens":{"input":merged["input_tokens"],"output":merged["output_tokens"]}});
        paths::write_json_atomic(
            &dir.join(".graphicle_analysis.json"),
            &analysis,
            Some(2),
            true,
        )
        .map_err(crate::Error::from)?;
        drop(profiled);
        if tokens(&merged, "output_tokens") > 0 {
            paths::write_json_atomic(
                &dir.join(".graphicle_semantic_marker"),
                &json!({"output_tokens":merged["output_tokens"]}),
                None,
                true,
            )
            .map_err(crate::Error::from)?;
        }
        out.line(format!(
            "[graphicle extract] wrote {}: {} nodes, {} edges, {} communities",
            graph_path.display(),
            g.nodes.len(),
            g.edge_count(),
            comm.len()
        ));
        crate::par::drop_later((g, comm));
    }
    crate::install_fs::write(&dir.join(".graphicle_root"), &target.to_string_lossy())?;
    if has_path {
        let stamped = manifest
            .as_object()
            .unwrap()
            .values()
            .flat_map(strings)
            .collect::<BTreeSet<_>>();
        let clear = semantic_paths
            .iter()
            .filter(|s| !stamped.contains(*s))
            .cloned()
            .collect();
        let _profiled = crate::profile::span("write.manifest");
        detect::save_manifest(
            manifest.as_object().unwrap(),
            &manifest_path,
            "both",
            &detect::ManifestOptions {
                root: Some(target.clone()),
                scan_corpus: Some(
                    files
                        .as_object()
                        .into_iter()
                        .flatten()
                        .flat_map(|(_, v)| strings(v))
                        .collect(),
                ),
                clear_semantic: Some(clear),
                clear_ast: Some(failed),
                ..Default::default()
            },
        )?;
    } else {
        let _ = fs::remove_file(manifest_path);
    }
    if !incomplete {
        let _ = fs::remove_file(dir.join("needs_update"));
    }
    if a.flag("global") {
        let mut store = crate::global_graph::GlobalStore::default_location()?;
        let tag = a.get("as").unwrap_or_else(|| {
            target
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("project")
        });
        match store.add(&graph_path, tag) {
            Ok(v) => out.line(format!("[graphicle global] {v}")),
            Err(e) => out.warn(format!("could not merge global graph: {e}")),
        }
    }
    if hits > 0 {
        out.line(format!(
            "[graphicle extract] semantic cache: {hits} cached, {} re-extracted",
            semantic.len() - hits
        ));
    }
    if tokens(&merged, "input_tokens") != 0 || tokens(&merged, "output_tokens") != 0 {
        out.line(format!(
            "tokens: {} in / {} out; estimated cost ${:.4}",
            merged["input_tokens"],
            merged["output_tokens"],
            runtime.config.estimate_cost(
                backend.as_deref().unwrap_or(""),
                tokens(&merged, "input_tokens") as f64,
                tokens(&merged, "output_tokens") as f64
            )
        ));
    }
    stages.mark("write", &mut d.stderr);
    stages.total(&mut d.stderr);
    out.diagnostics(d);
    Ok(())
}
#[cfg(test)]
mod tests {
    use serde_json::json;

    /// Graphify 0.9.85 (#4101): only a run with omitted files and no
    /// truncated or hollow chunk gets the coverage note.
    #[test]
    fn incomplete_warning_names_a_coverage_gap_only_without_truncation() {
        let gap = super::incomplete_semantic_warning(2, 0);
        assert!(gap.starts_with("[graphicle extract] semantic extraction is incomplete: 2 dispatched file(s) produced no nodes and 0 came back truncated or hollow. None of this run's chunks came back truncated"), "{gap}");
        assert!(gap.contains("try a smaller --token-budget"), "{gap}");
        assert!(gap.ends_with("The shrink guard stays armed for this write; pass --allow-partial to overwrite a larger existing graph anyway."), "{gap}");
        for (omitted, truncated) in [(2, 1), (0, 1)] {
            let text = super::incomplete_semantic_warning(omitted, truncated);
            assert!(!text.contains("None of this run's chunks"), "{text}");
            assert!(text.contains(&format!("{omitted} dispatched file(s) produced no nodes and {truncated} came back truncated or hollow. The shrink guard")), "{text}");
        }
    }

    /// Graphify 0.9.81/0.9.87/0.9.88: unchanged nodes keep the resolver
    /// markers that cannot be rebuilt from labels.
    #[test]
    fn resolution_context_keeps_the_newer_markers() {
        let root = tempfile::tempdir().unwrap();
        let existing = json!({"nodes": [{
            "id": "m_f", "label": "f()", "source_file": "m.py", "file_type": "code", "type": "function",
            "source_location": "L1", "_origin": "ast",
            "_ts_public_twin": "m_g", "_py_property": true, "_java_ret": "Widget", "_callable": true,
            "metadata": {"python_opaque_bases": ["Base"], "other": 1},
        }], "links": []});
        let files = json!({"code": ["m.py"]});
        let (nodes, _) = super::context(&existing, root.path(), &files, &[]);
        assert_eq!(nodes.len(), 1, "{nodes:?}");
        let node = &nodes[0];
        for key in ["_ts_public_twin", "_py_property", "_java_ret", "_callable"] {
            assert_eq!(node[key], existing["nodes"][0][key], "{key}");
        }
        assert_eq!(node["metadata"], json!({"python_opaque_bases": ["Base"]}));
    }
}
