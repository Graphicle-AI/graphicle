//! Workspace-owned inference configuration. Changes never rewrite a queued model fingerprint.
use crate::{
    Error, Result, model,
    store::{Store, rows, s},
    string,
};
use rusqlite::params;
use serde_json::{Value, json};

pub fn configuration(
    store: &Store,
    workspace: &str,
    base: &model::Config,
) -> Result<model::Config> {
    let mut config = base.clone();
    for row in rows(
        &store.connect()?,
        "SELECT * FROM am_model_profiles WHERE workspace_id=? ORDER BY name",
        [workspace],
    )? {
        let name = s(&row, "name").to_string();
        config
            .models
            .insert(name.clone(), serde_json::from_str(s(&row, "profile"))?);
        if row["is_default"] == 1 {
            config.default_model = Some(name.clone());
        }
        if row["is_embedding"] == 1 {
            config.embedding_model = Some(name);
        }
    }
    config.validate()?;
    Ok(config)
}
pub fn status(store: &Store, workspace: &str, token: &str, base: &model::Config) -> Result<Value> {
    store.authorized(workspace, token, "read", |_, _| Ok(()))?;
    let config = configuration(store, workspace, base)?;
    let mut value = config.status();
    for item in value["models"].as_array_mut().unwrap() {
        let profile = &config.models[s(item, "name")];
        item["credentials_available"] = json!(
            profile
                .key_env
                .as_deref()
                .is_none_or(|r| store.resolve_credential(workspace, r).is_ok())
        );
    }
    Ok(value)
}
pub fn configure(
    store: &Store,
    workspace: &str,
    token: &str,
    base: &model::Config,
    args: &Value,
) -> Result<Value> {
    store.authorized(workspace, token, "admin", |_, _| Ok(()))?;
    let name = string(args, "name", 100)?;
    let profile: model::Profile = serde_json::from_value(args["profile"].clone())?;
    if let Some(reference) = &profile.key_env {
        store.resolve_credential(workspace, reference)?;
    }
    let mut config = configuration(store, workspace, base)?;
    config.models.insert(name.into(), profile.clone());
    config.validate()?;
    if args["embedding"] == true && profile.protocol == "anthropic" {
        return Err(Error::bad("Use a separate provider for embeddings"));
    }
    store.authorized(workspace,token,"admin",|db,_|{
        if args["default"]==true{db.execute("UPDATE am_model_profiles SET is_default=0 WHERE workspace_id=?",[workspace])?;}
        if args["embedding"]==true{db.execute("UPDATE am_model_profiles SET is_embedding=0 WHERE workspace_id=?",[workspace])?;}
        db.execute("INSERT INTO am_model_profiles VALUES(?,?,?,?,?) ON CONFLICT(workspace_id,name) DO UPDATE SET profile=excluded.profile,is_default=excluded.is_default,is_embedding=excluded.is_embedding",params![workspace,name,serde_json::to_string(&profile)?,args["default"]==true,args["embedding"]==true])?;
        Ok(json!({"configured":name,"effective":"new inference calls; queued jobs keep their original fingerprint"}))
    })
}
