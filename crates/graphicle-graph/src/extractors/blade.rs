// Adapted from Graphify 0.9.103 (16218bfc), Apache-2.0; see NOTICE.md.
// Upstream: graphify/extractors/blade.py
//! Port of graphify/extractors/blade.py at 20a20d30d8e7eef77675651f0199d87f913bd3e7.
use crate::{fsutil, ids::make_id, regex_compat::compile};
use regex::Regex;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::path::Path;
use std::sync::LazyLock;

static EXTENDS: LazyLock<Regex> = LazyLock::new(|| compile(r#"@extends\(['"]([^'"]+)['"]"#));
static INCLUDE: LazyLock<Regex> = LazyLock::new(|| compile(r#"@include\(['"]([^'"]+)['"]"#));
static LIVEWIRE: LazyLock<Regex> = LazyLock::new(|| compile(r"<livewire:([\w.\-]+)"));
// An optional Livewire modifier (`wire:click.prevent=`), Graphify 0.9.97.
static CLICK: LazyLock<Regex> =
    LazyLock::new(|| compile(r#"wire:click(?:\.\w+)*=["']([^"']+)["']"#));
static METHOD_NAME: LazyLock<Regex> = LazyLock::new(|| compile(r"^[A-Za-z_]\w*"));

pub fn extract_blade(path: &Path) -> Value {
    let str_path = fsutil::path_string(path);
    let Ok(src) = super::base::read_source_text(path, true) else {
        return json!({"error":format!("cannot read {str_path}")});
    };
    let file_nid = make_id([&str_path]);
    let parts = crate::path_syntax::posix_parts(&str_path);
    let name = crate::path_syntax::posix_name(&parts);
    let mut nodes = vec![
        json!({"id":file_nid,"label":name,"file_type":"code","source_file":str_path,"source_location":null}),
    ];
    let mut edges = Vec::new();
    let mut seen = HashSet::from([file_nid.clone()]);
    // Upstream visits @extends (template inheritance), then @include, <livewire:>
    // and wire:click; a target seen earlier keeps its first node.
    for (pattern, relation) in [
        (&*EXTENDS, "extends"),
        (&*INCLUDE, "includes"),
        (&*LIVEWIRE, "uses_component"),
        (&*CLICK, "binds_method"),
    ] {
        for captures in pattern.captures_iter(&src) {
            let mut label = &captures[1];
            if relation == "binds_method" {
                // The bare method name: a call's arguments are not part of it,
                // and a magic action (`$set`, `$toggle`) is Livewire's own, so
                // it binds nothing (Graphify 0.9.97).
                let raw = label.trim();
                if raw.starts_with('$') {
                    continue;
                }
                let Some(name) = METHOD_NAME.find(raw) else {
                    continue;
                };
                label = name.as_str();
            }
            let target = if matches!(relation, "extends" | "includes") {
                label.replace('.', "/")
            } else {
                label.to_owned()
            };
            let nid = make_id([target]);
            if seen.insert(nid.clone()) {
                nodes.push(json!({"id":nid,"label":label,"file_type":"code","source_file":str_path,"source_location":null}));
            }
            edges.push(
                json!({"source":file_nid,"target":nid,"relation":relation,"confidence":"EXTRACTED",
                "confidence_score":1.0,"source_file":str_path,"source_location":null,"weight":1.0}),
            );
        }
    }
    json!({"nodes":nodes,"edges":edges})
}
