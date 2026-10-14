//! Opt-in (`--precise`) Dart import resolution, which the upstream port lacks
//! (upstream issue #2329: Dart `import`/`export` edges end at a label-only
//! node named after the URI, so reverse traversal such as `affected` never
//! reaches the importing file).
//!
//! A relative URI resolves against the importing file's directory, and
//! `package:<name>/<path>` against `lib/` of the nearest `pubspec.yaml`
//! declaring `name: <name>` (the importing package or any package with Dart
//! files in the corpus). `dart:` URIs and anything that does not name a file
//! node of the graph stay as they are. Resolved edges point at the file node
//! and carry `resolution: dart_import`; URI nodes left without edges are
//! removed. The default extraction never runs this module.
use super::resolution::source_key;
use rustc_hash::{FxHashMap, FxHashSet};
use serde_json::{Value, json};
use std::path::{Component, Path, PathBuf};

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v[k].as_str().unwrap_or("")
}
fn absolute(sf: &str, root: &Path) -> PathBuf {
    let p = Path::new(sf);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        root.join(p)
    }
}
/// Lexical `..`/`.` normalisation (import targets need not exist on disk).
fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}
/// `name:` of a pubspec, if it declares one at top level.
fn pubspec_name(dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(dir.join("pubspec.yaml")).ok()?;
    text.lines().find_map(|l| {
        let rest = l.strip_prefix("name:")?;
        let name = rest.split('#').next()?.trim().trim_matches(['"', '\'']);
        (!name.is_empty()).then(|| name.to_string())
    })
}

pub fn resolve(nodes: &mut Vec<Value>, edges: &mut [Value], context_nodes: &[Value], root: &Path) {
    // Dart file nodes by root-relative key, and each file's absolute path.
    let mut file_by_key: FxHashMap<String, String> = FxHashMap::default();
    let mut path_of: FxHashMap<String, PathBuf> = FxHashMap::default();
    for n in nodes.iter().chain(context_nodes) {
        let (sf, label) = (s(n, "source_file"), s(n, "label"));
        if sf.ends_with(".dart") && Path::new(sf).file_name() == Path::new(label).file_name() {
            file_by_key
                .entry(source_key(sf, root))
                .or_insert_with(|| s(n, "id").to_string());
            path_of
                .entry(s(n, "id").to_string())
                .or_insert_with(|| absolute(sf, root));
        }
    }
    if file_by_key.is_empty() {
        return;
    }
    // Packages: the nearest pubspec above every Dart file.
    let mut nearest: FxHashMap<PathBuf, Option<(PathBuf, String)>> = FxHashMap::default();
    let mut package_of = |file: &Path| -> Option<(PathBuf, String)> {
        let mut dir = file.parent()?.to_path_buf();
        let mut walked = vec![];
        let found = loop {
            if let Some(hit) = nearest.get(&dir) {
                break hit.clone();
            }
            walked.push(dir.clone());
            if let Some(name) = pubspec_name(&dir) {
                break Some((dir.clone(), name));
            }
            if !dir.starts_with(root) || !dir.pop() {
                break None;
            }
        };
        for d in walked {
            nearest.insert(d, found.clone());
        }
        found
    };
    // A corpus can contain different versions of the same package. A name
    // alone cannot choose between them; only the importer's own package is
    // known without reading Dart's resolved package configuration.
    let mut packages: FxHashMap<String, Option<PathBuf>> = FxHashMap::default();
    let mut importer_package: FxHashMap<String, (PathBuf, String)> = FxHashMap::default();
    let mut files = path_of.iter().collect::<Vec<_>>();
    files.sort();
    for (id, path) in files {
        if let Some((dir, name)) = package_of(path) {
            packages
                .entry(name.clone())
                .and_modify(|previous| {
                    if previous.as_ref() != Some(&dir) {
                        *previous = None;
                    }
                })
                .or_insert_with(|| Some(dir.clone()));
            importer_package.insert(id.clone(), (dir, name));
        }
    }
    let uri_nodes = nodes
        .iter()
        .filter(|n| n["source_file"].is_null() && s(n, "label").ends_with(".dart"))
        .map(|n| (s(n, "id").to_string(), s(n, "label").to_string()))
        .collect::<FxHashMap<_, _>>();
    // URI node ids come from the URI text, so `../x.dart` and `x.dart` share
    // one node; each edge's own URI is read back from its source line.
    let mut lines: FxHashMap<String, Vec<String>> = FxHashMap::default();
    let mut uri_at = |src: &str, e: &Value, fallback: &str| -> String {
        let file = lines.entry(src.to_string()).or_insert_with(|| {
            path_of
                .get(src)
                .and_then(|p| crate::fsutil::read_text_lossy(p).ok())
                .map(|t| t.lines().map(str::to_string).collect())
                .unwrap_or_default()
        });
        let line = s(e, "source_location")
            .strip_prefix('L')
            .and_then(|l| l.parse::<usize>().ok())
            .and_then(|l| file.get(l.wrapping_sub(1)));
        let quoted = line.and_then(|l| {
            let (_, rest) = l.split_once(['\'', '"'])?;
            let end = rest.find(['\'', '"'])?;
            Some(rest[..end].to_string())
        });
        quoted
            .filter(|q| q.ends_with(".dart"))
            .unwrap_or_else(|| fallback.to_string())
    };
    let mut pairs = edges
        .iter()
        .flat_map(|e| {
            let (a, b) = (s(e, "source").to_string(), s(e, "target").to_string());
            [(a.clone(), b.clone()), (b, a)]
        })
        .collect::<FxHashSet<_>>();
    let mut retargeted = FxHashSet::default();
    for e in edges.iter_mut() {
        if !matches!(s(e, "relation"), "imports" | "exports") {
            continue;
        }
        let (src, tgt) = (s(e, "source").to_string(), s(e, "target").to_string());
        let (Some(label), Some(importer)) = (uri_nodes.get(&tgt), path_of.get(&src)) else {
            continue;
        };
        let uri = uri_at(&src, e, label);
        let path = if let Some(rest) = uri.strip_prefix("package:") {
            let Some((name, sub)) = rest.split_once('/') else {
                continue;
            };
            let dir = importer_package
                .get(&src)
                .filter(|(_, n)| n == name)
                .map(|(d, _)| d)
                .or_else(|| packages.get(name).and_then(Option::as_ref));
            let Some(dir) = dir else { continue };
            dir.join("lib").join(sub)
        } else if uri.contains(':') {
            continue;
        } else {
            importer.parent().unwrap_or(Path::new("")).join(uri)
        };
        let key = source_key(&normalize(&path).to_string_lossy(), root);
        let Some(file) = file_by_key.get(&key) else {
            continue;
        };
        if file == &src || !pairs.insert((src.clone(), file.clone())) {
            continue;
        }
        pairs.insert((file.clone(), src.clone()));
        e["target"] = json!(file);
        e["resolution"] = json!("dart_import");
        retargeted.insert(tgt);
    }
    if retargeted.is_empty() {
        return;
    }
    let still = edges
        .iter()
        .flat_map(|e| [s(e, "source").to_string(), s(e, "target").to_string()])
        .filter(|id| retargeted.contains(id))
        .collect::<FxHashSet<_>>();
    nodes.retain(|n| {
        let id = s(n, "id");
        !retargeted.contains(id) || still.contains(id)
    });
}

#[cfg(test)]
mod tests {
    use crate::extract::{ExtractOptions, extract};
    use serde_json::Value;
    use std::path::{Path, PathBuf};

    fn write(root: &Path, rel: &str, body: &str) -> PathBuf {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, body).unwrap();
        p
    }
    fn run(root: &Path, paths: &[PathBuf], precise: bool) -> Value {
        let _serial = crate::extractors::parse_cache::test_serial();
        extract(
            paths,
            &ExtractOptions {
                root: Some(root.to_path_buf()),
                parallel: false,
                use_cache: false,
                receiver_resolution: false,
                precise,
                ..Default::default()
            },
        )
        .unwrap()
    }
    /// (source label, target label, whether the target is a file node).
    fn imports(v: &Value) -> Vec<(String, String, bool)> {
        let node = |id: &str| {
            v["nodes"]
                .as_array()
                .unwrap()
                .iter()
                .find(|n| n["id"] == id)
                .cloned()
                .unwrap_or_default()
        };
        let mut out = v["edges"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["relation"] == "imports" || e["relation"] == "exports")
            .map(|e| {
                let (a, b) = (
                    node(e["source"].as_str().unwrap()),
                    node(e["target"].as_str().unwrap()),
                );
                (
                    a["label"].as_str().unwrap_or("").to_string(),
                    b["label"].as_str().unwrap_or("").to_string(),
                    b["source_file"].is_string(),
                )
            })
            .collect::<Vec<_>>();
        out.sort();
        out
    }

    #[test]
    fn ambiguous_external_packages_do_not_bind_to_the_first_directory() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        write(&root, "app/pubspec.yaml", "name: app\n");
        write(&root, "v1/pubspec.yaml", "name: shared\n");
        write(&root, "v2/pubspec.yaml", "name: shared\n");
        let paths = vec![
            write(
                &root,
                "app/lib/main.dart",
                "import 'package:shared/api.dart';\n",
            ),
            write(&root, "v1/lib/api.dart", "void first() {}\n"),
            write(&root, "v2/lib/api.dart", "void second() {}\n"),
            write(
                &root,
                "v2/lib/own.dart",
                "import 'package:shared/api.dart';\n",
            ),
        ];
        let resolved = imports(&run(&root, &paths, true));
        assert!(
            resolved.contains(&("main.dart".into(), "package:shared/api.dart".into(), false)),
            "{resolved:?}"
        );
        assert!(
            resolved.contains(&("own.dart".into(), "api.dart".into(), true)),
            "{resolved:?}"
        );
    }

    #[test]
    fn relative_and_package_imports_reach_file_nodes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        write(&root, "app/pubspec.yaml", "name: app # the app\n");
        write(&root, "shared/pubspec.yaml", "name: 'shared'\n");
        let paths = vec![
            write(
                &root,
                "app/lib/a.dart",
                "import 'b.dart';\nimport 'package:app/src/c.dart';\nimport 'package:shared/s.dart';\nimport 'dart:io';\nexport '../lib/b.dart';\n",
            ),
            write(&root, "app/lib/b.dart", "void helperB() {}\n"),
            write(&root, "app/lib/src/c.dart", "void helperC() {}\n"),
            // `../b.dart` and `b.dart` share one URI node id.
            write(&root, "app/lib/src/d.dart", "import '../b.dart';\n"),
            write(&root, "shared/lib/s.dart", "void shared() {}\n"),
        ];
        // Default (Graphify parity): no import reaches a file node.
        let before = imports(&run(&root, &paths, false));
        assert!(before.iter().all(|(_, _, file)| !file), "{before:?}");
        let v = run(&root, &paths, true);
        let after = imports(&v);
        for want in ["b.dart", "c.dart", "s.dart"] {
            assert!(
                after
                    .iter()
                    .any(|(s, t, file)| s == "a.dart" && t == want && *file),
                "{after:?}"
            );
        }
        assert!(
            after
                .iter()
                .any(|(s, t, file)| s == "d.dart" && t == "b.dart" && *file),
            "{after:?}"
        );
        // SDK imports stay symbolic; the second edge to b.dart keeps its URI node.
        assert!(after.iter().any(|(_, t, file)| t == "dart:io" && !file));
        assert!(
            after
                .iter()
                .any(|(_, t, file)| t == "../lib/b.dart" && !file)
        );
        let uri_nodes = v["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|n| n["label"] == "package:app/src/c.dart")
            .count();
        assert_eq!(uri_nodes, 0);
    }
}
