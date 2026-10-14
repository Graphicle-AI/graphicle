//! Native robustness beyond the pinned R extractor's normalized-id behavior.
use graphicle::extractors::r::extract_r;

#[test]
fn colliding_scope_ids_leave_unknown_calls_unresolved() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("example.R");
    for source in [
        "`!!!` <- function() { external_call() }\n",
        "outer <- function() { `!!!` <- function() { external_call() }; external_call() }\n",
    ] {
        std::fs::write(&path, source).unwrap();
        let result = extract_r(&path);
        assert!(result.get("error").is_none(), "{result}");
        assert!(
            result["raw_calls"]
                .as_array()
                .unwrap()
                .iter()
                .any(|call| { call["callee"] == "external_call" && call["language"] == "r" }),
            "{result}"
        );
    }
}
