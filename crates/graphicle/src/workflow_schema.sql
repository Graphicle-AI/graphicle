CREATE TABLE IF NOT EXISTS am_jobs (
 id TEXT PRIMARY KEY, repository_id TEXT NOT NULL REFERENCES repo_projects(id) ON DELETE CASCADE,
 kind TEXT NOT NULL, input_hash TEXT NOT NULL, payload TEXT NOT NULL, principal_hash TEXT NOT NULL,
 state TEXT NOT NULL, priority INTEGER NOT NULL DEFAULT 0, attempt INTEGER NOT NULL DEFAULT 0,
 max_attempts INTEGER NOT NULL DEFAULT 3, not_before TEXT NOT NULL, lease_until TEXT, lease_token TEXT,
 result TEXT, error TEXT, created_at TEXT NOT NULL, updated_at TEXT NOT NULL,
 UNIQUE(repository_id,kind,input_hash));
CREATE INDEX IF NOT EXISTS am_jobs_ready ON am_jobs(state,not_before,priority);
CREATE TABLE IF NOT EXISTS am_model_calls (
 id TEXT PRIMARY KEY, repository_id TEXT NOT NULL REFERENCES repo_projects(id) ON DELETE CASCADE,
 job_id TEXT REFERENCES am_jobs(id) ON DELETE CASCADE, provider TEXT NOT NULL, model TEXT NOT NULL,
 request_hash TEXT NOT NULL, input_tokens INTEGER NOT NULL, output_tokens INTEGER NOT NULL,
 elapsed_ms INTEGER NOT NULL, cached INTEGER NOT NULL, created_at TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS am_call_reservations (
 id TEXT PRIMARY KEY, job_id TEXT NOT NULL REFERENCES am_jobs(id) ON DELETE CASCADE,
 request_hash TEXT NOT NULL, state TEXT NOT NULL, input_reserved INTEGER NOT NULL,
 output_reserved INTEGER NOT NULL, usage_reported INTEGER NOT NULL DEFAULT 0, error TEXT,
 created_at TEXT NOT NULL, finished_at TEXT);
CREATE TABLE IF NOT EXISTS am_model_cache (
 repository_id TEXT NOT NULL REFERENCES repo_projects(id) ON DELETE CASCADE,
 request_hash TEXT NOT NULL, source_state TEXT NOT NULL, result TEXT NOT NULL, created_at TEXT NOT NULL,
 PRIMARY KEY(repository_id,request_hash));
CREATE TABLE IF NOT EXISTS am_query_cache (
 repository_id TEXT NOT NULL REFERENCES repo_projects(id) ON DELETE CASCADE,
 request_hash TEXT NOT NULL, result TEXT NOT NULL, PRIMARY KEY(repository_id,request_hash));
CREATE TABLE IF NOT EXISTS am_claims (
 id TEXT PRIMARY KEY, repository_id TEXT NOT NULL REFERENCES repo_projects(id) ON DELETE CASCADE,
 memory_id TEXT REFERENCES repo_memories(id) ON DELETE CASCADE,
 state TEXT NOT NULL, data TEXT NOT NULL, source_state TEXT NOT NULL,
 created_at TEXT NOT NULL, updated_at TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS am_claims_project ON am_claims(repository_id,state);
CREATE TABLE IF NOT EXISTS am_entities (
 id TEXT PRIMARY KEY, repository_id TEXT NOT NULL REFERENCES repo_projects(id) ON DELETE CASCADE,
 name TEXT NOT NULL, kind TEXT NOT NULL, data TEXT NOT NULL, created_at TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS am_entity_links (
 repository_id TEXT NOT NULL REFERENCES repo_projects(id) ON DELETE CASCADE,
 claim_id TEXT NOT NULL REFERENCES am_claims(id) ON DELETE CASCADE,
 subject_id TEXT NOT NULL REFERENCES am_entities(id) ON DELETE CASCADE,
 object_id TEXT NOT NULL REFERENCES am_entities(id) ON DELETE CASCADE,
 PRIMARY KEY(repository_id,claim_id));
CREATE TABLE IF NOT EXISTS am_alias_proposals (
 id TEXT PRIMARY KEY, repository_id TEXT NOT NULL REFERENCES repo_projects(id) ON DELETE CASCADE,
 source_id TEXT NOT NULL REFERENCES am_entities(id) ON DELETE CASCADE,
 target_id TEXT NOT NULL REFERENCES am_entities(id) ON DELETE CASCADE,
 state TEXT NOT NULL, data TEXT NOT NULL, created_at TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS am_vectors (
 repository_id TEXT NOT NULL REFERENCES repo_projects(id) ON DELETE CASCADE,
 identity TEXT NOT NULL, source_sha TEXT NOT NULL, model_key TEXT NOT NULL,
 dimensions INTEGER NOT NULL, vector TEXT NOT NULL, PRIMARY KEY(repository_id,identity,model_key));
CREATE TABLE IF NOT EXISTS am_communities (
 id TEXT PRIMARY KEY, repository_id TEXT NOT NULL REFERENCES repo_projects(id) ON DELETE CASCADE,
 source_state TEXT NOT NULL, level INTEGER NOT NULL, parent_id TEXT, data TEXT NOT NULL,
 created_at TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS am_reports (
 id TEXT PRIMARY KEY, repository_id TEXT NOT NULL REFERENCES repo_projects(id) ON DELETE CASCADE,
 community_id TEXT NOT NULL REFERENCES am_communities(id) ON DELETE CASCADE,
 source_state TEXT NOT NULL, data TEXT NOT NULL, created_at TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS am_connectors (
 id TEXT PRIMARY KEY, repository_id TEXT NOT NULL REFERENCES repo_projects(id) ON DELETE CASCADE,
 name TEXT NOT NULL, kind TEXT NOT NULL, config TEXT NOT NULL, credential_env TEXT,
 state TEXT NOT NULL, cursor TEXT, generation INTEGER NOT NULL DEFAULT 0, last_sync TEXT,
 error TEXT, created_at TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS am_connector_docs (
 connector_id TEXT NOT NULL REFERENCES am_connectors(id) ON DELETE CASCADE,
 id TEXT NOT NULL, content TEXT NOT NULL, sha256 TEXT NOT NULL, metadata TEXT NOT NULL,
 acl TEXT NOT NULL, present INTEGER NOT NULL DEFAULT 1, generation INTEGER NOT NULL,
 PRIMARY KEY(connector_id,id));
CREATE TABLE IF NOT EXISTS am_connector_versions (
 connector_id TEXT NOT NULL REFERENCES am_connectors(id) ON DELETE CASCADE,
 id TEXT NOT NULL, sha256 TEXT NOT NULL, content TEXT NOT NULL, metadata TEXT NOT NULL, created_at TEXT NOT NULL,
 PRIMARY KEY(connector_id,id,sha256));
CREATE TABLE IF NOT EXISTS am_connector_stage (
 connector_id TEXT NOT NULL REFERENCES am_connectors(id) ON DELETE CASCADE,
 scan_id TEXT NOT NULL, id TEXT NOT NULL, content TEXT NOT NULL, sha256 TEXT NOT NULL,
 metadata TEXT NOT NULL, acl TEXT NOT NULL, PRIMARY KEY(connector_id,scan_id,id));
CREATE TABLE IF NOT EXISTS am_document_reads (
 repository_id TEXT NOT NULL REFERENCES repo_projects(id) ON DELETE CASCADE,
 source_id TEXT NOT NULL, source_sha TEXT NOT NULL, model_key TEXT NOT NULL,
 context_hash TEXT NOT NULL, claim_ids TEXT NOT NULL, created_at TEXT NOT NULL,
 PRIMARY KEY(repository_id,source_id,source_sha,model_key,context_hash));
CREATE TABLE IF NOT EXISTS am_claim_events (
 id TEXT PRIMARY KEY, repository_id TEXT NOT NULL REFERENCES repo_projects(id) ON DELETE CASCADE,
 claim_id TEXT NOT NULL REFERENCES am_claims(id) ON DELETE CASCADE,
 action TEXT NOT NULL, data TEXT NOT NULL, created_at TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS am_preparation_steps (
 repository_id TEXT NOT NULL REFERENCES repo_projects(id) ON DELETE CASCADE,
 step_key TEXT NOT NULL, kind TEXT NOT NULL, data TEXT NOT NULL, created_at TEXT NOT NULL,
 PRIMARY KEY(repository_id,step_key));
-- Observer notes (observer.rs): model-written, dated observations of one source
-- unit each. Derived evidence only; deleted when their source changes or goes.
CREATE TABLE IF NOT EXISTS am_observer_notes (
 repository_id TEXT NOT NULL REFERENCES repo_projects(id) ON DELETE CASCADE,
 source_id TEXT NOT NULL, source_sha256 TEXT NOT NULL, source_version TEXT NOT NULL,
 step_key TEXT NOT NULL, model_key TEXT NOT NULL, principal_hash TEXT NOT NULL, job_id TEXT NOT NULL,
 source_order TEXT NOT NULL, session_date TEXT, notes TEXT NOT NULL, dropped INTEGER NOT NULL,
 created_at TEXT NOT NULL, PRIMARY KEY(repository_id,source_id));
CREATE INDEX IF NOT EXISTS am_observer_notes_source ON am_observer_notes(source_id);
-- Observer reflection: no code writes it now. The table stays so the
-- rows earlier releases wrote are still deleted with any source they used
-- (`sources` lists their source ids; `inputs` maps note ids to digests).
CREATE TABLE IF NOT EXISTS am_observer_reflections (
 repository_id TEXT PRIMARY KEY REFERENCES repo_projects(id) ON DELETE CASCADE,
 step_key TEXT NOT NULL, model_key TEXT NOT NULL, principal_hash TEXT NOT NULL, job_id TEXT NOT NULL,
 items TEXT NOT NULL, inputs TEXT NOT NULL, sources TEXT NOT NULL, dropped INTEGER NOT NULL,
 created_at TEXT NOT NULL);
-- Verified deletion receipts (deletion.rs), hash-chained per workspace. No
-- project foreign key: a receipt outlives the project it describes.
CREATE TABLE IF NOT EXISTS am_deletion_receipts (
 id TEXT PRIMARY KEY, workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
 sequence INTEGER NOT NULL, repository_id TEXT NOT NULL, operation TEXT NOT NULL, target TEXT NOT NULL,
 body TEXT NOT NULL, digest TEXT NOT NULL, previous TEXT, created_at TEXT NOT NULL,
 UNIQUE(workspace_id,sequence));
-- One-way hashes of forgotten connector documents (deletion.rs): imports and
-- syncs skip them, so a deleted document is not silently brought back.
CREATE TABLE IF NOT EXISTS am_forgotten_sources (
 connector_id TEXT NOT NULL REFERENCES am_connectors(id) ON DELETE CASCADE,
 id_hash TEXT NOT NULL, forgotten_at TEXT NOT NULL, PRIMARY KEY(connector_id,id_hash));
