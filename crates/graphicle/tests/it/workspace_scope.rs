//! Workspace scope.
//!
//! `scope: "workspace"` recalls over the projects of a workspace the caller
//! may read, each read and authorized as a single-project read is, fused by
//! rank position; every window carries its project. The cross-repository
//! step follows the recorded import edges one hop in both directions. A
//! workspace MCP connection (`/mcp/{workspace}`, `graphicle mcp
//! --workspace`) answers `repo: <project>` and workspace recall, and lists
//! the projects; a project connection reads its own project only. A project
//! the caller may not read contributes no window, no edge and no count, and
//! one whose read fails is left out and reported.
use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use graphicle_runtime::{
    api::{self, App, ToolSet, router},
    jobs, model,
    store::{Store, s},
};
use serde_json::{Value, json};
use std::{
    path::Path,
    process::{Command, Stdio},
};
use tempfile::TempDir;
use tower::ServiceExt;

const MONEY_PYPROJECT: &str = "[project]\nname = \"moneylib\"\nversion = \"1.0.0\"\n";
const ROUNDING: &str =
    "def quantize_cents(value):\n    scaled = int(value * 100 + 0.5)\n    return scaled / 100\n";
const SHOP_PYPROJECT: &str =
    "[project]\nname = \"shopapp\"\nversion = \"0.1.0\"\ndependencies = [\"moneylib>=1.0\"]\n";
const INVOICE: &str = "from moneylib.rounding import quantize_cents\n\n\ndef settle_invoice(total, tax):\n    \"\"\"Settle an invoice: the amount due with tax.\"\"\"\n    return quantize_cents(total + tax)\n";
/// Shares content words with the shop's invoice only, never with the money
/// library's rounding (which the invoice calls).
const ABOUT_THE_SHOP: &str = "How is the amount due on an invoice settled?";
/// Shares content words with the money library's rounding only.
const ABOUT_THE_LIBRARY: &str = "Which values are scaled by a hundred?";
/// `ROUNDING` with a comment added: other bytes, no word of `ABOUT_THE_SHOP`.
const ROUNDING_EDITED: &str = "def quantize_cents(value):\n    # Half up, to whole cents.\n    scaled = int(value * 100 + 0.5)\n    return scaled / 100\n";
/// `INVOICE` with its docstring reworded: other bytes, the same import.
const INVOICE_EDITED: &str = "from moneylib.rounding import quantize_cents\n\n\ndef settle_invoice(total, tax):\n    \"\"\"Settle an invoice: the amount due with tax, in cents.\"\"\"\n    return quantize_cents(total + tax)\n";
/// Shares content words with both projects' code.
const ABOUT_BOTH: &str = "settle the invoice amount and scale the cents";

fn money_files() -> Vec<(&'static str, &'static str)> {
    vec![
        ("pyproject.toml", MONEY_PYPROJECT),
        ("moneylib/__init__.py", ""),
        ("moneylib/rounding.py", ROUNDING),
    ]
}
fn shop_files() -> Vec<(&'static str, &'static str)> {
    vec![
        ("pyproject.toml", SHOP_PYPROJECT),
        ("shopapp/__init__.py", ""),
        ("shopapp/invoice.py", INVOICE),
    ]
}

struct Fixture {
    dir: TempDir,
    store: Store,
    workspace: String,
    key: String,
}
impl Fixture {
    fn new() -> Self {
        crate::edition_setup();
        let dir = TempDir::new().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let ws = store.create_workspace("Workspace scope").unwrap();
        Self {
            workspace: s(&ws, "id").to_string(),
            key: s(&ws, "capability").to_string(),
            dir,
            store,
        }
    }
    /// Register a project, sync `files` into it and build its graph (with
    /// the workspace's link job).
    fn project(&self, key: &str, name: &str, files: &[(&str, &str)]) -> String {
        let id = s(
            &self
                .store
                .register(
                    &self.workspace,
                    &self.key,
                    &json!({"project_key":key,"name":name}),
                )
                .unwrap(),
            "id",
        )
        .to_string();
        self.sync(&id, files);
        jobs::drain_projection(&self.store, &id).unwrap();
        id
    }
    /// Sync `files` into a project without building its graph (as `sync
    /// --no-graph`): its recorded episode and import edges stay as they were.
    fn sync(&self, id: &str, files: &[(&str, &str)]) {
        let args = |mut v: Value| {
            v["repository_id"] = json!(id);
            v
        };
        let state = self
            .store
            .project_status(&self.workspace, &self.key, &args(json!({})))
            .unwrap();
        let begin = self
            .store
            .sync_prepare(
                &self.workspace,
                &self.key,
                &args(json!({"expected_snapshot":state["repository"]["snapshot"],"manifest":files.iter().map(|(path, text)| json!({
                    "path":path,"sha256":graphicle_memory::text::sha256(text.as_bytes()),"bytes":text.len()})).collect::<Vec<_>>()})),
            )
            .unwrap();
        self.store
            .sync_upload(
                &self.workspace,
                &self.key,
                &args(json!({"sync_id":begin["sync_id"],"files":files.iter().map(|(path, text)| json!({"path":path,"content":text})).collect::<Vec<_>>()})),
            )
            .unwrap();
        self.store
            .sync_commit(
                &self.workspace,
                &self.key,
                &args(json!({"sync_id":begin["sync_id"]})),
            )
            .unwrap();
    }
    /// The store's database, to damage it.
    fn db(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(self.dir.path().join("reviews.sqlite3")).unwrap()
    }
    /// The money library, then the shop that imports it, linked.
    fn shop_and_money() -> (Self, String, String) {
        let f = Self::new();
        let money = f.project("moneylib", "Money", &money_files());
        let shop = f.project("shopapp", "Shop", &shop_files());
        let linked = f.store.graph_link(&f.workspace, &f.key).unwrap();
        assert!(linked["edges"].as_u64().unwrap() >= 1, "{linked}");
        (f, shop, money)
    }
    fn context(&self, token: &str, args: Value) -> Value {
        self.store
            .project_context(&self.workspace, token, &args)
            .unwrap()
    }
    fn reader(&self, role: &str) -> (String, String) {
        let granted = self
            .store
            .grant(
                &self.workspace,
                &self.key,
                &json!({"role":role,"label":format!("{role} key")}),
            )
            .unwrap();
        (
            s(&granted, "id").to_string(),
            s(&granted, "capability").to_string(),
        )
    }
    fn revoke(&self, repository: &str, credential: &str) {
        self.store
            .graph_access(
                &self.workspace,
                &self.key,
                &json!({"repository_id":repository,"credential_id":credential,"state":"revoked"}),
            )
            .unwrap();
    }
    fn app(&self) -> App {
        App::new(
            self.store.clone(),
            model::Config::default(),
            vec!["localhost".into()],
        )
        .unwrap()
    }
}

/// The project ids of a packet's evidence windows, in order.
/// A call's arguments as these tests send them: recall_project and the
/// graph tools answer with their whole packet in `structuredContent` only
/// when asked (`structured: true`), and the tests read it.
fn structured(name: &str, mut arguments: Value) -> Value {
    if name == "recall_project" || graphicle_runtime::graph_tools::NAVIGATION.contains(&name) {
        arguments["structured"] = json!(true);
    }
    arguments
}

fn projects_cited(packet: &Value) -> Vec<String> {
    packet["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| s(w, "repository_id").to_string())
        .collect()
}

/// The window of `path` in `project`, if the packet cites it.
fn window<'a>(packet: &'a Value, project: &str, path: &str) -> Option<&'a Value> {
    packet["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["repository_id"] == project && w["id"] == format!("file:{path}"))
}

/// One question over both projects. Each window carries its project
/// (and the text names it); the packet reports the projects searched, a
/// combined source state and each project's own arm; with `scope: project`
/// (or none) the packet is the single project's, unchanged.
#[test]
fn workspace_recall_reads_every_project_and_windows_carry_their_project() {
    let (f, shop, money) = Fixture::shop_and_money();
    let question = ABOUT_BOTH;
    let packet = f.context(
        &f.key,
        json!({"question":question,"scope":"workspace","repository_id":shop}),
    );
    assert_eq!(packet["scope"], "workspace");
    assert_eq!(
        packet["workspace"]["projects_searched"], 2,
        "{}",
        packet["workspace"]
    );
    assert_eq!(packet["workspace"]["projects_left_out"], 0);
    assert_eq!(packet["mechanisms"]["scope"], "workspace");
    assert_eq!(packet["mechanisms"]["workspace"]["projects_searched"], 2);
    let projects: Vec<&str> = packet["projects"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| s(p, "repository_id"))
        .collect();
    assert_eq!(
        projects,
        [shop.as_str(), money.as_str()],
        "the named project first"
    );
    let cited = projects_cited(&packet);
    assert!(cited.contains(&shop) && cited.contains(&money), "{cited:?}");
    // The library's own best window is the function the shop's window calls:
    // it moves up beside that window (through the import) and is not
    // repeated at its own place.
    assert_eq!(
        cited.iter().filter(|p| **p == money).count(),
        1,
        "{cited:?}"
    );
    let library = window(&packet, &money, "moneylib/rounding.py").unwrap();
    assert_eq!(
        library["cross_repo"]["direction"], "imported_by",
        "{library}"
    );
    assert_eq!(cited[..2], [shop.clone(), money.clone()]);
    for window in packet["evidence"].as_array().unwrap() {
        let name = if window["repository_id"] == shop {
            "Shop"
        } else {
            "Money"
        };
        assert_eq!(window["project"], name, "{window}");
    }
    for source in packet["sources"].as_array().unwrap() {
        assert!(source["repository_id"] == shop || source["repository_id"] == money);
    }
    let markdown = s(&packet, "markdown");
    assert!(
        markdown.contains(&format!("Project: Shop ({shop})")),
        "{markdown}"
    );
    assert!(
        markdown.contains(&format!("Project: Money ({money})")),
        "{markdown}"
    );
    assert!(
        markdown
            .starts_with("Graphicle project evidence — 2 projects of this workspace: Shop, Money")
    );
    for project in packet["projects"].as_array().unwrap() {
        let status = s(&project["code_graph"], "status");
        assert!(["fused", "no_match"].contains(&status), "{project}");
        assert_eq!(project["code_graph"]["episode_lag"], 0);
        assert_eq!(project["graph_source"], "episode");
    }
    assert_eq!(packet["projects"][0]["code_graph"]["status"], "fused");
    assert_eq!(packet["code_graph"]["status"], "fused");
    assert_eq!(packet["code_graph"]["episode_lag"], 0);
    assert_eq!(packet["graph_source"], "episode");
    // One budget for the whole packet.
    assert!(packet["tokens"].as_u64().unwrap() <= 3000);
    // The combined source state differs from either project's own.
    let alone = f.context(&f.key, json!({"question":question,"repository_id":shop}));
    assert_ne!(packet["source_state"], alone["source_state"]);
    // Project scope: the single project's packet, with or without the switch.
    let explicit = f.context(
        &f.key,
        json!({"question":question,"repository_id":shop,"scope":"project"}),
    );
    assert_eq!(alone, explicit);
    assert_eq!(alone["mechanisms"]["scope"], "project");
    assert_eq!(
        alone["mechanisms"]["workspace"],
        json!({"projects_searched":1,"cross_edges_used":0})
    );
    assert!(alone.get("projects").is_none() && alone.get("workspace").is_none());
    assert!(
        alone["evidence"]
            .as_array()
            .unwrap()
            .iter()
            .all(|w| w.get("repository_id").is_none() && w.get("project").is_none())
    );
    assert!(!s(&alone, "markdown").contains("Project: "));
    // Without a named project, every readable project in creation order.
    // The library's window now comes first, and the shop's use of the
    // function it holds follows it through the import (`imports`), in place
    // of the shop's own later window of the same code, which is not
    // repeated.
    let unanchored = f.context(&f.key, json!({"question":question,"scope":"workspace"}));
    assert_eq!(unanchored["projects"][0]["repository_id"], money.as_str());
    assert_eq!(unanchored["repository_id"], Value::Null);
    let evidence = unanchored["evidence"].as_array().unwrap();
    assert_eq!(evidence[0]["repository_id"], money.as_str(), "{evidence:?}");
    assert!(evidence[0].get("cross_repo").is_none(), "{}", evidence[0]);
    let used = &evidence[1];
    assert_eq!(used["repository_id"], shop.as_str(), "{evidence:?}");
    assert_eq!(used["cross_repo"]["direction"], "imports", "{used}");
    assert_eq!(used["cross_repo"]["from"]["repository_id"], money.as_str());
    assert_eq!(used["cross_repo"]["node"]["label"], "settle_invoice()");
    assert!(s(used, "text").contains("def settle_invoice"));
    assert_eq!(
        evidence
            .iter()
            .filter(|w| w["repository_id"] == shop && w["id"] == "file:shopapp/invoice.py")
            .count(),
        1,
        "{evidence:?}"
    );
    assert_eq!(unanchored["workspace"]["cross_repo"]["lifted"], 1);
    assert_eq!(unanchored["workspace"]["cross_repo"]["annotated"], 0);
    // Bad values are refused.
    for bad in [
        json!({"question":question,"scope":"everything"}),
        json!({"question":question,"scope":"workspace","max_projects":33}),
        json!({"question":question,"repository_id":shop,"max_projects":2}),
    ] {
        assert!(
            f.store.project_context(&f.workspace, &f.key, &bad).is_err(),
            "{bad}"
        );
    }
}

/// A question about the shop's behaviour that its
/// call into the money library causes. The shop's window calls
/// `quantize_cents`, which shares no word with the question: the recorded
/// import edge (shop → money) lifts the library function's window, which
/// cites both ends of the edge. Without the step (`graph_retrieval: false`)
/// only the shop is cited.
#[test]
fn the_cross_repository_step_cites_the_library_function_the_app_calls() {
    let (f, shop, money) = Fixture::shop_and_money();
    let args = |graph: bool| json!({"question":ABOUT_THE_SHOP,"scope":"workspace","repository_id":shop,"graph_retrieval":graph});
    let without = f.context(&f.key, args(false));
    assert_eq!(
        projects_cited(&without)
            .iter()
            .filter(|p| **p == money)
            .count(),
        0,
        "the library was found without the step: {}",
        without["evidence"]
    );
    assert!(window(&without, &shop, "shopapp/invoice.py").is_some());
    assert_eq!(without["workspace"]["cross_repo"]["enabled"], false);
    assert_eq!(without["mechanisms"]["workspace"]["cross_edges_used"], 0);

    let with = f.context(&f.key, args(true));
    let invoice = window(&with, &shop, "shopapp/invoice.py").expect("the shop's window");
    let lifted = window(&with, &money, "moneylib/rounding.py")
        .unwrap_or_else(|| panic!("the library is not cited: {}", with["evidence"]));
    let facts = &lifted["cross_repo"];
    assert_eq!(facts["via"], "cross_repo_import", "{facts}");
    assert_eq!(facts["direction"], "imported_by");
    assert_eq!(facts["from"]["repository_id"], shop.as_str());
    assert_eq!(facts["from"]["evidence_id"], "file:shopapp/invoice.py");
    assert_eq!(facts["node"]["repository_id"], money.as_str());
    assert_eq!(facts["node"]["label"], "quantize_cents()");
    assert!(facts["node"]["stable_id"].is_string(), "{facts}");
    assert_eq!(facts["edge"]["symbol"], "quantize_cents");
    assert_eq!(facts["edge"]["source"]["repository_id"], shop.as_str());
    assert_eq!(facts["edge"]["target"]["repository_id"], money.as_str());
    assert_eq!(
        facts["edge"]["target"]["stable_id"],
        facts["node"]["stable_id"]
    );
    assert!(s(lifted, "text").contains("def quantize_cents"));
    // The lifted window follows the window that led to it.
    let evidence = with["evidence"].as_array().unwrap();
    let at = |w: &Value| evidence.iter().position(|e| e == w).unwrap();
    assert_eq!(at(lifted), at(invoice) + 1);
    assert_eq!(with["mechanisms"]["workspace"]["cross_edges_used"], 1);
    assert_eq!(with["workspace"]["cross_repo"]["lifted"], 1);
    assert_eq!(
        with["arms"]["cross_repo_import"]["selected"], 1,
        "{}",
        with["arms"]
    );
    assert!(s(&with, "markdown").contains(&format!("Project: Money ({money})")));
}

/// The other direction: a question that only the library's window
/// answers lifts the shop's use of the imported function (the importer's
/// file node, at the symbol that calls it).
#[test]
fn the_cross_repository_step_cites_where_another_project_uses_a_function() {
    let (f, shop, money) = Fixture::shop_and_money();
    let packet = f.context(
        &f.key,
        json!({"question":ABOUT_THE_LIBRARY,"scope":"workspace","repository_id":money}),
    );
    assert!(
        window(&packet, &money, "moneylib/rounding.py").is_some(),
        "{}",
        packet["evidence"]
    );
    let used = window(&packet, &shop, "shopapp/invoice.py")
        .unwrap_or_else(|| panic!("the shop's use is not cited: {}", packet["evidence"]));
    let facts = &used["cross_repo"];
    assert_eq!(facts["direction"], "imports", "{facts}");
    assert_eq!(facts["from"]["repository_id"], money.as_str());
    assert_eq!(facts["node"]["label"], "settle_invoice()", "{facts}");
    assert!(s(used, "text").contains("return quantize_cents(total + tax)"));
}

/// The guard against stale bytes under a stable id: the other end of an
/// edge is cited only while its file has the bytes the edge's episode
/// recorded. Once the library's file changed (synced, its graph not rebuilt)
/// the edge is withheld and nothing is lifted. An episode whose manifest
/// cannot be read is withheld the same way, and the read does not fail.
#[test]
fn an_import_whose_other_end_changed_is_withheld_not_lifted() {
    let (f, shop, money) = Fixture::shop_and_money();
    f.sync(
        &money,
        &[
            ("pyproject.toml", MONEY_PYPROJECT),
            ("moneylib/__init__.py", ""),
            ("moneylib/rounding.py", ROUNDING_EDITED),
        ],
    );
    let args = json!({"question":ABOUT_THE_SHOP,"scope":"workspace","repository_id":shop});
    for damaged in [false, true] {
        if damaged {
            f.db()
                .execute(
                    "UPDATE gf_episodes SET attributes=json_set(attributes,'$.source_heads',5) WHERE repository_id=?1",
                    [&money],
                )
                .unwrap();
        }
        let packet = f.context(&f.key, args.clone());
        let report = &packet["workspace"]["cross_repo"];
        assert_eq!(report["withheld_changed"], 1, "damaged {damaged}: {report}");
        assert_eq!(report["lifted"], 0);
        assert!(report.get("error").is_none(), "{report}");
        assert_eq!(packet["mechanisms"]["workspace"]["cross_edges_used"], 0);
        assert!(window(&packet, &shop, "shopapp/invoice.py").is_some());
        assert!(
            window(&packet, &money, "moneylib/rounding.py").is_none(),
            "{}",
            packet["evidence"]
        );
        assert_eq!(packet["workspace"]["projects_searched"], 2);
    }
}

/// The other placement: when the code an edge leads to is already in the
/// list before the window that leads to it, that window keeps its place and
/// gains the `cross_repo` facts. With no project named the library's window
/// comes first; its own walk to the shop's use is withheld (the shop's file
/// changed since the link), and the shop's window, which imports the
/// function, annotates the library's.
#[test]
fn an_earlier_window_the_step_reaches_gains_the_facts_in_place() {
    let (f, shop, money) = Fixture::shop_and_money();
    f.sync(
        &shop,
        &[
            ("pyproject.toml", SHOP_PYPROJECT),
            ("shopapp/__init__.py", ""),
            ("shopapp/invoice.py", INVOICE_EDITED),
        ],
    );
    let packet = f.context(&f.key, json!({"question":ABOUT_BOTH,"scope":"workspace"}));
    let evidence = packet["evidence"].as_array().unwrap();
    assert_eq!(
        projects_cited(&packet)[..2],
        [money.clone(), shop.clone()],
        "{evidence:?}"
    );
    let library = &evidence[0];
    assert_eq!(library["id"], "file:moneylib/rounding.py");
    let facts = &library["cross_repo"];
    assert_eq!(facts["direction"], "imported_by", "{library}");
    assert_eq!(facts["from"]["repository_id"], shop.as_str());
    assert_eq!(facts["from"]["evidence_id"], "file:shopapp/invoice.py");
    assert_eq!(facts["node"]["label"], "quantize_cents()");
    assert_eq!(facts["edge"]["symbol"], "quantize_cents");
    let report = &packet["workspace"]["cross_repo"];
    assert_eq!(
        (
            &report["annotated"],
            &report["lifted"],
            &report["withheld_changed"]
        ),
        (&json!(1), &json!(0), &json!(1)),
        "{report}"
    );
    assert_eq!(packet["mechanisms"]["workspace"]["cross_edges_used"], 1);
    // The shop's own window stays where it was, without facts.
    let invoice = window(&packet, &shop, "shopapp/invoice.py").unwrap();
    assert!(invoice.get("cross_repo").is_none(), "{invoice}");
    assert!(s(invoice, "text").contains("in cents"));
    assert_eq!(
        projects_cited(&packet)
            .iter()
            .filter(|p| **p == money)
            .count(),
        1
    );
}

/// `include` limits what the step can lift as it limits each project's
/// windows: with only the shop's folder included, the library's function is
/// not cited; with the library's folder too, it is.
#[test]
fn include_limits_what_the_step_can_lift() {
    let (f, shop, money) = Fixture::shop_and_money();
    let args = |include: Value| json!({"question":ABOUT_THE_SHOP,"scope":"workspace","repository_id":shop,"include":include});
    let packet = f.context(&f.key, args(json!(["file:shopapp/"])));
    assert!(window(&packet, &shop, "shopapp/invoice.py").is_some());
    assert!(
        projects_cited(&packet).iter().all(|p| *p == shop),
        "{}",
        packet["evidence"]
    );
    assert_eq!(packet["workspace"]["cross_repo"]["lifted"], 0);
    assert_eq!(packet["projects"][1]["repository_id"], money.as_str());
    assert_eq!(packet["projects"][1]["filter"]["documents_searched"], 0);
    let both = f.context(&f.key, args(json!(["file:shopapp/", "file:moneylib/"])));
    assert!(
        window(&both, &money, "moneylib/rounding.py").is_some(),
        "{}",
        both["evidence"]
    );
    assert_eq!(both["workspace"]["cross_repo"]["lifted"], 1);
}

/// Reviewed memories are fused as the windows are (each project's first,
/// then each one's second, in scope order), carry their project, and the
/// packet's text names it.
#[test]
fn reviewed_memories_of_each_project_are_fused_and_name_their_project() {
    let (f, shop, money) = Fixture::shop_and_money();
    let note = |project: &str, title: &str, body: &str| -> String {
        s(
            &f.store
                .note(
                    &f.workspace,
                    &f.key,
                    &json!({"repository_id":project,"kind":"decision","title":title,"body":body}),
                )
                .unwrap(),
            "id",
        )
        .to_string()
    };
    let shop_note = note(
        &shop,
        "Invoice rounding",
        "Invoices round the amount due once, after tax is added.",
    );
    let money_note = note(
        &money,
        "Rounding rule",
        "Amounts round half up to whole cents.",
    );
    let question = "Where are invoice amounts rounded?";
    let packet = f.context(
        &f.key,
        json!({"question":question,"scope":"workspace","repository_id":shop}),
    );
    let memories: Vec<(&str, &str, &str)> = packet["memories"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| (s(m, "id"), s(m, "repository_id"), s(m, "project")))
        .collect();
    assert_eq!(
        memories,
        [
            (shop_note.as_str(), shop.as_str(), "Shop"),
            (money_note.as_str(), money.as_str(), "Money")
        ],
        "{}",
        packet["memories"]
    );
    assert_eq!(packet["mechanisms"]["reviewed_memory_included"], 2);
    for project in packet["projects"].as_array().unwrap() {
        assert_eq!(project["memories"], 1, "{project}");
    }
    let markdown = s(&packet, "markdown");
    for (id, line) in [
        (&shop_note, format!("Project: Shop ({shop})")),
        (&money_note, format!("Project: Money ({money})")),
    ] {
        assert!(
            markdown.contains(&format!("Reference: {id}\n{line}\n")),
            "{markdown}"
        );
    }
    // With no project named, creation order: the library's memory first.
    let unanchored = f.context(&f.key, json!({"question":question,"scope":"workspace"}));
    let order: Vec<&str> = unanchored["memories"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| s(m, "id"))
        .collect();
    assert_eq!(order, [money_note.as_str(), shop_note.as_str()]);
}

/// A project whose read fails is left out and reported, and the others still
/// answer, as a degraded code arm never fails a query. Named first, it fails
/// the read, as a direct read of it does.
#[test]
fn a_project_that_cannot_be_read_is_left_out_and_reported() {
    let (f, shop, money) = Fixture::shop_and_money();
    // The library's stored bytes no longer match their hashes.
    f.db()
        .execute(
            "UPDATE repo_objects SET content=content||'# damaged' WHERE repository_id=?1",
            [&money],
        )
        .unwrap();
    for args in [
        json!({"question":ABOUT_THE_SHOP,"scope":"workspace"}),
        json!({"question":ABOUT_THE_SHOP,"scope":"workspace","repository_id":shop}),
    ] {
        let packet = f.context(&f.key, args.clone());
        assert_eq!(
            (
                &packet["workspace"]["projects_searched"],
                &packet["workspace"]["projects_failed"]
            ),
            (&json!(1), &json!(1)),
            "{args}: {}",
            packet["workspace"]
        );
        let reported: Vec<(&str, &str)> = packet["projects"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| (s(p, "repository_id"), s(p, "status")))
            .collect();
        let expected = if args.get("repository_id").is_some() {
            [(shop.as_str(), "searched"), (money.as_str(), "error")]
        } else {
            [(money.as_str(), "error"), (shop.as_str(), "searched")]
        };
        assert_eq!(reported, expected, "{args}");
        let failed = packet["projects"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["repository_id"] == money)
            .unwrap();
        assert!(
            s(failed, "error").contains("integrity check failed"),
            "{failed}"
        );
        assert!(projects_cited(&packet).iter().all(|p| *p == shop));
        assert!(window(&packet, &shop, "shopapp/invoice.py").is_some());
        assert_eq!(
            packet["code_graph"]["projects"].as_object().unwrap().len(),
            1
        );
    }
    for args in [
        json!({"question":ABOUT_THE_SHOP,"scope":"workspace","repository_id":money}),
        json!({"question":ABOUT_THE_SHOP,"repository_id":money}),
    ] {
        let error = f
            .store
            .project_context(&f.workspace, &f.key, &args)
            .unwrap_err();
        assert!(
            error.message.contains("integrity check failed"),
            "{args}: {}",
            error.message
        );
    }
}

/// The permission rule: a project the key
/// may not read contributes no window, no cross edge and no count, whether
/// its access was revoked after the key had read it or the key's repository
/// scope excluded it from the start. Over MCP its id is refused as `repo`
/// and `list_projects` does not show it.
#[test]
fn an_unreadable_project_contributes_no_windows_no_cross_edges_and_no_counts() {
    let (f, shop, money) = Fixture::shop_and_money();
    let (reader_id, reader) = f.reader("reader");
    let args = json!({"question":ABOUT_THE_SHOP,"scope":"workspace","repository_id":shop});
    let before = f.context(&reader, args.clone());
    assert!(
        projects_cited(&before).contains(&money),
        "{}",
        before["evidence"]
    );
    f.revoke(&money, &reader_id);
    let (agent_id, agent) = f.reader("agent");
    f.revoke(&money, &agent_id);
    let app = f.app();
    for token in [&reader, &agent] {
        let after = f.context(token, args.clone());
        assert_eq!(
            after["workspace"]["projects_searched"], 1,
            "{}",
            after["workspace"]
        );
        assert_eq!(after["workspace"]["projects_left_out"], 0);
        assert_eq!(after["workspace"]["cross_repo"]["edges_read"], 0);
        assert_eq!(after["mechanisms"]["workspace"]["cross_edges_used"], 0);
        assert!(projects_cited(&after).iter().all(|p| *p == shop));
        assert!(
            !after.to_string().contains(&money),
            "the revoked project leaked into the packet"
        );
        // Unanchored, the same.
        let all = f.context(
            token,
            json!({"question":ABOUT_THE_SHOP,"scope":"workspace"}),
        );
        assert_eq!(all["workspace"]["projects_searched"], 1);
        assert!(!all.to_string().contains(&money));
        // Named as the first project, it is refused, as a direct read is.
        assert!(
            f.store
                .project_context(
                    &f.workspace,
                    token,
                    &json!({"question":ABOUT_THE_SHOP,"scope":"workspace","repository_id":money}),
                )
                .is_err()
        );
        let listed = f
            .store
            .project_list(&f.workspace, token, &json!({}))
            .unwrap();
        assert_eq!(listed["projects"].as_array().unwrap().len(), 1, "{listed}");
        assert_eq!(listed["workspace_recall"]["readable"], 1);
        // Over a workspace MCP connection.
        let call = |name: &str, arguments: Value| {
            api::mcp_workspace(
                &app,
                &f.workspace,
                token,
                &json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
                "params":{"name":name,"arguments":structured(name, arguments)}}),
            )
            .unwrap()
        };
        let recalled = call("recall_project", json!({"question":ABOUT_THE_SHOP}));
        let packet = &recalled["result"]["structuredContent"];
        assert_eq!(packet["workspace"]["projects_searched"], 1, "{recalled}");
        assert!(!packet.to_string().contains(&money));
        let refused = call(
            "recall_project",
            json!({"question":ABOUT_THE_SHOP,"repo":money}),
        );
        assert_eq!(refused["result"]["isError"], true, "{refused}");
        let read = call(
            "read_project_source",
            json!({"path":"moneylib/rounding.py","repo":money}),
        );
        assert_eq!(read["result"]["isError"], true, "{read}");
        let listed = call("list_projects", json!({}));
        assert!(!listed.to_string().contains(&money), "{listed}");
    }
    // The owner still reads both.
    let owner = f.context(&f.key, args);
    assert_eq!(owner["workspace"]["projects_searched"], 2);
}

/// The fan-out cap (8 by default, `max_projects` up to 32) and PR scratch
/// projects: never searched, never listed by default, never counted.
#[test]
fn the_fan_out_is_capped_and_skips_pr_scratch_projects() {
    let f = Fixture::new();
    let mut ids = Vec::new();
    for i in 0..10 {
        ids.push(f.project(
            &format!("p{i}"),
            &format!("Project {i}"),
            &[(
                "notes.txt",
                &format!("Project {i} keeps the ledger of release {i}.\n"),
            )],
        ));
    }
    let scratch = f.project("git:scratch", "Scratch", &[("notes.txt", "ledger copy\n")]);
    f.store
        .mark_pr_scratch(&f.workspace, &f.key, &scratch)
        .unwrap();
    let packet = f.context(&f.key, json!({"question":"ledger","scope":"workspace"}));
    assert_eq!(packet["workspace"]["projects_searched"], 8);
    assert_eq!(packet["workspace"]["projects_left_out"], 2);
    assert_eq!(packet["workspace"]["max_projects"], 8);
    let searched: Vec<&str> = packet["projects"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| s(p, "repository_id"))
        .collect();
    assert_eq!(
        searched,
        ids[..8].iter().map(String::as_str).collect::<Vec<_>>()
    );
    let anchored = f.context(
        &f.key,
        json!({"question":"ledger","scope":"workspace","repository_id":ids[9]}),
    );
    assert_eq!(anchored["projects"][0]["repository_id"], ids[9].as_str());
    assert_eq!(anchored["workspace"]["projects_searched"], 8);
    let wide = f.context(
        &f.key,
        json!({"question":"ledger","scope":"workspace","max_projects":12}),
    );
    assert_eq!(wide["workspace"]["projects_searched"], 10);
    assert_eq!(wide["workspace"]["projects_left_out"], 0);
    assert!(!wide.to_string().contains(&scratch));
    let listed = f
        .store
        .project_list(&f.workspace, &f.key, &json!({}))
        .unwrap();
    assert_eq!(listed["projects"].as_array().unwrap().len(), 10);
    assert_eq!(
        listed["workspace_recall"],
        json!({"projects":8,"readable":10,"max_projects":8})
    );
    let with_scratch = f
        .store
        .project_list(&f.workspace, &f.key, &json!({"include_scratch":true}))
        .unwrap();
    let last = with_scratch["projects"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()
        .clone();
    assert_eq!(last["id"], scratch.as_str());
    assert_eq!(last["role"], "pr_scratch");
}

/// An answer job reads one project: workspace scope is refused there.
#[test]
fn answer_jobs_refuse_workspace_scope() {
    let (f, shop, _) = Fixture::shop_and_money();
    let error = f
        .store
        .enqueue(
            &f.workspace,
            &f.key,
            &json!({"repository_id":shop,"kind":"basic","question":ABOUT_THE_SHOP,"scope":"workspace"}),
            &model::Config::default(),
        )
        .unwrap_err();
    assert!(
        error.message.contains("scope workspace"),
        "{}",
        error.message
    );
}

async fn post(app: Router, uri: &str, token: &str, body: Value) -> (StatusCode, Value) {
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("authorization", format!("Bearer {token}"))
                .header("host", "localhost")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// One workspace MCP connection answers
/// both `repo: <project>` and workspace recall, and lists the projects (ids,
/// names and times, a page at a time). It publishes only the tools that take
/// `repo` and the workspace-level ones. A project connection reads its own
/// project only, as before workspace connections: no `repo` on recall and
/// source reads, no `list_projects`, `scope: workspace` refused; `scope`
/// stays out of every published schema.
#[tokio::test]
async fn one_workspace_connection_answers_repo_and_workspace_recall() {
    let (f, shop, money) = Fixture::shop_and_money();
    let rpc = |id: u64, method: &str, params: Value| json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
    let call = |name: &str, arguments: Value| {
        rpc(
            2,
            "tools/call",
            json!({"name":name,"arguments":structured(name, arguments)}),
        )
    };
    let workspace = format!("/mcp/{}", f.workspace);
    let app = router(f.app());
    let (status, init) = post(
        app.clone(),
        &workspace,
        &f.key,
        rpc(1, "initialize", json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        s(&init["result"], "instructions").contains("list_projects"),
        "{init}"
    );
    let (_, listed) = post(
        app.clone(),
        &workspace,
        &f.key,
        rpc(1, "tools/list", json!({})),
    )
    .await;
    let tools = listed["result"]["tools"].as_array().unwrap();
    let names: Vec<&str> = tools.iter().map(|t| s(t, "name")).collect();
    // The core's tools that take `repo` or need no project, the graph
    // engine's navigation tools among them; an edition's own
    // tools follow, each with a `repo` argument.
    let core = [
        "recall_project",
        "read_project_source",
        "list_projects",
        "graph_episodes",
        "graph_as_of",
        "graph_link",
        "graph_history_stats",
        "query_graph",
        "get_node",
        "get_neighbors",
        "get_community",
        "god_nodes",
        "graph_stats",
        "shortest_path",
    ];
    assert_eq!(names[..core.len()], core, "{names:?}");
    assert!(
        tools[core.len()..]
            .iter()
            .all(|t| t["inputSchema"]["properties"]["repo"].is_object()),
        "{names:?}"
    );
    let (_, projects) = post(
        app.clone(),
        &workspace,
        &f.key,
        call("list_projects", json!({})),
    )
    .await;
    let listed = &projects["result"]["structuredContent"];
    let ids: Vec<&str> = listed["projects"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| s(p, "id"))
        .collect();
    assert_eq!(ids, [money.as_str(), shop.as_str()]);
    assert_eq!(listed["total"], 2);
    assert!(listed["next_offset"].is_null());
    // Ids, names and times: a project's key can carry a path or a user id.
    for project in listed["projects"].as_array().unwrap() {
        assert!(project.get("project_key").is_none(), "{project}");
        assert!(project["name"].is_string() && project["updated_at"].is_string());
    }
    // A page at a time.
    let (_, first) = post(
        app.clone(),
        &workspace,
        &f.key,
        call("list_projects", json!({"limit":1})),
    )
    .await;
    let first = &first["result"]["structuredContent"];
    assert_eq!(first["projects"].as_array().unwrap().len(), 1, "{first}");
    assert_eq!(first["projects"][0]["id"], money.as_str());
    assert_eq!(
        (first["total"].as_u64(), first["next_offset"].as_u64()),
        (Some(2), Some(1))
    );
    let (_, next) = post(
        app.clone(),
        &workspace,
        &f.key,
        call("list_projects", json!({"offset":1,"limit":1})),
    )
    .await;
    let next = &next["result"]["structuredContent"];
    assert_eq!(next["projects"][0]["id"], shop.as_str(), "{next}");
    assert!(next["next_offset"].is_null());
    let (_, everywhere) = post(
        app.clone(),
        &workspace,
        &f.key,
        call("recall_project", json!({"question":ABOUT_THE_SHOP})),
    )
    .await;
    let packet = &everywhere["result"]["structuredContent"];
    assert_eq!(packet["scope"], "workspace", "{everywhere}");
    assert_eq!(packet["workspace"]["projects_searched"], 2);
    assert!(window(packet, &money, "moneylib/rounding.py").is_some());
    let (_, one) = post(
        app.clone(),
        &workspace,
        &f.key,
        call(
            "recall_project",
            json!({"question":ABOUT_THE_SHOP,"repo":shop}),
        ),
    )
    .await;
    let packet = &one["result"]["structuredContent"];
    assert_eq!(packet["mechanisms"]["scope"], "project", "{one}");
    assert_eq!(packet["repository_id"], shop.as_str());
    let (_, read) = post(
        app.clone(),
        &workspace,
        &f.key,
        call(
            "read_project_source",
            json!({"path":"moneylib/rounding.py","repo":money}),
        ),
    )
    .await;
    assert!(
        s(&read["result"]["structuredContent"], "quote").contains("quantize_cents"),
        "{read}"
    );
    for (name, arguments) in [
        (
            "read_project_source",
            json!({"path":"moneylib/rounding.py"}),
        ),
        ("graph_episodes", json!({})),
        ("recall_project", json!({"question":"x","scope":"project"})),
    ] {
        let (_, refused) = post(app.clone(), &workspace, &f.key, call(name, arguments)).await;
        assert!(
            s(&refused["error"], "message").contains("Name the project with repo"),
            "{name}: {refused}"
        );
    }
    let (_, unpublished) = post(
        app.clone(),
        &workspace,
        &f.key,
        call("repository_status", json!({})),
    )
    .await;
    assert_eq!(s(&unpublished["error"], "message"), "Tool unavailable");
    let (_, smuggled) = post(
        app.clone(),
        &workspace,
        &f.key,
        call(
            "recall_project",
            json!({"question":"x","repository_id":shop}),
        ),
    )
    .await;
    assert!(smuggled["error"].is_object(), "{smuggled}");

    // A project connection reads its own project only: the tools as before
    // workspace connections, every way to another project refused.
    let project = format!("/mcp/{}/{}", f.workspace, shop);
    let (_, listed) = post(
        app.clone(),
        &project,
        &f.key,
        rpc(1, "tools/list", json!({})),
    )
    .await;
    assert_eq!(listed["result"], api::mcp_tools_with(false, false));
    let tools = listed["result"]["tools"].as_array().unwrap();
    assert!(!tools.iter().any(|t| t["name"] == "list_projects"));
    assert!(tools.iter().any(|t| t["name"] == "repository_status"));
    for name in ["recall_project", "read_project_source"] {
        let properties =
            &tools.iter().find(|t| t["name"] == name).unwrap()["inputSchema"]["properties"];
        assert!(properties.get("repo").is_none(), "{name}: {properties}");
        assert!(properties.get(api::RECALL_SCOPE).is_none());
    }
    for (name, arguments, refusal) in [
        (
            "recall_project",
            json!({"question":ABOUT_THE_LIBRARY,"repo":money}),
            "repo needs a workspace connection",
        ),
        (
            "read_project_source",
            json!({"path":"moneylib/rounding.py","repo":money}),
            "repo needs a workspace connection",
        ),
        (
            "recall_project",
            json!({"question":ABOUT_THE_SHOP,"scope":"workspace"}),
            "scope workspace needs a workspace connection",
        ),
        ("list_projects", json!({}), "Tool unavailable"),
        (
            "recall_project",
            json!({"question":"x","scope":"all"}),
            "scope must be project or workspace",
        ),
    ] {
        let (_, refused) = post(app.clone(), &project, &f.key, call(name, arguments)).await;
        assert!(
            s(&refused["error"], "message").contains(refusal),
            "{name}: {refused}"
        );
    }
    // `scope: project` is the connection's own read.
    let (_, own) = post(
        app,
        &project,
        &f.key,
        call(
            "recall_project",
            json!({"question":ABOUT_THE_SHOP,"scope":"project"}),
        ),
    )
    .await;
    let packet = &own["result"]["structuredContent"];
    assert_eq!(packet["repository_id"], shop.as_str(), "{own}");
    assert_eq!(packet["mechanisms"]["scope"], "project");

    // The work-state tools are a project's: with them enabled, a workspace
    // connection neither publishes nor mentions them; a project connection
    // of the same server does both.
    let mut working = f.app();
    working.mcp_work_state = true;
    let init = api::mcp_workspace(
        &working,
        &f.workspace,
        &f.key,
        &rpc(1, "initialize", json!({})),
    )
    .unwrap();
    let instructions = s(&init["result"], "instructions");
    assert!(
        !instructions.contains("work_state") && instructions.contains("list_projects"),
        "{instructions}"
    );
    let tools = api::mcp_workspace(
        &working,
        &f.workspace,
        &f.key,
        &rpc(1, "tools/list", json!({})),
    )
    .unwrap();
    assert!(!tools.to_string().contains("work_state_"), "{tools}");
    let init = api::mcp(
        &working,
        &f.workspace,
        &f.key,
        &shop,
        &rpc(1, "initialize", json!({})),
    )
    .unwrap();
    assert!(s(&init["result"], "instructions").contains("work_state_write"));
}

/// The graph engine's tools on a workspace connection:
/// they take `repo` there as the graph-history
/// tools do, and need it; `repo` names a project by id or key for every
/// tool, recall and source reads included; the named project's graph
/// answers and is cited. A workspace connection offers no resources, which
/// describe one project's graph. The two tool sets partition its tools as
/// they partition a project connection's, and the instructions follow what
/// is published.
#[test]
fn a_workspace_connection_serves_the_graph_tools_of_the_project_repo_names() {
    let (f, shop, money) = Fixture::shop_and_money();
    let rpc = |app: &App, method: &str, params: Value| {
        api::mcp_workspace(
            app,
            &f.workspace,
            &f.key,
            &json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}),
        )
        .unwrap()
    };
    let call = |app: &App, name: &str, arguments: Value| {
        rpc(
            app,
            "tools/call",
            json!({"name":name,"arguments":structured(name, arguments)}),
        )
    };
    let app = f.app();
    let init = rpc(&app, "initialize", json!({}));
    assert_eq!(
        init["result"]["capabilities"],
        json!({"tools":{}}),
        "{init}"
    );
    let instructions = s(&init["result"], "instructions");
    for part in [
        "list_projects names them",
        "query_graph for the symbols around a question",
        "Name an earlier code graph with as_of.",
        "Use recall_project for what the code",
    ] {
        assert!(instructions.contains(part), "{part}: {instructions}");
    }
    assert!(
        !instructions.contains("Name another project with repo"),
        "{instructions}"
    );
    let refused = rpc(&app, "resources/list", json!({}));
    assert_eq!(
        s(&refused["error"], "message"),
        "Unsupported MCP method",
        "{refused}"
    );
    // A navigation tool names its project, by id or key.
    let unnamed = call(&app, "get_node", json!({"label":"quantize_cents"}));
    assert!(
        s(&unnamed["error"], "message").contains("Name the project with repo"),
        "{unnamed}"
    );
    for named in [money.as_str(), "moneylib"] {
        let node = call(
            &app,
            "get_node",
            json!({"label":"quantize_cents","repo":named}),
        );
        let structured = &node["result"]["structuredContent"];
        assert_eq!(structured["repo"], money.as_str(), "{node}");
        let cited = &structured["nodes"][0];
        assert_eq!(cited["label"], "quantize_cents()", "{cited}");
        assert_eq!(cited["source_id"], "file:moneylib/rounding.py");
        assert_eq!(cited["checked"], true);
        assert!(
            s(&node["result"]["content"][1], "text").starts_with("Citations (code-graph episode "),
            "{node}"
        );
    }
    let graph = call(
        &app,
        "query_graph",
        json!({"question":"settle_invoice","repo":shop}),
    );
    assert_eq!(
        graph["result"]["structuredContent"]["repo"],
        shop.as_str(),
        "{graph}"
    );
    // Recall names a project by its key too.
    let one = call(
        &app,
        "recall_project",
        json!({"question":ABOUT_THE_LIBRARY,"repo":"moneylib"}),
    );
    let packet = &one["result"]["structuredContent"];
    assert_eq!(packet["repository_id"], money.as_str(), "{one}");
    assert_eq!(packet["mechanisms"]["scope"], "project");
    let unknown = call(
        &app,
        "recall_project",
        json!({"question":ABOUT_THE_LIBRARY,"repo":"octocat/hello-world"}),
    );
    assert_eq!(unknown["result"]["isError"], true, "{unknown}");

    // The halves partition the workspace connection's tools.
    let names = |tools: ToolSet| -> Vec<String> {
        let mut app = f.app();
        app.mcp.tools = tools;
        rpc(&app, "tools/list", json!({}))["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| s(tool, "name").to_string())
            .collect()
    };
    let (all, graph, memory) = (
        names(ToolSet::All),
        names(ToolSet::Graph),
        names(ToolSet::Memory),
    );
    let in_graph: Vec<String> = all.iter().filter(|n| graph.contains(n)).cloned().collect();
    let rest: Vec<String> = all.iter().filter(|n| !graph.contains(n)).cloned().collect();
    assert_eq!(in_graph, graph);
    assert_eq!(rest, memory);
    assert_eq!(
        memory,
        ["recall_project", "read_project_source", "list_projects"],
        "{all:?}"
    );
    assert!(
        graph.contains(&"query_graph".to_string()) && graph.contains(&"graph_link".to_string())
    );
    let mut graph_only = f.app();
    graph_only.mcp.tools = ToolSet::Graph;
    let instructions = rpc(&graph_only, "initialize", json!({}));
    let instructions = s(&instructions["result"], "instructions");
    assert!(
        instructions.contains("name one with repo")
            && !instructions.contains("list_projects")
            && !instructions.contains("recall_project"),
        "{instructions}"
    );
}

/// What a connection to one project reads (docs/mcp.md, "Several projects
/// in one connection"): recall and source reads stay on its project, while
/// its graph tools and graph-history tools read another project the key can
/// read when `repo` names it. Revoking the key's access to that project
/// with `graph.access` refuses them too; the connection's own project still
/// answers.
#[test]
fn a_project_connections_graph_tools_read_another_project_until_access_is_revoked() {
    let (f, shop, money) = Fixture::shop_and_money();
    let (credential, reader) = f.reader("reader");
    let app = f.app();
    let call = |name: &str, arguments: Value| {
        api::mcp(
            &app,
            &f.workspace,
            &reader,
            &shop,
            &json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
                "params":{"name":name,"arguments":structured(name, arguments)}}),
        )
        .unwrap()
    };
    let refused =
        |reply: &Value| reply.get("error").is_some() || reply["result"]["isError"] == true;
    let graph_calls = [
        ("get_node", json!({"label":"quantize_cents","repo":money})),
        (
            "query_graph",
            json!({"question":"quantize_cents","repo":"moneylib"}),
        ),
        (
            "graph_as_of",
            json!({"repo":money,"as_of":"latest","query":"quantize_cents"}),
        ),
    ];
    for (name, arguments) in &graph_calls {
        let reply = call(name, arguments.clone());
        assert!(!refused(&reply), "{name}: {reply}");
        assert!(reply.to_string().contains("rounding.py"), "{name}: {reply}");
    }
    for (name, arguments) in [
        (
            "recall_project",
            json!({"question":ABOUT_THE_LIBRARY,"repo":money}),
        ),
        (
            "read_project_source",
            json!({"path":"moneylib/rounding.py","repo":money}),
        ),
    ] {
        let reply = call(name, arguments.clone());
        assert!(refused(&reply), "{name}: {reply}");
        assert!(
            !reply.to_string().contains("scaled = int"),
            "{name}: {reply}"
        );
    }
    f.revoke(&money, &credential);
    for (name, arguments) in &graph_calls {
        let reply = call(name, arguments.clone());
        assert!(refused(&reply), "{name}: {reply}");
        assert!(
            !reply.to_string().contains("rounding.py"),
            "{name}: {reply}"
        );
    }
    let own = call("get_node", json!({"label":"settle_invoice"}));
    assert!(!refused(&own), "{own}");
    assert!(own.to_string().contains("invoice.py"), "{own}");
}

/// A workspace connection remembers what it showed per project (`shown` and
/// `skip_shown`; source ids repeat across projects): the second recall
/// answers each project's windows with references naming that project.
#[test]
fn a_workspace_connection_skips_what_it_showed_per_project() {
    let (f, shop, money) = Fixture::shop_and_money();
    let app = f.app();
    let mut session = api::McpSession::default();
    let mut recall = || {
        api::mcp_workspace_session(
            &app,
            &f.workspace,
            &f.key,
            &json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"recall_project",
                "arguments":{"question":ABOUT_BOTH,"skip_shown":true,"structured":true}}}),
            &mut session,
        )
        .unwrap()["result"]["structuredContent"]
            .clone()
    };
    let first = recall();
    let shown: Vec<&str> = first["sources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|source| s(source, "repository_id"))
        .collect();
    assert!(
        shown.contains(&shop.as_str()) && shown.contains(&money.as_str()),
        "{first}"
    );
    let second = recall();
    let held: Vec<&str> = second["already_shown"]
        .as_array()
        .unwrap_or_else(|| panic!("{second}"))
        .iter()
        .map(|reference| s(reference, "repository_id"))
        .collect();
    assert_eq!(held, shown, "{second}");
    assert!(second["sources"].as_array().unwrap().is_empty(), "{second}");
}

fn graphicle(home: &Path, data: &Path, cwd: &Path, args: &[&str], input: &str) -> Value {
    use std::io::Write;
    let mut command = Command::new(env!("CARGO_BIN_EXE_graphicle"));
    command
        .current_dir(cwd)
        .env("HOME", home)
        .arg("--data-dir")
        .arg(data)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = crate::fork_guard::spawn(&mut command).unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    serde_json::from_str(&stdout).unwrap_or(json!(stdout))
}

/// End to end with the built program: two synced folders, `graphicle mcp
/// --workspace` over stdio answers `list_projects`, workspace recall (citing
/// the library through the import) and `repo`; `graphicle context
/// --workspace` in the shop's folder reads the shop first.
#[test]
fn graphicle_mcp_workspace_serves_every_project_over_stdio() {
    let dir = TempDir::new().unwrap();
    let home = dir.path().canonicalize().unwrap();
    let data = home.join("data");
    for (folder, files) in [("money", money_files()), ("shop", shop_files())] {
        for (path, text) in files {
            let path = home.join(folder).join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
    }
    let money = graphicle(&home, &data, &home.join("money"), &["sync", "."], "");
    let shop = graphicle(&home, &data, &home.join("shop"), &["sync", "."], "");
    assert_eq!(shop["graph"]["status"], "built", "{shop:#}");
    let (money, shop) = (s(&money, "repository_id"), s(&shop, "repository_id"));
    let requests = [
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}),
        json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"list_projects","arguments":{}}}),
        json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"recall_project","arguments":{"question":ABOUT_THE_SHOP,"structured":true}}}),
        json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"recall_project","arguments":{"question":ABOUT_THE_SHOP,"repo":money,"structured":true}}}),
        // Over stdio a graph tool's `repo` may name a synced folder,
        // on a workspace connection too.
        json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"get_node","arguments":{"label":"quantize_cents","repo":"money","structured":true}}}),
    ]
    .map(|r| r.to_string())
    .join("\n")
        + "\n";
    let mut command = Command::new(env!("CARGO_BIN_EXE_graphicle"));
    command
        .current_dir(&home)
        .env("HOME", &home)
        .arg("--data-dir")
        .arg(&data)
        .args(["mcp", "--workspace"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = crate::fork_guard::spawn(&mut command).unwrap();
    {
        use std::io::Write;
        child
            .stdin
            .take()
            .unwrap()
            .write_all(requests.as_bytes())
            .unwrap();
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let replies: Vec<Value> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let result = |id: u64| {
        replies.iter().find(|reply| reply["id"] == id).unwrap()["result"]["structuredContent"]
            .clone()
    };
    let ids: Vec<String> = result(2)["projects"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| s(p, "id").to_string())
        .collect();
    assert_eq!(ids, [money, shop]);
    let workspace = result(3);
    assert_eq!(
        workspace["workspace"]["projects_searched"], 2,
        "{workspace}"
    );
    assert!(window(&workspace, shop, "shopapp/invoice.py").is_some());
    let lifted = window(&workspace, money, "moneylib/rounding.py")
        .unwrap_or_else(|| panic!("no cross-repository window: {}", workspace["evidence"]));
    assert_eq!(lifted["cross_repo"]["direction"], "imported_by");
    assert_eq!(result(4)["repository_id"], money);
    assert_eq!(result(5)["repo"], money);
    assert_eq!(result(5)["nodes"][0]["label"], "quantize_cents()");
    // `context --workspace` in the shop's folder: the shop first.
    let context = graphicle(
        &home,
        &data,
        &home.join("shop"),
        &["context", ABOUT_THE_SHOP, "--workspace"],
        "",
    );
    assert_eq!(context["scope"], "workspace", "{context:#}");
    assert_eq!(context["projects"][0]["repository_id"], shop);
    assert!(window(&context, money, "moneylib/rounding.py").is_some());
    // The work-state tools are a project's: a workspace connection has none
    // to publish, so `--work-state` is refused beside `--workspace`; so are
    // the pull-request tools, which run gh in a project's folder.
    for flag in ["--work-state", "--pull-requests"] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_graphicle"));
        command
            .current_dir(&home)
            .env("HOME", &home)
            .arg("--data-dir")
            .arg(&data)
            .args(["mcp", "--workspace", flag])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let refused = crate::fork_guard::spawn(&mut command)
            .unwrap()
            .wait_with_output()
            .unwrap();
        assert!(!refused.status.success(), "{flag}");
        assert!(
            String::from_utf8_lossy(&refused.stderr).contains(flag),
            "{}",
            String::from_utf8_lossy(&refused.stderr)
        );
    }
}
