//! The Python grammar the engine links is tree-sitter-python 0.25.0, the
//! grammar behind Graphify's pinned Python parser, vendored as
//! `graphicle-tree-sitter-python` (`vendor/tree-sitter-python/`) with one
//! change: the grammar pack's fix for a one-byte overflow in the external
//! scanner's state serializer (`serialize-buffer-overflow.patch`). These tests
//! pin that identity on every platform the tests run on: the linked language
//! has the vendored parser's tables, the vendored files are the recorded ones,
//! the scanner differs from the published one by the patch alone, the lock
//! holds no other Python grammar, and the serializer stays inside its buffer.
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

fn vendor() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("vendor/tree-sitter-python")
}

fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn source() -> Value {
    serde_json::from_slice(&std::fs::read(vendor().join("SOURCE.json")).unwrap()).unwrap()
}

/// `#define NAME value` from the generated parser.
fn define(parser: &str, name: &str) -> usize {
    let prefix = format!("#define {name} ");
    parser
        .lines()
        .find_map(|line| line.strip_prefix(&prefix))
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or_else(|| panic!("{name} is not defined in parser.c"))
}

/// The lines a one-hunk unified diff removes and adds, context excluded.
fn hunk(patch: &str) -> (String, String) {
    assert_eq!(patch.lines().filter(|l| l.starts_with("@@")).count(), 1);
    let (mut removed, mut added) = (String::new(), String::new());
    for line in patch.lines().skip_while(|l| !l.starts_with("@@")).skip(1) {
        if let Some(text) = line.strip_prefix('-') {
            removed.push_str(text);
            removed.push('\n');
        } else if let Some(text) = line.strip_prefix('+') {
            added.push_str(text);
            added.push('\n');
        }
    }
    (removed, added)
}

/// Two grammar libraries in one binary can define the same
/// `tree_sitter_python` symbol, and the linker keeps one of them. The
/// language this build links must have the vendored `parser.c`'s tables.
#[test]
fn the_linked_python_grammar_is_the_vendored_one() {
    let parser = std::fs::read_to_string(vendor().join("grammar/parser.c")).unwrap();
    let language: tree_sitter::Language = tree_sitter_python::LANGUAGE.into();
    assert_eq!(language.abi_version(), define(&parser, "LANGUAGE_VERSION"));
    assert_eq!(
        language.node_kind_count(),
        define(&parser, "SYMBOL_COUNT") + define(&parser, "ALIAS_COUNT")
    );
    assert_eq!(language.parse_state_count(), define(&parser, "STATE_COUNT"));
    assert_eq!(language.field_count(), define(&parser, "FIELD_COUNT"));
    assert_eq!(language.name(), Some("python"));
    let metadata = language.metadata().unwrap();
    assert_eq!(
        (
            metadata.major_version,
            metadata.minor_version,
            metadata.patch_version
        ),
        (0, 25, 0)
    );
}

/// Every vendored file is the one `SOURCE.json` records, and undoing the
/// recorded patch gives back the published crate's scanner.
#[test]
fn the_vendored_python_grammar_is_the_published_one_plus_the_patch() {
    let source = source();
    let files = source["files"].as_object().unwrap();
    assert!(files.contains_key("grammar/parser.c") && files.contains_key("grammar/scanner.c"));
    for (name, digest) in files {
        let bytes = std::fs::read(vendor().join(name)).unwrap();
        assert_eq!(sha256(&bytes), digest.as_str().unwrap(), "{name}");
    }
    let patched = &source["patched"]["grammar/scanner.c"];
    let patch = std::fs::read_to_string(vendor().join(patched["patch"].as_str().unwrap())).unwrap();
    assert_eq!(
        sha256(patch.as_bytes()),
        patched["patch_sha256"].as_str().unwrap()
    );
    let (removed, added) = hunk(&patch);
    let scanner = std::fs::read_to_string(vendor().join("grammar/scanner.c")).unwrap();
    assert_eq!(scanner.matches(&added).count(), 1);
    assert_eq!(scanner.matches(&removed).count(), 0);
    let published = scanner.replacen(&added, &removed, 1);
    assert_eq!(
        sha256(published.as_bytes()),
        patched["upstream_sha256"].as_str().unwrap()
    );
}

/// The published tree-sitter-python crate, whose scanner has the overflow,
/// is not in the engine's lock: no build of the engine links it.
#[test]
fn the_lock_holds_only_the_vendored_python_grammar() {
    let lock =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.lock")).unwrap();
    let names: Vec<&str> = lock
        .lines()
        .filter_map(|line| line.strip_prefix("name = "))
        .collect();
    assert!(!names.contains(&"\"tree-sitter-python\""));
    assert_eq!(
        names
            .iter()
            .filter(|name| **name == "\"graphicle-tree-sitter-python\"")
            .count(),
        1
    );
}

/// `levels` nested `if` blocks, one space deeper each, around a string.
fn nested(levels: usize) -> String {
    let mut source = String::new();
    for depth in 0..levels {
        source.push_str(&" ".repeat(depth));
        source.push_str("if x:\n");
    }
    source.push_str(&" ".repeat(levels));
    source.push_str("y = \"z\"\n");
    source
}

/// tree-sitter-python 0.25.0's serializer wrote two bytes per indent level
/// under `size < 1024`. With an odd number of open string delimiters and more
/// than 510 nested indents, the second byte landed one past the runtime's
/// 1,024-byte buffer (`src/scanner.c:383` of the published crate). The patch
/// stops at `size + 2 <= 1024`. The tree-sitter runtime is compiled without
/// `NDEBUG` here, so it asserts the returned length, and the unpatched scanner
/// aborts this test.
#[test]
fn a_deep_indent_stack_inside_a_string_serializes_within_the_buffer() {
    let parse = |source: String| {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_python::LANGUAGE.into())
            .unwrap();
        parser.parse(source, None).unwrap()
    };
    // 500 levels fit (3 + 2 * 500 bytes), so the state round-trips exactly.
    assert!(!parse(nested(500)).root_node().has_error());
    // 600 levels do not fit. The state is cut at the buffer, as the grammar
    // pack intends: the deeper indents are lost and the tree has errors
    // (CPython itself refuses more than 100 levels), but the parse completes
    // over the whole file instead of writing past the buffer.
    let source = nested(600);
    let tree = parse(source.clone());
    assert_eq!(tree.root_node().end_byte(), source.len());
}
