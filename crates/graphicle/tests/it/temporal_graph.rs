//! The bitemporal timeline reads of `graph.as_of` (`valid_at`, `known_at`,
//! `branch`), historical aliases and hyperedges. Rows that the earlier
//! assertion writers (`graph.observe`, `graph.hyperedge`, `graph.commit`; see
//! unit `git-ancestry-writes`) recorded in older stores are seeded
//! directly, as those writers stored them.
use graphicle::graph::Graph;
use graphicle_runtime::{
    graph_projection,
    store::{Store, s},
    temporal_graph,
};
use rusqlite::params;
use serde_json::{Value, json};

struct Fixture {
    _dir: tempfile::TempDir,
    store: Store,
    workspace: String,
    key: String,
    repo: String,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let ws = store.create_workspace("Temporal proof").unwrap();
        let workspace = s(&ws, "id").to_string();
        let key = s(&ws, "capability").to_string();
        let repo = store
            .register(
                &workspace,
                &key,
                &json!({"project_key":"temporal","name":"Temporal"}),
            )
            .unwrap();
        Self {
            _dir: dir,
            store,
            workspace,
            key,
            repo: s(&repo, "id").into(),
        }
    }
    fn episode(&self, content: &str) -> String {
        self.store.transaction(|db| {
            let sha=graphicle_memory::text::sha256(content.as_bytes());
            db.execute("INSERT OR IGNORE INTO repo_objects VALUES(?,?,?,?)",params![self.repo,sha,content,content.len() as i64])?;
            db.execute("INSERT OR REPLACE INTO repo_heads VALUES(?,?,?)",params![self.repo,"a.py",sha])?;
            let episode=graph_projection::current_episode(db,&self.repo)?;
            let graph=Graph::from_node_link(&json!({"directed":true,"multigraph":true,"nodes":[{"id":"a","label":content,"stable_id":"a"},{"id":"b","label":"b","stable_id":"b"}],"links":[{"source":"a","target":"b","key":"calls","relation":"calls"}]}),"links")?;
            graph_projection::persist(db,&self.repo,&episode,&graph)?;Ok(episode)
        }).unwrap()
    }
    fn view(&self, args: Value) -> Value {
        self.store
            .graph_as_of(&self.workspace, &self.key, &self.repo, &args)
            .unwrap()
    }
}

#[test]
fn retroactive_correction_preserves_what_was_known_and_branch_scope() {
    // A later assertion corrects the same valid time; what was known before
    // it was recorded stays readable.
    let f = Fixture::new();
    let a = f.episode("def a(): return 1\n");
    let b = f.episode("def a(): return 2\n");
    let (first, second) = ("2021-01-01T00:00:00Z", "2021-01-02T00:00:00Z");
    let at = |s| temporal_graph::instant(s).unwrap();
    f.store
        .transaction(|db| {
            for (episode, recorded) in [(&a, first), (&b, second)] {
                temporal_graph::observe(
                    db,
                    &f.repo,
                    episode,
                    "release",
                    at("2020-01-01T00:00:00Z"),
                    at(recorded),
                    "explicit_assertion",
                )?;
            }
            Ok(())
        })
        .unwrap();
    assert_eq!(
        f.view(json!({"branch":"release","valid_at":"2020-01-02T00:00:00Z","known_at":first}))["episode"],
        a
    );
    assert_eq!(
        f.view(json!({"branch":"release","valid_at":"2020-01-02T00:00:00Z","known_at":second}))["episode"],
        b
    );
    assert!(
        f.store
            .graph_as_of(
                &f.workspace,
                &f.key,
                &f.repo,
                &json!({"branch":"other","valid_at":"2020-01-02T00:00:00Z"})
            )
            .is_err()
    );
    assert!(
        f.store
            .graph_as_of(
                &f.workspace,
                &f.key,
                &f.repo,
                &json!({"as_of":a,"known_at":first})
            )
            .is_err()
    );
}

#[test]
fn recurring_graphs_preserve_the_gap_and_exact_time_zone_boundaries() {
    // Graphiti #1865/#1864: recurrence must not reuse an ended occurrence,
    // and an equivalent UTC offset must select the same boundary.
    let f = Fixture::new();
    let a = f.episode("def a(): return 1\n");
    let b = f.episode("def a(): return 2\n");
    let at = |s| temporal_graph::instant(s).unwrap();
    f.store
        .transaction(|db| {
            for (episode, valid, known) in [
                (&a, "2020-01-01T00:00:00Z", "2020-01-01T00:00:00Z"),
                (&b, "2020-01-10T00:00:00Z", "2020-01-12T00:00:00Z"),
                (&a, "2020-01-20T00:00:00Z", "2020-01-22T00:00:00Z"),
            ] {
                temporal_graph::observe(
                    db,
                    &f.repo,
                    episode,
                    "recurrence",
                    at(valid),
                    at(known),
                    "fixture",
                )?;
            }
            Ok(())
        })
        .unwrap();
    for (valid, expected) in [
        ("2020-01-05T00:00:00Z", &a),
        ("2020-01-19T23:59:59.999999Z", &b),
        ("2020-01-20T00:00:00Z", &a),
        ("2020-01-20T05:30:00+05:30", &a),
    ] {
        assert_eq!(
            f.view(json!({"branch":"recurrence", "valid_at":valid,
            "known_at":"2020-02-01T00:00:00Z"}))["episode"],
            *expected
        );
    }
    assert_eq!(
        f.view(
            json!({"branch":"recurrence", "valid_at":"2020-01-25T00:00:00Z",
        "known_at":"2020-01-21T23:59:59.999999Z"})
        )["episode"],
        b
    );
}

#[test]
fn historical_aliases_and_hyperedges_survive_retraction_without_future_leakage() {
    let f = Fixture::new();
    f.episode("def a(): return 1\n");
    let at = temporal_graph::instant("2020-01-01T00:00:00Z").unwrap();
    f.store
        .transaction(|db| {
            let proposal = json!({"id":"p","from_node":"a","to_node":"b"});
            temporal_graph::alias_event(db, &f.repo, &proposal, true, at, at + 10)?;
            temporal_graph::alias_event(db, &f.repo, &proposal, false, at, at + 20)?;
            assert!(temporal_graph::aliases_at(db, &f.repo, at, at + 9)?.is_empty());
            assert!(!temporal_graph::aliases_at(db, &f.repo, at, at + 10)?.is_empty());
            assert!(temporal_graph::aliases_at(db, &f.repo, at, at + 20)?.is_empty());
            Ok(())
        })
        .unwrap();
    // A hyperedge created, then retracted, at the same valid time.
    let (created, retracted) = (at + 10, at + 20);
    f.store
        .transaction(|db| {
            let episode = graph_projection::current_episode(db, &f.repo)?;
            for (active, recorded) in [(true, created), (false, retracted)] {
                db.execute(
                    "INSERT INTO gf_hyperedges(repository_id,hyperedge_id,episode,relation,members,attributes,active,valid_us,recorded_us) VALUES(?,?,?,?,?,?,?,?,?)",
                    params![f.repo, "contract", episode, "requires", r#"["a","b"]"#, "{}", active, at, recorded],
                )?;
            }
            let graph = graph_projection::load(db, &f.repo, &episode)?;
            let known = |recorded| temporal_graph::hyperedges_at(db, &f.repo, at, recorded, &graph);
            assert!(known(created - 1)?.0.is_empty());
            assert_eq!(known(created)?.0.len(), 1);
            assert!(known(retracted)?.0.is_empty());
            Ok(())
        })
        .unwrap();
}

#[test]
fn malformed_temporal_selectors_are_rejected_and_hyperedges_report_truncation() {
    let f = Fixture::new();
    let episode = f.episode("def a(): return 1\n");
    for args in [
        json!({"valid_at":1}),
        json!({"known_at":null}),
        json!({"branch":1}),
        json!({"branch":""}),
        json!({"canonical":"yes"}),
        json!({"as_of":false}),
        json!({"valid_at":"2020-01-01"}),
        json!({"branch":"bad\nbranch"}),
    ] {
        assert!(
            f.store
                .graph_as_of(&f.workspace, &f.key, &f.repo, &args)
                .is_err(),
            "{args}"
        );
    }
    f.store.transaction(|db| {
        for i in 0..1001 {
            db.execute("INSERT INTO gf_hyperedges(repository_id,hyperedge_id,episode,relation,members,attributes,active,valid_us,recorded_us) VALUES(?,?,?,?,?,?,1,0,0)",
                params![f.repo,format!("h{i:04}"),episode,"requires",r#"["a","b"]"#,"{}"])?;
        }
        Ok(())
    }).unwrap();
    let view = f.view(json!({}));
    assert_eq!(view["hyperedges"].as_array().unwrap().len(), 1000);
    assert_eq!(view["hyperedges_truncated"], true);
    assert_eq!(view["hyperedges"][0]["unresolved_members"], json!([]));
    assert_eq!(view["hyperedges"][0]["hyperedge_id"], "h0000");
}
