//! Deep reranking: `rerank_depth` (at most `query::MAX_RERANK_DEPTH`) reads the
//! cross-encoder past `rank`'s 60-passage list into the fused order, skipping
//! repeated text and origins already at their cap as the selection does.
use graphicle_runtime::{
    query::MAX_RERANK_DEPTH,
    store::{Store, s},
};
use serde_json::{Value, json};
use tempfile::TempDir;

const QUESTION: &str = "How does Copperfern retry failed uploads?";
const NOTES: usize = 70;
const COPIES: usize = 10;

fn note(i: usize) -> String {
    format!(
        "Copperfern retries failed upload batch {i} after {} seconds and files ticket CF-{:04}.",
        i + 3,
        i * 37 + 11
    )
}

/// Seventy distinct notes, ten notes repeated verbatim under other ids, and a
/// manual whose five sections each become a passage of one origin.
fn documents() -> Vec<Value> {
    let mut documents = (0..NOTES)
        .map(|i| json!({"id":format!("note-{i}"),"title":format!("Note {i}"),"content":note(i)}))
        .collect::<Vec<_>>();
    documents.extend(
        (0..COPIES).map(
            |i| json!({"id":format!("copy-{i}"),"title":format!("Copy {i}"),"content":note(i)}),
        ),
    );
    let manual = (0..5)
        .map(|stage| {
            format!(
                "Copperfern retries failed uploads in stage {stage}. {}",
                "Operators log the batch identifier in the ledger. ".repeat(36)
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    documents.push(json!({"id":"manual","title":"Upload manual","content":manual}));
    documents
}

struct Fixture {
    _dir: TempDir,
    store: Store,
    workspace: String,
    reader: String,
    repo: String,
}
impl Fixture {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let ws = store.create_workspace("Rerank depth").unwrap();
        let workspace = s(&ws, "id").to_string();
        let owner = s(&ws, "capability").to_string();
        let repo = s(
            &store
                .register(
                    &workspace,
                    &owner,
                    &json!({"project_key":"depth","name":"Depth"}),
                )
                .unwrap(),
            "id",
        )
        .to_string();
        let reader = s(
            &store
                .grant(
                    &workspace,
                    &owner,
                    &json!({"label":"Reader","role":"reader"}),
                )
                .unwrap(),
            "capability",
        )
        .to_string();
        let connector = store.connector_add(&workspace,&owner,&json!({"repository_id":repo,"kind":"jsonl","name":"Notes","config":{"acl":{"workspace_readers":true,"credentials":[]}}})).unwrap();
        store.import_documents(&workspace,&owner,&json!({"repository_id":repo,"connector_id":connector["id"],"mode":"snapshot","documents":documents()})).unwrap();
        Self {
            _dir: dir,
            store,
            workspace,
            reader,
            repo,
        }
    }
    fn args(&self, options: Value) -> Value {
        let mut args =
            json!({"repository_id":self.repo,"question":QUESTION,"limit":8,"max_tokens":4000});
        for (key, value) in options.as_object().unwrap() {
            args[key] = value.clone();
        }
        args
    }
}

#[test]
fn rerank_depth_is_bounded() {
    let f = Fixture::new();
    assert_eq!(MAX_RERANK_DEPTH, 300);
    let deepest = f
        .store
        .project_context(
            &f.workspace,
            &f.reader,
            &f.args(json!({"rerank_depth":MAX_RERANK_DEPTH})),
        )
        .unwrap_or_else(|e| panic!("{}", e.message));
    let past = f
        .store
        .project_context(
            &f.workspace,
            &f.reader,
            &f.args(json!({"rerank_depth":MAX_RERANK_DEPTH + 1})),
        )
        .unwrap_err();
    assert!(
        past.message.contains("Invalid rerank_depth"),
        "{}",
        past.message
    );
    // Without a cross-encoder the depth changes nothing it would read.
    let plain = f
        .store
        .project_context(&f.workspace, &f.reader, &f.args(json!({})))
        .unwrap();
    assert!(plain.get("local_reranker").is_none());
    assert!(plain.get("candidates_debug").is_none());
    assert_eq!(
        plain["sources"], deepest["sources"],
        "rerank_depth without a reranker changed the packet"
    );
}

/// Run explicitly with digest-pinned assets. On macOS, ONNX Runtime can abort
/// in its own static teardown after the test has passed; the CLI avoids that
/// with `local_inference::exit_if_loaded`.
#[cfg(feature = "local-inference")]
mod cpu {
    use super::*;
    use graphicle_runtime::model;

    fn profile(variable: &str) -> model::Profile {
        let dir = std::path::PathBuf::from(
            std::env::var(variable).expect("explicit local model directory"),
        );
        let manifest: Value =
            serde_json::from_slice(&std::fs::read(dir.join("MODEL.json")).unwrap()).unwrap();
        serde_json::from_value(json!({"protocol":"local_onnx","base_url":url::Url::from_directory_path(dir).unwrap().as_str(),
            "model":manifest["model"],"input_limit":1000})).unwrap()
    }

    #[test]
    #[ignore = "requires GRAPHICLE_LOCAL_RERANKER and ORT_DYLIB_PATH"]
    fn deep_rerank_reads_the_fused_tail_once_per_text_and_within_origin_caps() {
        let reranker = profile("GRAPHICLE_LOCAL_RERANKER");
        let f = Fixture::new();
        let context = |options: Value| {
            f.store
                .project_context_local(
                    &f.workspace,
                    &f.reader,
                    &f.args(options),
                    None,
                    Some(&reranker),
                )
                .unwrap()
        };
        let candidates = |packet: &Value| packet["local_reranker"]["candidates"].as_u64().unwrap();
        // Distinct texts, at most two passages per origin: the seventy notes
        // and two of the manual's five sections. Copies never reach the model.
        let eligible = (NOTES + 2) as u64;
        let shallow = context(json!({}));
        assert_eq!(candidates(&shallow), 32, "default depth");
        let diagnosed = context(json!({"debug_candidates":400}));
        assert_eq!(
            candidates(&diagnosed),
            32,
            "diagnostics do not deepen reranking"
        );
        let ranked = context(json!({"rerank_depth":60}));
        assert_eq!(candidates(&ranked), 60, "rank's own list");
        for depth in [100, MAX_RERANK_DEPTH] {
            let deep = context(json!({"rerank_depth":depth}));
            assert_eq!(candidates(&deep), eligible, "depth {depth}: {deep}");
            assert!(
                deep.get("candidates_debug").is_none(),
                "deep reranking must not expose fused diagnostics"
            );
            let sources = deep["sources"].as_array().unwrap();
            assert!(!sources.is_empty() && sources.len() <= 8);
        }
        let traced = context(json!({"rerank_depth":100,"debug_candidates":5}));
        assert_eq!(traced["candidates_debug"].as_array().unwrap().len(), 5);
        assert_eq!(candidates(&traced), eligible);
    }
}
