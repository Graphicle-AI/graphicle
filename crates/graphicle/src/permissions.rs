//! Intersect explicit local sharing with current provider-side membership:
//! the read-time enforcement of `permission_mode: provider`. It fails closed
//! when a document's provider permissions are missing, incomplete or older
//! than the connector allows, for example in a store whose provider
//! permissions no installed extension keeps current.
use crate::{
    Result,
    store::{Principal, rows, s},
};
use rusqlite::{Connection, params};
use serde_json::Value;

pub fn allows(
    db: &Connection,
    workspace: &str,
    provider: &str,
    permission: &Value,
    p: &Principal,
    max_age: i64,
) -> Result<bool> {
    if permission["complete"] != true {
        return Ok(false);
    }
    let fresh = permission["checked_at"]
        .as_str()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .is_some_and(|t| {
            (0..=max_age).contains(&chrono::Utc::now().signed_duration_since(t).num_seconds())
        });
    if !fresh {
        return Ok(false);
    }
    // The source-fetch credential has demonstrated access for the importing owner.
    if p.role == "owner" {
        return Ok(true);
    }
    let identities = rows(
        db,
        "SELECT external_id FROM am_provider_identities WHERE workspace_id=? AND provider=? AND credential_id=?",
        params![workspace, provider, p.credential],
    )?;
    let mut accounts = rows(
        db,
        "SELECT external_id,email FROM am_oauth_accounts WHERE workspace_id=? AND provider=? AND state='active'",
        params![workspace, provider],
    )?;
    accounts.extend(rows(
        db,
        "SELECT external_id,email FROM am_provider_emails WHERE workspace_id=? AND provider=?",
        params![workspace, provider],
    )?);
    for grant in permission["principals"].as_array().into_iter().flatten() {
        if grant["type"] == "anyone" {
            return Ok(true);
        }
        for identity in &identities {
            if grant["type"] == "user" && grant["id"] == identity["external_id"] {
                return Ok(true);
            }
            if let Some(account) = accounts
                .iter()
                .find(|a| a["external_id"] == identity["external_id"])
            {
                let email = s(account, "email").to_lowercase();
                if !email.is_empty()
                    && grant["type"] == "email"
                    && s(grant, "email").eq_ignore_ascii_case(&email)
                {
                    return Ok(true);
                }
                if !email.is_empty()
                    && grant["type"] == "domain"
                    && email
                        .rsplit_once('@')
                        .is_some_and(|(_, domain)| domain.eq_ignore_ascii_case(s(grant, "domain")))
                {
                    return Ok(true);
                }
            }
        }
    }
    Ok(false)
}
