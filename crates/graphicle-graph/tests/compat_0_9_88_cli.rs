// Parity tests for the Graphify 0.9.78-0.9.88 command line, query surface,
// PR and model changes (Apache-2.0; see NOTICE.md).
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

fn run(args: &[&str]) -> graphicle::cli::Output {
    graphicle::cli::run(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
}

/// A node-link graph from `(id, label, source_file, source_location)` nodes
/// and `(source, target, relation)` links.
fn node_link(nodes: &[(&str, &str, &str, &str)], links: &[(&str, &str, &str)]) -> Value {
    json!({
        "directed": false, "multigraph": false, "graph": {},
        "nodes": nodes.iter().map(|(id, label, sf, loc)| json!({
            "id": id, "label": label, "source_file": sf, "source_location": loc,
        })).collect::<Vec<_>>(),
        "links": links.iter().map(|(a, b, rel)| json!({
            "source": a, "target": b, "relation": rel, "confidence": "EXTRACTED",
        })).collect::<Vec<_>>(),
    })
}

fn write(dir: &Path, graph: &Value) -> String {
    let gp = dir.join("graph.json");
    fs::write(&gp, graph.to_string()).unwrap();
    gp.to_str().unwrap().to_string()
}

/// Run `args --graph <graph>` on a fresh copy of `graph`.
fn cli(graph: &Value, args: &[&str]) -> graphicle::cli::Output {
    let dir = tempfile::tempdir().unwrap();
    let gp = write(dir.path(), graph);
    let mut argv = args.to_vec();
    argv.extend(["--graph", gp.as_str()]);
    run(&argv)
}

fn serve(action: &str, request: Value) -> Value {
    let mut request = request;
    request["operation"] = json!("serve");
    request["action"] = json!(action);
    graphicle::dispatch(&request).unwrap()
}

fn lookup_graph() -> Value {
    node_link(
        &[
            ("src_a_alphaclient", "AlphaClient", "src/a.ts", "L1"),
            ("src_a_alphaclient_nonce", ".nonce()", "src/a.ts", "L2"),
            ("src_b_betaclient", "BetaClient", "src/b.ts", "L1"),
            ("src_b_betaclient_nonce", ".nonce()", "src/b.ts", "L2"),
            ("src_c_run", "run()", "src/c.ts", "L1"),
            ("src_d_island", "island()", "src/d.ts", "L1"),
        ],
        &[
            ("src_c_run", "src_b_betaclient", "calls"),
            ("src_b_betaclient", "src_b_betaclient_nonce", "method"),
            ("src_a_alphaclient", "src_a_alphaclient_nonce", "method"),
        ],
    )
}

#[test]
fn lookups_exit_0_answered_3_ambiguous_4_not_found() {
    // Graphify 0.9.86 (#4320): a script tells an answer, an ambiguous name and
    // a miss apart by the exit code alone.
    assert_eq!(
        (
            graphicle::cli::EXIT_AMBIGUOUS,
            graphicle::cli::EXIT_NOT_FOUND
        ),
        (3, 4)
    );
    let g = lookup_graph();
    let out = cli(&g, &["explain", "run()"]);
    assert_eq!(out.exit_code, 0, "{}", out.stdout);
    assert!(
        out.stdout.contains("ID:        src_c_run"),
        "{}",
        out.stdout
    );
    let out = cli(&g, &["explain", ".nonce()"]);
    assert_eq!(out.exit_code, 3);
    assert!(
        out.stdout
            .contains("Ambiguous: '.nonce()' matches 2 nodes in different files."),
        "{}",
        out.stdout
    );
    for label in ["NoSuchSymbolXyz", "src/a.ts::NotThere"] {
        let out = cli(&g, &["explain", label]);
        assert_eq!(out.exit_code, 4, "{label}");
        assert!(
            out.stdout
                .contains(&format!("No node matching '{label}' found.")),
            "{}",
            out.stdout
        );
    }
    let out = cli(&g, &["path", "run()", "src/b.ts::.nonce()"]);
    assert_eq!(out.exit_code, 0, "{}", out.stderr);
    assert!(
        out.stdout.contains("Shortest path (2 hops):"),
        "{}",
        out.stdout
    );
    // No route between two existing endpoints is an answer, not a miss.
    let out = cli(&g, &["path", "run()", "island()"]);
    assert_eq!(out.exit_code, 0, "{}", out.stderr);
    assert!(
        out.stdout.contains("No directed path found"),
        "{}",
        out.stdout
    );
    let out = cli(&g, &["path", "run()", ".nonce()"]);
    assert_eq!(out.exit_code, 3);
    assert!(
        out.stderr.contains("Ambiguous: '.nonce()'"),
        "{}",
        out.stderr
    );
    let out = cli(&g, &["path", "run()", "src_c_run"]);
    assert_eq!(out.exit_code, 3);
    assert!(
        out.stderr.contains("both resolved to the same node"),
        "{}",
        out.stderr
    );
    for endpoint in ["NoSuchSymbolXyz", "src/a.ts::NotThere"] {
        let out = cli(&g, &["path", "run()", endpoint]);
        assert_eq!(out.exit_code, 4, "{endpoint}: {}", out.stderr);
        assert!(!out.stdout.contains("Shortest path"), "{}", out.stdout);
    }
    let out = cli(&g, &["query", "run"]);
    assert_eq!(out.exit_code, 0, "{}", out.stderr);
    assert!(out.stdout.contains("NODE run()"), "{}", out.stdout);
    let out = cli(&g, &["query", "zzqqxx"]);
    assert_eq!(out.exit_code, 4);
    assert!(
        out.stdout.contains("No matching nodes found."),
        "{}",
        out.stdout
    );
}

fn hub_graph(callers: usize) -> Value {
    let mut nodes = vec![json!({"id": "hub", "label": "hub()", "source_file": "lib/hub.py"})];
    let mut links = vec![];
    for i in 0..callers {
        let file = format!("app/mod{}.py", i % 3);
        nodes.push(json!({"id": format!("caller_{i}"), "label": format!("caller_{i}()"), "source_file": file}));
        links.push(json!({"source": format!("caller_{i}"), "target": "hub", "relation": "calls",
            "confidence": "EXTRACTED", "source_file": file, "source_location": format!("L{}", 10 + i)}));
    }
    json!({"directed": false, "multigraph": false, "graph": {}, "nodes": nodes, "links": links})
}

#[test]
fn explain_cut_names_the_total_and_all_lists_every_connection() {
    // Graphify 0.9.86 (#4323): `explain --all`, and the cut says so.
    let g = hub_graph(25);
    let out = cli(&g, &["explain", "hub"]);
    assert!(out.stdout.contains("Connections (25):"), "{}", out.stdout);
    assert!(
        out.stdout.contains("... and 5 more (--all lists all 25)"),
        "{}",
        out.stdout
    );
    let out = cli(&g, &["explain", "hub", "--all"]);
    assert_eq!(out.exit_code, 0, "{}", out.stderr);
    let listed = out
        .stdout
        .lines()
        .filter(|l| l.starts_with("  <-- caller_"))
        .count();
    assert_eq!(listed, 25, "{}", out.stdout);
    assert!(!out.stdout.contains("more"), "{}", out.stdout);
    assert!(!out.stdout.contains("Grouped by file:"), "{}", out.stdout);
}

fn budget_hint(text: &str) -> i64 {
    let at = text
        .find("token budget to ")
        .unwrap_or_else(|| panic!("no budget hint in {text}"));
    text[at + 16..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .unwrap()
}

#[test]
fn truncation_names_the_budget_of_the_full_answer() {
    let ids = (0..60).map(|i| format!("n{i}")).collect::<Vec<_>>();
    let nodes = ids
        .iter()
        .enumerate()
        .map(|(i, id)| json!({"id": id, "label": format!("node_number_{i}"), "source_file": format!("src/f{i}.py"), "source_location": "L1", "community": 0}))
        .collect::<Vec<_>>();
    let links = (1..60)
        .map(|i| json!({"source": ids[i - 1], "target": ids[i], "relation": "calls", "confidence": "EXTRACTED"}))
        .collect::<Vec<_>>();
    let graph = json!({"directed": false, "multigraph": false, "graph": {}, "nodes": nodes, "links": links});
    let edges = (1..60)
        .map(|i| json!([ids[i - 1], ids[i]]))
        .collect::<Vec<_>>();
    let text = |budget: i64| {
        serve(
            "subgraph_text",
            json!({"graph": graph, "nodes": ids, "edges": edges, "token_budget": budget, "seeds": ["n0"]}),
        )
        .as_str()
        .unwrap()
        .to_string()
    };
    let cut = text(50);
    assert!(cut.starts_with("[!] TRUNCATED"), "{cut}");
    let needed = budget_hint(&cut);
    assert!(cut.contains(&format!("--budget {needed}")), "{cut}");
    assert!(cut.contains(&format!("token_budget={needed}")), "{cut}");
    let full = text(needed);
    assert!(!full.contains("TRUNCATED"), "{full}");
    assert_eq!(
        full.matches("\nNODE ").count() + usize::from(full.starts_with("NODE ")),
        60
    );
    assert_eq!(full.matches("\nEDGE ").count(), 59);
    assert!(text(needed - 1).starts_with("[!] TRUNCATED"));

    let lines = (0..200)
        .map(|i| format!("  --> node{i} [calls] [EXTRACTED]"))
        .collect::<Vec<_>>();
    let cut_lines = |budget: i64, hint: &str| {
        serve(
            "cut_lines",
            json!({"lines": lines, "token_budget": budget, "hint": hint}),
        )
        .as_str()
        .unwrap()
        .to_string()
    };
    let cut = cut_lines(20, "use relation_filter");
    let needed = budget_hint(&cut);
    assert!(cut.contains(&format!("token_budget={needed}")), "{cut}");
    assert_eq!(cut_lines(needed, "x"), lines.join("\n"));
    assert!(cut_lines(needed - 1, "x").starts_with("[!] TRUNCATED"));
}

fn same_file_twins(reverse: bool) -> Value {
    let mut g = node_link(
        &[
            ("src_x", "x.ts", "src/x.ts", "L1"),
            ("src_x_run", "run()", "src/x.ts", "L3"),
            ("src_x_alphaclient", "AlphaClient", "src/x.ts", "L5"),
            ("src_x_alphaclient_nonce", ".nonce()", "src/x.ts", "L6"),
            ("src_x_betaclient", "BetaClient", "src/x.ts", "L9"),
            ("src_x_betaclient_nonce", ".nonce()", "src/x.ts", "L10"),
            (
                "tests_a_php_entitymanagerinterface",
                "EntityManagerInterface",
                "",
                "",
            ),
            (
                "tests_b_php_entitymanagerinterface",
                "EntityManagerInterface",
                "",
                "",
            ),
        ],
        &[
            ("src_x", "src_x_run", "contains"),
            ("src_x_run", "src_x_alphaclient", "calls"),
            ("src_x_run", "src_x_betaclient", "calls"),
            ("src_x_alphaclient", "src_x_alphaclient_nonce", "method"),
            ("src_x_betaclient", "src_x_betaclient_nonce", "method"),
        ],
    );
    if reverse {
        g["nodes"].as_array_mut().unwrap().reverse();
    }
    g
}

#[test]
fn explain_and_get_node_refuse_same_file_twins() {
    // Graphify 0.9.86 (#4321): one file's same-named symbols are refused too.
    for label in [".nonce()", "src/x.ts::.nonce()"] {
        let out = cli(&same_file_twins(false), &["explain", label]);
        assert!(
            out.stdout.contains(&format!(
                "Ambiguous: '{label}' matches 2 nodes in src/x.ts."
            )),
            "{}",
            out.stdout
        );
        assert!(out.stdout.contains("src_x_alphaclient_nonce"));
        assert!(out.stdout.contains("src_x_betaclient_nonce"));
        assert!(!out.stdout.contains("Node: .nonce()"));
        assert_eq!(out.exit_code, 3);
    }
    let out = cli(
        &same_file_twins(false),
        &["explain", "EntityManagerInterface"],
    );
    assert!(
        out.stdout
            .starts_with("Ambiguous: 'EntityManagerInterface' matches 2 nodes"),
        "{}",
        out.stdout
    );
    assert_eq!(out.exit_code, 3);
    let ids = |out: &graphicle::cli::Output| {
        let mut ids = out
            .stdout
            .lines()
            .map(str::trim)
            .filter(|l| l.starts_with("id:"))
            .map(str::to_owned)
            .collect::<Vec<_>>();
        ids.sort();
        ids
    };
    let forward = cli(&same_file_twins(false), &["explain", ".nonce()"]);
    let reverse = cli(&same_file_twins(true), &["explain", ".nonce()"]);
    assert_eq!(ids(&forward), ids(&reverse));
    assert_eq!(
        ids(&forward),
        ["id: src_x_alphaclient_nonce", "id: src_x_betaclient_nonce"]
    );
    let out = cli(
        &same_file_twins(false),
        &["explain", "src_x_betaclient_nonce"],
    );
    assert!(out.stdout.contains("ID:        src_x_betaclient_nonce"));
    assert_eq!(out.exit_code, 0);
    // A file path matches the file node and its members: precedence, not a tie.
    let out = cli(&same_file_twins(false), &["explain", "src/x.ts"]);
    assert!(out.stdout.contains("ID:        src_x\n"), "{}", out.stdout);
    assert_eq!(out.exit_code, 0);
    let mut g = same_file_twins(false);
    g["directed"] = json!(true);
    let resolved = serve("resolve", json!({"graph": g, "label": ".nonce()"}));
    assert!(resolved[0].is_null());
    assert!(
        resolved[1]
            .as_str()
            .unwrap()
            .starts_with("Ambiguous: '.nonce()' matches 2 nodes in src/x.ts."),
        "{resolved}"
    );
    let resolved = serve(
        "resolve",
        json!({"graph": g, "label": "src_x_alphaclient_nonce"}),
    );
    assert_eq!(resolved, json!(["src_x_alphaclient_nonce", null]));
}

fn cross_file_symbol_graph() -> Value {
    node_link(
        &[
            (
                "src_billing_route",
                "route.ts",
                "src/billing/route.ts",
                "L1",
            ),
            (
                "src_billing_route_readroute",
                "readRoute()",
                "src/billing/route.ts",
                "L3",
            ),
            ("src_seats_route", "route.ts", "src/seats/route.ts", "L1"),
            (
                "src_seats_route_readroute",
                "readRoute()",
                "src/seats/route.ts",
                "L3",
            ),
            ("src_admin_route", "route.ts", "src/admin/route.ts", "L1"),
            (
                "src_admin_route_listadmins",
                "listAdmins()",
                "src/admin/route.ts",
                "L3",
            ),
            (
                "src_billing_charge_chargecard",
                "chargeCard()",
                "src/billing/charge.ts",
                "L2",
            ),
        ],
        &[
            (
                "src_billing_route",
                "src_billing_route_readroute",
                "contains",
            ),
            ("src_seats_route", "src_seats_route_readroute", "contains"),
            ("src_admin_route", "src_admin_route_listadmins", "contains"),
            (
                "src_billing_route_readroute",
                "src_billing_charge_chargecard",
                "calls",
            ),
        ],
    )
}

#[test]
fn path_file_scoped_endpoint_fails_closed() {
    // Graphify 0.9.84 (#4264): a `file::symbol` endpoint whose file defines no
    // such symbol is refused, naming the files that do.
    let g = cross_file_symbol_graph();
    let out = cli(&g, &["path", "src/admin/route.ts::readRoute", "chargeCard"]);
    assert_eq!(out.exit_code, 4, "{}", out.stderr);
    assert!(!out.stdout.contains("Shortest path"), "{}", out.stdout);
    assert_eq!(
        out.stderr,
        "No node matching 'src/admin/route.ts::readRoute' found; src/admin/route.ts defines no 'readRoute'.\n  defined in src/billing/route.ts\n  defined in src/seats/route.ts\n"
    );
    let mut directed = g.clone();
    directed["directed"] = json!(true);
    let tool = serve(
        "shortest_path_text",
        json!({"graph": directed, "arguments": {"source": "src/admin/route.ts::readRoute", "target": "chargeCard"}}),
    );
    let tool = tool.as_str().unwrap();
    assert!(!tool.contains("Shortest path"), "{tool}");
    assert!(tool.contains("src/admin/route.ts") && tool.contains("readRoute"));
    let out = cli(
        &g,
        &["path", "src/billing/route.ts::readRoute", "chargeCard"],
    );
    assert!(
        out.stdout
            .contains("readRoute() --calls [EXTRACTED]--> chargeCard()"),
        "{}",
        out.stdout
    );
}

#[test]
fn dotted_python_symbols_resolve_by_module_path() {
    // Graphify 0.9.88 (#4366): `pkg.mod.func`, `pkg.mod:func`, `pkg.mod.func()`.
    let g = json!({"directed": true, "multigraph": false, "graph": {}, "links": [], "nodes": [
        {"id": "cli_main", "label": "main()", "source_file": "src/pkg/cli.py", "source_location": "L26"},
        {"id": "other_main", "label": "main()", "source_file": "src/pkg/other.py", "source_location": "L5"},
        {"id": "engine_scan", "label": "scan()", "source_file": "src/pkg/engine/__init__.py", "source_location": "L9"},
        {"id": "wf", "label": "scan job", "source_file": ".github/workflows/scan.yml", "source_location": "L1"},
    ]});
    for (query, expected) in [
        ("pkg.cli.main", "cli_main"),
        ("pkg.cli:main", "cli_main"),
        ("pkg.other.main()", "other_main"),
        ("pkg.engine.scan", "engine_scan"),
    ] {
        let resolved = serve("resolve", json!({"graph": g, "label": query}));
        assert_eq!(resolved, json!([expected, null]), "{query}");
    }
    let missing = serve("resolve", json!({"graph": g, "label": "pkg.missing.main"}));
    assert!(missing[0].is_null() && missing[1].is_string(), "{missing}");
    // A function and a same-named method of one module tie, for every lookup.
    let tie = json!({"directed": true, "multigraph": false, "graph": {}, "links": [], "nodes": [
        {"id": "f", "label": "run()", "source_file": "pkg/mod.py", "source_location": "L1"},
        {"id": "m", "label": ".run()", "source_file": "pkg/mod.py", "source_location": "L9"},
        {"id": "x", "label": "other()", "source_file": "pkg/x.py", "source_location": "L1"},
    ]});
    let resolved = serve("resolve", json!({"graph": tie, "label": "pkg.mod.run"}));
    assert!(resolved[0].is_null(), "{resolved}");
    assert!(resolved[1].as_str().unwrap().starts_with("Ambiguous"));
    let tool = serve(
        "shortest_path_text",
        json!({"graph": tie, "arguments": {"source": "pkg.mod.run", "target": "other()"}}),
    );
    assert!(tool.as_str().unwrap().starts_with("Ambiguous"), "{tool}");
    assert_eq!(
        serve(
            "python_qualified",
            json!({"graph": tie, "label": " pkg.mod:run "})
        ),
        json!(["f", "m"])
    );
}

fn seed_graph() -> Value {
    json!({"directed": false, "multigraph": false, "graph": {}, "nodes": [
        {"id": "n1", "label": "extract", "source_file": "pkg/extract.py", "source_location": "L10", "community": 0},
        {"id": "n2", "label": "cluster", "source_file": "pkg/cluster.py", "source_location": "L5", "community": 0},
        {"id": "n3", "label": "build", "source_file": "pkg/build.py", "source_location": "L1", "community": 1},
        {"id": "n4", "label": "report", "source_file": "pkg/report.py", "source_location": "L1", "community": 2},
    ], "links": [
        {"source": "n1", "target": "n2", "relation": "calls", "confidence": "EXTRACTED", "context": "call"},
        {"source": "n3", "target": "n4", "relation": "imports", "confidence": "EXTRACTED", "context": "import"},
    ]})
}

fn query(graph: &Value, question: &str, extra: Value) -> String {
    let mut request = json!({"graph": graph, "question": question, "depth": 1});
    for (k, v) in extra.as_object().unwrap() {
        request[k] = v.clone();
    }
    serve("query_text", request).as_str().unwrap().to_string()
}

#[test]
fn given_seeds_replace_keyword_seeds() {
    // Graphify 0.9.88 (#4293): `query --seed`.
    let g = seed_graph();
    let plain = query(&g, "extract", json!({}));
    assert!(plain.contains("Start: ['extract']") && !plain.contains("NODE build"));
    let seeded = query(&g, "extract", json!({"seeds": ["build"]}));
    assert!(seeded.contains("Start (given): ['build']"), "{seeded}");
    assert!(seeded.contains("NODE build") && seeded.contains("NODE report"));
    assert!(!seeded.contains("NODE extract"), "{seeded}");
    let text = query(
        &g,
        "anything",
        json!({"seeds": ["pkg/build.py", "n1", "n3"]}),
    );
    assert!(
        text.contains("Start (given): ['build', 'extract']"),
        "{text}"
    );
    let text = query(
        &g,
        "anything",
        json!({"seeds": ["no_such_node", "cluster"]}),
    );
    assert!(text.contains("Start (given): ['cluster']"), "{text}");
    assert!(
        text.contains("Unresolved seeds (skipped): ['no_such_node']"),
        "{text}"
    );
    assert_eq!(
        query(&g, "extract", json!({"seeds": ["no_such_node"]})),
        "No unique node match for --seed: no_such_node"
    );
    let text = query(
        &g,
        "anything",
        json!({"seeds": ["nope", "cluster", "nope"]}),
    );
    assert!(
        text.contains("Unresolved seeds (skipped): ['nope']"),
        "{text}"
    );
    let root = tempfile::tempdir().unwrap();
    let absolute = root.path().join("pkg/report.py");
    let text = query(
        &g,
        "anything",
        json!({"seeds": [absolute.to_str().unwrap()], "seed_root": root.path().to_str().unwrap()}),
    );
    assert!(text.contains("Start (given): ['report']"), "{text}");
    let mut callable = g.clone();
    callable["nodes"].as_array_mut().unwrap().push(json!({"id": "n5", "label": "refresh_token()", "source_file": "pkg/auth.py", "source_location": "L7", "community": 3}));
    for value in ["refresh_token", "refresh_token()"] {
        let text = query(&callable, "anything", json!({"seeds": [value]}));
        assert!(
            text.contains("Start (given): ['refresh_token()']"),
            "{value}: {text}"
        );
    }
    let mut filtered = g.clone();
    filtered["links"].as_array_mut().unwrap().push(json!({"source": "n3", "target": "n2", "relation": "calls", "confidence": "EXTRACTED", "context": "call"}));
    let kept = query(
        &filtered,
        "anything",
        json!({"seeds": ["build"], "context_filters": ["call"]}),
    );
    assert!(kept.contains("Start (given): ['build']") && kept.contains("Context: call (explicit)"));
    assert!(
        kept.contains("NODE cluster") && !kept.contains("NODE report"),
        "{kept}"
    );
}

#[test]
fn query_seed_flag_is_repeatable_and_fails_closed_without_a_value() {
    // Resolved, as pytest's tmp_path is: the graph path is resolved too.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let out_dir = root.join(".graphicle");
    fs::create_dir(&out_dir).unwrap();
    let gp = out_dir.join("graph.json");
    fs::write(&gp, seed_graph().to_string()).unwrap();
    let gp = gp.to_str().unwrap();
    let out = run(&[
        "query",
        "extract",
        "--seed",
        "build",
        "--seed=pkg/cluster.py",
        "--graph",
        gp,
    ]);
    assert!(
        out.stdout.contains("Start (given): ['build', 'cluster']"),
        "{}{}",
        out.stdout,
        out.stderr
    );
    // An absolute seed path is anchored at the graph's repository root.
    let absolute = root.join("pkg/build.py");
    let out = run(&[
        "query",
        "q",
        "--seed",
        absolute.to_str().unwrap(),
        "--graph",
        gp,
    ]);
    assert!(
        out.stdout.contains("Start (given): ['build']"),
        "{}",
        out.stdout
    );
    for argv in [
        vec!["query", "extract", "--graph", gp, "--seed"],
        vec!["query", "extract", "--seed=", "--graph", gp],
    ] {
        let out = run(&argv);
        assert_eq!(out.exit_code, 1, "{argv:?}");
        assert!(
            out.stderr.contains("--seed needs a value"),
            "{}",
            out.stderr
        );
        assert!(!out.stdout.contains("Start"), "{}", out.stdout);
    }
}

#[test]
fn given_seed_renders_first_and_survives_truncation() {
    let mut nodes = vec![
        json!({"id": "hub", "label": "Hub", "source_file": "hub.py", "source_location": "L1", "community": 0}),
    ];
    let mut links = vec![];
    for i in 0..40 {
        nodes.push(json!({"id": format!("s{i}"), "label": format!("spoke{i}"), "source_file": format!("s{i}.py"), "source_location": "L1", "community": 0}));
        links.push(json!({"source": "hub", "target": format!("s{i}"), "relation": "calls", "confidence": "EXTRACTED"}));
    }
    nodes.push(json!({"id": "answer", "label": "CompanySpacingGate", "source_file": "gate.py", "source_location": "L12", "community": 0}));
    links.push(
        json!({"source": "s0", "target": "answer", "relation": "calls", "confidence": "EXTRACTED"}),
    );
    let g = json!({"directed": false, "multigraph": false, "graph": {}, "nodes": nodes, "links": links});
    let text = query(
        &g,
        "hub",
        json!({"depth": 2, "token_budget": 40, "seeds": ["answer"]}),
    );
    assert!(
        text.contains("Start (given): ['CompanySpacingGate']"),
        "{text}"
    );
    let first = text.lines().find(|l| l.starts_with("NODE ")).unwrap();
    assert!(first.contains("CompanySpacingGate"), "{text}");
    assert!(text.contains("TRUNCATED"), "{text}");
}

fn order_graph() -> Value {
    let mut nodes = vec![
        json!({"id": "top", "label": "TopLevelRouter", "source_file": "router.py", "source_location": "L1"}),
        json!({"id": "apple_weak", "label": "apple_pie_formatting_helper", "source_file": "fmt.py", "source_location": "L3"}),
        json!({"id": "zebra_exact", "label": "zebra", "source_file": "zebra.py", "source_location": "L1"}),
    ];
    for i in 0..6 {
        nodes.push(json!({"id": format!("apple{i}"), "label": format!("apple_util_{i}"), "source_file": format!("apples{i}.py"), "source_location": "L1"}));
    }
    json!({"directed": false, "multigraph": false, "graph": {}, "nodes": nodes, "links": [
        {"source": "top", "target": "apple_weak"}, {"source": "top", "target": "zebra_exact"},
    ]})
}

#[test]
fn per_term_seeds_follow_match_strength() {
    // Graphify 0.9.87 (#4357).
    let g = order_graph();
    let scored = json!([[1000.0, "top"], [1.0, "apple_weak"], [1.0, "zebra_exact"]]);
    let best = json!({"apple": "apple_weak", "zebra": "zebra_exact"});
    let pick = |scored: &Value, order: Value| {
        serve(
            "pick_seeds",
            json!({"graph": g, "scored": scored, "best_seed_by_term": best, "term_order": order}),
        )
    };
    assert_eq!(
        pick(&scored, json!(["zebra", "apple"])),
        json!(["top", "zebra_exact", "apple_weak"])
    );
    assert_eq!(
        pick(&scored, Value::Null),
        json!(["top", "apple_weak", "zebra_exact"])
    );
    assert_eq!(
        pick(&json!([[1000.0, "top"]]), json!(["zebra"])),
        json!(["top", "zebra_exact", "apple_weak"])
    );
    let qs = serve(
        "score_query",
        json!({"graph": g, "terms": ["apple", "zebra"], "collect_per_term_seeds": true}),
    );
    assert_eq!(qs["term_order"][0], "zebra", "{qs}");
    assert_eq!(qs["term_order"].as_array().unwrap().len(), 2);
    assert_eq!(qs["best_seed_by_term"]["zebra"], "zebra_exact");

    let mut nodes = vec![];
    for name in ["alpha", "bravo", "charlie", "zebra", "apple"] {
        nodes.push(json!({"id": name, "label": name, "source_file": format!("{name}.py"), "source_location": "L1"}));
    }
    for i in 0..8 {
        nodes.push(json!({"id": format!("apple{i}"), "label": format!("apple_util_{i}"), "source_file": format!("apples{i}.py"), "source_location": "L1"}));
    }
    let five =
        json!({"directed": false, "multigraph": false, "graph": {}, "nodes": nodes, "links": []});
    let text = query(
        &five,
        "alpha bravo charlie apple zebra",
        json!({"mode": "bfs"}),
    );
    assert_eq!(
        text,
        query(
            &five,
            "alpha bravo charlie apple zebra",
            json!({"mode": "bfs"})
        )
    );
    let start = text.split("Start: ").nth(1).unwrap();
    let start = &start[..start.find(']').unwrap()];
    assert!(
        start.find("'charlie'").unwrap() < start.find("'apple'").unwrap(),
        "{start}"
    );
}

fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .args([
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
        ])
        .args(args)
        .current_dir(dir)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn stats(graph_path: &Path) -> String {
    let out = serve(
        "rpc",
        json!({"path": graph_path.to_str().unwrap(), "request": {"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "graph_stats", "arguments": {}}}}),
    );
    out["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .to_string()
}

#[test]
fn graph_stats_name_the_build_commit_and_staleness() {
    // Graphify 0.9.80 (#3354): the build commit, and whether HEAD moved on.
    let dir = tempfile::tempdir().unwrap();
    let mut g = lookup_graph();
    let gp = dir.path().join("graph.json");
    fs::write(&gp, g.to_string()).unwrap();
    assert!(!stats(&gp).contains("Built at commit"), "{}", stats(&gp));
    for junk in [
        json!(null),
        json!(0),
        json!(""),
        json!("   "),
        json!(["x"]),
        json!({"a": 1}),
    ] {
        g["built_at_commit"] = junk;
        fs::write(&gp, g.to_string()).unwrap();
        assert!(!stats(&gp).contains("Built at commit"), "{}", stats(&gp));
    }
    g["built_at_commit"] = json!("  0123456789abcdef  ");
    fs::write(&gp, g.to_string()).unwrap();
    let text = stats(&gp);
    if git(dir.path(), &["rev-parse", "HEAD"]).is_none() {
        assert!(
            text.ends_with("AMBIGUOUS: 0%\nBuilt at commit: 0123456789abcdef\n"),
            "{text}"
        );
    }
    let repo = tempfile::tempdir().unwrap();
    if git(repo.path(), &["init", "-q"]).is_none()
        || git(
            repo.path(),
            &["commit", "-q", "--allow-empty", "-m", "fixture"],
        )
        .is_none()
    {
        return;
    }
    let head = git(repo.path(), &["rev-parse", "HEAD"]).unwrap();
    let gp = repo.path().join("graph.json");
    g["built_at_commit"] = json!(head);
    fs::write(&gp, g.to_string()).unwrap();
    assert!(
        stats(&gp).ends_with(&format!("Built at commit: {head} (matches HEAD)\n")),
        "{}",
        stats(&gp)
    );
    g["built_at_commit"] = json!("feedfacecafe");
    fs::write(&gp, g.to_string()).unwrap();
    assert!(
        stats(&gp).ends_with(&format!(
            "Built at commit: feedfacecafe\nHEAD is {}, graph built at feedfac: graph may be stale\n",
            &head[..7]
        )),
        "{}",
        stats(&gp)
    );
}

fn read(path: &Path) -> (String, Value) {
    let text = fs::read_to_string(path).unwrap();
    let value = serde_json::from_str(&text).unwrap();
    (text, value)
}

fn assert_compact(text: &str) {
    assert!(!text.trim_end().contains('\n'), "not one line: {text}");
    assert!(!text.contains("\": ") && !text.contains(", \""), "{text}");
}

#[test]
fn no_cluster_graph_is_compact_and_carries_no_run_only_keys() {
    // Graphify 0.9.80 (#4189) and 0.9.86 (#3413).
    let root = tempfile::tempdir().unwrap();
    let corpus = root.path().join("corpus");
    fs::create_dir(&corpus).unwrap();
    fs::write(
        corpus.join("app.py"),
        "import requests\n\n\ndef fetch(url):\n    return requests.get(url)\n",
    )
    .unwrap();
    let out = run(&[
        "extract",
        corpus.to_str().unwrap(),
        "--code-only",
        "--no-cluster",
    ]);
    assert_eq!(out.exit_code, 0, "{}{}", out.stdout, out.stderr);
    let (text, graph) = read(&corpus.join(".graphicle/graph.json"));
    assert_compact(&text);
    for key in graphicle::extract::RUN_ONLY_EXTRACTION_KEYS {
        assert!(graph.get(key).is_none(), "{key} in {text}");
    }
    for root in [corpus.clone(), fs::canonicalize(&corpus).unwrap()] {
        assert!(!text.contains(root.to_str().unwrap()), "{text}");
    }
    let declared = graph["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["id"].clone())
        .collect::<Vec<_>>();
    for e in graph["edges"].as_array().unwrap() {
        assert!(declared.contains(&e["source"]), "{e}");
        assert!(declared.contains(&e["target"]), "{e}");
    }
}

#[test]
fn finalize_raw_graph_endpoints_matches_the_clustered_rules() {
    let mut data = json!({
        "nodes": [
            {"id": "app", "label": "app.py", "source_file": "app.py"},
            {"id": "app_fetch", "label": "fetch()", "source_file": "app.py"},
            {"id": "old_stub", "label": "gone", "type": "external", "external": true},
        ],
        "edges": [
            {"source": "app", "target": "requests", "relation": "imports"},
            {"source": "app", "target": "App_Fetch", "relation": "contains"},
            {"source": "app_fetch", "target": "nowhere", "relation": "calls"},
            {"source": "app_fetch", "target": 7, "relation": "calls"},
        ],
        "hyperedges": [],
    });
    graphicle::build::finalize_raw_graph_endpoints(&mut data);
    let ids = data["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["id"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert!(ids.contains(&"requests"), "{data}");
    assert!(!ids.contains(&"old_stub"), "{data}");
    let edges = data["edges"].as_array().unwrap();
    assert_eq!(edges.len(), 2, "{data}");
    assert_eq!(edges[1]["target"], "app_fetch", "{data}");
}

#[test]
fn merge_writers_are_compact() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.json");
    let b = dir.path().join("b.json");
    fs::write(&a, lookup_graph().to_string()).unwrap();
    fs::write(
        &b,
        json!({"directed": false, "multigraph": false, "graph": {}, "links": [],
            "nodes": [{"id": "extra", "label": "Extra", "file_type": "code"}]})
        .to_string(),
    )
    .unwrap();
    let merged = dir.path().join("merged.json");
    let out = run(&[
        "merge-graphs",
        a.to_str().unwrap(),
        b.to_str().unwrap(),
        "--out",
        merged.to_str().unwrap(),
    ]);
    assert_eq!(out.exit_code, 0, "{}", out.stderr);
    assert_compact(&read(&merged).0);
    let ours = dir.path().join("ours.json");
    fs::copy(&a, &ours).unwrap();
    let out = run(&[
        "merge-driver",
        a.to_str().unwrap(),
        ours.to_str().unwrap(),
        b.to_str().unwrap(),
    ]);
    assert_eq!(out.exit_code, 0, "{}", out.stderr);
    let (text, value) = read(&ours);
    assert_compact(&text);
    assert!(
        value["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n["id"] == "extra")
    );
}

fn prs(function: &str, request: Value) -> Value {
    let mut request = request;
    request["operation"] = json!("prs");
    request["function"] = json!(function);
    graphicle::dispatch(&request).unwrap()
}

fn pr_record(number: i64, state: &str, base: &str) -> Value {
    json!({"number": number, "title": "Fix something", "state": state, "headRefName": "fix", "baseRefName": base,
        "author": {"login": "bob"}, "isDraft": false, "reviewDecision": "", "statusCheckRollup": [],
        "updatedAt": "2026-09-24T12:00:00Z"})
}

#[test]
fn prs_fetch_a_pr_beyond_the_open_list() {
    // Graphify 0.9.87 (#3829).
    let out = prs(
        "fetch_pr",
        json!({"number": 105, "base": "v8", "command_responses": [
            {"program": "gh", "stdout": pr_record(105, "OPEN", "v8").to_string()},
        ]}),
    );
    assert_eq!(out["value"]["number"], 105, "{out}");
    let args = out["command_calls"][0]["args"].as_array().unwrap();
    assert_eq!(
        args[0..3],
        [json!("pr"), json!("view"), json!("105")],
        "{out}"
    );
    assert!(args.contains(&json!(graphicle::prs::PR_JSON_FIELDS)));
    for state in ["CLOSED", "MERGED"] {
        let out = prs(
            "fetch_pr",
            json!({"number": 105, "base": "v8", "command_responses": [
                {"program": "gh", "stdout": pr_record(105, state, "v8").to_string()},
            ]}),
        );
        assert!(out["value"].is_null(), "{out}");
    }
    let out = prs(
        "fetch_pr",
        json!({"number": 105, "repo": "owner/repo", "base": "v8", "command_responses": [
            {"program": "gh", "stdout": "not json"},
        ]}),
    );
    assert!(out["value"].is_null(), "{out}");
    let args = out["command_calls"][0]["args"].as_array().unwrap();
    assert!(args.contains(&json!("--repo")) && args.contains(&json!("owner/repo")));
    let minimal = prs(
        "_parse_pr_dict",
        json!({"item": {"number": 7, "title": "Minimal PR", "headRefName": "patch-1", "baseRefName": "main"}, "expected_base": "main"}),
    );
    let pr = &minimal["value"];
    assert_eq!(
        (
            &pr["author"],
            &pr["is_draft"],
            &pr["review_decision"],
            &pr["ci_status"]
        ),
        (&json!("?"), &json!(false), &json!(""), &json!("NONE")),
        "{pr}"
    );
}

#[test]
fn prs_detail_of_an_unlisted_pr_includes_its_wrong_base_impact() {
    let dir = tempfile::tempdir().unwrap();
    let graph = dir.path().join("graph.json");
    fs::write(
        &graph,
        json!({"nodes": [
            {"id": "n1", "source_file": "src/auth.py", "community": 0, "label": "auth"},
            {"id": "n2", "source_file": "src/auth.py", "community": 0, "label": "login"},
            {"id": "n3", "source_file": "src/api.py", "community": 1, "label": "api"},
        ]})
        .to_string(),
    )
    .unwrap();
    let out = prs(
        "cmd_prs",
        json!({"argv": ["--base", "v8", "--graph", graph.to_str().unwrap(), "105"],
        "now_micros": 1_790_000_000_000_000i64,
        "command_responses": [
            {"program": "gh", "stdout": "[]"},
            {"program": "git", "stdout": ""},
            {"program": "gh", "stdout": pr_record(105, "OPEN", "develop").to_string()},
            {"program": "gh", "stdout": "src/auth.py\n"},
        ]}),
    );
    let value = &out["value"];
    assert_eq!(value["code"], 0, "{out}");
    let stdout = value["stdout"].as_str().unwrap();
    assert!(stdout.contains("PR #105"), "{stdout}");
    assert!(stdout.contains("2 nodes"), "{stdout}");
    let calls = out["command_calls"].as_array().unwrap();
    assert_eq!(calls[2]["args"][1], "view", "{out}");
    assert_eq!(calls[3]["args"][1], "diff", "{out}");
}

fn llm(function: &str, request: Value) -> Value {
    let mut request = request;
    request["operation"] = json!("llm");
    request["function"] = json!(function);
    graphicle::dispatch(&request).unwrap_or_else(|e| json!({"error": e.to_string()}))
}

#[test]
fn ollama_seed_is_opt_in_and_validated() {
    // Graphify 0.9.86 (#4168).
    let seed = |raw: Option<&str>| {
        let env = raw.map_or(json!({}), |r| json!({"GRAPHICLE_OLLAMA_SEED": r}));
        llm("_resolve_ollama_seed", json!({"env": env}))
    };
    assert!(seed(None)["value"].is_null());
    assert!(seed(Some("  "))["value"].is_null());
    assert_eq!(seed(Some(" 0042 "))["value"], 42);
    assert_eq!(
        seed(Some("9223372036854775807"))["value"],
        9_223_372_036_854_775_807i64
    );
    for bad in ["-1", "4.2", "abc", "9223372036854775808", "١٢"] {
        let error = seed(Some(bad))["error"].as_str().unwrap_or("").to_string();
        assert!(
            error.contains(
                "GRAPHICLE_OLLAMA_SEED must be an ASCII integer in the range 0..9223372036854775807."
            ),
            "{bad}: {error}"
        );
    }
    for plain in [true, false] {
        let plan = llm(
            "request_plan",
            json!({"env": {"GRAPHICLE_OLLAMA_SEED": "7", "OLLAMA_API_KEY": "x"}, "backend": "ollama",
                "user_message": "hi", "plain": plain}),
        );
        assert_eq!(plan["value"]["body"]["seed"], 7, "{plan}");
        let plan = llm(
            "request_plan",
            json!({"env": {"GRAPHICLE_OLLAMA_SEED": "7", "OPENAI_API_KEY": "x"}, "backend": "openai",
                "user_message": "hi", "plain": plain}),
        );
        assert!(plan["value"]["body"].get("seed").is_none(), "{plan}");
    }
}

#[test]
fn claude_cli_plain_calls_honour_the_model_setting() {
    // Graphify 0.9.87 (#3846).
    let args = |env: Value, model: Option<&str>| {
        let plan = llm(
            "request_plan",
            json!({"env": env, "backend": "claude-cli", "user_message": "label", "plain": true, "model": model}),
        );
        plan["value"]["cli_args"].clone()
    };
    let with_env = args(json!({"GRAPHICLE_CLAUDE_CLI_MODEL": " haiku "}), None);
    assert!(
        with_env
            .as_array()
            .unwrap()
            .windows(2)
            .any(|w| w == [json!("--model"), json!("haiku")]),
        "{with_env}"
    );
    assert!(!args(json!({}), None).to_string().contains("--model"));
    let explicit = args(json!({"GRAPHICLE_CLAUDE_CLI_MODEL": "haiku"}), Some("opus"));
    assert!(
        explicit
            .as_array()
            .unwrap()
            .windows(2)
            .any(|w| w == [json!("--model"), json!("opus")]),
        "{explicit}"
    );
}

#[test]
fn null_token_counts_read_as_zero() {
    // Graphify 0.9.86 (#4325).
    for (backend, env, response) in [
        (
            "openai",
            json!({"OPENAI_API_KEY": "x"}),
            json!({"choices": [{"message": {"content": "{}"}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": null, "completion_tokens": null}}),
        ),
        (
            "claude",
            json!({"ANTHROPIC_API_KEY": "x"}),
            json!({"content": [{"type": "text", "text": "{}"}], "stop_reason": "end_turn",
                "usage": {"input_tokens": null, "output_tokens": null}}),
        ),
        (
            "bedrock",
            json!({}),
            json!({"output": {"message": {"content": [{"text": "{}"}]}}, "stopReason": "end_turn",
                "usage": {"inputTokens": null, "outputTokens": null}}),
        ),
    ] {
        let out = llm(
            "interpret_extraction",
            json!({"env": env, "backend": backend, "user_message": "x", "response": response}),
        );
        assert_eq!(out["value"]["input_tokens"], 0, "{backend}: {out}");
        assert_eq!(out["value"]["output_tokens"], 0, "{backend}: {out}");
    }
}

#[test]
fn semantic_results_are_stamped_semantic() {
    // Graphify 0.9.87 (#2843): an existing stamp is kept.
    let out = llm(
        "_stamp_semantic_origin",
        json!({"result": {"nodes": [{"id": "a", "source_location": "L12"}, {"id": "b", "_origin": "ast"}, 3],
            "edges": [{"source": "a", "target": "b"}]}}),
    );
    let value = &out["value"];
    assert_eq!(value["nodes"][0]["_origin"], "semantic");
    assert_eq!(value["nodes"][1]["_origin"], "ast");
    assert_eq!(value["nodes"][2], 3);
    assert_eq!(value["edges"][0]["_origin"], "semantic");
}

#[test]
fn file_to_text_reads_text_and_converts_pdfs() {
    let dir = tempfile::tempdir().unwrap();
    let path: PathBuf = dir.path().join("notes.md");
    fs::write(&path, "line one\r\nline two\rline three\n").unwrap();
    let out = llm("_file_to_text", json!({"path": path.to_str().unwrap()}));
    assert_eq!(out["value"], "line one\nline two\nline three\n", "{out}");
}
