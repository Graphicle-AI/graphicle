//! Server retrieval defaults: unset options take `query::recipe_defaults`
//! (stemming, the automatic speaker arm and focus packing), the packet reports
//! which ones did, and an explicit `false` turns each off. Per-origin quotas
//! and the entity link (`graph`) arm stay opt-in.
use graphicle_memory::text;
use graphicle_runtime::{
    query,
    store::{Store, s},
};
use serde_json::{Value, json};
use tempfile::TempDir;

const QUESTION: &str = "When does the canary rollout pause?";
const LABELS: [&str; 5] = ["alpha", "bravo", "charlie", "delta", "echo"];

/// One note whose five canary sections sit between unrelated sections, each
/// long enough to be its own passage, so retrieved windows never merge.
fn runbook() -> String {
    let filler = |topic: &str| {
        format!(
            "## {topic}\n\n{}\n\n",
            "Operators log the batch identifier in the ledger. ".repeat(56)
        )
    };
    let mut note = String::from("# Runbook\n\n");
    for (i, label) in LABELS.iter().enumerate() {
        note.push_str(&format!(
            "## Gate {label}\n\nThe canary rollout pauses at the {label} gate when error budget burn exceeds its limit. {}\n\n",
            "Operators log the batch identifier in the ledger. ".repeat(52)
        ));
        note.push_str(&filler(&format!("Ledger {i}")));
    }
    note
}

struct Fixture {
    _dir: TempDir,
    store: Store,
    workspace: String,
    key: String,
    repo: String,
}
impl Fixture {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let ws = store.create_workspace("Retrieval defaults").unwrap();
        let workspace = s(&ws, "id").to_string();
        let key = s(&ws, "capability").to_string();
        let repo = s(
            &store
                .register(
                    &workspace,
                    &key,
                    &json!({"project_key":"defaults","name":"Defaults"}),
                )
                .unwrap(),
            "id",
        )
        .to_string();
        let f = Self {
            _dir: dir,
            store,
            workspace,
            key,
            repo,
        };
        let files = [
            ("runbook.md".to_owned(), runbook()),
            (
                "budget.md".to_owned(),
                "# Budget\n\nCloud spend is capped at forty thousand dollars a month.\n".to_owned(),
            ),
        ];
        let state = f
            .store
            .project_status(&f.workspace, &f.key, &f.args(json!({})))
            .unwrap();
        let begin = f
            .store
            .sync_prepare(
                &f.workspace,
                &f.key,
                &f.args(json!({"expected_snapshot":state["repository"]["snapshot"],
                    "manifest":files.iter().map(|(name,text)|json!({"path":name,"sha256":text::sha256(text.as_bytes()),"bytes":text.len()})).collect::<Vec<_>>()})),
            )
            .unwrap();
        f.store
            .sync_upload(
                &f.workspace,
                &f.key,
                &f.args(json!({"sync_id":begin["sync_id"],
                    "files":files.iter().map(|(name,text)|json!({"path":name,"content":text})).collect::<Vec<_>>()})),
            )
            .unwrap();
        f.store
            .sync_commit(
                &f.workspace,
                &f.key,
                &f.args(json!({"sync_id":begin["sync_id"]})),
            )
            .unwrap();
        f
    }
    fn args(&self, mut args: Value) -> Value {
        args["repository_id"] = json!(self.repo);
        args
    }
    fn context(&self, options: Value) -> graphicle_runtime::Result<Value> {
        let mut args = json!({"question":QUESTION,"limit":8,"max_tokens":8000});
        for (key, value) in options.as_object().unwrap() {
            args[key] = value.clone();
        }
        self.store
            .project_context(&self.workspace, &self.key, &self.args(args))
    }
}

/// Canary gates whose sentence reached the packet.
fn gates(packet: &Value) -> usize {
    let text: String = packet["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["text"].as_str().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n");
    LABELS
        .iter()
        .filter(|label| text.contains(&format!("pauses at the {label} gate")))
        .count()
}

#[test]
fn unset_options_take_the_reported_defaults() {
    let f = Fixture::new();
    let packet = f.context(json!({})).unwrap();
    assert_eq!(packet["retrieval_defaults"], query::recipe_defaults());
    assert_eq!(packet["stemmer"], "porter");
    assert!(packet.get("origin_quota").is_none());
    // A null option counts as unset.
    let nulls = f
        .context(json!({"stem":null,"speaker":null,"pack_focus":null,"origin_quota":null}))
        .unwrap();
    assert_eq!(nulls["retrieval_defaults"], query::recipe_defaults());
    // Options the caller sets are not reported as defaults.
    let explicit = f.context(query::recipe_defaults()).unwrap();
    assert_eq!(explicit["retrieval_defaults"], json!({}));
    assert_eq!(gates(&explicit), gates(&packet));
}

#[test]
fn explicit_false_turns_each_default_off() {
    let f = Fixture::new();
    let off = f
        .context(json!({"stem":false,"speaker":false,"pack_focus":false,"origin_quota":false}))
        .unwrap();
    assert_eq!(off["retrieval_defaults"], json!({}));
    assert!(off.get("stemmer").is_none());
    assert!(off.get("origin_quota").is_none());
    assert!(off.get("speaker").is_none());
    let partial = f.context(json!({"stem":false})).unwrap();
    let mut expected = query::recipe_defaults();
    expected.as_object_mut().unwrap().remove("stem");
    assert_eq!(partial["retrieval_defaults"], expected);
    assert!(partial.get("stemmer").is_none());
}

#[test]
fn origin_quota_is_opt_in_and_false_keeps_the_flat_cap() {
    let f = Fixture::new();
    let unset = f.context(json!({"pack_focus":false})).unwrap();
    let off = f
        .context(json!({"pack_focus":false,"origin_quota":false}))
        .unwrap();
    let on = f
        .context(json!({"pack_focus":false,"origin_quota":true}))
        .unwrap();
    // Unset and false keep the flat cap of two excerpts per origin.
    assert!(unset.get("origin_quota").is_none());
    assert_eq!(gates(&unset), 2);
    assert_eq!(gates(&off), 2);
    // One origin: the quota's maximum of three, then leftover slots from the
    // same origin.
    assert_eq!(on["origin_quota"]["origins"], 1);
    assert!(gates(&on) > 2, "quota packet: {}", gates(&on));
}

#[test]
fn malformed_origin_quota_is_rejected() {
    let f = Fixture::new();
    for bad in [json!(3), json!("3"), json!([3, 1])] {
        assert!(f.context(json!({"origin_quota":bad})).is_err(), "{bad}");
    }
    assert!(f.context(json!({"origin_quota":true})).is_ok());
}

#[test]
fn pack_focus_takes_stem_and_context_options() {
    let f = Fixture::new();
    for good in [
        json!({}),
        json!({"stem":true}),
        json!({"stem":false,"context":0}),
        json!({"context":8}),
    ] {
        let packet = f.context(json!({"pack_focus":good})).unwrap();
        assert!(
            packet["retrieval_defaults"].get("pack_focus").is_none(),
            "{good}"
        );
    }
    for bad in [
        json!("yes"),
        json!(1),
        json!({"context":9}),
        json!({"context":-1}),
        json!({"stem":1}),
        json!({"span":2}),
    ] {
        assert!(f.context(json!({"pack_focus":bad})).is_err(), "{bad}");
    }
}
