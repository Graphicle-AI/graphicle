//! Exact source identities for literal Jest/Vitest test registrations.
//! Never expand dynamic `.each` names, guess computed titles, or merge duplicates.
use super::{
    base::read_text,
    engine_types::named,
    resolution_facts::{parse_js_tree, walk},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, path::Path};
use tree_sitter::Node;

fn registration<'t>(node: Node<'t>, source: &[u8]) -> Option<(bool, Option<String>, Node<'t>)> {
    if node.kind() != "call_expression" {
        return None;
    }
    let function = node.child_by_field_name("function")?;
    let name = read_text(function, source);
    let parts = name.split('.').collect::<Vec<_>>();
    let suite = matches!(*parts.first()?, "describe" | "suite");
    if !suite && !matches!(*parts.first()?, "test" | "it") {
        return None;
    }
    if parts
        .iter()
        .skip(1)
        .any(|p| !matches!(*p, "only" | "skip" | "todo" | "concurrent" | "sequential"))
    {
        return None;
    }
    let args = named(node.child_by_field_name("arguments")?);
    let title = args.first()?;
    let literal = if matches!(title.kind(), "string" | "template_string") {
        let text = read_text(*title, source);
        if text.contains('\\') || text.contains("${") || text.len() < 2 {
            None
        } else {
            Some(text[1..text.len() - 1].to_string())
        }
    } else {
        None
    };
    let callback = *args
        .iter()
        .find(|n| matches!(n.kind(), "arrow_function" | "function_expression"))?;
    Some((suite, literal, callback.child_by_field_name("body")?))
}

pub fn augment(path: &Path, record: &mut Value, root: &Path) {
    if !crate::paths::is_test_path(&path.to_string_lossy())
        || !matches!(
            path.extension().and_then(|s| s.to_str()),
            Some("ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs")
        )
    {
        return;
    }
    let Some(tree) = parse_js_tree(path) else {
        return;
    };
    let Some(file_id) = record["nodes"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|n| n["source_location"] == "L1")
        .and_then(|n| n["id"].as_str())
        .map(str::to_string)
    else {
        return;
    };
    let source = &tree.source;
    let mut tests = vec![];
    for node in walk(tree.tree.root_node()) {
        let Some((false, Some(title), body)) = registration(node, source) else {
            continue;
        };
        let mut names = vec![title];
        let mut ancestor = node.parent();
        let mut exact = true;
        while let Some(parent) = ancestor {
            if let Some((true, name, _)) = registration(parent, source) {
                if let Some(name) = name {
                    names.push(name);
                } else {
                    exact = false;
                    break;
                }
            } else if parent.kind() == "call_expression"
                && parent.child_by_field_name("function").is_some_and(|f| {
                    matches!(
                        read_text(f, source).split(['.', '(']).next(),
                        Some("describe" | "suite")
                    )
                })
            {
                // A dynamic or parameterised suite changes the runtime full
                // name. Its literal child title is not a standalone test id.
                exact = false;
                break;
            }
            ancestor = parent.parent();
        }
        if !exact {
            continue;
        }
        names.reverse();
        let full = names.join(" ");
        tests.push((
            full,
            node.start_position().row + 1,
            node.end_position().row + 1,
            body.start_position().row + 1,
            body.end_position().row + 1,
            node.start_byte(),
            node.end_byte(),
        ));
    }
    let mut counts = BTreeMap::new();
    for (name, ..) in &tests {
        *counts.entry(name.clone()).or_insert(0) += 1;
    }
    tests.retain(|(name, ..)| counts[name] == 1);
    let relative = super::resolution::source_key(&path.to_string_lossy(), root);
    let mut ranges = vec![];
    for (full, start, end, body_start, body_end, byte_start, byte_end) in tests {
        let digest = format!("{:x}", Sha256::digest(full.as_bytes()));
        let id = format!("{file_id}_test_{}", &digest[..16]);
        let Some(nodes) = record["nodes"].as_array_mut() else {
            return;
        };
        nodes.push(json!({
            "id":id,"label":full,"qualified_name":full,"test_id":format!("{relative}::{full}"),
            "kind":"test","type":"function","file_type":"code","is_test":true,
            "source_file":path,"source_location":format!("L{start}-L{end}"),
            "byte_start":byte_start,"byte_end":byte_end,"test_framework":"jest_compatible_literal_registration"
        }));
        if let Some(edges) = record["edges"].as_array_mut() {
            edges.push(json!({"source":file_id,"target":id,
            "relation":"contains","confidence":"EXTRACTED","source_file":path,"source_location":format!("L{start}"),"weight":1.0}));
        }
        ranges.push((id, body_start, body_end));
    }
    for call in record["raw_calls"].as_array_mut().into_iter().flatten() {
        if call["caller_nid"] != file_id {
            continue;
        }
        let line = call["source_location"]
            .as_str()
            .and_then(|s| s.trim_start_matches('L').split(['-', ':']).next())
            .and_then(|s| s.parse::<usize>().ok());
        let Some(line) = line else {
            continue;
        };
        let owners = ranges
            .iter()
            .filter(|(_, start, end)| (*start..=*end).contains(&line))
            .collect::<Vec<_>>();
        if let [owner] = owners.as_slice() {
            call["caller_nid"] = json!(owner.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::extract::{ExtractOptions, extract};
    #[test]
    fn exact_suite_names_resolve_calls_and_ambiguous_names_do_not_merge() {
        let _serial = crate::extractors::parse_cache::test_serial();
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("math.ts");
        let tests = root.path().join("math.test.ts");
        std::fs::write(
            &source,
            "export function add(a:number,b:number){return a+b;}\n",
        )
        .unwrap();
        std::fs::write(&tests,"import {test,describe,expect} from 'vitest';\nimport {add} from './math';\ndescribe('math',()=>{\n test('adds values',()=>{\n  expect(add(1,2)).toBe(3);\n });\n});\ntest('duplicate',()=>{});\ntest('duplicate',()=>{});\ntest(name,()=>{});\n").unwrap();
        let options = ExtractOptions {
            root: Some(root.path().into()),
            use_cache: false,
            parallel: false,
            test_identities: true,
            ..Default::default()
        };
        let graph = extract(&[source, tests], &options).unwrap();
        let nodes = graph["nodes"].as_array().unwrap();
        let tests = nodes
            .iter()
            .filter(|n| n["kind"] == "test")
            .collect::<Vec<_>>();
        assert_eq!(tests.len(), 1);
        assert_eq!(tests[0]["test_id"], "math.test.ts::math adds values");
        let target = nodes.iter().find(|n| n["label"] == "add()").unwrap();
        assert!(
            graph["edges"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["source"] == tests[0]["id"]
                    && e["target"] == target["id"]
                    && e["relation"] == "calls")
        );
    }
}
