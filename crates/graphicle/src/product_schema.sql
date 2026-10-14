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
