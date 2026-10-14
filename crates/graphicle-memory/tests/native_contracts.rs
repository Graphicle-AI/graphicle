use graphicle_memory::{
    ErrorKind, archived, community, dispatch, entities, lifecycle, packing, process, retrieval,
    temporal, text,
};
use serde_json::{Value, json};

#[test]
fn reciprocal_rank_fusion_has_explicit_scores_and_deterministic_ties() {
    let rankings = vec![vec!["b".into(), "a".into()], vec!["a".into(), "b".into()]];
    let result = retrieval::rrf(&rankings, 1.0, 0.0).unwrap();
    assert_eq!(result, vec![("a".into(), 1.5), ("b".into(), 1.5)]);
    assert!(retrieval::rrf(&rankings, 1.0, 1.6).unwrap().is_empty());
    for constant in [0.0, -1.0, f64::NAN, f64::INFINITY] {
        assert_eq!(
            retrieval::rrf(&rankings, constant, 0.0).unwrap_err().kind(),
            ErrorKind::InvalidInput
        );
    }
}

#[test]
fn lexical_retrieval_rejects_duplicate_identities_and_unknown_modes() {
    let row = json!({"identity":"one","title":"Authentication","text":"The token expires after ten minutes"});
    let mut request = json!({"scope":"native-contract-ranking","question":"token expires","rows":[row.clone(),row]});
    assert!(
        retrieval::rank(&request)
            .unwrap_err()
            .to_string()
            .contains("unique")
    );
    request["rows"] = json!([]);
    request["mode"] = json!("unsupported");
    assert!(retrieval::rank(&request).is_err());
    // The overview mode is not supported (unit `community-reports`): auto
    // mode ranks a
    // question that asks for an overview with the other modes' rules.
    let rows = json!([passage("one", "d1", "user: The service has three layers.")]);
    let overview = retrieval::rank(&json!({"scope":"native-contract-ranking",
        "question":"Give an overview of the architecture","rows":rows}))
    .unwrap();
    assert_eq!(overview["mode"], "balanced", "{overview}");
}

#[test]
fn diversity_selection_preserves_origin_caps_duplicate_suppression_and_need_coverage() {
    let rows: Vec<_> = [
        (
            "a",
            "source",
            "token rotation",
            "token rotation rollback policy common",
            "one",
        ),
        (
            "duplicate",
            "source",
            "token rotation",
            "token rotation rollback policy common",
            "two",
        ),
        ("b", "source", "token", "token expires common", "one"),
        (
            "c",
            "source",
            "rollback",
            "rollback recovery safety unique",
            "three",
        ),
        ("d", "source", "token", "token cache common", "one"),
        (
            "m",
            "memory",
            "rotation lesson",
            "rotation safely recovery rules",
            "memory",
        ),
    ]
    .into_iter()
    .map(|(identity, kind, title, body, origin)| {
        json!({
            "identity":identity,"kind":kind,"title":title,"text":body,"origin":origin,
            "source":{"path":format!("{identity}.py")},"memory":{"kind":"lesson"}
        })
    })
    .collect();
    let request = json!({"question":"token rotation rollback","scope":"diversity-contract",
        "rows":rows,"limit":4,"needs":["rotation rollback"],
        "rankings":{"vector":["a","duplicate","b","c","d","m"]}});
    let result = retrieval::rank(&request).unwrap();
    assert_eq!(result["sources"], json!(["a", "c", "b"]));
    assert_eq!(result["memories"], json!(["m"]));
    assert_eq!(result["decisions"], 4);
    assert_eq!(result["unresolved_needs"], json!([]));
    assert_eq!(result["complete_evidence_guaranteed"], false);
}

#[test]
fn contradictions_close_only_overlapping_older_edges_and_preserve_input() {
    let request = json!({"scope":"repo","recorded_at":"2026-09-23T10:00:00Z",
        "resolved_edge":{"scope":"repo","valid_at":"2026-09-20T00:00:00Z"},
        "candidates":[{"id":"old","scope":"repo","valid_at":"2026-09-01T00:00:00Z"},
        {"id":"expired","scope":"repo","valid_at":"2026-08-01T00:00:00Z","invalid_at":"2026-09-19T00:00:00Z"},
        {"id":"unknown","scope":"repo"}]});
    let original = request.clone();
    let result = temporal::contradictions(&request).unwrap();
    assert_eq!(request, original);
    assert_eq!(result["invalidated_edges"].as_array().unwrap().len(), 1);
    assert_eq!(
        result["invalidated_edges"][0]["invalid_at"],
        "2026-09-20T00:00:00Z"
    );
    assert_eq!(
        result["invalidated_edges"][0]["expired_at"],
        "2026-09-23T10:00:00Z"
    );
    assert_eq!(result["unresolved"][0]["id"], "unknown");
    assert_eq!(result["semantic_contradiction_checked"], false);
    assert_eq!(result["automatic_commit"], false);
}

#[test]
fn temporal_scope_mismatch_and_inverted_intervals_are_rejected() {
    let mut request = json!({"scope":"one","recorded_at":"2026-09-23T00:00:00Z","resolved_edge":{"scope":"two"},"candidates":[]});
    assert!(temporal::contradictions(&request).is_err());
    request["resolved_edge"] = json!({"scope":"one","valid_at":"2026-09-23T00:00:00Z","invalid_at":"2026-09-22T00:00:00Z"});
    assert!(temporal::contradictions(&request).is_err());
    assert!(temporal::instant("2026-09-23T10:00:00").is_err());
}

#[test]
fn history_distinguishes_knowledge_time_event_time_and_unknown_dates() {
    let request = json!({"known_at":"2026-09-22T23:00:00Z","event_at":"2026-09-20T00:00:00Z","records":[
        {"id":"fact","revision":1,"recorded_at":"2026-09-21T00:00:00Z","valid_at":"2026-09-01T00:00:00Z","state":"reviewed","value":"old"},
        {"id":"fact","revision":2,"recorded_at":"2026-09-23T00:00:00Z","valid_at":"2026-09-01T00:00:00Z","state":"reviewed","value":"future knowledge"},
        {"id":"undated","revision":1,"recorded_at":"2026-09-21T00:00:00Z","state":"reviewed"},
        {"id":"draft","revision":1,"recorded_at":"2026-09-21T00:00:00Z","state":"draft"}]});
    let result = temporal::history(&request).unwrap();
    assert_eq!(result["records"].as_array().unwrap().len(), 1);
    assert_eq!(result["records"][0]["value"], "old");
    assert_eq!(result["unknown_event_time"][0]["id"], "undated");
    assert_eq!(result["current_source_validity_not_implied"], true);
}

#[test]
fn dependency_cycles_missing_revisions_and_depth_limits_withhold_reuse() {
    let cyclic = json!({"root":"a","nodes":[{"id":"a","revision":1,"state":"reviewed","parents":{"b":1}},{"id":"b","revision":1,"state":"reviewed","parents":{"a":1}}]});
    let result = lifecycle::dependencies(&cyclic).unwrap();
    assert_eq!(result["stale"], true);
    assert!(
        result["reasons"]
            .as_array()
            .unwrap()
            .contains(&json!("dependency_cycle:a"))
    );
    assert_eq!(result["automatic_deletion"], false);
    let missing =
        lifecycle::dependencies(&json!({"root":"a","nodes":[{"id":"a","parents":{"b":2}}]}))
            .unwrap();
    assert_eq!(missing["reasons"], json!(["decision_changed:b"]));
    let nodes:Vec<_> = (0..103).map(|i|json!({"id":i.to_string(),"revision":1,"state":"reviewed","parents":{(i+1).to_string():1}})).collect();
    let result = lifecycle::dependencies(&json!({"root":"0","nodes":nodes})).unwrap();
    assert_eq!(result["complete"], false);
    assert!(
        result["reasons"]
            .as_array()
            .unwrap()
            .contains(&json!("dependency_depth_exceeded"))
    );
}

/// What a listing asks per record: a root carrying the record's parents and
/// reasons, walked alongside every record.
fn per_root(nodes: &[Value], node: &Value) -> Value {
    let mut all = vec![json!({"id":"root","revision":0,"state":"reviewed",
        "parents":node.get("parents").cloned().unwrap_or(json!({})),
        "reasons":node.get("reasons").cloned().unwrap_or(json!([]))})];
    all.extend(nodes.iter().cloned());
    lifecycle::dependencies(&json!({"root":"root","nodes":all})).unwrap()["reasons"].clone()
}

#[test]
fn batched_dependencies_equal_one_walk_per_record() {
    // Deterministic pseudo-random graphs: shared ancestry, cycles, changed
    // revisions, unreviewed parents, missing parents and the reserved id.
    let mut seed: u64 = 0x5eed;
    let mut next = |bound: u64| {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (seed >> 33) % bound
    };
    for round in 0..60 {
        let count = 1 + next(40) as usize;
        let nodes: Vec<Value> = (0..count)
            .map(|i| {
                let mut parents = serde_json::Map::new();
                for _ in 0..next(4) {
                    let target = match next(20) {
                        0 => "missing".to_string(),
                        1 if round % 7 == 0 => "root".to_string(),
                        // Mostly older records, sometimes newer ones (cycles).
                        _ if next(5) == 0 => format!("m{}", next(count as u64)),
                        _ => format!("m{}", next(i.max(1) as u64)),
                    };
                    let revision = if next(8) == 0 { 2 } else { 1 };
                    parents.insert(target, json!(revision));
                }
                let reasons: Vec<String> = (0..next(3))
                    .map(|r| format!("source_changed_or_unavailable:s{}", next(5) + r))
                    .collect();
                json!({"id":format!("m{i}"),"revision":1,
                    "state":if next(6) == 0 {"draft"} else {"reviewed"},
                    "parents":parents,"reasons":reasons})
            })
            .collect();
        let batched = lifecycle::dependencies_all(&json!({"nodes":nodes})).unwrap();
        for node in &nodes {
            let id = node["id"].as_str().unwrap();
            assert_eq!(
                batched["reasons"][id],
                per_root(&nodes, node),
                "round {round}, {id}: {nodes:#?}"
            );
        }
    }
    // A chain deeper than the walk limit reports the same early stop.
    let chain: Vec<Value> = (0..130)
        .map(|i| {
            json!({"id":format!("c{i}"),"revision":1,"state":"reviewed",
            "parents":if i + 1 < 130 {json!({format!("c{}", i + 1):1})} else {json!({})},
            "reasons":if i == 129 {json!(["source_changed_or_unavailable:deep"])} else {json!([])}})
        })
        .collect();
    let batched = lifecycle::dependencies_all(&json!({"nodes":chain})).unwrap();
    for node in &chain {
        let id = node["id"].as_str().unwrap();
        assert_eq!(batched["reasons"][id], per_root(&chain, node), "{id}");
    }
    assert_eq!(
        batched["reasons"]["c0"],
        json!(["dependency_depth_exceeded"])
    );
    assert_eq!(
        batched["reasons"]["c100"],
        json!(["source_changed_or_unavailable:deep"])
    );
    assert!(
        lifecycle::dependencies_all(&json!({"nodes":[{"id":"root"}]})).is_err(),
        "the root id is reserved"
    );
}

#[test]
fn retractions_prune_grounding_without_mutating_history() {
    let request = json!({"live_ids":["live"],"based_on":{"world":[{"id":"live"},{"id":"gone"}],"observation":[{"id":"gone"}]}});
    let original = request.clone();
    let result = lifecycle::retractions(&request).unwrap();
    assert_eq!(request, original);
    assert_eq!(result["pruned"]["world"], json!([{"id":"live"}]));
    assert_eq!(result["unresolvable"], false);
    assert_eq!(result["requires_review"], true);
    assert_eq!(result["original_preserved"], true);
}

#[test]
fn readiness_uses_current_snapshot_and_cost_without_rescheduling_running_jobs() {
    let result = lifecycle::readiness(&json!({"snapshot":"new","budget":3,"jobs":[
        {"id":"ready","status":"ready","snapshot":"new","cost":1},
        {"id":"stale","status":"ready","snapshot":"old","priority":9,"cost":2},
        {"id":"busy","status":"running","priority":10,"cost":1},
        {"id":"cheap","status":"queued","priority":1,"cost":1}]}))
    .unwrap();
    assert_eq!(result["ready"], json!(["ready"]));
    assert_eq!(result["suggested"], json!(["stale", "cheap"]));
    assert_eq!(result["cost"], 3);
    assert_eq!(result["automatic_execution"], false);
}

#[test]
fn community_weight_counts_distinct_sources_instead_of_mentions() {
    let result = community::weights(
        &json!({"reports":[{"community_id":"a"},{"community_id":"b"}],"entities":[
        {"community_ids":["a"],"text_unit_ids":["one","one","two"]},
        {"community_ids":["a","b"],"text_unit_ids":["one"]}]}),
    )
    .unwrap();
    assert_eq!(result["reports"][0]["attributes"]["occurrence"], 1.0);
    assert_eq!(result["reports"][1]["attributes"]["occurrence"], 0.5);
}

#[test]
fn community_context_requires_sources_and_respects_final_token_budget() {
    let result = community::context(&json!({"entities":[],"max_tokens":128,"reports":[
        {"id":"missing","community_id":"a","title":"No provenance","summary":"Must not be included","source_ids":[]},
        {"id":"large","community_id":"b","title":"Too long","summary":"long report ".repeat(500),"source_ids":["one"]},
        {"id":"small","community_id":"c","title":"Small","summary":"Supported statement","source_ids":["two"]}]})).unwrap();
    assert_eq!(result["reports"].as_array().unwrap().len(), 1);
    assert_eq!(result["reports"][0]["id"], "small");
    assert!(result["tokens"].as_u64().unwrap() <= 128);
    assert_eq!(result["model_calls"], 0);
}

#[test]
fn packed_utf8_excerpts_have_exact_byte_offsets_and_never_cut_procedures() {
    let body = "世界 🌍 authentication procedure\n".repeat(1200);
    let result = packing::pack(&json!({"repo":{"id":"r","name":"Repo","snapshot":"one"},"question":"authentication","hits":[
        {"identity":"source","text":body,"source":{"source_kind":"repository_file","path":"readme.md","id":"s","sha256":"hash","byte_start":9,"line":1}}],
        "memories":[{"memory":{"id":"p","kind":"procedure","title":"All steps","body":"required step ".repeat(3000)}}],"max_tokens":768,"max_chars":4000})).unwrap();
    let markdown = result["markdown"].as_str().unwrap();
    assert_eq!(
        result["tokens"].as_u64().unwrap() as usize,
        text::count(markdown)
    );
    assert_eq!(
        result["characters"].as_u64().unwrap() as usize,
        markdown.chars().count()
    );
    assert!(text::count(markdown) <= 768);
    assert!(markdown.chars().count() <= 4000);
    assert!(result["memories"].as_array().unwrap().is_empty());
    assert_eq!(result["omitted"]["memories"], 1);
    let source = &result["sources"][0];
    let end = source["byte_end"].as_u64().unwrap() as usize - 9;
    assert!(body.is_char_boundary(end));
    assert!(markdown.contains(&body[..end]));
    assert_eq!(source["next_byte"], source["byte_end"]);
    assert_eq!(source["complete"], false);
}

#[test]
fn abi_and_in_process_boundaries_reject_malformed_input_without_panics() {
    for bytes in [
        b"not json".as_slice(),
        b"{}",
        b"{\"abi\":2,\"operation\":\"info\"}",
    ] {
        let response: Value = serde_json::from_slice(&process(bytes)).unwrap();
        assert_eq!(response["ok"], false);
    }
    let response: Value =
        serde_json::from_slice(&process(b"{\"abi\":1,\"operation\":\"info\"}")).unwrap();
    assert_eq!(response["ok"], true);
    for value in [Value::Null, json!([]), json!({"operation":"absent"})] {
        let result = std::panic::catch_unwind(|| dispatch(value)).unwrap();
        assert!(result.is_err());
        assert_ne!(result.unwrap_err().kind(), ErrorKind::Internal);
    }
    let result =
        dispatch(json!({"operation":"graph.info","unused":"x".repeat(8*1024*1024)})).unwrap();
    assert_eq!(result["implementation"], "rust");
}

fn named(question: &str) -> Vec<(&'static str, String)> {
    entities::terms(question)
        .into_iter()
        .map(|t| (t.kind, t.text))
        .collect()
}

#[test]
fn question_terms_are_names_quotes_identifiers_and_quantities() {
    assert_eq!(
        named("How many fish are in my 30-gallon tank?"),
        [("quantity", "30-gallon".to_string())]
    );
    // mem0 drops candidates of two characters or fewer ("Q3").
    assert_eq!(
        named("What did my manager at NovaTech say about the Q3 launch?"),
        [("proper", "NovaTech".to_string())]
    );
    assert_eq!(
        named("Which cafe in Shinjuku did I like, and what did Sarah's sister recommend?"),
        [
            ("proper", "Shinjuku".to_string()),
            ("proper", "Sarah".to_string())
        ]
    );
    // Connectors join names; calendar words and sentence openers are not names.
    assert_eq!(
        named("Remind me what I bought at the Museum of Modern Art last Saturday in March."),
        [("proper", "Museum of Modern Art".to_string())]
    );
    // As upstream, a quoted phrase keeps the names inside it.
    assert_eq!(
        named(
            r#"What did I say about "The Night Circus" and config.yaml in 2023, and the $250 fee?"#
        ),
        [
            ("proper", "Night Circus".to_string()),
            ("identifier", "config.yaml".to_string()),
            ("quantity", "2023".to_string()),
            ("quantity", "$250".to_string()),
            ("quoted", "The Night Circus".to_string()),
        ]
    );
    assert!(named("What is my favourite colour?").is_empty());
    assert!(named("How long have I been running on weekends?").is_empty());
}

#[test]
fn absent_terms_occur_in_no_source_under_any_spacing_case_or_plural() {
    let terms = entities::terms(
        "How many fish are in my 30-gallon tank from Aqua World and \"Blue Lagoon\"?",
    );
    let sources = [
        "user: I set up a 20 gallon tank from AQUA world's shop.",
        "user: We went to the blue lagoons.",
    ];
    let missing: Vec<_> = entities::absent(&terms, sources)
        .into_iter()
        .map(|t| t.text.clone())
        .collect();
    assert_eq!(missing, ["30-gallon"]);
    let later = ["user: Upgraded to a 30 gallons setup."];
    assert!(entities::absent(&terms[..1], later).is_empty());
    let result = dispatch(json!({"operation":"entities.terms","text":"Did I visit Harajuku with Priya?","sources":["user: Priya and I went to Shinjuku."]})).unwrap();
    assert_eq!(
        result["terms"],
        json!([{"kind":"proper","text":"Harajuku"},{"kind":"proper","text":"Priya"}])
    );
    assert_eq!(
        result["absent"],
        json!([{"kind":"proper","text":"Harajuku"}])
    );
}

#[test]
fn question_periods_resolve_against_the_date_asked() {
    let asked = chrono::NaiveDate::from_ymd_opt(2023, 5, 30).unwrap(); // a Tuesday
    let period = |question: &str| {
        temporal::window(question, asked).map(|(a, b, rule)| (a.to_string(), b.to_string(), rule))
    };
    let expect = |a: &str, b: &str, rule: &'static str| Some((a.to_string(), b.to_string(), rule));
    assert_eq!(
        period("How many times did I bake in the past two weeks?"),
        expect("2023-05-16", "2023-05-30", "past_count")
    );
    assert_eq!(
        period("What did I buy 10 days ago?"),
        expect("2023-05-19", "2023-05-21", "count_ago")
    );
    assert_eq!(
        period("Which event was two weeks ago?"),
        expect("2023-05-13", "2023-05-19", "count_ago")
    );
    assert_eq!(
        period("What certification did I complete last month?"),
        expect("2023-04-01", "2023-04-30", "last_unit")
    );
    assert_eq!(
        period("Where was I last week?"),
        expect("2023-05-22", "2023-05-28", "last_unit")
    );
    assert_eq!(
        period("What game did I beat last weekend?"),
        expect("2023-05-27", "2023-05-28", "last_weekend")
    );
    assert_eq!(
        period("Who did I meet last Tuesday?"),
        expect("2023-05-23", "2023-05-23", "last_weekday")
    );
    assert_eq!(
        period("What did I cook a couple of days ago?"),
        expect("2023-05-27", "2023-05-29", "couple_ago")
    );
    assert_eq!(
        period("How much did I earn this month?"),
        expect("2023-05-01", "2023-05-30", "this_unit")
    );
    assert_eq!(
        period("Which museums did I visit in December?"),
        expect("2022-12-01", "2022-12-31", "in_month")
    );
    assert_eq!(
        period("What did I do in March 2023?"),
        expect("2023-03-01", "2023-03-31", "month_year")
    );
    assert_eq!(
        period("Where did I travel in 2021?"),
        expect("2021-01-01", "2021-12-31", "in_year")
    );
    // No period named, a day-month date, or a port number: no window.
    assert_eq!(period("How many days ago did I buy the lamp?"), None);
    assert_eq!(period("What happened on 13 July 2023?"), None);
    assert_eq!(period("Is the server listening in 8080?"), None);
    assert_eq!(
        period("Which did I start first, the fence or the barn?"),
        None
    );
    // "Before today" bounds a span instead of naming the day asked.
    assert_eq!(
        period("What is the order of airlines I flew with, before today?"),
        None
    );
    assert_eq!(period("How many books had I read as of today?"), None);
    assert_eq!(
        period("What did I order for dinner today?"),
        expect("2023-05-30", "2023-05-30", "today")
    );
    // Written dates of sources: a recorded clock first, else the header line.
    assert_eq!(
        temporal::source_date(
            None,
            "Conversation date: 2023/05/20 (Sat) 02:21\n\nuser: hi"
        )
        .map(|d| d.to_string()),
        Some("2023-05-20".into())
    );
    assert_eq!(
        temporal::source_date(
            Some("2024-01-02T09:00:00Z"),
            "Conversation date: 2023/05/20"
        )
        .map(|d| d.to_string()),
        Some("2024-01-02".into())
    );
    assert_eq!(temporal::source_date(None, "# Notes\n\nNo date here"), None);
    let result = dispatch(json!({"operation":"temporal.window","question":"What did I buy last month?","asked":"2023/05/30 (Tue) 23:40"})).unwrap();
    assert_eq!(
        result,
        json!({"start":"2023-04-01","end":"2023-04-30","rule":"last_unit"})
    );
}

/// The previous `text::prefix`: every probe tokenizes the whole prefix.
fn prefix_by_full_counts(value: &str, tokens: usize, characters: usize) -> String {
    if tokens == 0 || characters == 0 {
        return String::new();
    }
    let boundaries: Vec<usize> = value
        .char_indices()
        .map(|(i, _)| i)
        .chain([value.len()])
        .take(characters.saturating_add(1))
        .collect();
    let last = *boundaries.last().unwrap_or(&0);
    if text::count(&value[..last]) <= tokens {
        return value[..last].to_owned();
    }
    let (mut low, mut high) = (0usize, boundaries.len() - 1);
    while low < high {
        let mid = low + (high - low).div_ceil(2);
        if text::count(&value[..boundaries[mid]]) <= tokens {
            low = mid;
        } else {
            high = mid - 1;
        }
    }
    while low > 0 && text::count(&value[..boundaries[low]]) > tokens {
        low -= 1;
    }
    value[..boundaries[low]].to_owned()
}

#[test]
fn line_reusing_prefix_counts_equal_whole_prefix_counts() {
    // Fragments chosen to stress cl100k's pre-tokenizer at line breaks:
    // indentation, CRLF, trailing whitespace, punctuation runs that absorb
    // breaks, contractions, digit runs, non-ASCII letters and spaces, emoji.
    const PARTS: &[&str] = &[
        "\n",
        "\r\n",
        "\n\n",
        " ",
        "  ",
        "\t",
        " \t ",
        "a",
        "Hello",
        "world",
        "don't",
        "'s",
        "'LL",
        "12",
        "34567",
        ".",
        "...",
        "!?",
        ",",
        "-",
        "_",
        "#",
        "(",
        ")",
        "\"",
        "é",
        "日本語",
        "🙂",
        "\u{a0}",
        "\u{2003}",
        "x1",
        "  \n",
        ".\n",
        "\n  ",
        "\n\t- item",
        "\n   \n",
        ":\r\n",
        "user: ",
        "assistant: ",
        "\u{0b}",
        "\u{0c}",
        "'",
    ];
    let mut seed: u64 = 0x005e_ed0f_11ce;
    let mut next = |bound: usize| {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((seed >> 33) % bound as u64) as usize
    };
    let mut texts: Vec<String> = vec![
        String::new(),
        "\n".into(),
        "a\n b".into(),
        "a\n\n  b".into(),
        "x  \n  \n  y".into(),
        "end with spaces   ".into(),
        "line\r\n\r\n  next".into(),
        "p.\n\n.q".into(),
    ];
    for _ in 0..90 {
        let length = 1 + next(90);
        texts.push((0..length).map(|_| PARTS[next(PARTS.len())]).collect());
    }
    for value in &texts {
        let ends: Vec<usize> = value
            .char_indices()
            .map(|(i, _)| i)
            .chain([value.len()])
            .collect();
        let direct: Vec<usize> = ends.iter().map(|&end| text::count(&value[..end])).collect();
        // Every candidate split (spacing 1), a middle spacing and the default.
        for segment in [1, 7, 64] {
            assert_eq!(
                text::prefix_counts(value, &ends, segment),
                direct,
                "{value:?} segment={segment}"
            );
        }
        for tokens in [1, 2, 3, 5, 8, 13, 40, 400] {
            for characters in [1, 7, 30, 10_000] {
                assert_eq!(
                    text::prefix(value, tokens, characters),
                    prefix_by_full_counts(value, tokens, characters),
                    "{value:?} tokens={tokens} characters={characters}"
                );
            }
        }
    }
}

#[test]
fn prefix_counts_split_long_runs_after_letters_and_digits() {
    // Runs with no space or line break: base64 and markup from a drawio SVG,
    // lock-file hashes and URLs, identifiers with digits and contractions.
    let texts = [
        "content=\"7Vttk6I4EP41Vu19kILwIn6cmR13r2r3yjqnau8+ZiAit0C4EGf0fv0lEBBCUFR0nLvRmlGaJgndTzpPd3BkPsSbLwSmq+uY/2j==\"",
        "&lt;mxfile&gt;&lt;diagram id=&quot;jyERGzDynktFHFRGN0ph&quot;&gt;",
        "sha256:6f1d6f2f4c2b9a0e1e5f0a6b7c8d9e0f1a2b3c4d5e6f708192a3b4c5d6e7f809",
        "https://files.pythonhosted.org/packages/a1/b2/abc123def-4.5.6-py3-none-any.whl#x=1",
        "don'tIt'sWe'LLx1y22z333w4444'd'S'Ve'RE9a",
        "ab12cd34ef56gh78ij90kl!?mn--op__qr..st,,uv((wx))yz",
    ];
    for value in texts {
        let value = value.repeat(3);
        let ends: Vec<usize> = value
            .char_indices()
            .map(|(i, _)| i)
            .chain([value.len()])
            .collect();
        let direct: Vec<usize> = ends.iter().map(|&end| text::count(&value[..end])).collect();
        for segment in [1, 7, 64] {
            assert_eq!(
                text::prefix_counts(&value, &ends, segment),
                direct,
                "{value:?} segment={segment}"
            );
        }
        for tokens in [1, 2, 3, 5, 8, 13, 40, 400] {
            for characters in [1, 7, 30, 10_000] {
                assert_eq!(
                    text::prefix(&value, tokens, characters),
                    prefix_by_full_counts(&value, tokens, characters),
                    "{value:?} tokens={tokens} characters={characters}"
                );
            }
        }
    }
}

#[test]
fn compact_word_features_answer_like_a_word_count_map() {
    use graphicle_memory::cache;
    let texts = [
        "The tank holds 30 fish; the fish are tetras. Tank water is 24C.",
        "Zürich trip: museum, museum café, and the Kunsthaus museum shop",
        "",
        "a b c a b a",
    ];
    for (i, value) in texts.iter().enumerate() {
        let features = cache::features("compact-test", value).unwrap();
        let mut expected: std::collections::BTreeMap<String, usize> = Default::default();
        for word in text::words(value) {
            *expected.entry(word).or_default() += 1;
        }
        assert_eq!(features.length, expected.values().sum::<usize>(), "{value}");
        assert_eq!(features.words.len(), expected.len(), "{value}");
        assert_eq!(
            features.words.keys().collect::<Vec<_>>(),
            expected.keys().map(String::as_str).collect::<Vec<_>>()
        );
        for (word, n) in &expected {
            assert!(features.words.contains_key(word));
            assert_eq!(features.words.get(word), *n, "{word}");
        }
        for absent in ["", "zzz", "tan", "tanks", "museums"] {
            assert_eq!(
                features.words.contains_key(absent),
                expected.contains_key(absent),
                "{absent}"
            );
        }
        for other in &texts[i..] {
            let b = cache::features("compact-test", other).unwrap();
            let manual = expected.keys().filter(|w| b.words.contains_key(w)).count();
            assert_eq!(features.words.common(&b.words), manual, "{value} / {other}");
        }
    }
    // A repeated lookup is a hit on the same shared value.
    let first = cache::features("compact-test", texts[0]).unwrap();
    let again = cache::features("compact-test", texts[0]).unwrap();
    assert!(std::sync::Arc::ptr_eq(&first, &again));
}

#[test]
fn chunks_keep_the_newest_documents_at_the_passage_bound_in_input_order() {
    let document = |id: &str, observed: Option<&str>, recorded: Option<&str>| {
        let content = format!("user: note {id} about the tank\n");
        json!({"id":id,"title":id,"sha256":text::sha256(content.as_bytes()),"content":content,
            "observed_at":observed,"recorded_at":recorded})
    };
    let documents = json!([
        document("b", Some("2026-01-02T00:00:00Z"), None),
        document("a", Some("2026-01-01T00:00:00Z"), None),
        // The source's own time wins over when it was observed.
        document(
            "c",
            Some("2026-01-01T00:00:00Z"),
            Some("2026-03-01T00:00:00+02:00")
        ),
        document("d", None, None),
    ]);
    let chunks = |prefer: Value, max: usize| {
        dispatch(
            json!({"operation":"chunks","scope":"prefer-test","documents":documents,
            "max_passages":max,"characters":2600,"tokens":640,"prefer":prefer}),
        )
    };
    let origins = |result: &Value| -> Vec<String> {
        result["rows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["origin"].as_str().unwrap().to_owned())
            .collect()
    };
    // When everything fits, the preference changes nothing.
    let all = chunks(json!("newest"), 10).unwrap();
    assert_eq!(all["rows"], chunks(Value::Null, 10).unwrap()["rows"]);
    assert_eq!(origins(&all), ["b", "a", "c", "d"]);
    assert_eq!(all["passage_limit_reached"], false);
    assert_eq!(all["omitted_documents_are"], "none");
    let newest = chunks(json!("newest"), 2).unwrap();
    assert_eq!(origins(&newest), ["b", "c"]);
    assert_eq!(newest["passage_limit_reached"], true);
    assert_eq!(newest["omitted_documents"], 2);
    // a (January 1) and the undated d are older than both kept documents.
    assert_eq!(newest["omitted_documents_are"], "oldest");
    assert_eq!(newest["preference"], "newest");
    let input = chunks(json!("input"), 2).unwrap();
    assert_eq!(origins(&input), ["b", "a"]);
    assert_eq!(input["omitted_documents_are"], "last_listed");
    assert_eq!(
        chunks(json!("newest"), 0).unwrap()["omitted_documents_are"],
        "all"
    );
    assert!(chunks(json!("oldest"), 2).is_err());
    assert!(chunks(json!(1), 2).is_err());
}

/// P-CAP (EVAL_PLAN R5): files synced from one folder share one time and are
/// listed by path, so at the passage bound the documents of equal time are
/// taken from the end of the listing, the latest session files, and the
/// report says which ones were left out.
#[test]
fn equal_times_at_the_passage_bound_keep_the_latest_listed_documents() {
    let document = |id: &str, observed: Option<&str>| {
        let content = format!("user: session {id} about the tank\n");
        json!({"id":id,"title":id,"sha256":text::sha256(content.as_bytes()),"content":content,
            "observed_at":observed,"recorded_at":null})
    };
    let chunks = |documents: &Value, max: usize| {
        dispatch(
            json!({"operation":"chunks","scope":"tie-test","documents":documents,
            "max_passages":max,"characters":2600,"tokens":640,"prefer":"newest"}),
        )
        .unwrap()
    };
    let origins = |result: &Value| -> Vec<String> {
        result["rows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["origin"].as_str().unwrap().to_owned())
            .collect()
    };
    let synced = Some("2026-02-01T00:00:00Z");
    let folder = json!([
        document("session-00001.txt", synced),
        document("session-00002.txt", synced),
        document("session-00003.txt", synced),
        document("session-00004.txt", synced),
    ]);
    let kept = chunks(&folder, 2);
    assert_eq!(origins(&kept), ["session-00003.txt", "session-00004.txt"]);
    assert_eq!(kept["omitted_documents"], 2);
    assert_eq!(kept["omitted_documents_are"], "earliest_listed");
    // Undated documents tie with each other the same way.
    let undated = json!([
        document("a", None),
        document("b", None),
        document("c", None)
    ]);
    let kept = chunks(&undated, 1);
    assert_eq!(origins(&kept), ["c"]);
    assert_eq!(kept["omitted_documents_are"], "earliest_listed");
    // A newer document is still taken before every equally old one, and the
    // ones left out are then older than everything kept.
    let mixed = json!([
        document("old-1", synced),
        document("old-2", synced),
        document("new", Some("2026-03-01T00:00:00Z")),
    ]);
    let kept = chunks(&mixed, 1);
    assert_eq!(origins(&kept), ["new"]);
    assert_eq!(kept["omitted_documents_are"], "oldest");
    let kept = chunks(&mixed, 2);
    assert_eq!(origins(&kept), ["old-2", "new"]);
    assert_eq!(kept["omitted_documents_are"], "earliest_listed");
}

/// P-CAP: one request may ask for 40,000 passages (it was 20,000), and the
/// passage-bound consumers follow the constant.
#[test]
fn a_request_may_ask_for_forty_thousand_passages() {
    use graphicle_memory::ingest;
    const { assert!(ingest::MAX_PASSAGES >= 40_000) };
    assert_eq!(
        retrieval::MAX_ROWS,
        ingest::MAX_PASSAGES + retrieval::MAX_RECALL_MEMORIES
    );
    let documents: Vec<Value> = (0..ingest::MAX_PASSAGES + 2)
        .map(|i| {
            let content = format!("note {i}");
            json!({"id":format!("n{i:05}"),"title":"t","sha256":text::sha256(content.as_bytes()),
                "content":content,"observed_at":"2026-01-01T00:00:00Z"})
        })
        .collect();
    let request = |max: usize| {
        json!({"operation":"chunks","scope":"cap-test","documents":documents,
        "max_passages":max,"characters":2600,"tokens":640,"prefer":"newest"})
    };
    let result = dispatch(request(ingest::MAX_PASSAGES)).unwrap();
    let rows = result["rows"].as_array().unwrap();
    assert_eq!(rows.len(), ingest::MAX_PASSAGES);
    // The two earliest listed of the equally dated notes are the ones left out.
    assert_eq!(rows[0]["origin"], "n00002");
    assert_eq!(result["omitted_documents"], 2);
    assert_eq!(result["omitted_documents_are"], "earliest_listed");
    assert!(
        dispatch(request(ingest::MAX_PASSAGES + 1))
            .unwrap_err()
            .to_string()
            .contains("supported range")
    );
}

#[test]
fn chunks_and_dependency_passes_accept_past_the_former_bounds() {
    use graphicle_memory::ingest;
    let documents: Vec<Value> = (0..6000)
        .map(|i| {
            let content = format!("note {i}");
            json!({"id":format!("n{i}"),"title":"t","sha256":text::sha256(content.as_bytes()),"content":content})
        })
        .collect();
    let result = dispatch(
        json!({"operation":"chunks","scope":"bounds-test","documents":documents,
        "max_passages":ingest::MAX_PASSAGES,"characters":2600,"tokens":640}),
    )
    .unwrap();
    assert_eq!(result["rows"].as_array().unwrap().len(), 6000);
    assert!(
        result.get("document_limit_reached").is_none(),
        "{}",
        result["omitted_documents"]
    );
    // Past the document bound the request degrades instead of
    // failing. The documents taken last are left out unread (a malformed one
    // there is never read) and reported; the rest are chunked as before.
    let mut too_many: Vec<Value> = (0..ingest::MAX_DOCUMENTS)
        .map(|i| {
            let content = format!("note {i}");
            json!({"id":format!("n{i}"),"title":"t","sha256":text::sha256(content.as_bytes()),"content":content})
        })
        .collect();
    too_many.push(json!({}));
    let degraded = dispatch(
        json!({"operation":"chunks","scope":"bounds-test","documents":too_many,
        "max_passages":10,"characters":2600,"tokens":640}),
    )
    .unwrap();
    let rows = degraded["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 10);
    assert_eq!(rows[0]["origin"], "n0");
    assert_eq!(degraded["document_limit_reached"], true);
    assert_eq!(degraded["omitted_by_document_limit"], 1);
    assert_eq!(
        degraded["omitted_documents"],
        ingest::MAX_DOCUMENTS + 1 - 10
    );
    assert_eq!(degraded["omitted_documents_are"], "last_listed");
    assert_eq!(degraded["passage_limit_reached"], true);
    // Every memory of a repository (past the former 5,000 and 10,000) in one pass.
    let nodes: Vec<Value> = (0..12_000)
        .map(|i| json!({"id":format!("m{i}"),"revision":1,"state":"reviewed","parents":{},"reasons":[]}))
        .collect();
    let all = lifecycle::dependencies_all(&json!({"nodes":nodes})).unwrap();
    assert_eq!(all["reasons"].as_object().unwrap().len(), 12_000);
    let mut rooted = nodes.clone();
    rooted.push(json!({"id":"root","revision":0,"state":"reviewed","parents":{},"reasons":[]}));
    assert!(lifecycle::dependencies(&json!({"root":"root","nodes":rooted})).is_ok());
    let over = vec![json!({"id":"x"}); lifecycle::MAX_MEMORIES + 1];
    assert!(lifecycle::dependencies_all(&json!({"nodes":over})).is_err());
}

#[test]
fn section_anchors_resolve_batch_headings_slugs_and_headings_and_blocks_report_their_section() {
    let split =
        dispatch(json!({"operation":"document.split","markdown":"# A\n\nText.\n"})).unwrap();
    let delta = |document: &Value, operations: Value| {
        dispatch(json!({"operation":"document.delta","document":document,"operations":operations}))
            .unwrap()
    };
    let ids = |result: &Value| -> Vec<String> {
        result["document"]["sections"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["id"].as_str().unwrap().to_owned())
            .collect()
    };
    let chained = delta(
        &split["document"],
        json!([
            {"op":"add_section","heading":"A","blocks":["Second A."]},
            // "A" is no section id; it is the heading of the section just added.
            {"op":"add_section","heading":"B","blocks":[],"after_section_id":"A"},
            {"op":"add_section","heading":"C","blocks":[],"new_id":"custom","after_section_id":"a"},
            {"op":"add_section","heading":"D","blocks":[],"after_section_id":"custom"},
            {"op":"add_section","heading":"E","blocks":[],"after_section_id":"Missing Heading"}
        ]),
    );
    assert_eq!(ids(&chained), ["a", "custom", "d", "a-2", "b"], "{chained}");
    assert_eq!(chained["skipped"].as_array().unwrap().len(), 1);
    assert_eq!(chained["skipped"][0]["index"], 4);
    assert_eq!(
        chained["skipped"][0]["reason"],
        "unknown after_section_id: Missing Heading"
    );
    // Slug and plain-heading fallbacks for existing sections; exact ids win.
    let document = json!({"version":2,"sections":[
        {"id":"team-plans","heading":"Team plans","level":2,"blocks":[{"id":"x","text":"One."}]},
        {"id":"s-1","heading":"Budget Notes","level":2,"blocks":[{"id":"y","text":"Two."}]},
        {"id":"budget-notes","heading":"Other","level":2,"blocks":[]}]});
    let fallbacks = delta(
        &document,
        json!([
            {"op":"add_section","heading":"After plans","blocks":[],"after_section_id":"Team Plans!"},
            {"op":"add_section","heading":"After s-1","blocks":[],"after_section_id":"s-1"},
            // The slug `budget-notes` is an exact id and wins over the heading.
            {"op":"add_section","heading":"After budget","blocks":[],"after_section_id":"Budget Notes"}
        ]),
    );
    assert_eq!(
        ids(&fallbacks),
        [
            "team-plans",
            "after-plans",
            "s-1",
            "after-s-1",
            "budget-notes",
            "after-budget"
        ],
        "{fallbacks}"
    );
    let heading_only = delta(
        &json!({"version":2,"sections":[{"id":"s-9","heading":"Budget Notes","level":2,"blocks":[]}]}),
        json!([{"op":"add_section","heading":"Next","blocks":[],"after_section_id":"Budget Notes"}]),
    );
    assert_eq!(ids(&heading_only), ["s-9", "next"]);
    let blocks = delta(
        &document,
        json!([
            {"op":"replace_block","section_id":"team-plans","block_id":"y","text":"Moved?"},
            {"op":"remove_block","section_id":"team-plans","block_id":"zzz"},
            {"op":"append_block","section_id":"nope","text":"x"}
        ]),
    );
    let reasons: Vec<&str> = blocks["skipped"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["reason"].as_str().unwrap())
        .collect();
    assert_eq!(
        reasons,
        [
            "block_id y belongs to section s-1, not team-plans",
            "unknown block_id: zzz",
            "unknown section_id: nope"
        ]
    );
    assert_eq!(blocks["changed"], false);
}

fn passage(identity: &str, origin: &str, text: &str) -> Value {
    json!({"identity":identity,"kind":"source","origin":origin,"title":"","text":text,
        "source":{"id":origin,"byte_start":0,"byte_end":text.len()}})
}

/// Unsupported `rank` options (src/archived.rs): turned on, each is refused;
/// at an inert value it changes nothing. Unsupported values of options that
/// stay are refused the same way.
#[test]
fn archived_rank_options_are_refused_and_their_inert_values_change_nothing() {
    let mut rows = vec![passage(
        "ship",
        "d1",
        "user: The release ships on Friday after the canary.",
    )];
    for i in 0..6 {
        rows.push(passage(
            &format!("x{i}"),
            &format!("n{i}"),
            &format!("user: Note {i} about compost bins in the garden."),
        ));
    }
    let base = json!({"scope":"archived-options","question":"When does the release ship?",
        "rows":rows,"limit":3,"debug_fused":3});
    let plain = retrieval::rank(&base).unwrap();
    assert_eq!(plain["sources"][0], "ship", "{plain}");
    assert!(!archived::OPTIONS.is_empty());
    for entry in archived::OPTIONS {
        assert_eq!(
            entry.message(),
            format!("`{}` is not supported by Graphicle", entry.option)
        );
        let mut request = base.clone();
        request[entry.option] = json!(true);
        let error = retrieval::rank(&request).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidInput, "{}", entry.option);
        assert_eq!(error.to_string(), entry.message());
        for value in [Value::Null, json!(false), json!(0), json!({})] {
            if !(entry.inert)(&value) {
                continue;
            }
            request[entry.option] = value.clone();
            let output = retrieval::rank(&request).unwrap();
            assert_eq!(output["fused"], plain["fused"], "{}: {value}", entry.option);
            assert_eq!(
                output["sources"], plain["sources"],
                "{}: {value}",
                entry.option
            );
        }
    }
    for entry in archived::VALUES {
        let mut request = base.clone();
        request[entry.option] = json!(entry.value);
        let error = retrieval::rank(&request).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidInput, "{}", entry.value);
        assert_eq!(error.to_string(), entry.message());
    }
}

#[test]
fn weighted_fusion_equals_plain_fusion_at_unit_weights_and_reorders_otherwise() {
    let rankings = vec![
        vec!["a".to_string(), "b".into()],
        vec!["b".to_string(), "a".into()],
    ];
    assert_eq!(
        retrieval::rrf(&rankings, 61.0, 0.0).unwrap(),
        retrieval::rrf_weighted(&rankings, &[1.0, 1.0], 61.0, 0.0).unwrap()
    );
    let weighted = retrieval::rrf_weighted(&rankings, &[1.0, 2.0], 61.0, 0.0).unwrap();
    assert_eq!(weighted[0].0, "b");
    assert!(retrieval::rrf_weighted(&rankings, &[1.0], 61.0, 0.0).is_err());
    assert!(retrieval::rrf_weighted(&rankings, &[1.0, -1.0], 61.0, 0.0).is_err());
}

#[test]
fn stemming_matches_inflected_forms_only_when_requested() {
    let mut rows = vec![
        passage(
            "s1",
            "d1",
            "user: We planned camping trips with the kids twice last summer.",
        ),
        passage("s2", "d2", "user: My plan for the business trip is ready."),
    ];
    for i in 0..10 {
        rows.push(passage(
            &format!("x{i}"),
            &format!("n{i}"),
            &format!("user: Note {i} about compost bins in the garden."),
        ));
    }
    let base = json!({"scope":"stem-test","question":"When did we plan the camping trip?",
        "rows":rows,"limit":2,"debug_fused":2});
    // Exact words: "plan" and "trip" outweigh "camping" alone.
    let plain = retrieval::rank(&base).unwrap();
    assert!(plain.get("stemmer").is_none());
    assert_eq!(plain["fused"][0], "s2", "{plain}");
    // Stems: "planned"/"plan", "camping"/"camp" and "trips"/"trip" all match.
    let mut request = base.clone();
    request["stem"] = json!(true);
    let stemmed = retrieval::rank(&request).unwrap();
    assert_eq!(stemmed["stemmer"], "porter");
    assert_eq!(stemmed["fused"][0], "s1", "{stemmed}");
    request["stem"] = json!("yes");
    assert!(retrieval::rank(&request).is_err());
    request["stem"] = json!(false);
    assert_eq!(retrieval::rank(&request).unwrap()["fused"], plain["fused"]);
}

#[test]
fn query_variants_fuse_rephrasings_keywords_and_hypothetical_passages_as_weighted_arms() {
    let mut rows = vec![
        passage(
            "s1",
            "d1",
            "user: I adopted a rescue greyhound named Pixel in March.",
        ),
        passage(
            "s2",
            "d2",
            "user: The pet store sells dog food and leashes.",
        ),
    ];
    for i in 0..10 {
        rows.push(passage(
            &format!("x{i}"),
            &format!("n{i}"),
            &format!("user: Note {i} about compost bins in the garden."),
        ));
    }
    // The question shares no word with s1.
    let base = json!({"scope":"variants-test","question":"What is the name of my dog?",
        "rows":rows,"limit":2,"debug_fused":3});
    let plain = retrieval::rank(&base).unwrap();
    assert!(plain.get("query_variants").is_none());
    assert_eq!(plain["fused"], json!(["s2"]), "{plain}");
    let mut request = base.clone();
    request["query_variants"] = json!([
        {"text":"What did I call the greyhound I adopted?","weight":1.3},
        {"text":"greyhound name","kind":"keyword"},
        {"text":"  GREYHOUND name ","kind":"keyword","weight":0.5},
        {"text":"The user adopted a greyhound named Pixel.","kind":"passage","weight":0.7}
    ]);
    let fused = retrieval::rank(&request).unwrap();
    // Case-insensitive duplicates of one kind merge and add their weights.
    assert_eq!(
        fused["query_variants"],
        json!([
            {"arm":"lexical_q1","kind":"query","weight":1.3,"terms":5,"ranked":1},
            {"arm":"lexical_q2","kind":"keyword","weight":1.5,"terms":2,"ranked":1},
            {"arm":"lexical_q3","kind":"passage","weight":0.7,"terms":5,"ranked":12}
        ]),
        "{fused}"
    );
    assert_eq!(fused["fused"][0], "s1", "{fused}");
    let reasons = fused["reasons"]["s1"].as_array().unwrap();
    for arm in ["lexical_q1", "lexical_q2", "lexical_q3"] {
        assert!(reasons.contains(&json!(arm)), "{fused}");
    }
    // `arm_weights` overrides a variant's own weight.
    request["arm_weights"] = json!({"lexical_q1":0,"lexical_q2":0,"lexical_q3":0});
    assert_eq!(retrieval::rank(&request).unwrap()["fused"][0], "s2");
    // A caller's dense ranking for variant i counts the variant's weight times
    // the `semantic` arm's, unless `arm_weights` names it.
    let mut dense = base.clone();
    dense["query_variants"] = json!([{"text":"greyhound","weight":1.5}]);
    dense["rankings"] = json!({"semantic_q1":["x5"]});
    dense["arm_weights"] = json!({"semantic":0.5});
    let low = retrieval::rank(&dense).unwrap();
    assert_eq!(low["query_variants"][0]["dense"], "semantic_q1");
    assert_eq!(low["fused"], json!(["s1", "s2", "x5"]), "{low}");
    dense["arm_weights"] = json!({"semantic":2});
    assert_eq!(retrieval::rank(&dense).unwrap()["fused"][0], "x5");
    dense["arm_weights"] = json!({"semantic":2,"semantic_q1":0.5});
    assert_eq!(retrieval::rank(&dense).unwrap()["fused"][0], "s1");
    // Variant terms are stemmed with the question's.
    let mut stemmed = base.clone();
    stemmed["query_variants"] = json!([{"text":"adopting greyhounds"}]);
    assert_eq!(
        retrieval::rank(&stemmed).unwrap()["query_variants"][0]["ranked"],
        0
    );
    stemmed["stem"] = json!(true);
    assert_eq!(
        retrieval::rank(&stemmed).unwrap()["query_variants"][0]["ranked"],
        1
    );
    // Invalid variants are rejected; unset or empty variants change nothing.
    for invalid in [
        json!("greyhound"),
        json!([{"text":""}]),
        json!([{"text":"   "}]),
        json!([{"text":"x".repeat(4001)}]),
        json!([{"text":"greyhound","kind":"hyde"}]),
        json!([{"text":"greyhound","weight":4.5}]),
        json!([{"text":"greyhound","weight":-1}]),
        json!([{"text":"greyhound","note":1}]),
        json!(["greyhound"]),
        json!(
            (0..9)
                .map(|i| json!({"text":format!("q{i}")}))
                .collect::<Vec<_>>()
        ),
    ] {
        let mut request = base.clone();
        request["query_variants"] = invalid.clone();
        assert!(retrieval::rank(&request).is_err(), "{invalid}");
    }
    let mut empty = base.clone();
    empty["query_variants"] = json!([]);
    let empty = retrieval::rank(&empty).unwrap();
    assert!(empty.get("query_variants").is_none());
    assert_eq!(empty["fused"], plain["fused"]);
    assert_eq!(retrieval::rank(&base).unwrap()["fused"], plain["fused"]);
}

#[test]
fn the_speaker_arm_ranks_passages_by_what_the_user_said() {
    let mut rows = vec![
        passage(
            "told",
            "d1",
            "user: I finally bought running shoes from Brooks.\n\nassistant: Enjoy them!",
        ),
        passage(
            "advised",
            "d2",
            "user: Any tips for a beginner?\n\nassistant: Good running shoes matter. \
             Try running shoes from Brooks or Hoka; buy running shoes after a gait test.",
        ),
    ];
    for i in 0..10 {
        rows.push(passage(
            &format!("x{i}"),
            &format!("n{i}"),
            &format!("user: Note {i} about compost bins in the garden."),
        ));
    }
    let base = json!({"scope":"speaker-test","question":"Which brand of running shoes did I buy?",
        "rows":rows,"limit":2,"debug_fused":2});
    let plain = retrieval::rank(&base).unwrap();
    assert!(plain.get("speaker").is_none());
    assert_eq!(plain["fused"][0], "advised", "{plain}");
    let mut request = base.clone();
    request["speaker"] = json!(true);
    let keyed = retrieval::rank(&request).unwrap();
    assert_eq!(keyed["fused"][0], "told", "{keyed}");
    assert_eq!(keyed["speaker"]["keep"], json!(["user"]));
    assert_eq!(keyed["speaker"]["rows"], 12);
    assert_eq!(keyed["speaker"]["ranked"], 1);
    assert!(
        keyed["reasons"]["told"]
            .as_array()
            .unwrap()
            .contains(&json!("speaker"))
    );
    // `auto` keys the arm on the side the question asks about.
    let mut auto = base.clone();
    auto["speaker"] = json!("auto");
    let mine = retrieval::rank(&auto).unwrap();
    assert_eq!(mine["speaker"]["auto"], "user");
    assert_eq!(mine["speaker"]["keep"], json!(["user"]));
    assert_eq!(mine["fused"], keyed["fused"]);
    auto["question"] = json!("Which brand of running shoes did you recommend?");
    let theirs = retrieval::rank(&auto).unwrap();
    assert_eq!(theirs["speaker"]["keep"], json!(["assistant"]));
    assert_eq!(theirs["speaker"]["rows"], 2);
    assert_eq!(theirs["fused"][0], "advised", "{theirs}");
    auto["question"] = json!("In our previous chat, which running shoes came up?");
    assert_eq!(
        retrieval::rank(&auto).unwrap()["speaker"],
        json!({"auto":"none"})
    );
    for (question, side) in [
        ("What did you say about compost?", "assistant"),
        ("You told me to stretch first; why?", "assistant"),
        ("Remind me of your last answer about tents.", "assistant"),
        ("Can you recommend running shoes?", "user"),
        ("What did I tell you about my shoes?", "user"),
        ("We talked about shoes, right?", "none"),
    ] {
        assert_eq!(retrieval::auto_speaker(question), side, "{question}");
    }
    request["arm_weights"] = json!({"speaker":0.0});
    assert_eq!(retrieval::rank(&request).unwrap()["fused"], plain["fused"]);
    request["speaker"] = json!(false);
    assert_eq!(retrieval::rank(&request).unwrap()["fused"], plain["fused"]);
    for invalid in [
        json!("user"),
        json!({"keep":[]}),
        json!({"keep":["a:b"]}),
        json!({"keep":["user"],"other":1}),
    ] {
        request["speaker"] = invalid;
        assert!(retrieval::rank(&request).is_err());
    }
    let user = std::collections::BTreeSet::from(["user".to_owned()]);
    let transcript = "tail of an earlier turn\nUser: first line\nsecond line\nNote: still mine\n\
                      assistant: reply\nuser: again";
    assert_eq!(
        retrieval::speaker_text(transcript, &user),
        " first line\nsecond line\nNote: still mine\n again\n"
    );
    // A kept name ends at any other capitalized speaker name.
    let caroline = std::collections::BTreeSet::from(["caroline".to_owned()]);
    let chat =
        "Caroline: I went to a LGBTQ support group.\nMelanie: That's brave!\nCaroline: Thanks";
    assert_eq!(
        retrieval::speaker_text(chat, &caroline),
        " I went to a LGBTQ support group.\n Thanks\n"
    );
}

#[test]
fn key_expansion_merges_a_rows_abstractions_into_its_lexical_key() {
    let mut rows = vec![
        passage(
            "kept",
            "a",
            "user: Brought him home today. He chewed a slipper.",
        ),
        passage(
            "said",
            "b",
            "user: Which shelter has beagle puppies? I want to adopt one.",
        ),
    ];
    for i in 0..6 {
        rows.push(passage(
            &format!("x{i}"),
            &format!("n{i}"),
            &format!("user: Note {i} about compost bins in the garden."),
        ));
    }
    rows[0]["abstractions"] = json!(["The user adopted a beagle puppy named Max."]);
    let mut request = json!({"scope":"key-expansion-test",
        "question":"What name did I give the beagle puppy I adopted?",
        "rows":rows,"limit":2,"debug_fused":3});
    let plain = retrieval::rank(&request).unwrap();
    assert!(plain.get("key_expansion").is_none());
    assert!(
        !plain["fused"].as_array().unwrap().contains(&json!("kept")),
        "{plain}"
    );
    request["key_expansion"] = json!("merge");
    let merged = retrieval::rank(&request).unwrap();
    assert_eq!(merged["key_expansion"], json!({"mode":"merge","rows":1}));
    assert_eq!(merged["fused"][0], "kept", "{merged}");
    assert_eq!(merged["sources"][0], "kept");
    request["key_expansion"] = json!(false);
    assert_eq!(retrieval::rank(&request).unwrap()["fused"], plain["fused"]);
    for invalid in [json!(true), json!("replace")] {
        request["key_expansion"] = invalid;
        assert!(retrieval::rank(&request).is_err());
    }
    request["key_expansion"] = json!("merge");
    request["rows"][0]["abstractions"] = json!(["x".repeat(1001)]);
    assert!(retrieval::rank(&request).is_err());
}

#[test]
fn origin_quotas_let_the_best_origin_give_more_and_counting_keeps_the_limit() {
    let mut rows = Vec::new();
    for i in 0..5 {
        rows.push(passage(
            &format!("a{i}"),
            "A",
            &format!("user: The aquarium tank {i} holds neon tetras and guppies."),
        ));
    }
    for i in 0..3 {
        rows.push(passage(
            &format!("b{i}"),
            &format!("B{i}"),
            &format!("user: A tank {i} note about fish."),
        ));
    }
    let base = json!({"scope":"quota-test","question":"Which fish does the aquarium tank hold?","rows":rows,"limit":6});
    let count = |ranked: &Value| {
        ranked["sources"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|s| s.as_str().unwrap().starts_with('a'))
            .count()
    };
    assert_eq!(count(&retrieval::rank(&base).unwrap()), 2);
    let mut quota = base.clone();
    quota["origin_quota"] = json!({"max": 3, "min": 1});
    let ranked = retrieval::rank(&quota).unwrap();
    assert!(count(&ranked) >= 3, "{ranked}");
    // A counting question is still reported as one; the selection keeps the
    // requested limit (the deeper read, `adaptive_depth`, is not supported).
    let mut counting = base.clone();
    counting["question"] = json!("How many fish does the aquarium tank hold?");
    counting["limit"] = json!(2);
    counting["debug_fused"] = json!(5);
    let counted = retrieval::rank(&counting).unwrap();
    assert_eq!(counted["evidence_strength"]["aggregate"], true);
    assert_eq!(counted["evidence_strength"]["limit"], 2);
    assert!(counted["evidence_strength"].get("adaptive_depth").is_none());
    assert_eq!(counted["sources"].as_array().unwrap().len(), 2, "{counted}");
    assert_eq!(counted["fused"].as_array().unwrap().len(), 5);
}

#[test]
fn an_overflowing_window_keeps_its_best_matching_paragraphs() {
    let filler = "assistant: ".to_string()
        + &"Here is a long general answer about many unrelated topics. ".repeat(40);
    let body = format!(
        "{filler}\n\n{filler}\n\nuser: My new aquarium holds six neon tetras.\n\nassistant: Nice.\n"
    );
    let hit = json!({"identity":"h","text":body,"source":{"id":"file:s.txt","path":"s.txt","source_kind":"repository_file",
        "byte_start":100,"byte_end":100 + body.len(),"line":7,"sha256":"x"}});
    let request = |focus: bool| {
        json!({"repo":{"name":"r","snapshot":"s"},"question":"How many neon tetras are in my aquarium?","hits":[hit.clone()],
            "memories":[],"max_tokens":600,"max_chars":4000,"pack_focus":focus})
    };
    let plain = packing::pack(&request(false)).unwrap();
    assert!(
        !plain["markdown"].as_str().unwrap().contains("neon tetras"),
        "the prefix cut drops the answer"
    );
    let focused = packing::pack(&request(true)).unwrap();
    assert!(
        focused["markdown"]
            .as_str()
            .unwrap()
            .contains("six neon tetras"),
        "{}",
        focused["markdown"]
    );
    let source = &focused["sources"][0];
    assert_eq!(source["focused"], true);
    let start = source["byte_start"].as_u64().unwrap() as usize;
    let end = source["byte_end"].as_u64().unwrap() as usize;
    assert!(start > 100 && end <= 100 + body.len());
    assert!(body[start - 100..end - 100].contains("six neon tetras"));
    assert_eq!(source["window_byte_start"], 100);
    assert_eq!(
        source["line"].as_u64().unwrap(),
        7 + body[..start - 100].matches('\n').count() as u64
    );
    assert!(focused["markdown"].as_str().unwrap().contains(&format!(
        "Source [repository_file]: s.txt:{}\n",
        source["line"]
    )));
    assert!(
        !focused["markdown"]
            .as_str()
            .unwrap()
            .contains("Original source header"),
        "a window after the document start has no opening header to keep"
    );
}

#[test]
fn a_focused_excerpt_keeps_the_document_header_it_skips() {
    let filler = "Caroline: ".to_string()
        + &"I talked about many unrelated weekend plans and errands. ".repeat(40);
    let header = "Conversation date: 1:56 pm on 8 May, 2023";
    let body = format!(
        "{header}\n\n{filler}\n\n{filler}\n\nMelanie: I adopted six neon tetras yesterday.\n\nCaroline: Nice.\n"
    );
    let hit = json!({"identity":"h","text":body,"source":{"id":"file:s.txt","path":"s.txt","source_kind":"repository_file",
        "byte_start":0,"byte_end":body.len(),"line":1,"sha256":"x"}});
    let request = json!({"repo":{"name":"r","snapshot":"s"},"question":"When did Melanie adopt the neon tetras?",
        "hits":[hit],"memories":[],"max_tokens":600,"max_chars":4000,"pack_focus":true});
    let packed = packing::pack(&request).unwrap();
    let markdown = packed["markdown"].as_str().unwrap();
    assert!(markdown.contains("six neon tetras"), "{markdown}");
    assert!(
        markdown.contains(&format!(
            "Original source header (quoted data): {}\n",
            serde_json::to_string(header).unwrap()
        )),
        "{markdown}"
    );
    let source = &packed["sources"][0];
    assert_eq!(source["focused"], true);
    assert_eq!(source["source_context"]["text"], header);
    assert_eq!(source["source_context"]["byte_end"], header.len());
    let start = source["byte_start"].as_u64().unwrap() as usize;
    let end = source["byte_end"].as_u64().unwrap() as usize;
    assert!(start > header.len() && body[start..end].contains("six neon tetras"));
    assert!(packed["tokens"].as_u64().unwrap() <= 600);
}

#[test]
fn focus_options_match_stems_and_bound_the_run() {
    let filler = |i: usize| {
        let speaker = if i.is_multiple_of(2) {
            "Caroline"
        } else {
            "Ana"
        };
        format!("{speaker}: Turn {i} is about errands and weekend plans of every sort.\n\n")
    };
    let answer = "Ana: I painted the harbour at dawn.\n\nCaroline: Lovely.\n\n";
    let body = (0..30).map(filler).collect::<String>()
        + answer
        + &(30..60).map(filler).collect::<String>();
    let other: String = (0..60)
        .map(|i| format!("Bo: Line {i} of a different talk about bread and buses.\n\n"))
        .collect();
    let hit = |n: usize, text: &str| {
        json!({"identity":format!("h{n}"),"text":text,"source":{"id":format!("file:s{n}.txt"),"path":format!("s{n}.txt"),
            "source_kind":"repository_file","byte_start":100,"byte_end":100 + text.len(),"line":7,"sha256":"x"}})
    };
    let ask = |question: &str, focus: Value| {
        json!({"repo":{"name":"r","snapshot":"s"},"question":question,"hits":[hit(1, &body), hit(2, &other)],
            "memories":[],"max_tokens":900,"max_chars":8000,"pack_focus":focus})
    };
    let request = |focus: Value| ask("Who paints harbours?", focus);
    let excerpt = |packed: &Value, i: usize, text: &str| {
        let source = &packed["sources"][i];
        let start = source["byte_start"].as_u64().unwrap() as usize - 100;
        let end = source["byte_end"].as_u64().unwrap() as usize - 100;
        text[start..end].to_owned()
    };
    let exact = packing::pack(&request(json!(true))).unwrap();
    assert!(
        !excerpt(&exact, 0, &body).contains("painted the harbour"),
        "no exact word matches, so the prefix stays"
    );
    let stemmed = packing::pack(&request(json!({"stem":true}))).unwrap();
    assert!(excerpt(&stemmed, 0, &body).contains("painted the harbour"));
    assert_eq!(stemmed["sources"][0]["focused"], true);
    let bounded = packing::pack(&request(json!({"stem":true,"context":1}))).unwrap();
    assert_eq!(excerpt(&bounded, 0, &body), filler(29) + answer);
    // "Ana" tags half the turns, so her other turns do not extend the run.
    let named = packing::pack(&ask(
        "When did Ana paint harbours?",
        json!({"stem":true,"context":1}),
    ))
    .unwrap();
    assert_eq!(excerpt(&named, 0, &body), filler(29) + answer);
    assert!(
        excerpt(&bounded, 1, &other).len() > excerpt(&stemmed, 1, &other).len(),
        "the share the bounded run leaves goes to the next source"
    );
    for packed in [&exact, &stemmed, &bounded] {
        assert!(packed["tokens"].as_u64().unwrap() <= 900);
    }
}

#[test]
fn compact_citations_spend_the_citation_lines_on_evidence() {
    let turns: String = (0..60)
        .map(|i| format!("user: Turn {i} mentions a garden visit and the weather.\n\n"))
        .collect();
    let hit = |n: usize, id: &str| {
        json!({"identity":format!("h{n}"),"text":turns,"source":{"id":id,"path":format!("s{n}.txt"),
            "source_kind":"repository_file","byte_start":0,"byte_end":turns.len(),"line":1,
            "sha256":"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"},
            "next_byte":turns.len() + 10})
    };
    let request = |compact: bool| {
        json!({"repo":{"name":"r","snapshot":"s"},"question":"garden visit",
            "hits":[hit(1, "file:s1.txt"), hit(2, "memory:m2")],"memories":[],"max_tokens":500,"max_chars":4000,
            "compact_citations":compact})
    };
    let full = packing::pack(&request(false)).unwrap();
    let compact = packing::pack(&request(true)).unwrap();
    let full_md = full["markdown"].as_str().unwrap();
    let compact_md = compact["markdown"].as_str().unwrap();
    assert!(full_md.contains("SHA-256: 0123") && full_md.contains("ID: file:s1.txt"));
    assert!(full_md.contains("Continue at byte"), "{full_md}");
    assert!(!compact_md.contains("SHA-256"), "{compact_md}");
    assert!(!compact_md.contains("ID: file:s1.txt"), "{compact_md}");
    assert!(
        compact_md.contains("ID: memory:m2"),
        "an ID the path cannot rebuild is kept"
    );
    assert!(!compact_md.contains("Continue at byte"), "{compact_md}");
    assert!(compact_md.contains("Source [repository_file]: s1.txt:1\n"));
    assert_eq!(compact["compact_citations"], true);
    assert_eq!(full["compact_citations"], false);
    for (a, b) in full["sources"]
        .as_array()
        .unwrap()
        .iter()
        .zip(compact["sources"].as_array().unwrap())
    {
        assert_eq!(a["id"], b["id"]);
        assert_eq!(a["sha256"], b["sha256"]);
        assert_eq!(a["byte_start"], b["byte_start"]);
        assert!(b["byte_end"].as_u64() >= a["byte_end"].as_u64());
    }
    let quoted = |packet: &serde_json::Value| -> u64 {
        packet["sources"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["byte_end"].as_u64().unwrap() - s["byte_start"].as_u64().unwrap())
            .sum()
    };
    assert!(
        quoted(&compact) > quoted(&full),
        "the saved citation tokens buy more quoted evidence"
    );
    assert!(compact["tokens"].as_u64().unwrap() <= 500);
}

#[test]
fn packing_rejects_wrapping_source_offsets() {
    let mut request = json!({"repo":{"name":"r","snapshot":"s"},
        "hits":[{"identity":"h","text":"evidence","source":{"id":"file:s.txt","path":"s.txt",
            "source_kind":"repository_file","byte_start":u64::MAX,"line":1,"sha256":"x"}}],
        "memories":[],"max_tokens":600,"max_chars":4000});
    assert_eq!(
        packing::pack(&request).unwrap_err().kind(),
        ErrorKind::InvalidInput
    );
    request["hits"][0]["source"]["byte_start"] = json!(0);
    request["hits"][0]["source"]["line"] = json!(u64::MAX);
    request["hits"][0]["text"] = json!("unrelated information\n\n".repeat(200) + "neon tetras\n");
    request["pack_focus"] = json!(true);
    request["question"] = json!("neon tetras");
    assert_eq!(
        packing::pack(&request).unwrap_err().kind(),
        ErrorKind::InvalidInput
    );
}

fn conversation_hit(identity: &str, id: &str, date: &str, start: u64, text: &str) -> Value {
    json!({"identity":identity,"text":text,"source":{"id":id,"path":format!("{id}.md"),
        "source_kind":"conversation","byte_start":start,"byte_end":start + text.len() as u64,
        "line":1,"sha256":format!("{id}-hash"),"recorded_at":date}})
}

#[test]
fn chronological_packing_spends_by_rank_and_presents_by_date_with_gap_markers() {
    let hits = json!([
        conversation_hit("may", "s2", "2023-05-08T09:00:00Z", 0, "We adopted a beagle named Pip."),
        conversation_hit("late", "s1", "2023-01-02T10:00:00Z", 900, "Later that day we chose the name."),
        {"identity":"code","text":"fn pet() {}","source":{"id":"file:src/pet.rs","path":"src/pet.rs",
            "source_kind":"repository_file","byte_start":0,"line":1,"sha256":"code-hash"}},
        conversation_hit("early", "s1", "2023-01-02T10:00:00Z", 0, "We visited the shelter."),
    ]);
    let request = |order: Value| {
        json!({"repo":{"id":"r","name":"Repo","snapshot":"one"},"question":"When did we adopt the dog?",
            "hits":hits,"memories":[],"max_tokens":1200,"max_chars":8000,"pack_order":order})
    };
    let ranked = packing::pack(&request(Value::Null)).unwrap();
    let dated = packing::pack(&request(json!("chronological"))).unwrap();
    assert!(ranked.get("pack_order").is_none());
    assert_eq!(dated["pack_order"], "chronological");
    let order = |packet: &Value| {
        packet["sources"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| format!("{}@{}", s["id"].as_str().unwrap(), s["byte_start"]))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        order(&ranked),
        ["s2@0", "s1@900", "file:src/pet.rs@0", "s1@0"]
    );
    // The same excerpts, earliest first, a source's excerpts in reading
    // order and the undated after the dated.
    assert_eq!(
        order(&dated),
        ["s1@0", "s1@900", "s2@0", "file:src/pet.rs@0"]
    );
    let ranks: Vec<_> = dated["sources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["rank"].as_u64().unwrap())
        .collect();
    assert_eq!(ranks, [4, 2, 1, 3]);
    let markdown = dated["markdown"].as_str().unwrap();
    assert_eq!(dated["gap_markers"], 1);
    let marker = markdown
        .find("[Not shown: bytes 23–900 of this source lie between these excerpts; read them with read_project_evidence.]")
        .unwrap();
    assert!(markdown.find("We visited the shelter.").unwrap() < marker);
    assert!(marker < markdown.find("Later that day").unwrap());
    assert!(markdown.find("Later that day").unwrap() < markdown.find("beagle").unwrap());
    assert!(markdown.find("beagle").unwrap() < markdown.find("fn pet()").unwrap());
    assert_eq!(
        dated["tokens"].as_u64().unwrap() as usize,
        text::count(markdown)
    );
    assert!(text::count(markdown) <= 1200);
    assert!(
        packing::pack(&request(json!("date")))
            .unwrap_err()
            .to_string()
            .contains("pack_order")
    );
}

#[test]
fn excerpts_the_caller_holds_become_references_and_their_share_goes_on() {
    let long = "The deploy runbook says to drain the queue first. ".repeat(120);
    let hits = json!([
        conversation_hit(
            "held",
            "s1",
            "2023-01-02",
            0,
            "Deploys happen on Tuesdays after review."
        ),
        conversation_hit("fresh", "s2", "2023-01-03", 0, &long),
    ]);
    let request = |shown: Value| {
        json!({"repo":{"id":"r","name":"Repo","snapshot":"one"},"question":"How do deploys work?",
            "hits":hits,"memories":[],"max_tokens":700,"max_chars":6000,"shown":shown})
    };
    let plain = packing::pack(&request(Value::Null)).unwrap();
    assert!(plain.get("already_shown").is_none());
    let held = packing::pack(&request(
        json!([{"id":"s1","sha256":"s1-hash","byte_start":0,"byte_end":20},
        {"id":"s1","byte_start":15,"byte_end":200}]),
    ))
    .unwrap();
    let markdown = held["markdown"].as_str().unwrap();
    assert!(!markdown.contains("Deploys happen on Tuesdays"));
    assert!(markdown.contains(
        "Already in context: bytes 0–40 were quoted to you earlier and are not repeated."
    ));
    assert_eq!(held["already_shown"][0]["id"], "s1");
    assert_eq!(held["already_shown"][0]["byte_end"], 40);
    let ids: Vec<_> = held["sources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["s2"]);
    let quoted = |packet: &Value, id: &str| {
        packet["sources"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|s| s["id"] == id)
            .map(|s| s["byte_end"].as_u64().unwrap() - s["byte_start"].as_u64().unwrap())
            .sum::<u64>()
    };
    assert!(
        quoted(&held, "s2") > quoted(&plain, "s2"),
        "the reference's share goes to the next hit"
    );
    assert!(text::count(markdown) <= 700);
    // A span of another version, or one that does not cover the excerpt,
    // leaves the text in place.
    for shown in [
        json!([{"id":"s1","sha256":"older","byte_start":0,"byte_end":200}]),
        json!([{"id":"s1","byte_start":0,"byte_end":20},{"id":"s1","byte_start":21,"byte_end":200}]),
    ] {
        let packet = packing::pack(&request(shown)).unwrap();
        assert!(
            packet["markdown"]
                .as_str()
                .unwrap()
                .contains("Deploys happen on Tuesdays")
        );
        assert_eq!(packet["already_shown"], json!([]));
    }
    for bad in [
        json!([{"id":"s1","byte_start":5,"byte_end":5}]),
        json!([{"byte_start":0,"byte_end":5}]),
        json!("s1"),
        json!(vec![json!({"id":"s1","byte_start":0,"byte_end":5}); 201]),
    ] {
        assert!(packing::pack(&request(bad)).is_err());
    }
}

#[test]
fn reading_guidance_is_an_opt_in_header_line_within_budget() {
    let request = |guidance: Value| {
        json!({"repo":{"id":"r","name":"Repo","snapshot":"one"},"question":"When is the trip?",
            "hits":[conversation_hit("trip", "s1", "2023-01-02", 0, "Assistant: You could go on May 3.")],
            "memories":[],"max_tokens":600,"max_chars":4000,"reading_guidance":guidance})
    };
    let plain = packing::pack(&request(Value::Null)).unwrap();
    let guided = packing::pack(&request(json!(true))).unwrap();
    assert!(
        !plain["markdown"]
            .as_str()
            .unwrap()
            .contains("Reading conversations")
    );
    assert!(plain.get("reading_guidance").is_none());
    let markdown = guided["markdown"].as_str().unwrap();
    let guidance = markdown
        .find("Reading conversations: what a user says")
        .unwrap();
    assert!(guidance < markdown.find("Source [conversation]").unwrap());
    assert!(markdown.contains("an assistant's proposals, examples and suggested dates"));
    assert_eq!(guided["reading_guidance"], true);
    assert_eq!(
        guided["tokens"].as_u64().unwrap() as usize,
        text::count(markdown)
    );
    assert!(packing::pack(&request(json!("yes"))).is_err());
}

/// Past `MAX_STATIC_EDGES`, `rank` folds the static
/// relationship edges per (source file, target file, kind) instead of failing,
/// with the same relationship lift, and cuts them at the bound only when the
/// distinct pairs alone exceed it. Either way the result says so; within the
/// bound nothing is reported.
#[test]
fn rank_folds_or_caps_static_edges_past_the_bound_instead_of_failing() {
    let row = |path: &str, text: &str| {
        json!({"identity":format!("file:{path}:0"),"kind":"source","origin":format!("file:{path}"),
            "title":path,"text":text,"source":{"id":format!("file:{path}"),"path":path}})
    };
    let rows = vec![
        row(
            "src/auth.py",
            "def refresh_token(): rotate the session token",
        ),
        row("src/session.py", "class Session: stores cookies"),
        row("src/unrelated.py", "def render(): draws widgets"),
    ];
    let request = |edges: Vec<Value>| json!({"scope":"static-edge-bound","question":"refresh token","rows":rows,"limit":3,"edges":edges});
    let call = |line: usize| json!({"source":"src/auth.py","target":"src/session.py","kind":"calls","line":line});
    // Within the bound: one edge per call site, no report.
    let within = retrieval::rank(&request((1..=10).map(call).collect())).unwrap();
    assert!(within.get("static_edges").is_none(), "{within}");
    assert!(
        within["reasons"]["file:src/session.py:0"]
            .as_array()
            .unwrap()
            .contains(&json!("static_relationship:1")),
        "{within}"
    );
    // 60,000 call sites between two files fold to one pair: same lift.
    let folded = retrieval::rank(&request((1..=60_000).map(call).collect())).unwrap();
    assert_eq!(folded["static_edges"]["received"], 60_000);
    assert_eq!(folded["static_edges"]["distinct"], 1);
    assert_eq!(folded["static_edges"]["aggregated"], true);
    assert_eq!(folded["static_edges"]["capped"], false);
    assert_eq!(folded["sources"], within["sources"]);
    assert_eq!(folded["reasons"], within["reasons"]);
    // 60,000 distinct file pairs: cut at the bound and reported as capped.
    let unrelated = || {
        (0..60_000)
            .map(|i| json!({"source":format!("a/{}.py", i / 300),"target":format!("b/{}.py", i % 300),"kind":"calls","line":1}))
            .collect::<Vec<Value>>()
    };
    let capped = retrieval::rank(&request(unrelated())).unwrap();
    assert_eq!(capped["static_edges"]["distinct"], 60_000);
    assert_eq!(capped["static_edges"]["used"], retrieval::MAX_STATIC_EDGES);
    assert_eq!(capped["static_edges"]["capped"], true);
    assert_eq!(capped["static_edges"]["lift_complete"], true);
    // The pair the lift reads comes last in input order and still survives
    // the cut: pairs touching the top passages' files are kept first.
    let mut late = unrelated();
    late.push(call(1));
    let kept = retrieval::rank(&request(late)).unwrap();
    assert_eq!(
        kept["static_edges"]["capped"], true,
        "{}",
        kept["static_edges"]
    );
    assert_eq!(kept["static_edges"]["lift_complete"], true);
    assert_eq!(kept["sources"], within["sources"]);
    assert_eq!(kept["reasons"], within["reasons"]);
    // Two hops (relationships mode): the second hop's pair is kept as well,
    // so the lift is exactly the one the two pairs alone give.
    let two_hops = |edges: Vec<Value>| {
        let mut rows = rows.clone();
        rows.push(row("src/store.py", "def save(): writes rows"));
        json!({"scope":"static-edge-bound","question":"refresh token","rows":rows,"limit":4,
            "mode":"relationships","edges":edges})
    };
    let chain = || {
        vec![
            call(1),
            json!({"source":"src/session.py","target":"src/store.py","kind":"calls","line":1}),
        ]
    };
    let small = retrieval::rank(&two_hops(chain())).unwrap();
    assert!(
        small["reasons"]["file:src/store.py:0"]
            .as_array()
            .unwrap()
            .contains(&json!("static_relationship:2")),
        "{small}"
    );
    let mut long = unrelated();
    long.extend(chain());
    let large = retrieval::rank(&two_hops(long)).unwrap();
    assert_eq!(large["static_edges"]["lift_complete"], true);
    assert_eq!(large["sources"], small["sources"]);
    assert_eq!(large["reasons"], small["reasons"]);
    // More pairs touch the top passages' file than the bound holds: the
    // report says the lift could not read them all.
    let fan: Vec<Value> = (0..=retrieval::MAX_STATIC_EDGES)
        .map(|i| json!({"source":"src/auth.py","target":format!("t/{i}.py"),"kind":"calls","line":1}))
        .collect();
    let partial = retrieval::rank(&request(fan)).unwrap();
    assert_eq!(partial["static_edges"]["capped"], true);
    assert_eq!(partial["static_edges"]["lift_complete"], false);
}

/// A packet over several projects of a workspace names each
/// excerpt's and memory's project (`project` and `repository_id` on the
/// source or memory), and a packet over one project, whose sources carry
/// neither, is unchanged.
#[test]
fn excerpts_and_memories_that_name_their_project_say_so() {
    let request = |project: bool, compact: bool| {
        let mut source = json!({"source_kind":"repository_file","path":"app/invoice.py","id":"file:app/invoice.py",
            "sha256":"hash","byte_start":0,"line":1});
        let mut memory = json!({"id":"m1","kind":"decision","authority":"reviewed","title":"Rounding","body":"Round half to even."});
        if project {
            for item in [&mut source, &mut memory] {
                item["project"] = json!("Shop");
                item["repository_id"] = json!("p-shop");
            }
        }
        json!({"repo":{"id":null,"name":"2 projects of this workspace: Shop, Money","snapshot":"s"},
            "question":"invoice total","hits":[{"identity":"p-shop:file:app/invoice.py:0","text":"def settle_invoice(): pass\n","source":source}],
            "memories":[{"memory":memory}],"max_tokens":800,"max_chars":4000,"compact_citations":compact})
    };
    let plain = packing::pack(&request(false, false)).unwrap();
    let named = packing::pack(&request(true, false)).unwrap();
    let markdown = named["markdown"].as_str().unwrap();
    assert!(
        markdown.contains("Source [repository_file]: app/invoice.py:1\nProject: Shop (p-shop)\nID: file:app/invoice.py\n"),
        "{markdown}"
    );
    assert!(
        markdown.contains("Reference: m1\nProject: Shop (p-shop)\n"),
        "{markdown}"
    );
    assert!(!plain["markdown"].as_str().unwrap().contains("Project: "));
    assert_eq!(named["sources"][0]["repository_id"], "p-shop");
    assert_eq!(named["memories"][0]["project"], "Shop");
    let compact = packing::pack(&request(true, true)).unwrap();
    assert!(
        compact["markdown"]
            .as_str()
            .unwrap()
            .contains("app/invoice.py:1\nProject: Shop\n"),
        "{}",
        compact["markdown"]
    );
}

/// Source IDs repeat across the projects of
/// a workspace (`file:README.md` in each). A `shown` span stands for one
/// project's source, its own `repository_id`'s or else the packet's
/// (`repo.id`, none in a packet that names no project), and the reference
/// it produces names that project. Chronological presentation keeps each
/// project's excerpts of a source together and marks gaps within them only.
#[test]
fn shown_spans_and_gap_markers_keep_the_projects_of_a_workspace_apart() {
    let readme = |project: &str, start: u64, text: &str| {
        json!({"identity":format!("{project}:file:README.md:{start}"),"text":text,
            "source":{"id":"file:README.md","path":"README.md","source_kind":"repository_file",
                "byte_start":start,"byte_end":start + text.len() as u64,"line":1,"sha256":"same-bytes",
                "recorded_at":"2023-01-02T10:00:00Z","project":project.to_uppercase(),"repository_id":project}})
    };
    let hits = json!([
        readme("a", 0, "Alpha installs with make."),
        readme("b", 0, "Beta installs with cargo."),
        readme("a", 500, "Alpha runs on Linux."),
        readme("b", 900, "Beta runs on macOS."),
    ]);
    let request = |named: Option<&str>, shown: Value, order: Value| {
        json!({"repo":{"id":named,"name":"2 projects of this workspace: A, B","snapshot":"s"},
            "question":"How do they install?","hits":hits,"memories":[],"max_tokens":1200,
            "max_chars":8000,"shown":shown,"pack_order":order})
    };
    // A span of project a's README holds a's excerpt, not b's same bytes.
    let held = packing::pack(&request(
        None,
        json!([{"id":"file:README.md","repository_id":"a","byte_start":0,"byte_end":30}]),
        Value::Null,
    ))
    .unwrap();
    let markdown = held["markdown"].as_str().unwrap();
    assert!(!markdown.contains("Alpha installs"), "{markdown}");
    assert!(markdown.contains("Beta installs"), "{markdown}");
    assert_eq!(
        held["already_shown"],
        json!([{"id":"file:README.md","sha256":"same-bytes","path":"README.md","line":1,
            "byte_start":0,"byte_end":25,"repository_id":"a"}])
    );
    // A span that names no project is the packet's project's: in a packet
    // that names none it holds nothing; in one that names b, b's excerpt.
    let unnamed = json!([{"id":"file:README.md","byte_start":0,"byte_end":30}]);
    let nobody = packing::pack(&request(None, unnamed.clone(), Value::Null)).unwrap();
    assert_eq!(nobody["already_shown"], json!([]));
    let anchored = packing::pack(&request(Some("b"), unnamed, Value::Null)).unwrap();
    assert_eq!(anchored["already_shown"][0]["repository_id"], "b");
    let markdown = anchored["markdown"].as_str().unwrap();
    assert!(markdown.contains("Alpha installs") && !markdown.contains("Beta installs"));
    // By date: each project's excerpts of README.md together, in reading
    // order, with a marker inside each project's README and none between
    // the two projects.
    let dated = packing::pack(&request(None, Value::Null, json!("chronological"))).unwrap();
    let order: Vec<String> = dated["sources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| {
            format!(
                "{}@{}",
                s["repository_id"].as_str().unwrap(),
                s["byte_start"]
            )
        })
        .collect();
    assert_eq!(order, ["a@0", "a@500", "b@0", "b@900"]);
    assert_eq!(dated["gap_markers"], 2);
    let markdown = dated["markdown"].as_str().unwrap();
    let alpha = markdown.find("Alpha installs").unwrap();
    let alpha_gap = markdown
        .find("[Not shown: bytes 25–500 of this source")
        .unwrap();
    let beta = markdown.find("Beta installs").unwrap();
    let beta_gap = markdown
        .find("[Not shown: bytes 25–900 of this source")
        .unwrap();
    assert!(alpha < alpha_gap && alpha_gap < markdown.find("Alpha runs").unwrap());
    assert!(markdown.find("Alpha runs").unwrap() < beta);
    assert!(beta < beta_gap && beta_gap < markdown.find("Beta runs").unwrap());
}
