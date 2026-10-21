//! Read-only source adapters with resumable staging and atomic activation.
use crate::{Error, Result, digest, id, integer, now, string};
use crate::{
    jobs::{self, Job, Worker},
    memory, model,
    store::{self, Store, decode, one, repo, rows, s},
};
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

/// Why a connector in `permission_mode: provider` cannot sync in this
/// program: no installed extension syncs provider permissions. Its documents
/// stay withheld once their last check is older than the connector's
/// `permission_max_age_seconds` (enforcement on read fails closed).
pub const PROVIDER_SYNC_UNAVAILABLE: &str =
    "Provider permission sync is not part of Graphicle Core";

/// Whether this program syncs provider permissions, as an installed
/// extension declares with the `provider_permission_sync` capability (it
/// then serves `Extension::source_permissions`, `schedule` and the
/// `permissions` job). Enforcement on read never depends on it.
pub fn provider_permission_sync() -> bool {
    crate::core_capabilities::capabilities()["provider_permission_sync"] == true
}

/// The connector kinds of the core. An installed extension may add others
/// (`extension::Source`).
pub const CORE_KINDS: &[&str] = &[
    "github",
    "slack",
    "notion",
    "gdrive",
    "jira",
    "confluence",
    "jsonl",
];

/// The core kinds whose provider ACL an extension can sync without naming
/// them: GitHub, Slack and Drive.
const PROVIDER_ACL_KINDS: &[&str] = &["github", "slack", "gdrive"];

/// The connector kinds whose provider permissions an installed extension
/// syncs: its `provider_permission_kinds` capability, or GitHub, Slack and
/// Drive for one that only sets `provider_permission_sync`. Empty in Core.
pub fn provider_permission_kinds() -> Vec<String> {
    let capabilities = crate::core_capabilities::capabilities();
    if capabilities["provider_permission_sync"] != true {
        return Vec::new();
    }
    match capabilities["provider_permission_kinds"].as_array() {
        Some(kinds) => kinds
            .iter()
            .filter_map(|kind| kind.as_str().map(str::to_string))
            .collect(),
        None => PROVIDER_ACL_KINDS
            .iter()
            .map(|kind| kind.to_string())
            .collect(),
    }
}

/// The provider of a connector kind's connected accounts and verified
/// identities: Atlassian for Jira and Confluence, the declared OAuth
/// provider for a kind an extension adds, else the kind itself.
pub fn identity_provider(kind: &str) -> &str {
    match kind {
        "jira" | "confluence" => "atlassian",
        _ => crate::extension::installed()
            .source(kind)
            .map_or(kind, |source| source.oauth_provider()),
    }
}

/// The kinds whose identity provider is not the kind itself, as a JSON object
/// of kind to provider (for joins in SQL).
pub fn identity_providers() -> Value {
    let mut providers = json!({"jira":"atlassian","confluence":"atlassian"});
    for source in crate::extension::installed().sources() {
        providers[source.kind()] = json!(source.oauth_provider());
    }
    providers
}

fn validate_acl(db: &Connection, workspace: &str, acl: &Value) -> Result<()> {
    if !acl.is_object()
        || !acl["workspace_readers"].is_boolean()
        || !acl["credentials"].is_array()
        || acl
            .as_object()
            .unwrap()
            .keys()
            .any(|k| !["workspace_readers", "credentials"].contains(&k.as_str()))
    {
        return Err(Error::bad(
            "Set workspace_readers and an explicit credentials list",
        ));
    }
    let ids = acl["credentials"].as_array().unwrap();
    if ids.len() > 200 {
        return Err(Error::bad("Too many source readers"));
    }
    for id in ids {
        if one(
            db,
            "SELECT id FROM credentials WHERE workspace_id=? AND id=? AND revoked_at IS NULL",
            params![
                workspace,
                id.as_str().ok_or_else(|| Error::bad("Invalid reader ID"))?
            ],
        )?
        .is_none()
        {
            return Err(Error::bad(
                "Source reader is not an active workspace credential",
            ));
        }
    }
    Ok(())
}
fn validate_config(kind: &str, config: &Value) -> Result<()> {
    let mut allowed = match kind {
        "github" => vec![
            "repository",
            "branch",
            "include_issues",
            "include_files",
            "acl",
            "max_age_seconds",
        ],
        "slack" => vec!["channels", "acl", "max_age_seconds"],
        "notion" => vec!["page_ids", "acl", "max_age_seconds"],
        "gdrive" => vec!["folder_id", "file_ids", "acl", "max_age_seconds"],
        "jira" => vec![
            "base_url",
            "site_url",
            "auth_type",
            "jql",
            "acl",
            "max_age_seconds",
        ],
        "confluence" => vec![
            "base_url",
            "site_url",
            "auth_type",
            "space_id",
            "space_key",
            "acl",
            "max_age_seconds",
        ],
        "jsonl" => vec!["acl", "max_age_seconds"],
        _ => match crate::extension::installed().source(kind) {
            Some(source) => [&["acl", "max_age_seconds"], source.fields()].concat(),
            None => return Err(Error::bad("Unsupported source connector")),
        },
    };
    allowed.push("observe");
    if let Some(policy) = config.get("observe") {
        crate::auto_observe::validate(policy)?;
    }
    if kind != "jsonl" {
        allowed.push("sync_every_seconds");
        allowed.extend(["permission_mode", "permission_max_age_seconds"]);
    }
    if kind == "gdrive" {
        allowed.push("recursive");
        allowed.push("resolve_groups");
    }
    let permission_mode = config["permission_mode"].as_str().unwrap_or("explicit");
    if !["explicit", "provider"].contains(&permission_mode) {
        return Err(Error::bad("Choose explicit or provider permission mode"));
    }
    let synced = provider_permission_kinds();
    let synced = synced.iter().any(|synced| synced == kind);
    if permission_mode == "provider" && !PROVIDER_ACL_KINDS.contains(&kind) && !synced {
        return Err(Error::bad(
            "This provider does not expose the required source ACL API; use explicit sharing",
        ));
    }
    if permission_mode == "provider" && !synced {
        return Err(Error::bad(
            "Provider permission mode is not part of Graphicle Core; use explicit sharing",
        ));
    }
    integer(config, "permission_max_age_seconds", 300, 3600)?;
    let interval = integer(config, "sync_every_seconds", 0, 604800)?;
    if interval > 0 && interval < 300 {
        return Err(Error::bad(
            "Source sync interval must be at least five minutes",
        ));
    }
    if !config.is_object()
        || config
            .as_object()
            .unwrap()
            .keys()
            .any(|k| !allowed.contains(&k.as_str()))
    {
        return Err(Error::bad("Unknown connector configuration field"));
    }
    integer(config, "max_age_seconds", 86400, 604800)?;
    if ["jira", "confluence"].contains(&kind) {
        crate::atlassian::validate(kind, config)?;
        if let Some(site) = config["site_url"].as_str() {
            model::checked_url(site)?;
        }
    }
    if let Some(source) = crate::extension::installed().source(kind) {
        source.validate(config)?;
    }
    if kind == "github" {
        let name = string(config, "repository", 200)?;
        if name.split('/').count() != 2
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-._/".contains(&b))
        {
            return Err(Error::bad("Use GitHub owner/repository"));
        }
    }
    for field in ["channels", "page_ids", "file_ids"] {
        if let Some(items) = config.get(field)
            && items.as_array().is_none_or(|v| {
                v.len() > 500
                    || v.iter().any(|v| {
                        v.as_str().is_none_or(|s| {
                            s.is_empty()
                                || s.len() > 200
                                || !s
                                    .bytes()
                                    .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
                        })
                    })
            })
        {
            return Err(Error::bad(format!("Invalid {field}")));
        }
    }
    if kind == "slack" && config["channels"].as_array().is_none_or(Vec::is_empty) {
        return Err(Error::bad("Select Slack channel IDs explicitly"));
    }
    if kind == "notion" && config["page_ids"].as_array().is_none_or(Vec::is_empty) {
        return Err(Error::bad("Select Notion page IDs explicitly"));
    }
    if kind == "gdrive"
        && config["folder_id"].as_str().is_none()
        && config["file_ids"].as_array().is_none_or(Vec::is_empty)
    {
        return Err(Error::bad("Select Drive file IDs or a folder"));
    }
    Ok(())
}
impl Store {
    pub fn schedule_connectors(&self) -> Result<usize> {
        // Extension work first (for example provider-permission refreshes).
        let scheduled = crate::extension::installed().schedule(self)?;
        let provider_sync = provider_permission_sync();
        self.transaction(|db| {
            let mut count=scheduled;
            for mut connector in rows(db,"SELECT c.*,w.capability_hash FROM am_connectors c JOIN repo_projects p ON p.id=c.repository_id JOIN workspaces w ON w.id=p.workspace_id WHERE c.state='active' AND c.kind!='jsonl'",[])? {
                decode(&mut connector,"config")?;
                let interval=connector["config"]["sync_every_seconds"].as_i64().unwrap_or(0);
                if interval<300 {continue;}
                // Without provider-permission sync such a connector cannot sync here.
                if connector["config"]["permission_mode"]=="provider" && !provider_sync {continue;}
                let recent=one(db,"SELECT state,updated_at,result FROM am_jobs WHERE repository_id=? AND kind='connector' AND json_extract(payload,'$.connector_id')=? ORDER BY created_at DESC LIMIT 1",params![s(&connector,"repository_id"),s(&connector,"id")])?;
                let waiting=connector["cursor"].as_str().is_some_and(|s|s!="null");
                if let Some(recent)=&recent {
                    if ["queued","running","retry"].contains(&s(recent,"state")){continue;}
                    let since=chrono::DateTime::parse_from_rfc3339(s(recent,"updated_at")).map(|t|chrono::Utc::now().signed_duration_since(t).num_seconds()).unwrap_or(i64::MAX);
                    let backoff=if waiting && recent["state"]=="complete" {60} else {interval};
                    if since<backoff {continue;}
                }
                if !waiting && connector["last_sync"].as_str().and_then(|v|chrono::DateTime::parse_from_rfc3339(v).ok()).is_some_and(|t|chrono::Utc::now().signed_duration_since(t).num_seconds()<interval){continue;}
                let queued:i64=db.query_row("SELECT count(*) FROM am_jobs WHERE repository_id=? AND state IN ('queued','retry','running')",[s(&connector,"repository_id")],|r|r.get(0))?;
                if queued>=100 {continue;}
                let payload=json!({"kind":"connector","repository_id":connector["repository_id"],"connector_id":connector["id"],"max_requests":64,"max_calls":0,"max_input_tokens":0,"max_output_tokens":0,"scheduled":true,"interval_seconds":interval});
                let input=digest(&json!({"payload":payload,"generation":connector["generation"],"cursor":connector["cursor"],"slot":chrono::Utc::now().timestamp()/60}));
                let stamp=now();
                count+=db.execute("INSERT OR IGNORE INTO am_jobs(id,repository_id,kind,input_hash,payload,principal_hash,state,max_attempts,not_before,created_at,updated_at) VALUES(?,?,'connector',?,?,?,'queued',1,?,?,?)",params![id(),s(&connector,"repository_id"),input,payload.to_string(),s(&connector,"capability_hash"),stamp,stamp,stamp])?;
            }
            Ok(count)
        })
    }
    pub fn connector_schedule(&self, workspace: &str, token: &str, args: &Value) -> Result<Value> {
        self.authorized(workspace,token,"admin",|db,p| {
            repo(db,p,string(args,"repository_id",100)?)?;
            let mut connector=one(db,"SELECT * FROM am_connectors WHERE repository_id=? AND id=? AND kind!='jsonl'",params![s(args,"repository_id"),string(args,"connector_id",100)?])?.ok_or_else(||Error::bad("Remote connector unavailable"))?;
            decode(&mut connector,"config")?;
            connector["config"]["sync_every_seconds"]=json!(integer(args,"sync_every_seconds",0,604800)?);
            validate_config(s(&connector,"kind"),&connector["config"])?;
            db.execute("UPDATE am_connectors SET config=?,cursor=NULL,generation=generation+1 WHERE id=?",params![connector["config"].to_string(),s(&connector,"id")])?;
            db.execute("DELETE FROM am_connector_stage WHERE connector_id=?",[s(&connector,"id")])?;
            Ok(json!({"scheduled":connector["config"]["sync_every_seconds"]!=0,"sync_every_seconds":connector["config"]["sync_every_seconds"],"automatic_model_calls":connector["config"].get("observe").is_some()}))
        })
    }
    /// Set (`observe`: a policy) or clear (`observe`: null) a connector's
    /// automatic observation policy; see `auto_observe`. The connector's
    /// sources and generation are unchanged.
    pub fn connector_observe(&self, workspace: &str, token: &str, args: &Value) -> Result<Value> {
        self.authorized(workspace,token,"admin",|db,p| {
            repo(db,p,string(args,"repository_id",100)?)?;
            let mut connector=one(db,"SELECT * FROM am_connectors WHERE repository_id=? AND id=? AND state='active'",params![s(args,"repository_id"),string(args,"connector_id",100)?])?.ok_or_else(||Error::bad("Connector unavailable"))?;
            decode(&mut connector,"config")?;
            match args.get("observe") {
                Some(Value::Null) => {connector["config"].as_object_mut().map(|c|c.remove("observe"));}
                Some(policy) => connector["config"]["observe"]=policy.clone(),
                None => return Err(Error::bad("Give an observe policy, or null to clear it")),
            }
            validate_config(s(&connector,"kind"),&connector["config"])?;
            db.execute("UPDATE am_connectors SET config=? WHERE id=?",params![connector["config"].to_string(),s(&connector,"id")])?;
            let policy=connector["config"].get("observe").cloned();
            Ok(json!({"connector_id":connector["id"],"automatic_model_calls":policy.is_some(),"observe":policy}))
        })
    }
    pub fn connector_add(&self, workspace: &str, token: &str, args: &Value) -> Result<Value> {
        self.authorized(workspace,token,"admin",|db,p| {
            let repository=repo(db,p,string(args,"repository_id",100)?)?;let kind=string(args,"kind",30)?;let name=string(args,"name",150)?;
            let config=args["config"].clone();validate_config(kind,&config)?;validate_acl(db,workspace,&config["acl"])?;
            let credential=args["credential_ref"].as_str().or(args["credential_env"].as_str());
            if kind!="jsonl" && credential.is_none(){return Err(Error::bad("Connect an account or choose a saved source credential"));}
            if credential.is_some_and(|v|v.is_empty() || v.len()>100 || !v.bytes().all(|b|b.is_ascii_alphanumeric() || b"_:-".contains(&b))) {return Err(Error::bad("Invalid credential reference"));}
            if let Some(reference)=credential.filter(|v|v.starts_with("secret:"))&& one(db,"SELECT id FROM am_secrets WHERE workspace_id=? AND id=?",params![workspace,&reference[7..]])?.is_none(){return Err(Error::denied());}
            if let Some(reference)=credential.filter(|v|v.starts_with("oauth:")) {let provider=identity_provider(kind);if one(db,"SELECT id FROM am_oauth_accounts WHERE workspace_id=? AND id=? AND provider=? AND state='active'",params![workspace,&reference[6..],provider])?.is_none(){return Err(Error::denied());}}
            let identifier=id();db.execute("INSERT INTO am_connectors(id,repository_id,name,kind,config,credential_env,state,created_at) VALUES(?,?,?,?,?,?,'active',?)",params![identifier,s(&repository,"id"),name,kind,config.to_string(),credential,now()])?;
            Ok(json!({"id":identifier,"name":name,"kind":kind,"state":"active","sharing":"explicit_workspace_policy","provider_permissions_synced":false}))
        })
    }
    pub fn connector_list(&self, workspace: &str, token: &str, args: &Value) -> Result<Value> {
        self.authorized_read(workspace,token,"admin",|db,p| {
            let repository=repo(db,p,string(args,"repository_id",100)?)?;let mut values=rows(db,"SELECT id,name,kind,state,config,cursor,generation,last_sync,error FROM am_connectors WHERE repository_id=? ORDER BY name",[s(&repository,"id")])?;
            for value in &mut values {decode(value,"config")?;let cursor:Value=value["cursor"].as_str().map(serde_json::from_str).transpose()?.unwrap_or(Value::Null);value["pending_scan"]=json!(!cursor.is_null());value["cursor"]=cursor.get("index").cloned().unwrap_or(Value::Null);
                let max_age=value["config"]["max_age_seconds"].as_i64().unwrap_or(86400).max(60);
                value["source_snapshot_expired"]=json!(value["kind"]!="jsonl" && value["last_sync"].as_str().and_then(|v|chrono::DateTime::parse_from_rfc3339(v).ok()).is_none_or(|t|chrono::Utc::now().signed_duration_since(t).num_seconds()>max_age));
                let max_permission_age=value["config"]["permission_max_age_seconds"].as_i64().unwrap_or(300);
                let mut permissions=one(db,"SELECT count(*) AS sources,coalesce(sum(CASE WHEN p.complete=1 AND p.generation=d.generation THEN 1 ELSE 0 END),0) AS checked,min(p.checked_at) AS oldest_check FROM am_connector_docs d LEFT JOIN am_source_permissions p ON p.connector_id=d.connector_id AND p.source_id=d.id WHERE d.connector_id=? AND d.present=1",[s(value,"id")])?.unwrap();
                let fresh=permissions["oldest_check"].as_str().and_then(|v|chrono::DateTime::parse_from_rfc3339(v).ok()).is_some_and(|t|chrono::Utc::now().signed_duration_since(t).num_seconds()<=max_permission_age);
                value["provider_permissions_synced"]=json!(value["config"]["permission_mode"]=="provider" && permissions["sources"].as_i64().unwrap_or(0)>0 && permissions["checked"]==permissions["sources"] && fresh);
                permissions["max_age_seconds"]=json!(max_permission_age);value["permissions"]=permissions;
                if value["config"]["permission_mode"]=="provider" && !provider_permission_sync() {value["provider_permissions_unavailable"]=json!(PROVIDER_SYNC_UNAVAILABLE);}
            }
            Ok(json!({"connectors":values}))
        })
    }
    /// `document.versions`: the stored versions of one connector document,
    /// oldest first. Visible to a caller who can read the document now; older
    /// contents are returned only to the owner, because a document's sharing
    /// can widen after an earlier version was written.
    pub fn document_versions(&self, workspace: &str, token: &str, args: &Value) -> Result<Value> {
        self.authorized_read(workspace, token, "read", |db, p| {
            let repository = repo(db, p, string(args, "repository_id", 100)?)?;
            let connector = string(args, "connector_id", 100)?;
            let document = string(args, "document_id", 500)?;
            let source = crate::invalidation::connector_document_id(connector, document);
            let visible = crate::memory::documents(db, &repository, p)?
                .iter()
                .any(|d| s(d, "id") == source);
            if !visible {
                return Err(Error::bad("Document unavailable"));
            }
            let content = p.role == "owner" && args["include_content"] == true;
            let mut versions = Vec::new();
            for mut row in rows(
                db,
                "SELECT sha256,content,metadata,created_at FROM am_connector_versions WHERE connector_id=? AND id=? ORDER BY created_at,sha256",
                params![connector, document],
            )? {
                decode(&mut row, "metadata")?;
                let mut version = json!({"sha256":row["sha256"],"created_at":row["created_at"],
                    "title":row["metadata"]["title"],"bytes":s(&row, "content").len()});
                if content {
                    version["content"] = row["content"].clone();
                }
                versions.push(version);
            }
            Ok(json!({"source_id":source,"versions":versions,"content_included":content}))
        })
    }
    pub fn connector_sharing(&self, workspace: &str, token: &str, args: &Value) -> Result<Value> {
        self.authorized(workspace, token, "admin", |db, p| {
            repo(db, p, string(args, "repository_id", 100)?)?;
            validate_acl(db, workspace, &args["acl"])?;
            let mut connector = one(
                db,
                "SELECT * FROM am_connectors WHERE repository_id=? AND id=?",
                params![s(args, "repository_id"), string(args, "connector_id", 100)?],
            )?
            .ok_or_else(|| Error::bad("Connector unavailable"))?;
            decode(&mut connector, "config")?;
            connector["config"]["acl"] = args["acl"].clone();
            db.execute(
                "UPDATE am_connectors SET config=?,cursor=NULL,generation=generation+1 WHERE id=?",
                params![connector["config"].to_string(), s(&connector, "id")],
            )?;
            db.execute(
                "UPDATE am_connector_docs SET acl=? WHERE connector_id=?",
                params![args["acl"].to_string(), s(&connector, "id")],
            )?;
            db.execute(
                "DELETE FROM am_connector_stage WHERE connector_id=?",
                [s(&connector, "id")],
            )?;
            // Cached model input and output may include now-private data.
            db.execute(
                "DELETE FROM am_model_cache WHERE repository_id=?",
                [s(args, "repository_id")],
            )?;
            store::invalidate(db, s(args, "repository_id"))?;
            let sources: Vec<String> = rows(
                db,
                "SELECT id FROM am_connector_docs WHERE connector_id=? AND present=1",
                [s(&connector, "id")],
            )?
            .iter()
            .map(|row| {
                crate::invalidation::connector_document_id(s(&connector, "id"), s(row, "id"))
            })
            .collect();
            crate::invalidation::permissions_changed(
                db,
                workspace,
                None,
                &sources,
                "connector_acl_changed",
            )?;
            Ok(json!({"updated":true}))
        })
    }
    pub fn import_documents(&self, workspace: &str, token: &str, args: &Value) -> Result<Value> {
        self.authorized(workspace,token,"collect",|db,p| {
            let repository=repo(db,p,string(args,"repository_id",100)?)?;
            let mut connector=one(db,"SELECT * FROM am_connectors WHERE repository_id=? AND id=? AND kind='jsonl' AND state='active'",params![s(&repository,"id"),string(args,"connector_id",100)?])?.ok_or_else(||Error::bad("Create a JSONL source connector first"))?;
            decode(&mut connector,"config")?;
            if !memory::acl_allows(&connector["config"]["acl"],p){return Err(Error::denied());}
            let documents=args["documents"].as_array().filter(|v|v.len()<=5000).ok_or_else(||Error::bad("Expected at most 5,000 documents"))?;
            let mode=args["mode"].as_str().unwrap_or("delta");if !["delta","snapshot"].contains(&mode){return Err(Error::bad("Use delta or snapshot import mode"));}
            let scan=id();let mut seen=BTreeSet::new();let mut skipped=0;
            for doc in documents {
                if !seen.insert(string(doc,"id",500)?){return Err(Error::bad("Duplicate imported document ID"));}
                if crate::deletion::forgotten(db,s(&connector,"id"),string(doc,"id",500)?)? {skipped+=1;continue;}
                stage(db,&connector,&scan,doc)?;
            }
            let generation=activate(db,&connector,&scan,mode=="snapshot")?;
            if let Some(removed)=args["deleted_ids"].as_array(){for doc in removed {let doc=doc.as_str().ok_or_else(||Error::bad("Invalid deleted source ID"))?;db.execute("UPDATE am_connector_docs SET present=0 WHERE connector_id=? AND id=?",params![s(&connector,"id"),doc])?;
                crate::invalidation::document_changed(db,workspace,&crate::invalidation::connector_document_id(s(&connector,"id"),doc),"connector_document_deleted")?;
                crate::observer::forget_document(db,workspace,&crate::invalidation::connector_document_id(s(&connector,"id"),doc))?;}}
            crate::doc_links::after_sync(db,s(&repository,"id"),s(&connector,"id"),p)?;
            Ok(json!({"imported":documents.len()-skipped,"forgotten_skipped":skipped,"generation":generation,"mode":mode}))
        })
    }
}
/// How every refusal of one connector document for its size or structure
/// starts: a connector sync lists such a document under `skipped` with the
/// reason and goes on to the next.
pub(crate) const TOO_LARGE: &str = "Source document too large";

/// The refusal of one connector document past the limit `name` (`limit`).
pub(crate) fn too_large(what: &str, name: &str, limit: usize) -> Error {
    let shown = if name.ends_with("_bytes") {
        crate::limits::size(limit)
    } else {
        crate::limits::grouped(limit)
    };
    Error::bad(format!(
        "{TOO_LARGE}: {what} (the limit is {shown}); raise the limit with {}",
        crate::limits::raise(name)
    ))
}

/// One connector document's text past `limits.document_text_bytes`.
fn text_too_large(what: &str, bytes: usize) -> Error {
    too_large(
        &format!("{what} has {} of text", crate::limits::size(bytes)),
        "document_text_bytes",
        crate::documents::max_text(),
    )
}

/// A listed item for people: its path, key, number, name or ID.
fn item_label(item: &Value) -> Value {
    for value in [&item["value"], item] {
        for key in ["path", "key", "number", "name", "title", "id"] {
            if let Some(found) = value.get(key).filter(|v| v.is_string() || v.is_number()) {
                return found.clone();
            }
        }
    }
    Value::Null
}

pub(crate) fn stage(db: &Connection, connector: &Value, scan: &str, doc: &Value) -> Result<()> {
    let identifier = string(doc, "id", 500)?;
    // A forgotten document stays forgotten, even if its provider still has it.
    if crate::deletion::forgotten(db, s(connector, "id"), identifier)? {
        return Ok(());
    }
    let content = doc["content"]
        .as_str()
        .filter(|v| !v.contains('\0'))
        .ok_or_else(|| {
            Error::bad(format!(
                "Source document {identifier} must be UTF-8 text without NUL characters"
            ))
        })?;
    if content.len() > crate::documents::max_text() {
        return Err(text_too_large(
            &format!("source document {identifier}"),
            content.len(),
        ));
    }
    let title = string(doc, "title", 1000)?;
    let timestamp = doc["source_timestamp"].as_str();
    if let Some(stamp) = timestamp {
        graphicle_memory::temporal::instant(stamp).map_err(Error::bad)?;
    }
    let url = doc["url"].as_str();
    if let Some(url) = url {
        let parsed = url::Url::parse(url).map_err(|_| Error::bad("Invalid source URL"))?;
        if !["http", "https"].contains(&parsed.scheme()) {
            return Err(Error::bad("Source URLs must use HTTP(S)"));
        }
    }
    let sha = graphicle_memory::text::sha256(content.as_bytes());
    let mut metadata = json!({"title":title,"source_timestamp":timestamp,"url":url,"observed_at":now(),"provider_metadata":doc.get("metadata").cloned().unwrap_or(json!({})),"provider_permissions":doc.get("provider_permissions").cloned().unwrap_or(Value::Null)});
    // Inline-code spans of the converted text, as
    // byte offsets beside it. The text itself, its sha256 and the source's
    // version are unchanged by them; the mention resolver reads them.
    if let Some(spans) = doc.get("code_spans").filter(|v| !v.is_null()) {
        let spans = crate::doc_links::parse_code_spans(spans, content)?;
        if !spans.is_empty() {
            metadata["code_spans"] = json!(spans.iter().map(|(a, b)| [a, b]).collect::<Vec<_>>());
        }
    }
    if metadata.to_string().len() > 64000 {
        return Err(Error::bad("Source metadata is too large"));
    }
    db.execute(
        "INSERT OR REPLACE INTO am_connector_stage VALUES(?,?,?,?,?,?,?)",
        params![
            s(connector, "id"),
            scan,
            identifier,
            content,
            sha,
            metadata.to_string(),
            connector["config"]["acl"].to_string()
        ],
    )?;
    let bytes:i64=db.query_row("SELECT coalesce(sum(length(CAST(content AS BLOB))),0) FROM am_connector_stage WHERE connector_id=?",[s(connector,"id")],|r|r.get(0))?;
    // Earlier builds: 64 MiB.
    let cap = crate::limits::current().connector_snapshot_bytes;
    if usize::try_from(bytes).unwrap_or(usize::MAX) > cap {
        return Err(Error::bad(format!(
            "Connector snapshot exceeds {}; narrow the source's scope, or raise the limit with {}",
            crate::limits::size(cap),
            crate::limits::raise("connector_snapshot_bytes")
        )));
    }
    Ok(())
}
pub(crate) fn activate(db: &Connection, connector: &Value, scan: &str, full: bool) -> Result<i64> {
    let identifier = s(connector, "id");
    let generation = connector["generation"].as_i64().unwrap_or(0) + 1;
    let current: i64 = db.query_row(
        "SELECT generation FROM am_connectors WHERE id=?",
        [identifier],
        |r| r.get(0),
    )?;
    if current + 1 != generation {
        return Err(Error::conflict("Another connector sync completed first"));
    }
    let workspace = crate::invalidation::workspace_of(db, s(connector, "repository_id"))?;
    for row in rows(
        db,
        "SELECT * FROM am_connector_stage WHERE connector_id=? AND scan_id=?",
        params![identifier, scan],
    )? {
        let mut previous = one(
            db,
            "SELECT sha256,metadata FROM am_connector_docs WHERE connector_id=? AND id=? AND present=1",
            params![identifier, s(&row, "id")],
        )?;
        if let Some(previous) = &mut previous {
            decode(previous, "metadata")?;
        }
        let metadata: Value = serde_json::from_str(s(&row, "metadata"))?;
        // Date, title and provider permissions affect the source version even
        // when its text is unchanged. Ignore sync-only observed_at timestamps.
        let unchanged = previous.as_ref().is_some_and(|previous| {
            previous["sha256"] == row["sha256"]
                && ["title", "source_timestamp", "provider_permissions"]
                    .iter()
                    .all(|key| previous["metadata"][key] == metadata[key])
        });
        if !unchanged {
            crate::invalidation::document_changed(
                db,
                &workspace,
                &crate::invalidation::connector_document_id(identifier, s(&row, "id")),
                "connector_document_updated",
            )?;
            crate::observer::forget_document(
                db,
                &workspace,
                &crate::invalidation::connector_document_id(identifier, s(&row, "id")),
            )?;
        }
        db.execute(
            "INSERT OR IGNORE INTO am_connector_versions VALUES(?,?,?,?,?,?)",
            params![
                identifier,
                s(&row, "id"),
                s(&row, "sha256"),
                s(&row, "content"),
                s(&row, "metadata"),
                now()
            ],
        )?;
        db.execute("INSERT INTO am_connector_docs VALUES(?,?,?,?,?,?,1,?) ON CONFLICT(connector_id,id) DO UPDATE SET content=excluded.content,sha256=excluded.sha256,metadata=excluded.metadata,acl=excluded.acl,present=1,generation=excluded.generation",params![identifier,s(&row,"id"),s(&row,"content"),s(&row,"sha256"),s(&row,"metadata"),s(&row,"acl"),generation])?;
    }
    if full {
        for row in rows(
            db,
            "SELECT id FROM am_connector_docs WHERE connector_id=? AND generation!=? AND present=1",
            params![identifier, generation],
        )? {
            crate::invalidation::document_changed(
                db,
                &workspace,
                &crate::invalidation::connector_document_id(identifier, s(&row, "id")),
                "connector_document_removed",
            )?;
            crate::observer::forget_document(
                db,
                &workspace,
                &crate::invalidation::connector_document_id(identifier, s(&row, "id")),
            )?;
        }
        db.execute(
            "UPDATE am_connector_docs SET present=0 WHERE connector_id=? AND generation!=?",
            params![identifier, generation],
        )?;
    }
    db.execute(
        "UPDATE am_connectors SET cursor=NULL,last_sync=?,generation=?,error=NULL WHERE id=?",
        params![now(), generation, identifier],
    )?;
    db.execute(
        "DELETE FROM am_connector_stage WHERE connector_id=? AND scan_id=?",
        params![identifier, scan],
    )?;
    db.execute(
        "DELETE FROM am_model_cache WHERE repository_id=?",
        [s(connector, "repository_id")],
    )?;
    store::invalidate(db, s(connector, "repository_id"))?;
    Ok(generation)
}

/// One connector job's view of its provider: the API base, the request
/// budget and the provider permissions already fetched in this run.
pub struct Remote<'a> {
    pub worker: &'a Worker,
    pub job: &'a Job,
    pub connector: &'a Value,
    pub base: String,
    pub calls: usize,
    pub limit: usize,
    pub permissions: BTreeMap<String, Value>,
}
impl Remote<'_> {
    /// One provider request within the job's budget, with the connector's
    /// credential; a JSON reply. 401 and 403 withhold the connector's
    /// documents until its next complete sync; rate limits are reported.
    pub async fn request(
        &mut self,
        method: reqwest::Method,
        path: &str,
        query: &[(String, String)],
        body: Option<Value>,
    ) -> Result<Value> {
        if self.calls >= self.limit {
            return Err(Error::bad(
                "Connector request budget reached; staged progress can be resumed",
            ));
        }
        jobs::guarded(&self.worker.store, self.job, |db, _, _| {
            jobs::renew(db, self.job)
        })?;
        self.calls += 1;
        let secret = crate::oauth::for_connector(
            &self.worker.store,
            &self.worker.client,
            self.job,
            s(self.connector, "credential_env"),
        )
        .await?;
        let endpoint = format!(
            "{}/{}",
            self.base.trim_end_matches('/'),
            path.trim_start_matches('/')
        );
        let mut request = self
            .worker
            .client
            .request(method, endpoint)
            .header("User-Agent", "Graphicle/0.4")
            .query(query);
        if ["jira", "confluence"].contains(&s(self.connector, "kind"))
            && self.connector["config"]["auth_type"] == "basic"
        {
            let credentials: Value = serde_json::from_str(&secret).map_err(|_| {
                Error::bad("Atlassian API tokens require a saved username/password credential")
            })?;
            request = request.basic_auth(
                string(&credentials, "username", 254)?,
                Some(string(&credentials, "password", 32768)?),
            );
        } else {
            request = request.bearer_auth(secret);
        }
        if self.connector["kind"] == "notion" {
            request = request.header("Notion-Version", "2025-09-03");
        }
        if self.connector["kind"] == "github" {
            request = request
                .header("X-GitHub-Api-Version", "2022-11-28")
                .header("Accept", "application/vnd.github+json");
        }
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request
            .send()
            .await
            .map_err(|_| Error::bad("Source request failed or timed out"))?;
        let status = response.status();
        let headers = response.headers();
        // An exhausted rate limit says nothing about access, so it must not
        // withhold the synced documents the way a denial does. GitHub reports
        // one as 403 (or 429) with `x-ratelimit-remaining: 0`; others send
        // `retry-after`. Every GitHub reply carries the rate headers, so only
        // an exhausted count counts.
        let rate_limited = status == reqwest::StatusCode::TOO_MANY_REQUESTS
            || (status == reqwest::StatusCode::FORBIDDEN
                && (headers.contains_key("retry-after")
                    || headers
                        .get("x-ratelimit-remaining")
                        .and_then(|v| v.to_str().ok())
                        == Some("0")));
        if rate_limited {
            // Reported, never slept on: a worker is not pinned by a server's delay.
            let wait = headers
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse::<u64>().ok())
                .or_else(|| {
                    headers
                        .get("x-ratelimit-reset")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.trim().parse::<i64>().ok())
                        .map(|reset| (reset - chrono::Utc::now().timestamp()).max(0) as u64)
                })
                .unwrap_or(60);
            return Err(Error::bad(format!(
                "Source rate limited this sync; retry after {wait} seconds"
            )));
        }
        if [
            reqwest::StatusCode::UNAUTHORIZED,
            reqwest::StatusCode::FORBIDDEN,
        ]
        .contains(&status)
        {
            self.worker.store.transaction(|db| {
                db.execute(
                    "UPDATE am_connectors SET last_sync=NULL WHERE id=?",
                    [s(self.connector, "id")],
                )?;
                store::invalidate(db, &self.job.repository)?;
                Ok(())
            })?;
        }
        let response = model::bounded_json(response).await?;
        if self.connector["kind"] == "slack" && response["ok"] != true {
            return Err(Error::bad(
                "Slack declined the source request; check channel access and scopes",
            ));
        }
        Ok(response)
    }
    pub async fn get(&mut self, path: &str, query: Vec<(String, String)>) -> Result<Value> {
        self.request(reqwest::Method::GET, path, &query, None).await
    }
    async fn binary(
        &mut self,
        path: &str,
        query: Vec<(String, String)>,
        limit: usize,
    ) -> Result<Vec<u8>> {
        if self.calls >= self.limit {
            return Err(Error::bad(
                "Connector request budget reached; resume the staged sync",
            ));
        }
        jobs::guarded(&self.worker.store, self.job, |db, _, _| {
            jobs::renew(db, self.job)
        })?;
        self.calls += 1;
        let token = crate::oauth::for_connector(
            &self.worker.store,
            &self.worker.client,
            self.job,
            s(self.connector, "credential_env"),
        )
        .await?;
        let mut response = self
            .worker
            .client
            .get(format!(
                "{}/{}",
                self.base.trim_end_matches('/'),
                path.trim_start_matches('/')
            ))
            .bearer_auth(token)
            .query(&query)
            .send()
            .await
            .map_err(|_| Error::bad("Source text request failed"))?;
        if [
            reqwest::StatusCode::UNAUTHORIZED,
            reqwest::StatusCode::FORBIDDEN,
        ]
        .contains(&response.status())
        {
            self.worker.store.transaction(|db| {
                db.execute(
                    "UPDATE am_connectors SET last_sync=NULL WHERE id=?",
                    [s(self.connector, "id")],
                )?;
                store::invalidate(db, &self.job.repository)?;
                Ok(())
            })?;
        }
        if !response.status().is_success() {
            return Err(Error::bad(format!(
                "Source returned HTTP {}",
                response.status().as_u16()
            )));
        }
        let mut out = Vec::new();
        while let Some(bytes) = response
            .chunk()
            .await
            .map_err(|_| Error::bad("Source response interrupted"))?
        {
            if out.len() + bytes.len() > limit {
                return Err(Error::bad(format!(
                    "{TOO_LARGE}: the download exceeds {}; raise limits.document_bytes (documents) or limits.document_text_bytes (text) in the configuration file or GRAPHICLE_MAX_DOCUMENT_BYTES or GRAPHICLE_MAX_DOCUMENT_TEXT_BYTES",
                    crate::limits::size(limit)
                )));
            }
            out.extend_from_slice(&bytes);
        }
        Ok(out)
    }
    async fn text(&mut self, path: &str, query: Vec<(String, String)>) -> Result<String> {
        String::from_utf8(
            self.binary(path, query, crate::documents::max_text())
                .await?,
        )
        .map_err(|_| Error::bad("Source is not UTF-8 text"))
    }
}
pub fn pair(key: &str, value: impl ToString) -> (String, String) {
    (key.into(), value.to_string())
}
/// The text of Notion rich text (its runs' plain text, in order) and the
/// byte ranges of its runs annotated as `code`. The text is what the
/// connector stored before documents were linked to code: inline code is
/// recorded beside it, never
/// written into it.
pub fn rich_spans(value: &Value) -> (String, Vec<(usize, usize)>) {
    let mut text = String::new();
    let mut spans = Vec::new();
    for run in value.as_array().into_iter().flatten() {
        let start = text.len();
        text.push_str(
            run["plain_text"]
                .as_str()
                .or(run["text"]["content"].as_str())
                .unwrap_or(""),
        );
        if text.len() > start && run["annotations"]["code"] == true {
            spans.push((start, text.len()));
        }
    }
    (text, spans)
}

/// The text of a Notion page from its title and its rendered blocks, each
/// with the byte ranges of its inline code, and the code spans of the whole
/// text.
pub fn notion_text(
    title: &(String, Vec<(usize, usize)>),
    blocks: &[(String, Vec<(usize, usize)>)],
) -> (String, Vec<(usize, usize)>) {
    let mut text = format!("# {}\n\n", title.0);
    let mut spans: Vec<(usize, usize)> = title.1.iter().map(|(a, b)| (a + 2, b + 2)).collect();
    for (index, (block, marked)) in blocks.iter().enumerate() {
        if index > 0 {
            text.push_str("\n\n");
        }
        let base = text.len();
        spans.extend(marked.iter().map(|(a, b)| (base + a, base + b)));
        text.push_str(block);
    }
    (text, spans)
}

/// A bounded, read-only diagnostic. It never treats one account's access as team ACL proof.
pub async fn probe(worker: &Worker, job: &Job) -> Result<Value> {
    let mut connector = jobs::guarded(&worker.store, job, |db, _, _| {
        one(
            db,
            "SELECT * FROM am_connectors WHERE repository_id=? AND id=? AND state='active'",
            params![job.repository, string(&job.payload, "connector_id", 100)?],
        )?
        .ok_or_else(|| Error::bad("Connector unavailable"))
    })?;
    decode(&mut connector, "config")?;
    let kind = s(&connector, "kind");
    let cfg = &connector["config"];
    let source = crate::extension::installed().source(kind);
    let default = match kind {
        "github" => "https://api.github.com".to_string(),
        "slack" => "https://slack.com/api".to_string(),
        "notion" => "https://api.notion.com/v1".to_string(),
        "gdrive" => "https://www.googleapis.com/drive/v3".to_string(),
        "jira" | "confluence" => string(cfg, "base_url", 2000)?.to_string(),
        _ => match source {
            Some(source) => source.base_url(cfg)?,
            None => return Err(Error::bad("This source does not use a remote account")),
        },
    };
    let endpoint = worker
        .config
        .connector_endpoints
        .get(kind)
        .cloned()
        .unwrap_or(default);
    let url = model::checked_url(&endpoint)?;
    let loopback = url
        .host_str()
        .is_some_and(|s| s == "localhost" || s == "127.0.0.1" || s == "[::1]");
    let mut remote = Remote {
        worker,
        job,
        connector: &connector,
        base: endpoint.clone(),
        calls: 0,
        limit: 8,
        permissions: BTreeMap::new(),
    };
    let (identity, target) = match kind {
        "github" => {
            let who = remote.get("user", vec![]).await?;
            let target = remote
                .get(
                    &format!("repos/{}", string(cfg, "repository", 300)?),
                    vec![],
                )
                .await?;
            (
                json!({"id":who["id"],"name":who["login"]}),
                json!({"id":target["id"],"name":target["full_name"]}),
            )
        }
        "slack" => {
            let who = remote.get("auth.test", vec![]).await?;
            let channel = cfg["channels"][0]
                .as_str()
                .ok_or_else(|| Error::bad("Choose a Slack channel"))?;
            let target = remote
                .get("conversations.info", vec![pair("channel", channel)])
                .await?;
            (
                json!({"id":who["user_id"],"team_id":who["team_id"]}),
                json!({"id":target["channel"]["id"],"name":target["channel"]["name"]}),
            )
        }
        "gdrive" => {
            let who = remote.get("about", vec![pair("fields", "user")]).await?;
            let identifier = cfg["folder_id"]
                .as_str()
                .or(cfg["file_ids"][0].as_str())
                .ok_or_else(|| Error::bad("Choose a Drive folder or file"))?;
            let target = remote
                .get(
                    &format!("files/{}", segment(identifier)),
                    vec![
                        pair("fields", "id,name,mimeType"),
                        pair("supportsAllDrives", "true"),
                    ],
                )
                .await?;
            (
                json!({"id":who["user"]["permissionId"],"name":who["user"]["displayName"]}),
                target,
            )
        }
        "notion" => {
            let who = remote.get("users/me", vec![]).await?;
            let identifier = cfg["page_ids"][0]
                .as_str()
                .ok_or_else(|| Error::bad("Choose a Notion page"))?;
            let target = remote
                .get(&format!("pages/{}", segment(identifier)), vec![])
                .await?;
            (
                json!({"id":who["id"],"name":who["name"]}),
                json!({"id":target["id"],"url":target["url"]}),
            )
        }
        "jira" => {
            let who = remote.get("rest/api/3/myself", vec![]).await?;
            let target = remote
                .request(
                    reqwest::Method::POST,
                    "rest/api/3/search/jql",
                    &[],
                    Some(json!({"jql":cfg["jql"],"maxResults":1,"fields":["summary"]})),
                )
                .await?;
            if !target["issues"].is_array() {
                return Err(Error::bad("Jira did not return an issue listing"));
            }
            (
                json!({"id":who["accountId"],"name":who["displayName"]}),
                json!({"query_accepted":true,"sample_items":target["issues"].as_array().unwrap().len(),"zero_matches_is_not_proof_of_project_access":true}),
            )
        }
        "confluence" => {
            let target = if let Some(identifier) = cfg["space_id"].as_str() {
                remote
                    .get(
                        &format!("wiki/api/v2/spaces/{}", segment(identifier)),
                        vec![],
                    )
                    .await?
            } else {
                let r = remote
                    .get(
                        "wiki/api/v2/spaces",
                        vec![
                            pair("keys", string(cfg, "space_key", 100)?),
                            pair("limit", 2),
                        ],
                    )
                    .await?;
                let rows = r["results"]
                    .as_array()
                    .filter(|v| v.len() == 1)
                    .ok_or_else(|| Error::bad("Space is missing, ambiguous, or not accessible"))?;
                rows[0].clone()
            };
            (
                json!({"credential_accepted":true}),
                json!({"id":target["id"],"name":target["name"]}),
            )
        }
        _ => match source {
            Some(source) => source.probe(&mut remote).await?,
            None => unreachable!(),
        },
    };
    jobs::guarded(&worker.store, job, |db, _, _| {
        ensure_connector(db, &connector)
    })?;
    Ok(
        json!({"status":"connected","kind":kind,"identity":identity,"target":target,"requests":remote.calls,"checked_at":now(),
        "endpoint_origin":url.origin().ascii_serialization(),"endpoint_is_loopback":loopback,
        "evidence":if loopback{"local_fixture_response"}else{"remote_api_response"},
        "scope":"Account and first configured target only. This does not certify all documents, team permission equivalence, or the next synchronization."}),
    )
}

pub async fn sync(worker: &Worker, job: &Job) -> Result<Value> {
    let mut connector = jobs::guarded(&worker.store, job, |db, _, _| {
        one(
            db,
            "SELECT * FROM am_connectors WHERE repository_id=? AND id=? AND state='active'",
            params![job.repository, string(&job.payload, "connector_id", 100)?],
        )?
        .ok_or_else(|| Error::bad("Connector unavailable"))
    })?;
    decode(&mut connector, "config")?;
    decode(&mut connector, "cursor")?;
    let kind = s(&connector, "kind");
    if kind == "jsonl" {
        return Err(Error::bad("Use document import for JSONL sources"));
    }
    if connector["config"]["permission_mode"] == "provider" && !provider_permission_sync() {
        return Err(Error::bad(PROVIDER_SYNC_UNAVAILABLE));
    }
    let default = match kind {
        "github" => "https://api.github.com".to_string(),
        "slack" => "https://slack.com/api".to_string(),
        "notion" => "https://api.notion.com/v1".to_string(),
        "gdrive" => "https://www.googleapis.com/drive/v3".to_string(),
        "jira" | "confluence" => string(&connector["config"], "base_url", 2000)?.to_string(),
        _ => match crate::extension::installed().source(kind) {
            Some(source) => source.base_url(&connector["config"])?,
            None => return Err(Error::bad("Unknown source connector")),
        },
    };
    let base = worker
        .config
        .connector_endpoints
        .get(kind)
        .cloned()
        .unwrap_or(default);
    model::checked_url(&base)?;
    let mut remote = Remote {
        worker,
        job,
        connector: &connector,
        base,
        calls: 0,
        limit: integer(&job.payload, "max_requests", 64, 256)?.max(1),
        permissions: BTreeMap::new(),
    };
    let mut cursor = if connector["cursor"].is_object() {
        connector["cursor"].clone()
    } else {
        json!({"scan_id":id(),"config_hash":digest(&connector["config"]),"items":[],"list_cursor":null,"listed":false,"index":0})
    };
    if cursor["config_hash"] != digest(&connector["config"]) {
        return Err(Error::conflict(
            "Source configuration changed; reset the pending scan",
        ));
    }
    while cursor["listed"] != true && remote.calls < remote.limit {
        let (items, next) = listing(&mut remote, &cursor["list_cursor"]).await?;
        cursor["items"].as_array_mut().unwrap().extend(items);
        let cap = crate::limits::current().connector_documents;
        if cursor["items"].as_array().unwrap().len() > cap {
            return Err(Error::bad(format!(
                "Source listing exceeds {} documents; narrow the source's scope, or raise the limit with {}",
                crate::limits::grouped(cap),
                crate::limits::raise("connector_documents")
            )));
        }
        cursor["listed"] = json!(next.is_null());
        cursor["list_cursor"] = next;
        save_cursor(worker, job, &connector, &cursor)?;
    }
    while cursor["listed"] == true
        && (cursor["index"].as_u64().unwrap_or(0) as usize)
            < cursor["items"].as_array().unwrap().len()
        && remote.calls < remote.limit
    {
        let index = cursor["index"].as_u64().unwrap() as usize;
        let item = cursor["items"][index].clone();
        // A document past a size or structure limit is listed under
        // `skipped` with the reason, and the scan goes on without it.
        let mut document = match fetch_document(&mut remote, &item).await {
            Ok(Some(doc))
                if doc["content"]
                    .as_str()
                    .is_some_and(|text| text.len() > crate::documents::max_text()) =>
            {
                let bytes = doc["content"].as_str().map_or(0, str::len);
                skip(&mut cursor, &item, &text_too_large("the document", bytes));
                None
            }
            Ok(document) => document,
            Err(error) if error.message.starts_with(TOO_LARGE) => {
                skip(&mut cursor, &item, &error);
                None
            }
            Err(error) => return Err(error),
        };
        if connector["config"]["permission_mode"] == "provider"
            && let Some(doc) = document.as_mut()
        {
            let permissions = match crate::extension::installed()
                .source_permissions(&mut remote, &item, doc)
                .await
            {
                Some(fetched) => fetched?,
                None => return Err(Error::bad(PROVIDER_SYNC_UNAVAILABLE)),
            };
            doc["provider_permissions"] = permissions;
        }
        jobs::guarded(&worker.store, job, |db, _, _| {
            ensure_connector(db, &connector)?;
            if let Some(doc) = &document {
                stage(db, &connector, s(&cursor, "scan_id"), doc)?;
            }
            Ok(())
        })?;
        cursor["index"] = json!(index + 1);
        save_cursor(worker, job, &connector, &cursor)?;
    }
    let complete = cursor["listed"] == true
        && cursor["index"].as_u64().unwrap_or(0) as usize
            == cursor["items"].as_array().unwrap().len();
    let generation = if complete {
        Some(jobs::guarded(&worker.store, job, |db, p, _| {
            ensure_connector(db, &connector)?;
            let generation = activate(db, &connector, s(&cursor, "scan_id"), true)?;
            crate::doc_links::after_sync(db, &job.repository, s(&connector, "id"), p)?;
            Ok(generation)
        })?)
    } else {
        None
    };
    let mut result = json!({"connector_id":connector["id"],"complete":complete,"staged_documents":cursor["index"],"listed_documents":cursor["items"].as_array().unwrap().len(),"requests":remote.calls,"generation":generation,"provider_permissions_synced":complete&&connector["config"]["permission_mode"]=="provider"});
    if cursor["skipped_count"].as_u64().unwrap_or(0) > 0 {
        result["skipped"] = cursor["skipped"].clone();
        result["skipped_count"] = cursor["skipped_count"].clone();
    }
    Ok(result)
}

/// Record a listed document left out of this scan, with the reason: the
/// first 1,000 are listed, all are counted.
fn skip(cursor: &mut Value, item: &Value, error: &Error) {
    let count = cursor["skipped_count"].as_u64().unwrap_or(0);
    cursor["skipped_count"] = json!(count + 1);
    if !cursor["skipped"].is_array() {
        cursor["skipped"] = json!([]);
    }
    let listed = cursor["skipped"].as_array_mut().expect("an array");
    if listed.len() < 1000 {
        listed.push(json!({"item":item_label(item),"reason":error.message}));
    }
}
pub fn ensure_connector(db: &Connection, connector: &Value) -> Result<()> {
    let current = one(
        db,
        "SELECT generation,config,state FROM am_connectors WHERE id=?",
        [s(connector, "id")],
    )?
    .ok_or_else(|| Error::conflict("Connector removed"))?;
    if current["state"] != "active"
        || current["generation"] != connector["generation"]
        || serde_json::from_str::<Value>(s(&current, "config"))? != connector["config"]
    {
        return Err(Error::conflict("Connector changed during sync"));
    }
    Ok(())
}
fn save_cursor(worker: &Worker, job: &Job, connector: &Value, cursor: &Value) -> Result<()> {
    jobs::guarded(&worker.store, job, |db, _, _| {
        ensure_connector(db, connector)?;
        db.execute(
            "UPDATE am_connectors SET cursor=? WHERE id=?",
            params![cursor.to_string(), s(connector, "id")],
        )?;
        Ok(())
    })
}
pub fn segment(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes()).collect()
}
async fn listing(remote: &mut Remote<'_>, cursor: &Value) -> Result<(Vec<Value>, Value)> {
    let config = remote.connector["config"].clone();
    match s(remote.connector, "kind") {
        "jira" | "confluence" => crate::atlassian::listing(remote, cursor).await,
        "github" => {
            let repository = s(&config, "repository");
            let stage = cursor["stage"].as_str().unwrap_or("issues");
            if stage == "issues" && config["include_issues"] != false {
                let page = cursor["page"].as_u64().unwrap_or(1);
                let values = remote
                    .get(
                        &format!("repos/{repository}/issues"),
                        vec![
                            pair("state", "all"),
                            pair("per_page", 100),
                            pair("page", page),
                        ],
                    )
                    .await?;
                let items = values
                    .as_array()
                    .ok_or_else(|| Error::bad("Invalid GitHub issue list"))?;
                let next = if items.len() == 100 {
                    json!({"stage":"issues","page":page+1})
                } else if config["include_files"] != false {
                    json!({"stage":"files"})
                } else {
                    Value::Null
                };
                Ok((
                    items
                        .iter()
                        .map(|v| json!({"type":"issue","value":v}))
                        .collect(),
                    next,
                ))
            } else if config["include_files"] != false {
                let branch = if let Some(branch) = config["branch"].as_str() {
                    branch.to_string()
                } else {
                    let repo = remote.get(&format!("repos/{repository}"), vec![]).await?;
                    string(&repo, "default_branch", 200)?.to_string()
                };
                let tree = remote
                    .get(
                        &format!("repos/{repository}/git/trees/{}", segment(&branch)),
                        vec![pair("recursive", 1)],
                    )
                    .await?;
                if tree["truncated"] == true {
                    return Err(Error::bad(
                        "GitHub truncated the tree; select a smaller repository or sync its checkout locally",
                    ));
                }
                let mut items = Vec::new();
                for item in tree["tree"]
                    .as_array()
                    .ok_or_else(|| Error::bad("Missing GitHub tree"))?
                {
                    // A file over the text limit is listed too, and the
                    // scan reports it under `skipped` without fetching it.
                    if item["type"] == "blob" && item["size"].is_u64() && indexable(s(item, "path"))
                    {
                        items.push(json!({"type":"file","value":item,"commit":tree["sha"]}));
                    }
                }
                Ok((items, Value::Null))
            } else {
                Ok((Vec::new(), Value::Null))
            }
        }
        "slack" => {
            let channels = config["channels"].as_array().unwrap();
            let index = cursor["channel"].as_u64().unwrap_or(0) as usize;
            if index >= channels.len() {
                return Ok((vec![], Value::Null));
            }
            let channel = channels[index].as_str().unwrap();
            let mut query = vec![pair("channel", channel), pair("limit", 100)];
            if let Some(next) = cursor["next"].as_str() {
                query.push(pair("cursor", next));
            }
            let response = remote.get("conversations.history", query).await?;
            let messages = response["messages"]
                .as_array()
                .ok_or_else(|| Error::bad("Missing Slack history"))?;
            let mut seen = BTreeSet::new();
            let items = messages
                .iter()
                .filter_map(|message| {
                    let timestamp = message["thread_ts"].as_str().or(message["ts"].as_str())?;
                    if !seen.insert(timestamp) {
                        return None;
                    }
                    Some(json!({"channel":channel,"ts":timestamp,"message":message}))
                })
                .collect();
            let next = response["response_metadata"]["next_cursor"]
                .as_str()
                .unwrap_or("");
            let next = if !next.is_empty() {
                json!({"channel":index,"next":next})
            } else if index + 1 < channels.len() {
                json!({"channel":index+1})
            } else {
                Value::Null
            };
            Ok((items, next))
        }
        "notion" => Ok((
            config["page_ids"]
                .as_array()
                .unwrap()
                .iter()
                .map(|id| json!({"id":id}))
                .collect(),
            Value::Null,
        )),
        "gdrive" => {
            if let Some(ids) = config["file_ids"].as_array()
                && !ids.is_empty()
            {
                return Ok((ids.iter().map(|id| json!({"id":id})).collect(), Value::Null));
            }
            let root = string(&config, "folder_id", 200)?;
            let mut folders = cursor["folders"]
                .as_array()
                .cloned()
                .unwrap_or_else(|| vec![json!(root)]);
            let mut visited = cursor["visited"].as_array().cloned().unwrap_or_default();
            if folders.is_empty() {
                return Ok((vec![], Value::Null));
            }
            let folder = folders[0]
                .as_str()
                .ok_or_else(|| Error::bad("Invalid Drive folder cursor"))?
                .to_string();
            if !folder
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
            {
                return Err(Error::bad("Invalid Drive folder ID"));
            }
            let mut query = vec![
                pair("q", format!("'{folder}' in parents and trashed = false")),
                pair("pageSize", 100),
                pair(
                    "fields",
                    "nextPageToken,files(id,name,mimeType,modifiedTime,webViewLink,trashed)",
                ),
                pair("supportsAllDrives", "true"),
                pair("includeItemsFromAllDrives", "true"),
            ];
            if let Some(next) = cursor["next"].as_str() {
                query.push(pair("pageToken", next));
            }
            let response = remote.get("files", query).await?;
            let mut files = Vec::new();
            for file in response["files"]
                .as_array()
                .ok_or_else(|| Error::bad("Missing Drive file list"))?
            {
                if file["mimeType"] == "application/vnd.google-apps.folder" {
                    if config["recursive"] != false
                        && !folders.contains(&file["id"])
                        && !visited.contains(&file["id"])
                    {
                        folders.push(file["id"].clone());
                    }
                } else {
                    files.push(file.clone());
                }
            }
            let cap = crate::limits::current().connector_documents;
            if folders.len() + visited.len() > cap {
                return Err(Error::bad(format!(
                    "Drive folder traversal exceeds {} folders; narrow the source's scope, or raise the limit with {}",
                    crate::limits::grouped(cap),
                    crate::limits::raise("connector_documents")
                )));
            }
            let next = if let Some(page) = response["nextPageToken"].as_str() {
                json!({"folders":folders,"visited":visited,"next":page})
            } else {
                visited.push(folders.remove(0));
                if folders.is_empty() {
                    Value::Null
                } else {
                    json!({"folders":folders,"visited":visited})
                }
            };
            Ok((files, next))
        }
        kind => match crate::extension::installed().source(kind) {
            Some(source) => source.listing(remote, cursor).await,
            None => Err(Error::bad("Unsupported connector listing")),
        },
    }
}
fn indexable(path: &str) -> bool {
    let excluded = [
        ".git",
        "node_modules",
        "vendor",
        "dist",
        "build",
        ".venv",
        "__pycache__",
        ".env",
        ".ssh",
    ];
    if path.split('/').any(|p| {
        excluded.contains(&p)
            || p.starts_with(".env.")
            || p.ends_with(".pem")
            || p.ends_with(".key")
    }) {
        return false;
    }
    let name = path.rsplit('/').next().unwrap_or("").to_lowercase();
    let ext = name.rsplit('.').next().unwrap_or("");
    if graphicle_memory::structure::expanded_language(path).is_some() {
        return true;
    }
    [
        "md", "mdx", "markdown", "txt", "rst", "py", "js", "jsx", "ts", "tsx", "rs", "go", "java",
        "c", "h", "cpp", "hpp", "toml", "yaml", "yml", "json", "sql", "sh", "css", "html",
    ]
    .contains(&ext)
        || [
            "readme",
            "license",
            "licence",
            "changelog",
            "contributing",
            "authors",
            "notice",
            "makefile",
            "dockerfile",
        ]
        .contains(&name.as_str())
}
async fn fetch_document(remote: &mut Remote<'_>, item: &Value) -> Result<Option<Value>> {
    match s(remote.connector, "kind") {
        "github" => github_document(remote, item).await,
        "slack" => slack_document(remote, item).await,
        "notion" => notion_document(remote, item).await,
        "gdrive" => drive_document(remote, item).await,
        "jira" | "confluence" => crate::atlassian::document(remote, item).await,
        kind => match crate::extension::installed().source(kind) {
            Some(source) => source.document(remote, item).await,
            None => Err(Error::bad("Unsupported connector document")),
        },
    }
}
async fn github_document(remote: &mut Remote<'_>, item: &Value) -> Result<Option<Value>> {
    let repository = s(&remote.connector["config"], "repository").to_string();
    let value = &item["value"];
    if item["type"] == "file" {
        let size = value["size"].as_u64().unwrap_or(0) as usize;
        if size > crate::documents::max_text() {
            return Err(text_too_large(
                &format!("GitHub file {repository}/{}", s(value, "path")),
                size,
            ));
        }
        let blob = remote
            .get(
                &format!("repos/{repository}/git/blobs/{}", segment(s(value, "sha"))),
                vec![],
            )
            .await?;
        if blob["encoding"] != "base64" {
            return Err(Error::bad("Unsupported GitHub blob encoding"));
        }
        use base64::Engine;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(s(&blob, "content").replace(['\n', '\r'], ""))
            .map_err(|_| Error::bad("Invalid GitHub blob"))?;
        let text =
            String::from_utf8(bytes).map_err(|_| Error::bad("GitHub source is not UTF-8"))?;
        return Ok(Some(
            json!({"id":format!("{repository}:file:{}",s(value,"path")),"title":format!("{repository}/{}",s(value,"path")),"content":text,"url":format!("https://github.com/{repository}/blob/{}/{}",s(item,"commit"),s(value,"path")),"source_timestamp":null,"metadata":{"git_blob":value["sha"],"git_commit":item["commit"]}}),
        ));
    }
    let number = value["number"]
        .as_u64()
        .ok_or_else(|| Error::bad("GitHub issue number missing"))?;
    let mut text = format!(
        "# {}\n\nAuthor: {}\nCreated: {}\nUpdated: {}\nStatus: {}\n\n{}",
        s(value, "title"),
        s(&value["user"], "login"),
        s(value, "created_at"),
        s(value, "updated_at"),
        s(value, "state"),
        s(value, "body")
    );
    let mut page = 1;
    if value["comments"].as_u64().unwrap_or(0) > 0 {
        loop {
            let response = remote
                .get(
                    &format!("repos/{repository}/issues/{number}/comments"),
                    vec![pair("per_page", 100), pair("page", page)],
                )
                .await?;
            let comments = response
                .as_array()
                .ok_or_else(|| Error::bad("Invalid GitHub comments"))?;
            for comment in comments {
                text.push_str(&format!(
                    "\n\nComment by {} at {} (updated {})\n{}",
                    s(&comment["user"], "login"),
                    s(comment, "created_at"),
                    s(comment, "updated_at"),
                    s(comment, "body")
                ));
            }
            if text.len() > crate::documents::max_text() {
                return Err(text_too_large(
                    &format!("GitHub discussion {repository}#{number}"),
                    text.len(),
                ));
            }
            if comments.len() < 100 {
                break;
            }
            page += 1;
        }
    }
    Ok(Some(
        json!({"id":format!("{repository}:issue:{number}"),"title":value["title"],"content":text,"url":value["html_url"],"source_timestamp":null,"metadata":{"type":if value["pull_request"].is_object(){"pull_request"}else{"issue"},"created_at":value["created_at"],"updated_at":value["updated_at"]}}),
    ))
}
async fn slack_document(remote: &mut Remote<'_>, item: &Value) -> Result<Option<Value>> {
    let channel = string(item, "channel", 200)?;
    let timestamp = string(item, "ts", 100)?;
    let mut messages = Vec::new();
    let mut cursor = String::new();
    if item["message"]["reply_count"].as_u64().unwrap_or(0) == 0
        && item["message"]["ts"] == item["ts"]
    {
        messages.push(item["message"].clone());
    } else {
        loop {
            let mut query = vec![
                pair("channel", channel),
                pair("ts", timestamp),
                pair("limit", 100),
            ];
            if !cursor.is_empty() {
                query.push(pair("cursor", &cursor));
            }
            let response = remote.get("conversations.replies", query).await?;
            messages.extend(
                response["messages"]
                    .as_array()
                    .ok_or_else(|| Error::bad("Missing Slack thread"))?
                    .clone(),
            );
            let cap = crate::limits::current().connector_items;
            if messages.len() > cap {
                return Err(too_large(
                    &format!(
                        "a Slack thread has more than {} messages",
                        crate::limits::grouped(cap)
                    ),
                    "connector_items",
                    cap,
                ));
            }
            cursor = s(&response["response_metadata"], "next_cursor").to_string();
            if cursor.is_empty() {
                break;
            }
        }
    }
    messages.sort_by(|a, b| s(a, "ts").cmp(s(b, "ts")));
    messages.dedup_by(|a, b| a["ts"] == b["ts"]);
    let content = messages
        .iter()
        .map(|m| {
            let ts = s(m, "ts");
            let seconds = ts.split('.').next().unwrap_or("").parse::<i64>().ok();
            let date = seconds
                .and_then(|n| chrono::DateTime::from_timestamp(n, 0))
                .map(|d| d.to_rfc3339())
                .unwrap_or_else(|| ts.into());
            format!(
                "[{}] {}: {}",
                date,
                m["user"]
                    .as_str()
                    .or(m["bot_id"].as_str())
                    .unwrap_or("unknown"),
                s(m, "text")
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    Ok(Some(
        json!({"id":format!("{channel}:{timestamp}"),"title":format!("Slack {channel} · {timestamp}"),"content":content,"url":null,"source_timestamp":null,"metadata":{"channel_id":channel,"thread_ts":timestamp,"messages":messages.len(),"files_downloaded":false}}),
    ))
}
async fn notion_document(remote: &mut Remote<'_>, item: &Value) -> Result<Option<Value>> {
    let page_id = string(item, "id", 200)?;
    let page = remote
        .get(&format!("pages/{}", segment(page_id)), vec![])
        .await?;
    if page["archived"] == true || page["in_trash"] == true {
        return Ok(None);
    }
    let titled = page["properties"]
        .as_object()
        .into_iter()
        .flat_map(|p| p.values())
        .find(|p| p["type"] == "title")
        .map(|p| rich_spans(&p["title"]))
        .filter(|(v, _)| !v.is_empty())
        .unwrap_or_else(|| (format!("Notion {page_id}"), Vec::new()));
    let title = titled.0.clone();
    // A stack holds both block rendering and paginated child fetches. Pushing
    // siblings in reverse keeps each parent's children before its next sibling.
    enum Pending {
        Page(String, usize, String),
        Block(Value, usize),
    }
    let mut work = vec![Pending::Page(page_id.to_string(), 0, String::new())];
    let mut blocks = Vec::new();
    let mut unsupported = BTreeSet::new();
    let mut visited = 0;
    while let Some(item) = work.pop() {
        match item {
            Pending::Page(block_id, depth, cursor) => {
                if depth > 20 {
                    return Err(Error::bad("Notion block tree exceeds depth limit"));
                }
                let mut query = vec![pair("page_size", 100)];
                if !cursor.is_empty() {
                    query.push(pair("start_cursor", cursor));
                }
                let result = remote
                    .get(&format!("blocks/{}/children", segment(&block_id)), query)
                    .await?;
                let children = result["results"]
                    .as_array()
                    .ok_or_else(|| Error::bad("Missing Notion blocks"))?;
                if result["has_more"] == true {
                    work.push(Pending::Page(
                        block_id,
                        depth,
                        string(&result, "next_cursor", 500)?.to_string(),
                    ));
                }
                for child in children.iter().rev() {
                    work.push(Pending::Block(child.clone(), depth));
                }
            }
            Pending::Block(block, depth) => {
                visited += 1;
                let cap = crate::limits::current().connector_items;
                if visited > cap {
                    return Err(too_large(
                        &format!(
                            "a Notion page has more than {} blocks",
                            crate::limits::grouped(cap)
                        ),
                        "connector_items",
                        cap,
                    ));
                }
                if block["archived"] == true || block["in_trash"] == true {
                    continue;
                }
                let kind = s(&block, "type");
                let value = &block[kind];
                let (mut text, mut spans) = rich_spans(&value["rich_text"]);
                if kind == "table_row" {
                    let mut row = String::new();
                    spans.clear();
                    for (index, cell) in value["cells"].as_array().into_iter().flatten().enumerate()
                    {
                        if index > 0 {
                            row.push_str(" | ");
                        }
                        let (cell, marked) = rich_spans(cell);
                        spans.extend(marked.iter().map(|(a, b)| (row.len() + a, row.len() + b)));
                        row.push_str(&cell);
                    }
                    text = row;
                }
                if kind == "child_page" {
                    text = s(value, "title").to_string();
                    spans.clear();
                }
                if kind == "code" {
                    // A code block is code, not a mention of it.
                    text = format!("```{}\n{}\n```", s(value, "language"), text);
                    spans.clear();
                }
                if text.is_empty() && !["column_list", "column", "table", "divider"].contains(&kind)
                {
                    unsupported.insert(kind.to_string());
                }
                if !text.is_empty() {
                    let indent = "  ".repeat(depth);
                    let marked = spans
                        .iter()
                        .map(|(a, b)| (indent.len() + a, indent.len() + b))
                        .collect();
                    blocks.push((format!("{indent}{text}"), marked));
                }
                if block["has_children"] == true {
                    work.push(Pending::Page(
                        s(&block, "id").to_string(),
                        depth + 1,
                        String::new(),
                    ));
                }
            }
        }
    }
    let (content, spans) = notion_text(&titled, &blocks);
    let mut document = json!({"id":page_id,"title":title,"content":content,"url":page["url"],"source_timestamp":null,"metadata":{"created_at":page["created_time"],"updated_at":page["last_edited_time"],"unsupported_block_types":unsupported,"provider_order":"page blocks with explicit tree indentation"}});
    if let Some(spans) = crate::doc_links::spans_json(&spans) {
        document["code_spans"] = spans;
    }
    Ok(Some(document))
}
/// Keep the original bytes of a connector document parsed by
/// `documents::extract` (`parsing`), for **Download original**, within the
/// repository's retained-originals limit (`retained_original_bytes`).
pub fn keep_original(
    remote: &Remote<'_>,
    document: &str,
    name: &str,
    parsing: &Value,
    bytes: &[u8],
) -> Result<()> {
    jobs::guarded(&remote.worker.store, remote.job, |db, _, _| {
        let used:i64=db.query_row("SELECT coalesce(sum(length(bytes)),0) FROM am_original_documents WHERE repository_id=?",[&remote.job.repository],|r|r.get(0))?;
        let cap = crate::limits::current().retained_original_bytes;
        if usize::try_from(used + bytes.len() as i64).unwrap_or(usize::MAX) > cap {
            return Err(Error::bad(format!(
                "Original document storage exceeds {}; prune retained versions, or raise the limit with {}",
                crate::limits::size(cap),
                crate::limits::raise("retained_original_bytes")
            )));
        }
        db.execute(
            "INSERT OR IGNORE INTO am_original_documents VALUES(?,?,?,?,?,?,?,?,?)",
            params![
                remote.job.repository,
                s(remote.connector, "id"),
                format!("connector:{}:{document}", s(remote.connector, "id")),
                s(parsing, "original_sha256"),
                name,
                s(parsing, "parser"),
                s(parsing, "text_sha256"),
                bytes,
                now()
            ],
        )?;
        Ok(())
    })
}
async fn drive_document(remote: &mut Remote<'_>, item: &Value) -> Result<Option<Value>> {
    let file_id = string(item, "id", 200)?;
    let metadata = remote
        .get(
            &format!("files/{}", segment(file_id)),
            vec![
                pair(
                    "fields",
                    "id,name,mimeType,modifiedTime,webViewLink,trashed,size",
                ),
                pair("supportsAllDrives", "true"),
            ],
        )
        .await?;
    if metadata["trashed"] == true {
        return Ok(None);
    }
    let mime = s(&metadata, "mimeType");
    let mut parsing = Value::Null;
    let content = if mime == "application/vnd.google-apps.document" {
        remote
            .text(
                &format!("files/{}/export", segment(file_id)),
                vec![pair("mimeType", "text/plain")],
            )
            .await?
    } else if mime.starts_with("text/") || ["application/json", "application/xml"].contains(&mime) {
        remote
            .text(
                &format!("files/{}", segment(file_id)),
                vec![pair("alt", "media"), pair("supportsAllDrives", "true")],
            )
            .await?
    } else {
        let (name,bytes)=match mime {
            "application/vnd.google-apps.spreadsheet"=>(format!("{}.xlsx",s(&metadata,"name")),remote.binary(&format!("files/{}/export",segment(file_id)),vec![pair("mimeType","application/vnd.openxmlformats-officedocument.spreadsheetml.sheet")],crate::documents::max_binary()).await?),
            "application/vnd.google-apps.presentation"=>(format!("{}.pptx",s(&metadata,"name")),remote.binary(&format!("files/{}/export",segment(file_id)),vec![pair("mimeType","application/vnd.openxmlformats-officedocument.presentationml.presentation")],crate::documents::max_binary()).await?),
            _ if crate::documents::supported(s(&metadata,"name"))=>(s(&metadata,"name").to_string(),remote.binary(&format!("files/{}",segment(file_id)),vec![pair("alt","media"),pair("supportsAllDrives","true")],crate::documents::max_binary()).await?),
            _=>return Err(Error::bad(format!("Drive format {mime} is not supported by an installed document parser")))
        };
        parsing = crate::documents::extract(&name, &bytes)?;
        keep_original(remote, file_id, &name, &parsing, &bytes)?;
        s(&parsing, "text").to_string()
    };
    Ok(Some(
        json!({"id":file_id,"title":metadata["name"],"content":content,"url":metadata["webViewLink"],"source_timestamp":null,"metadata":{"updated_at":metadata["modifiedTime"],"mime_type":mime,"original_sha256":parsing["original_sha256"],"parser":parsing["parser"]}}),
    ))
}
