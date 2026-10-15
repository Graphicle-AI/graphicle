use graphicle_memory::dispatch;
use serde_json::json;
use std::path::PathBuf;

#[test]
fn core_bridge_does_not_require_or_dispatch_pro_operations() {
    let request = json!({"operation":"graph.impact.diff",
        "base":{"nodes":[],"links":[]},"head":{"nodes":[],"links":[]}});
    assert_eq!(
        dispatch(request).unwrap_err().kind(),
        graphicle_memory::ErrorKind::UnsupportedOperation
    );
    let graph = json!({"directed":true,"multigraph":false,"graph":{},
        "nodes":[{"id":"one","label":"alpha","source_file":"a.py"}],"links":[]});
    let annotated =
        dispatch(json!({"operation":"graph.identity.annotate","graph":graph,"repo":"core"}))
            .unwrap();
    assert!(annotated.is_object());
}

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("graphicle-{}-{nonce}", std::process::id()));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn graph_ids_reach_the_engine_through_the_memory_abi() {
    let result=dispatch(json!({"operation":"graph.ids","values":["İslemYap","Große"],"parts":[["src","session.py","ValidateToken"]]})).unwrap();
    assert_eq!(
        result,
        json!({"normalized":["i_slemyap","grosse"],"made":["src_session_py_validatetoken"]})
    );
}

#[test]
fn completed_engine_reports_coverage_and_rejects_unknown_operations() {
    let info = dispatch(json!({"operation":"graph.info"})).unwrap();
    let based_on = &info["based_on"];
    assert_eq!(based_on["complete_port"], true);
    assert_eq!(based_on["application_modules"], 95);
    assert_eq!(based_on["upstream_version"], "0.9.91");
    assert_eq!(
        based_on["upstream_commit"],
        "10130e79ed9caf8f60d998073e097531eaa1a395"
    );
    assert_eq!(
        dispatch(json!({"operation":"graph.not_implemented"}))
            .unwrap_err()
            .kind(),
        graphicle_memory::ErrorKind::UnsupportedOperation
    );
}

#[test]
fn native_language_extraction_runs_inside_the_existing_memory_core() {
    let scratch = Scratch::new();
    let path = scratch.0.join("widget.rs");
    std::fs::write(&path, "pub struct Widget; impl Widget { pub fn new() -> Widget { Widget } } pub fn build() -> Widget { Widget::new() }").unwrap();
    let parsed = dispatch(json!({"operation":"graph.extract.rust","path":path})).unwrap();
    let nodes = parsed["nodes"].as_array().unwrap();
    let build = &nodes.iter().find(|n| n["label"] == "build()").unwrap()["id"];
    let constructor = &nodes.iter().find(|n| n["label"] == ".new()").unwrap()["id"];
    assert!(
        parsed["edges"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| &e["source"] == build
                && &e["target"] == constructor
                && e["relation"] == "calls")
    );
    let manifest = scratch.0.join("Module.psd1");
    std::fs::write(
        &manifest,
        "@{RootModule='Main.psm1'; RequiredModules=@(@{ModuleName='Pester'; ModuleVersion='5.0'})}",
    )
    .unwrap();
    let parsed =
        dispatch(json!({"operation":"graph.extract.powershell_manifest","path":manifest})).unwrap();
    let targets: Vec<_> = parsed["edges"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["target"].as_str().unwrap())
        .collect();
    assert_eq!(targets, vec!["main", "pester"]);
}

#[test]
fn native_resolution_persists_and_drives_reverse_impact_through_runtime_dispatch() {
    let nodes = json!([
        {"id":"file","label":"main.cs","source_file":"main.cs","file_type":"code"},
        {"id":"ns","label":"Contracts","type":"namespace"},
        {"id":"run","label":".Run()","source_file":"main.cs","file_type":"code"},
        {"id":"iface","label":"IReport","source_file":"iface.cs","file_type":"code","metadata":{"namespace":"Contracts"}},
        {"id":"wrong","label":"IReport","source_file":"other.cs","file_type":"code","metadata":{"namespace":"Unrelated"}},
        {"id":"placeholder","label":"IReport","source_file":"","file_type":"code"},
        {"id":"iface_m","label":".Build()","source_file":"iface.cs","file_type":"code"},
        {"id":"impl","label":"Report","source_file":"impl.cs","file_type":"code"},
        {"id":"impl_m","label":".Build()","source_file":"impl.cs","file_type":"code"}
    ]);
    let edges = json!([
        {"source":"file","target":"ns","relation":"imports","source_file":"main.cs","metadata":{"using_kind":"namespace","target_fqn":"Contracts"}},
        {"source":"run","target":"placeholder","relation":"references","source_file":"main.cs"},
        {"source":"impl","target":"iface","relation":"implements"},
        {"source":"iface","target":"iface_m","relation":"method"},
        {"source":"impl","target":"impl_m","relation":"method"},
        {"source":"run","target":"iface_m","relation":"calls","source_file":"main.cs","source_location":"L42"}
    ]);
    let resolved =
        dispatch(json!({"operation":"graph.csharp.references","nodes":nodes,"edges":edges}))
            .unwrap();
    assert!(
        resolved["edges"]
            .as_array()
            .unwrap()
            .iter()
            .any(|edge| edge["source"] == "run"
                && edge["target"] == "iface"
                && edge["relation"] == "references")
    );
    assert!(
        !resolved["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|node| node["id"] == "placeholder")
    );
    let linked = dispatch(json!({"operation":"graph.csharp_dispatch.resolve","per_file":[],"nodes":resolved["nodes"],"edges":resolved["edges"]})).unwrap();
    assert!(
        linked["edges"]
            .as_array()
            .unwrap()
            .iter()
            .any(|edge| edge["source"] == "iface_m"
                && edge["target"] == "impl_m"
                && edge["relation"] == "dispatches_to")
    );
    let graph = json!({"directed":true,"multigraph":false,"graph":{},"nodes":linked["nodes"],"links":linked["edges"]});
    let scratch = Scratch::new();
    let path = scratch.0.join("graph.json");
    dispatch(json!({"operation":"graph.paths.write_json","path":path,"value":graph,"indent":2}))
        .unwrap();
    let loaded =
        dispatch(json!({"operation":"graph.paths.load_graph","data_or_path":path})).unwrap();
    let hits = dispatch(json!({"operation":"graph.affected.hits","graph":loaded,"seed":"impl_m","relations":["calls","dispatches_to"],"depth":3})).unwrap();
    // `iface_m` is the interface member `impl_m` overrides: since Graphify
    // 0.9.85 it is walked at the seed's own depth, so its caller is one hop
    // from the seed (and `iface_m` is reported through its dispatch edge).
    assert_eq!(
        hits,
        json!([
            {"node_id":"iface_m","depth":1,"via_relation":"dispatches_to","via_file":"impl.cs","via_location":null},
            {"node_id":"run","depth":1,"via_relation":"calls","via_file":"main.cs","via_location":"L42"}
        ])
    );
    assert_eq!(
        std::fs::read_dir(&scratch.0).unwrap().count(),
        1,
        "atomic writes must clean temporary files"
    );
}

#[test]
fn the_memory_facade_can_construct_and_query_a_graph_without_json_dispatch() {
    use graphicle_memory::graph::{AffectedQuery, Graph, GraphHandle};

    let mut graph = Graph::new(true, true);
    graph.node("caller", serde_json::Map::new()).unwrap();
    graph.node("target", serde_json::Map::new()).unwrap();
    graph
        .relate("caller", "target", "calls", serde_json::Map::new())
        .unwrap();
    let handle = GraphHandle::new(graph);
    let hits = AffectedQuery::new("target").run(&handle).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].node_id, "caller");
    assert_eq!(hits[0].via_relation, "calls");
    assert_eq!(hits[0].depth, 1);
}

#[test]
fn resident_graphs_serve_repeated_queries_through_the_runtime_and_the_typed_api() {
    use graphicle_memory::graph::{AffectedQuery, ClusterOptions, GraphHandle, registry};
    let graph = json!({
        "directed": true,
        "nodes": [
            {"id": "main.cs", "label": "main.cs"},
            {"id": "run", "label": "run"},
            {"id": "iface_m", "label": "iface_m"},
            {"id": "impl_m", "label": "impl_m"}
        ],
        "links": [
            {"source": "main.cs", "target": "run", "relation": "contains"},
            {"source": "run", "target": "iface_m", "relation": "calls"},
            {"source": "iface_m", "target": "impl_m", "relation": "dispatches_to"}
        ]
    });
    let expected = dispatch(json!({"operation":"graph.affected.hits","graph":graph,
        "seed":"impl_m","relations":["calls","dispatches_to"],"depth":3}))
    .unwrap();
    let opened =
        dispatch(json!({"operation":"graph.graph.open","name":"bridge","graph":graph})).unwrap();
    assert_eq!(opened["graph_ref"], json!("bridge"));
    assert_eq!(opened["nodes"], json!(4));
    for _ in 0..3 {
        let hits = dispatch(
            json!({"operation":"graph.affected.hits","graph_ref":"bridge",
            "seed":"impl_m","relations":["calls","dispatches_to"],"depth":3}),
        )
        .unwrap();
        assert_eq!(hits, expected);
    }
    let handle: GraphHandle = registry().get("bridge").unwrap();
    let typed = AffectedQuery::new("impl_m")
        .relations(["calls", "dispatches_to"])
        .depth(3)
        .run(&handle)
        .unwrap();
    assert_eq!(json!(typed), expected);
    assert_eq!(
        ClusterOptions::default().partition(&handle).unwrap().len(),
        4
    );
    let listed = dispatch(json!({"operation":"graph.graph.list"})).unwrap();
    let entry = listed["graphs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|info| info["graph_ref"] == "bridge")
        .unwrap();
    assert!(entry["uses"].as_u64().unwrap() >= 4);
    assert_eq!(
        dispatch(json!({"operation":"graph.graph.close","graph_ref":"bridge"})).unwrap(),
        json!({"closed": true})
    );
    let gone =
        dispatch(json!({"operation":"graph.affected.hits","graph_ref":"bridge","seed":"impl_m"}))
            .unwrap_err();
    assert!(gone.to_string().contains("Unknown graph_ref"));
    assert_eq!(
        handle.node_count(),
        4,
        "closed handles stay usable by their holders"
    );
}

#[test]
fn identity_annotation_gives_stable_ids_and_revisions_through_the_runtime_and_the_typed_api() {
    use graphicle_memory::graph::{Graph, Identity, attributes, registry};

    let source =
        "class Report:\n    def build(self):\n        return 1\n\ndef helper():\n    pass\n";
    let mut graph = Graph::new(true, false);
    for (id, label, line) in [
        ("file", "report.py", 1),
        ("Report", "Report", 1),
        ("build", ".build()", 2),
        ("helper", "helper()", 5),
    ] {
        graph
            .node(
                id,
                attributes([
                    ("label", json!(label)),
                    ("source_file", json!("src/report.py")),
                    ("source_location", json!(format!("L{line}"))),
                    ("file_type", json!("code")),
                ]),
            )
            .unwrap();
    }
    graph
        .relate(
            "file",
            "Report",
            "contains",
            attributes([("confidence", json!("EXTRACTED"))]),
        )
        .unwrap();
    graph
        .relate(
            "Report",
            "build",
            "method",
            attributes([("confidence", json!("EXTRACTED"))]),
        )
        .unwrap();
    graph
        .relate(
            "helper",
            "build",
            "calls",
            attributes([("confidence", json!("INFERRED"))]),
        )
        .unwrap();

    let identity = Identity::new()
        .repo("demo")
        .strip_prefix("src/")
        .source("report.py", source);
    let mut typed = graph.clone();
    let summary = identity.annotate(&mut typed).unwrap();
    assert_eq!(summary.nodes, 4);
    assert_eq!(summary.with_revision, 4);
    assert!(summary.missing_sources.is_empty());
    let build = typed.nodes().iter().find(|n| n.id == "build").unwrap();
    assert_eq!(build.attributes["qualified_name"], json!("Report.build"));
    assert_eq!(build.attributes["repo"], json!("demo"));

    // Editing the helper changes only the helper's revision; stable ids hold.
    let edited = format!("{source}    return 2\n");
    let mut after = graph.clone();
    Identity::new()
        .repo("demo")
        .strip_prefix("src/")
        .source("report.py", edited.clone())
        .annotate(&mut after)
        .unwrap();
    for (before, now) in typed.nodes().iter().zip(after.nodes()) {
        assert_eq!(before.attributes["stable_id"], now.attributes["stable_id"]);
        let changed = before.attributes["revision"] != now.attributes["revision"];
        assert_eq!(
            changed,
            before.id == "helper" || before.id == "file",
            "{}",
            before.id
        );
    }

    let boundary = dispatch(json!({
        "operation": "graph.identity.annotate", "repo": "demo", "strip_prefix": "src/",
        "sources": {"report.py": source}, "graph": graph.to_node_link("links"),
        "open": "identity-bridge",
    }))
    .unwrap();
    assert_eq!(boundary["summary"], json!(summary));
    assert_eq!(boundary["graph_ref"], json!("identity-bridge"));
    let resident = registry().get("identity-bridge").unwrap();
    assert_eq!(resident.to_node_link("links"), typed.to_node_link("links"));
    drop(resident);
    assert!(registry().close("identity-bridge"));
}

#[test]
fn retrieval_entry_points_serve_resident_graphs_through_the_runtime_and_the_typed_api() {
    use graphicle_memory::graph::{
        ExpandOptions, Graph, Retriever, TraversalMode, attributes, registry,
    };

    let mut graph = Graph::new(true, false);
    for (id, label, file) in [
        ("app", "app.py", "src/app.py"),
        ("handler", "request_handler()", "src/app.py"),
        ("helper", "parse_payload()", "src/util.py"),
        ("doc", "Design notes", "docs/design.md"),
    ] {
        graph
            .node(
                id,
                attributes([
                    ("label", json!(label)),
                    ("source_file", json!(file)),
                    ("file_type", json!("code")),
                ]),
            )
            .unwrap();
    }
    for (s, t, relation, context) in [
        ("app", "handler", "contains", "code"),
        ("handler", "helper", "calls", "code"),
        ("doc", "handler", "mentions", "docs"),
    ] {
        graph
            .relate(
                s,
                t,
                relation,
                attributes([
                    ("confidence", json!("EXTRACTED")),
                    ("context", json!(context)),
                ]),
            )
            .unwrap();
    }

    let retriever = Retriever::new(&graph);
    let options = ExpandOptions {
        depth: 2,
        mode: TraversalMode::Bfs,
        filters: vec!["code".into()],
        question: String::new(),
    };
    let (candidates, expansion) = retriever.retrieve("parse payload", 5, &options).unwrap();
    assert_eq!(candidates.seeds, vec!["helper"]);
    assert_eq!(candidates.candidates[0].node_id, "helper");
    assert_eq!(
        candidates.candidates[0].source_file.as_deref(),
        Some("src/util.py")
    );
    let mut reached: Vec<&str> = expansion.nodes.iter().map(|n| n.node_id.as_str()).collect();
    reached.sort_unstable();
    // The `docs` edge is filtered out, so the design note is never reached.
    assert_eq!(reached, vec!["app", "handler", "helper"]);
    assert!(expansion.edges.iter().all(|e| e.confidence == "EXTRACTED"));

    let opened = dispatch(json!({
        "operation": "graph.graph.open", "graph": graph.to_node_link("links"),
        "name": "retrieval-bridge",
    }))
    .unwrap();
    assert_eq!(opened["graph_ref"], json!("retrieval-bridge"));
    let boundary_candidates = dispatch(json!({
        "operation": "graph.retrieval.candidates", "graph_ref": "retrieval-bridge",
        "query": "parse payload", "limit": 5,
    }))
    .unwrap();
    assert_eq!(boundary_candidates, json!(candidates));
    let boundary_expansion = dispatch(json!({
        "operation": "graph.retrieval.expand", "graph_ref": "retrieval-bridge",
        "nodes": candidates.seeds, "depth": 2, "mode": "bfs", "filters": ["code"],
    }))
    .unwrap();
    assert_eq!(boundary_expansion, json!(expansion));
    assert!(registry().close("retrieval-bridge"));
    let closed = dispatch(json!({
        "operation": "graph.retrieval.candidates", "graph_ref": "retrieval-bridge",
        "query": "parse payload",
    }));
    assert!(closed.is_err());
}
