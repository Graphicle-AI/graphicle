-- Request accounting contains counters only, never source text, credentials or prompts.
CREATE TABLE IF NOT EXISTS am_usage (
 workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
 day TEXT NOT NULL, requests INTEGER NOT NULL DEFAULT 0,
 errors INTEGER NOT NULL DEFAULT 0, elapsed_ms INTEGER NOT NULL DEFAULT 0,
 PRIMARY KEY(workspace_id,day));
CREATE TABLE IF NOT EXISTS am_product_policy (
 workspace_id TEXT PRIMARY KEY REFERENCES workspaces(id) ON DELETE CASCADE,
 monthly_request_budget INTEGER CHECK(monthly_request_budget >= 0),
 updated_at TEXT NOT NULL);
-- What Graphicle served (crate::metering): counters only, never a question,
-- a result, a source id or a client's own session id.
CREATE TABLE IF NOT EXISTS am_served_usage (
 repository_id TEXT NOT NULL REFERENCES repo_projects(id) ON DELETE CASCADE,
 workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
 day TEXT NOT NULL, credential_id TEXT NOT NULL, session TEXT NOT NULL,
 channel TEXT NOT NULL, kind TEXT NOT NULL,
 calls INTEGER NOT NULL DEFAULT 0, tokens INTEGER NOT NULL DEFAULT 0,
 PRIMARY KEY(repository_id,day,credential_id,session,channel,kind));
CREATE INDEX IF NOT EXISTS am_served_usage_workspace ON am_served_usage(workspace_id,day);
-- A credential's month (an installed edition's quotas, the reports).
CREATE INDEX IF NOT EXISTS am_served_usage_credential ON am_served_usage(workspace_id,credential_id,day);
