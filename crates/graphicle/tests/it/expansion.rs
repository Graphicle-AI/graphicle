use axum::{
    Json, Router,
    extract::{Form, State},
    routing::{get, post},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use graphicle_runtime::{
    documents, model, oauth, permissions,
    store::{Store, s},
};
use serde_json::{Value, json};
use std::{
    io::{Cursor, Write},
    sync::{Arc, Mutex},
};

/// Installs the edition under test before a fixture is built: nothing in this
/// Core test binary. An extension crate can re-run this suite from its own
/// test binary, whose root `edition_setup` installs its extensions (`crate::`
/// names the root of whichever test binary the suite is compiled into).
#[allow(dead_code)]
fn edition_setup() {}

fn fixture() -> (tempfile::TempDir, Store, String, String, String) {
    crate::edition_setup();
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let workspace = store.create_workspace("Expansion test").unwrap();
    let ws = s(&workspace, "id").to_string();
    let token = s(&workspace, "capability").to_string();
    let repo = store
        .register(
            &ws,
            &token,
            &json!({"project_key":"expansion","name":"Expansion"}),
        )
        .unwrap();
    let project = s(&repo, "id").to_string();
    (dir, store, ws, token, project)
}
fn office(parts: &[(&str, &str)]) -> Vec<u8> {
    let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for (name, contents) in parts {
        zip.start_file(*name, zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(contents.as_bytes()).unwrap();
    }
    zip.finish().unwrap().into_inner()
}
#[test]
fn docx_preserves_order_entities_and_omits_deleted_text() {
    let bytes = office(&[(
        "word/document.xml",
        "<w:document xmlns:w='w'><w:p><w:r><w:t>Mira &amp; Arun</w:t></w:r></w:p><w:del><w:r><w:delText>obsolete entrance</w:delText></w:r></w:del><w:p><w:r><w:t>Use the west entrance.</w:t></w:r></w:p></w:document>",
    )]);
    let result = documents::extract("plan.docx", &bytes).unwrap();
    let text = s(&result, "text");
    assert!(text.contains("Mira & Arun"));
    assert!(text.contains("west entrance"));
    assert!(!text.contains("obsolete"));
    assert_eq!(
        result["original_sha256"],
        graphicle_memory::text::sha256(&bytes)
    );
}
#[test]
fn pptx_uses_numeric_slide_order() {
    let bytes = office(&[
        ("ppt/slides/slide10.xml", "<p>tenth</p>"),
        ("ppt/slides/slide2.xml", "<p>second</p>"),
        ("ppt/slides/slide1.xml", "<p>first</p>"),
    ]);
    let result = documents::extract("deck.pptx", &bytes).unwrap();
    let text = s(&result, "text");
    assert!(text.find("first").unwrap() < text.find("second").unwrap());
    assert!(text.find("second").unwrap() < text.find("tenth").unwrap());
}
#[test]
fn rejects_external_entities_and_archive_traversal() {
    assert!(
        documents::xml_text(b"<!DOCTYPE x SYSTEM 'file:///etc/passwd'><x>&secret;</x>").is_err()
    );
    let bytes = office(&[("../word/document.xml", "<p>escape</p>")]);
    assert!(documents::extract("bad.docx", &bytes).is_err());
}
#[test]
fn binary_original_follows_permissions_and_cascade_deletion() {
    let (_dir, store, ws, token, project) = fixture();
    let connector=store.connector_add(&ws,&token,&json!({"repository_id":project,"kind":"jsonl","name":"Documents","config":{"acl":{"workspace_readers":false,"credentials":[]}}})).unwrap();
    let bytes = office(&[(
        "word/document.xml",
        "<document><p>Use the west entrance.</p></document>",
    )]);
    let result=store.import_binary(&ws,&token,&json!({"repository_id":project,"connector_id":connector["id"],"name":"plan.docx","base64":STANDARD.encode(&bytes)})).unwrap();
    let original = store
        .original_document(
            &ws,
            &token,
            &json!({"repository_id":project,"source_id":result["source_id"]}),
        )
        .unwrap();
    assert_eq!(STANDARD.decode(s(&original, "base64")).unwrap(), bytes);
    let reader = store
        .grant(&ws, &token, &json!({"role":"reader","label":"Reader"}))
        .unwrap();
    assert!(
        store
            .original_document(
                &ws,
                s(&reader, "capability"),
                &json!({"repository_id":project,"source_id":result["source_id"]})
            )
            .is_err()
    );
    store
        .maintain(
            &ws,
            &token,
            "connector.delete",
            &json!({"repository_id":project,"connector_id":connector["id"]}),
        )
        .unwrap();
    let remaining: i64 = store
        .connect()
        .unwrap()
        .query_row("SELECT count(*) FROM am_original_documents", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(remaining, 0);
}
#[test]
fn secrets_are_encrypted_and_workspace_bound() {
    let (_dir, store, ws, token, project) = fixture();
    let secret = store
        .put_credential(
            &ws,
            &token,
            &json!({"name":"test-source","value":"TEST-ONLY-credential-12345"}),
        )
        .unwrap();
    let ciphertext: Vec<u8> = store
        .connect()
        .unwrap()
        .query_row(
            "SELECT ciphertext FROM am_secrets WHERE id=?",
            [s(&secret, "id")],
            |r| r.get(0),
        )
        .unwrap();
    let needle = b"TEST-ONLY-credential-12345";
    assert!(!ciphertext.windows(needle.len()).any(|w| w == needle));
    let other = store.create_workspace("Another workspace").unwrap();
    let repo = store
        .register(
            s(&other, "id"),
            s(&other, "capability"),
            &json!({"project_key":"other","name":"Other"}),
        )
        .unwrap();
    let config =
        json!({"repository":"org/repo","acl":{"workspace_readers":false,"credentials":[]}});
    assert!(store.connector_add(s(&other,"id"),s(&other,"capability"),&json!({"repository_id":repo["id"],"name":"wrong workspace","kind":"github","config":config,"credential_ref":secret["credential_ref"]})).is_err());
    let c=store.connector_add(&ws,&token,&json!({"repository_id":project,"name":"source","kind":"github","config":config,"credential_ref":secret["credential_ref"]})).unwrap();
    store.remove_credential(&ws, &token, &secret).unwrap();
    assert_eq!(
        store
            .connector_list(&ws, &token, &json!({"repository_id":project}))
            .unwrap()["connectors"][0]["id"],
        c["id"]
    );
    assert_eq!(
        store
            .connector_list(&ws, &token, &json!({"repository_id":project}))
            .unwrap()["connectors"][0]["state"],
        "paused"
    );
}
#[test]
fn provider_membership_expiry_and_revocation_change_recall() {
    let (_dir, store, ws, token, project) = fixture();
    let reader = store
        .grant(&ws, &token, &json!({"role":"reader","label":"Teammate"}))
        .unwrap();
    // A verified mapping, as the commercial edition's provider.identity.map records it.
    store
        .connect()
        .unwrap()
        .execute(
            "INSERT INTO am_provider_identities VALUES(?,?,?,?,?)",
            rusqlite::params![
                ws,
                "slack",
                "U123",
                s(&reader, "id"),
                graphicle_runtime::now()
            ],
        )
        .unwrap();
    store.authorized(&ws,s(&reader,"capability"),"read",|db,p|{
        let current=json!({"complete":true,"checked_at":graphicle_runtime::now(),"principals":[{"type":"user","id":"U123"}]});assert!(permissions::allows(db,&ws,"slack",&current,p,300)?);
        assert!(!permissions::allows(db,&ws,"slack",&json!({"complete":true,"checked_at":graphicle_runtime::now(),"principals":[]}),p,300)?);
        assert!(!permissions::allows(db,&ws,"slack",&json!({"complete":true,"checked_at":"2000-01-01T00:00:00Z","principals":[{"type":"user","id":"U123"}]}),p,300)?);
        Ok(())
    }).unwrap();
    let _ = project;
}
#[derive(Clone, Default)]
struct AuthState(Arc<Mutex<Vec<Value>>>);
async fn exchange(
    State(state): State<AuthState>,
    Form(form): Form<std::collections::BTreeMap<String, String>>,
) -> Json<Value> {
    state.0.lock().unwrap().push(json!(form));
    Json(
        json!({"access_token":"test-provider-access","refresh_token":"test-provider-refresh","expires_in":3600,"scope":"read"}),
    )
}
async fn identity() -> Json<Value> {
    Json(json!({"id":123,"login":"fixture-user"}))
}
#[tokio::test]
async fn oauth_pkce_roundtrip_and_replay_rejection() {
    let (_dir, store, ws, token, _project) = fixture();
    let state = AuthState::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let router = Router::new()
        .route("/token", post(exchange))
        .route("/user", get(identity))
        .with_state(state.clone());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    oauth::configure(&store,&ws,&token,&json!({"provider":"github","config":{"client_id":"test-client","client_secret_ref":null,"authorization_url":format!("{base}/authorize"),"token_url":format!("{base}/token"),"identity_url":format!("{base}/user"),"scopes":["read"],"pkce":true}})).unwrap();
    let started = oauth::start(
        &store,
        &ws,
        &token,
        "github",
        &format!("{base}/oauth/callback/github"),
    )
    .unwrap();
    let url = url::Url::parse(s(&started, "authorization_url")).unwrap();
    let params: std::collections::BTreeMap<_, _> = url.query_pairs().into_owned().collect();
    let result = oauth::callback(
        &store,
        &model::client().unwrap(),
        "github",
        &params["state"],
        "test-code",
    )
    .await
    .unwrap();
    assert_eq!(result["connected"], true);
    assert!(
        oauth::callback(
            &store,
            &model::client().unwrap(),
            "github",
            &params["state"],
            "test-code"
        )
        .await
        .is_err()
    );
    let requests = state.0.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0]["code"], "test-code");
    use sha2::{Digest, Sha256};
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(Sha256::digest(s(&requests[0], "code_verifier").as_bytes()));
    assert_eq!(challenge, params["code_challenge"]);
    drop(requests);
    oauth::disconnect(&store, &ws, &token, s(&result, "account_id")).unwrap();
    assert!(
        oauth::status(&store, &ws, &token).unwrap()["accounts"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    server.abort();
}
#[test]
fn expanded_grammars_find_real_java_and_go_definitions() {
    let java = graphicle_memory::structure::analyze(
        "Service.java",
        "class Service { public int count() { return 3; } }",
    )
    .unwrap();
    assert_eq!(java["language"], "java");
    assert!(
        java["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["name"].as_str().unwrap_or("").contains("count"))
    );
    let go = graphicle_memory::structure::analyze(
        "main.go",
        "package main\nfunc count() int { return 3 }\n",
    )
    .unwrap();
    assert!(
        go["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["name"] == "count")
    );
}
