CREATE TABLE IF NOT EXISTS am_original_documents (
 repository_id TEXT NOT NULL REFERENCES repo_projects(id) ON DELETE CASCADE,
 connector_id TEXT REFERENCES am_connectors(id) ON DELETE CASCADE,
 source_id TEXT NOT NULL, sha256 TEXT NOT NULL, name TEXT NOT NULL, parser TEXT NOT NULL,
 text_sha256 TEXT NOT NULL, bytes BLOB NOT NULL, created_at TEXT NOT NULL,
 PRIMARY KEY(repository_id,source_id,sha256));
CREATE TABLE IF NOT EXISTS am_secrets (
 id TEXT PRIMARY KEY, workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
 name TEXT NOT NULL, ciphertext BLOB NOT NULL, nonce BLOB NOT NULL, updated_at TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS am_oauth_pending (
 state_hash TEXT PRIMARY KEY, workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
 principal_hash TEXT NOT NULL, provider TEXT NOT NULL, redirect_uri TEXT NOT NULL,
 verifier_secret TEXT NOT NULL REFERENCES am_secrets(id) ON DELETE CASCADE,
 expires_at TEXT NOT NULL, used INTEGER NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS am_oauth_accounts (
 id TEXT PRIMARY KEY, workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
 provider TEXT NOT NULL, external_id TEXT NOT NULL, email TEXT, display_name TEXT NOT NULL,
 token_secret TEXT NOT NULL REFERENCES am_secrets(id) ON DELETE CASCADE,
 refresh_secret TEXT REFERENCES am_secrets(id) ON DELETE SET NULL,
 expires_at TEXT, scopes TEXT NOT NULL, state TEXT NOT NULL, updated_at TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS am_provider_identities (
 workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
 provider TEXT NOT NULL, external_id TEXT NOT NULL, credential_id TEXT NOT NULL,
 verified_at TEXT NOT NULL, PRIMARY KEY(workspace_id,provider,external_id,credential_id));
CREATE TABLE IF NOT EXISTS am_source_permissions (
 connector_id TEXT NOT NULL REFERENCES am_connectors(id) ON DELETE CASCADE,
 source_id TEXT NOT NULL, principals TEXT NOT NULL, checked_at TEXT NOT NULL,
 complete INTEGER NOT NULL, generation INTEGER NOT NULL,
 PRIMARY KEY(connector_id,source_id));
CREATE TABLE IF NOT EXISTS am_language_servers (
 repository_id TEXT NOT NULL REFERENCES repo_projects(id) ON DELETE CASCADE,
 language TEXT NOT NULL, root TEXT NOT NULL, executable TEXT NOT NULL, arguments TEXT NOT NULL,
 initialization TEXT NOT NULL, PRIMARY KEY(repository_id,language));
CREATE TABLE IF NOT EXISTS am_binary_heads (
 repository_id TEXT NOT NULL REFERENCES repo_projects(id) ON DELETE CASCADE,
 path TEXT NOT NULL, source_sha TEXT NOT NULL, original_sha TEXT NOT NULL, parser TEXT NOT NULL,
 PRIMARY KEY(repository_id,path));
CREATE TABLE IF NOT EXISTS am_model_profiles (
 workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
 name TEXT NOT NULL, profile TEXT NOT NULL, is_default INTEGER NOT NULL, is_embedding INTEGER NOT NULL,
 PRIMARY KEY(workspace_id,name));
CREATE TABLE IF NOT EXISTS am_backends (
 repository_id TEXT NOT NULL REFERENCES repo_projects(id) ON DELETE CASCADE,
 kind TEXT NOT NULL, config TEXT NOT NULL, PRIMARY KEY(repository_id,kind));
CREATE TABLE IF NOT EXISTS am_provider_emails (
 workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
 provider TEXT NOT NULL, external_id TEXT NOT NULL, email TEXT NOT NULL,
 verified_at TEXT NOT NULL, PRIMARY KEY(workspace_id,provider,external_id));
CREATE TABLE IF NOT EXISTS am_identity_audit (
 id TEXT PRIMARY KEY, workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
 actor_credential TEXT NOT NULL, provider TEXT NOT NULL, external_id TEXT NOT NULL,
 target_credential TEXT NOT NULL, action TEXT NOT NULL, reason TEXT NOT NULL, created_at TEXT NOT NULL);
