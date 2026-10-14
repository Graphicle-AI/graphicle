//! Stores that the commercial edition has used, opened by this edition. Core never
//! creates the team and commerce tables, but such a store holds them, and
//! Core keeps every guarantee earlier programs kept on it
//! (`graphicle_runtime::store_compat`): revocation deactivates the member,
//! member credentials follow the member and team sign-in (also for queued
//! jobs), portable backups carry no session, token or paid entitlement, and
//! `deletion.state` treats the team's access history as history.
//!
//! The fixture applies the tables as the commercial edition creates them
//! (`tests/fixtures/pro_store_tables.sql`; the commercial edition's tests check the copy against its
//! schema). the commercial edition re-runs this suite with its extensions installed,
//! where the tables already exist and its own hooks apply as well.
use graphicle_runtime::{
    api::{self, App},
    jobs::Worker,
    model,
    store::{Store, s},
};
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use tempfile::TempDir;

/// Installs the edition under test before a fixture is built: nothing in this
/// Core test binary. the commercial edition re-runs this suite from its own test
/// binary, whose root `edition_setup` installs its extensions.
#[allow(dead_code)]
fn edition_setup() {}

const PRO_TABLES: &str = include_str!("../fixtures/pro_store_tables.sql");

struct Fixture {
    _dir: TempDir,
    store: Store,
    workspace: String,
    owner: String,
    repo: String,
}

/// A member as the commercial edition provisions one: a workspace credential linked
/// to an `am_members` row. That edition never hands out the credential's token; the
/// fixture keeps it so the suite can authorize with it directly.
struct Member {
    id: String,
    credential: String,
    capability: String,
}

impl Fixture {
    /// A store with the team and commerce tables, team sign-in enabled.
    fn new() -> Self {
        crate::edition_setup();
        let dir = TempDir::new().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store.connect().unwrap().execute_batch(PRO_TABLES).unwrap();
        let created = store.create_workspace("Edition-used store").unwrap();
        let workspace = s(&created, "id").to_string();
        let owner = s(&created, "capability").to_string();
        let project = store
            .register(
                &workspace,
                &owner,
                &json!({"project_key":"compat","name":"Compat"}),
            )
            .unwrap();
        let fixture = Self {
            _dir: dir,
            store,
            workspace,
            owner,
            repo: s(&project, "id").to_string(),
        };
        fixture.db().execute(
            "INSERT INTO am_sso(workspace_id,config,enabled,updated_at) VALUES(?,'{\"settings\":{}}',1,?)",
            params![fixture.workspace, graphicle_runtime::now()],
        ).unwrap();
        fixture
    }
    fn db(&self) -> Connection {
        self.store.connect().unwrap()
    }
    fn count(&self, sql: &str) -> i64 {
        self.db().query_row(sql, [], |r| r.get(0)).unwrap()
    }
    fn args(&self, mut args: Value) -> Value {
        args["repository_id"] = json!(self.repo);
        args
    }
    /// A member with `role`, active, with one browser session.
    fn member(&self, name: &str, role: &str) -> Member {
        let credential = self
            .store
            .grant(
                &self.workspace,
                &self.owner,
                &json!({"label":name,"role":role}),
            )
            .unwrap();
        let id = format!("member-{name}");
        let stamp = graphicle_runtime::now();
        let db = self.db();
        db.execute(
            "INSERT INTO am_members(id,workspace_id,credential_id,external_id,username,display_name,issuer,subject,active,base_role,created_at,updated_at,deleted_at) VALUES(?,?,?,?,?,?,NULL,NULL,1,?,?,?,NULL)",
            params![id, self.workspace, s(&credential, "id"), format!("subject-{name}"), format!("{name}@example.test"), name, role, stamp, stamp],
        )
        .unwrap();
        db.execute(
            "INSERT INTO am_browser_sessions(token_hash,workspace_id,member_id,created_at,expires_at) VALUES(?,?,?,?,'2999-01-01T00:00:00.000000Z')",
            params![graphicle_memory::text::sha256(format!("session-{name}").as_bytes()), self.workspace, id, stamp],
        )
        .unwrap();
        Member {
            id,
            credential: s(&credential, "id").to_string(),
            capability: s(&credential, "capability").to_string(),
        }
    }
    fn sessions(&self, member: &Member) -> i64 {
        self.db()
            .query_row(
                "SELECT count(*) FROM am_browser_sessions WHERE member_id=?",
                [&member.id],
                |r| r.get(0),
            )
            .unwrap()
    }
    fn active(&self, member: &Member) -> i64 {
        self.db()
            .query_row(
                "SELECT active FROM am_members WHERE id=?",
                [&member.id],
                |r| r.get(0),
            )
            .unwrap()
    }
    fn allowed(&self, member: &Member) -> bool {
        self.store
            .workspace(&self.workspace, &member.capability)
            .is_ok()
    }
    fn state(&self) -> Value {
        let app = App::new(
            self.store.clone(),
            model::Config::default(),
            vec!["localhost".into()],
        )
        .unwrap();
        api::command(
            &app,
            &self.workspace,
            &self.owner,
            "deletion.state",
            &self.args(json!({})),
        )
        .unwrap()
    }
}

#[test]
fn revoking_a_member_credential_deactivates_the_member_and_ends_its_sessions() {
    let f = Fixture::new();
    let ada = f.member("ada", "reader");
    let grace = f.member("grace", "reader");
    assert!(f.allowed(&ada));
    f.store
        .revoke(&f.workspace, &f.owner, &ada.credential)
        .unwrap();
    // As every earlier program did: the commercial edition restores the
    // credential of a member it finds active, so the member must not stay
    // active.
    assert_eq!(f.active(&ada), 0);
    assert_eq!(f.sessions(&ada), 0);
    assert!(!f.allowed(&ada));
    let revoked: bool = f
        .db()
        .query_row(
            "SELECT revoked_at IS NOT NULL FROM credentials WHERE id=?",
            [&ada.credential],
            |r| r.get(0),
        )
        .unwrap();
    assert!(revoked);
    // Other members keep their state.
    assert_eq!(f.active(&grace), 1);
    assert_eq!(f.sessions(&grace), 1);
    assert!(f.allowed(&grace));
}

#[test]
fn a_member_credential_follows_the_member_and_team_sign_in() {
    let f = Fixture::new();
    let ada = f.member("ada", "reader");
    let key = f
        .store
        .grant(
            &f.workspace,
            &f.owner,
            &json!({"label":"agent key","role":"reader"}),
        )
        .unwrap();
    let key_allowed = || {
        f.store
            .workspace(&f.workspace, s(&key, "capability"))
            .is_ok()
    };
    assert!(f.allowed(&ada));
    for (change, undo) in [
        ("UPDATE am_sso SET enabled=0", "UPDATE am_sso SET enabled=1"),
        (
            "UPDATE am_members SET active=0 WHERE id='member-ada'",
            "UPDATE am_members SET active=1 WHERE id='member-ada'",
        ),
        (
            "UPDATE am_members SET deleted_at='2026-01-01T00:00:00.000000Z' WHERE id='member-ada'",
            "UPDATE am_members SET deleted_at=NULL WHERE id='member-ada'",
        ),
    ] {
        f.db().execute_batch(change).unwrap();
        assert!(!f.allowed(&ada), "allowed after {change}");
        // A credential no member owns is not affected.
        assert!(key_allowed(), "agent key refused after {change}");
        f.db().execute_batch(undo).unwrap();
        assert!(f.allowed(&ada), "refused after {undo}");
    }
    // Without a sign-in configuration the member is refused (fail closed).
    f.db()
        .execute("DELETE FROM am_sso WHERE workspace_id=?", [&f.workspace])
        .unwrap();
    assert!(!f.allowed(&ada));
    assert!(key_allowed());
}

#[tokio::test]
async fn a_member_job_is_cancelled_at_lease_once_team_sign_in_is_off() {
    let f = Fixture::new();
    let ada = f.member("ada", "collector");
    // The member syncs a file, which queues a projection job under the
    // member's credential.
    let content = "def authenticate():\n    return True\n";
    let status = f
        .store
        .project_status(&f.workspace, &ada.capability, &f.args(json!({})))
        .unwrap();
    let begin = f
        .store
        .sync_prepare(
            &f.workspace,
            &ada.capability,
            &f.args(json!({"expected_snapshot":status["repository"]["snapshot"],
                "manifest":[{"path":"auth.py","sha256":graphicle_memory::text::sha256(content.as_bytes()),"bytes":content.len()}]})),
        )
        .unwrap();
    f.store
        .sync_upload(
            &f.workspace,
            &ada.capability,
            &f.args(
                json!({"sync_id":begin["sync_id"],"files":[{"path":"auth.py","content":content}]}),
            ),
        )
        .unwrap();
    f.store
        .sync_commit(
            &f.workspace,
            &ada.capability,
            &f.args(json!({"sync_id":begin["sync_id"]})),
        )
        .unwrap();
    let queued = f.count("SELECT count(*) FROM am_jobs WHERE kind='project' AND state='queued'");
    assert_eq!(queued, 1);
    // The owner turns team sign-in off before any worker runs.
    f.db().execute("UPDATE am_sso SET enabled=0", []).unwrap();
    let worker = Worker::new(f.store.clone(), model::Config::default()).unwrap();
    worker.once().await.unwrap();
    let (state, error): (String, Option<String>) = f
        .db()
        .query_row(
            "SELECT state,error FROM am_jobs WHERE kind='project'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(state, "cancelled");
    assert_eq!(error.as_deref(), Some("Credential unavailable"));
}

#[test]
fn portable_backups_carry_no_session_token_or_paid_entitlement() {
    let f = Fixture::new();
    let ada = f.member("ada", "reader");
    let stamp = graphicle_runtime::now();
    let db = f.db();
    db.execute(
        "INSERT INTO am_scim_tokens(workspace_id,token_hash,created_at) VALUES(?,?,?)",
        params![f.workspace, "0".repeat(64), stamp],
    )
    .unwrap();
    db.execute(
        "INSERT INTO am_subscriptions(workspace_id,customer_id,subscription_id,price_id,status,request_allowance,updated_at) VALUES(?,'cus_fixture','sub_fixture','price_fixture','active',100000,?)",
        params![f.workspace, stamp],
    )
    .unwrap();
    db.execute(
        "INSERT INTO am_checkout_intents(workspace_id,id,price_id,expires_at,session_id,url) VALUES(?,'intent','price_fixture',4102444800,NULL,NULL)",
        [&f.workspace],
    )
    .unwrap();
    let file = f.store.directory.join("portable.sqlite3");
    f.store.backup(&file).unwrap();
    let copy = Connection::open(&file).unwrap();
    let count =
        |db: &Connection, sql: &str| -> i64 { db.query_row(sql, [], |r| r.get(0)).unwrap() };
    assert_eq!(count(&copy, "SELECT count(*) FROM am_browser_sessions"), 0);
    assert_eq!(count(&copy, "SELECT count(*) FROM am_scim_tokens"), 0);
    assert_eq!(count(&copy, "SELECT count(*) FROM am_checkout_intents"), 0);
    assert_eq!(
        count(&copy, "SELECT count(*) FROM am_sso WHERE enabled=1"),
        0
    );
    let (status, allowance, customer): (String, i64, String) = copy
        .query_row(
            "SELECT status,request_allowance,customer_id FROM am_subscriptions",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(status, "restore_required");
    assert_eq!(allowance, 0);
    // The customer binding stays, for owner-triggered reconciliation.
    assert_eq!(customer, "cus_fixture");
    // Members and their (unusable) credentials stay; the live store is unchanged.
    assert_eq!(count(&copy, "SELECT count(*) FROM am_members"), 1);
    assert_eq!(f.sessions(&ada), 1);
    assert_eq!(f.count("SELECT count(*) FROM am_scim_tokens"), 1);
    assert_eq!(f.count("SELECT count(*) FROM am_sso WHERE enabled=1"), 1);
    assert_eq!(
        f.count("SELECT count(*) FROM am_subscriptions WHERE status='active'"),
        1
    );
}

#[test]
fn deletion_state_treats_the_team_access_history_as_history() {
    let f = Fixture::new();
    let before = f.state();
    // Sign-ins and the team audit trail are access history, whichever
    // edition reads the store.
    f.member("ada", "reader");
    let after_member = f.state();
    f.db()
        .execute(
            "INSERT INTO am_team_audit(id,workspace_id,actor,action,target_id,data,created_at) VALUES('audit',?,'owner','sso.configured',?,'{}',?)",
            params![f.workspace, f.workspace, graphicle_runtime::now()],
        )
        .unwrap();
    let after = f.state();
    assert_eq!(after["contract"], "graphicle.state.v2");
    // Listed first, in the order the commercial edition declares them, so one store
    // reports the same lists in both editions.
    assert_eq!(
        after["excluded"]["history"].as_array().unwrap()[..3],
        [
            json!("am_team_audit"),
            json!("am_login_pending"),
            json!("am_browser_sessions")
        ],
        "{after}"
    );
    for table in ["am_team_audit", "am_browser_sessions", "am_login_pending"] {
        assert!(
            after["tables"].get(table).is_none(),
            "{table} digested: {after}"
        );
        assert!(
            after["excluded"]["history"]
                .as_array()
                .unwrap()
                .contains(&json!(table)),
            "{table} not listed: {after}"
        );
    }
    // The audit row and the session change nothing; the member and its
    // credential are content.
    assert_eq!(after["digest"], after_member["digest"]);
    assert_ne!(after_member["digest"], before["digest"]);
}

#[test]
fn a_store_without_the_team_tables_names_none_of_them() {
    crate::edition_setup();
    let dir = TempDir::new().unwrap();
    let store = Store::open(dir.path()).unwrap();
    if graphicle_runtime::core_capabilities::capabilities()["edition"] != "core" {
        // An edition with the team extension creates the tables itself.
        return;
    }
    let created = store.create_workspace("Core store").unwrap();
    let workspace = s(&created, "id");
    let owner = s(&created, "capability");
    let project = store
        .register(
            workspace,
            owner,
            &json!({"project_key":"core","name":"Core"}),
        )
        .unwrap();
    let app = App::new(
        store.clone(),
        model::Config::default(),
        vec!["localhost".into()],
    )
    .unwrap();
    let state = api::command(
        &app,
        workspace,
        owner,
        "deletion.state",
        &json!({"repository_id":project["id"]}),
    )
    .unwrap();
    for table in ["am_team_audit", "am_browser_sessions", "am_login_pending"] {
        assert!(
            !state["excluded"]["history"]
                .as_array()
                .unwrap()
                .contains(&json!(table)),
            "{state}"
        );
    }
    // Revocation and backups work without them.
    let key = store
        .grant(workspace, owner, &json!({"label":"key","role":"reader"}))
        .unwrap();
    store.revoke(workspace, owner, s(&key, "id")).unwrap();
    store
        .backup(&store.directory.join("core-backup.sqlite3"))
        .unwrap();
    let tables: i64 = store
        .connect()
        .unwrap()
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE name IN ('am_members','am_sso','am_subscriptions')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(tables, 0);
}
