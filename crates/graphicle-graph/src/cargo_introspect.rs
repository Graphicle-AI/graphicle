// Adapted from Graphify 0.9.88 (a9860f70), Apache-2.0; see NOTICE.md.
// Upstream: graphify/cargo_introspect.py
//! Port of graphify/cargo_introspect.py at 20a20d30d8e7eef77675651f0199d87f913bd3e7.
//! Reads manifests only; does not execute Cargo or project build scripts.
use crate::Result;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub fn load_toml(path: &Path) -> Result<toml::Value> {
    let bytes =
        std::fs::read(path).map_err(|error| format!("OSError: {}: {error}", path.display()))?;
    let text =
        std::str::from_utf8(&bytes).map_err(|error| format!("UnicodeDecodeError: {error}"))?;
    toml::from_str(text)
        .map_err(|error| format!("TOMLDecodeError: {error}"))
        .map_err(crate::Error::from)
}

pub fn member_manifest_paths(root: &Path, data: &toml::Value) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    if data.get("package").is_some_and(toml::Value::is_table) {
        paths.push(root.join("Cargo.toml"));
    }
    let Some(members) = data
        .get("workspace")
        .and_then(toml::Value::as_table)
        .and_then(|workspace| workspace.get("members"))
        .and_then(toml::Value::as_array)
    else {
        return Ok(paths);
    };
    for pattern in members.iter().filter_map(toml::Value::as_str) {
        if Path::new(pattern).is_absolute() {
            return Err("NotImplementedError: Non-relative patterns are unsupported".into());
        }
        if pattern.is_empty() {
            return Err("ValueError: Unacceptable pattern: ''".into());
        }
        let pattern = format!(
            "{}/{}",
            glob::Pattern::escape(&root.to_string_lossy()),
            pattern
        );
        let matches = glob::glob_with(
            &pattern,
            glob::MatchOptions {
                case_sensitive: true,
                require_literal_separator: true,
                require_literal_leading_dot: false,
            },
        )
        .map_err(|error| format!("ValueError: {error}"))?;
        let mut matches = matches
            .filter_map(std::result::Result::ok)
            .collect::<Vec<_>>();
        matches.sort();
        for member in matches {
            let manifest = member.join("Cargo.toml");
            if manifest.is_file() && !paths.contains(&manifest) {
                paths.push(manifest);
            }
        }
    }
    Ok(paths)
}

/// Bounded-depth search for a `Cargo.toml` below *root* (0.9.66, #3678),
/// pruning the same noise directories the AST scanner skips. Returns the
/// sorted-first candidate, mirroring `_discover_root_manifest`.
pub fn discover_root_manifest(root: &Path) -> Option<PathBuf> {
    const MAX_DISCOVERY_DEPTH: usize = 2;
    let mut found = Vec::new();
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    while let Some((current, depth)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&current) else {
            continue;
        };
        let mut dirs = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                dirs.push(path);
            } else if depth > 0 && path.file_name().is_some_and(|n| n == "Cargo.toml") {
                found.push(path);
            }
        }
        if depth >= MAX_DISCOVERY_DEPTH {
            continue;
        }
        for dir in dirs {
            let name = dir
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            if !crate::detect_types::is_noise_dir(&name, Some(&current)) {
                stack.push((dir, depth + 1));
            }
        }
    }
    found.sort();
    found.into_iter().next()
}

pub fn introspect_cargo(root: &Path) -> Result<Value> {
    let root = root
        .canonicalize()
        .map_err(|error| format!("OSError: {}: {error}", root.display()))?;
    let mut root_manifest = root.join("Cargo.toml");
    // #3678: discover a manifest below the scan root when none sits at it.
    if !root_manifest.is_file()
        && let Some(discovered) = discover_root_manifest(&root)
    {
        root_manifest = discovered;
    }
    let root_data = load_toml(&root_manifest)?;
    // #3678: members resolve relative to the manifest's own directory.
    let member_root = root_manifest.parent().unwrap_or(&root).to_path_buf();
    let workspace_dependencies = root_data
        .get("workspace")
        .and_then(toml::Value::as_table)
        .and_then(|workspace| workspace.get("dependencies"))
        .and_then(toml::Value::as_table)
        .cloned()
        .unwrap_or_default();
    let manifests = member_manifest_paths(&member_root, &root_data)?;
    let mut crates = BTreeMap::new();
    for manifest in manifests {
        let data = if manifest == root_manifest {
            root_data.clone()
        } else {
            load_toml(&manifest)?
        };
        let Some(name) = data
            .get("package")
            .and_then(toml::Value::as_table)
            .and_then(|package| package.get("name"))
            .and_then(toml::Value::as_str)
        else {
            continue;
        };
        crates.insert(name.to_owned(), (format!("crate:{name}"), manifest, data));
    }
    let relative = |path: &Path| -> Result<String> {
        Ok(path
            .strip_prefix(&root)
            .map(|path| {
                path.to_string_lossy()
                    .replace(std::path::MAIN_SEPARATOR, "/")
            })
            .map_err(|error| format!("ValueError: {error}"))?)
    };
    let mut nodes = Vec::new();
    let mut edges = Vec::new();
    for (name, (id, manifest, _)) in &crates {
        nodes.push(
            json!({"id":id,"label":name,"source_file":relative(manifest)?,"source_location":"L1"}),
        );
    }
    for (source_id, manifest, data) in crates.values() {
        let Some(dependencies) = data.get("dependencies").and_then(toml::Value::as_table) else {
            continue;
        };
        let mut names = dependencies.keys().collect::<Vec<_>>();
        names.sort();
        for name in names {
            let mut spec = &dependencies[name];
            let inherited = spec
                .as_table()
                .is_some_and(|table| table.contains_key("workspace"));
            if inherited {
                // #3734: `dep = { workspace = true }` takes its identity from
                // `[workspace.dependencies]`; only path deps can be local.
                if spec.get("workspace").and_then(toml::Value::as_bool) != Some(true) {
                    continue;
                }
                match workspace_dependencies.get(name.as_str()) {
                    Some(shared)
                        if shared.is_table()
                            && shared.get("path").is_some_and(toml::Value::is_str) =>
                    {
                        spec = shared;
                    }
                    _ => continue,
                }
            }
            let actual_name = spec
                .as_table()
                .and_then(|spec| spec.get("package"))
                .and_then(toml::Value::as_str)
                .filter(|name| !name.is_empty())
                .unwrap_or(name);
            let Some((target_id, target_manifest, _)) = crates.get(actual_name) else {
                continue;
            };
            if inherited {
                // Shared paths are relative to the workspace root; a matching
                // name alone cannot prove a registry/git dep is local.
                let shared_path = spec.get("path").and_then(toml::Value::as_str).unwrap_or("");
                let inherited_manifest = member_root.join(shared_path).join("Cargo.toml");
                let same = inherited_manifest
                    .canonicalize()
                    .ok()
                    .zip(target_manifest.canonicalize().ok())
                    .is_some_and(|(a, b)| a == b);
                if !same {
                    continue;
                }
            }
            edges.push(
                json!({"source":source_id,"target":target_id,"relation":"crate_depends_on",
                "context":"cargo_dependency","weight":1.0,"confidence":"EXTRACTED",
                "source_file":relative(manifest)?,"source_location":"L1"}),
            );
        }
    }
    Ok(json!({"nodes":nodes,"edges":edges}))
}
