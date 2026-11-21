// Adapted from Graphify 0.9.107, Apache-2.0; see NOTICE.md.
// Upstream: tests/test_extract_php_closures.py
//! Extraction changes ported from Graphify 0.9.107. Cases follow the
//! upstream tests named in each comment.
use graphicle::api::{ExtractOptions, extract};
use std::collections::BTreeSet;

fn labels(name: &str, text: &str) -> BTreeSet<String> {
    graphicle::rcfile::set_language_overrides(None);
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let path = root.join(name);
    std::fs::write(&path, text).unwrap();
    let r = extract(
        &[path],
        &ExtractOptions {
            root: Some(root.clone()),
            cache_root: Some(root.join("cache")),
            parallel: false,
            ..Default::default()
        },
    )
    .unwrap();
    r["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|n| n["label"].as_str().map(str::to_string))
        .collect()
}

#[test]
fn php_route_path_comes_only_from_arguments_before_the_closure() {
    // test_extract_php_closures.py (#5135)
    let l = labels("late_path.php", "<?php\n$app->get(function () { return 1; }, '/late');\n");
    assert!(!l.iter().any(|x| x.starts_with("GET ")), "{l:?}");
    let l = labels(
        "late_prefix.php",
        "<?php\n$app->group(function ($group) {\n    $group->get('/users', function () { return 1; });\n}, '/late');\n",
    );
    assert!(l.contains("GET /users()"), "{l:?}");
    assert!(!l.contains("GET /late/users()"), "{l:?}");
}
