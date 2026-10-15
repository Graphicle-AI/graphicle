// Adapted from Graphify 0.9.91 (10130e79), Apache-2.0; see NOTICE.md.
// Upstream: graphify/__main__.py, graphify/watch.py
//! Path identities for incremental updates, including relocated graph roots.
use crate::{build, detect_ignore::absolute_lexical, extractors::resolution::resolved};
use serde_json::{Value, json};
use std::{
    cell::RefCell,
    collections::{BTreeSet, HashMap},
    path::{Path, PathBuf},
    sync::LazyLock,
};

pub fn text(v: &Value, key: &str) -> String {
    v[key].as_str().unwrap_or("").to_string()
}
pub fn items(v: &Value, key: &str) -> Vec<Value> {
    v[key].as_array().cloned().unwrap_or_default()
}
/// The array under `key`, borrowed (empty when absent or not an array): what
/// [`items`] copies, for readers that only look.
pub fn items_ref<'a>(v: &'a Value, key: &str) -> &'a [Value] {
    v[key].as_array().map_or(&[], Vec::as_slice)
}
pub fn norm(source: &str, root: Option<&Path>) -> String {
    build::norm_path(source, root)
}
static REMOTE_RE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"^[A-Za-z][A-Za-z0-9+.\-]+://?").unwrap());
pub fn remote(source: &str) -> bool {
    REMOTE_RE.is_match(source)
}
pub fn report_root_label(path: &Path) -> String {
    if path.is_absolute() {
        path.file_name()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string_lossy().into_owned())
    } else if path == Path::new(".") {
        std::env::current_dir()
            .ok()
            .and_then(|p| p.file_name().map(|s| s.to_string_lossy().into_owned()))
            .unwrap_or_default()
    } else {
        path.to_string_lossy().into_owned()
    }
}
pub fn changed_candidates(raw: &Path, change_root: &Path, watch_root: &Path) -> Vec<PathBuf> {
    let mut out = vec![];
    let bases = if raw.is_absolute() {
        vec![raw.to_path_buf()]
    } else {
        vec![change_root.join(raw), watch_root.join(raw)]
    };
    for p in bases {
        for p in [absolute_lexical(&p), resolved(&p)] {
            if !out.contains(&p) {
                out.push(p);
            }
        }
    }
    out
}
pub fn relativize(payload: &mut Value, root: &Path, scope: Option<&Path>) {
    for bucket in ["nodes", "edges", "hyperedges"] {
        if let Some(a) = payload.get_mut(bucket).and_then(Value::as_array_mut) {
            for item in a {
                for key in ["source_file", "definition_file"] {
                    let source = text(item, key);
                    let path = Path::new(&source);
                    if !path.is_absolute() {
                        continue;
                    }
                    let full = resolved(path);
                    if scope.is_some_and(|s| !full.starts_with(s)) {
                        continue;
                    }
                    if let Ok(rel) = full.strip_prefix(root) {
                        item[key] = json!(rel.to_string_lossy().replace('\\', "/"));
                    }
                }
            }
        }
    }
}
pub fn rebase(payload: &mut Value, source_root: &Path, target_root: &Path) {
    if source_root == target_root {
        return;
    }
    for bucket in ["nodes", "edges", "hyperedges"] {
        if let Some(a) = payload.get_mut(bucket).and_then(Value::as_array_mut) {
            for item in a {
                for key in ["source_file", "definition_file"] {
                    let source = text(item, key);
                    if source.is_empty() || Path::new(&source).is_absolute() {
                        continue;
                    }
                    if let Ok(rel) = source_root.join(source).strip_prefix(target_root) {
                        item[key] = json!(rel.to_string_lossy().replace('\\', "/"));
                    }
                }
            }
        }
    }
}
pub struct StoredSourcePaths {
    pub project_root: PathBuf,
    pub watch_root: PathBuf,
    pub existing_source_root: PathBuf,
    pub legacy_watch_relative: bool,
    /// Memo of `identity()` results: identities are pure functions of the
    /// stored source string once the roots are fixed, and a rebuild asks for
    /// the same handful of file identities once per node/edge.
    identity_cache: RefCell<HashMap<String, String>>,
    normalize_cache: RefCell<HashMap<String, String>>,
}
impl StoredSourcePaths {
    pub fn new(existing: &Value, out: &Path, project_root: &Path, watch_root: &Path) -> Self {
        let mut result = Self {
            project_root: project_root.into(),
            watch_root: watch_root.into(),
            existing_source_root: project_root.into(),
            legacy_watch_relative: false,
            identity_cache: RefCell::new(HashMap::new()),
            normalize_cache: RefCell::new(HashMap::new()),
        };
        let mut prefix = None;
        if let Ok(s) = std::fs::read_to_string(out.join(".graphicle_root")) {
            let path = PathBuf::from(s.trim_start_matches('\u{feff}').trim());
            if path.is_absolute() {
                let p = resolved(&path);
                result.existing_source_root = if anchors(existing, &p, 25) {
                    p
                } else {
                    [project_root.to_path_buf(), resolved(Path::new("."))]
                        .into_iter()
                        .find(|p| anchors(existing, p, 25))
                        .unwrap_or(p)
                };
            } else {
                let cwd = resolved(Path::new("."));
                if resolved(&cwd.join(&path)) == watch_root {
                    result.existing_source_root = cwd;
                    prefix = Some(crate::paths::normpath(&path.to_string_lossy()));
                }
            }
        }
        if let Some(prefix) = prefix.filter(|s| s != ".") {
            result.legacy_watch_relative =
                !["nodes", "links", "edges", "hyperedges"].iter().any(|b| {
                    items_ref(existing, b).iter().any(|item| {
                        let sf = norm(&text(item, "source_file"), None);
                        !sf.is_empty()
                            && !Path::new(&sf).is_absolute()
                            && (sf == prefix || sf.starts_with(&format!("{prefix}/")))
                    })
                });
        }
        result
    }
    pub fn normalize(&self, source: &str) -> String {
        if let Some(hit) = self.normalize_cache.borrow().get(source) {
            return hit.clone();
        }
        let s = norm(source, Some(&self.project_root));
        let s = if s.is_empty() {
            s
        } else {
            crate::paths::normpath(&s)
        };
        self.normalize_cache
            .borrow_mut()
            .insert(source.to_owned(), s.clone());
        s
    }
    pub fn absolute_identity(&self, source: &str, root: &Path) -> String {
        let n = norm(source, None);
        if n.is_empty() {
            return n;
        }
        let n = crate::paths::normpath(&n);
        let p = Path::new(&n);
        absolute_lexical(&if p.is_absolute() {
            p.to_path_buf()
        } else {
            root.join(p)
        })
        .to_string_lossy()
        .replace('\\', "/")
    }
    pub fn identity(&self, source: &str) -> String {
        if let Some(hit) = self.identity_cache.borrow().get(source) {
            return hit.clone();
        }
        let id = self.identity_uncached(source);
        self.identity_cache
            .borrow_mut()
            .insert(source.to_string(), id.clone());
        id
    }
    fn identity_uncached(&self, source: &str) -> String {
        let n = norm(source, None);
        let root = if !n.is_empty() && !Path::new(&n).is_absolute() && self.legacy_watch_relative {
            &self.watch_root
        } else {
            &self.existing_source_root
        };
        self.absolute_identity(&n, root)
    }
    pub fn in_watch(&self, source: &str) -> bool {
        let id = self.identity(source);
        !id.is_empty() && Path::new(&id).starts_with(&self.watch_root)
    }
    pub fn evicted(&self, item: &Value, ids: &BTreeSet<String>) -> bool {
        ids.contains(&self.identity(&text(item, "source_file")))
    }
    pub fn rebase_preserved(&self, item: &mut Value) {
        let sf = text(item, "source_file");
        let id = self.identity(&sf);
        if id.is_empty() {
            return;
        }
        let p = Path::new(&id);
        item["source_file"] = json!(if !p.starts_with(&self.watch_root) {
            self.normalize(&sf)
        } else {
            p.strip_prefix(&self.project_root)
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .unwrap_or(id)
        });
    }
}
fn anchors(existing: &Value, root: &Path, sample: usize) -> bool {
    let mut checked = 0;
    for bucket in ["nodes", "links", "edges", "hyperedges"] {
        for item in items_ref(existing, bucket) {
            let n = norm(&text(item, "source_file"), None);
            if n.is_empty() || crate::paths::is_absolute_any_platform(Some(&n)) {
                continue;
            }
            checked += 1;
            if root.join(crate::paths::normpath(&n)).exists() {
                return true;
            }
            if checked >= sample {
                return false;
            }
        }
    }
    checked == 0
}
