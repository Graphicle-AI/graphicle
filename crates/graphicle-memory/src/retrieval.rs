//! Graphiti RRF adaptation; Graphicle BM25 and evidence-set scheduling. See
//! NOTICE.md for upstream provenance and changes.
//!
//! Portions adapted from Graphiti (<https://github.com/getzep/graphiti>), Copyright 2024, Zep Software, Inc., Apache-2.0; changes are described in NOTICE.md.
use crate::{Result, cache, text};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

/// Most reviewed memories one recall ranks; the most recently updated are kept.
pub const MAX_RECALL_MEMORIES: usize = 20_000;
/// Most rows [`rank`] accepts: a full passage catalogue plus the recall
/// memories, so no in-bounds store fails recall.
pub const MAX_ROWS: usize = crate::ingest::MAX_PASSAGES + MAX_RECALL_MEMORIES;
/// Most sources one [`rank`] call selects (a reranker's candidate pool).
pub const MAX_RANK_LIMIT: usize = 60;

/// Counting and listing questions need every instance, not the best one.
fn aggregate_question(lowercase: &str) -> bool {
    static RX: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RX.get_or_init(|| {
        regex::Regex::new(
            r"\b(how many|how much|in total|total|altogether|combined|list|all the|every)\b",
        )
        .unwrap()
    })
    .is_match(lowercase)
}

/// Labels that open a turn in a `role: text` transcript.
pub(crate) const SPEAKER_ROLES: &[&str] = &[
    "user",
    "assistant",
    "system",
    "tool",
    "human",
    "ai",
    "bot",
    "model",
    "function",
    "developer",
];

/// Which side of a chat a question asks about, for `"speaker": "auto"`:
/// `assistant` when it names something the assistant said ("what did you
/// recommend", "you told me", "your last answer"), `none` when it only
/// points at an earlier conversation ("our previous chat", "we talked
/// about") and `user` otherwise, since a user's facts live in their own
/// turns. Keying every question on user turns costs the questions about
/// what the assistant said.
pub fn auto_speaker(question: &str) -> &'static str {
    static RX: std::sync::OnceLock<[regex::Regex; 2]> = std::sync::OnceLock::new();
    let [assistant, shared] = RX.get_or_init(|| {
        [
            regex::Regex::new(
                r"(?i)\byou(?:['’](?:d|ve))?(?:\s+\w+){0,3}?\s+(?:said|told|mentioned|recommended|suggested|gave|provided|listed|shared|wrote|explained|described|came up with|named|offered|showed|taught|helped|created|made|generated|proposed|put together|compiled|included)\b|\bdid\s+you\s+(?:say|tell|mention|recommend|suggest|give|provide|list|share|write|explain|describe|name|offer|show|propose|include)\b|\byour\s+(?:previous|earlier|last)\s+(?:answer|response|reply|suggestion|recommendation|advice|list)\b",
            )
            .unwrap(),
            regex::Regex::new(
                r"(?i)\b(?:our|the|that)\s+(?:previous|earlier|last|past)\s+(?:chat|conversation|discussion)s?\b|\bwe\s+(?:talked|discussed|chatted|spoke|went over|covered)\b",
            )
            .unwrap(),
        ]
    });
    if assistant.is_match(question) {
        "assistant"
    } else if shared.is_match(question) {
        "none"
    } else {
        "user"
    }
}

/// The `speaker` option: `true` keys the speaker arm on user turns,
/// `{"keep": [roles]}` on the turns of up to 8 roles and `"auto"` on the
/// side `auto_speaker` reads from the question; absent or `false` adds no
/// arm.
fn speaker_roles(value: &Value, question: &str) -> Result<Option<BTreeSet<String>>> {
    match value {
        Value::Null | Value::Bool(false) => Ok(None),
        Value::Bool(true) => Ok(Some(BTreeSet::from(["user".to_owned()]))),
        Value::String(mode) if mode == "auto" => Ok(match auto_speaker(question) {
            "none" => None,
            side => Some(BTreeSet::from([side.to_owned()])),
        }),
        Value::Object(object) => {
            if object.keys().any(|k| k != "keep") {
                return Err("speaker accepts only keep".into());
            }
            let keep = object
                .get("keep")
                .and_then(Value::as_array)
                .filter(|roles| (1..=8).contains(&roles.len()))
                .ok_or("speaker.keep must list 1 to 8 roles")?;
            let mut roles = BTreeSet::new();
            for role in keep {
                let role = role
                    .as_str()
                    .map(|r| r.trim().to_lowercase())
                    .filter(|r| !r.is_empty() && r.len() <= 40 && !r.contains([':', '\n', '\r']))
                    .ok_or("speaker roles must be short labels without colons")?;
                roles.insert(role);
            }
            Ok(Some(roles))
        }
        _ => Err("speaker must be a boolean, \"auto\" or {\"keep\": [roles]}".into()),
    }
}

/// The words of the turns spoken by `keep` in a `role: text` transcript.
/// LongMemEval's reference retrievers (xiaowu0162/LongMemEval@9e0b455f:
/// `process_item_flat_index` in `src/retrieval/run_retrieval.py`; MIT) key
/// sessions and turns on the user's side only; long assistant replies dilute
/// the user's own facts. A turn runs from its `label:` line to the next one,
/// where a label is a kept role, a standard chat role or, when a kept role
/// is a person's name, any name of up to three capitalized words. Text before
/// the first turn is not attributed.
pub fn speaker_text(text: &str, keep: &BTreeSet<String>) -> String {
    let named = keep.iter().any(|r| !SPEAKER_ROLES.contains(&r.as_str()));
    let is_name = |label: &str| {
        let words: Vec<&str> = label.split_whitespace().collect();
        (1..=3).contains(&words.len())
            && words.iter().all(|w| {
                w.chars().next().is_some_and(char::is_uppercase)
                    && w.chars().all(|c| c.is_alphabetic() || "'.-".contains(c))
            })
    };
    let mut spoken = String::new();
    let mut keeping = false;
    for line in text.lines() {
        let mut content = line;
        if let Some((raw, rest)) = line.split_once(':') {
            let label = raw.trim().to_lowercase();
            if keep.contains(&label)
                || SPEAKER_ROLES.contains(&label.as_str())
                || (named && is_name(raw.trim()))
            {
                keeping = keep.contains(&label);
                content = rest;
            }
        }
        if keeping {
            spoken.push_str(content);
            spoken.push('\n');
        }
    }
    spoken
}

/// Most caller `abstractions` per row, and bytes in each (read by
/// `key_expansion: "merge"`).
const MAX_ABSTRACTIONS: usize = 32;
const MAX_ABSTRACTION_BYTES: usize = 1000;

/// A row's caller `abstractions`, blank ones skipped: at most 32 strings of
/// at most 1,000 bytes each.
fn abstractions(row: &Value) -> Result<Vec<&str>> {
    let Some(value) = row.get("abstractions").filter(|v| !v.is_null()) else {
        return Ok(Vec::new());
    };
    let items = value
        .as_array()
        .filter(|items| items.len() <= MAX_ABSTRACTIONS)
        .ok_or("abstractions must be an array of at most 32 strings")?;
    let mut given = Vec::with_capacity(items.len());
    for item in items {
        let unit = item
            .as_str()
            .filter(|unit| unit.len() <= MAX_ABSTRACTION_BYTES)
            .ok_or("Each abstraction must be a string of at most 1000 bytes")?;
        if !unit.trim().is_empty() {
            given.push(unit);
        }
    }
    Ok(given)
}

pub fn rrf(rankings: &[Vec<String>], constant: f64, min_score: f64) -> Result<Vec<(String, f64)>> {
    rrf_weighted(rankings, &vec![1.0; rankings.len()], constant, min_score)
}
/// RRF with a weight per ranking: Σ_r w_r / (k + rank_r) (Memora, memind).
/// A zero weight disables an arm, including its candidate membership.
pub fn rrf_weighted(
    rankings: &[Vec<String>],
    weights: &[f64],
    constant: f64,
    min_score: f64,
) -> Result<Vec<(String, f64)>> {
    if !constant.is_finite()
        || constant <= 0.0
        || !min_score.is_finite()
        || weights.len() != rankings.len()
        || weights.iter().any(|w| !w.is_finite() || *w < 0.0)
    {
        return Err("Invalid RRF parameters".into());
    }
    let mut scores = std::collections::HashMap::<&str, f64>::new();
    for (ranking, weight) in rankings.iter().zip(weights) {
        if *weight == 0.0 {
            continue;
        }
        for (i, id) in ranking.iter().enumerate() {
            *scores.entry(id.as_str()).or_default() += weight / (i as f64 + constant);
        }
    }
    let mut result: Vec<_> = scores
        .into_iter()
        .filter(|(_, v)| *v >= min_score)
        .map(|(id, score)| (id.to_owned(), score))
        .collect();
    result.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    Ok(result)
}
/// Most `query_variants` one [`rank`] call accepts.
pub const MAX_QUERY_VARIANTS: usize = 8;
/// Longest query variant text, in bytes.
pub const MAX_QUERY_VARIANT_BYTES: usize = 4000;
/// Variant kinds: `query` (a rephrased question), `keyword` (a short keyword
/// query) and `passage` (a hypothetical answer passage, HyDE / query2doc).
pub const QUERY_VARIANT_KINDS: [&str; 3] = ["query", "keyword", "passage"];

/// One caller query variant, fused by [`rank`] as one more lexical arm. Onyx
/// multi-query search fuses an LLM rephrase, keyword queries and the original
/// question by weighted RRF; RAG-Fusion does the same with plain rephrasings,
/// and HyDE / query2doc search with a hypothetical answer passage.
#[derive(Clone, Debug, PartialEq)]
pub struct QueryVariant {
    pub text: String,
    pub weight: f64,
    pub kind: &'static str,
}

/// Parses `query_variants`: an array of `{text, weight?, kind?}` (weight 0–4,
/// default 1; kind default `query`). Duplicates of one kind, compared
/// case-insensitively, merge and add their weights (Onyx
/// `deduplicate_queries`), capped at 4.
pub fn query_variants(value: &Value) -> Result<Vec<QueryVariant>> {
    let items = match value {
        Value::Null => return Ok(Vec::new()),
        Value::Array(items) => items,
        _ => return Err("query_variants must be an array".into()),
    };
    if items.len() > MAX_QUERY_VARIANTS {
        return Err(format!("At most {MAX_QUERY_VARIANTS} query variants").into());
    }
    let mut variants: Vec<QueryVariant> = Vec::new();
    for item in items {
        let fields = item.as_object().ok_or("Query variants must be objects")?;
        if fields
            .keys()
            .any(|key| !["text", "weight", "kind"].contains(&key.as_str()))
        {
            return Err("Query variant fields are text, weight and kind".into());
        }
        let text = fields
            .get("text")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty() && text.len() <= MAX_QUERY_VARIANT_BYTES)
            .ok_or("Query variant text must be 1 to 4000 bytes")?;
        let weight = match fields.get("weight") {
            None | Some(Value::Null) => 1.0,
            Some(value) => value
                .as_f64()
                .filter(|weight| (0.0..=4.0).contains(weight))
                .ok_or("Query variant weights must be between 0 and 4")?,
        };
        let kind = match fields.get("kind") {
            None | Some(Value::Null) => "query",
            Some(value) => value
                .as_str()
                .and_then(|kind| QUERY_VARIANT_KINDS.into_iter().find(|k| *k == kind))
                .ok_or("Query variant kind must be query, keyword or passage")?,
        };
        let folded = text.to_lowercase();
        if let Some(seen) = variants
            .iter_mut()
            .find(|seen| seen.kind == kind && seen.text.to_lowercase() == folded)
        {
            seen.weight = (seen.weight + weight).min(4.0);
        } else {
            variants.push(QueryVariant {
                text: text.to_owned(),
                weight,
                kind,
            });
        }
    }
    Ok(variants)
}
/// BM25 (k1 1.2, b 0.75, Lucene idf) of every row for `bag`, best first, ties
/// by identity; rows sharing no term are left out.
fn bm25_rank(
    rows: &[Value],
    features: &[std::sync::Arc<cache::Features>],
    bag: &BTreeSet<String>,
    average: f64,
) -> Vec<(String, f64)> {
    let n = features.len() as f64;
    let weights: Vec<(&String, f64)> = bag
        .iter()
        .map(|term| {
            let df = features
                .iter()
                .filter(|f| f.words.contains_key(term))
                .count() as f64;
            (term, (1.0 + (n - df + 0.5) / (df + 0.5)).ln())
        })
        .collect();
    let mut ranked: Vec<(String, f64)> = features
        .iter()
        .enumerate()
        .filter_map(|(i, feature)| {
            let mut score = 0.0;
            for &(term, idf) in &weights {
                let tf = feature.words.get(term) as f64;
                if tf > 0.0 {
                    score += idf * tf * 2.2
                        / (tf + 1.2 * (0.25 + 0.75 * feature.length as f64 / average));
                }
            }
            (score > 0.0).then(|| (rows[i]["identity"].as_str().unwrap_or("").to_owned(), score))
        })
        .collect();
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    ranked
}
fn terms(value: &str) -> BTreeSet<String> {
    const STOP: &[&str] = &[
        "a", "an", "the", "to", "of", "in", "is", "are", "how", "what", "we", "do", "does", "can",
        "for", "and", "with", "this", "that", "should",
    ];
    text::words(value)
        .into_iter()
        .filter(|x| !STOP.contains(&x.as_str()))
        .collect()
}
pub fn unresolved_needs(needs: &[String], documents: &[String]) -> Vec<String> {
    let available: BTreeSet<_> = documents.iter().flat_map(|x| terms(x)).collect();
    needs
        .iter()
        .filter(|need| {
            let required = terms(need);
            required.is_empty() || required.intersection(&available).count() * 2 < required.len()
        })
        .cloned()
        .collect()
}

pub fn rank(request: &Value) -> Result<Value> {
    // An unsupported option is refused, or at an inert value ignored, first.
    crate::archived::check(request)?;
    let rows = request
        .get("rows")
        .and_then(Value::as_array)
        .ok_or("Expected source rows")?;
    let question = crate::string(request, "question")?;
    let scope = crate::string(request, "scope")?;
    let limit = request.get("limit").and_then(Value::as_u64).unwrap_or(6) as usize;
    if rows.len() > MAX_ROWS
        || scope.len() > 500
        || question.len() > 16000
        || !(1..=MAX_RANK_LIMIT).contains(&limit)
    {
        return Err("Retrieval limits exceeded".into());
    }
    let mut q = terms(question);
    if let Some(needs) = request.get("needs").and_then(Value::as_array) {
        if needs.len() > 20 {
            return Err("At most 20 evidence needs".into());
        }
        for need in needs {
            q.extend(terms(
                need.as_str().ok_or("Evidence needs must be strings")?,
            ));
        }
    }
    let mut ids = BTreeMap::new();
    let requested_mode = request
        .get("mode")
        .and_then(Value::as_str)
        .unwrap_or("auto");
    let mode = match requested_mode {
        "auto"
            if q.iter()
                .any(|t| ["procedure", "steps", "repeat", "lesson"].contains(&t.as_str())) =>
        {
            "procedures"
        }
        "auto"
            if q.iter().any(|t| {
                ["import", "calls", "impact", "relationship", "dependencies"].contains(&t.as_str())
            }) =>
        {
            "relationships"
        }
        "auto" => "balanced",
        "balanced" | "procedures" | "relationships" => requested_mode,
        _ => return Err("Unsupported memory query mode".into()),
    };
    // Opt-in Porter stemming of query and candidate words (Lucene/Anserini
    // English BM25; Mem0's lemmatized BM25). Mode detection above keeps the
    // surface words.
    let stem = match request.get("stem") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(value)) => *value,
        Some(_) => return Err("stem must be a boolean".into()),
    };
    let mut stemmer = crate::stem::Stemmer::default();
    if stem {
        q = q.iter().map(|t| stemmer.stem(t)).collect();
    }
    let speaker_option = request.get("speaker").unwrap_or(&Value::Null);
    let auto_side = (speaker_option.as_str() == Some("auto")).then(|| auto_speaker(question));
    let speaker = speaker_roles(speaker_option, question)?;
    // Opt-in `key_expansion: "merge"`: LongMemEval's key expansion
    // (xiaowu0162/LongMemEval@9e0b455f: `resolve_expansion`, join mode
    // `merge`, in `src/retrieval/index_expansion_utils.py`; MIT). A row's
    // lexical key is its `abstractions`, then its title and text (K = V +
    // fact), so a fact extracted from a passage also finds the passage. Its
    // evidence is still the text alone.
    let merge_keys = match request.get("key_expansion") {
        None | Some(Value::Null) | Some(Value::Bool(false)) => false,
        Some(Value::String(mode)) if mode == "merge" => true,
        Some(_) => return Err("key_expansion must be \"merge\" or false".into()),
    };
    let mut keyed_rows = 0;
    let mut contents = Vec::with_capacity(rows.len());
    for (i, row) in rows.iter().enumerate() {
        let id = crate::string(row, "identity")?;
        if ids.insert(id.to_owned(), i).is_some() {
            return Err("Candidate identities must be unique".into());
        }
        let title = crate::string(row, "title")?;
        let body = crate::string(row, "text")?;
        let mut content = String::new();
        if merge_keys {
            let keys = abstractions(row)?;
            keyed_rows += usize::from(!keys.is_empty());
            for key in keys {
                content.push_str(key);
                content.push(' ');
            }
        }
        content.push_str(title);
        content.push(' ');
        content.push_str(body);
        contents.push(content);
    }
    // Features depend only on the text, so the rows' are computed side by
    // side (a command-line process starts with an empty cache) and kept in
    // row order.
    let features = cache::par_map(
        &contents,
        crate::stem::Stemmer::default,
        |stemmer, content| {
            if stem {
                cache::stemmed_features(scope, content, stemmer)
            } else {
                cache::features(scope, content)
            }
        },
    )
    .into_iter()
    .collect::<Result<Vec<_>>>()?;
    drop(contents);
    let average = (features.iter().map(|x| x.length).sum::<usize>() as f64
        / (rows.len().max(1) as f64))
        .max(1.0);
    let term_weights: Vec<_> = q
        .iter()
        .map(|t| {
            let frequency = features.iter().filter(|x| x.words.contains_key(t)).count() as f64;
            let idf = (1.0 + (rows.len() as f64 - frequency + 0.5) / (frequency + 0.5)).ln();
            (t, idf)
        })
        .collect();
    let mut lexical = Vec::new();
    let question_lowercase = question.to_lowercase();
    for (i, row) in rows.iter().enumerate() {
        let feature = &features[i];
        let mut score = 0.0;
        for &(term, idf) in &term_weights {
            let tf = feature.words.get(term) as f64;
            if tf > 0.0 {
                score +=
                    idf * tf * 2.2 / (tf + 1.2 * (0.25 + 0.75 * feature.length as f64 / average));
            }
        }
        let title = row["title"].as_str().unwrap_or("").to_lowercase();
        if !title.is_empty() && question_lowercase.contains(&title) {
            score += 5.0;
        }
        if score > 0.0 {
            lexical.push((row["identity"].as_str().unwrap().to_owned(), score));
        }
    }
    lexical.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    let mut rankings = vec![
        lexical
            .iter()
            .take(200)
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>(),
    ];
    let mut names = vec!["lexical".to_owned()];
    // Opt-in `speaker`: BM25 over the kept speakers' turns only, fused as one
    // more arm beside the whole-passage ranking.
    let mut speaker_report = auto_side.map_or(json!(null), |side| json!({"auto": side}));
    if let Some(keep) = &speaker {
        let mut bags = Vec::new();
        let spoken = cache::par_map(rows, crate::stem::Stemmer::default, |stemmer, row| {
            let spoken = speaker_text(row["text"].as_str().unwrap_or(""), keep);
            if stem {
                cache::stemmed_features(scope, &spoken, stemmer)
            } else {
                cache::features(scope, &spoken)
            }
        });
        for (i, feature) in spoken.into_iter().enumerate() {
            let feature = feature?;
            if feature.length > 0 {
                bags.push((i, feature));
            }
        }
        let n = bags.len() as f64;
        let average =
            (bags.iter().map(|(_, f)| f.length).sum::<usize>() as f64 / n.max(1.0)).max(1.0);
        let idf: Vec<(&String, f64)> = q
            .iter()
            .map(|t| {
                let df = bags.iter().filter(|(_, f)| f.words.contains_key(t)).count() as f64;
                (t, (1.0 + (n - df + 0.5) / (df + 0.5)).ln())
            })
            .collect();
        let mut spoken_rank: Vec<(String, f64)> = bags
            .iter()
            .filter_map(|(i, feature)| {
                let score: f64 = idf
                    .iter()
                    .map(|&(term, idf)| {
                        let tf = feature.words.get(term) as f64;
                        idf * tf * 2.2
                            / (tf + 1.2 * (0.25 + 0.75 * feature.length as f64 / average))
                    })
                    .sum();
                (score > 0.0).then(|| (rows[*i]["identity"].as_str().unwrap().to_owned(), score))
            })
            .collect();
        spoken_rank.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        speaker_report = json!({"keep":keep,"rows":bags.len(),"ranked":spoken_rank.len()});
        if let Some(side) = auto_side {
            speaker_report["auto"] = json!(side);
        }
        if !spoken_rank.is_empty() {
            rankings.push(
                spoken_rank
                    .into_iter()
                    .take(200)
                    .map(|(id, _)| id)
                    .collect(),
            );
            names.push("speaker".to_owned());
        }
    }
    // Opt-in `query_variants`: each variant ranks the same candidates by BM25
    // (stemmed like the question) and fuses as arm `lexical_q1`, `lexical_q2`,
    // … weighted by its `weight` unless `arm_weights` names that arm.
    let variants = query_variants(request.get("query_variants").unwrap_or(&Value::Null))?;
    let mut variant_report = Vec::new();
    let mut variant_weights = Vec::new();
    for (i, variant) in variants.iter().enumerate() {
        let mut bag = terms(&variant.text);
        if stem {
            bag = bag.iter().map(|t| stemmer.stem(t)).collect();
        }
        let ranked = bm25_rank(rows, &features, &bag, average);
        let arm = format!("lexical_q{}", i + 1);
        variant_report.push(
            json!({"arm":arm,"kind":variant.kind,"weight":variant.weight,
            "terms":bag.len(),"ranked":ranked.len()}),
        );
        if !ranked.is_empty() {
            rankings.push(ranked.into_iter().take(200).map(|(id, _)| id).collect());
            names.push(arm.clone());
            variant_weights.push((arm, variant.weight));
        }
    }
    if let Some(external) = request.get("rankings").and_then(Value::as_object) {
        for (name, ranking) in external {
            let items = ranking.as_array().ok_or("Ranking must be an array")?;
            let mut seen = BTreeSet::new();
            let mut list = Vec::new();
            for id in items.iter().take(200).filter_map(Value::as_str) {
                if ids.contains_key(id) && seen.insert(id) {
                    list.push(id.to_owned());
                }
            }
            rankings.push(list);
            names.push(name.clone());
        }
    }
    // Arm weights and the RRF constant are opt-in; unset, every arm counts 1
    // and k is 61, as before.
    let rrf_k = request.get("rrf_k").and_then(Value::as_f64).unwrap_or(61.0);
    if !(1.0..=1000.0).contains(&rrf_k) {
        return Err("rrf_k must be between 1 and 1000".into());
    }
    let mut arm_weights = match request.get("arm_weights") {
        None | Some(Value::Null) => serde_json::Map::new(),
        Some(Value::Object(map)) => map.clone(),
        Some(_) => return Err("arm_weights must map arm names to weights".into()),
    };
    for (arm, weight) in variant_weights {
        arm_weights.entry(arm).or_insert(json!(weight));
    }
    // A caller's dense ranking for variant i (`rankings.semantic_q{i}`) counts
    // the variant's weight times the `semantic` arm's unless named here.
    let semantic = arm_weights
        .get("semantic")
        .and_then(Value::as_f64)
        .unwrap_or(1.0);
    for (i, variant) in variants.iter().enumerate() {
        let arm = format!("semantic_q{}", i + 1);
        if names.contains(&arm) {
            let weight = (variant.weight * semantic).min(4.0);
            arm_weights.entry(arm.clone()).or_insert(json!(weight));
            variant_report[i]["dense"] = json!(arm);
        }
    }
    let weight_of = |name: &str| -> Result<f64> {
        match arm_weights.get(name) {
            None => Ok(1.0),
            Some(v) => v
                .as_f64()
                .filter(|w| (0.0..=4.0).contains(w))
                .ok_or_else(|| "Arm weights must be between 0 and 4".into()),
        }
    };
    let weights = names
        .iter()
        .map(|n| weight_of(n))
        .collect::<Result<Vec<_>>>()?;
    // Opt-in `origin_prior`: a weight between 0 and 1 per origin (the
    // runtime's code-first prior gives a code question's documentation and
    // test files less than their source). Every fused score of the origin's
    // rows, and every score the relationship lift gives them, is multiplied
    // by it before the order is taken, so selection and packing see it.
    let priors = origin_priors(request)?;
    let prior = |i: usize| -> f64 {
        rows[i]
            .get("origin")
            .and_then(Value::as_str)
            .and_then(|origin| priors.get(origin))
            .copied()
            .unwrap_or(1.0)
    };
    let mut scores: BTreeMap<String, f64> = rrf_weighted(&rankings, &weights, rrf_k, 0.0)?
        .into_iter()
        .map(|(id, score)| {
            let weight = ids.get(&id).map_or(1.0, |&i| prior(i));
            (id, score * weight)
        })
        .collect();
    let mut reasons: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for ((name, ranking), weight) in names.iter().zip(&rankings).zip(&weights) {
        if *weight == 0.0 {
            continue;
        }
        for id in ranking {
            reasons.entry(id.clone()).or_default().push(name.clone());
        }
    }
    let global = request
        .get("global_notes")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    for id in global.iter().filter_map(Value::as_str) {
        if ids.contains_key(id) {
            scores.entry(id.to_owned()).or_insert(0.001);
            reasons
                .entry(id.to_owned())
                .or_default()
                .push("global_reviewed_decision".into());
        }
    }
    for (id, score) in &mut scores {
        let row = &rows[ids[id]];
        let kind = row
            .get("memory")
            .and_then(|x| x.get("kind"))
            .and_then(Value::as_str)
            .unwrap_or("");
        if mode == "procedures" && ["procedure", "lesson"].contains(&kind) {
            *score *= 1.3;
            reasons
                .entry(id.clone())
                .or_default()
                .push(format!("query_mode:{mode}"));
        }
    }
    let mut base: Vec<_> = scores.iter().map(|(k, v)| (k.clone(), *v)).collect();
    base.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    let received = request
        .get("edges")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let mut frontier: BTreeSet<String> = base
        .iter()
        .take(4)
        .filter_map(|(id, _)| {
            rows[*ids.get(id)?]
                .get("source")?
                .get("path")?
                .as_str()
                .map(str::to_owned)
        })
        .collect();
    let hops = if mode == "relationships" { 2 } else { 1 };
    let (bounded, static_edges) = bound_static_edges(received, &frontier, hops);
    let edges: &[Value] = bounded.as_deref().unwrap_or(received);
    let mut visited = frontier.clone();
    let top = base.first().map(|(_, s)| *s).unwrap_or(0.0);
    for hop in 1..=hops {
        let mut next = BTreeSet::new();
        for edge in edges {
            let a = edge.get("source").and_then(Value::as_str).unwrap_or("");
            let b = edge.get("target").and_then(Value::as_str).unwrap_or("");
            if frontier.contains(a) && !visited.contains(b) {
                next.insert(b.to_owned());
            }
            if frontier.contains(b) && !visited.contains(a) {
                next.insert(a.to_owned());
            }
        }
        for (i, row) in rows.iter().enumerate() {
            let path = row
                .get("source")
                .and_then(|x| x.get("path"))
                .and_then(Value::as_str)
                .unwrap_or("");
            if next.contains(path) {
                let id = row["identity"].as_str().unwrap();
                // Prefer a passage with query evidence within the related file.
                let overlap = q
                    .iter()
                    .filter(|t| features[i].words.contains_key(t))
                    .count() as f64
                    / q.len().max(1) as f64;
                let score = top * (0.45 + 0.15 * overlap) / (hop as f64) * prior(i);
                let entry = scores.entry(id.to_owned()).or_default();
                *entry = entry.max(score);
                reasons
                    .entry(id.to_owned())
                    .or_default()
                    .push(format!("static_relationship:{hop}"));
            }
        }
        visited.extend(next.iter().cloned());
        frontier = next;
    }
    let mut remaining: Vec<_> = scores.into_iter().collect();
    remaining.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    remaining.truncate(400);
    // Diagnostics: the fused order before selection (for recall harnesses).
    let debug_fused = request
        .get("debug_fused")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        .min(400) as usize;
    let fused: Vec<String> = remaining
        .iter()
        .filter(|(id, _)| rows[ids[id]]["kind"] != "memory")
        .take(debug_fused)
        .map(|(id, _)| id.clone())
        .collect();
    // Evidence strength (Tiermem's router features, memind's strong signal):
    // weak when the best passages cover little of the question, or the best
    // lexical score is low and barely above the next.
    let squash = |x: f64| x / (1.0 + x);
    let s1 = lexical.first().map_or(0.0, |x| squash(x.1));
    let s2 = lexical.get(1).map_or(0.0, |x| squash(x.1));
    let top3: Vec<usize> = remaining
        .iter()
        .map(|(id, _)| ids[id])
        .filter(|&i| rows[i]["kind"] != "memory")
        .take(3)
        .collect();
    let question_coverage = if q.is_empty() {
        1.0
    } else {
        q.iter()
            .filter(|t| top3.iter().any(|&i| features[i].words.contains_key(t)))
            .count() as f64
            / q.len() as f64
    };
    let weak = question_coverage < 0.5 || (s1 < 0.85 && s1 - s2 < 0.15);
    let aggregate = aggregate_question(&question_lowercase);
    // Per-origin quotas (LightRAG's linear gradient): the origin with the best
    // candidate may give `max` sources, later ones down to `min`; leftover
    // slots go one at a time to the earliest origins. Unset: 2 per origin.
    let quota = match request.get("origin_quota") {
        None | Some(Value::Null) => None,
        Some(v) => {
            let bound = |key: &str, default: u64| {
                v.get(key)
                    .and_then(Value::as_u64)
                    .unwrap_or(default)
                    .clamp(1, 10) as usize
            };
            let (max, min) = (bound("max", 3), bound("min", 1));
            Some((max.max(min), min.min(max)))
        }
    };
    let mut origin_order: BTreeMap<String, usize> = BTreeMap::new();
    for (id, _) in &remaining {
        let row = &rows[ids[id]];
        if row["kind"] != "memory" {
            let origin = row["origin"].as_str().unwrap_or("").to_owned();
            let next = origin_order.len();
            origin_order.entry(origin).or_insert(next);
        }
    }
    let origin_count = origin_order.len();
    let cap_of = |origin: &str| -> usize {
        match quota {
            None => 2,
            Some((max, min)) => {
                let i = origin_order.get(origin).copied().unwrap_or(origin_count);
                if origin_count <= 1 {
                    max
                } else {
                    let span = (max - min) as f64 * i.min(origin_count - 1) as f64
                        / (origin_count - 1) as f64;
                    ((max as f64 - span).round() as usize).clamp(min, max)
                }
            }
        }
    };
    let maximum = remaining.first().map(|x| x.1).unwrap_or(1.0).max(1e-12);
    let needs: Vec<String> = request
        .get("needs")
        .and_then(Value::as_array)
        .map(|x| {
            x.iter()
                .filter_map(Value::as_str)
                .take(20)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    let need_terms: Vec<BTreeSet<String>> = needs
        .iter()
        .map(|x| {
            let set = terms(x);
            if stem {
                set.iter().map(|t| stemmer.stem(t)).collect()
            } else {
                set
            }
        })
        .collect();
    let mut covered = BTreeSet::new();
    let mut covered_terms = BTreeSet::new();
    let mut selected: Vec<usize> = Vec::new();
    // Each candidate/selected pair is invariant during this request. Advance
    // its maximum only across newly selected rows, preserving score and tie
    // order without recomputing all earlier pairs on every selection.
    let mut redundancy = vec![(0usize, 0.0_f64); rows.len()];
    let mut sources = Vec::new();
    let mut memories = Vec::new();
    let mut origins: BTreeMap<String, usize> = BTreeMap::new();
    let mut content_seen = BTreeSet::new();
    while !remaining.is_empty() {
        let mut best = None;
        for (position, (id, score)) in remaining.iter().enumerate() {
            let i = ids[id];
            let row = &rows[i];
            let memory = row["kind"] == "memory";
            let origin = crate::string(row, "origin")?;
            if (memory && memories.len() >= 12)
                || (!memory
                    && (sources.len() >= limit
                        || *origins.get(origin).unwrap_or(&0) >= cap_of(origin)))
            {
                continue;
            }
            let novelty = q
                .iter()
                .filter(|t| features[i].words.contains_key(t) && !covered_terms.contains(*t))
                .count() as f64
                / q.len().max(1) as f64;
            let new_needs = need_terms
                .iter()
                .enumerate()
                .filter(|(n, t)| {
                    !covered.contains(n)
                        && !t.is_empty()
                        && t.iter()
                            .filter(|x| features[i].words.contains_key(x))
                            .count()
                            * 2
                            >= t.len()
                })
                .count() as f64;
            let (compared, maximum_similarity) = &mut redundancy[i];
            let new_maximum = selected[*compared..]
                .iter()
                .map(|j| {
                    let a = &features[i].words;
                    let b = &features[*j].words;
                    let common = a.common(b) as f64;
                    common / (a.len() + b.len()).max(1) as f64 * 2.0
                })
                .fold(*maximum_similarity, f64::max);
            *compared = selected.len();
            *maximum_similarity = new_maximum;
            let utility = score / maximum + 0.2 * novelty + 0.15 * new_needs - 0.2 * new_maximum;
            if best
                .as_ref()
                .is_none_or(|(_, u): &(usize, f64)| utility > *u)
            {
                best = Some((position, utility));
            }
        }
        let best = best.map(|(position, _)| position).or_else(|| {
            // Leftover polling: with quotas, unfilled source slots go to the
            // remaining candidate of the earliest origin.
            if quota.is_none() || sources.len() >= limit {
                return None;
            }
            remaining
                .iter()
                .enumerate()
                .filter(|(_, (id, _))| rows[ids[id]]["kind"] != "memory")
                .min_by_key(|(position, (id, _))| {
                    let origin = rows[ids[id]]["origin"].as_str().unwrap_or("");
                    (
                        origin_order.get(origin).copied().unwrap_or(usize::MAX),
                        *position,
                    )
                })
                .map(|(position, _)| position)
        });
        let Some(position) = best else {
            break;
        };
        let (id, _) = remaining.remove(position);
        let i = ids[&id];
        let row = &rows[i];
        let body = crate::string(row, "text")?;
        let content_hash = text::sha256(body.as_bytes());
        if !content_seen.insert(content_hash) {
            continue;
        }
        for (n, t) in need_terms.iter().enumerate() {
            if !t.is_empty()
                && t.iter()
                    .filter(|x| features[i].words.contains_key(x))
                    .count()
                    * 2
                    >= t.len()
            {
                covered.insert(n);
            }
        }
        covered_terms.extend(
            q.iter()
                .filter(|t| features[i].words.contains_key(t))
                .cloned(),
        );
        if row["kind"] == "memory" {
            memories.push(id);
        } else {
            *origins
                .entry(row["origin"].as_str().unwrap_or("").to_owned())
                .or_default() += 1;
            sources.push(id);
        }
        selected.push(i);
    }
    let unresolved: Vec<_> = needs
        .iter()
        .enumerate()
        .filter(|(i, _)| !covered.contains(i))
        .map(|(_, x)| x)
        .collect();
    let mut output = json!({"sources":sources,"memories":memories,"reasons":reasons,"unresolved_needs":unresolved,"mode":mode,
        "relationship_hops":hops,"decisions":selected.len(),
        "need_assessment":"lexical candidate coverage, not semantic sufficiency","complete_evidence_guaranteed":false,
        "strategy":"hybrid_rrf_graph_sequential_diversity","network_requests":0,"cache":cache::stats()?,
        "evidence_strength":{"s1":s1,"s2":s2,"question_coverage":question_coverage,"weak":weak,"aggregate":aggregate,
            "limit":limit}});
    if debug_fused > 0 {
        output["fused"] = json!(fused);
    }
    if quota.is_some() {
        output["origin_quota"] = json!({"origins":origin_count});
    }
    if !priors.is_empty() {
        output["origin_prior"] = json!({"origins":priors.len()});
    }
    if stem {
        output["stemmer"] = json!("porter");
    }
    if merge_keys {
        output["key_expansion"] = json!({"mode":"merge","rows":keyed_rows});
    }
    if !speaker_report.is_null() {
        output["speaker"] = speaker_report;
    }
    if !variant_report.is_empty() {
        output["query_variants"] = json!(variant_report);
    }
    if !static_edges.is_null() {
        output["static_edges"] = static_edges;
    }
    Ok(output)
}

/// The request's `origin_prior`: origin → weight, each between 0 and 1;
/// absent or `null`, none. At most [`MAX_ROWS`] origins.
fn origin_priors(request: &Value) -> Result<BTreeMap<String, f64>> {
    let Some(value) = request.get("origin_prior").filter(|v| !v.is_null()) else {
        return Ok(BTreeMap::new());
    };
    let refused = || "origin_prior must map origins to weights between 0 and 1".into();
    let entries = value
        .as_object()
        .filter(|entries| entries.len() <= MAX_ROWS)
        .ok_or_else(refused)?;
    entries
        .iter()
        .map(|(origin, weight)| {
            weight
                .as_f64()
                .filter(|w| (0.0..=1.0).contains(w))
                .map(|w| (origin.clone(), w))
                .ok_or_else(refused)
        })
        .collect()
}

/// Most static relationship edges [`rank`] reads.
pub const MAX_STATIC_EDGES: usize = 50_000;

/// Within [`MAX_STATIC_EDGES`], the edges as given (`None`) and no report.
/// Past it they are folded to one per (source file, target
/// file, kind), which leaves the relationship lift unchanged because it reads
/// file pairs only. If the distinct pairs alone still exceed the bound, the
/// pairs the lift reads are kept first: those touching `frontier` (the files
/// of the top-ranked passages) and, with `hops` above 1, the files the lift
/// reaches before its last hop. The rest fill the bound in input order. The
/// report says what happened: `aggregated`, `capped`, and `lift_complete`
/// (every pair the lift reads was kept, so it lifts exactly what the whole
/// list would). Past the bound `rank` used to fail the request.
pub fn bound_static_edges(
    edges: &[Value],
    frontier: &BTreeSet<String>,
    hops: usize,
) -> (Option<Vec<Value>>, Value) {
    if edges.len() <= MAX_STATIC_EDGES {
        return (None, Value::Null);
    }
    fn field<'a>(edge: &'a Value, key: &str) -> &'a str {
        edge.get(key).and_then(Value::as_str).unwrap_or("")
    }
    let mut seen = BTreeSet::new();
    let mut folded: Vec<(&str, &str, &str)> = Vec::new();
    for edge in edges {
        let key = (
            field(edge, "source"),
            field(edge, "target"),
            field(edge, "kind"),
        );
        if seen.insert(key) {
            folded.push(key);
        }
    }
    let distinct = folded.len();
    let mut lift_complete = true;
    if distinct > MAX_STATIC_EDGES {
        // The files whose pairs the lift reads: the frontier, then the files
        // each hop but the last newly reaches.
        let mut near: BTreeSet<&str> = frontier.iter().map(String::as_str).collect();
        let mut current = near.clone();
        for _ in 1..hops {
            let mut next = BTreeSet::new();
            for (a, b, _) in &folded {
                if current.contains(a) && !near.contains(b) {
                    next.insert(*b);
                }
                if current.contains(b) && !near.contains(a) {
                    next.insert(*a);
                }
            }
            near.extend(next.iter().copied());
            current = next;
        }
        let (mut kept, rest): (Vec<_>, Vec<_>) = folded
            .into_iter()
            .partition(|(a, b, _)| near.contains(a) || near.contains(b));
        lift_complete = kept.len() <= MAX_STATIC_EDGES;
        kept.extend(rest);
        kept.truncate(MAX_STATIC_EDGES);
        folded = kept;
    }
    let used: Vec<Value> = folded
        .iter()
        .map(|(source, target, kind)| json!({"source":source,"target":target,"kind":kind}))
        .collect();
    let report = json!({"received":edges.len(),"distinct":distinct,"used":used.len(),
        "bound":MAX_STATIC_EDGES,"aggregated":true,"capped":distinct > MAX_STATIC_EDGES,
        "lift_complete":lift_complete});
    (Some(used), report)
}
