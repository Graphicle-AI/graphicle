// Adapted from Graphify 0.9.88 (a9860f70), Apache-2.0; see NOTICE.md.
// Upstream: graphify/benchmark.py
//! The upstream character-count context estimate; this is not an accuracy benchmark.
use crate::{
    Result, graph::Graph, paths, python_compat, query_terms, security, serve,
    serve_search::SearchIndex, serve_text,
};
use serde_json::{Value, json};
use std::path::Path;

pub const SAMPLE_QUESTIONS: &[&str] = &[
    "how does authentication work",
    "what is the main entry point",
    "how are errors handled",
    "what connects the data layer to the api",
    "what are the core abstractions",
];
pub fn safe<'a>(text: &'a str, fallback: &'a str, encoding: &str) -> &'a str {
    let encoding = encoding.to_ascii_lowercase().replace('_', "-");
    let valid = match encoding.as_str() {
        "ascii" | "us-ascii" => text.is_ascii(),
        "latin-1" | "latin1" | "iso-8859-1" => text.chars().all(|c| (c as u32) < 256),
        "utf-16" | "utf-32" | "utf-16-le" | "utf-16-be" | "utf-32-le" | "utf-32-be" => true,
        _ => {
            encoding_rs::Encoding::for_label(encoding.as_bytes()).is_some_and(|e| !e.encode(text).2)
        }
    };
    if valid { text } else { fallback }
}
pub fn horizontal_rule(width: i64, encoding: &str) -> String {
    safe("─", "-", encoding).repeat(width.max(0) as usize)
}
pub fn estimate_tokens(text: &str) -> usize {
    (text.chars().count() / 4).max(1)
}
/// Estimated tokens of the answer `query_graph` returns for `question` (BFS,
/// the MCP tool's defaults), 0 when nothing in the graph matches it. The
/// answer is the real one an agent receives: seeds, budget cut and notices
/// (Graphify 0.9.87, #4347).
pub fn query_subgraph_tokens(
    graph: &Graph,
    index: &SearchIndex,
    question: &str,
    depth: i64,
    token_budget: i64,
) -> Result<usize> {
    let text = serve_text::query_text(
        graph,
        index,
        question,
        serve_text::QueryOptions {
            mode: "bfs",
            depth,
            budget: token_budget,
            filters: &[],
            path: None,
            seeds: None,
            seed_root: None,
        },
    )?;
    // The answer the MCP tool returns when no node matches the question.
    if text == serve_text::QUERY_NO_MATCH {
        return Ok(0);
    }
    Ok(estimate_tokens(&text))
}
fn round_one(value: f64) -> f64 {
    format!("{value:.1}").parse().unwrap()
}
pub fn run_benchmark(
    graph_path: Option<&Path>,
    corpus_words: Option<i64>,
    questions: Option<&[String]>,
    cap: Option<u64>,
) -> Result<Value> {
    let default = paths::default_graph_json();
    let path = graph_path
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new(&default));
    if let Some(cap) = cap {
        security::check_graph_file_size_cap_with(path, cap)?;
    }
    // Loaded exactly as the MCP server loads it (size cap, links/edges
    // normalization for a raw --no-cluster graph), so every answer measured
    // is the one query_graph returns.
    let graph = serve::load_graph(path)?;
    let index = SearchIndex::new(&graph);
    let words = corpus_words.unwrap_or(graph.nodes.len() as i64 * 50);
    let tokens = words
        .checked_mul(100)
        .ok_or("Corpus word count exceeds supported range")?
        .div_euclid(75);
    let defaults: Vec<_> = SAMPLE_QUESTIONS.iter().map(|s| s.to_string()).collect();
    let questions = questions.filter(|q| !q.is_empty()).unwrap_or(&defaults);
    let mut per_question = Vec::new();
    let mut sum = 0;
    for question in questions {
        let count = query_subgraph_tokens(&graph, &index, question, 3, 2000)?;
        if count > 0 {
            sum += count;
            per_question.push(json!({"question":question,"query_tokens":count,"reduction":round_one(tokens as f64/count as f64)}));
        }
    }
    if per_question.is_empty() {
        return Ok(
            json!({"error":"No matching nodes found for sample questions. Build the graph first."}),
        );
    }
    let avg = sum / per_question.len();
    Ok(
        json!({"corpus_tokens":tokens,"corpus_words":words,"nodes":graph.nodes.len(),"edges":graph.edge_count(),
        "avg_query_tokens":avg,"reduction_ratio":round_one(tokens as f64/avg as f64),"per_question":per_question}),
    )
}
fn commas(value: &Value) -> String {
    let text = python_compat::string(value);
    let negative = text.starts_with('-');
    let digits = text.trim_start_matches('-');
    let mut output = String::new();
    if negative {
        output.push('-');
    }
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            output.push(',');
        }
        output.push(c);
    }
    output
}
pub fn format_benchmark(result: &Value, encoding: &str) -> String {
    if let Some(e) = result.get("error") {
        return format!("Benchmark error: {}\n", python_compat::string(e));
    }
    let mut out = format!(
        "\ngraphicle token reduction benchmark\n{}\n  Corpus:          {} words {} ~{} tokens (naive)\n  Graph:           {} nodes, {} edges\n  Avg query cost:  ~{} tokens\n  Reduction:       {}x fewer tokens per query\n\n  Per question:\n",
        horizontal_rule(50, encoding),
        commas(&result["corpus_words"]),
        safe("→", "->", encoding),
        commas(&result["corpus_tokens"]),
        commas(&result["nodes"]),
        commas(&result["edges"]),
        commas(&result["avg_query_tokens"]),
        python_compat::string(&result["reduction_ratio"])
    );
    for p in result["per_question"].as_array().into_iter().flatten() {
        out.push_str(&format!(
            "    [{}x] {}\n",
            python_compat::string(&p["reduction"]),
            p["question"]
                .as_str()
                .unwrap_or("")
                .chars()
                .take(55)
                .collect::<String>()
        ));
    }
    out.push('\n');
    out
}
pub fn dispatch(r: &Value) -> Result<Value> {
    let encoding = r["encoding"].as_str().unwrap_or("utf-8");
    let value = match r["function"].as_str().unwrap_or("run_benchmark") {
        "_safe" => json!(safe(
            r["unicode_char"].as_str().unwrap_or(""),
            r["ascii_fallback"].as_str().unwrap_or(""),
            encoding
        )),
        "_hr" => json!(horizontal_rule(r["width"].as_i64().unwrap_or(50), encoding)),
        "_estimate_tokens" => json!(estimate_tokens(r["text"].as_str().unwrap_or(""))),
        "_query_terms" => json!(query_terms::query_terms_with(
            r["question"].as_str().unwrap_or(""),
            r["segmentation"] != "bigrams"
        )),
        "_query_subgraph_tokens" => {
            let graph = paths::load_node_link_graph(&r["G"])?;
            json!(query_subgraph_tokens(
                &graph,
                &SearchIndex::new(&graph),
                r["question"].as_str().unwrap_or(""),
                r["depth"].as_i64().unwrap_or(3),
                r["token_budget"].as_i64().unwrap_or(2000)
            )?)
        }
        "print_benchmark" => json!(format_benchmark(&r["result"], encoding)),
        "run_benchmark" => {
            let questions: Option<Vec<String>> = r["questions"].as_array().map(|qs| {
                qs.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            });
            run_benchmark(
                r["graph_path"].as_str().map(Path::new),
                r["corpus_words"].as_i64(),
                questions.as_deref(),
                r["max_graph_bytes"].as_u64(),
            )?
        }
        _ => return Err("Unknown benchmark function".into()),
    };
    Ok(value)
}
