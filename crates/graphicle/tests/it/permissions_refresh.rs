//! Provider permissions in Core, which enforces them on read but does not
//! sync them (the sync is a the commercial edition extension, tested with it); portable
//! backups; model reply envelopes.
use graphicle_runtime::{
    api::{self, App},
    jobs::Worker,
    model,
    store::{Store, s},
};
use serde_json::json;

/// Installs the edition under test before a fixture is built: nothing in this
/// Core test binary. An extension crate can re-run this suite from its own
/// test binary, whose root `edition_setup` installs its extensions (`crate::`
/// names the root of whichever test binary the suite is compiled into).
#[allow(dead_code)]
fn edition_setup() {}

/// Whether the edition under test is the `pro` edition (`build.info` edition).
fn pro_edition() -> bool {
    crate::edition_setup();
    graphicle_runtime::core_capabilities::capabilities()["edition"] == "pro"
}

/// A store with a GitHub connector in provider permission mode, as the
/// commercial edition's program creates it (Core refuses to create one).
fn provider_store() -> (tempfile::TempDir, Store, String, String, String, String) {
    crate::edition_setup();
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let ws = store.create_workspace("Company").unwrap();
    let (w, key) = (s(&ws, "id").to_string(), s(&ws, "capability").to_string());
    let repo = store
        .register(
            &w,
            &key,
            &json!({"name":"Private project","project_key":"private"}),
        )
        .unwrap();
    let r = s(&repo, "id").to_string();
    let connector=store.connector_add(&w,&key,&json!({"repository_id":r,"name":"Fixture","kind":"jsonl","config":{"acl":{"workspace_readers":true,"credentials":[]}}})).unwrap();
    let credential = store
        .put_credential(
            &w,
            &key,
            &json!({"name":"Fixture token","value":"fixture-only-access"}),
        )
        .unwrap();
    let cfg = json!({"repository":"org/project","permission_mode":"provider","permission_max_age_seconds":300,"sync_every_seconds":900,"acl":{"workspace_readers":true,"credentials":[]}});
    // Never synced, so a scheduler would queue its first sync.
    store.connect().unwrap().execute("UPDATE am_connectors SET kind='github',config=?,credential_env=?,last_sync=NULL WHERE id=?",rusqlite::params![cfg.to_string(),s(&credential,"credential_ref"),s(&connector,"id")]).unwrap();
    let connector = s(&connector, "id").to_string();
    (dir, store, w, key, r, connector)
}

/// Core neither creates nor syncs provider-mode connectors: it says that
/// this needs the commercial edition, and work that edition's program queued
/// in the same store stays queued for it instead of failing (never leased
/// here). With that edition the mode is accepted and nothing waits.
#[tokio::test]
async fn provider_permission_work_waits_for_graphicle_pro() {
    let (_dir, store, w, key, r, connector) = provider_store();
    let info = |store: &Store| {
        api::command(
            &App::new(store.clone(), model::Config::default(), vec![]).unwrap(),
            &w,
            &key,
            "build.info",
            &json!({}),
        )
        .unwrap()
    };
    let added = store
        .connector_add(&w,&key,&json!({"repository_id":r,"name":"New","kind":"github","credential_env":"GRAPHICLE_FIXTURE_CREDENTIAL","config":{"repository":"org/other","permission_mode":"provider","acl":{"workspace_readers":true,"credentials":[]}}}));
    if pro_edition() {
        added.unwrap();
        let listed = store
            .connector_list(&w, &key, &json!({"repository_id":r}))
            .unwrap();
        for connector in listed["connectors"].as_array().unwrap() {
            assert!(
                connector.get("provider_permissions_unavailable").is_none(),
                "{connector}"
            );
        }
        assert_eq!(info(&store)["provider_permission_sync"], true);
        return;
    }
    let refused = added.unwrap_err();
    assert!(
        refused.message.contains("is not part of Graphicle Core"),
        "{}",
        refused.message
    );
    let config = model::Config::default();
    for kind in ["permissions", "connector"] {
        let refused = store
            .enqueue(
                &w,
                &key,
                &json!({"repository_id":r,"kind":kind,"connector_id":connector}),
                &config,
            )
            .unwrap_err();
        assert_eq!(
            refused.message,
            "Provider permission sync is not part of Graphicle Core"
        );
    }
    assert_eq!(
        store.schedule_connectors().unwrap(),
        0,
        "nothing to schedule here"
    );
    let listed = store
        .connector_list(&w, &key, &json!({"repository_id":r}))
        .unwrap();
    assert_eq!(
        listed["connectors"][0]["provider_permissions_unavailable"],
        "Provider permission sync is not part of Graphicle Core"
    );
    // Work the commercial edition's program queued in this store.
    let owner = graphicle_memory::text::sha256(key.as_bytes());
    let stamp = graphicle_runtime::now();
    for kind in ["permissions", "connector"] {
        let payload = json!({"kind":kind,"repository_id":r,"connector_id":connector,"max_requests":64,"max_calls":0,"scheduled":true});
        store.connect().unwrap().execute("INSERT INTO am_jobs(id,repository_id,kind,input_hash,payload,principal_hash,state,priority,max_attempts,not_before,created_at,updated_at) VALUES(?,?,?,?,?,?,'queued',10,1,?,?,?)",rusqlite::params![format!("pro-{kind}"),r,kind,format!("input-{kind}"),payload.to_string(),owner,stamp,stamp,stamp]).unwrap();
    }
    let worker = Worker::new(store.clone(), config).unwrap();
    assert!(!worker.once().await.unwrap(), "nothing is leased");
    let status = store
        .job_status(&w, &key, &json!({"repository_id":r}))
        .unwrap();
    let jobs = status["jobs"].as_array().unwrap();
    assert_eq!(jobs.len(), 2, "{status}");
    for job in jobs {
        assert_eq!(job["state"], "queued", "{job}");
        assert_eq!(
            job["waiting_for"],
            "Provider permission sync is not part of Graphicle Core"
        );
    }
    assert_eq!(info(&store)["provider_permission_sync"], false);
}

#[test]
fn portable_backup_excludes_encrypted_credentials_and_pauses_their_connections() {
    crate::edition_setup();
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("store")).unwrap();
    let ws = store.create_workspace("Backup").unwrap();
    let w = s(&ws, "id");
    let key = s(&ws, "capability");
    let repo = store
        .register(w, key, &json!({"name":"Project","project_key":"project"}))
        .unwrap();
    let credential = store
        .put_credential(
            w,
            key,
            &json!({"name":"Access","value":"private-fixture-secret"}),
        )
        .unwrap();
    store.connector_add(w,key,&json!({"repository_id":repo["id"],"name":"GitHub","kind":"github","credential_ref":credential["credential_ref"],"config":{"repository":"org/project","acl":{"workspace_readers":false,"credentials":[]}}})).unwrap();
    let file = dir.path().join("backup.sqlite3");
    store.backup(&file).unwrap();
    let restored = rusqlite::Connection::open(file).unwrap();
    assert_eq!(
        restored
            .query_row("SELECT count(*) FROM am_secrets", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        restored
            .query_row("SELECT state FROM am_connectors", [], |r| r
                .get::<_, String>(0))
            .unwrap(),
        "paused"
    );
    assert_eq!(
        store
            .connect()
            .unwrap()
            .query_row("SELECT count(*) FROM am_secrets", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
}

#[test]
fn envelope_recovery_preserves_every_field_and_never_fills_missing_values() {
    let reply = model::Reply {
        text: r#"[{"speaker":"Mira","date":null}]"#.into(),
        input_tokens: 1,
        output_tokens: 1,
        usage_reported: true,
        elapsed_ms: 1,
        model: "fixture".into(),
    };
    let schema = json!({"type":"object","properties":{"observations":{"type":"array","items":{"type":"object"}}},"required":["observations"]});
    assert_eq!(
        reply.json_for(&schema).unwrap(),
        json!({"observations":[{"speaker":"Mira","date":null}]})
    );
    assert_eq!(reply.json_for(&json!({"type":"object","properties":{"answer":{"type":"string"},"citations":{"type":"array"}}})).unwrap(),reply.json().unwrap());
    // An array or object property sent as its JSON text is decoded; a string
    // property is never touched, and text of the wrong type stays as sent.
    let encoded = model::Reply {
        text: r#"{"operations":"[{\"op\":\"remove_section\",\"section_id\":\"a\"}]","evidence":[],"note":"[1]","detail":"[2]"}"#.into(),
        ..reply.clone()
    };
    let schema = json!({"type":"object","properties":{"operations":{"type":"array"},"evidence":{"type":"array"},
        "note":{"type":"string"},"detail":{"type":"object"}}});
    assert_eq!(
        encoded.json_for(&schema).unwrap(),
        json!({"operations":[{"op":"remove_section","section_id":"a"}],"evidence":[],"note":"[1]","detail":"[2]"})
    );
}
