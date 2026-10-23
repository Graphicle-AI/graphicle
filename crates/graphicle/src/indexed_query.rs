//! `context` over a project's passage index ([`crate::passage_index`]): a
//! question is looked up in the index instead of chunking and scoring every
//! file of the project ([`packet`]). The packet is the one
//! `query::project_packet` builds: `rank` gets the lexical and speaker orders
//! of the whole catalogue, computed here from the index's counts with its
//! own formula, and only the passages the arms rank, so its fusion,
//! selection and packing are unchanged.
//!
//! The index serves `context`'s defaults; a call that asks for something
//! else (a filter, other lexical options, followed links, local models) and
//! a project with reviewed memory (whose graph walk reads every passage)
//! take the full path ([`ineligible`]), as does a project whose index is
//! missing or lacks more than [`MAX_MISSING`] of its current files.
use crate::code_graph_arm;
use crate::passage_index::{self, Index};
use crate::query::{self, Indexed, Lift, LiftScoring, Passages, Snapshot};
use crate::store::{Principal, s};
use crate::{Error, Result, digest, memory, string};
use graphicle_memory::{cache, retrieval, stem::Stemmer};
use rusqlite::Connection;
use serde_json::{Value, json};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

/// Current files the index may lack (changed since its last update) that a
/// question still reads, by laying them out and counting them itself.
pub const MAX_MISSING: usize = 2_000;
/// Bytes of those files.
pub const MAX_MISSING_BYTES: usize = 64 << 20;
/// The lexical and speaker arms' list lengths in `rank`.
const RANKED: usize = 200;

/// Why a call takes the full path instead of the index, or `None`: the
/// index serves `context`'s defaults.
pub fn ineligible(args: &Value) -> Result<Option<&'static str>> {
    // GRAPHICLE_PASSAGE_INDEX=0: every question takes the full path.
    if std::env::var("GRAPHICLE_PASSAGE_INDEX").is_ok_and(|v| {
        matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "no" | "off"
        )
    }) {
        return Ok(Some("off"));
    }
    if query::Filter::parse(args)?.is_some() {
        return Ok(Some("filter"));
    }
    let set = |key: &str| args.get(key).filter(|v| !v.is_null());
    if set("stem").is_some_and(|v| v != &json!(true)) {
        return Ok(Some("stem"));
    }
    if set("speaker").is_some_and(|v| !(v == "auto" || v.is_boolean())) {
        return Ok(Some("speaker"));
    }
    if set("query_variants").is_some_and(|v| v.as_array().is_none_or(|list| !list.is_empty())) {
        return Ok(Some("query_variants"));
    }
    if set("key_expansion").is_some_and(|v| v != &json!(false)) {
        return Ok(Some("key_expansion"));
    }
    if set("note_keys").is_some() {
        return Ok(Some("note_keys"));
    }
    for key in ["doc_links", "symbol_links", "source_age"] {
        if set(key) == Some(&json!(true)) {
            return Ok(Some(key));
        }
    }
    if crate::notes_first::switch(args)?.0 {
        return Ok(Some("notes_first"));
    }
    if args["graph_retrieval"] == true {
        return Ok(Some("graph_retrieval"));
    }
    for key in ["local_embedding_profile", "local_reranker_profile"] {
        if set(key).is_some() {
            return Ok(Some(key));
        }
    }
    Ok(None)
}

/// The speaker roles `rank` keys its speaker arm on for this call, as
/// `retrieval::rank` resolves `speaker` (left out: the recipe's `auto`).
fn speaker_keep(args: &Value, query: &str) -> Option<&'static str> {
    match args.get("speaker").filter(|v| !v.is_null()) {
        Some(Value::Bool(false)) => None,
        Some(Value::Bool(true)) => Some("user"),
        _ => match retrieval::auto_speaker(query) {
            "none" => None,
            "assistant" => Some("assistant"),
            _ => Some("user"),
        },
    }
}

/// A passage `rank` scores: one of the index's (file, passage), or a row of
/// `extra` (a connector document's passage, or a memory).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum Key {
    Indexed(usize, u32),
    Extra(usize),
}

/// The lexical or speaker order of the catalogue, as `rank` computes it:
/// the first [`RANKED`] keys with their scores, and the number of rows the
/// arm read and scored.
struct Order {
    ranked: Vec<(Key, f64)>,
    rows: u64,
    scored: u64,
}

/// One arm's BM25 over the index and the extra rows: `terms` in their
/// order, `prefix` before each term in the index (the speaker arm's side),
/// each row's length (`length` of an index passage; `extra` rows'
/// features), and a title bonus per file and extra row.
#[allow(clippy::too_many_arguments)]
fn bm25(
    db: &Connection,
    index: &Index,
    terms: &BTreeSet<String>,
    prefix: &str,
    length: &dyn Fn(&passage_index::Passage) -> usize,
    extra: &[Option<std::sync::Arc<cache::Features>>],
    bonus: Option<(&[bool], &[bool])>,
    identity: &dyn Fn(Key) -> String,
) -> Result<Order> {
    // Rows: every index passage with a length (all of them for the words,
    // those with turns for a speaker side), and the extra rows with
    // features.
    let mut offsets = Vec::with_capacity(index.files.len() + 1);
    let mut total = 0usize;
    for file in &index.files {
        offsets.push(total);
        total += file.passages.len();
    }
    offsets.push(total);
    let mut rows = 0u64;
    let mut sum = 0usize;
    for file in &index.files {
        for p in &file.passages {
            let n = length(p);
            if n > 0 || prefix.is_empty() {
                rows += 1;
                sum += n;
            }
        }
    }
    for features in extra.iter().flatten() {
        rows += 1;
        sum += features.length;
    }
    let n = rows as f64;
    let average = (sum as f64 / (rows.max(1) as f64)).max(1.0);
    let mut scores = vec![0.0f64; total];
    let mut extra_scores = vec![0.0f64; extra.len()];
    for term in terms {
        let postings = index.postings(db, &format!("{prefix}{term}"))?;
        let held_extra = extra
            .iter()
            .flatten()
            .filter(|features| features.words.contains_key(term))
            .count();
        let frequency = (postings.len() + held_extra) as f64;
        let idf = (1.0 + (n - frequency + 0.5) / (frequency + 0.5)).ln();
        for (file, passage, count) in postings {
            let p = &index.files[file].passages[passage as usize];
            let tf = f64::from(count);
            scores[offsets[file] + passage as usize] +=
                idf * tf * 2.2 / (tf + 1.2 * (0.25 + 0.75 * length(p) as f64 / average));
        }
        for (i, features) in extra.iter().enumerate() {
            let Some(features) = features else {
                continue;
            };
            let tf = features.words.get(term) as f64;
            if tf > 0.0 {
                extra_scores[i] +=
                    idf * tf * 2.2 / (tf + 1.2 * (0.25 + 0.75 * features.length as f64 / average));
            }
        }
    }
    if let Some((files, rows_bonus)) = bonus {
        for (file, &named) in files.iter().enumerate() {
            if named {
                for score in &mut scores[offsets[file]..offsets[file + 1]] {
                    *score += 5.0;
                }
            }
        }
        for (i, &named) in rows_bonus.iter().enumerate() {
            if named {
                extra_scores[i] += 5.0;
            }
        }
    }
    let mut positive: Vec<(f64, Key)> = Vec::new();
    for (file, window) in offsets.windows(2).enumerate() {
        for (passage, &score) in scores[window[0]..window[1]].iter().enumerate() {
            if score > 0.0 {
                positive.push((score, Key::Indexed(file, passage as u32)));
            }
        }
    }
    for (i, &score) in extra_scores.iter().enumerate() {
        if score > 0.0 {
            positive.push((score, Key::Extra(i)));
        }
    }
    let scored = positive.len() as u64;
    // The first RANKED by score, ties by identity: everything above the
    // RANKED-th score, and every tie at it.
    if positive.len() > RANKED {
        let cut = RANKED - 1;
        positive.select_nth_unstable_by(cut, |a, b| b.0.total_cmp(&a.0));
        let threshold = positive[cut].0;
        positive.retain(|(score, _)| *score >= threshold);
    }
    let mut named: Vec<(f64, String, Key)> = positive
        .into_iter()
        .map(|(score, key)| (score, identity(key), key))
        .collect();
    named.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
    named.truncate(RANKED);
    Ok(Order {
        ranked: named
            .into_iter()
            .map(|(score, _, key)| (key, score))
            .collect(),
        rows,
        scored,
    })
}

/// The `file:` ids memories cite: the text their staleness checks may read.
fn memory_sources(db: &Connection, repository: &str) -> Result<BTreeSet<String>> {
    let mut wanted = BTreeSet::new();
    for row in crate::store::rows(
        db,
        "SELECT data FROM repo_memories WHERE repository_id=?",
        [repository],
    )? {
        let data: Value = serde_json::from_str(s(&row, "data")).unwrap_or(Value::Null);
        for field in ["dependencies", "evidence_links", "native_sources"] {
            for key in data[field].as_object().into_iter().flat_map(|m| m.keys()) {
                if field == "dependencies" {
                    wanted.insert(format!("file:{key}"));
                } else if key.starts_with("file:") {
                    wanted.insert(key.clone());
                }
            }
        }
        for path in data["symbol_files"]
            .as_object()
            .into_iter()
            .flat_map(|m| m.keys())
        {
            wanted.insert(format!("file:{path}"));
        }
    }
    Ok(wanted)
}

/// The project's documents with the text of `wanted` read into them (the
/// others keep an empty `content`), by id.
struct Documents {
    list: Vec<Value>,
    by_id: HashMap<String, usize>,
    loaded: BTreeSet<String>,
}

impl Documents {
    fn load(
        &mut self,
        db: &Connection,
        repository: &Value,
        wanted: &BTreeSet<String>,
    ) -> Result<()> {
        let missing: BTreeSet<String> = wanted
            .iter()
            .filter(|id| !self.loaded.contains(*id) && self.by_id.contains_key(*id))
            .cloned()
            .collect();
        if missing.is_empty() {
            return Ok(());
        }
        memory::load_contents(db, repository, &mut self.list, &missing)?;
        self.loaded.extend(missing);
        Ok(())
    }
    fn get(&self, id: &str) -> Option<&Value> {
        self.by_id.get(id).map(|&i| &self.list[i])
    }
}

/// The passage rows of `file` (an index file) at `passages`, as `chunks`
/// returns them, from its document with text.
fn file_rows(document: &Value, file: &passage_index::File, passages: &[u32]) -> Result<Vec<Value>> {
    let spans: Vec<cache::Span> = passages
        .iter()
        .filter_map(|&p| file.passages.get(p as usize))
        .map(|p| cache::Span {
            start: p.start,
            end: p.end,
            line: p.line,
            end_line: p.end_line,
        })
        .collect();
    graphicle_memory::ingest::document_rows(document, &spans).map_err(Error::from)
}

/// The packet `query::project_packet` builds for `args`, from the passage
/// index; `None` when the call or the project takes the full path.
pub fn packet(
    db: &Connection,
    p: &Principal,
    repository: &Value,
    args: &Value,
) -> Result<Option<(Snapshot, Value)>> {
    if ineligible(args)?.is_some() {
        return Ok(None);
    }
    let id = s(repository, "id");
    // A project with reviewed memory walks its graph over every passage.
    let reviewed: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM repo_memories WHERE repository_id=? AND state='reviewed')",
        [id],
        |r| r.get(0),
    )?;
    if reviewed && args["graph_retrieval"] != false && args["memory_retrieval"] != false {
        return Ok(None);
    }
    let heads = crate::store::heads(db, id)?;
    let Some((mut index, coverage)) =
        crate::phase::phase("context_index", || Index::open(db, id, &heads))?
    else {
        return Ok(None);
    };
    if coverage.missing.len() > MAX_MISSING {
        return Ok(None);
    }
    // Every document's metadata, the text of those read here.
    let mut list = memory::repository_documents(db, repository, false)?;
    let connectors = memory::connector_documents(db, repository, p)?;
    for document in &connectors {
        if graphicle_memory::text::sha256(s(document, "content").as_bytes())
            != s(document, "sha256")
        {
            return Err(Error::bad("Source integrity check failed"));
        }
    }
    let first_connector = list.len();
    list.extend(connectors);
    let by_id: HashMap<String, usize> = list
        .iter()
        .enumerate()
        .map(|(i, d)| (s(d, "id").to_string(), i))
        .collect();
    let mut docs = Documents {
        loaded: list[first_connector..]
            .iter()
            .map(|d| s(d, "id").to_string())
            .collect(),
        list,
        by_id,
    };
    // The files the index lacks: read and counted here.
    let missing: BTreeSet<String> = coverage
        .missing
        .iter()
        .map(|path| format!("file:{path}"))
        .collect();
    docs.load(db, repository, &missing)?;
    let mut added = Vec::new();
    let mut bytes = 0usize;
    for path in &coverage.missing {
        let Some(document) = docs.get(&format!("file:{path}")) else {
            continue;
        };
        bytes += s(document, "content").len();
        added.push((
            path.clone(),
            s(document, "sha256").to_string(),
            s(document, "content").to_string(),
        ));
    }
    if bytes > MAX_MISSING_BYTES {
        return Ok(None);
    }
    crate::phase::phase("context_index_missing", || index.add_files(added))?;
    docs.load(db, repository, &memory_sources(db, id)?)?;
    let memories = memory::list_of(db, repository, &docs.list, true)?;
    // The file relationships: the saved ones of the current episode
    // (crate::query_graph), else the snapshot's graph as `context` reads it.
    let current = crate::graph_projection::current_episode(db, id)?;
    let saved_edges = if crate::graph_projection::is_recorded(db, id, &current)? {
        crate::phase::phase("snapshot_file_edges", || {
            crate::query_graph::FileEdges::read(db, id, &current)
        })
    } else {
        None
    };
    let graph = match &saved_edges {
        Some(_) => json!({"edges":[],"graph_source":"episode","episode":current}),
        None => crate::phase::phase("snapshot_graph", || query::source_graph(db, repository, p))?,
    };
    let state = digest(
        &json!({"sources":memory::state(&docs.list),"memories":memories.iter().map(|m|json!([m["id"],m["revision"],m["state"],m["stale"]])).collect::<Vec<_>>(),"reviewed_state":memory::reviewed_state_for(db,repository,&memories)?}),
    );
    let mut memories = memories;
    if args["memory_retrieval"] == false {
        memories.clear();
    }
    let question = string(args, "question", 16000)?;
    let focus = crate::code_question::focus(args, question)?;
    let query_text = focus.text.clone();
    let query = query_text.as_str();
    // The code-graph arm's graph half; its citations need the text of the
    // files its nodes are in.
    let links = crate::doc_links::Options::parse(args)?;
    let code_first = crate::code_question::decide(args, question, query)?;
    let plan = if args["graph_retrieval"] == false {
        None
    } else {
        Some(crate::phase::phase("context_arm", || {
            code_graph_arm::plan(db, id, query, links.documents() && !code_first.applied)
        }))
    };
    let arm = match plan {
        None => code_graph_arm::Arm::skipped("disabled"),
        Some(Err(error)) => {
            let mut arm = code_graph_arm::Arm::skipped("error");
            arm.diagnostics["error"] = json!(error.message);
            arm
        }
        Some(Ok(plan)) => {
            let wanted: BTreeSet<String> = plan
                .files()
                .iter()
                .map(|path| format!("file:{path}"))
                .collect();
            docs.load(db, repository, &wanted)?;
            let by_id: HashMap<&str, &Value> = docs.list.iter().map(|d| (s(d, "id"), d)).collect();
            plan.anchor(db, id, &by_id).unwrap_or_else(|error| {
                let mut arm = code_graph_arm::Arm::skipped("error");
                arm.diagnostics["error"] = json!(error.message);
                arm
            })
        }
    };
    // The identifier arm over the index; names with separators are matched
    // in the text of the passages that hold their words.
    let identifiers = {
        let mut text = |files: &BTreeSet<usize>| -> Result<HashMap<(usize, u32), String>> {
            let wanted: BTreeSet<String> = files
                .iter()
                .map(|&f| format!("file:{}", index.files[f].path))
                .collect();
            docs.load(db, repository, &wanted)?;
            let mut texts = HashMap::new();
            for &f in files {
                let file = &index.files[f];
                let Some(document) = docs.get(&format!("file:{}", file.path)) else {
                    continue;
                };
                let content = s(document, "content");
                for (p, passage) in file.passages.iter().enumerate() {
                    if let Some(text) = content.get(passage.start..passage.end) {
                        texts.insert((f, p as u32), text.to_string());
                    }
                }
            }
            Ok(texts)
        };
        crate::phase::phase("context_identifiers", || {
            crate::identifier_arm::arm_indexed(db, id, &index, &mut text, args, question, query)
        })?
    };
    // The rows `rank` scores besides the index's: the connector documents'
    // passages, and the reviewed memories `context` reads (as `context_on`
    // filters them).
    let connector_snapshot = Snapshot {
        repository: repository.clone(),
        documents: docs.list[first_connector..].to_vec(),
        memories: Vec::new(),
        graph: Value::Null,
        state: String::new(),
    };
    let connector_rows = if connector_snapshot.documents.is_empty() {
        Vec::new()
    } else {
        query::passages(&connector_snapshot)?.rows
    };
    let mut memory_rows = Vec::new();
    if args["memory_retrieval"] != false {
        let at = graphicle_memory::temporal::instant(&crate::now()).map_err(Error::bad)?;
        let mut recalled = 0usize;
        for memory in &memories {
            if !crate::structured_memory::context_allows(memory, args, at)?
                || memory["stale"] == true
                || memory["state"] != "reviewed"
                || recalled >= graphicle_memory::retrieval::MAX_RECALL_MEMORIES
            {
                continue;
            }
            recalled += 1;
            memory_rows.push(json!({"identity":format!("memory:{}",s(memory,"id")),"title":memory["title"],"text":memory["body"]}));
        }
    }
    let extra: Vec<&Value> = connector_rows.iter().chain(&memory_rows).collect();
    // The lexical and speaker orders of the whole catalogue.
    let mut terms = retrieval::terms(query);
    if let Some(needs) = args.get("needs").and_then(Value::as_array) {
        for need in needs.iter().filter_map(Value::as_str) {
            terms.extend(retrieval::terms(need));
        }
    }
    let mut stemmer = Stemmer::default();
    let terms: BTreeSet<String> = terms.iter().map(|t| stemmer.stem(t)).collect();
    let scope = id;
    let features: Vec<Option<std::sync::Arc<cache::Features>>> = extra
        .iter()
        .map(|row| {
            let content = format!("{} {}", s(row, "title"), s(row, "text"));
            cache::stemmed_features(scope, &content, &mut stemmer)
                .map(Some)
                .map_err(Error::from)
        })
        .collect::<Result<_>>()?;
    let question_lowercase = query.to_lowercase();
    let named_title = |title: &str| {
        let title = title.to_lowercase();
        !title.is_empty() && question_lowercase.contains(&title)
    };
    let file_bonus: Vec<bool> = index.files.iter().map(|f| named_title(&f.path)).collect();
    let extra_bonus: Vec<bool> = extra
        .iter()
        .map(|row| named_title(s(row, "title")))
        .collect();
    let identity = |key: Key| match key {
        Key::Indexed(file, passage) => format!(
            "file:{}:{}",
            index.files[file].path, index.files[file].passages[passage as usize].start
        ),
        Key::Extra(i) => s(extra[i], "identity").to_string(),
    };
    let lexical = crate::phase::phase("context_lexical", || {
        bm25(
            db,
            &index,
            &terms,
            "",
            &|p| p.length,
            &features,
            Some((&file_bonus, &extra_bonus)),
            &identity,
        )
    })?;
    let side = speaker_keep(args, query);
    let speaker = match side {
        None => None,
        Some(side) => {
            let keep = BTreeSet::from([side.to_string()]);
            let spoken: Vec<Option<std::sync::Arc<cache::Features>>> = extra
                .iter()
                .map(|row| {
                    let spoken = retrieval::speaker_text(s(row, "text"), &keep);
                    let features = cache::stemmed_features(scope, &spoken, &mut stemmer)
                        .map_err(Error::from)?;
                    Ok((features.length > 0).then_some(features))
                })
                .collect::<Result<_>>()?;
            let (prefix, length): (&str, &dyn Fn(&passage_index::Passage) -> usize) =
                if side == "assistant" {
                    (passage_index::ASSISTANT, &|p| p.assistant)
                } else {
                    (passage_index::USER, &|p| p.user)
                };
            Some((
                side,
                crate::phase::phase("context_speaker", || {
                    bm25(db, &index, &terms, prefix, length, &spoken, None, &identity)
                })?,
            ))
        }
    };
    // The passages the arms rank: their rows, with the text of their files.
    let mut wanted: BTreeMap<usize, BTreeSet<u32>> = BTreeMap::new();
    let mut add = |key: &Key| {
        if let Key::Indexed(file, passage) = key {
            wanted.entry(*file).or_default().insert(*passage);
        }
    };
    for (key, _) in &lexical.ranked {
        add(key);
    }
    if let Some((_, order)) = &speaker {
        for (key, _) in &order.ranked {
            add(key);
        }
    }
    let locate = |identity: &str| -> Option<(usize, u32)> {
        let rest = identity.strip_prefix("file:")?;
        let (path, start) = rest.rsplit_once(':')?;
        let start: usize = start.parse().ok()?;
        let file = index.file(path)?;
        let passage = index.files[file]
            .passages
            .iter()
            .position(|p| p.start == start)?;
        Some((file, passage as u32))
    };
    for identity in identifiers.ranking.iter().flatten() {
        if let Some((file, passage)) = locate(identity) {
            wanted.entry(file).or_default().insert(passage);
        }
    }
    // The passages that cover the code-graph arm's citations.
    for hit in &arm.hits {
        let Some(file) = hit
            .document
            .strip_prefix("file:")
            .and_then(|path| index.file(path))
        else {
            continue;
        };
        for (p, passage) in index.files[file].passages.iter().enumerate() {
            if passage.start < hit.byte_end && passage.end > hit.byte_start {
                wanted.entry(file).or_default().insert(p as u32);
            }
        }
    }
    let ids: BTreeSet<String> = wanted
        .keys()
        .map(|&f| format!("file:{}", index.files[f].path))
        .collect();
    docs.load(db, repository, &ids)?;
    let order: HashMap<&str, usize> = docs
        .list
        .iter()
        .enumerate()
        .map(|(i, d)| (s(d, "id"), i))
        .collect();
    let mut rows: Vec<(usize, usize, Value)> = Vec::new();
    for (&file, passages) in &wanted {
        let entry = &index.files[file];
        let id = format!("file:{}", entry.path);
        let Some(document) = docs.get(&id) else {
            continue;
        };
        let list: Vec<u32> = passages.iter().copied().collect();
        for row in file_rows(document, entry, &list)? {
            let start = row["source"]["byte_start"].as_u64().unwrap_or(0) as usize;
            rows.push((
                order.get(id.as_str()).copied().unwrap_or(usize::MAX),
                start,
                row,
            ));
        }
    }
    for row in &connector_rows {
        let at = order
            .get(s(&row["source"], "id"))
            .copied()
            .unwrap_or(usize::MAX);
        let start = row["source"]["byte_start"].as_u64().unwrap_or(0) as usize;
        rows.push((at, start, row.clone()));
    }
    rows.sort_by_key(|row| (row.0, row.1));
    let rows: Vec<Value> = rows.into_iter().map(|(.., row)| row).collect();
    let totals = index.totals();
    let catalogue = totals.passages as usize + connector_rows.len();
    let report =
        json!({"passage_limit_reached":false,"omitted_documents":0,"omitted_documents_are":"none"});
    let shared = Passages::from_parts(rows, report);
    let mut snap = Snapshot {
        repository: repository.clone(),
        documents: docs.list.clone(),
        memories,
        graph,
        state,
    };
    // The lift's edges: the snapshot's file relationships, as `context_on`
    // filters them.
    let graph_enabled = args["graph_retrieval"] != false;
    let all_edges: Vec<&Value> = if graph_enabled {
        snap_edges(&snap.graph, links.documents())
    } else {
        Vec::new()
    };
    let documents = RefCell::new(docs);
    let index_ref = &index;
    // The passages of the files the lift reaches (by path, with the hop that
    // reaches each) that `rank` can keep: past MAX_FUSED of them, each is
    // scored as `rank` scores it ([`lift_choice`]) and the best MAX_FUSED are
    // sent. A passage after them has MAX_FUSED that `rank` orders before it
    // (with their own scores or better), so it would not be kept.
    let lift_rows =
        |reached: &BTreeMap<String, usize>, scoring: &LiftScoring| -> Result<Vec<Value>> {
            let files: Vec<(usize, usize)> = reached
                .iter()
                .filter_map(|(path, &hop)| index_ref.file(path).map(|file| (file, hop)))
                .collect();
            let chosen = lift_choice(db, index_ref, &files, scoring)?;
            let mut docs = documents.borrow_mut();
            let wanted: BTreeSet<String> = chosen
                .keys()
                .map(|&file| format!("file:{}", index_ref.files[file].path))
                .collect();
            docs.load(db, repository, &wanted)?;
            let mut rows = Vec::new();
            for path in reached.keys() {
                let Some(file) = index_ref.file(path) else {
                    continue;
                };
                let Some(passages) = chosen.get(&file) else {
                    continue;
                };
                let Some(document) = docs.get(&format!("file:{path}")) else {
                    continue;
                };
                rows.extend(file_rows(document, &index_ref.files[file], passages)?);
            }
            Ok(rows)
        };
    let documents_flag = links.documents();
    let mut lift = |starts: &[String], hops: usize, scoring: &LiftScoring| -> Result<Lift> {
        if let Some(saved) = saved_edges.as_ref().filter(|_| graph_enabled) {
            return lift_saved(saved, documents_flag, starts, hops, &|reached| {
                lift_rows(reached, scoring)
            });
        }
        let frontier: BTreeSet<&str> = starts.iter().map(String::as_str).collect();
        let neighbours = |from: &BTreeSet<&str>| -> BTreeSet<String> {
            let mut next = BTreeSet::new();
            for edge in &all_edges {
                let (a, b) = (s(edge, "source"), s(edge, "target"));
                if from.contains(a) {
                    next.insert(b.to_string());
                }
                if from.contains(b) {
                    next.insert(a.to_string());
                }
            }
            next
        };
        // The pairs the lift reads: those touching the frontier and, with
        // two hops, the files the first hop reaches.
        let first = if hops > 1 {
            neighbours(&frontier)
        } else {
            BTreeSet::new()
        };
        let mut near: BTreeSet<&str> = frontier.clone();
        near.extend(first.iter().map(String::as_str));
        let used: Vec<Value> = all_edges
            .iter()
            .filter(|edge| near.contains(s(edge, "source")) || near.contains(s(edge, "target")))
            .map(|edge| (*edge).clone())
            .collect();
        let rows = if used.is_empty() {
            Vec::new()
        } else {
            lift_rows(&reach(&used, starts, hops), scoring)?
        };
        Ok(Lift {
            rows,
            static_edges: static_edges_report(&all_edges, &used),
            edges: Value::Array(used),
            edge_count: all_edges.len(),
        })
    };
    let mut contents = |ids: &BTreeSet<String>| -> Result<Vec<Value>> {
        let mut docs = documents.borrow_mut();
        docs.load(db, repository, ids)?;
        Ok(ids.iter().filter_map(|id| docs.get(id).cloned()).collect())
    };
    // The files `chunks` gives passages, as the code-first report counts them.
    let origins: Vec<Value> = index
        .files
        .iter()
        .filter(|f| !f.passages.is_empty())
        .map(|f| json!({"origin": format!("file:{}", f.path)}))
        .collect();
    let lexical_ranked: Vec<Value> = lexical
        .ranked
        .iter()
        .map(|(key, score)| json!([identity(*key), score]))
        .collect();
    let speaker_ranked = speaker.as_ref().map(|(side, order)| {
        json!({"keep":[side],"ranking":order.ranked.iter().map(|(key, _)| identity(*key)).collect::<Vec<_>>(),
            "rows":order.rows,"ranked":order.scored})
    });
    let mut indexed = Indexed {
        lexical: json!(lexical_ranked),
        speaker: speaker_ranked,
        coverage: json!({"source_passages":catalogue,"passage_limit_reached":false,
            "omitted_documents":0,"omitted_documents_are":"none"}),
        origins,
        lift: &mut lift,
        contents: &mut contents,
    };
    let mut rankings = json!({});
    if let Some(ranking) = identifiers.ranking {
        rankings[crate::identifier_arm::ARM] = json!(ranking);
    }
    if !identifiers.defining.is_empty() {
        rankings[crate::code_question::DEFINING] = json!(identifiers.defining);
    }
    let lookup =
        crate::doc_links::Lookup::new(db, id).with_folder(crate::folders::current_folder(db, id)?);
    let mut packet = crate::phase::phase("context_assemble", || {
        query::context_indexed(
            &snap,
            args,
            rankings,
            Some(&arm),
            shared,
            Some(&lookup),
            &mut indexed,
        )
    })?;
    packet["local_dense"] = json!({"status":"disabled"});
    packet["identifier_arm"] = identifiers.report;
    packet["freshness"] = json!(crate::invalidation::repository_freshness(
        db,
        &p.workspace,
        id
    )?);
    packet["passage_index"] = json!({"status":"ready","files":index.files.len(),
        "passages":totals.passages,"read_here":coverage.missing.len()});
    drop(indexed);
    // The snapshot holds the text of every file the packet read, its sources
    // among them (the workspace scope cites them from it), as the full
    // path's holds every file's.
    snap.documents = documents.into_inner().list;
    Ok(Some((snap, packet)))
}

/// Whether a question to `repository` takes the passage index's path, and
/// fast: its index is built and current enough ([`MAX_MISSING`]), it has no
/// reviewed memory, and the code graph `context` reads is saved
/// ([`crate::query_graph`]). What the prompt hook and `doctor` check.
pub fn ready(db: &Connection, repository: &str) -> Result<bool> {
    let heads = crate::store::heads(db, repository)?;
    let coverage = passage_index::coverage(db, repository, &heads)?;
    if !coverage.exists() || coverage.missing.len() > MAX_MISSING {
        return Ok(false);
    }
    let reviewed: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM repo_memories WHERE repository_id=? AND state='reviewed')",
        [repository],
        |r| r.get(0),
    )?;
    if reviewed {
        return Ok(false);
    }
    let latest = crate::store::one(
        db,
        "SELECT episode FROM gf_episodes WHERE repository_id=? ORDER BY sequence DESC LIMIT 1",
        [repository],
    )?;
    Ok(match latest {
        None => true,
        Some(row) => crate::query_graph::saved(db, repository, s(&row, "episode")),
    })
}

/// A project with fewer files than this answers fast from the full path:
/// `doctor` and `setup` say nothing of its passage index (Info), and the
/// prompt hook asks it in full.
pub const SMALL_PROJECT_FILES: usize = 2_000;

/// How the prompt hook asks a project ([`hook_route`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HookRoute {
    /// `graphicle context` as it is: the passage index is ready, or the
    /// project is small enough for the full path.
    Full,
    /// `graphicle context` as it is, for a project with reviewed memory:
    /// only the full path reads its memories, and a sync does not make that
    /// faster.
    Memory,
    /// The quick packet ([`graph_only_packet`]) while a large project's
    /// passage index or saved code graph is not ready; a sync fixes it.
    Quick,
}

/// [`HookRoute`] of `repository`: a project of fewer than
/// [`SMALL_PROJECT_FILES`] files is asked in full, a project with reviewed
/// memory in full ([`HookRoute::Memory`]), and a larger one in full when it
/// is [`ready`], else for the quick packet.
pub fn hook_route(db: &Connection, repository: &str) -> Result<HookRoute> {
    let files: i64 = db.query_row(
        "SELECT count(*) FROM repo_heads WHERE repository_id=?",
        [repository],
        |r| r.get(0),
    )?;
    if (files as usize) < SMALL_PROJECT_FILES {
        return Ok(HookRoute::Full);
    }
    let reviewed: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM repo_memories WHERE repository_id=? AND state='reviewed')",
        [repository],
        |r| r.get(0),
    )?;
    if reviewed {
        return Ok(HookRoute::Memory);
    }
    Ok(if ready(db, repository)? {
        HookRoute::Full
    } else {
        HookRoute::Quick
    })
}

/// What `doctor` and `setup` say about how fast questions to `repository`
/// are: `state` (`ready`, `small`, `missing`, `stale`, `memory` or
/// `graph`), a `detail` line and the command that `fix`es it. A project
/// whose index is missing or stale reads every file for each question, and
/// the prompt hook then gives the quick packet only.
pub fn readiness(db: &Connection, repository: &str) -> Result<Value> {
    let heads = crate::store::heads(db, repository)?;
    let coverage = passage_index::coverage(db, repository, &heads)?;
    let files = heads.len();
    let reviewed: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM repo_memories WHERE repository_id=? AND state='reviewed')",
        [repository],
        |r| r.get(0),
    )?;
    let latest = crate::store::one(
        db,
        "SELECT episode FROM gf_episodes WHERE repository_id=? ORDER BY sequence DESC LIMIT 1",
        [repository],
    )?;
    let graph_saved = latest
        .as_ref()
        .is_none_or(|row| crate::query_graph::saved(db, repository, s(row, "episode")));
    let small = files < SMALL_PROJECT_FILES;
    let (state, detail, fix) = if !coverage.exists() {
        (
            if small { "small" } else { "missing" },
            format!(
                "the passage index of this project ({files} files) is not built: each question reads every file{}",
                if small {
                    ""
                } else {
                    ", too slow for the prompt hook's budget, which then gives the files that define the names a prompt gives"
                }
            ),
            "graphicle sync",
        )
    } else if coverage.missing.len() > MAX_MISSING {
        (
            "stale",
            format!(
                "{} files changed since the passage index was built: each question reads every file until the next sync",
                coverage.missing.len()
            ),
            "graphicle sync",
        )
    } else if reviewed {
        (
            if small { "small" } else { "memory" },
            "this project has reviewed memory: questions take the full path, which reads every file".to_string(),
            "",
        )
    } else if !graph_saved {
        (
            if small { "small" } else { "graph" },
            "the code graph is not saved for questions yet: each question reads it from the store"
                .to_string(),
            "graphicle sync",
        )
    } else {
        (
            "ready",
            format!(
                "the passage index covers {} of {files} files and the code graph is saved",
                coverage.indexed
            ),
            "",
        )
    };
    Ok(
        json!({"state":state,"detail":detail,"fix":fix,"files":files,
        "indexed":coverage.indexed,"missing":coverage.missing.len(),"graph_saved":graph_saved}),
    )
}

/// Most files the quick packet ([`graph_only_packet`]) reads.
pub const GRAPH_ONLY_FILES: usize = 40;

/// The quick packet the prompt hook asks for while a project's passage index
/// is not ready (`graph_only`): the files that define the code names the
/// question gives (the code graph's symbols, `gf_symbol_heads`, as the
/// identifier route finds definitions) and the files it names by path, at
/// most [`GRAPH_ONLY_FILES`], ranked over their own passages with the
/// identifier arm and without the code-graph arm, so no file of the project
/// is read but those.
pub fn graph_only_packet(
    db: &Connection,
    p: &Principal,
    repository: &Value,
    args: &Value,
) -> Result<(Snapshot, Value)> {
    let id = s(repository, "id");
    let question = string(args, "question", 16000)?;
    let focus = crate::code_question::focus(args, question)?;
    let query = focus.text.clone();
    let named = crate::identifier_arm::named(&query);
    let heads = crate::store::heads(db, id)?;
    // Definitions: dotted symbol names that end with a name's segments.
    let mut files: Vec<String> = Vec::new();
    if !named.names.is_empty() {
        let symbols: Vec<(String, String)> = crate::store::rows(
            db,
            "SELECT relpath,qualified_name FROM gf_symbol_heads WHERE repository_id=?",
            [id],
        )?
        .iter()
        .map(|row| {
            (
                s(row, "relpath").to_string(),
                s(row, "qualified_name")
                    .replace("::", ".")
                    .replace(['#'], ".")
                    .replace("->", "."),
            )
        })
        .collect();
        for name in &named.names {
            let dotted = name
                .replace("::", ".")
                .replace(['#'], ".")
                .replace("->", ".");
            let segments: Vec<&str> = dotted.split('.').filter(|s| !s.is_empty()).collect();
            let Some(last) = segments.last() else {
                continue;
            };
            let suffix = segments.join(".");
            let mut found: Vec<&str> = symbols
                .iter()
                .filter(|(_, qualified)| {
                    qualified == &suffix
                        || qualified
                            .strip_suffix(suffix.as_str())
                            .is_some_and(|head| head.ends_with('.'))
                        || (segments.len() == 1
                            && (qualified == last
                                || qualified
                                    .strip_suffix(last)
                                    .is_some_and(|head| head.ends_with('.'))))
                })
                .map(|(path, _)| path.as_str())
                .collect();
            found.sort_unstable();
            found.dedup();
            // A name defined in many files says little about where to look.
            if found.len() <= crate::identifier_arm::MAX_DEFINITIONS {
                files.extend(found.into_iter().map(str::to_string));
            }
        }
    }
    for path in &named.paths {
        let path = path.replace('\\', "/");
        files.extend(
            heads
                .keys()
                .filter(|file| {
                    *file == &path
                        || file.ends_with(&format!("/{path}"))
                        || path.ends_with(&format!("/{file}"))
                })
                .cloned(),
        );
    }
    let mut seen = BTreeSet::new();
    files.retain(|file| heads.contains_key(file) && seen.insert(file.clone()));
    files.truncate(GRAPH_ONLY_FILES);
    let mut list = memory::repository_documents(db, repository, false)?;
    let wanted: BTreeSet<String> = files.iter().map(|path| format!("file:{path}")).collect();
    memory::load_contents(db, repository, &mut list, &wanted)?;
    let read: Vec<Value> = list
        .iter()
        .filter(|d| wanted.contains(s(d, "id")))
        .cloned()
        .collect();
    let state = digest(
        &json!({"sources":memory::state(&list),"memories":Vec::<Value>::new(),"reviewed_state":memory::reviewed_state_for(db,repository,&[])?}),
    );
    let passages = query::passages(&Snapshot {
        repository: repository.clone(),
        documents: read,
        memories: Vec::new(),
        graph: Value::Null,
        state: String::new(),
    })?;
    let snap = Snapshot {
        repository: repository.clone(),
        documents: list,
        memories: Vec::new(),
        graph: json!({"edges":[],"graph_source":"none"}),
        state,
    };
    let mut quick = args.clone();
    quick["graph_retrieval"] = json!(false);
    quick["memory_retrieval"] = json!(false);
    let identifiers = crate::identifier_arm::arm(db, id, &passages, &quick, question, &query)?;
    let mut rankings = json!({});
    if let Some(ranking) = identifiers.ranking {
        rankings[crate::identifier_arm::ARM] = json!(ranking);
    }
    if !identifiers.defining.is_empty() {
        rankings[crate::code_question::DEFINING] = json!(identifiers.defining);
    }
    let lookup =
        crate::doc_links::Lookup::new(db, id).with_folder(crate::folders::current_folder(db, id)?);
    let mut packet = query::context_on(
        &snap,
        &quick,
        rankings,
        None,
        None,
        Some(passages),
        Some(&lookup),
    )?;
    packet["identifier_arm"] = identifiers.report;
    packet["passage_index"] =
        json!({"status":"not_ready","graph_only":true,"files_read":files.len()});
    packet["local_dense"] = json!({"status":"disabled"});
    let _ = p;
    Ok((snap, packet))
}

/// The lift over the saved file relationships ([`crate::query_graph`]):
/// the pairs touching the frontier (and, with two hops, the files the first
/// hop reaches), the passages of the files they reach, and `rank`'s report
/// for the whole list.
fn lift_saved(
    saved: &crate::query_graph::FileEdges,
    documents: bool,
    starts: &[String],
    hops: usize,
    rows: &LiftRows,
) -> Result<Lift> {
    let keep = |edge: &Value| documents || !crate::doc_links::document_edge(edge);
    let frontier: BTreeSet<&str> = starts.iter().map(String::as_str).collect();
    let touching = |files: &BTreeSet<&str>| -> Vec<Value> {
        saved
            .touching(files)
            .into_iter()
            .filter(|e| keep(e))
            .collect()
    };
    let ends = |edges: &[Value], files: &BTreeSet<&str>| -> BTreeSet<String> {
        let mut found = BTreeSet::new();
        for edge in edges {
            let (a, b) = (s(edge, "source"), s(edge, "target"));
            if files.contains(a) {
                found.insert(b.to_string());
            }
            if files.contains(b) {
                found.insert(a.to_string());
            }
        }
        found
    };
    let first_edges = touching(&frontier);
    let used = if hops > 1 {
        let first = ends(&first_edges, &frontier);
        let mut near: BTreeSet<&str> = frontier.clone();
        near.extend(first.iter().map(String::as_str));
        touching(&near)
    } else {
        first_edges
    };
    let lifted = if used.is_empty() {
        Vec::new()
    } else {
        rows(&reach(&used, starts, hops))?
    };
    // The whole list as `rank` would read it: `episode_graph`'s edges (less
    // the documents' when they are left out, counted from the pairs).
    let (distinct, listed) = if documents {
        (saved.pairs.len(), saved.listed)
    } else {
        let mut distinct = 0usize;
        let mut listed = 0usize;
        for ((a, b, k), lines) in saved.pairs.iter().zip(&saved.lines) {
            if keep(
                &json!({"source":saved.paths[*a as usize],"target":saved.paths[*b as usize],
                "kind":saved.kinds[*k as usize]}),
            ) {
                distinct += 1;
                listed += *lines as usize;
            }
        }
        (distinct, listed)
    };
    let bound = graphicle_memory::retrieval::MAX_STATIC_EDGES;
    let static_edges = if listed <= bound {
        Value::Null
    } else {
        json!({"received":listed,"distinct":distinct,"used":distinct.min(bound),
            "bound":bound,"aggregated":true,"capped":distinct > bound,
            "lift_complete":used.len() <= bound})
    };
    Ok(Lift {
        rows: lifted,
        static_edges,
        edges: Value::Array(used),
        edge_count: listed,
    })
}

/// The rows of the passages `rank` can keep of the files the lift reaches
/// (by path, with the hop that reaches each).
type LiftRows<'a> = dyn Fn(&BTreeMap<String, usize>) -> Result<Vec<Value>> + 'a;

/// The files `rank`'s relationship lift reaches from the `frontier` files
/// over `used`, bounded as `rank` bounds them, with the hop that first
/// reaches each: `rank`'s own walk.
fn reach(used: &[Value], frontier: &[String], hops: usize) -> BTreeMap<String, usize> {
    let start: BTreeSet<String> = frontier.iter().cloned().collect();
    let (bounded, _) = retrieval::bound_static_edges(used, &start, hops);
    let edges = bounded.as_deref().unwrap_or(used);
    let mut visited = start.clone();
    let mut current = start;
    let mut reached = BTreeMap::new();
    for hop in 1..=hops {
        let mut next = BTreeSet::new();
        for edge in edges {
            let (a, b) = (s(edge, "source"), s(edge, "target"));
            if current.contains(a) && !visited.contains(b) {
                next.insert(b.to_string());
            }
            if current.contains(b) && !visited.contains(a) {
                next.insert(a.to_string());
            }
        }
        for path in &next {
            reached.insert(path.clone(), hop);
        }
        visited.extend(next.iter().cloned());
        current = next;
    }
    reached
}

/// The passages, by index file, of `files` (with the hop that reaches each)
/// that `rank`'s relationship lift can keep: all of them when they are at
/// most [`retrieval::MAX_FUSED`], else the first MAX_FUSED by the score
/// `rank` gives them (`top` times 0.45 plus 0.15 times the share of the
/// question's terms the passage holds, over the hop, times the origin's
/// prior), ties by identity.
fn lift_choice(
    db: &Connection,
    index: &Index,
    files: &[(usize, usize)],
    scoring: &LiftScoring,
) -> Result<BTreeMap<usize, Vec<u32>>> {
    let total: usize = files
        .iter()
        .map(|&(file, _)| index.files[file].passages.len())
        .sum();
    let mut chosen: BTreeMap<usize, Vec<u32>> = BTreeMap::new();
    if total <= retrieval::MAX_FUSED {
        for &(file, _) in files {
            chosen.insert(file, (0..index.files[file].passages.len() as u32).collect());
        }
        return Ok(chosen);
    }
    let wanted: HashSet<usize> = files.iter().map(|&(file, _)| file).collect();
    let mut held: HashMap<(usize, u32), usize> = HashMap::new();
    for term in &scoring.terms {
        for (file, passage, _) in index.postings(db, term)? {
            if wanted.contains(&file) {
                *held.entry((file, passage)).or_default() += 1;
            }
        }
    }
    let terms = scoring.terms.len().max(1) as f64;
    let mut scored: Vec<(f64, String, usize, u32)> = Vec::with_capacity(total);
    for &(file, hop) in files {
        let entry = &index.files[file];
        let prior = (scoring.prior)(&format!("file:{}", entry.path));
        for (p, passage) in entry.passages.iter().enumerate() {
            let p = p as u32;
            // `rank`'s expression, term for term, so the scores are equal.
            let overlap = held.get(&(file, p)).copied().unwrap_or(0) as f64 / terms;
            let score = scoring.top * (0.45 + 0.15 * overlap) / (hop as f64) * prior;
            scored.push((
                score,
                format!("file:{}:{}", entry.path, passage.start),
                file,
                p,
            ));
        }
    }
    scored.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
    scored.truncate(retrieval::MAX_FUSED);
    for (_, _, file, passage) in scored {
        chosen.entry(file).or_default().push(passage);
    }
    for passages in chosen.values_mut() {
        passages.sort_unstable();
    }
    Ok(chosen)
}

/// The snapshot graph's file relationship edges, as `context_on` reads
/// them: without the edges from documents when `documents` is false.
fn snap_edges(graph: &Value, documents: bool) -> Vec<&Value> {
    graph
        .get("edges")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|edge| documents || !crate::doc_links::document_edge(edge))
        .collect()
}

/// `rank`'s `static_edges` report for the whole edge list (`None` within
/// its bound), when `used` holds every pair the lift reads.
fn static_edges_report(all: &[&Value], used: &[Value]) -> Value {
    let bound = graphicle_memory::retrieval::MAX_STATIC_EDGES;
    if all.len() <= bound {
        return Value::Null;
    }
    let distinct: BTreeSet<(&str, &str, &str)> = all
        .iter()
        .map(|edge| (s(edge, "source"), s(edge, "target"), s(edge, "kind")))
        .collect();
    let near: BTreeSet<(&str, &str, &str)> = used
        .iter()
        .map(|edge| (s(edge, "source"), s(edge, "target"), s(edge, "kind")))
        .collect();
    json!({"received":all.len(),"distinct":distinct.len(),"used":distinct.len().min(bound),
        "bound":bound,"aggregated":true,"capped":distinct.len() > bound,
        "lift_complete":near.len() <= bound})
}
