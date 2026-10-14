// Adapted from Graphify 0.9.88 (a9860f70), Apache-2.0; see NOTICE.md.
// Upstream: graphify/extract.py
//! Corpus-wide Python absolute-import ambiguity guard (upstream #3729).
//!
//! Ports `extract._python_absolute_import_alias_files` and
//! `extract._suppress_ambiguous_python_imports`. A file can be importable
//! under more than one absolute module name (`a/src/pkg/mod.py` as `pkg.mod`
//! or `src.pkg.mod`), and two scanned trees can answer to the same name. Such
//! imports stay dangling instead of binding to whichever file a resolver
//! happens to probe first.
use crate::{
    extractors::{
        base::make_id,
        engine_config::probe_python_module_candidate,
        resolution::{resolved, s},
        resolution_facts::AmbiguousModules,
    },
    unicode15::{IDENTIFIER_CONTINUE, IDENTIFIER_START},
};
use rustc_hash::{FxHashMap, FxHashSet};
use serde_json::{Value, json};
use sha1::{Digest, Sha1};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

/// Every scanned file an absolute module id resolves to.
pub type ModuleFiles = BTreeMap<String, BTreeSet<String>>;

fn in_ranges(ranges: &[(u32, u32)], c: char) -> bool {
    let i = ranges.partition_point(|&(lo, _)| lo <= c as u32);
    i > 0 && c as u32 <= ranges[i - 1].1
}

/// Python's `str.isidentifier()` over the pinned Unicode 15.0 tables.
pub fn is_identifier(text: &str) -> bool {
    let mut chars = text.chars();
    chars.next().is_some_and(|c| in_ranges(IDENTIFIER_START, c))
        && chars.all(|c| in_ranges(IDENTIFIER_CONTINUE, c))
}

/// Module ids mapped to the scanned `.py` files that answer to them, plus the
/// ids the scan root itself resolves (a unique root hit is authoritative, as
/// the shared resolver probes the scan root before importer ancestors).
pub fn absolute_import_alias_files(
    paths: &[PathBuf],
    root: &Path,
) -> (ModuleFiles, BTreeSet<String>) {
    let root = resolved(root);
    let mut packages: FxHashMap<PathBuf, bool> = FxHashMap::default();
    let mut aliases = ModuleFiles::new();
    let mut root_aliases = BTreeSet::new();
    // Aliases are sets, so a file listed twice contributes nothing new.
    let mut seen = FxHashSet::default();
    for raw in paths {
        // The shared resolver probes `.py` files, never a lone `.pyi`.
        if raw
            .extension()
            .is_none_or(|e| e.to_string_lossy().to_lowercase() != "py")
            || !seen.insert(raw.as_path())
        {
            continue;
        }
        let path = resolved(raw);
        if !path.starts_with(&root) {
            continue;
        }
        let module_path = if path.file_name().is_some_and(|n| n == "__init__.py") {
            path.parent().map(Path::to_path_buf).unwrap_or_default()
        } else {
            path.with_extension("")
        };
        let mut roots = vec![root.clone()];
        for ancestor in path.ancestors().skip(1) {
            if ancestor == root || !ancestor.starts_with(&root) {
                break;
            }
            let package = *packages
                .entry(ancestor.to_path_buf())
                .or_insert_with(|| ancestor.join("__init__.py").is_file());
            if !package {
                roots.push(ancestor.to_path_buf());
            }
        }
        for candidate_root in &roots {
            let Ok(relative) = module_path.strip_prefix(candidate_root) else {
                continue;
            };
            let parts = relative
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>();
            if parts.is_empty() || !parts.iter().all(|p| is_identifier(p)) {
                continue;
            }
            let candidate = parts
                .iter()
                .fold(candidate_root.clone(), |p, part| p.join(part));
            if probe_python_module_candidate(&candidate).is_none_or(|hit| resolved(&hit) != path) {
                continue;
            }
            let id = make_id([parts.join(".").as_str()]);
            aliases
                .entry(id.clone())
                .or_default()
                .insert(path.to_string_lossy().into_owned());
            if *candidate_root == root {
                root_aliases.insert(id);
            }
        }
    }
    (aliases, root_aliases)
}

/// The Python files an extraction run can bind imports to: its own paths plus
/// the unchanged files an incremental rebuild hands over as context.
pub fn scan_paths(paths: &[PathBuf], context_nodes: &[Value], root: &Path) -> Vec<PathBuf> {
    let mut out = paths.to_vec();
    // Many context nodes share a file: probe each distinct file once. The
    // consumer collects files into sets, so repeated entries never mattered.
    let mut seen = FxHashSet::default();
    for node in context_nodes {
        let source = s(node, "source_file");
        if source.is_empty()
            || !seen.insert(source)
            || Path::new(source)
                .extension()
                .is_none_or(|e| e.to_string_lossy().to_lowercase() != "py")
        {
            continue;
        }
        let path = Path::new(source);
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            root.join(path)
        };
        if path.is_file() {
            out.push(path);
        }
    }
    out
}

/// Module ids more than one file answers to, unless the scan root resolves
/// them itself.
pub fn ambiguous_modules(
    aliases: &ModuleFiles,
    root_aliases: &BTreeSet<String>,
) -> AmbiguousModules {
    aliases
        .iter()
        .filter(|(id, files)| files.len() > 1 && !root_aliases.contains(*id))
        .map(|(id, _)| id.clone())
        .collect()
}

fn take(edge: &mut Value, key: &str) -> Option<Value> {
    edge.as_object_mut().and_then(|m| m.shift_remove(key))
}

fn bindings(value: Option<Value>) -> Vec<(String, String)> {
    value
        .as_ref()
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|pair| {
            let pair = pair.as_array()?;
            Some((
                pair.first()?.as_str()?.to_string(),
                pair.get(1)?.as_str()?.to_string(),
            ))
        })
        .collect()
}

/// A bare `import helper` bound to a scanned sibling in the importer's own
/// non-package directory is unambiguous even when another loose directory
/// has its own `helper.py`.
fn loose_sibling(edge: &Value, module: &str, files: &ModuleFiles, root: &Path) -> bool {
    let (target, source) = (s(edge, "target_file"), s(edge, "source_file"));
    if target.is_empty() || source.is_empty() {
        return false;
    }
    let Some(candidates) = files.get(module).filter(|c| !c.is_empty()) else {
        return false;
    };
    let source = Path::new(source);
    let source = resolved(&if source.is_absolute() {
        source.to_path_buf()
    } else {
        root.join(source)
    });
    let target = resolved(Path::new(target));
    let (Some(dir), Some(target_dir)) = (source.parent(), target.parent()) else {
        return false;
    };
    target_dir == dir
        && !dir.join("__init__.py").is_file()
        && !dir.join("__init__.pyi").is_file()
        && candidates.iter().any(|c| resolved(Path::new(c)) == target)
}

/// Strip the transient `_python_import_*` keys from every import edge, drop
/// namespace-package markers, point ambiguous imports at a node-less id so
/// the graph builder drops them, and flag raw calls whose receiver or callee
/// is bound by an ambiguous import (`_ambiguous_python_import`).
pub fn suppress_ambiguous_imports(
    edges: &mut Vec<Value>,
    nodes: &[Value],
    per: &mut [Value],
    ambiguous: &AmbiguousModules,
    files: &ModuleFiles,
    root: &Path,
) {
    let node_ids = nodes.iter().map(|n| s(n, "id")).collect::<FxHashSet<_>>();
    let mut bound: FxHashMap<String, FxHashSet<String>> = FxHashMap::default();
    let mut star_files: FxHashSet<String> = FxHashSet::default();
    let mut kept = Vec::with_capacity(edges.len());
    for mut edge in std::mem::take(edges) {
        if !edge.is_object() {
            kept.push(edge);
            continue;
        }
        let module = take(&mut edge, "_python_import_module");
        let pairs = bindings(take(&mut edge, "_python_import_bindings"));
        let module_binding = take(&mut edge, "_python_import_module_binding");
        // Plain-import provenance for the external member-call arm (33edd89).
        if module_binding == Some(Value::Bool(true)) {
            edge["_python_plain_import"] = Value::Bool(true);
        }
        let module_binding = module_binding.is_some_and(|v| crate::build::truthy(&v));
        let marker =
            take(&mut edge, "_python_import_marker_only").is_some_and(|v| crate::build::truthy(&v));
        let Some(module) = module
            .filter(crate::build::truthy)
            .map(|m| crate::python_compat::string(&m))
        else {
            if !marker {
                kept.push(edge);
            }
            continue;
        };
        if !marker && !matches!(s(&edge, "relation"), "imports" | "imports_from") {
            kept.push(edge);
            continue;
        }
        let module_id = make_id([module.as_str()]);
        let source_file = s(&edge, "source_file").to_string();
        let ambiguous_module = ambiguous.contains(&module_id)
            && !(module_binding
                && !module.contains('.')
                && loose_sibling(&edge, &module_id, files, root));
        if module_binding {
            if ambiguous_module {
                bound
                    .entry(source_file.clone())
                    .or_default()
                    .extend(pairs.into_iter().map(|(_, local)| local));
            }
        } else {
            for (imported, local) in pairs {
                if imported == "*" {
                    let prefix = format!("{module_id}_");
                    if ambiguous_module || ambiguous.iter().any(|m| m.starts_with(&prefix)) {
                        star_files.insert(source_file.clone());
                    }
                    continue;
                }
                if ambiguous_module
                    || ambiguous.contains(&make_id([format!("{module}.{imported}").as_str()]))
                {
                    bound.entry(source_file.clone()).or_default().insert(local);
                }
            }
        }
        if marker {
            continue;
        }
        if ambiguous_module {
            // A raw dotted alias could belong to an unrelated real node: use a
            // stable node-less id so the builder drops the unresolved edge.
            let identity = [
                module.as_str(),
                source_file.as_str(),
                s(&edge, "source_location"),
                s(&edge, "source"),
            ]
            .join("\0");
            let digest = format!("{:x}", Sha1::digest(identity.as_bytes()));
            let mut target =
                make_id([format!("ambiguous_python_import_{}", &digest[..12]).as_str()]);
            while node_ids.contains(target.as_str()) {
                target.push('_');
            }
            edge["target"] = json!(target);
            take(&mut edge, "target_file");
        }
        kept.push(edge);
    }
    *edges = kept;
    if bound.is_empty() && star_files.is_empty() {
        return;
    }
    for record in per.iter_mut() {
        for call in record
            .get_mut("raw_calls")
            .and_then(Value::as_array_mut)
            .into_iter()
            .flatten()
        {
            let source_file = s(call, "source_file");
            // `str(raw_call.get(key, ""))`: a missing key reads as "".
            let text = |key: &str| {
                call.get(key)
                    .map(crate::python_compat::string)
                    .unwrap_or_default()
            };
            let flagged = star_files.contains(source_file)
                || bound.get(source_file).is_some_and(|names| {
                    let receiver = text("receiver");
                    let root_name = receiver.split('.').next().unwrap_or("");
                    names.contains(&text("callee")) || names.contains(root_name)
                });
            if flagged {
                call["_ambiguous_python_import"] = json!(true);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn identifiers_follow_python() {
        for (text, expected) in [
            ("pkg", true),
            ("_private", true),
            ("mod2", true),
            ("2mod", false),
            ("my-mod", false),
            ("", false),
            ("café", true),
            ("𝒂b", true),
            ("a b", false),
        ] {
            assert_eq!(is_identifier(text), expected, "{text}");
        }
    }
    #[test]
    fn nested_roots_make_a_module_name_ambiguous() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for tree in ["a/src/pkg", "b/src/pkg"] {
            std::fs::create_dir_all(root.join(tree)).unwrap();
            std::fs::write(root.join(tree).join("__init__.py"), "").unwrap();
            std::fs::write(root.join(tree).join("mod.py"), "").unwrap();
        }
        let paths = ["a/src/pkg/mod.py", "b/src/pkg/mod.py"]
            .map(|p| root.join(p))
            .to_vec();
        let (aliases, root_aliases) = absolute_import_alias_files(&paths, root);
        let ambiguous = ambiguous_modules(&aliases, &root_aliases);
        assert!(ambiguous.contains("pkg_mod"), "{aliases:?}");
        assert!(!ambiguous.contains("a_src_pkg_mod"));
    }
}
