//! Mirrors upstream 0.9.66 `tests/test_cobol_extractor.py` (new COBOL
//! extractor, #3660) plus the 0.9.68 fixed-format and PERFORM THRU fixes.
use graphicle::api::{ExtractOptions, extract};
use serde_json::Value;
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/new_languages/sample.cob")
        .canonicalize()
        .unwrap()
}

fn run(paths: &[PathBuf], cache_root: &Path) -> Value {
    let options = ExtractOptions {
        root: Some(cache_root.to_path_buf()),
        cache_root: Some(cache_root.to_path_buf()),
        parallel: false,
        ..Default::default()
    };
    extract(paths, &options).unwrap()
}

#[test]
fn test_cobol_is_registered_and_detected_as_code() {
    assert!(graphicle::extractors::registered_languages().contains(&"cobol"));
    assert_eq!(
        graphicle::extract::get_extractor(Path::new("a.cob")).as_deref(),
        Some("cobol")
    );
    assert_eq!(graphicle::analyze::file_category("x/a.cbl"), "code");
}

fn labels(result: &Value) -> BTreeSet<String> {
    result["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["label"].as_str().unwrap().to_string())
        .collect()
}

fn edge_labels(result: &Value, relation: &str) -> BTreeSet<(String, String)> {
    let by_id: HashMap<&str, &str> = result["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| (n["id"].as_str().unwrap(), n["label"].as_str().unwrap()))
        .collect();
    result["edges"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["relation"] == relation)
        .map(|e| {
            let s = e["source"].as_str().unwrap();
            let t = e["target"].as_str().unwrap();
            (
                by_id.get(s).copied().unwrap_or(s).to_string(),
                by_id.get(t).copied().unwrap_or(t).to_string(),
            )
        })
        .collect()
}

fn has(set: &BTreeSet<(String, String)>, s: &str, t: &str) -> bool {
    set.contains(&(s.to_string(), t.to_string()))
}

#[test]
fn test_cobol_program_paragraphs_and_perform_calls() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let source = root.join("demo.cbl");
    std::fs::write(
        &source,
        "       IDENTIFICATION DIVISION.\n       PROGRAM-ID. DEMO.\n       PROCEDURE DIVISION.\n       MAIN-PARA.\n           PERFORM WORK-PARA.\n           STOP RUN.\n       WORK-PARA.\n           DISPLAY 'ok'.\n",
    )
    .unwrap();
    let result = run(&[source], &root);
    let labels = labels(&result);
    for want in ["DEMO", "MAIN-PARA", "WORK-PARA"] {
        assert!(labels.contains(want), "{want} missing from {labels:?}");
    }
    assert!(has(
        &edge_labels(&result, "calls"),
        "MAIN-PARA",
        "WORK-PARA"
    ));
}

#[test]
fn test_cobol_fixed_free_copy_call_and_exec_blocks() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let copybook = root.join("CUSTOMER.cpy");
    std::fs::write(
        &copybook,
        "       01  CUSTOMER-RECORD.\n       05  CUSTOMER-\n      -    NAME PIC X(20).\n",
    )
    .unwrap();
    let subprogram = root.join("subprog.cbl");
    std::fs::write(
        &subprogram,
        "       IDENTIFICATION DIVISION.\n       PROGRAM-ID. SUBPROG.\n       PROCEDURE DIVISION.\n       ENTRY-PARA.\n           GOBACK.\n",
    )
    .unwrap();
    let source = root.join("main.cob");
    std::fs::write(
        &source,
        concat!(
            ">>SOURCE FORMAT FREE\n",
            "IDENTIFICATION DIVISION.\n",
            "PROGRAM-ID. MAINPROG.\n",
            "DATA DIVISION.\n",
            "WORKING-STORAGE SECTION.\n",
            "01 TARGET-NAME PIC X(8).\n",
            "COPY CUSTOMER REPLACING ==CUSTOMER-RECORD== BY ==CLIENT-RECORD==.\n",
            "PROCEDURE DIVISION.\n",
            "MAIN-PARA.\n",
            "    PERFORM WORK-PARA.\n",
            "    CALL 'SUBPROG'.\n",
            "    CALL TARGET-NAME.\n",
            "    EXEC CICS\n",
            "      CALL 'FAKE'\n",
            "    END-EXEC.\n",
            "WORK-PARA SECTION.\n",
            "    DISPLAY \"PERFORM FAKE-PARA\".\n",
            "    DISPLAY \"CALL 'SUBPROG'\".\n",
            "    STOP RUN.\n",
        ),
    )
    .unwrap();

    let result = run(&[source, subprogram, copybook], &root);
    let labels = labels(&result);
    for want in [
        "MAINPROG",
        "SUBPROG",
        "MAIN-PARA",
        "WORK-PARA",
        "TARGET-NAME",
        "CUSTOMER-RECORD",
        "CUSTOMER-NAME",
    ] {
        assert!(labels.contains(want), "{want} missing from {labels:?}");
    }
    let calls = edge_labels(&result, "calls");
    assert!(has(&calls, "MAIN-PARA", "WORK-PARA"), "{calls:?}");
    assert!(has(&calls, "MAIN-PARA", "SUBPROG"), "{calls:?}");
    assert!(!has(&calls, "WORK-PARA", "SUBPROG"), "{calls:?}");
    assert!(calls.iter().all(|(_, t)| t != "FAKE"), "{calls:?}");
    assert!(
        result["edges"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["relation"] == "imports_from")
    );
}

#[test]
fn test_cobol_fixture_uses_normal_extract_path() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let result = run(&[fixture()], &root);
    let labels = labels(&result);
    for want in ["SAMPLE", "HELPER-PARA", "MAIN-PARA"] {
        assert!(labels.contains(want), "{want} missing from {labels:?}");
    }
    assert!(has(
        &edge_labels(&result, "calls"),
        "MAIN-PARA",
        "HELPER-PARA"
    ));
}

#[test]
fn test_cobol_malformed_tail_comments_and_strings_do_not_create_phantoms() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let source = root.join("broken.cbl");
    std::fs::write(
        &source,
        "       IDENTIFICATION DIVISION.\n       PROGRAM-ID. KEPT.\n       PROCEDURE DIVISION.\n       MAIN.\n           DISPLAY 'PERFORM GHOST'.\n",
    )
    .unwrap();
    let result = run(&[source], &root);
    let labels: BTreeSet<String> = labels(&result)
        .into_iter()
        .map(|l| l.to_lowercase())
        .collect();
    assert!(labels.contains("kept"), "{labels:?}");
    assert!(!labels.contains("ghost"), "{labels:?}");
}
