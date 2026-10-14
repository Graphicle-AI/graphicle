//! Content-addressed, per-file resolution with a conservative dependency closure.
//!
//! Identifier uses watch every possible provider, including a definition added
//! after an unresolved call. Strongly connected components are hashed together;
//! callers depend on provider hashes, not on other callers of that provider.
//! Existing resolvers run on the union of the dirty files' forward closures.
//! Results are owned by source file, so unchanged callers can retain their
//! resolved records. Unsupported languages or unowned records use the full
//! resolver explicitly rather than risking an incomplete graph.
use crate::{
    Result, cache,
    extract::{self, ExtractOptions, ExtractReport},
    extractors,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

// v11: the private owner/shared keys below were renamed with the product.
const SCHEMA: &str = "resolution-files-v11";
const OWNER: &str = "__graphicle_private_resolution_owner";
const SHARED: &str = "__graphicle_private_shared_resolution_node";
const ARRAYS: [&str; 5] = [
    "nodes",
    "edges",
    "unresolved_calls",
    "failed_sources",
    "extracted_sources",
];

#[derive(Debug, Default, Serialize)]
pub struct Report {
    #[serde(flatten)]
    pub parsing: ExtractReport,
    pub components: usize,
    /// Files whose records were invalidated (includes affected dependents).
    pub dirty_files: usize,
    /// Inputs whose cached dependency description avoided loading parser records.
    pub skipped_parsing: usize,
    /// All files passed to the resolver, including unchanged context providers.
    pub resolved_files: usize,
    pub context_files: usize,
    /// Files whose previous resolved records were used in the final output.
    pub reused_files: usize,
    pub largest_component: usize,
    pub fallback: Option<String>,
}
#[derive(Serialize, Deserialize)]
struct Cached {
    key: String,
    #[serde(default)]
    root_scope: Option<String>,
    output: Value,
    output_sha256: String,
}

fn words(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|s| !s.is_empty())
        .map(str::to_lowercase)
}
fn strings(value: &Value, out: &mut BTreeSet<String>) {
    match value {
        Value::String(s) => out.extend(words(s)),
        Value::Array(a) => a.iter().for_each(|v| strings(v, out)),
        Value::Object(m) => {
            for (k, v) in m {
                if !matches!(
                    k.as_str(),
                    "source_file"
                        | "definition_file"
                        | "source_location"
                        | "file_type"
                        | "language"
                        | "confidence"
                        | "context"
                        | "type"
                        | "relation"
                ) {
                    strings(v, out);
                }
            }
        }
        _ => {}
    }
}
#[derive(Clone, Serialize, Deserialize)]
struct File {
    relative: String,
    origin_path: PathBuf,
    sha: String,
    uses: BTreeSet<String>,
    provides: BTreeSet<String>,
}
fn file(path: &Path, raw: &Value, root: &Path) -> Result<File> {
    let relative = cache::relative(&extractors::resolution::resolved(path), root)?;
    let source = std::fs::read(path)?;
    let text = String::from_utf8_lossy(&source);
    let mut counts = BTreeMap::<String, usize>::new();
    for word in words(&text) {
        *counts.entry(word).or_default() += 1;
    }
    let mut provides = BTreeSet::new();
    let mut collisions = BTreeSet::new();
    let mut stub_uses = BTreeSet::new();
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default();
    provides.extend(words(stem));
    // Package entry points also provide the directory name used by imports.
    if (matches!(stem, "__init__" | "index") || path.extension().is_some_and(|x| x == "go"))
        && let Some(parent) = path
            .parent()
            .and_then(Path::file_name)
            .and_then(|s| s.to_str())
    {
        provides.extend(words(parent));
    }
    for n in raw["nodes"].as_array().into_iter().flatten() {
        // An unresolved external stub is a use, not a provider. Treating
        // e.g. `typing.Any` stubs as declarations joins unrelated callers.
        if n["source_file"].as_str().is_none_or(str::is_empty) {
            for key in ["id", "label"] {
                if let Some(value) = n[key].as_str() {
                    stub_uses.extend(words(value));
                }
            }
            continue;
        }
        if let Some(id) = n["id"].as_str() {
            provides.insert(id.to_lowercase());
            collisions.insert(format!("id:{id}"));
        }
        let label = n["label"].as_str().unwrap_or_default();
        let file_label = Path::new(label).file_name() == path.file_name();
        if !file_label {
            for key in ["label", "qualified_name", "name"] {
                if let Some(v) = n[key].as_str() {
                    let normalized = v
                        .trim()
                        .trim_start_matches('.')
                        .trim_end_matches("()")
                        .to_lowercase();
                    if !normalized.is_empty() {
                        provides.insert(normalized);
                    }
                    // Qualified code names can be referenced by their member;
                    // prose headings cannot provide each word as a symbol.
                    if n["file_type"] == "code" {
                        provides.extend(words(v));
                    }
                }
            }
            // A declaration is not a reference to every other declaration of
            // that name. Remove only its own lexical occurrence. Calls,
            // annotations, strings and comments remain conservative watches.
            let name = label.trim_start_matches('.').trim_end_matches("()");
            if n["file_type"] == "code"
                && !name.is_empty()
                && name.chars().all(|c| c.is_alphanumeric() || c == '_')
                && let Some(n) = counts.get_mut(&name.to_lowercase())
            {
                *n = n.saturating_sub(1);
            }
        }
    }
    let mut uses = counts
        .into_iter()
        .filter(|(_, n)| *n > 0)
        .map(|(w, _)| w)
        .collect::<BTreeSet<_>>();
    if let Some(m) = raw.as_object() {
        for (key, value) in m {
            if key != "nodes" {
                strings(value, &mut uses);
            }
        }
    }
    // Identical raw IDs must see all competitors even without a call.
    uses.extend(stub_uses);
    uses.extend(collisions.iter().cloned());
    provides.extend(collisions);
    Ok(File {
        relative,
        origin_path: path.into(),
        sha: format!("{:x}", Sha256::digest(source)),
        uses,
        provides,
    })
}

fn configuration(paths: &[PathBuf], root: &Path) -> Result<Vec<(String, String)>> {
    let mut configs = BTreeSet::new();
    for path in paths {
        let mut parent = path.parent();
        while let Some(dir) = parent {
            if !dir.starts_with(root) {
                break;
            }
            for name in [
                "go.mod",
                "go.work",
                "tsconfig.json",
                "jsconfig.json",
                "package.json",
                "pyproject.toml",
            ] {
                configs.insert(dir.join(name));
            }
            if dir == root {
                break;
            }
            parent = dir.parent();
        }
    }
    configs
        .into_iter()
        .map(|p| {
            let hash = match std::fs::read(&p) {
                Ok(b) => format!("{:x}", Sha256::digest(b)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => "missing".into(),
                Err(e) => return Err(e.into()),
            };
            Ok((cache::relative(&p, root)?, hash))
        })
        .collect()
}
fn dependencies(files: &[File]) -> Vec<Vec<usize>> {
    let mut providers = hashbrown::HashMap::<&str, Vec<usize>>::new();
    for (i, f) in files.iter().enumerate() {
        for name in &f.provides {
            providers.entry(name).or_default().push(i);
        }
    }
    // Many identifiers resolve to the same provider file. A generation mark
    // replaces a tree insertion for each repeated candidate; sort only the
    // distinct neighbors, retaining the resolver's original tie-break order.
    let mut seen = vec![usize::MAX; files.len()];
    let mut edges = Vec::with_capacity(files.len());
    for (i, file) in files.iter().enumerate() {
        seen[i] = i;
        let mut adjacent = Vec::new();
        for name in &file.uses {
            if let Some(candidates) = providers.get(name.as_str()) {
                for &j in candidates {
                    if seen[j] != i {
                        seen[j] = i;
                        adjacent.push(j);
                    }
                }
            }
        }
        adjacent.sort_unstable();
        edges.push(adjacent);
    }
    edges
}
fn finish_order(edges: &[Vec<usize>]) -> Vec<usize> {
    let mut seen = vec![false; edges.len()];
    let mut out = Vec::with_capacity(edges.len());
    for start in 0..edges.len() {
        if seen[start] {
            continue;
        }
        seen[start] = true;
        let mut stack = vec![(start, 0)];
        while let Some((i, n)) = stack.last_mut() {
            if *n < edges[*i].len() {
                let j = edges[*i][*n];
                *n += 1;
                if !seen[j] {
                    seen[j] = true;
                    stack.push((j, 0));
                }
            } else {
                out.push(*i);
                stack.pop();
            }
        }
    }
    out
}
fn components(edges: &[Vec<usize>]) -> (Vec<Vec<usize>>, Vec<usize>) {
    let mut reverse = vec![vec![]; edges.len()];
    for (i, js) in edges.iter().enumerate() {
        for j in js {
            reverse[*j].push(i);
        }
    }
    let mut group_of = vec![usize::MAX; edges.len()];
    let mut groups = vec![];
    for start in finish_order(edges).into_iter().rev() {
        if group_of[start] != usize::MAX {
            continue;
        }
        let c = groups.len();
        let mut group = vec![];
        let mut stack = vec![start];
        group_of[start] = c;
        while let Some(i) = stack.pop() {
            group.push(i);
            for j in &reverse[i] {
                if group_of[*j] == usize::MAX {
                    group_of[*j] = c;
                    stack.push(*j);
                }
            }
        }
        group.sort_unstable();
        groups.push(group);
    }
    (groups, group_of)
}
fn keys(
    files: &[File],
    deps: &[Vec<usize>],
    groups: &[Vec<usize>],
    group_of: &[usize],
    salt: &Value,
) -> Result<Vec<String>> {
    let links = groups
        .iter()
        .enumerate()
        .map(|(c, g)| {
            g.iter()
                .flat_map(|i| deps[*i].iter().map(|j| group_of[*j]))
                .filter(|d| *d != c)
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect()
        })
        .collect::<Vec<Vec<_>>>();
    let mut hashes = vec![String::new(); groups.len()];
    for c in finish_order(&links) {
        // Ordering of competing providers is semantic for legacy tie-breaks.
        let mut dependency_groups = links[c].clone();
        dependency_groups.sort_by_key(|d| groups[*d][0]);
        hashes[c] = format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&json!({"salt":salt,
                "files":groups[c].iter().map(|i|(&files[*i].relative,&files[*i].sha)).collect::<Vec<_>>(),
                "dependencies":dependency_groups.iter().map(|d|&hashes[*d]).collect::<Vec<_>>()
            }))?)
        );
    }
    Ok(files
        .iter()
        .enumerate()
        .map(|(i, f)| {
            format!(
                "{:x}",
                Sha256::digest(format!("{}\0{}", hashes[group_of[i]], f.relative))
            )
        })
        .collect())
}
fn empty(receiver: bool) -> Value {
    let mut output = json!({"nodes":[],"edges":[],"input_tokens":0,"output_tokens":0,"failed_sources":[],"extracted_sources":[]});
    if receiver {
        output["unresolved_calls"] = json!([]);
    }
    output
}
fn order(value: &mut Value) {
    for key in ARRAYS {
        if let Some(a) = value.get_mut(key).and_then(Value::as_array_mut) {
            a.sort_by_cached_key(|v| crate::extract_normalize::canonical(v).to_string());
        }
    }
}
fn append(output: &mut Value, mut part: Value) {
    for key in ARRAYS {
        if let Some(a) = part.get_mut(key).and_then(Value::as_array_mut)
            && let Some(b) = output.get_mut(key).and_then(Value::as_array_mut)
        {
            b.append(a);
        }
    }
}
fn source_owner(source: &str, root: &Path, owners: &BTreeMap<&str, usize>) -> Option<usize> {
    if source.is_empty() {
        return None;
    }
    let path = Path::new(source);
    let relative = if path.is_absolute() {
        cache::relative(&extractors::resolution::resolved(path), root).ok()?
    } else {
        source.replace('\\', "/")
    };
    owners.get(relative.as_str()).copied()
}
/// Refuse partial reuse when a resolver produces a record without an
/// unambiguous originating file. Returning the full graph is always safe.
fn partition(
    output: &mut Value,
    files: &[File],
    root: &Path,
    receiver: bool,
) -> Option<Vec<Value>> {
    let owners = files
        .iter()
        .enumerate()
        .map(|(i, f)| (f.relative.as_str(), i))
        .collect::<BTreeMap<_, _>>();
    let mut nodes = BTreeMap::<String, BTreeSet<usize>>::new();
    for node in output["nodes"].as_array()? {
        if let Some(i) = node
            .get(OWNER)
            .and_then(Value::as_str)
            .and_then(|sf| owners.get(sf).copied())
            .or_else(|| {
                node["source_file"]
                    .as_str()
                    .and_then(|sf| source_owner(sf, root, &owners))
            })
        {
            nodes
                .entry(node["id"].as_str()?.into())
                .or_default()
                .insert(i);
        }
    }
    // Resolvers synthesize shared external type nodes (for example io.Reader)
    // without a source file. Their lifetime belongs to every referencing file,
    // so deleting one caller must not remove the node for another caller.
    let mut references = BTreeMap::<String, BTreeSet<usize>>::new();
    for edge in output["edges"].as_array()? {
        let owner = edge["source_file"]
            .as_str()
            .and_then(|sf| source_owner(sf, root, &owners))
            .or_else(|| {
                let ids = nodes.get(edge["source"].as_str()?)?;
                (ids.len() == 1).then(|| *ids.first().expect("one owner"))
            });
        if let Some(owner) = owner {
            for key in ["source", "target"] {
                if let Some(id) = edge[key].as_str() {
                    references.entry(id.into()).or_default().insert(owner);
                }
            }
        }
    }
    enum Assignment<'a> {
        One(usize),
        Shared(&'a BTreeSet<usize>),
    }
    let assignment = |key: &str, item: &Value| -> Option<Assignment<'_>> {
        let explicit = item
            .get(OWNER)
            .and_then(Value::as_str)
            .and_then(|sf| owners.get(sf).copied());
        if key == "nodes"
            && explicit.is_none()
            && item["source_file"].as_str().is_none_or(str::is_empty)
            && let Some(refs) = item["id"].as_str().and_then(|id| references.get(id))
        {
            return Some(Assignment::Shared(refs));
        }
        let owner = if matches!(key, "failed_sources" | "extracted_sources") {
            source_owner(item.as_str()?, root, &owners)
        } else {
            explicit
                .or_else(|| {
                    item["source_file"]
                        .as_str()
                        .and_then(|sf| source_owner(sf, root, &owners))
                })
                .or_else(|| {
                    if key != "edges" {
                        return None;
                    }
                    let ids = nodes.get(item["source"].as_str()?)?;
                    (ids.len() == 1).then(|| *ids.first().expect("one owner"))
                })
        }?;
        Some(Assignment::One(owner))
    };
    // Validate ownership before moving anything. A failed partition must leave
    // the completed full-corpus extraction available, without cloning it or
    // parsing and resolving the entire repository a second time.
    for key in ARRAYS {
        for item in output[key].as_array().into_iter().flatten() {
            if assignment(key, item).is_none() {
                if std::env::var_os("GRAPHICLE_PROFILE").is_some() {
                    eprintln!(
                        "graphicle_profile incremental_unowned kind={key} id={} source_file={}",
                        item["id"], item["source_file"]
                    );
                }
                return None;
            }
        }
    }
    let mut parts = (0..files.len())
        .map(|_| empty(receiver))
        .collect::<Vec<_>>();
    for key in ARRAYS {
        if let Some(items) = output.get_mut(key).and_then(Value::as_array_mut) {
            for mut item in std::mem::take(items) {
                let owner = assignment(key, &item)?;
                if let Some(object) = item.as_object_mut() {
                    object.shift_remove(OWNER);
                }
                match owner {
                    Assignment::One(i) => parts[i][key].as_array_mut()?.push(item),
                    Assignment::Shared(refs) => {
                        item[SHARED] = json!(true);
                        for &i in refs {
                            parts[i][key].as_array_mut()?.push(item.clone());
                        }
                    }
                }
            }
        }
    }
    Some(parts)
}
// Relocate only explicit source-list paths. Legacy dangling IDs and user
// documentation can also contain root-like strings; those must stay intact.
fn relocate(output: &mut Value, from: &Path, to: &Path) {
    for key in ["failed_sources", "extracted_sources"] {
        for value in output
            .get_mut(key)
            .and_then(Value::as_array_mut)
            .into_iter()
            .flatten()
        {
            if let Some(s) = value.as_str()
                && let Ok(relative) = Path::new(s).strip_prefix(from)
            {
                *value = json!(to.join(relative));
            }
        }
    }
}
// Both spellings matter on systems where /var is a symlink to /private/var.
// Legacy extractors may use the caller's path spelling inside dangling IDs.
fn root_spellings(root: &Path, source: Option<&Path>) -> Vec<PathBuf> {
    let mut roots = vec![root.to_path_buf()];
    if let Some(source) = source
        && let Ok(relative) = extractors::resolution::resolved(source).strip_prefix(root)
        && let Some(origin) = source.ancestors().nth(relative.components().count())
        && origin != root
    {
        roots.push(origin.to_path_buf());
    }
    roots
}
fn load(path: &Path, key: &str, root: &Path, source: Option<&Path>) -> Option<Value> {
    let mut c: Cached = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    let scope = serde_json::to_string(&root_spellings(root, source)).ok()?;
    if c.key != key
        || c.root_scope.as_deref().is_some_and(|saved| saved != scope)
        || format!("{:x}", Sha256::digest(serde_json::to_vec(&c.output).ok()?)) != c.output_sha256
    {
        return None;
    }
    relocate(&mut c.output, Path::new(cache::ROOT_MARKER), root);
    Some(c.output)
}
fn save(
    path: &Path,
    key: String,
    mut output: Value,
    root: &Path,
    source: Option<&Path>,
) -> Result<()> {
    // Some legacy unresolved link IDs embed the absolute root. They cannot
    // be relocated by rewriting user data or arbitrary IDs. Re-resolve just
    // these source records when the materialization root changes.
    let roots = root_spellings(root, source);
    fn sensitive(v: &Value, root: &str, prefix: &str) -> bool {
        match v {
            Value::String(s) => s.contains(root) || s.starts_with(prefix),
            Value::Array(a) => a.iter().any(|v| sensitive(v, root, prefix)),
            Value::Object(m) => m.values().any(|v| sensitive(v, root, prefix)),
            _ => false,
        }
    }
    let root_sensitive = roots.iter().any(|origin| {
        let root_text = origin.to_string_lossy();
        if root_text.is_empty() {
            return true;
        }
        let id_prefix = format!("{}_", extractors::base::make_id([root_text.as_ref()]));
        match &output {
            Value::Object(m) => m
                .iter()
                .filter(|(k, _)| !matches!(k.as_str(), "extracted_sources" | "failed_sources"))
                .any(|(_, v)| sensitive(v, &root_text, &id_prefix)),
            v => sensitive(v, &root_text, &id_prefix),
        }
    });
    let root_scope = root_sensitive
        .then(|| serde_json::to_string(&roots))
        .transpose()?;
    for origin in &roots {
        relocate(&mut output, origin, Path::new(cache::ROOT_MARKER));
    }
    let output_sha256 = format!("{:x}", Sha256::digest(serde_json::to_vec(&output)?));
    let mut tmp = tempfile::NamedTempFile::new_in(path.parent().unwrap_or(root))?;
    {
        use std::io::Write;
        // serde_json emits many tiny writes; a File alone makes each one a
        // syscall. Buffer each per-file record before the atomic rename.
        let mut writer = std::io::BufWriter::new(tmp.as_file_mut());
        serde_json::to_writer(
            &mut writer,
            &Cached {
                key,
                root_scope,
                output,
                output_sha256,
            },
        )?;
        writer.flush()?;
    }
    tmp.persist(path).map_err(|e| crate::Error::from(e.error))?;
    Ok(())
}
fn full(paths: &[PathBuf], options: &ExtractOptions, reason: &str) -> Result<(Value, Report)> {
    let (mut output, parsing) = extract::extract_with_report(paths, options)?;
    order(&mut output);
    Ok((
        output,
        Report {
            parsing,
            components: usize::from(!paths.is_empty()),
            dirty_files: paths.len(),
            resolved_files: paths.len(),
            largest_component: paths.len(),
            fallback: Some(reason.into()),
            ..Default::default()
        },
    ))
}
/// `engine` must change whenever resolver code changes. The service supplies
/// its executable digest. Cache reuse is optional and never changes the
/// default full-extraction/parity API.
pub fn extract(
    paths: &[PathBuf],
    options: &ExtractOptions,
    cache_dir: Option<&Path>,
    engine: &str,
) -> Result<(Value, Report)> {
    let input_paths = paths;
    let root = extractors::resolution::resolved(
        options
            .root
            .as_deref()
            .ok_or_else(|| crate::Error::from("Incremental extraction needs an explicit root"))?,
    );
    let paths = paths
        .iter()
        .map(|p| extractors::resolution::resolved(p))
        .collect::<Vec<_>>();
    let supported = paths.iter().all(|p| {
        extract::get_extractor(p).is_none()
            || matches!(
                p.file_name().and_then(|s| s.to_str()),
                Some("go.mod" | "go.work")
            )
            || matches!(
                p.extension().and_then(|s| s.to_str()),
                Some(
                    "py" | "pyi"
                        | "ts"
                        | "tsx"
                        | "mts"
                        | "cts"
                        | "js"
                        | "jsx"
                        | "mjs"
                        | "cjs"
                        | "go"
                        | "md"
                        | "txt"
                        | "rst"
                        | "json"
                        | "yml"
                        | "yaml"
                        | "toml"
                )
            )
    });
    if !supported
        || options.include_intermediate
        || !options.resolution_context_nodes.is_empty()
        || !options.resolution_context_edges.is_empty()
    {
        return full(
            input_paths,
            options,
            "language_or_external_context_requires_full_resolution",
        );
    }
    if paths.iter().any(|p| !p.is_file() || !p.starts_with(&root))
        || paths.iter().collect::<BTreeSet<_>>().len() != paths.len()
    {
        return full(
            input_paths,
            options,
            "missing_outside_or_duplicate_source_requires_full_resolution",
        );
    }
    let _sources = extractors::parse_cache::RunScope::begin();
    extractors::resolution_js::clear();
    extractors::markdown::clear_link_index_cache();
    let salt = json!({"schema":SCHEMA,"engine":engine,"receiver":options.receiver_resolution,"tests":options.test_identities,"configuration":configuration(&paths,&root)?});
    let index_key = format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&json!({"salt":salt,"root":root}))?)
    );
    let mut previous = cache_dir
        .and_then(|dir| load(&dir.join("index.json"), &index_key, &root, None))
        .and_then(|value| serde_json::from_value::<Vec<File>>(value).ok())
        .map(|files| {
            files
                .into_iter()
                .map(|f| (f.relative.clone(), f))
                .collect::<BTreeMap<_, _>>()
        })
        .unwrap_or_default();
    let mut files = Vec::with_capacity(paths.len());
    let mut changed = Vec::new();
    for (i, path) in paths.iter().enumerate() {
        let relative = cache::relative(path, &root)?;
        let sha = format!("{:x}", Sha256::digest(std::fs::read(path)?));
        let description = previous
            .remove(&relative)
            .filter(|f| f.sha == sha && f.origin_path == input_paths[i]);
        if description.is_none() {
            changed.push(i);
        }
        files.push(description);
    }
    let index_changed = !changed.is_empty() || !previous.is_empty();
    // A new resolver binary gets its own parser cache too. Otherwise cached
    // raw records from an older extractor could defeat source invalidation.
    let mut parse_options = options.clone();
    if let Some(dir) = cache_dir {
        parse_options.cache_root = Some(
            dir.join("inputs")
                .join(format!("{:x}", Sha256::digest(engine.as_bytes()))),
        );
    }
    let mut per = vec![Value::Null; paths.len()];
    let mut loaded = vec![false; paths.len()];
    let mut parsing = ExtractReport::default();
    if !changed.is_empty() {
        let selected = changed
            .iter()
            .map(|i| input_paths[*i].clone())
            .collect::<Vec<_>>();
        let (records, first) = extract::records_with_report(&selected, &parse_options, &paths)?;
        parsing = first;
        for (i, record) in changed.into_iter().zip(records) {
            files[i] = Some(file(&input_paths[i], &record, &root)?);
            per[i] = record;
            loaded[i] = true;
        }
    }
    let files = files
        .into_iter()
        .map(|f| f.expect("cached or parsed file description"))
        .collect::<Vec<_>>();
    let deps = dependencies(&files);
    let (groups, group_of) = components(&deps);
    let keys = keys(&files, &deps, &groups, &group_of, &salt)?;
    if let Some(dir) = cache_dir {
        std::fs::create_dir_all(dir)?;
    }
    let mut parts = keys
        .iter()
        .enumerate()
        .map(|(i, key)| {
            cache_dir.and_then(|dir| {
                load(
                    &dir.join(format!("{key}.json")),
                    key,
                    &root,
                    Some(&input_paths[i]),
                )
            })
        })
        .collect::<Vec<_>>();
    let dirty = parts
        .iter()
        .enumerate()
        .filter(|(_, p)| p.is_none())
        .map(|(i, _)| i)
        .collect::<Vec<_>>();
    let mut report = Report {
        parsing,
        components: groups.len(),
        dirty_files: dirty.len(),
        reused_files: paths.len() - dirty.len(),
        largest_component: groups.iter().map(Vec::len).max().unwrap_or(0),
        ..Default::default()
    };
    if !dirty.is_empty() {
        let mut include = vec![false; paths.len()];
        let mut stack = dirty.clone();
        while let Some(i) = stack.pop() {
            if !include[i] {
                include[i] = true;
                stack.extend(deps[i].iter().copied());
            }
        }
        let context = include
            .iter()
            .enumerate()
            .filter(|(_, yes)| **yes)
            .map(|(i, _)| i)
            .collect::<Vec<_>>();
        report.resolved_files = context.len();
        report.context_files = context.len() - dirty.len();
        let missing = context
            .iter()
            .copied()
            .filter(|i| !loaded[*i])
            .collect::<Vec<_>>();
        if !missing.is_empty() {
            let selected = missing
                .iter()
                .map(|i| input_paths[*i].clone())
                .collect::<Vec<_>>();
            let (records, next) = extract::records_with_report(&selected, &parse_options, &paths)?;
            report.parsing.changed.extend(next.changed);
            report.parsing.unchanged += next.unchanged;
            report.parsing.unsupported += next.unsupported;
            for (i, record) in missing.into_iter().zip(records) {
                per[i] = record;
                loaded[i] = true;
            }
        }
        let selected = context
            .iter()
            .map(|i| input_paths[*i].clone())
            .collect::<Vec<_>>();
        let owners = files
            .iter()
            .enumerate()
            .map(|(i, file)| (file.relative.as_str(), i))
            .collect::<BTreeMap<_, _>>();
        for &i in &context {
            for node in per[i]["nodes"].as_array_mut().into_iter().flatten() {
                if node.get(OWNER).is_some() || node.get(SHARED).is_some() {
                    return full(
                        input_paths,
                        options,
                        "reserved_resolution_field_requires_full_resolution",
                    );
                }
                // Import rescue can emit a placeholder whose source_file is
                // a missing path or external package. Its originating parser
                // owns this occurrence; the referenced path owns no records.
                if node["source_file"]
                    .as_str()
                    .and_then(|source| source_owner(source, &root, &owners))
                    .is_none()
                {
                    node[OWNER] = json!(files[i].relative);
                }
            }
        }
        let raw = context
            .iter()
            .map(|i| std::mem::take(&mut per[*i]))
            .collect();
        let mut completed = extract::assemble_with_corpus(&selected, raw, &root, options, &paths)?;
        let Some(mut resolved) =
            partition(&mut completed, &files, &root, options.receiver_resolution)
        else {
            // All inputs were already resolved, so this is the full fallback
            // result. Only a genuinely partial pass needs a second extraction.
            if context.len() == paths.len() {
                for key in ARRAYS {
                    for item in completed
                        .get_mut(key)
                        .and_then(Value::as_array_mut)
                        .into_iter()
                        .flatten()
                    {
                        if let Some(object) = item.as_object_mut() {
                            object.shift_remove(OWNER);
                        }
                    }
                }
                order(&mut completed);
                report.fallback = Some("unowned_resolver_records_require_full_resolution".into());
                report.dirty_files = paths.len();
                report.reused_files = 0;
                report.context_files = 0;
                return Ok((completed, report));
            }
            // These partial records cannot safely be assigned to a source.
            return full(
                input_paths,
                options,
                "unowned_resolver_records_require_full_resolution",
            );
        };
        for i in dirty {
            let part = std::mem::take(&mut resolved[i]);
            if let Some(dir) = cache_dir {
                save(
                    &dir.join(format!("{}.json", keys[i])),
                    keys[i].clone(),
                    part.clone(),
                    &root,
                    Some(&input_paths[i]),
                )?;
            }
            parts[i] = Some(part);
        }
    }
    report.skipped_parsing = loaded.iter().filter(|yes| !**yes).count();
    if index_changed && let Some(dir) = cache_dir {
        save(
            &dir.join("index.json"),
            index_key,
            serde_json::to_value(&files)?,
            &root,
            None,
        )?;
    }
    let mut output = empty(options.receiver_resolution);
    for part in parts {
        append(&mut output, part.expect("every dirty file was resolved"));
    }
    if let Some(nodes) = output["nodes"].as_array_mut() {
        let mut shared = BTreeSet::new();
        nodes.retain_mut(|node| {
            if node
                .as_object_mut()
                .and_then(|m| m.shift_remove(SHARED))
                .is_some()
            {
                shared.insert(crate::extract_normalize::canonical(node).to_string())
            } else {
                true
            }
        });
    }
    if let Some(dir) = cache_dir {
        let live = keys
            .iter()
            .map(|key| format!("{key}.json"))
            .collect::<BTreeSet<_>>();
        for item in std::fs::read_dir(dir)? {
            let item = item?;
            let name = item.file_name().to_string_lossy().into_owned();
            if name.len() == 69
                && name.ends_with(".json")
                && name[..64].bytes().all(|b| b.is_ascii_hexdigit())
                && !live.contains(&name)
            {
                let _ = std::fs::remove_file(item.path());
            }
        }
    }
    output["extracted_sources"] = json!(input_paths);
    for value in output["failed_sources"]
        .as_array_mut()
        .into_iter()
        .flatten()
    {
        if let Some(source) = value.as_str()
            && let Some(i) = paths.iter().position(|p| p == Path::new(source))
        {
            *value = json!(input_paths[i]);
        }
    }
    order(&mut output);
    Ok((output, report))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dependency_candidates_preserve_all_providers_and_sorted_ties() {
        for count in [0, 1, 3, 64, 257] {
            let files: Vec<_> = (0..count)
                .map(|i| File {
                    relative: format!("{i}.py"),
                    origin_path: format!("/fixture/{i}.py").into(),
                    sha: String::new(),
                    provides: (0..19)
                        .filter(|n| (i + n) % 3 == 0)
                        .map(|n| format!("name{n}"))
                        .collect(),
                    uses: (0..23)
                        .filter(|n| (i * 7 + n) % 5 == 0)
                        .map(|n| format!("name{n}"))
                        .collect(),
                })
                .collect();
            // Independent all-pairs intersection: no lookup table, generation
            // marks or duplicate insertion semantics shared with production.
            let expected: Vec<Vec<usize>> = files
                .iter()
                .enumerate()
                .map(|(i, file)| {
                    files
                        .iter()
                        .enumerate()
                        .filter(|(j, provider)| {
                            i != *j && !file.uses.is_disjoint(&provider.provides)
                        })
                        .map(|(j, _)| j)
                        .collect()
                })
                .collect();
            assert_eq!(dependencies(&files), expected);
        }
    }

    #[test]
    fn unowned_records_leave_the_completed_graph_intact() {
        let files = [File {
            relative: "a.py".into(),
            origin_path: PathBuf::from("/fixture/a.py"),
            sha: "unused".into(),
            uses: BTreeSet::new(),
            provides: BTreeSet::new(),
        }];
        let mut output = json!({
            "nodes": [{"id":"a", "source_file":"a.py", (OWNER):"a.py"}],
            "edges": [{"source":"unresolved", "target":"missing", "relation":"CALLS"}],
            "extracted_sources": ["a.py"], "failed_sources": []
        });
        let completed = output.clone();
        assert!(partition(&mut output, &files, Path::new("/fixture"), true).is_none());
        assert_eq!(
            output, completed,
            "a full fallback must retain all completed records"
        );
    }
    #[test]
    fn missing_import_placeholders_follow_their_originating_files() {
        let _serial = extractors::parse_cache::test_serial();
        let root = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let options = ExtractOptions {
            root: Some(root.path().into()),
            parallel: false,
            use_cache: true,
            receiver_resolution: true,
            ..Default::default()
        };
        let source = "export async function load() { return import('./missing.js'); }\n";
        for name in ["a.js", "b.js"] {
            std::fs::write(root.path().join(name), source).unwrap();
        }
        std::fs::write(
            root.path().join("isolate.py"),
            "def orbit_unique(): return 7\n",
        )
        .unwrap();
        let mut names = vec!["a.js", "b.js", "isolate.py"];
        for step in 0..8 {
            match step {
                2 => {
                    names.retain(|name| *name != "a.js");
                    std::fs::remove_file(root.path().join("a.js")).unwrap();
                }
                3 => {
                    names.push("missing.js");
                    std::fs::write(root.path().join("missing.js"), "export const value = 1;\n")
                        .unwrap();
                }
                4 => {
                    names.retain(|name| *name != "missing.js");
                    std::fs::remove_file(root.path().join("missing.js")).unwrap();
                }
                5 => {
                    names.push("a.js");
                    std::fs::write(root.path().join("a.js"), source).unwrap();
                }
                6 => {
                    names.retain(|name| *name == "isolate.py");
                    for name in ["a.js", "b.js"] {
                        std::fs::remove_file(root.path().join(name)).unwrap();
                    }
                }
                _ => {}
            }
            let paths = names
                .iter()
                .map(|name| root.path().join(name))
                .collect::<Vec<_>>();
            let (actual, report) =
                extract(&paths, &options, Some(cache.path()), "missing-imports").unwrap();
            let mut expected = extract::extract(&paths, &options).unwrap();
            order(&mut expected);
            assert_eq!(actual, expected, "step {step}");
            assert!(report.fallback.is_none(), "step {step}: {report:?}");
            assert_eq!(
                actual["nodes"].as_array().unwrap().iter().any(|node| {
                    node["source_file"]
                        .as_str()
                        .is_some_and(|path| path.ends_with("missing.js"))
                }),
                step < 6
            );
            if step == 1 || step == 7 {
                assert_eq!(report.resolved_files, 0, "{report:?}");
            }
        }
    }

    #[test]
    fn shared_external_go_types_live_until_the_last_referencing_file_is_removed() {
        let _serial = extractors::parse_cache::test_serial();
        let root = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let a = root.path().join("a.go");
        let b = root.path().join("b.go");
        std::fs::write(
            &a,
            "package widget\nimport \"io\"\ntype A struct { Reader io.Reader }\n",
        )
        .unwrap();
        std::fs::write(
            &b,
            "package widget\nimport \"io\"\ntype B struct { Reader io.Reader }\n",
        )
        .unwrap();
        let options = ExtractOptions {
            root: Some(root.path().into()),
            parallel: false,
            use_cache: false,
            receiver_resolution: true,
            ..Default::default()
        };
        let mut paths = vec![a.clone(), b.clone()];
        for step in 0..4 {
            let (actual, report) =
                extract(&paths, &options, Some(cache.path()), "shared-go").unwrap();
            let mut expected = extract::extract(&paths, &options).unwrap();
            order(&mut expected);
            assert_eq!(actual, expected, "step {step}");
            assert!(report.fallback.is_none(), "{:?}", report.fallback);
            assert_eq!(
                actual["nodes"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|n| n["id"] == "go_type_io_reader")
                    .count(),
                usize::from(step < 3)
            );
            if step == 1 {
                std::fs::remove_file(&a).unwrap();
                paths.remove(0);
            }
            if step == 2 {
                std::fs::remove_file(&b).unwrap();
                paths.clear();
            }
        }
    }
    #[test]
    fn dangling_document_ids_and_literal_root_text_relocate_without_rewriting_data() {
        let _serial = extractors::parse_cache::test_serial();
        let root = tempfile::tempdir().unwrap();
        let moved = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let source = format!(
            "# Reference\n[missing](advanced/missing.md)\n\n## Literal {}\n",
            root.path().display()
        );
        for dir in [root.path(), moved.path()] {
            std::fs::write(dir.join("README.md"), &source).unwrap();
            std::fs::write(dir.join("a.py"), "def orbit_unique(): return 1\n").unwrap();
        }
        for dir in [root.path(), moved.path()] {
            let options = ExtractOptions {
                root: Some(dir.into()),
                use_cache: false,
                parallel: false,
                ..Default::default()
            };
            let paths = vec![dir.join("README.md"), dir.join("a.py")];
            let (actual, _) =
                extract(&paths, &options, Some(cache.path()), "root-literal").unwrap();
            let mut expected = extract::extract(&paths, &options).unwrap();
            order(&mut expected);
            assert_eq!(actual, expected);
            assert!(
                actual
                    .to_string()
                    .contains(&root.path().to_string_lossy().to_string())
            );
        }
    }
    #[test]
    fn editing_one_caller_reuses_other_callers_of_a_shared_provider() {
        let _serial = extractors::parse_cache::test_serial();
        let root = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let options = ExtractOptions {
            root: Some(root.path().into()),
            parallel: false,
            use_cache: false,
            receiver_resolution: true,
            ..Default::default()
        };
        let mut paths = vec![root.path().join("api.py")];
        std::fs::write(&paths[0], "def send():\n    return 1\n").unwrap();
        for i in 0..32 {
            let p = root.path().join(format!("caller{i}.py"));
            std::fs::write(&p, "from api import send\ndef run():\n    return send()\n").unwrap();
            paths.push(p);
        }
        extract(&paths, &options, Some(cache.path()), "fan-out").unwrap();
        std::fs::write(
            &paths[1],
            "from api import send\ndef run():\n    return send() + 2\n",
        )
        .unwrap();
        let (actual, report) = extract(&paths, &options, Some(cache.path()), "fan-out").unwrap();
        let mut expected = extract::extract(&paths, &options).unwrap();
        order(&mut expected);
        assert_eq!(actual, expected);
        assert_eq!(report.dirty_files, 1, "{report:?}");
        assert_eq!(report.resolved_files, 2, "{report:?}");
        assert_eq!(report.reused_files, 32, "{report:?}");
        std::fs::write(&paths[0], "def send():\n    return 3\n").unwrap();
        let (actual, report) = extract(&paths, &options, Some(cache.path()), "fan-out").unwrap();
        let mut expected = extract::extract(&paths, &options).unwrap();
        order(&mut expected);
        assert_eq!(actual, expected);
        assert_eq!(report.dirty_files, 33, "{report:?}");
    }
    #[test]
    fn additions_deletions_unresolved_names_and_root_relocation_match_full() {
        let _serial = extractors::parse_cache::test_serial();
        let root = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let options = ExtractOptions {
            root: Some(root.path().into()),
            use_cache: false,
            parallel: false,
            receiver_resolution: true,
            ..Default::default()
        };
        let mut names = vec!["caller.py", "independent.py"];
        std::fs::write(
            root.path().join("caller.py"),
            "def caller():\n    return missing()\n",
        )
        .unwrap();
        std::fs::write(
            root.path().join("independent.py"),
            "def separate():\n    return 7\n",
        )
        .unwrap();
        for step in 0..6 {
            match step {
                1 => {
                    names.push("provider.py");
                    std::fs::write(
                        root.path().join("provider.py"),
                        "def missing():\n    return 1\n",
                    )
                    .unwrap();
                }
                2 => {
                    std::fs::write(
                        root.path().join("provider.py"),
                        "def renamed():\n    return 2\n",
                    )
                    .unwrap();
                }
                3 => {
                    names.retain(|n| *n != "provider.py");
                    std::fs::remove_file(root.path().join("provider.py")).unwrap();
                }
                _ => {}
            }
            let paths = names
                .iter()
                .map(|n| root.path().join(n))
                .collect::<Vec<_>>();
            let (actual, report) = extract(&paths, &options, Some(cache.path()), "test").unwrap();
            let mut expected = extract::extract(&paths, &options).unwrap();
            order(&mut expected);
            assert_eq!(actual, expected, "step {step}");
            if step > 0 {
                assert!(report.reused_files >= 1, "{report:?}");
            }
            if step == 4 {
                assert_eq!(report.resolved_files, 0);
            }
        }
        let moved = tempfile::tempdir().unwrap();
        for name in &names {
            std::fs::copy(root.path().join(name), moved.path().join(name)).unwrap();
        }
        let paths = names
            .iter()
            .map(|n| moved.path().join(n))
            .collect::<Vec<_>>();
        let options = ExtractOptions {
            root: Some(moved.path().into()),
            ..options
        };
        let (actual, report) = extract(&paths, &options, Some(cache.path()), "test").unwrap();
        let mut expected = extract::extract(&paths, &options).unwrap();
        order(&mut expected);
        assert_eq!(actual, expected);
        assert_eq!(report.resolved_files, 0);
    }
    #[test]
    fn multi_language_edits_and_config_changes_match_full() {
        let _serial = extractors::parse_cache::test_serial();
        let cases: Vec<Vec<(&str, &str)>> = vec![
            vec![
                (
                    "pkg/base.py",
                    "class Sender:\n    def send(self):\n        return 1\n",
                ),
                ("pkg/forward.py", "from .base import Sender as Forwarded\n"),
                (
                    "app.py",
                    "from pkg.forward import Forwarded\ndef run():\n    return Forwarded().send()\n",
                ),
                ("isolate.py", "def orbit_unique():\n    return 9\n"),
                (
                    "README.md",
                    "# API\nUse [Sender](pkg/base.py) for sending.\n",
                ),
            ],
            vec![
                (
                    "src/base.ts",
                    "export class Sender { send():number { return 1; } }\n",
                ),
                (
                    "src/index.ts",
                    "export { Sender as Forwarded } from './base';\n",
                ),
                (
                    "app.ts",
                    "import { Forwarded } from './src';\nexport function run() { return new Forwarded().send(); }\n",
                ),
                (
                    "isolate.ts",
                    "export function orbit_unique() { return 9; }\n",
                ),
                ("README.md", "# API\nUse [Sender](src/base.ts).\n"),
            ],
            vec![
                ("go.mod", "module example.test/fixture\n\ngo 1.23\n"),
                (
                    "sender/base.go",
                    "package sender\ntype Sender struct {}\nfunc (s Sender) Send() int { return 1 }\n",
                ),
                (
                    "app.go",
                    "package fixture\nimport \"example.test/fixture/sender\"\nfunc Run() int { s := sender.Sender{}; return s.Send() }\n",
                ),
                (
                    "orbit/isolate.go",
                    "package orbit\nfunc Unique() int { return 9 }\n",
                ),
                ("README.md", "# API\nUse [Sender](sender/base.go).\n"),
            ],
        ];
        for entries in cases {
            let root = tempfile::tempdir().unwrap();
            let cache = tempfile::tempdir().unwrap();
            let canonical = root.path().canonicalize().unwrap();
            let options = ExtractOptions {
                root: Some(canonical.clone()),
                use_cache: false,
                parallel: false,
                receiver_resolution: true,
                test_identities: true,
                ..Default::default()
            };
            let mut paths = vec![];
            for (name, content) in &entries {
                let p = canonical.join(name);
                std::fs::create_dir_all(p.parent().unwrap()).unwrap();
                std::fs::write(&p, content).unwrap();
                paths.push(p);
            }
            for step in 0..6 {
                if step == 1 {
                    let p = paths
                        .iter()
                        .find(|p| p.file_stem().unwrap() == "base")
                        .unwrap();
                    let source = std::fs::read_to_string(p)
                        .unwrap()
                        .replace("return 1", "return 2");
                    std::fs::write(p, source).unwrap();
                } else if step == 2 {
                    let p = paths
                        .iter()
                        .find(|p| p.file_stem().unwrap() == "isolate")
                        .unwrap();
                    std::fs::write(p, std::fs::read_to_string(p).unwrap().replace("9", "8"))
                        .unwrap();
                } else if step == 3 {
                    let p = canonical.join("tsconfig.json");
                    std::fs::write(
                        &p,
                        r#"{"compilerOptions":{"baseUrl":".","paths":{"@src/*":["src/*"]}}}"#,
                    )
                    .unwrap();
                    paths.push(p);
                } else if step == 4 {
                    // A syntactically valid, modified cache entry is still a miss.
                    for entry in std::fs::read_dir(cache.path()).unwrap() {
                        let path = entry.unwrap().path();
                        let mut value: Value =
                            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
                        if value["output"].is_array() {
                            value["output"][0]["provides"] = json!(["corrupt_dependency"]);
                        } else {
                            value["output"]["nodes"] = json!([]);
                        }
                        std::fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
                    }
                }
                let (actual, report) =
                    extract(&paths, &options, Some(cache.path()), "multi-test").unwrap();
                let mut full = extract::extract(&paths, &options).unwrap();
                order(&mut full);
                assert_eq!(actual, full, "{} step {step} {report:?}", entries[0].0);
                assert!(report.fallback.is_none(), "{report:?}");
                if step == 5 {
                    assert_eq!(report.resolved_files, 0, "{report:?}");
                }
            }
        }
    }
}
