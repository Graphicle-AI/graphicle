// Adapted from Graphify 0.9.95 (92741d16), Apache-2.0; see NOTICE.md.
// Upstream: graphify/dedup.py
//! Development JSON bindings for the typed native deduplication API.
use crate::{Result, dedup::*, dedup_strings};
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

fn options(r: &Value) -> Result<DedupOptions> {
    Ok(DedupOptions {
        communities: r["communities"].as_object().cloned().unwrap_or_default(),
        root: r["root"].as_str().map(PathBuf::from),
        dedup_llm_backend: r["dedup_llm_backend"].as_str().map(str::to_owned),
        protected_ids: serde_json::from_value(
            r.get("protected_ids")
                .filter(|v| !v.is_null())
                .cloned()
                .unwrap_or(json!([])),
        )
        .map_err(crate::Error::from)?,
    })
}
// The recorded-client binding is a qualification scenario, never a provider
// fallback. Real callers inject a native TiebreakClient into the library API.
struct RecordedClient {
    available: String,
    env_keys: String,
    responses: Vec<Value>,
    calls: Vec<Value>,
}
impl TiebreakClient for RecordedClient {
    fn availability(&self, _: &str) -> BackendAvailability {
        match self.available.as_str() {
            "unknown" => BackendAvailability::Unknown,
            "missing" => BackendAvailability::MissingCredentials(self.env_keys.clone()),
            _ => BackendAvailability::Available,
        }
    }
    fn call(&mut self, prompt: &str, backend: &str, max_tokens: u32) -> Result<String> {
        let i = self.calls.len();
        self.calls
            .push(json!({"prompt":prompt,"backend":backend,"max_tokens":max_tokens}));
        let response = self.responses.get(i).ok_or("Recorded response exhausted")?;
        if let Some(error) = response["error"].as_str() {
            Err(error.into())
        } else {
            response
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| "Expected recorded text response".into())
        }
    }
}
fn client(r: &Value) -> RecordedClient {
    RecordedClient {
        available: r["availability"].as_str().unwrap_or("available").into(),
        env_keys: r["env_keys"].as_str().unwrap_or("MODEL_API_KEY").into(),
        responses: r["responses"].as_array().cloned().unwrap_or_default(),
        calls: Vec::new(),
    }
}
pub fn dispatch(r: &Value) -> Result<Value> {
    let s = |key: &str| r[key].as_str().unwrap_or("");
    let array = |key: &str| {
        r[key]
            .as_array()
            .ok_or_else(|| format!("Expected {key} array"))
    };
    let root = r["root"].as_str().map(std::path::Path::new);
    match s("function") {
        "_norm" => Ok(json!(norm(&r["label"]))),
        "_entropy" => Ok(json!(entropy(&r["label"]))),
        "_shingles" => Ok(json!(shingles(s("text"), r["k"].as_i64().unwrap_or(3)))),
        "_make_minhash" => Ok(json!(
            make_minhash(s("text"), r["num_perm"].as_u64().unwrap_or(128) as usize).hashvalues
        )),
        "_is_variant_pair" => Ok(json!(is_variant_pair(s("a"), s("b")))),
        "_short_label_blocked" => Ok(json!(short_label_blocked(
            s("a"),
            s("b"),
            r["jw_score"].as_f64().ok_or("Expected score")?
        ))),
        "_numeric_tokens_differ" => Ok(json!(numeric_tokens_differ(s("a"), s("b")))),
        "_same_word_variant" => Ok(json!(same_word_variant(s("x"), s("y")))),
        "_content_token_swap" => Ok(json!(content_token_swap(s("a"), s("b")))),
        "_crossfile_fileanchored_blocked" => Ok(json!(crossfile_fileanchored_blocked(
            &r["node"],
            &r["neighbor"]
        ))),
        "_is_code" => Ok(json!(is_code(&r["node"]))),
        "_id_prefixes" => Ok(json!(id_prefixes(s("source_file")))),
        "_defines_id" => Ok(json!(defines_id(&r["node"]))),
        "_reads_as_file_entity" => Ok(json!(reads_as_file_entity(&r["node"]))),
        "_lifecycle_penalty" => Ok(json!(lifecycle_penalty(s("rank_path")))),
        "_rank_path" => Ok(json!(rank_path(s("source_file"), root))),
        "_collision_rank" => {
            let mentions: Mentions = r["mentions"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|pair| {
                    Some((pair[0].as_str()?.to_owned(), pair[1].as_str()?.to_owned()))
                })
                .collect();
            Ok(json!(collision_rank_with(&r["node"], root, &mentions)))
        }
        "_full_stem" => Ok(json!(full_stem(s("source_file")))),
        "_extension_qualified_id" => Ok(json!(extension_qualified_id(&r["node"]))),
        "_mentions" => {
            let mut found = mentions(array("nodes")?, array("edges")?, root)
                .into_iter()
                .collect::<Vec<_>>();
            found.sort();
            Ok(json!(found))
        }
        "_own_page_files" => {
            let mut found = own_page_files(array("nodes")?, root)
                .into_iter()
                .collect::<Vec<_>>();
            found.sort();
            Ok(json!(found))
        }
        "_slice_references" => {
            let mut found = slice_references(array("nodes")?, root)
                .into_iter()
                .collect::<Vec<_>>();
            found.sort();
            Ok(json!(found))
        }
        "_disambiguate_defining_collisions" | "_path_qualify_dropped_collisions" => {
            let mut nodes = array("nodes")?.clone();
            let mut edges = array("edges")?.clone();
            let mut hyperedges = r["hyperedges"].as_array().cloned();
            let mut stderr = Vec::new();
            if s("function") == "_disambiguate_defining_collisions" {
                disambiguate_defining_collisions(
                    &mut nodes,
                    &mut edges,
                    hyperedges.as_deref_mut(),
                    root,
                );
            } else {
                path_qualify_dropped_collisions(
                    &mut nodes,
                    &mut edges,
                    hyperedges.as_deref_mut(),
                    root,
                    &mut stderr,
                );
            }
            Ok(json!({"nodes": nodes, "edges": edges, "hyperedges": hyperedges, "stderr": stderr}))
        }
        "_same_source_entity" => Ok(json!(same_source_entity(&r["survivor"], &r["duplicate"]))),
        "_merge_missing_attributes" => {
            Ok(merge_missing_attributes(&r["survivor"], &r["duplicate"]))
        }
        "_report_id_collision" => Ok(json!(report_id_collision(
            s("nid"),
            &r["survivor"],
            array("losers")?
        ))),
        "_content_richness" => Ok(json!(content_richness(&r["n"]))),
        "_pick_winner" => Ok(pick_winner(array("nodes")?)?.clone()),
        "_remap_hyperedge_members" => {
            let mut hyperedges = array("hyperedges")?.clone();
            let remap: HashMap<String, String> =
                serde_json::from_value(r["remap"].clone()).map_err(crate::Error::from)?;
            remap_hyperedge_members(&mut hyperedges, &remap);
            Ok(json!(hyperedges))
        }
        "similarity" => Ok(
            json!({"jaro":dedup_strings::jaro(s("a"),s("b")),"jaro_winkler":dedup_strings::jaro_winkler(s("a"),s("b"))}),
        ),
        "uf_scenario" => {
            let mut uf = UnionFind::default();
            let mut values = Vec::new();
            for action in array("actions")? {
                match action[0].as_str() {
                    Some("find") => {
                        values.push(json!(uf.find(action[1].as_str().ok_or("Expected ID")?)))
                    }
                    Some("union") => {
                        uf.union(action[1].get_string()?, action[2].get_string()?);
                        values.push(Value::Null);
                    }
                    Some("components") => values.push(json!(
                        uf.components()
                            .into_iter()
                            .map(|(k, v)| (k, json!(v)))
                            .collect::<Map<_, _>>()
                    )),
                    _ => return Err("Unknown union-find action".into()),
                }
            }
            Ok(json!(values))
        }
        "deduplicate_entities" | "dedup_scenario" => {
            let mut hyperedges = r["hyperedges"].as_array().cloned();
            let mut recorded = client(r);
            let injected: Option<&mut dyn TiebreakClient> =
                (s("function") == "dedup_scenario").then_some(&mut recorded);
            let result = deduplicate_entities(
                array("nodes")?,
                array("edges")?,
                &options(r)?,
                hyperedges.as_deref_mut(),
                injected,
            )?;
            Ok(
                json!({"nodes":result.nodes,"edges":result.edges,"hyperedges":hyperedges,"stdout":result.stdout,"stderr":result.stderr,"calls":recorded.calls}),
            )
        }
        "llm_scenario" => {
            let mut uf = UnionFind::default();
            for pair in r["unions"].as_array().into_iter().flatten() {
                uf.union(pair[0].get_string()?, pair[1].get_string()?);
            }
            let protected: HashSet<String> =
                serde_json::from_value(r.get("protected_ids").cloned().unwrap_or(json!([])))
                    .map_err(crate::Error::from)?;
            let mut recorded = client(r);
            let messages = llm_tiebreak(
                array("candidates")?,
                &mut uf,
                &r["communities"].as_object().cloned().unwrap_or_default(),
                crate::dedup::TiebreakOptions {
                    backend: s("backend"),
                    batch_size: r["batch_size"].as_u64().unwrap_or(30) as usize,
                    low: r["low"].as_f64().unwrap_or(75.0),
                    high: r["high"].as_f64().unwrap_or(92.0),
                },
                &protected,
                &mut recorded,
            )?;
            Ok(
                json!({"components":uf.components().into_iter().map(|(k,v)| (k,json!(v))).collect::<Map<_,_>>(),"stdout":messages,"calls":recorded.calls}),
            )
        }
        _ => Err("Unknown deduplication function".into()),
    }
}
trait ValueString {
    fn get_string(&self) -> Result<&str>;
}
impl ValueString for Value {
    fn get_string(&self) -> Result<&str> {
        self.as_str().ok_or_else(|| "Expected ID string".into())
    }
}
