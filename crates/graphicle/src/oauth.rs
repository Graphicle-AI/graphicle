//! Authorization-code connections with one-use state, PKCE and encrypted tokens.
use crate::{
    Error, Result, digest, id, local, model, now,
    store::{Store, authorize_hash, one, rows, s},
    string,
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::{RngCore, rngs::OsRng};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provider {
    pub client_id: String,
    pub client_secret_ref: Option<String>,
    pub authorization_url: String,
    pub token_url: String,
    pub identity_url: String,
    pub scopes: Vec<String>,
    #[serde(default)]
    pub pkce: bool,
}
fn providers(store: &Store, workspace: &str) -> Result<BTreeMap<String, Provider>> {
    let path = store.directory.join(format!("oauth-{workspace}.json"));
    if !path.exists() {
        return Ok(BTreeMap::new());
    }
    if path.is_symlink() {
        return Err(Error::bad("OAuth configuration cannot be a symlink"));
    }
    let bytes = std::fs::read(path)?;
    if bytes.len() > 65536 {
        return Err(Error::bad("OAuth configuration exceeds limits"));
    }
    serde_json::from_slice(&bytes).map_err(Into::into)
}
fn validate(name: &str, p: &Provider) -> Result<()> {
    if !["github", "slack", "notion", "gdrive", "atlassian"].contains(&name)
        || p.client_id.is_empty()
        || p.client_id.len() > 1000
        || p.scopes.len() > 30
    {
        return Err(Error::bad("Invalid OAuth provider"));
    }
    for value in [&p.authorization_url, &p.token_url, &p.identity_url] {
        model::checked_url(value)?;
    }
    if p.scopes
        .iter()
        .any(|v| v.is_empty() || v.len() > 200 || v.contains(['\r', '\n']))
    {
        return Err(Error::bad("Invalid OAuth scope"));
    }
    if !p.pkce && p.client_secret_ref.is_none() {
        return Err(Error::bad(
            "A confidential OAuth client secret is required when PKCE is unavailable",
        ));
    }
    Ok(())
}
pub fn configure(store: &Store, workspace: &str, capability: &str, args: &Value) -> Result<Value> {
    store.authorized(workspace, capability, "admin", |_, _| Ok(()))?;
    let name = string(args, "provider", 20)?;
    let config: Provider = serde_json::from_value(args["config"].clone())?;
    validate(name, &config)?;
    if let Some(reference) = &config.client_secret_ref {
        store.resolve_credential(workspace, reference)?;
    }
    let mut all = providers(store, workspace)?;
    all.insert(name.to_string(), config);
    // Configuration is an administrator-owned local file; it never accepts a path from sources.
    local::write_private(
        &store.directory.join(format!("oauth-{workspace}.json")),
        &serde_json::to_vec_pretty(&all)?,
    )?;
    Ok(json!({"configured":name}))
}
pub fn status(store: &Store, workspace: &str, capability: &str) -> Result<Value> {
    store.authorized(workspace,capability,"read",|db,p|{
        let accounts=if p.permits("admin"){rows(db,"SELECT id,provider,external_id,email,display_name,expires_at,scopes,state FROM am_oauth_accounts WHERE workspace_id=? ORDER BY display_name",[workspace])?}else{vec![]};
        Ok(json!({"providers":providers(store,workspace)?.keys().collect::<Vec<_>>(),"accounts":accounts}))
    })
}
pub fn start(
    store: &Store,
    workspace: &str,
    capability: &str,
    name: &str,
    redirect: &str,
) -> Result<Value> {
    let principal = store.authorized(workspace, capability, "admin", |_, p| Ok(p.clone()))?;
    let all = providers(store, workspace)?;
    let config = all
        .get(name)
        .ok_or_else(|| Error::bad("Register this OAuth application in Connections first"))?;
    validate(name, config)?;
    model::checked_url(redirect)?;
    let mut random = [0u8; 32];
    OsRng.fill_bytes(&mut random);
    let verifier = URL_SAFE_NO_PAD.encode(random);
    OsRng.fill_bytes(&mut random);
    let state = URL_SAFE_NO_PAD.encode(random);
    let state_hash = digest(&json!(state));
    let binding =
        json!({"verifier":verifier,"configuration":digest(&serde_json::to_value(config)?)})
            .to_string();
    let secret = store.save_secret(workspace, &format!("oauth-verifier:{state_hash}"), &binding)?;
    let expires = (chrono::Utc::now() + chrono::Duration::minutes(10))
        .to_rfc3339_opts(chrono::SecondsFormat::Micros, true);
    store.transaction(|db|{
        authorize_hash(db,workspace,&principal.hash,"admin")?;
        db.execute("DELETE FROM am_secrets WHERE id IN (SELECT verifier_secret FROM am_oauth_pending WHERE workspace_id=? AND (expires_at<? OR used=1))",params![workspace,now()])?;
        let count:i64=db.query_row("SELECT count(*) FROM am_oauth_pending WHERE workspace_id=?",[workspace],|r|r.get(0))?;
        if count>=20{return Err(Error::bad("Too many pending connection requests; allow them to expire"));}
        db.execute("INSERT INTO am_oauth_pending VALUES(?,?,?,?,?,?,?,0)",params![state_hash,workspace,principal.hash,name,redirect,secret,expires])?;Ok(())
    })?;
    let mut url = model::checked_url(&config.authorization_url)?;
    {
        let mut q = url.query_pairs_mut();
        q.append_pair("client_id", &config.client_id)
            .append_pair("redirect_uri", redirect)
            .append_pair("response_type", "code")
            .append_pair("state", &state);
        q.append_pair(
            if name == "slack" {
                "user_scope"
            } else {
                "scope"
            },
            &config.scopes.join(if name == "slack" { "," } else { " " }),
        );
        if config.pkce {
            q.append_pair("code_challenge_method", "S256").append_pair(
                "code_challenge",
                &URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())),
            );
        }
        if name == "gdrive" {
            q.append_pair("access_type", "offline")
                .append_pair("prompt", "consent");
        }
        if name == "notion" {
            q.append_pair("owner", "user");
        }
        if name == "atlassian" {
            q.append_pair("audience", "api.atlassian.com")
                .append_pair("prompt", "consent");
        }
    }
    Ok(json!({"authorization_url":url.as_str(),"expires_at":expires,"provider":name}))
}
async fn exchange(
    client: &reqwest::Client,
    store: &Store,
    workspace: &str,
    name: &str,
    p: &Provider,
    form: &[(String, String)],
) -> Result<Value> {
    let mut request = client
        .post(&p.token_url)
        .header("Accept", "application/json");
    let mut values = form.to_vec();
    values.push(("client_id".into(), p.client_id.clone()));
    if let Some(secret) = &p.client_secret_ref {
        let secret = store.resolve_credential(workspace, secret)?;
        if name == "notion" {
            request = request.basic_auth(&p.client_id, Some(secret));
        } else {
            values.push(("client_secret".into(), secret));
        }
    }
    if ["notion", "atlassian"].contains(&name) {
        request = request.json(&values.iter().cloned().collect::<BTreeMap<_, _>>());
    } else {
        request = request.form(&values);
    }
    let value = model::bounded_json(
        request
            .send()
            .await
            .map_err(|_| Error::bad("OAuth token exchange failed"))?,
    )
    .await?;
    if value.get("error").is_some() || value.get("ok") == Some(&json!(false)) {
        return Err(Error::bad(
            "Provider declined authorization; reconnect the account",
        ));
    }
    Ok(value)
}
fn token_fields<'a>(value: &'a Value, name: &str) -> &'a Value {
    if name == "slack" && value["authed_user"]["access_token"].is_string() {
        &value["authed_user"]
    } else {
        value
    }
}
fn access(value: &Value, name: &str) -> Result<String> {
    Ok(string(token_fields(value, name), "access_token", 32768)?.to_string())
}
pub async fn callback(
    store: &Store,
    client: &reqwest::Client,
    name: &str,
    state: &str,
    code: &str,
) -> Result<Value> {
    if state.len() > 200 || code.len() > 8192 || code.is_empty() {
        return Err(Error::bad("Invalid OAuth response"));
    }
    let state_hash = digest(&json!(state));
    let pending=store.transaction(|db|{
        let p=one(db,"SELECT * FROM am_oauth_pending WHERE state_hash=? AND provider=? AND used=0 AND expires_at>?",params![state_hash,name,now()])?.ok_or_else(||Error::bad("Connection request expired or was already used"))?;
        authorize_hash(db,s(&p,"workspace_id"),s(&p,"principal_hash"),"admin")?;
        db.execute("UPDATE am_oauth_pending SET used=1 WHERE state_hash=?",[&state_hash])?;Ok(p)
    })?;
    let workspace = s(&pending, "workspace_id");
    let all = providers(store, workspace)?;
    let config = all
        .get(name)
        .ok_or_else(|| Error::bad("OAuth provider was removed"))?;
    let bound: Value =
        serde_json::from_str(&store.secret(workspace, s(&pending, "verifier_secret"))?)?;
    if bound["configuration"] != digest(&serde_json::to_value(config)?) {
        return Err(Error::conflict(
            "OAuth application changed; start a new connection",
        ));
    }
    let verifier = string(&bound, "verifier", 200)?.to_string();
    let mut form = vec![
        ("grant_type".into(), "authorization_code".into()),
        ("code".into(), code.into()),
        ("redirect_uri".into(), s(&pending, "redirect_uri").into()),
    ];
    if config.pkce {
        form.push(("code_verifier".into(), verifier));
    }
    let tokens = exchange(client, store, workspace, name, config, &form).await?;
    let token = access(&tokens, name)?;
    let mut request = client
        .get(&config.identity_url)
        .bearer_auth(&token)
        .header("User-Agent", "Graphicle/0.3");
    if name == "notion" {
        request = request.header("Notion-Version", "2025-09-03");
    }
    let who = model::bounded_json(
        request
            .send()
            .await
            .map_err(|_| Error::bad("Account identity request failed"))?,
    )
    .await?;
    if who.get("ok") == Some(&json!(false)) {
        return Err(Error::bad("Could not verify provider account"));
    }
    let external = match name {
        "github" => who["id"]
            .as_i64()
            .map(|v| v.to_string())
            .or_else(|| who["id"].as_str().map(str::to_string)),
        "slack" => who["user_id"].as_str().map(str::to_string),
        "gdrive" => who["sub"].as_str().map(str::to_string),
        "atlassian" => who["account_id"].as_str().map(str::to_string),
        "notion" => tokens["owner"]["user"]["id"]
            .as_str()
            .or(who["id"].as_str())
            .map(str::to_string),
        _ => None,
    }
    .filter(|v| !v.is_empty())
    .ok_or_else(|| Error::bad("Provider did not return a stable account identity"))?;
    let display = who["name"]
        .as_str()
        .or(who["login"].as_str())
        .or(who["user"].as_str())
        .unwrap_or(&external)
        .to_string();
    let email = if name == "gdrive" && who["email_verified"] == true {
        who["email"].as_str()
    } else {
        None
    };
    let token_secret = store.save_secret(workspace, &format!("{name}:access"), &token)?;
    let token_metadata = token_fields(&tokens, name);
    let refresh_secret = token_metadata["refresh_token"]
        .as_str()
        .map(|s| store.save_secret(workspace, &format!("{name}:refresh"), s))
        .transpose()?;
    let expiry = token_metadata["expires_in"]
        .as_i64()
        .filter(|n| *n > 0)
        .map(|n| {
            (chrono::Utc::now() + chrono::Duration::seconds(n.min(31536000)))
                .to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
        });
    let identifier = id();
    store.transaction(|db|{
        let principal=authorize_hash(db,workspace,s(&pending,"principal_hash"),"admin")?;
        db.execute("INSERT INTO am_oauth_accounts VALUES(?,?,?,?,?,?,?,?,?,?,?,?)",params![identifier,workspace,name,external,email,display,token_secret,refresh_secret,expiry,token_metadata["scope"].as_str().unwrap_or(""),"active",now()])?;
        db.execute("INSERT OR REPLACE INTO am_provider_identities VALUES(?,?,?,?,?)",params![workspace,name,external,principal.credential,now()])?;
        db.execute("DELETE FROM am_oauth_pending WHERE state_hash=?",[&state_hash])?;
        db.execute("DELETE FROM am_secrets WHERE id=?",[s(&pending,"verifier_secret")])?;
        Ok(json!({"connected":true,"provider":name,"account_id":identifier,"display_name":display,"credential_ref":format!("oauth:{identifier}")}))
    })
}
pub async fn refresh(
    store: &Store,
    client: &reqwest::Client,
    workspace: &str,
    capability: &str,
    identifier: &str,
) -> Result<Value> {
    let principal = store.authorized(workspace, capability, "admin", |_, p| Ok(p.hash.clone()))?;
    refresh_as(store, client, workspace, &principal, "admin", identifier).await
}
async fn refresh_as(
    store: &Store,
    client: &reqwest::Client,
    workspace: &str,
    principal: &str,
    permission: &str,
    identifier: &str,
) -> Result<Value> {
    let account = store.transaction(|db| {
        authorize_hash(db, workspace, principal, permission)?;
        one(
            db,
            "SELECT * FROM am_oauth_accounts WHERE workspace_id=? AND id=? AND state='active'",
            params![workspace, identifier],
        )?
        .ok_or_else(|| Error::bad("Account unavailable"))
    })?;
    let name = s(&account, "provider");
    let all = providers(store, workspace)?;
    let profile = profile_for(&all, name)?;
    let refresh = store.secret(workspace, s(&account, "refresh_secret"))?;
    let response = exchange(
        client,
        store,
        workspace,
        name,
        profile,
        &[
            ("grant_type".into(), "refresh_token".into()),
            ("refresh_token".into(), refresh),
        ],
    )
    .await?;
    let access = store.save_secret(
        workspace,
        &format!("{name}:access"),
        &access(&response, name)?,
    )?;
    let token_metadata = token_fields(&response, name);
    let renewed = token_metadata["refresh_token"]
        .as_str()
        .map(|v| store.save_secret(workspace, &format!("{name}:refresh"), v))
        .transpose()?;
    let expires = token_metadata["expires_in"]
        .as_i64()
        .filter(|n| *n > 0)
        .map(|n| {
            (chrono::Utc::now() + chrono::Duration::seconds(n.min(31536000)))
                .to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
        });
    let result=store.transaction(|db|{
        authorize_hash(db,workspace,principal,permission)?;
        let changed=db.execute("UPDATE am_oauth_accounts SET token_secret=?,refresh_secret=coalesce(?,refresh_secret),expires_at=?,updated_at=? WHERE workspace_id=? AND id=? AND state='active' AND token_secret=?",params![access,renewed,expires,now(),workspace,identifier,s(&account,"token_secret")])?;
        if changed!=1{return Err(Error::conflict("Account changed while refreshing"));}
        db.execute("DELETE FROM am_secrets WHERE id=?",[s(&account,"token_secret")])?;
        if renewed.is_some(){db.execute("DELETE FROM am_secrets WHERE id=?",[s(&account,"refresh_secret")])?;}
        Ok(json!({"refreshed":true,"expires_at":expires}))
    });
    if result.is_err() {
        store.transaction(|db| {
            db.execute(
                "DELETE FROM am_secrets WHERE id=? OR id=?",
                params![access, renewed],
            )?;
            Ok(())
        })?;
    }
    result
}
fn profile_for<'a>(profiles: &'a BTreeMap<String, Provider>, name: &str) -> Result<&'a Provider> {
    profiles
        .get(name)
        .ok_or_else(|| Error::bad("Provider unavailable"))
}
/// Called only after a connector job has passed its normal source/actor guard.
pub(crate) async fn for_connector(
    store: &Store,
    client: &reqwest::Client,
    job: &crate::jobs::Job,
    reference: &str,
) -> Result<String> {
    if let Some(identifier) = reference.strip_prefix("oauth:") {
        let account = store.transaction(|db| {
            authorize_hash(db, &job.workspace, &job.principal_hash, "collect")?;
            one(
                db,
                "SELECT * FROM am_oauth_accounts WHERE workspace_id=? AND id=? AND state='active'",
                params![job.workspace, identifier],
            )?
            .ok_or_else(|| Error::bad("Connected account unavailable"))
        })?;
        let expiring = account["expires_at"]
            .as_str()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .is_some_and(|at| at < chrono::Utc::now() + chrono::Duration::seconds(60));
        if expiring {
            refresh_as(
                store,
                client,
                &job.workspace,
                &job.principal_hash,
                "collect",
                identifier,
            )
            .await?;
        }
    }
    store.resolve_credential(&job.workspace, reference)
}
pub fn disconnect(
    store: &Store,
    workspace: &str,
    capability: &str,
    identifier: &str,
) -> Result<Value> {
    store.authorized(workspace,capability,"admin",|db,_|{
        let account=one(db,"SELECT * FROM am_oauth_accounts WHERE workspace_id=? AND id=?",params![workspace,identifier])?.ok_or_else(||Error::bad("Account unavailable"))?;
        let reference=format!("oauth:{identifier}");
        let projects=rows(db,"SELECT repository_id FROM am_connectors WHERE credential_env=? AND repository_id IN (SELECT id FROM repo_projects WHERE workspace_id=?)",params![reference,workspace])?;
        db.execute("UPDATE am_connectors SET state='paused',last_sync=NULL,generation=generation+1 WHERE credential_env=? AND repository_id IN (SELECT id FROM repo_projects WHERE workspace_id=?)",params![reference,workspace])?;
        db.execute("DELETE FROM am_oauth_accounts WHERE id=?",[identifier])?;
        db.execute("DELETE FROM am_secrets WHERE id IN (?,?)",params![s(&account,"token_secret"),s(&account,"refresh_secret")])?;
        for p in projects{db.execute("DELETE FROM am_model_cache WHERE repository_id=?",[s(&p,"repository_id")])?;crate::store::invalidate(db,s(&p,"repository_id"))?;}
        Ok(json!({"disconnected":true,"provider_token_revoked":false}))
    })
}
pub async fn resources(
    store: &Store,
    client: &reqwest::Client,
    workspace: &str,
    token: &str,
    args: &Value,
) -> Result<Value> {
    let account_id = string(args, "account_id", 100)?;
    let account=store.authorized(workspace,token,"admin",|db,_|{
        one(db,"SELECT * FROM am_oauth_accounts WHERE workspace_id=? AND id=? AND provider='atlassian' AND state='active'",params![workspace,account_id])?.ok_or_else(||Error::bad("Choose a connected Atlassian account"))
    })?;
    if account["expires_at"]
        .as_str()
        .and_then(|v| chrono::DateTime::parse_from_rfc3339(v).ok())
        .is_some_and(|at| at < chrono::Utc::now() + chrono::Duration::seconds(60))
    {
        refresh(store, client, workspace, token, account_id).await?;
    }
    let profiles = providers(store, workspace)?;
    let profile = profile_for(&profiles, "atlassian")?;
    let origin = model::checked_url(&profile.identity_url)?
        .origin()
        .ascii_serialization();
    let response = model::bounded_json(
        client
            .get(format!("{origin}/oauth/token/accessible-resources"))
            .bearer_auth(store.resolve_credential(workspace, &format!("oauth:{account_id}"))?)
            .send()
            .await
            .map_err(|_| Error::bad("Cannot list authorized Atlassian sites"))?,
    )
    .await?;
    let rows = response
        .as_array()
        .filter(|v| v.len() <= 500)
        .ok_or_else(|| Error::bad("Invalid Atlassian site list"))?;
    let sites: Vec<Value> = rows
        .iter()
        .map(|r| {
            string(r, "id", 200)?;
            string(r, "name", 300)?;
            model::checked_url(string(r, "url", 2000)?)?;
            Ok(json!({"id":r["id"],"name":r["name"],"url":r["url"],"scopes":r["scopes"]}))
        })
        .collect::<Result<_>>()?;
    store.authorized(workspace, token, "admin", |_, _| Ok(json!({"sites":sites})))
}
