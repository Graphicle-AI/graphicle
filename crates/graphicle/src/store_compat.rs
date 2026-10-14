//! Store compatibility with the team and commerce tables of the commercial edition.
//!
//! Core never creates these tables, but a store that the commercial edition has used,
//! or one made by a program built before the editions were split, holds them.
//! Core keeps every guarantee those programs kept on such a store, so opening
//! it with Core never weakens what the commercial edition enforces when it
//! opens the store, or a backup of it, again:
//! - revoking a member's credential deactivates the member and ends their
//!   browser sessions ([`credential_revoked`]), so that edition keeps it
//!   revoked;
//! - a credential linked to a member is usable only while the member is
//!   active and not deleted and team sign-in is enabled
//!   ([`credential_allowed`]); queued jobs re-check this at every lease;
//! - a portable backup carries no browser session, provisioning token or
//!   checkout intent, has team sign-in disabled and asks the owner to
//!   reconcile a subscription ([`scrub_backup`]);
//! - verified deletion treats the team's access history as history
//!   ([`HISTORY_TABLES`], [`RETAINED_TABLES`]), so `deletion.state` of a store
//!   does not depend on the edition that reads it.
//!
//! Each statement is the one the pre-split programs ran, and it runs only
//! when the store has its table, so a store Core created is never changed.
//! The table names here are intentional vocabulary: they describe data that
//! may already be in a user's store, not features of Core.
use crate::{Result, now, store::one};
use rusqlite::{Connection, params};

/// The team tables of access history: sign-in attempts, browser sessions
/// and the audit trail.
pub const HISTORY_TABLES: &[&str] = &["am_team_audit", "am_login_pending", "am_browser_sessions"];
/// The team audit trail, which deliberately keeps naming what it audited.
pub const RETAINED_TABLES: &[&str] = &["am_team_audit"];

/// Whether this store has `table`.
fn has(db: &Connection, table: &str) -> Result<bool> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?)",
        [table],
        |r| r.get(0),
    )?)
}

/// Append each of `tables` that this store has and `list` does not name yet.
pub fn add_present(
    db: &Connection,
    list: &mut Vec<&'static str>,
    tables: &[&'static str],
) -> Result<()> {
    for &table in tables {
        if !list.contains(&table) && has(db, table)? {
            list.push(table);
        }
    }
    Ok(())
}

/// A credential was revoked, inside the revoking transaction: a member linked
/// to it is deactivated and their browser sessions end. The commercial edition
/// treats an active member as authority to restore the member's credential, so
/// leaving the member active would undo the revocation once it uses the store.
pub fn credential_revoked(db: &Connection, workspace: &str, credential: &str) -> Result<()> {
    if !has(db, "am_members")? {
        return Ok(());
    }
    db.execute(
        "UPDATE am_members SET active=0,updated_at=? WHERE credential_id=? AND workspace_id=?",
        params![now(), credential, workspace],
    )?;
    if has(db, "am_browser_sessions")? {
        db.execute("DELETE FROM am_browser_sessions WHERE member_id IN (SELECT id FROM am_members WHERE credential_id=? AND workspace_id=?)",params![credential,workspace])?;
    }
    Ok(())
}

/// Whether a workspace credential may be used as far as the team tables
/// say: a credential linked to a member only while the member is active and
/// not deleted and team sign-in is enabled for its workspace. It fails
/// closed: a member without an enabled sign-in configuration is refused.
pub fn credential_allowed(db: &Connection, credential: &str) -> Result<bool> {
    if !has(db, "am_members")? {
        return Ok(true);
    }
    let query = if has(db, "am_sso")? {
        "SELECT m.active,m.deleted_at,coalesce(o.enabled,0) AS enabled FROM am_members m LEFT JOIN am_sso o ON o.workspace_id=m.workspace_id WHERE m.credential_id=?"
    } else {
        "SELECT m.active,m.deleted_at,0 AS enabled FROM am_members m WHERE m.credential_id=?"
    };
    Ok(one(db, query, [credential])?.is_none_or(|member| {
        member["active"] == 1 && member["enabled"] == 1 && member["deleted_at"].is_null()
    }))
}

/// Scrub a portable backup copy (never the live store): no browser session,
/// provisioning token or checkout intent, team sign-in off until an owner
/// enables it again, and no paid entitlement the copy cannot attest (the
/// customer binding stays for owner-triggered reconciliation).
pub fn scrub_backup(db: &Connection) -> Result<()> {
    let mut statements = Vec::new();
    for (table, statement) in [
        ("am_browser_sessions", "DELETE FROM am_browser_sessions;"),
        ("am_scim_tokens", "DELETE FROM am_scim_tokens;"),
        ("am_sso", "UPDATE am_sso SET enabled=0;"),
        ("am_checkout_intents", "DELETE FROM am_checkout_intents;"),
        (
            "am_subscriptions",
            "UPDATE am_subscriptions SET status='restore_required',request_allowance=0 WHERE subscription_id IS NOT NULL;",
        ),
    ] {
        if has(db, table)? {
            statements.push(statement);
        }
    }
    if statements.is_empty() {
        return Ok(());
    }
    db.execute_batch(&format!(
        "PRAGMA foreign_keys=ON; PRAGMA secure_delete=ON; BEGIN IMMEDIATE; {} COMMIT;",
        statements.join(" ")
    ))?;
    Ok(())
}
