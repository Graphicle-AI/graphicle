CREATE TABLE IF NOT EXISTS am_projection_cleanup (
 id TEXT PRIMARY KEY, workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
 scope TEXT NOT NULL, kind TEXT NOT NULL, config TEXT NOT NULL, config_hash TEXT NOT NULL,
 revision INTEGER NOT NULL, state TEXT NOT NULL, attempts INTEGER NOT NULL DEFAULT 0,
 lease_token TEXT, lease_until TEXT, not_before TEXT NOT NULL,
 reason TEXT NOT NULL, error TEXT, created_at TEXT NOT NULL, updated_at TEXT NOT NULL,
 UNIQUE(scope,kind,config_hash));
