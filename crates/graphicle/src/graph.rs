//! Property graph projections over current authorized sources and reviewed knowledge.
//! Neo4j is optional infrastructure; its results never bypass the canonical source view.
use crate::{
    Error, Result, claims, digest, integer, model, now, query,
    store::{Principal, Store, decode, one, repo, rows, s},
    string,
};
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// Most nodes of one property graph projection.
pub const MAX_NODES: usize = 50_000;
/// Most edges of one property graph projection.
pub const MAX_EDGES: usize = 200_000;
/// Key beside a query's rankings that carries `graph_local`'s status when it
/// did not rank (`degraded` past its budget) into the packet's `mechanisms`;
/// `context` takes it out before fusion, as it does the observer's note keys.
pub const LOCAL_STATUS: &str = "graph_local_status";

pub fn snapshot(db: &Connection, repository: &Value, principal: &Principal) -> Result<Value> {
    let snap = query::snapshot(db, repository, principal)?;
    from_snapshot(db, repository, principal, &snap)
}

/// The property graph, or what exceeded its budget.
enum Built {
    Graph(Value),
    OverBudget(Value),
}

pub fn from_snapshot(
    db: &Connection,
    repository: &Value,
    principal: &Principal,
    snap: &query::Snapshot,
) -> Result<Value> {
    match build(db, repository, principal, snap, true)? {
        Built::Graph(graph) => Ok(graph),
        Built::OverBudget(_) => Err(Error::bad(
            "Property graph exceeds its configured node/edge budget",
        )),
    }
}

/// The property graph over `snap`; without `documents` (`doc_links: false`)
/// the static edges from documents to code are left
/// out, and the graph says so (`document_edges: "excluded"`).
fn build(
    db: &Connection,
    repository: &Value,
    principal: &Principal,
    snap: &query::Snapshot,
    documents: bool,
) -> Result<Built> {
    let budget = |nodes: usize, edges: Option<usize>| {
        Built::OverBudget(
            json!({"nodes":nodes,"edges":edges,"max_nodes":MAX_NODES,"max_edges":MAX_EDGES}),
        )
    };
    // Every readable document is a node: past the node budget, say so before
    // reading claims, entities and memories.
    if snap.documents.len() > MAX_NODES {
        return Ok(budget(snap.documents.len(), None));
    }
    let mut nodes = BTreeMap::<String, Value>::new();
    let mut edges = BTreeMap::<String, Value>::new();
    for doc in &snap.documents {
        nodes.insert(s(doc,"id").into(),json!({"id":doc["id"],"kind":"source","name":doc["title"],"sha256":doc["sha256"],"recorded_at":doc["recorded_at"],"url":doc["url"]}));
    }
    fn edge(
        edges: &mut BTreeMap<String, Value>,
        from: String,
        to: String,
        kind: &str,
        properties: Value,
    ) {
        let id = digest(&json!([from, to, kind, properties]));
        edges.insert(
            id.clone(),
            json!({"id":id,"source":from,"target":to,"kind":kind,"properties":properties}),
        );
    }
    for e in snap.graph["edges"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|e| documents || !crate::doc_links::document_edge(e))
    {
        let from = format!("file:{}", s(e, "source"));
        let to = format!("file:{}", s(e, "target"));
        if nodes.contains_key(&from) && nodes.contains_key(&to) {
            edge(&mut edges, from, to, "code", e.clone());
        }
    }
    let facts = claims::current(db, repository, principal)?;
    let links = rows(
        db,
        "SELECT * FROM am_entity_links WHERE repository_id=?",
        [s(repository, "id")],
    )?;
    let mut timeline = Vec::new();
    for fact in &facts {
        let id = format!("claim:{}", s(fact, "id"));
        let data = &fact["data"];
        nodes.insert(id.clone(),json!({"id":id,"kind":"claim","name":data["statement"],"speaker":data["speaker"],"modality":data["modality"],"conditions":data["conditions"],"valid_at":data["valid_at"],"invalid_at":data["invalid_at"],"knowledge_at":data["knowledge_at"],"memory_id":fact["memory_id"],"memory_revision":fact["memory_revision"]}));
        for evidence in data["evidence"].as_array().into_iter().flatten() {
            if nodes.contains_key(s(evidence, "id")) {
                edge(
                    &mut edges,
                    id.clone(),
                    s(evidence, "id").into(),
                    "supported_by",
                    json!({"quote":evidence["quote"]}),
                );
            }
        }
        for link in links.iter().filter(|l| l["claim_id"] == fact["id"]) {
            for (field, direction) in [("subject_id", "subject"), ("object_id", "object")] {
                let entity_id = format!("entity:{}", s(link, field));
                if !nodes.contains_key(&entity_id)
                    && let Some(mut e) = one(
                        db,
                        "SELECT * FROM am_entities WHERE repository_id=? AND id=?",
                        params![s(repository, "id"), s(link, field)],
                    )?
                {
                    decode(&mut e, "data")?;
                    nodes.insert(entity_id.clone(),json!({"id":entity_id,"kind":"entity","name":e["name"],"entity_kind":e["kind"],"aliases":e["data"]["aliases"]}));
                }
                edge(
                    &mut edges,
                    id.clone(),
                    entity_id,
                    direction,
                    json!({"reviewed":true}),
                );
            }
            let causal = [
                "causes",
                "caused_by",
                "depends_on",
                "blocks",
                "blocked_by",
                "enables",
            ]
            .contains(&s(data, "relation"));
            edge(
                &mut edges,
                format!("entity:{}", s(link, "subject_id")),
                format!("entity:{}", s(link, "object_id")),
                if causal { "causal" } else { "semantic" },
                json!({"relation":data["relation"],"claim_id":fact["id"],"evidence":data["evidence"],"valid_at":data["valid_at"],"invalid_at":data["invalid_at"],"reviewed":true}),
            );
        }
        if let Some(at) = data["valid_at"].as_str() {
            timeline.push((at.to_string(), id));
        }
    }
    timeline.sort();
    for pair in timeline.windows(2) {
        if pair[0].0 != pair[1].0 {
            edge(
                &mut edges,
                pair[0].1.clone(),
                pair[1].1.clone(),
                "temporal_next",
                json!({"basis":"explicit event timestamps; no causation implied"}),
            );
        }
    }
    for m in snap
        .memories
        .iter()
        .filter(|m| m["state"] == "reviewed" && m["stale"] != true)
    {
        let id = format!("memory:{}", s(m, "id"));
        nodes.insert(
            id.clone(),
            json!({"id":id,"kind":m["kind"],"name":m["title"],"revision":m["revision"]}),
        );
        for (field, prefix) in [
            ("dependencies", "file:"),
            ("native_sources", ""),
            ("evidence_links", ""),
        ] {
            for key in m["data"][field]
                .as_object()
                .into_iter()
                .flat_map(|o| o.keys())
            {
                let target = format!("{prefix}{key}");
                if nodes.contains_key(&target) {
                    edge(&mut edges, id.clone(), target, "derived_from", json!({}));
                }
            }
        }
        for key in m["data"]["notes"]
            .as_object()
            .into_iter()
            .flat_map(|o| o.keys())
        {
            let target = format!("memory:{key}");
            edge(&mut edges, id.clone(), target, "uses_memory", json!({}));
        }
    }
    edges.retain(|_, edge| {
        nodes.contains_key(s(edge, "source")) && nodes.contains_key(s(edge, "target"))
    });
    if nodes.len() > MAX_NODES || edges.len() > MAX_EDGES {
        return Ok(budget(nodes.len(), Some(edges.len())));
    }
    let mut graph = json!({"nodes":nodes.into_values().collect::<Vec<_>>(),"edges":edges.into_values().collect::<Vec<_>>(),"source_state":snap.state,"reviewed_interpretations_only":true,"code_coverage":snap.graph["coverage"]});
    if !documents {
        graph[DOCUMENT_EDGES] = json!("excluded");
    }
    Ok(Built::Graph(graph))
}

/// Key of a property graph built without documents' edges to code
/// (`doc_links: false`): its seed search leaves them out too.
const DOCUMENT_EDGES: &str = "document_edges";

/// The property graph `graph_local` ranks over, when the query asks for it
/// or a current reviewed memory exists. Past its budget the
/// query goes on without `graph_local` and reports it `degraded`
/// (`{"degraded": {nodes, edges, max_nodes, max_edges}}`) instead of failing.
/// With `doc_links: false` neither the graph nor
/// its seed search has an edge from a document to code.
pub fn for_query(
    db: &Connection,
    repository: &Value,
    principal: &Principal,
    snap: &query::Snapshot,
    args: &Value,
) -> Result<Option<Value>> {
    if args["graph_retrieval"] == false
        || !(args["graph_retrieval"] == true
            || snap
                .memories
                .iter()
                .any(|m| m["state"] == "reviewed" && m["stale"] != true))
    {
        return Ok(None);
    }
    let documents = crate::doc_links::Options::parse(args)?.documents();
    Ok(Some(
        match build(db, repository, principal, snap, documents)? {
            Built::Graph(graph) => graph,
            Built::OverBudget(report) => json!({"degraded": report}),
        },
    ))
}

pub fn add_rankings(
    snap: &query::Snapshot,
    graph: Option<&Value>,
    question: &str,
    rankings: Value,
) -> Result<Value> {
    add_rankings_on(snap, graph, question, rankings, None)
}

/// [`add_rankings`] over the query's own passages (`query::passages`), so
/// `graph_local` chunks nothing again; with `None` it chunks the snapshot
/// once, and only when its graph has anything to walk.
pub fn add_rankings_on(
    snap: &query::Snapshot,
    graph: Option<&Value>,
    question: &str,
    mut rankings: Value,
    passages: Option<&query::Passages>,
) -> Result<Value> {
    if let Some(graph) = graph {
        if !rankings.is_object() {
            rankings = json!({});
        }
        if let Some(report) = graph.get("degraded") {
            let mut status = report.clone();
            status["status"] = json!("degraded");
            rankings[LOCAL_STATUS] = status;
            return Ok(rankings);
        }
        let ranks = local_rankings_on(snap, graph, question, passages)?;
        rankings
            .as_object_mut()
            .unwrap()
            .extend(ranks.as_object().unwrap().clone());
    }
    Ok(rankings)
}

pub fn read(store: &Store, workspace: &str, token: &str, args: &Value) -> Result<Value> {
    store.authorized(workspace,token,"read",|db,p|{
        let repository=repo(db,p,string(args,"repository_id",100)?)?;
        let snap=query::snapshot(db,&repository,p)?;
        let graph=match build(db,&repository,p,&snap,true)? {
            Built::Graph(graph)=>graph,
            // Past the property graph's budget a read answers with no nodes
            // and says why (`degraded`), as `graph_local` does, instead of
            // failing.
            Built::OverBudget(report)=>return Ok(json!({"nodes":[],"edges":[],"path":[],"truncated":true,"degraded":report,"source_state":snap.state,"reviewed_interpretations_only":true})),
        };
        let nodes=graph["nodes"].as_array().unwrap();let edges=graph["edges"].as_array().unwrap();
        let limit=integer(args,"limit",100,1000)?.max(1);let depth=integer(args,"depth",2,8)?;
        let direction=args["direction"].as_str().unwrap_or("both");if !["in","out","both"].contains(&direction){return Err(Error::bad("Choose in, out or both graph directions"));}
        let kind=args["edge_kind"].as_str();let eligible=|e:&&Value|kind.is_none_or(|k|s(e,"kind")==k);
        let mut visited=BTreeSet::new();let mut queue=VecDeque::new();let mut parents=BTreeMap::<String,String>::new();
        if let Some(start)=args["start"].as_str(){if !nodes.iter().any(|n|s(n,"id")==start){return Err(Error::bad("Start node unavailable"));}queue.push_back((start.to_string(),0));visited.insert(start.to_string());}
        else{
            let q=args["query"].as_str().unwrap_or("").to_lowercase();
            for node in nodes.iter().filter(|n|q.is_empty()||s(n,"name").to_lowercase().contains(&q)).take(limit){let id=s(node,"id").to_string();visited.insert(id.clone());queue.push_back((id,0));}
        }
        let mut truncated=false;
        while let Some((current,at))=queue.pop_front(){
            if at>=depth{continue;}
            for edge in edges.iter().filter(eligible){
                let neighbor=if direction!="in"&&s(edge,"source")==current{Some(s(edge,"target"))}else if direction!="out"&&s(edge,"target")==current{Some(s(edge,"source"))}else{None};
                if let Some(next)=neighbor&&!visited.contains(next){if visited.len()>=limit{truncated=true;continue;}visited.insert(next.into());parents.insert(next.into(),current.clone());queue.push_back((next.into(),at+1));}
            }
        }
        let mut path=Vec::new();if let Some(end)=args["end"].as_str()&&visited.contains(end){let mut current=end;path.push(current.to_string());while let Some(parent)=parents.get(current){path.push(parent.clone());current=parent;}path.reverse();}
        Ok(json!({"nodes":nodes.iter().filter(|n|visited.contains(s(n,"id"))).collect::<Vec<_>>(),"edges":edges.iter().filter(eligible).filter(|e|visited.contains(s(e,"source"))&&visited.contains(s(e,"target"))).collect::<Vec<_>>(),"path":path,"truncated":truncated,"source_state":graph["source_state"],"reviewed_interpretations_only":true}))
    })
}

/// Expand seed evidence through reviewed entities/claims, then return passage IDs
/// suitable for RRF. Unreviewed aliases and stale/denied sources never enter this graph.
pub fn local_rankings(snap: &query::Snapshot, graph: &Value, question: &str) -> Result<Value> {
    local_rankings_on(snap, graph, question, None)
}

/// [`local_rankings`] over passages already chunked from `snap` (`None`:
/// chunked here, once, when the graph has anything to walk): the seed search
/// and the walk share them instead of chunking the documents twice more
///. The seed is still a lexical `context` of its own, so
/// what `graph_local` ranks is unchanged.
pub fn local_rankings_on(
    snap: &query::Snapshot,
    graph: &Value,
    question: &str,
    passages: Option<&query::Passages>,
) -> Result<Value> {
    let mut adjacency = BTreeMap::<&str, Vec<&str>>::new();
    for edge in graph["edges"].as_array().into_iter().flatten().filter(|e| {
        [
            "supported_by",
            "subject",
            "object",
            "semantic",
            "causal",
            "code",
            "derived_from",
            "uses_memory",
        ]
        .contains(&s(e, "kind"))
    }) {
        adjacency
            .entry(s(edge, "source"))
            .or_default()
            .push(s(edge, "target"));
        adjacency
            .entry(s(edge, "target"))
            .or_default()
            .push(s(edge, "source"));
    }
    if adjacency.is_empty() {
        return Ok(json!({"graph_local":[]}));
    }
    let owned;
    let passages = match passages {
        Some(passages) => passages,
        None => {
            owned = query::passages(snap)?;
            &owned
        }
    };
    let mut seed_args = json!({"question":question,"limit":8,"max_tokens":1500,"max_chars":6000});
    if graph.get(DOCUMENT_EDGES).is_some() {
        seed_args["doc_links"] = json!(false);
    }
    let seed = query::context_on(
        snap,
        &seed_args,
        json!([]),
        None,
        None,
        Some(passages.clone()),
        None,
    )?;
    let mut distances = BTreeMap::<String, usize>::new();
    let mut queue = VecDeque::new();
    for source in seed["evidence"].as_array().into_iter().flatten() {
        distances.insert(s(source, "id").into(), 0);
        queue.push_back((s(source, "id").to_string(), 0));
    }
    for memory in seed["memories"].as_array().into_iter().flatten() {
        let id = format!("memory:{}", s(memory, "id"));
        distances.insert(id.clone(), 0);
        queue.push_back((id, 0));
    }
    let query_words = graphicle_memory::text::words(question);
    for node in graph["nodes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|n| n["kind"] == "entity")
    {
        let mut names = std::iter::once(s(node, "name")).chain(
            node["aliases"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str),
        );
        if names.any(|name| {
            let words = graphicle_memory::text::words(name);
            name.len() >= 3
                && !words.is_empty()
                && query_words
                    .windows(words.len())
                    .any(|window| window == words)
        }) {
            let id = s(node, "id").to_string();
            distances.insert(id.clone(), 0);
            queue.push_back((id, 0));
        }
    }
    while let Some((node, depth)) = queue.pop_front() {
        // One shared-entity join is source -> claim -> entity -> claim -> source.
        if depth >= 4 {
            continue;
        }
        for next in adjacency.get(node.as_str()).into_iter().flatten().copied() {
            if !distances.contains_key(next) && distances.len() < 1000 {
                distances.insert(next.into(), depth + 1);
                queue.push_back((next.to_string(), depth + 1));
            }
        }
    }
    // Only source passages can be reached here (a memory row has no source).
    let candidates = &passages.rows;
    // Rank the exact cited portions of reached originals ahead of unrelated
    // prefixes. A document-level join must not turn into "read its first page".
    let docs: BTreeMap<_, _> = snap.documents.iter().map(|d| (s(d, "id"), d)).collect();
    let mut quote_ranges = BTreeMap::<&str, BTreeSet<(usize, usize)>>::new();
    let mut seen_quotes = BTreeSet::new();
    for edge in graph["edges"].as_array().into_iter().flatten().filter(|e| {
        e["kind"] == "supported_by"
            && distances.contains_key(s(e, "source"))
            && distances.contains_key(s(e, "target"))
    }) {
        let quote = s(&edge["properties"], "quote");
        let source = s(edge, "target");
        if !quote.is_empty()
            && seen_quotes.insert((source, quote))
            && let Some(doc) = docs.get(source)
        {
            quote_ranges.entry(source).or_default().extend(
                s(doc, "content")
                    .match_indices(quote)
                    .take(80)
                    .map(|(start, _)| (start, start + quote.len())),
            );
        }
    }
    let terms: BTreeSet<_> = graphicle_memory::text::words(question)
        .into_iter()
        .filter(|t| t.len() > 2)
        .collect();
    let mut ranked = candidates
        .iter()
        .filter_map(|row| {
            let source_id = s(&row["source"], "id");
            let depth = *distances.get(source_id)?;
            // Repeating the seed list is not an independent graph signal.
            if depth == 0 {
                return None;
            }
            let a = row["source"]["byte_start"].as_u64()? as usize;
            let b = row["source"]["byte_end"].as_u64()? as usize;
            let cited = quote_ranges
                .get(source_id)
                .is_some_and(|ranges| ranges.iter().any(|(x, y)| *x < b && *y > a));
            let words: BTreeSet<_> = graphicle_memory::text::words(s(row, "text"))
                .into_iter()
                .collect();
            let overlap = terms.iter().filter(|t| words.contains(*t)).count();
            if quote_ranges.contains_key(source_id) && !cited && overlap == 0 {
                return None;
            }
            Some((
                !cited,
                depth,
                std::cmp::Reverse(overlap),
                s(row, "identity").to_string(),
            ))
        })
        .collect::<Vec<_>>();
    ranked.sort();
    Ok(json!({"graph_local":ranked.into_iter().take(200).map(|(_,_,_,id)|id).collect::<Vec<_>>()}))
}

pub fn configure_neo4j(store: &Store, workspace: &str, token: &str, args: &Value) -> Result<Value> {
    store.authorized(workspace, token, "admin", |db, p| {
        repo(db, p, string(args, "repository_id", 100)?)?;
        Ok(())
    })?;
    let endpoint = string(args, "endpoint", 2000)?;
    model::checked_url(endpoint)?;
    let database = args["database"].as_str().unwrap_or("neo4j");
    if database.is_empty()
        || database.len() > 100
        || !database
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(Error::bad("Invalid graph database name"));
    }
    let reference = string(args, "credential_ref", 100)?;
    let secret: Value = serde_json::from_str(&store.resolve_credential(workspace, reference)?)?;
    string(&secret, "username", 200)?;
    string(&secret, "password", 1000)?;
    let value = json!({"endpoint":endpoint,"database":database,"credential_ref":reference});
    store.authorized(workspace,token,"admin",|db,p|{let repository=repo(db,p,string(args,"repository_id",100)?)?;if let Some(old)=one(db,"SELECT config FROM am_backends WHERE repository_id=? AND kind='neo4j'",[s(&repository,"id")])?{let cfg:Value=serde_json::from_str(s(&old,"config"))?;if cfg!=value{crate::projection::enqueue_config(db,workspace,s(&repository,"id"),"neo4j",&cfg,"backend_reconfigured")?;}}db.execute("INSERT OR REPLACE INTO am_backends VALUES(?,?,?)",params![s(&repository,"id"),"neo4j",value.to_string()])?;Ok(json!({"configured":true,"index_visibility":"private administrator-owned graph projection"}))})
}
async fn cypher(
    store: &Store,
    client: &reqwest::Client,
    workspace: &str,
    config: &Value,
    statements: Value,
) -> Result<Value> {
    let secret: Value = serde_json::from_str(
        &store.resolve_credential(workspace, string(config, "credential_ref", 100)?)?,
    )?;
    let endpoint = format!(
        "{}/db/{}/tx/commit",
        s(config, "endpoint").trim_end_matches('/'),
        s(config, "database")
    );
    let response = model::bounded_json(
        client
            .post(endpoint)
            .basic_auth(
                string(&secret, "username", 200)?,
                Some(string(&secret, "password", 1000)?),
            )
            .json(&json!({"statements":statements}))
            .send()
            .await
            .map_err(|_| Error::bad("Graph database request failed"))?,
    )
    .await?;
    if response["errors"].as_array().is_none_or(|e| !e.is_empty()) {
        return Err(Error::bad("Neo4j rejected the transaction"));
    }
    Ok(response["results"].clone())
}
pub async fn neo4j(
    store: &Store,
    client: &reqwest::Client,
    workspace: &str,
    token: &str,
    args: &Value,
    publish: bool,
) -> Result<Value> {
    let (config, graph, revision) = store.authorized(workspace, token, "admin", |db, p| {
        let repository = repo(db, p, string(args, "repository_id", 100)?)?;
        let mut backend = one(
            db,
            "SELECT config FROM am_backends WHERE repository_id=? AND kind='neo4j'",
            [s(&repository, "id")],
        )?
        .ok_or_else(|| Error::bad("Configure a Neo4j database first"))?;
        decode(&mut backend, "config")?;
        if publish {
            crate::projection::ready(db, s(&repository, "id"), "neo4j", &backend["config"])?;
        }
        Ok((
            backend["config"].clone(),
            snapshot(db, &repository, p)?,
            crate::projection::marker(db, s(&repository, "id"), "neo4j", &backend["config"])?,
        ))
    })?;
    let scope = string(args, "repository_id", 100)?;
    if publish {
        let mut touched = false;
        let result=async {
        let nodes=graph["nodes"].as_array().unwrap().iter().map(|n|json!({"id":n["id"],"name":n["name"],"kind":n["kind"],"properties":n.to_string()})).collect::<Vec<_>>();
        let edges=graph["edges"].as_array().unwrap().iter().map(|e|json!({"id":e["id"],"source":e["source"],"target":e["target"],"kind":e["kind"],"properties":e["properties"].to_string()})).collect::<Vec<_>>();
        let generation = crate::id();
        // Keep one transaction below the HTTP body bound. No partially published graph.
        if nodes.len() > 10000
            || edges.len() > 30000
            || json!([nodes, edges]).to_string().len() > 3 * 1024 * 1024
        {
            return Err(Error::bad(
                "Neo4j projection exceeds the atomic export budget",
            ));
        }
        let statements = json!([
          {"statement":"MATCH (n:Graphicle {scope:$scope}) DETACH DELETE n","parameters":{"scope":scope}},
          {"statement":"UNWIND $nodes AS item CREATE (n:Graphicle {scope:$scope, id:item.id, kind:item.kind, name:item.name, properties:item.properties, generation:$generation})","parameters":{"scope":scope,"generation":generation,"nodes":nodes}},
          {"statement":"UNWIND $edges AS item MATCH (a:Graphicle {scope:$scope,id:item.source}), (b:Graphicle {scope:$scope,id:item.target}) CREATE (a)-[:LINK {id:item.id,kind:item.kind,properties:item.properties}]->(b)","parameters":{"scope":scope,"edges":edges}}
        ]);
        touched=true;
        cypher(store, client, workspace, &config, statements).await?;
        let still_current = store.authorized(workspace, token, "admin", |db, p| {
            let repository = repo(db, p, scope)?;
            Ok(snapshot(db, &repository, p)?["source_state"] == graph["source_state"] && crate::projection::marker(db, scope, "neo4j", &config)? == revision)
        })?;
        if !still_current {
            cypher(store,client,workspace,&config,json!([{"statement":"MATCH (n:Graphicle {scope:$scope,generation:$generation}) DETACH DELETE n","parameters":{"scope":scope,"generation":generation}}])).await?;
            return Err(Error::conflict(
                "Sources changed during graph publication; the obsolete projection was removed",
            ));
        }
        Ok(
            json!({"published":true,"nodes":nodes.len(),"edges":edges.len(),"source_state":graph["source_state"],"published_at":now()}),
        )
        }.await;
        if touched && result.is_err() {
            store.transaction(|db| {
                crate::projection::enqueue_config(
                    db,
                    workspace,
                    scope,
                    "neo4j",
                    &config,
                    "publication_incomplete",
                )
            })?;
        }
        result
    } else {
        let results=cypher(store,client,workspace,&config,json!([{"statement":"MATCH (n:Graphicle {scope:$scope}) WHERE toLower(n.name) CONTAINS toLower($query) RETURN n.id AS id LIMIT $limit","parameters":{"scope":scope,"query":args["query"].as_str().unwrap_or(""),"limit":integer(args,"limit",20,200)?}}])).await?;
        // Read a new canonical view after the network request, so revocation wins.
        store.authorized(workspace,token,"admin",|db,p|{
            let repository=repo(db,p,scope)?;let current=snapshot(db,&repository,p)?;
            let ids=results[0]["data"].as_array().into_iter().flatten().filter_map(|v|v["row"][0].as_str()).collect::<BTreeSet<_>>();
            Ok(json!({"nodes":current["nodes"].as_array().unwrap().iter().filter(|n|ids.contains(s(n,"id"))).collect::<Vec<_>>(),"canonical_revalidation":true,"source_state":current["source_state"]}))
        })
    }
}
pub(crate) async fn clear_projection(
    store: &Store,
    client: &reqwest::Client,
    workspace: &str,
    config: &Value,
    scope: &str,
) -> Result<()> {
    cypher(store,client,workspace,config,json!([{"statement":"MATCH (n:Graphicle {scope:$scope}) DETACH DELETE n","parameters":{"scope":scope}}])).await?;
    Ok(())
}
