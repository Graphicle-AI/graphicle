-- The team and commerce tables as the commercial edition creates them in a
-- store, for the store-compatibility suite (tests/it/store_compat.rs). Core
-- never creates them. That edition's tests check that the statements below
-- equal its own schema files, byte for byte.
CREATE TABLE IF NOT EXISTS am_sso (
 workspace_id TEXT PRIMARY KEY REFERENCES workspaces(id) ON DELETE CASCADE,
 config TEXT NOT NULL, enabled INTEGER NOT NULL, updated_at TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS am_members (
 id TEXT PRIMARY KEY, workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
 credential_id TEXT NOT NULL UNIQUE REFERENCES credentials(id) ON DELETE CASCADE,
 external_id TEXT, username TEXT NOT NULL, display_name TEXT NOT NULL,
 issuer TEXT, subject TEXT, active INTEGER NOT NULL, base_role TEXT NOT NULL,
 created_at TEXT NOT NULL, updated_at TEXT NOT NULL, deleted_at TEXT);
CREATE UNIQUE INDEX IF NOT EXISTS am_member_username ON am_members(workspace_id,username) WHERE deleted_at IS NULL;
CREATE UNIQUE INDEX IF NOT EXISTS am_member_external ON am_members(workspace_id,external_id) WHERE deleted_at IS NULL;
CREATE UNIQUE INDEX IF NOT EXISTS am_member_subject ON am_members(workspace_id,issuer,subject) WHERE deleted_at IS NULL;
CREATE TABLE IF NOT EXISTS am_groups (
 id TEXT PRIMARY KEY, workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
 external_id TEXT, display_name TEXT NOT NULL, created_at TEXT NOT NULL, updated_at TEXT NOT NULL,
 UNIQUE(workspace_id,external_id));
CREATE TABLE IF NOT EXISTS am_group_members (
 group_id TEXT NOT NULL REFERENCES am_groups(id) ON DELETE CASCADE,
 member_id TEXT NOT NULL REFERENCES am_members(id) ON DELETE CASCADE,
 PRIMARY KEY(group_id,member_id));
CREATE TABLE IF NOT EXISTS am_login_pending (
 state_hash TEXT PRIMARY KEY, workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
 binding_hash TEXT NOT NULL, secret_id TEXT NOT NULL REFERENCES am_secrets(id) ON DELETE CASCADE,
 config_hash TEXT NOT NULL, redirect_uri TEXT NOT NULL, expires_at TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS am_browser_sessions (
 token_hash TEXT PRIMARY KEY, workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
 member_id TEXT NOT NULL REFERENCES am_members(id) ON DELETE CASCADE,
 created_at TEXT NOT NULL, expires_at TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS am_browser_sessions_member ON am_browser_sessions(member_id);
CREATE TABLE IF NOT EXISTS am_scim_tokens (
 workspace_id TEXT PRIMARY KEY REFERENCES workspaces(id) ON DELETE CASCADE,
 token_hash TEXT NOT NULL, created_at TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS am_team_audit (
 id TEXT PRIMARY KEY, workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
 actor TEXT NOT NULL, action TEXT NOT NULL, target_id TEXT NOT NULL,
 data TEXT NOT NULL, created_at TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS am_subscriptions (
 workspace_id TEXT PRIMARY KEY REFERENCES workspaces(id) ON DELETE CASCADE,
 customer_id TEXT UNIQUE, subscription_id TEXT UNIQUE,
 price_id TEXT, status TEXT NOT NULL DEFAULT 'unconfigured',
 request_allowance INTEGER CHECK(request_allowance >= 0),
 event_created INTEGER NOT NULL DEFAULT 0, event_id TEXT NOT NULL DEFAULT '',
 updated_at TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS am_billing_events (
 id TEXT PRIMARY KEY, workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
 created INTEGER NOT NULL, received_at TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS am_checkout_intents (
 workspace_id TEXT PRIMARY KEY REFERENCES workspaces(id) ON DELETE CASCADE,
 id TEXT NOT NULL UNIQUE, price_id TEXT NOT NULL, expires_at INTEGER NOT NULL,
 session_id TEXT, url TEXT);
