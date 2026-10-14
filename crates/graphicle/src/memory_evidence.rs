//! Follow reviewed interpretations back to current, authorized original bytes.
//! Citations locate evidence; they never establish that an interpretation is true.
use crate::{Result, query::Snapshot, store::s};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

struct Support {
    id: String,
    revision: Value,
    citations: Vec<Value>,
    unresolved: usize,
    limited: bool,
}

pub struct LinkedSources {
    pub hits: Vec<Value>,
    supports: Vec<Support>,
}

impl LinkedSources {
    pub fn coverage(&self, evidence: &[Value]) -> Value {
        json!({"selected_memories":self.supports.len(),"candidate_windows":self.hits.len(),
            "memories":self.supports.iter().map(|support| {
                let present = support.citations.iter().filter(|quote| evidence.iter().any(|e| {
                    e["id"] == quote["id"] && (s(e,"text").contains(s(quote,"quote"))
                        || s(&e["source_context"],"text").contains(s(quote,"quote")))
                })).count();
                json!({"id":support.id,"revision":support.revision,
                    "located_citations":support.citations.len(),"included_citations":present,
                    "unavailable_citations":support.unresolved,"traversal_limited":support.limited,
                    "all_cited_quotes_present":!support.citations.is_empty() && present == support.citations.len() && support.unresolved == 0 && !support.limited,
                    "semantic_entailment_verified":false})
            }).collect::<Vec<_>>()})
    }
}

fn source_is_current(data: &Value, doc: &Value) -> bool {
    let id = s(doc, "id");
    let expected = if let Some(path) = id.strip_prefix("file:") {
        data["dependencies"][path].as_str()
    } else if data["evidence_links"][id].is_object() {
        data["evidence_links"][id]["sha256"].as_str()
    } else {
        data["native_sources"][id].as_str()
    };
    expected.is_some_and(|hash| !hash.is_empty() && hash == s(doc, "sha256"))
}

/// Inputs are retrieved memory references, not arbitrary memory IDs from a model.
/// Older records retain their citations inside the originating workflow payload.
pub fn gather(snap: &Snapshot, selected: &[Value]) -> Result<LinkedSources> {
    gather_with_memories(snap, selected, snap.memories.iter())
}

/// Keep scope/validity filtering in the evidence chain without copying the
/// source documents or graph. The caller supplies an already-authorized view.
pub(crate) fn gather_with_memories<'a>(
    snap: &Snapshot,
    selected: &[Value],
    memories: impl IntoIterator<Item = &'a Value>,
) -> Result<LinkedSources> {
    let docs: BTreeMap<_, _> = snap.documents.iter().map(|d| (s(d, "id"), d)).collect();
    let memories: BTreeMap<_, _> = memories.into_iter().map(|m| (s(m, "id"), m)).collect();
    let mut result = LinkedSources {
        hits: Vec::new(),
        supports: Vec::new(),
    };
    let mut windows = BTreeSet::new();
    let mut roots = BTreeSet::new();
    for selected in selected.iter().take(200) {
        let id = selected["memory"]["id"]
            .as_str()
            .unwrap_or_else(|| s(selected, "id"));
        let Some(root) = memories
            .get(id)
            .filter(|m| m["state"] == "reviewed" && m["stale"] != true)
        else {
            continue;
        };
        if !roots.insert(id) {
            continue;
        }
        let mut support = Support {
            id: id.into(),
            revision: root["revision"].clone(),
            citations: Vec::new(),
            unresolved: 0,
            limited: false,
        };
        let mut queue = VecDeque::from([(*root, 0usize)]);
        let mut visited = BTreeSet::new();
        let mut citations = BTreeSet::new();
        while let Some((memory, depth)) = queue.pop_front() {
            if !visited.insert(s(memory, "id")) {
                continue;
            }
            if visited.len() > 32 {
                support.limited = true;
                break;
            }
            let data = &memory["data"];
            for owner in [
                data,
                &data["claim"],
                &data["answer"],
                &data["report"],
                &data["procedure"],
                &data["candidate"],
            ] {
                for citation in owner["evidence"].as_array().into_iter().flatten() {
                    let source_id = s(citation, "id");
                    let quote = s(citation, "quote");
                    if !citations.insert((source_id.to_string(), quote.to_string())) {
                        continue;
                    }
                    if citations.len() > 80 {
                        support.limited = true;
                        break;
                    }
                    // Where the quote can be found: its source when that is
                    // the version the memory depends on; else, for a file
                    // judged by its code symbols, the current
                    // bytes of those symbols, wherever an exact move put them.
                    let spans: Vec<(&Value, usize, usize)> =
                        match docs.get(source_id).filter(|d| source_is_current(data, d)) {
                            Some(doc) => vec![(*doc, 0, s(doc, "content").len())],
                            None => memory["symbol_links"]["locations"][source_id]
                                .as_array()
                                .into_iter()
                                .flatten()
                                .filter_map(|found| {
                                    Some((
                                        *docs.get(s(found, "id"))?,
                                        found["byte_start"].as_u64()? as usize,
                                        found["byte_end"].as_u64()? as usize,
                                    ))
                                })
                                .collect(),
                        };
                    if spans.is_empty() || quote.is_empty() || quote.len() > 12000 {
                        support.unresolved += 1;
                        continue;
                    }
                    let mut occurrences: Vec<(&Value, usize)> = Vec::new();
                    for (doc, a, b) in &spans {
                        if let Some(range) = s(doc, "content").get(*a..*b) {
                            occurrences.extend(
                                range
                                    .match_indices(quote)
                                    .map(|(at, _)| (*doc, a + at))
                                    .take(5),
                            );
                        }
                    }
                    occurrences.sort_by_key(|(doc, at)| (s(doc, "id").to_string(), *at));
                    occurrences.dedup_by_key(|(doc, at)| (s(doc, "id").to_string(), *at));
                    if occurrences.is_empty() {
                        support.unresolved += 1;
                        continue;
                    }
                    if occurrences.len() > 4 {
                        support.limited = true;
                    }
                    support
                        .citations
                        .push(json!({"id":s(occurrences[0].0, "id"),"quote":quote}));
                    for (doc, start) in occurrences.into_iter().take(4) {
                        let found = s(doc, "id");
                        let end = start + quote.len();
                        if !windows.insert((found.to_string(), start, end)) {
                            continue;
                        }
                        if result.hits.len() >= 40 {
                            support.limited = true;
                            continue;
                        }
                        let mut source = doc.clone();
                        source.as_object_mut().unwrap().remove("content");
                        source["byte_start"] = json!(start);
                        source["byte_end"] = json!(end);
                        result.hits.push(json!({"identity":format!("memory-source:{found}:{start}:{end}"),
                            "kind":"source","origin":found,"title":doc["title"],"text":quote,"source":source}));
                    }
                }
            }
            for (parent, revision) in data["notes"].as_object().into_iter().flatten() {
                if depth >= 4 {
                    support.limited = true;
                    continue;
                }
                if let Some(parent) = memories.get(parent.as_str()).filter(|m| {
                    m["revision"] == *revision && m["state"] == "reviewed" && m["stale"] != true
                }) {
                    queue.push_back((parent, depth + 1));
                } else {
                    support.unresolved += 1;
                }
            }
        }
        result.supports.push(support);
    }
    Ok(result)
}
