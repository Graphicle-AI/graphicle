// Adapted from Graphify 0.9.95 (92741d16), Apache-2.0; see NOTICE.md.
// Upstream: graphify/detect.py
//! File discovery and incremental corpus detection from graphify/detect.py.
pub use crate::detect_documents::{
    DocumentLimits, convert_office_file, count_words, docx_to_markdown, extract_pdf_text,
    file_within_size_cap, xlsx_extract_structure, xlsx_to_markdown, zip_within_caps,
};
pub use crate::detect_ignore::{EncodingWarnings, IgnoreState, Patterns};
pub use crate::detect_manifest::{ManifestOptions, load_manifest, save_manifest};
pub use crate::detect_types::{FileType, classify_file, is_sensitive};
use crate::{
    Result,
    cache::{CacheSession, ExtractionCache},
    detect_ignore as ig, detect_manifest as mf, detect_types as ty, fsutil, google_workspace as gw,
    io_compat, paths,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
};
#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DetectOptions {
    pub follow_symlinks: Option<bool>,
    pub google_workspace: Option<bool>,
    pub extra_excludes: Vec<String>,
    pub cache_root: Option<PathBuf>,
    #[serde(default = "yes")]
    pub gitignore: bool,
}
fn yes() -> bool {
    true
}
impl DetectOptions {
    pub fn new() -> Self {
        Self::default()
    }
}
impl Default for DetectOptions {
    fn default() -> Self {
        Self {
            follow_symlinks: None,
            google_workspace: None,
            extra_excludes: vec![],
            cache_root: None,
            gitignore: true,
        }
    }
}
pub struct Detector {
    pub cache: CacheSession,
    pub warnings: EncodingWarnings,
    pub output: String,
    pub limits: DocumentLimits,
    predicates: HashMap<u64, IgnoreState>,
    next_predicate: u64,
}
impl Default for Detector {
    fn default() -> Self {
        Self {
            cache: CacheSession::default(),
            warnings: EncodingWarnings::default(),
            output: paths::OUT_DIR.to_string(),
            limits: DocumentLimits::default(),
            predicates: HashMap::new(),
            next_predicate: 0,
        }
    }
}
struct Walk {
    memory: PathBuf,
    configured_output: PathBuf,
    configured_names: HashSet<String>,
    files: Vec<PathBuf>,
    seen: HashSet<PathBuf>,
    walk_errors: Vec<String>,
    ignored: Vec<String>,
    noise: Vec<String>,
    sensitive: Vec<String>,
}
impl Detector {
    fn walk(
        &mut self,
        dp: &Path,
        in_memory: bool,
        follow: bool,
        rules: &mut IgnoreState,
        state: &mut Walk,
        active: &mut HashSet<PathBuf>,
    ) -> Result<()> {
        // The resolved directory only guards symlink cycles, which exist only
        // when following links; otherwise it is never read.
        let real = if follow {
            fsutil::resolve_missing(dp).ok()
        } else {
            None
        };
        if follow
            && let Some(real) = &real
            && !active.insert(real.clone())
        {
            return Ok(());
        }
        let entries = match fs::read_dir(dp) {
            Ok(entries) => entries,
            Err(error) => {
                let target = fsutil::path_string(dp);
                let message = io_compat::read_error(&error, dp);
                state.walk_errors.push(format!("{target}: {message}"));
                self.warnings.messages.push(format!("[graphicle] WARNING: could not scan {target} ({message}); its files are missing from this run's enumeration."));
                if let Some(real) = &real {
                    active.remove(real);
                }
                return Ok(());
            }
        };
        let mut dirs = vec![];
        for entry in entries {
            match entry {
                Ok(entry) => {
                    let path = entry.path();
                    // `path.is_dir()` from the directory entry's type where it
                    // is not a link (which it follows), sparing a stat per entry.
                    let is_dir = match entry.file_type() {
                        Ok(kind) if !kind.is_symlink() => kind.is_dir(),
                        _ => path.is_dir(),
                    };
                    if is_dir {
                        dirs.push(path);
                    } else if !ty::contains("_SKIP_FILES", &entry.file_name().to_string_lossy())
                        && !ty::is_installed_rule_file(&path)
                        && state.seen.insert(path.clone())
                    {
                        state.files.push(path);
                    }
                }
                Err(error) => {
                    let target = fsutil::path_string(dp);
                    let message = io_compat::read_error(&error, dp);
                    state.walk_errors.push(format!("{target}: {message}"));
                    self.warnings.messages.push(format!("[graphicle] WARNING: could not scan {target} ({message}); its files are missing from this run's enumeration."));
                }
            }
        }
        if !in_memory {
            rules.load_directory(dp, &mut self.warnings)?;
        }
        for child in dirs {
            let name = child.file_name().unwrap_or_default().to_string_lossy();
            if !in_memory {
                let configured = state.configured_names.contains(name.as_ref())
                    && fsutil::resolve_missing(&child).is_ok_and(|p| p == state.configured_output);
                if configured || ty::is_noise_dir(&name, Some(dp)) {
                    state.noise.push(format!(
                        "{}{s}",
                        fsutil::path_string(&child),
                        s = std::path::MAIN_SEPARATOR
                    ));
                    continue;
                }
                if rules.is_scan_ignored(&child) {
                    state.ignored.push(format!(
                        "{}{s}",
                        fsutil::path_string(&child),
                        s = std::path::MAIN_SEPARATOR
                    ));
                    continue;
                }
                if follow && child.is_symlink() && !ig::resolves_under_root(&child, &rules.root) {
                    state.sensitive.push(format!(
                        "{} [symlink target outside scan root]",
                        fsutil::path_string(&child)
                    ));
                    continue;
                }
            }
            if follow || !child.is_symlink() {
                self.walk(&child, in_memory, follow, rules, state, active)?;
            }
        }
        if let Some(real) = &real {
            active.remove(real);
        }
        Ok(())
    }
    /// Total words of admitted files, each count cached against the file's
    /// stat; counted on up to `workers` threads, cached in `files` order.
    fn count_words_of(
        &mut self,
        files: &[PathBuf],
        root: &Path,
        options: &DetectOptions,
        workers: usize,
    ) -> Result<usize> {
        let limits = self.limits;
        let paths = files.iter().map(PathBuf::as_path).collect::<Vec<_>>();
        let counts = self.cache.cached_word_counts(
            &paths,
            root,
            options.cache_root.as_deref(),
            workers,
            |path| json!(count_words(path, limits)),
        )?;
        Ok(counts
            .iter()
            .map(|n| n.as_u64().unwrap_or(0) as usize)
            .sum())
    }
    pub fn detect(&mut self, root: &Path, options: &DetectOptions) -> Result<Value> {
        let root = fsutil::resolve_missing(root).map_err(crate::Error::from)?;
        // The project's extension->language declarations (0.9.87, #2961)
        // shape classification for this scan.
        crate::rcfile::activate_language_overrides(&root);
        let configured = root.join(&self.output);
        let mut names = HashSet::from([configured
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned()]);
        let configured = fsutil::resolve_missing(&configured)
            .unwrap_or_else(|_| ig::absolute_lexical(&configured));
        names.insert(
            configured
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
        );
        let mut rules = IgnoreState::new(
            &root,
            &options.extra_excludes,
            options.gitignore,
            &mut self.warnings,
        )?;
        let memory = root.join(&self.output).join("memory");
        let mut scan_roots = vec![root.clone()];
        if memory.exists() {
            scan_roots.push(memory.clone());
        }
        let mut walk = Walk {
            memory,
            configured_output: configured,
            configured_names: names,
            files: vec![],
            seen: HashSet::new(),
            walk_errors: vec![],
            ignored: vec![],
            noise: vec![],
            sensitive: vec![],
        };
        let profiled = crate::profile::span("detect.walk");
        for path in scan_roots {
            let in_memory = walk.memory.exists()
                && fsutil::path_string(&path).starts_with(&fsutil::path_string(&walk.memory));
            self.walk(
                &path,
                in_memory,
                options.follow_symlinks.unwrap_or(false),
                &mut rules,
                &mut walk,
                &mut HashSet::new(),
            )?;
        }
        walk.files.sort();
        drop(profiled);
        let converted = options
            .cache_root
            .as_ref()
            .map(|p| fsutil::resolve_missing(p).map_err(crate::Error::from))
            .transpose()?
            .unwrap_or_else(|| root.clone())
            .join(&self.output)
            .join("converted");
        let enabled = options
            .google_workspace
            .unwrap_or_else(|| gw::google_workspace_enabled(None));
        let mut files: Map<String, Value> = FileType::ALL
            .iter()
            .map(|t| (t.as_str().into(), json!([])))
            .collect();
        let mut unclassified = vec![];
        let mut total_words = 0usize;
        // The per-file checks (ignore rules, symlink targets, file type,
        // sensitive names, classification by extension or content) decide
        // each file on its own, so they run in parallel; the outcomes are then
        // applied in sorted order exactly as a sequential loop would, with
        // conversions (which write files) on this thread.
        enum Checked {
            Skip,
            Ignored(String),
            Sensitive(String),
            Unclassified(String),
            Admit(FileType),
        }
        let memory_exists = walk.memory.exists();
        let memory_prefix = fsutil::path_string(&walk.memory);
        let converted_prefix = fsutil::path_string(&converted);
        let resolved_root = fsutil::resolve_missing(&root).ok();
        let workers = crate::par::workers(None);
        let profiled = crate::profile::span("detect.checks");
        let checked = {
            let rules = &rules;
            crate::par::map_with(
                &walk.files,
                workers,
                <((ig::MatchCache, ig::MatchCache), HashMap<PathBuf, PathBuf>)>::default,
                |(caches, dirs), p| {
                    let path = fsutil::path_string(p);
                    let in_memory = memory_exists && path.starts_with(&memory_prefix);
                    if !in_memory && path.starts_with(&converted_prefix) {
                        return Checked::Skip;
                    }
                    if !in_memory && rules.is_scan_ignored_with(p, caches) {
                        return Checked::Ignored(path);
                    }
                    if !ig::resolves_under_resolved_root(p, resolved_root.as_deref(), dirs) {
                        return Checked::Sensitive(format!(
                            "{path} [symlink target outside scan root]"
                        ));
                    }
                    if !p.is_file() {
                        return Checked::Sensitive(format!("{path} [not a regular file]"));
                    }
                    if is_sensitive(p) {
                        return Checked::Sensitive(path);
                    }
                    match classify_file(p) {
                        Some(kind) => Checked::Admit(kind),
                        None => Checked::Unclassified(path),
                    }
                },
            )
        };
        drop(profiled);
        let profiled = crate::profile::span("detect.admit");
        let mut counted = vec![];
        for (p, checked) in walk.files.into_iter().zip(checked) {
            let path = fsutil::path_string(&p);
            let kind = match checked {
                Checked::Skip => continue,
                Checked::Ignored(path) => {
                    walk.ignored.push(path);
                    continue;
                }
                Checked::Sensitive(entry) => {
                    walk.sensitive.push(entry);
                    continue;
                }
                Checked::Unclassified(path) => {
                    unclassified.push(path);
                    continue;
                }
                Checked::Admit(kind) => kind,
            };
            let suffix = ty::suffix(&p);
            let is_google = gw::GOOGLE_WORKSPACE_EXTENSIONS.contains(&suffix.as_str());
            let is_office = ty::contains("OFFICE_EXTENSIONS", &suffix);
            let admitted = if is_google {
                if !enabled {
                    walk.sensitive.push(format!("{path} [Google Workspace shortcut skipped - pass --google-workspace or set GRAPHICLE_GOOGLE_WORKSPACE=1]"));
                    continue;
                }
                let limits = self.limits;
                let converter = move |p: &Path| xlsx_to_markdown(p, limits);
                match gw::convert_google_workspace_file(
                    &p,
                    &converted,
                    Some(&converter),
                    Some(&root),
                ) {
                    Ok(Some(path)) => path,
                    Ok(None) => {
                        walk.sensitive.push(format!(
                            "{path} [Google Workspace export produced no readable text]"
                        ));
                        continue;
                    }
                    Err(error) => {
                        walk.sensitive
                            .push(format!("{path} [Google Workspace export failed: {error}]"));
                        continue;
                    }
                }
            } else if is_office {
                let converted_file =
                    match convert_office_file(&p, &converted, Some(&root), self.limits) {
                        Ok(converted_file) => converted_file,
                        Err(error) => {
                            // The files before this one were counted by now.
                            self.count_words_of(&counted, &root, options, workers)?;
                            return Err(error);
                        }
                    };
                match converted_file {
                    Some(path) => path,
                    None => {
                        walk.sensitive
                            .push(format!("{path} [office conversion failed]"));
                        continue;
                    }
                }
            } else {
                p
            };
            if (is_google || is_office)
                && rules.is_scan_ignored(&admitted)
                && !admitted.starts_with(&converted)
            {
                continue;
            }
            files[kind.as_str()]
                .as_array_mut()
                .unwrap()
                .push(json!(fsutil::path_string(&admitted)));
            // Binary media never counts as text (Graphify 0.9.95, #4548); this
            // also bypasses counts cached from such bytes before.
            if !crate::detect_documents::is_binary_media(&admitted) {
                counted.push(admitted);
            }
        }
        drop(profiled);
        let profiled = crate::profile::span("detect.word_counts");
        total_words += self.count_words_of(&counted, &root, options, workers)?;
        drop(profiled);
        for list in files.values_mut() {
            list.as_array_mut()
                .unwrap()
                .sort_by(|a, b| a.as_str().cmp(&b.as_str()));
        }
        let total_files: usize = files.values().map(|v| v.as_array().unwrap().len()).sum();
        let needs_graph = total_words >= 50000;
        let number = |n: usize| {
            let s = n.to_string();
            s.chars()
                .enumerate()
                .map(|(i, c)| {
                    if i > 0 && (s.len() - i).is_multiple_of(3) {
                        format!(",{c}")
                    } else {
                        c.to_string()
                    }
                })
                .collect::<String>()
        };
        let warning = if !needs_graph {
            Some(format!(
                "Corpus is ~{} words - fits in a single context window. You may not need a graph.",
                number(total_words)
            ))
        } else if total_words >= 500000 || total_files >= 500 {
            Some(format!(
                "Large corpus: {total_files} files · ~{} words. Semantic extraction will be expensive (many Claude tokens). Consider running on a subfolder.",
                number(total_words)
            ))
        } else {
            None
        };
        unclassified.sort();
        walk.ignored.sort();
        walk.noise.sort();
        Ok(
            json!({"files":files,"total_files":total_files,"total_words":total_words,"needs_graph":needs_graph,"warning":warning,"skipped_sensitive":walk.sensitive,"unclassified":unclassified,"walk_errors":walk.walk_errors,"ignored":walk.ignored,"pruned_noise_dirs":walk.noise,"ignore_patterns":rules.patterns.len(),"scan_root":fsutil::path_string(&root)}),
        )
    }
    pub fn detect_incremental(
        &mut self,
        root: &Path,
        manifest: &Path,
        kind: &str,
        options: &DetectOptions,
    ) -> Result<Value> {
        let full = self.detect(root, options)?;
        mf::incremental_from_full(full, &load_manifest(manifest, Some(root)), kind, Some(root))
    }
}
static CONTEXT: OnceLock<Mutex<Detector>> = OnceLock::new();
pub fn flush() {
    if let Some(context) = CONTEXT.get()
        && let Ok(mut c) = context.lock()
    {
        c.cache.flush();
    }
}
fn string_list(value: &Value) -> Vec<String> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect()
}
fn pattern_list(value: &Value) -> Result<Patterns> {
    value
        .as_array()
        .ok_or("Expected anchored patterns")?
        .iter()
        .map(|v| {
            Ok((
                PathBuf::from(v[0].as_str().ok_or("Expected pattern anchor")?),
                v[1].as_str().ok_or("Expected pattern text")?.into(),
            ))
        })
        .collect()
}
pub fn dispatch(r: &Value) -> Result<Value> {
    let mut context = CONTEXT
        .get_or_init(|| Mutex::new(Detector::default()))
        .lock()
        .map_err(crate::Error::from)?;
    if let Some(output) = r["output"].as_str() {
        context.output = output.into();
        context.cache.output = output.into();
    }
    if let Some(limits) = r.get("limits") {
        context.limits = serde_json::from_value(limits.clone()).map_err(crate::Error::from)?;
    }
    let p = |key: &str, default: &str| {
        PathBuf::from(r.get(key).and_then(Value::as_str).unwrap_or(default))
    };
    let path = p("path", ".");
    let root = p("root", ".");
    let default_manifest = paths::out_path_with(&context.output, &["manifest.json".into()]);
    let manifest = r["manifest_path"]
        .as_str()
        .map(PathBuf::from)
        .unwrap_or(default_manifest);
    let t = |key: &str| r[key].as_str().unwrap_or("");
    let opt_root = r["root"].as_str().map(Path::new);
    let method = t("function");
    let (result, rc_warnings) = crate::rcfile::capture_warnings(|| match method {
        "constants" => Ok(ty::CONSTANTS.clone()),
        "classify_file" => Ok(json!(classify_file(&path))),
        "_is_sensitive" => Ok(json!(is_sensitive(&path))),
        "_is_graphable_source" => Ok(json!(ty::is_graphable_source(&path))),
        "_looks_like_paper" => Ok(json!(ty::looks_like_paper(&path))),
        "_is_prose_note" => Ok(json!(ty::is_prose_note(&path))),
        "_generic_keyword_hit" => Ok(json!(ty::generic_keyword_hit(t("name")))),
        "_is_env_template" => Ok(json!(ty::is_env_template(t("name")))),
        "_split_env_s" => ty::split_env_s(t("value"), &string_list(&r["rest"])).map(|v| json!(v)),
        "_env_command_args" => ty::env_command_args(
            &string_list(&r["args"]),
            r["allow_split"].as_bool().unwrap_or(true),
        )
        .map(|v| json!(v)),
        "_shebang_interpreter" => Ok(json!(ty::shebang_interpreter(&path))),
        "_shebang_file_type" => Ok(json!(ty::shebang_file_type(&path))),
        "_file_within_size_cap" => Ok(json!(file_within_size_cap(
            &path,
            r["cap"].as_u64().unwrap_or(context.limits.raw_bytes)
        ))),
        "_zip_within_caps" => Ok(json!(zip_within_caps(&path, context.limits))),
        "extract_pdf_text" => Ok(json!(extract_pdf_text(&path, context.limits))),
        "docx_to_markdown" => Ok(json!(docx_to_markdown(&path, context.limits))),
        "xlsx_to_markdown" => Ok(json!(xlsx_to_markdown(&path, context.limits))),
        "xlsx_extract_structure" => Ok(xlsx_extract_structure(&path, context.limits)),
        "convert_office_file" => {
            convert_office_file(&path, &p("out_dir", "."), opt_root, context.limits)
                .map(|v| json!(v))
        }
        "count_words" => Ok(json!(count_words(&path, context.limits))),
        "_is_regular_file" => Ok(json!(path.is_file())),
        "_has_coverage_artifacts" => Ok(json!(ty::has_coverage_artifacts(&path))),
        "_has_build_output_markers" => Ok(json!(ty::has_build_output_markers(&path))),
        "_has_venv_markers" => Ok(json!(ty::has_venv_markers(&path))),
        "_is_noise_dir" => Ok(json!(ty::is_noise_dir(
            t("part"),
            r["parent"].as_str().map(Path::new)
        ))),
        "_parse_gitignore_line" => Ok(json!(ig::parse_gitignore_line(t("raw")))),
        "_find_vcs_root" => Ok(json!(ig::find_vcs_root(&path))),
        "_path_identity" => Ok(json!(ig::path_identity(&path))),
        "_git_tracked_path_keys" => {
            let (a, b) = ig::git_tracked_path_keys(&root);
            let mut a = a.into_iter().collect::<Vec<_>>();
            let mut b = b.into_iter().collect::<Vec<_>>();
            a.sort();
            b.sort();
            Ok(json!([a, b]))
        }
        "_git_info_exclude" => Ok(json!(ig::git_info_exclude(&root))),
        "_read_ignore_text" => context.warnings.read(&path).map(|v| json!(v)),
        "_load_dir_own_ignore" => ig::load_dir_own_ignore(
            &path,
            r["gitignore"].as_bool().unwrap_or(true),
            &mut context.warnings,
        )
        .map(|v| json!(v)),
        "_load_ignore_file" => ig::load_ignore_file(
            &root,
            r["gitignore"].as_bool().unwrap_or(true),
            &mut context.warnings,
        )
        .map(|v| json!(v)),
        "_parse_ignore_pattern" => Ok(json!(ig::parse_ignore_pattern(t("pattern")))),
        "_lexical_relative" => Ok(json!(ig::lexical_relative(&path, &p("anchor", ".")))),
        "_match_anchored_ignore_pattern" => Ok(json!(ig::match_anchored_ignore_pattern(
            t("value"),
            t("pattern")
        ))),
        "_wildcard" => Ok(json!(ig::wildcard(t("value"), t("pattern")))),
        "_is_ignored" => Ok(json!(ig::is_ignored(
            &path,
            &root,
            &pattern_list(&r["patterns"])?,
            &mut HashMap::new()
        ))),
        "predicate_new" => {
            let rules = IgnoreState::new(
                &root,
                &string_list(&r["extra_excludes"]),
                r["gitignore"].as_bool().unwrap_or(true),
                &mut context.warnings,
            )?;
            context.next_predicate += 1;
            let id = context.next_predicate;
            context.predicates.insert(id, rules);
            Ok(json!(id))
        }
        "predicate" => {
            let id = r["id"].as_u64().ok_or("Expected predicate id")?;
            let Detector {
                predicates,
                warnings,
                ..
            } = &mut *context;
            predicates
                .get_mut(&id)
                .ok_or("Unknown predicate")?
                .predicate(&path, warnings)
                .map(|v| json!(v))
        }
        "predicate_drop" => {
            context.predicates.remove(&r["id"].as_u64().unwrap_or(0));
            Ok(Value::Null)
        }
        "_auto_follow_symlinks" => Ok(json!(ig::auto_follow_symlinks(&root))),
        "_resolves_under_root" => Ok(json!(ig::resolves_under_root(&path, &root))),
        "_os_path" => Ok(json!(mf::os_path(&path))),
        "_md5_file" => Ok(json!(mf::md5_file(&path))),
        "_stat_and_hash" => Ok(json!(mf::stat_and_hash(t("path")))),
        "_nfc" => Ok(json!(paths::nfc(t("value")))),
        "_to_relative_for_storage" => Ok(json!(mf::to_relative_for_storage(t("key"), &root))),
        "_to_absolute_from_storage" => {
            mf::to_absolute_from_storage(t("key"), &root).map(|v| json!(v))
        }
        "load_manifest" => Ok(load_manifest(&manifest, opt_root)),
        "save_manifest" => save_manifest(
            r["files"].as_object().ok_or("Expected file groups")?,
            &manifest,
            r["kind"].as_str().unwrap_or("both"),
            &ManifestOptions {
                root: opt_root.map(Path::to_path_buf),
                scan_corpus: r
                    .get("scan_corpus")
                    .filter(|v| !v.is_null())
                    .map(string_list),
                clear_semantic: r
                    .get("clear_semantic")
                    .filter(|v| !v.is_null())
                    .map(string_list),
                clear_ast: r.get("clear_ast").filter(|v| !v.is_null()).map(string_list),
                ast_empty: r.get("ast_empty").filter(|v| !v.is_null()).map(string_list),
                now: r["now"].as_f64(),
            },
        )
        .map(|_| Value::Null),
        "_mtime_may_hide_a_rewrite" => Ok(json!(mf::mtime_may_hide_a_rewrite(
            r["current_mtime"].as_f64().ok_or("Expected mtime")?,
            &r["stored"]
        ))),
        "incremental_from_full" => mf::incremental_from_full(
            r["full"].clone(),
            &r["manifest"],
            r["kind"].as_str().unwrap_or("semantic"),
            opt_root,
        ),
        "detect" | "detect_incremental" => {
            let options = DetectOptions {
                follow_symlinks: r["follow_symlinks"].as_bool(),
                google_workspace: r["google_workspace"].as_bool(),
                extra_excludes: string_list(&r["extra_excludes"]),
                cache_root: r["cache_root"].as_str().map(PathBuf::from),
                gitignore: r["gitignore"].as_bool().unwrap_or(true),
            };
            if method == "detect" {
                context.detect(&root, &options)
            } else {
                crate::manifest::detect_incremental(
                    &mut context,
                    &root,
                    &manifest,
                    r["kind"].as_str().unwrap_or("semantic"),
                    &options,
                )
            }
        }
        "reset" => {
            context.cache.flush();
            context.cache = CacheSession(ExtractionCache::default());
            context.warnings = EncodingWarnings::default();
            context.limits = DocumentLimits::default();
            context.predicates.clear();
            Ok(Value::Null)
        }
        "flush" => {
            context.cache.flush();
            Ok(Value::Null)
        }
        "_git_global_excludes" => Ok(json!(ig::git_global_excludes(&root))),
        _ => Err(format!("Unknown discovery function: {method}")).map_err(crate::Error::from),
    });
    // The `.graphiclerc` warning is printed first, where activation happens.
    let mut warnings = rc_warnings;
    warnings.extend(std::mem::take(&mut context.warnings.messages));
    result.map(|value| json!({"value":value,"warnings":warnings}))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn repeated_discovery_reuses_word_counts_and_prunes_ignored_subtrees() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        fs::write(root.join(".graphicleignore"), "omitted/\n").unwrap();
        fs::create_dir_all(root.join("omitted/deep")).unwrap();
        fs::write(root.join("omitted/deep/secret.md"), "must not be counted").unwrap();
        fs::write(root.join("kept.md"), "one two three").unwrap();
        let mut detector = Detector::default();
        detector.cache.now_ns = Some(crate::cache::clock_ns() + 5_000_000_000);
        let first = detector.detect(root, &DetectOptions::default()).unwrap();
        assert_eq!(first["total_files"], 1);
        assert_eq!(first["total_words"], 3);
        assert_eq!(first["ignored"].as_array().unwrap().len(), 1);
        assert_eq!(detector.cache.computed_word_counts, 1);
        let second = detector.detect(root, &DetectOptions::default()).unwrap();
        assert_eq!(second["files"], first["files"]);
        assert_eq!(detector.cache.computed_word_counts, 1);
        fs::write(root.join("kept.md"), "one two three four five").unwrap();
        assert_eq!(
            detector.detect(root, &DetectOptions::default()).unwrap()["total_words"],
            5
        );
        assert_eq!(detector.cache.computed_word_counts, 2);
    }
}
