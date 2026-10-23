use crate::store::{self, Principal, Store, decode, one, repo, rows, s};
use crate::{Error, Result, digest, id, integer, native, now, string};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

pub fn documents(db: &Connection, repository: &Value, p: &Principal) -> Result<Vec<Value>> {
    let mut output = repository_documents(db, repository, true)?;
    output.extend(connector_documents(db, repository, p)?);
    for doc in &output {
        if graphicle_memory::text::sha256(s(doc, "content").as_bytes()) != s(doc, "sha256") {
            return Err(Error::bad("Source integrity check failed"));
        }
    }
    Ok(output)
}

/// The project's files as [`documents`] lists them, in path order; with
/// `content` false each one's `content` is empty, for a caller that reads
/// the text of a few files only ([`load_contents`]).
pub fn repository_documents(
    db: &Connection,
    repository: &Value,
    content: bool,
) -> Result<Vec<Value>> {
    let identifier = s(repository, "id");
    let mut output = Vec::new();
    let sql = if content {
        "SELECT h.path,h.sha256,o.content,b.original_sha,b.parser FROM repo_heads h JOIN repo_objects o ON o.repository_id=h.repository_id AND o.sha256=h.sha256 LEFT JOIN am_binary_heads b ON b.repository_id=h.repository_id AND b.path=h.path AND b.source_sha=h.sha256 WHERE h.repository_id=? ORDER BY h.path"
    } else {
        "SELECT h.path,h.sha256,'' AS content,b.original_sha,b.parser FROM repo_heads h JOIN repo_objects o ON o.repository_id=h.repository_id AND o.sha256=h.sha256 LEFT JOIN am_binary_heads b ON b.repository_id=h.repository_id AND b.path=h.path AND b.source_sha=h.sha256 WHERE h.repository_id=? ORDER BY h.path"
    };
    for row in rows(db, sql, [identifier])? {
        let path = s(&row, "path");
        output.push(json!({"id":format!("file:{path}"),"path":path,"title":path,"source_kind":"repository_file",
            "sha256":row["sha256"],"snapshot":repository["snapshot"],"version":row["sha256"],
            "content":row["content"],"recorded_at":null,"observed_at":repository["updated_at"],
            "authority":"original_repository_source","url":null,"metadata":{"original_sha256":row["original_sha"],"parser":row["parser"]}}));
    }
    Ok(output)
}

/// The text of the project files among `documents` whose ids `wanted`
/// names, read into their `content` (from [`repository_documents`] without
/// content) and checked against their SHA-256, as [`documents`] checks it.
pub fn load_contents(
    db: &Connection,
    repository: &Value,
    documents: &mut [Value],
    wanted: &BTreeSet<String>,
) -> Result<()> {
    let mut load =
        db.prepare_cached("SELECT content FROM repo_objects WHERE repository_id=? AND sha256=?")?;
    for document in documents.iter_mut() {
        if document["source_kind"] != "repository_file" || !wanted.contains(s(document, "id")) {
            continue;
        }
        let content: String = load
            .query_row(params![s(repository, "id"), s(document, "sha256")], |r| {
                r.get(0)
            })
            .optional()?
            .ok_or_else(|| Error::bad("Source unavailable"))?;
        if graphicle_memory::text::sha256(content.as_bytes()) != s(document, "sha256") {
            return Err(Error::bad("Source integrity check failed"));
        }
        document["content"] = json!(content);
    }
    Ok(())
}

/// The connector documents of the project that `p` may read, as
/// [`documents`] lists them after the project's files.
pub fn connector_documents(
    db: &Connection,
    repository: &Value,
    p: &Principal,
) -> Result<Vec<Value>> {
    let identifier = s(repository, "id");
    let mut output = Vec::new();
    // The Python host's workspace-source links
    // (`repo_evidence_links` over `sources`) are no longer read; nothing in
    // this program writes them. Their tables stay.
    for mut row in rows(
        db,
        "SELECT d.*,c.name AS connector_name,c.kind,c.config,c.last_sync,c.credential_env,p.principals AS synced_principals,p.checked_at AS permission_checked_at,p.complete AS permission_complete,p.generation AS permission_generation FROM am_connector_docs d JOIN am_connectors c ON c.id=d.connector_id LEFT JOIN am_source_permissions p ON p.connector_id=d.connector_id AND p.source_id=d.id WHERE c.repository_id=? AND c.state='active' AND d.present=1 ORDER BY d.connector_id,d.id",
        [identifier],
    )? {
        decode(&mut row, "acl")?;
        decode(&mut row, "metadata")?;
        decode(&mut row, "config")?;
        if !acl_allows(&row["acl"], p) {
            continue;
        }
        if let Some(secret) = s(&row, "credential_env").strip_prefix("secret:")
            && one(
                db,
                "SELECT id FROM am_secrets WHERE workspace_id=? AND id=?",
                params![p.workspace, secret],
            )?
            .is_none()
        {
            continue;
        }
        if let Some(account) = s(&row, "credential_env").strip_prefix("oauth:")
            && one(
                db,
                "SELECT id FROM am_oauth_accounts WHERE workspace_id=? AND id=? AND state='active'",
                params![p.workspace, account],
            )?
            .is_none()
        {
            continue;
        }
        if row["permission_generation"] == row["generation"] && row["synced_principals"].is_string()
        {
            row["metadata"]["provider_permissions"] = json!({"principals":serde_json::from_str::<Value>(s(&row,"synced_principals"))?,"complete":row["permission_complete"]==1,"checked_at":row["permission_checked_at"]});
        }
        if row["config"]["permission_mode"] == "provider"
            && !crate::permissions::allows(
                db,
                &p.workspace,
                crate::connectors::identity_provider(s(&row, "kind")),
                &row["metadata"]["provider_permissions"],
                p,
                integer(&row["config"], "permission_max_age_seconds", 300, 3600)?.max(30) as i64,
            )?
        {
            continue;
        }
        // Remote authorization can change outside this process. Expired source
        // snapshots are withheld until the connector completes another scan.
        if row["kind"] != "jsonl" {
            let max_age = integer(&row["config"], "max_age_seconds", 86400, 604800)?.max(60) as i64;
            let fresh = row["last_sync"]
                .as_str()
                .and_then(|v| chrono::DateTime::parse_from_rfc3339(v).ok())
                .is_some_and(|at| {
                    chrono::Utc::now().signed_duration_since(at).num_seconds() <= max_age
                });
            if !fresh {
                continue;
            }
        }
        let metadata = &row["metadata"];
        output.push(json!({"id":format!("connector:{}:{}",s(&row,"connector_id"),s(&row,"id")),
            "path":metadata["title"],"title":metadata["title"],"source_kind":"connector_source",
            "sha256":row["sha256"],"version":digest(&json!({"sha256":row["sha256"],"title":metadata["title"],"source_timestamp":metadata["source_timestamp"],"permissions":metadata["provider_permissions"]["principals"]})),"content":row["content"],"recorded_at":metadata["source_timestamp"],"metadata":metadata["provider_metadata"],
            "observed_at":metadata["observed_at"],"authority":"imported_source","url":metadata["url"],"connector_id":row["connector_id"]}));
    }
    Ok(output)
}
pub fn acl_allows(acl: &Value, p: &Principal) -> bool {
    // Connector data has an explicit sharing policy; missing metadata is denied.
    p.role == "owner"
        || acl["workspace_readers"] == true
        || acl["credentials"]
            .as_array()
            .is_some_and(|v| v.iter().any(|x| x.as_str() == Some(&p.credential)))
}
pub fn state(documents: &[Value]) -> String {
    digest(&json!(
        documents
            .iter()
            .map(|d| json!({"id":d["id"],"sha256":d["sha256"],"version":d["version"]}))
            .collect::<Vec<_>>()
    ))
}
pub fn reviewed_state(db: &Connection, repository: &Value, p: &Principal) -> Result<String> {
    reviewed_state_for(db, repository, &list(db, repository, p, true)?)
}
/// `reviewed_state` over sources the caller already read for this principal.
pub fn reviewed_state_of(
    db: &Connection,
    repository: &Value,
    documents: &[Value],
) -> Result<String> {
    reviewed_state_for(db, repository, &list_of(db, repository, documents, true)?)
}
pub fn reviewed_state_for(
    db: &Connection,
    repository: &Value,
    memories: &[Value],
) -> Result<String> {
    let visible: BTreeSet<_> = memories
        .iter()
        .filter(|m| m["state"] == "reviewed" && m["stale"] != true)
        .map(|m| s(m, "id"))
        .collect();
    // Identity review edits graph links without editing memory text/revisions.
    // Include visible links so cached and in-flight answers observe that change.
    let links: Vec<_> = rows(db,"SELECT l.claim_id,l.subject_id,l.object_id,c.memory_id FROM am_entity_links l JOIN am_claims c ON c.id=l.claim_id AND c.repository_id=l.repository_id WHERE l.repository_id=? ORDER BY l.claim_id,l.subject_id,l.object_id",[s(repository,"id")])?
        .into_iter().filter(|l|visible.contains(s(l,"memory_id"))).collect();
    Ok(digest(&json!({"memories":memories.iter()
        .filter(|m| m["state"] == "reviewed" || m["state"] == "retired")
        .map(|m|json!([m["id"],m["revision"],m["state"],m["stale"]])).collect::<Vec<_>>(),"entity_links":links})))
}
/// Check every citation against the current authorized sources and record
/// what the memory depends on: repository files and workspace links by
/// sha256, connector documents by sha256. Every memory producer passes
/// through here, so this is also where quotes resolve to the code symbols
/// that contain them ([`crate::symbols::anchor`]).
pub fn dependencies(
    db: &Connection,
    repository: &str,
    citations: &[Value],
    documents: &[Value],
) -> Result<Value> {
    let mut files = BTreeMap::new();
    let mut links = BTreeMap::new();
    let mut native_sources = BTreeMap::new();
    for quote in citations {
        let identifier = string(quote, "id", 1500)?;
        let source = documents
            .iter()
            .find(|x| s(x, "id") == identifier)
            .ok_or_else(|| Error::bad("Citation is outside the current authorized source set"))?;
        let text = string(quote, "quote", 12000)?;
        if !s(source, "content").contains(text) {
            return Err(Error::bad(
                "Citation quote is absent from the original source",
            ));
        }
        if s(source, "source_kind") == "repository_file" {
            files.insert(s(source, "path").to_string(), source["sha256"].clone());
        } else if s(source, "source_kind") == "workspace_source" {
            links.insert(
                identifier.to_string(),
                json!({"sha256":source["sha256"],"version":source["version"]}),
            );
        } else {
            native_sources.insert(identifier.to_string(), source["sha256"].clone());
        }
    }
    let mut data = json!({"dependencies":files,"evidence_links":links,"native_sources":native_sources,"evidence":citations});
    crate::symbols::anchor(db, repository, citations, documents, &mut data)?;
    Ok(data)
}

/// Record all originals consulted by a model. Source windows are often larger
/// than a semantic citation; split their provenance into bounded exact quotes
/// without truncating context or relaxing the public citation validator.
pub fn source_dependencies(
    db: &Connection,
    repository: &str,
    windows: &[Value],
    documents: &[Value],
) -> Result<Value> {
    let mut citations = Vec::new();
    for window in windows {
        let identifier = string(window, "id", 1500)?;
        let original = documents
            .iter()
            .find(|doc| s(doc, "id") == identifier)
            .ok_or_else(|| {
                Error::bad("Source window is outside the current authorized source set")
            })?;
        let text = window["text"]
            .as_str()
            .ok_or_else(|| Error::bad("Source window needs original text"))?;
        if !s(original, "content").contains(text) {
            return Err(Error::bad("Source window is absent from its original"));
        }
        let mut start = 0;
        while start < text.len() {
            let mut end = (start + 12000).min(text.len());
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            let quote = &text[start..end];
            if !quote.trim().is_empty() {
                citations.push(json!({"id":identifier,"quote":quote}));
            }
            start = end;
        }
    }
    dependencies(db, repository, &citations, documents)
}
/// Lookups shared by every record of one listing: current document hashes,
/// memory records by id, and what the symbol links of changed files need.
/// Building them once keeps a listing linear.
struct Lookup<'a> {
    live: BTreeMap<&'a str, &'a str>,
    memories: BTreeMap<&'a str, &'a Value>,
    symbols: crate::symbols::Check,
}

impl<'a> Lookup<'a> {
    /// `extra` is a record checked beside the listing (`reasons`'s root).
    fn new(
        db: &Connection,
        repository: &str,
        documents: &'a [Value],
        memories: &'a [Value],
        extra: Option<(Option<&str>, &Value)>,
    ) -> Result<Self> {
        let live: BTreeMap<&str, &str> = documents
            .iter()
            .map(|d| (s(d, "id"), s(d, "sha256")))
            .collect();
        let mut records: Vec<(Option<&str>, &Value)> = memories
            .iter()
            .map(|m| (Some(s(m, "id")), &m["data"]))
            .collect();
        records.extend(extra);
        let symbols = crate::symbols::Check::prepare(db, repository, &records, &live, documents)?;
        Ok(Self {
            live,
            memories: memories.iter().map(|m| (s(m, "id"), m)).collect(),
            symbols,
        })
    }

    /// Reasons a record's own data withholds it: a changed or unavailable
    /// source, or a refresh target that moved on. A changed file whose
    /// grounds all lie in code symbols is judged by those symbols instead:
    /// `symbol_changed:<id>`, `symbol_missing:<id>` or
    /// `projection_pending:…`, and nothing while they are all current.
    fn direct(&self, id: Option<&str>, data: &Value) -> Vec<String> {
        let mut reasons = Vec::new();
        for field in ["dependencies", "evidence_links", "native_sources"] {
            if let Some(deps) = data[field].as_object() {
                for (key, value) in deps {
                    let identifier = if field == "dependencies" {
                        format!("file:{key}")
                    } else {
                        key.clone()
                    };
                    let expected = if field == "evidence_links" {
                        s(value, "sha256")
                    } else {
                        value.as_str().unwrap_or("")
                    };
                    if self.live.get(identifier.as_str()).copied() != Some(expected) {
                        if field == "dependencies"
                            && let Some(outcome) = self.symbols.file(id, data, key)
                        {
                            reasons.extend(outcome.reasons);
                            continue;
                        }
                        reasons.push(format!("source_changed_or_unavailable:{identifier}"));
                    }
                }
            }
        }
        if let Some(target) = data.get("refresh_target")
            && !target["id"]
                .as_str()
                .and_then(|id| self.memories.get(id))
                .is_some_and(|m| m["revision"] == target["revision"] && m["state"] != "retired")
        {
            reasons.push("refresh_target_changed".into());
        }
        reasons
    }

    fn node(&self, item: &Value) -> Value {
        json!({"id":item["id"],"revision":item["revision"],"state":item["state"],
            "parents":item["data"].get("notes").cloned().unwrap_or(json!({})),"reasons":self.direct(Some(s(item, "id")), &item["data"])})
    }
}

/// Why `data` (a revision of memory `memory_id`, when it has one) would be
/// withheld against the current sources and `memories`.
pub fn reasons(
    db: &Connection,
    repository: &str,
    memory_id: Option<&str>,
    data: &Value,
    documents: &[Value],
    memories: &[Value],
) -> Result<Vec<String>> {
    let lookup = Lookup::new(db, repository, documents, memories, Some((memory_id, data)))?;
    let mut nodes = Vec::with_capacity(memories.len() + 1);
    nodes.push(json!({"id":"root","revision":0,"state":"reviewed","parents":data.get("notes").cloned().unwrap_or(json!({})),"reasons":lookup.direct(memory_id, data)}));
    nodes.extend(memories.iter().map(|item| lookup.node(item)));
    Ok(serde_json::from_value(
        native(
            "lifecycle.dependencies",
            json!({"root":"root","nodes":nodes}),
        )?["reasons"]
            .clone(),
    )?)
}
pub fn list(
    db: &Connection,
    repository: &Value,
    p: &Principal,
    include_retired: bool,
) -> Result<Vec<Value>> {
    list_of(
        db,
        repository,
        &documents(db, repository, p)?,
        include_retired,
    )
}
/// `list` over the sources one principal can read (`documents`).
pub fn list_of(
    db: &Connection,
    repository: &Value,
    documents: &[Value],
    include_retired: bool,
) -> Result<Vec<Value>> {
    let mut values = rows(
        db,
        "SELECT * FROM repo_memories WHERE repository_id=? ORDER BY updated_at DESC,id",
        [s(repository, "id")],
    )?;
    if values.len() > graphicle_memory::lifecycle::MAX_MEMORIES {
        return Err(Error::bad(format!(
            "Memory count exceeds the {}-record listing bound",
            graphicle_memory::lifecycle::MAX_MEMORIES
        )));
    }
    for row in &mut values {
        decode(row, "data")?;
    }
    let original = values.clone();
    let lookup = Lookup::new(db, s(repository, "id"), documents, &original, None)?;
    // Every record's reasons in one pass; each equals `reasons` for that record.
    let nodes: Vec<Value> = original.iter().map(|item| lookup.node(item)).collect();
    let batched = native("lifecycle.dependencies_all", json!({"nodes":nodes}))?;
    let allowed: BTreeSet<&str> = lookup.live.keys().copied().collect();
    for row in &mut values {
        // A user who cannot read a dependency cannot read its derived memory.
        let problems: Vec<String> =
            serde_json::from_value(batched["reasons"][s(row, "id")].clone())?;
        row["stale"] = json!(!problems.is_empty());
        row["stale_reasons"] = json!(problems);
        row["authority"] = row["data"]
            .get("authority")
            .cloned()
            .unwrap_or(json!("member_assertion"));
        // Only a memory with symbol links has this field.
        if let Some(summary) =
            lookup
                .symbols
                .summary(Some(s(row, "id")), &row["data"], &lookup.live)
        {
            row["symbol_links"] = summary;
        }
    }
    values.retain(|v| {
        (include_retired || v["state"] != "retired")
            && accessible_with(&v["data"], &allowed, &lookup.memories)
    });
    Ok(values)
}
pub fn accessible(data: &Value, documents: &[Value], memories: &[Value]) -> bool {
    let allowed: BTreeSet<_> = documents.iter().map(|d| s(d, "id")).collect();
    let by_id: BTreeMap<_, _> = memories.iter().map(|m| (s(m, "id"), m)).collect();
    accessible_with(data, &allowed, &by_id)
}
fn accessible_with(data: &Value, allowed: &BTreeSet<&str>, by_id: &BTreeMap<&str, &Value>) -> bool {
    let mut pending = vec![data];
    let mut visited = BTreeSet::new();
    while let Some(data) = pending.pop() {
        if data["native_sources"]
            .as_object()
            .is_some_and(|v| v.keys().any(|id| !allowed.contains(id.as_str())))
        {
            return false;
        }
        let mut parents: Vec<&str> = data["notes"]
            .as_object()
            .into_iter()
            .flat_map(|v| v.keys())
            .map(String::as_str)
            .collect();
        if let Some(id) = data["historical_target"]["memory_id"].as_str() {
            parents.push(id);
        }
        for id in parents {
            if visited.insert(id) {
                let Some(parent) = by_id.get(id) else {
                    return false;
                };
                pending.push(&parent["data"]);
            }
        }
    }
    true
}
// The arguments mirror the shared immutable revision record.
#[allow(clippy::too_many_arguments)]
pub fn save(
    db: &Connection,
    repository: &str,
    identifier: Option<&str>,
    expected: Option<i64>,
    kind: &str,
    title: &str,
    body: &str,
    state: &str,
    data: &Value,
) -> Result<Value> {
    if title.trim().is_empty()
        || title.len() > 500
        || body.trim().is_empty()
        || body.len() > 64000
        || !["draft", "reviewed", "retired"].contains(&state)
    {
        return Err(Error::bad("Invalid memory title, body or state"));
    }
    let identifier = identifier.map(str::to_string).unwrap_or_else(id);
    let old = one(
        db,
        "SELECT * FROM repo_memories WHERE repository_id=? AND id=?",
        params![repository, identifier],
    )?;
    if let Some(ref row) = old {
        if expected != row["revision"].as_i64() {
            return Err(Error::conflict(
                "Memory changed; review its current revision",
            ));
        }
    } else if expected.is_some() {
        return Err(Error::conflict("Memory no longer exists"));
    }
    let revision = old
        .as_ref()
        .and_then(|r| r["revision"].as_i64())
        .unwrap_or(0)
        + 1;
    let stamp = now();
    let created = old
        .as_ref()
        .map(|r| s(r, "created_at").to_string())
        .unwrap_or(stamp.clone());
    let written = db.execute("INSERT INTO repo_memories VALUES(?,?,?,?,?,?,?,?,?,?) ON CONFLICT(id) DO UPDATE SET kind=excluded.kind,revision=excluded.revision,state=excluded.state,title=excluded.title,body=excluded.body,data=excluded.data,updated_at=excluded.updated_at WHERE repo_memories.repository_id=excluded.repository_id",params![identifier,repository,kind,revision,state,title,body,data.to_string(),created,stamp])?;
    if written != 1 {
        return Err(Error::conflict(
            "Memory identifier unavailable in this repository",
        ));
    }
    let raw = json!({"id":identifier,"repository_id":repository,"kind":kind,"revision":revision,"state":state,"title":title,"body":body,"data":data.to_string(),"created_at":created,"updated_at":stamp});
    db.execute(
        "INSERT INTO repo_memory_revisions VALUES(?,?,?,?)",
        params![identifier, revision, raw.to_string(), stamp],
    )?;
    store::invalidate(db, repository)?;
    let workspace = crate::invalidation::workspace_of(db, repository)?;
    crate::invalidation::record_memory(db, &workspace, repository, &identifier, data)?;
    // Quotes written before their code was projected wait to
    // be pinned to their symbols.
    crate::symbols::record_pins(db, repository, &identifier, data)?;
    store::event(
        db,
        repository,
        "memory_saved",
        json!({"id":identifier,"revision":revision,"state":state}),
    )?;
    let mut value = raw;
    value["data"] = data.clone();
    Ok(value)
}
pub fn read_source(doc: &Value, args: &Value) -> Result<Value> {
    let text = s(doc, "content");
    let lines: Vec<_> = text.split_inclusive('\n').collect();
    let requested = integer(args, "start_line", 1, usize::MAX)?.max(1);
    let count = integer(args, "line_count", 60, 400)?.max(1);
    let start = if let Some(byte) = args.get("start_byte").filter(|v| !v.is_null()) {
        byte.as_u64()
            .and_then(|v| usize::try_from(v).ok())
            .filter(|v| *v <= text.len() && text.is_char_boundary(*v))
            .ok_or_else(|| Error::bad("Invalid UTF-8 byte offset"))?
    } else {
        if requested > lines.len().max(1) {
            return Err(Error::bad("Line outside source"));
        }
        lines.iter().take(requested - 1).map(|s| s.len()).sum()
    };
    let line = text[..start].bytes().filter(|b| *b == b'\n').count() + 1;
    let selected = text[start..]
        .split_inclusive('\n')
        .take(count)
        .collect::<String>();
    let mut end = start + selected.len().min(32000);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let quote = &text[start..end];
    let mut result = doc.clone();
    result.as_object_mut().unwrap().remove("content");
    for (key,value) in json!({"line":line,"end_line":line+quote.lines().count().saturating_sub(1),"byte_start":start,"byte_end":end,"quote":quote,"total_lines":lines.len(),"total_bytes":text.len(),"complete":start==0 && end==text.len(),"truncated":end<text.len(),"next_byte":if end<text.len(){Some(end)}else{None}}).as_object().unwrap(){result[key]=value.clone();}
    Ok(result)
}

impl Store {
    pub fn note(&self, workspace: &str, token: &str, args: &Value) -> Result<Value> {
        self.authorized(workspace, token, "approve", |db, p| {
            let repository = repo(db, p, string(args, "repository_id", 100)?)?;
            let docs = documents(db, &repository, p)?;
            let paths = args["dependencies"].as_array().cloned().unwrap_or_default();
            let mut files = BTreeMap::new();
            for value in &paths {
                let path = value
                    .as_str()
                    .ok_or_else(|| Error::bad("Expected dependency paths"))?;
                let source = docs
                    .iter()
                    .find(|s| s["id"] == format!("file:{path}"))
                    .ok_or_else(|| Error::bad("Dependency not in current sources"))?;
                files.insert(path.to_string(), source["sha256"].clone());
            }
            let evidence = args["evidence"].as_array().cloned().unwrap_or_default();
            let identifier = s(&repository, "id");
            let mut data = dependencies(db, identifier, &evidence, &docs)?;
            // Symbols named by stable id, or by path and
            // qualified name; files named as a whole keep the file rule.
            let named = match args.get("symbols") {
                None | Some(Value::Null) => Vec::new(),
                Some(Value::Array(items)) => items.clone(),
                Some(_) => return Err(Error::bad("symbols must be a list")),
            };
            crate::symbols::name(db, identifier, &named, &docs, &mut data)?;
            let named_files: Vec<String> = files.keys().cloned().collect();
            data["dependencies"].as_object_mut().unwrap().extend(files);
            crate::symbols::name_files(&mut data, &named_files);
            data["notes"] = json!({});
            data["authority"] = json!("member_reviewed_statement");
            data["maintainer"] = json!(args["maintainer"].as_str().unwrap_or("Member"));
            crate::structured_memory::attach(db, s(&repository, "id"), args, &mut data)?;
            let replaced = if args.get("supersedes").is_some() {
                crate::structured_memory::replacement(
                    args,
                    &mut data,
                    &list_of(db, &repository, &docs, true)?,
                )?
            } else {
                None
            };
            let result = save(
                db,
                s(&repository, "id"),
                args["memory_id"].as_str(),
                args["expected_revision"].as_i64(),
                args["kind"].as_str().unwrap_or("note"),
                string(args, "title", 500)?,
                string(args, "body", 64000)?,
                "reviewed",
                &data,
            )?;
            if let Some(old) = replaced {
                crate::structured_memory::close_replaced(db, s(&repository, "id"), &old, &result)?;
            }
            Ok(result)
        })
    }
    pub fn refresh_context(&self, workspace: &str, token: &str, args: &Value) -> Result<Value> {
        self.authorized(workspace, token, "read", |db, p| {
            let repository = repo(db, p, string(args, "repository_id", 100)?)?;
            prepare_refresh(db, &repository, p, string(args, "memory_id", 100)?)
        })
    }
    pub fn review_memory(&self, workspace: &str, token: &str, args: &Value) -> Result<Value> {
        self.authorized(workspace, token, "approve", |db, p| {
            let repository = repo(db, p, string(args, "repository_id", 100)?)?;
            apply_review(db, &repository, p, args, None)
        })
    }
}

/// Both member review and explicitly delegated model review use the same
/// freshness, optimistic revision and claim-transition checks. The provenance
/// of a model review is supplied internally, never accepted from API arguments.
pub(crate) fn apply_review(
    db: &Connection,
    repository: &Value,
    p: &Principal,
    args: &Value,
    model_review: Option<&Value>,
) -> Result<Value> {
    if !crate::store::permitted(db, p, "approve")? {
        return Err(Error::denied());
    }
    let identifier = s(repository, "id");
    let memories = list(db, repository, p, true)?;
    let old = memories
        .iter()
        .find(|m| s(m, "id") == s(args, "memory_id"))
        .ok_or_else(|| Error::bad("Memory unavailable"))?;
    if old["revision"] != args["expected_revision"] || old["state"] != "draft" {
        return Err(Error::conflict("Select the current draft revision"));
    }
    let decision = string(args, "decision", 20)?;
    if !["approve", "reject"].contains(&decision) {
        return Err(Error::bad("Approve or reject a draft"));
    }
    if decision == "approve" && old["stale"] == true {
        return Err(Error::conflict(
            "Draft sources changed; regenerate before approval",
        ));
    }
    let mut data = old["data"].clone();
    data["review"] = json!({"maintainer":string(args,"maintainer",100)?,"reason":string(args,"reason",2000)?,"decision":decision,"at":now(),"method":model_review.unwrap_or(&json!({"kind":"member"})).clone()});
    if let Some(method) = model_review {
        for field in ["dependencies", "evidence_links", "native_sources"] {
            if let Some(deps) = method["source_dependencies"][field].as_object() {
                data[field].as_object_mut().unwrap().extend(deps.clone());
            }
        }
    }
    let state = if decision == "approve" {
        "reviewed"
    } else {
        "retired"
    };
    if decision == "approve" {
        data["authority"] = json!(if model_review.is_some() {
            "model_reviewed_ai_interpretation"
        } else {
            "member_reviewed_ai_interpretation"
        });
    }
    let target = data.get("refresh_target").cloned();
    let result = if let Some(target) = target.filter(|_| decision == "approve") {
        data.as_object_mut().unwrap().remove("refresh_target");
        let current = memories
            .iter()
            .find(|m| m["id"] == target["id"])
            .ok_or_else(|| Error::conflict("Refresh target was removed"))?;
        if current["state"] == "retired" || current["revision"] != target["revision"] {
            return Err(Error::conflict("Refresh target changed"));
        }
        let result = save(
            db,
            identifier,
            Some(s(current, "id")),
            current["revision"].as_i64(),
            s(current, "kind"),
            s(old, "title"),
            s(old, "body"),
            state,
            &data,
        )?;
        let mut retired = old["data"].clone();
        retired["applied_to"] = json!(s(current, "id"));
        save(
            db,
            identifier,
            Some(s(old, "id")),
            old["revision"].as_i64(),
            s(old, "kind"),
            s(old, "title"),
            s(old, "body"),
            "retired",
            &retired,
        )?;
        result
    } else {
        save(
            db,
            identifier,
            Some(s(old, "id")),
            old["revision"].as_i64(),
            s(old, "kind"),
            s(old, "title"),
            s(old, "body"),
            state,
            &data,
        )?
    };
    crate::claims::apply_memory_review(db, identifier, s(old, "id"), s(&result, "id"), decision)?;
    Ok(result)
}

pub fn prepare_refresh(
    db: &Connection,
    repository: &Value,
    p: &Principal,
    memory_id: &str,
) -> Result<Value> {
    let memories = list(db, repository, p, true)?;
    let old = memories
        .iter()
        .find(|m| s(m, "id") == memory_id && m["state"] != "retired")
        .ok_or_else(|| Error::bad("Memory unavailable"))?;
    let docs = documents(db, repository, p)?;
    let all_state = state(&docs);
    let mut paths: BTreeSet<String> = old["data"]["dependencies"]
        .as_object()
        .map(|v| v.keys().map(|p| format!("file:{p}")).collect())
        .unwrap_or_default();
    // Where a moved symbol's bytes are now.
    if let Some(locations) = old["symbol_links"]["locations"].as_object() {
        for found in locations
            .values()
            .flat_map(|v| v.as_array().into_iter().flatten())
        {
            paths.insert(s(found, "id").to_string());
        }
    }
    let mut selected = Vec::new();
    let mut used = 0;
    for doc in &docs {
        if paths.contains(s(doc, "id"))
            || old["data"]["native_sources"].get(s(doc, "id")).is_some()
            || old["data"]["evidence_links"].get(s(doc, "id")).is_some()
        {
            let value = graphicle_memory::text::prefix(s(doc, "content"), 2000, 12000);
            let tokens = graphicle_memory::text::count(&value);
            if used + tokens > 12000 {
                continue;
            }
            used += tokens;
            selected.push(json!({"id":doc["id"],"sha256":doc["sha256"],"text":value,"recorded_at":doc["recorded_at"],"truncated":value!=s(doc,"content")}));
        }
    }
    let mut context = json!({"memory_id":memory_id,"revision":old["revision"],"source_state":all_state,"snapshot":repository["snapshot"],
        "title":old["title"],"original_markdown":old["body"],"document":native("document.split",json!({"markdown":old["body"]}))?["document"],
        "sources":selected,"stale_reasons":old["stale_reasons"],"reference_tokens":used,
        "instructions":"Update only supported blocks. Preserve attribution, prerequisites, limits and unaffected text. Return operations and exact source citations. Do not replace missing information with a guess."});
    context["sha256"] = json!(digest(&context));
    Ok(
        json!({"context":context,"operation_schema":native("document.contract",json!({}))?,"approval_required":true}),
    )
}
pub fn propose_refresh(
    db: &Connection,
    repository: &Value,
    p: &Principal,
    args: &Value,
) -> Result<Value> {
    let prepared = prepare_refresh(db, repository, p, string(args, "memory_id", 100)?)?;
    let context = &prepared["context"];
    if context["sha256"] != args["context_sha256"] {
        return Err(Error::conflict("Refresh context changed"));
    }
    let preview = native(
        "document.delta",
        json!({"document":context["document"],"operations":args["operations"]}),
    )?;
    if preview["skipped"].as_array().is_none_or(|v| !v.is_empty()) {
        let reasons: Vec<&str> = preview["skipped"]
            .as_array()
            .into_iter()
            .flatten()
            .take(3)
            .filter_map(|v| v["reason"].as_str())
            .collect();
        let mut message = format!("Invalid document delta targets: {}", reasons.join("; "));
        if message.len() > 500 {
            let mut end = 500;
            while !message.is_char_boundary(end) {
                end -= 1;
            }
            message.truncate(end);
        }
        return Err(Error::bad(message));
    }
    if preview["changed"] != true {
        return Ok(json!({"drafts":[],"changed":false}));
    }
    let docs = documents(db, repository, p)?;
    let citations = args["evidence"]
        .as_array()
        .filter(|v| !v.is_empty() && v.len() <= 12)
        .ok_or_else(|| Error::bad("Refresh needs 1–12 citations"))?;
    // Require the exact excerpts given to the model, not a second hidden source set.
    for citation in citations {
        if !context["sources"]
            .as_array()
            .unwrap()
            .iter()
            .any(|src| src["id"] == citation["id"] && s(src, "text").contains(s(citation, "quote")))
        {
            return Err(Error::bad(
                "Refresh citation is absent from prepared context",
            ));
        }
    }
    let mut data = dependencies(db, s(repository, "id"), citations, &docs)?;
    data["notes"] = json!({});
    data["authority"] = json!("unreviewed_ai_draft");
    data["refresh_target"] = json!({"id":context["memory_id"],"revision":context["revision"]});
    data["candidate"] = json!({"evidence":citations,"title":context["title"]});
    data["warnings"] =
        json!(["Review the changes against current sources. Earlier revisions remain available."]);
    let old = list(db, repository, p, false)?
        .into_iter()
        .find(|m| m["id"] == context["memory_id"])
        .ok_or_else(|| Error::conflict("Memory disappeared"))?;
    // Unchanged blocks keep every original dependency, including missing ones.
    let kept: BTreeSet<String> = old["data"]["dependencies"]
        .as_object()
        .into_iter()
        .flat_map(|before| before.keys())
        .filter(|path| data["dependencies"].get(path.as_str()).is_none())
        .cloned()
        .collect();
    for field in ["dependencies", "evidence_links", "native_sources"] {
        if let Some(before) = old["data"][field].as_object() {
            for (key, value) in before {
                data[field]
                    .as_object_mut()
                    .unwrap()
                    .entry(key.clone())
                    .or_insert(value.clone());
            }
        }
    }
    // And the symbol links of the files it keeps.
    crate::symbols::carry(&mut data, &old["data"], &kept);
    data["notes"] = old["data"].get("notes").cloned().unwrap_or(json!({}));
    let draft = save(
        db,
        s(repository, "id"),
        None,
        None,
        s(&old, "kind"),
        s(context, "title"),
        s(&preview, "markdown"),
        "draft",
        &data,
    )?;
    Ok(json!({"drafts":[draft],"approval_required":true}))
}
