-- Compatible source and revision tables from the existing Graphicle host.
-- New workflow tables are appended by this Rust service.

-- store.py

                CREATE TABLE IF NOT EXISTS workspaces (
                    id TEXT PRIMARY KEY, capability_hash TEXT NOT NULL, name TEXT NOT NULL,
                    created_at TEXT NOT NULL, version INTEGER NOT NULL DEFAULT 0);
                CREATE TABLE IF NOT EXISTS sources (
                    id TEXT PRIMARY KEY, workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
                    name TEXT NOT NULL, role TEXT NOT NULL, content TEXT NOT NULL, sha256 TEXT NOT NULL,
                    created_at TEXT NOT NULL, version INTEGER NOT NULL);
                CREATE TABLE IF NOT EXISTS reviews (
                    id TEXT PRIMARY KEY, workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
                    source_version INTEGER NOT NULL, data TEXT NOT NULL, created_at TEXT NOT NULL);
                CREATE TABLE IF NOT EXISTS decisions (
                    workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
                    review_id TEXT NOT NULL REFERENCES reviews(id) ON DELETE CASCADE,
                    change_id TEXT NOT NULL, status TEXT NOT NULL, note TEXT NOT NULL, updated_at TEXT NOT NULL,
                    PRIMARY KEY (review_id, change_id));
                CREATE TABLE IF NOT EXISTS indices (
                    workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
                    source_hash TEXT NOT NULL, manifest_sha256 TEXT NOT NULL,
                    PRIMARY KEY (workspace_id, source_hash));
                CREATE TABLE IF NOT EXISTS source_heads (
                    workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
                    name TEXT NOT NULL, role TEXT NOT NULL,
                    source_id TEXT REFERENCES sources(id) ON DELETE SET NULL,
                    PRIMARY KEY (workspace_id, name, role));
                CREATE INDEX IF NOT EXISTS sources_workspace ON sources(workspace_id, version);
                CREATE TABLE IF NOT EXISTS credentials (
                    id TEXT PRIMARY KEY, workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
                    capability_hash TEXT NOT NULL UNIQUE, role TEXT NOT NULL, label TEXT NOT NULL,
                    created_at TEXT NOT NULL, expires_at TEXT, revoked_at TEXT);
                INSERT OR IGNORE INTO source_heads (workspace_id,name,role,source_id)
                    SELECT s.workspace_id,s.name,s.role,s.id FROM sources s
                    WHERE s.version=(SELECT max(latest.version) FROM sources latest
                        WHERE latest.workspace_id=s.workspace_id AND latest.name=s.name AND latest.role=s.role);
            
-- repository.py

                CREATE TABLE IF NOT EXISTS repo_projects (
                    id TEXT PRIMARY KEY, workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
                    project_key TEXT NOT NULL, name TEXT NOT NULL, snapshot TEXT NOT NULL DEFAULT '',
                    generation INTEGER NOT NULL DEFAULT 0, metadata TEXT NOT NULL DEFAULT '{}',
                    created_at TEXT NOT NULL, updated_at TEXT NOT NULL, UNIQUE(workspace_id,project_key));
                CREATE TABLE IF NOT EXISTS repo_objects (
                    repository_id TEXT NOT NULL REFERENCES repo_projects(id) ON DELETE CASCADE,
                    sha256 TEXT NOT NULL, content TEXT NOT NULL, bytes INTEGER NOT NULL,
                    PRIMARY KEY(repository_id,sha256));
                CREATE TABLE IF NOT EXISTS repo_parses (
                    repository_id TEXT NOT NULL, path TEXT NOT NULL, sha256 TEXT NOT NULL,
                    revision TEXT NOT NULL, data TEXT NOT NULL, PRIMARY KEY(repository_id,path,sha256),
                    FOREIGN KEY(repository_id,sha256) REFERENCES repo_objects(repository_id,sha256) ON DELETE CASCADE);
                CREATE TABLE IF NOT EXISTS repo_heads (
                    repository_id TEXT NOT NULL, path TEXT NOT NULL, sha256 TEXT NOT NULL,
                    PRIMARY KEY(repository_id,path),
                    FOREIGN KEY(repository_id,sha256) REFERENCES repo_objects(repository_id,sha256) ON DELETE CASCADE);
                CREATE TABLE IF NOT EXISTS repo_snapshots (
                    repository_id TEXT NOT NULL REFERENCES repo_projects(id) ON DELETE CASCADE,
                    sha256 TEXT NOT NULL, manifest TEXT NOT NULL, metadata TEXT NOT NULL,
                    created_at TEXT NOT NULL, PRIMARY KEY(repository_id,sha256));
                CREATE TABLE IF NOT EXISTS repo_syncs (
                    id TEXT PRIMARY KEY, repository_id TEXT NOT NULL REFERENCES repo_projects(id) ON DELETE CASCADE,
                    expected_snapshot TEXT NOT NULL, snapshot TEXT NOT NULL, manifest TEXT NOT NULL,
                    metadata TEXT NOT NULL, state TEXT NOT NULL, created_at TEXT NOT NULL);
                CREATE TABLE IF NOT EXISTS repo_events (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    repository_id TEXT NOT NULL REFERENCES repo_projects(id) ON DELETE CASCADE,
                    kind TEXT NOT NULL, data TEXT NOT NULL, created_at TEXT NOT NULL);
                CREATE TABLE IF NOT EXISTS repo_chunks (
                    id INTEGER PRIMARY KEY AUTOINCREMENT, repository_id TEXT NOT NULL,
                    path TEXT NOT NULL, sha256 TEXT NOT NULL, line INTEGER NOT NULL, end_line INTEGER NOT NULL,
                    index_text TEXT NOT NULL, UNIQUE(repository_id,path,sha256,line),
                    FOREIGN KEY(repository_id,sha256) REFERENCES repo_objects(repository_id,sha256) ON DELETE CASCADE);
                CREATE VIRTUAL TABLE IF NOT EXISTS repo_search USING fts5(index_text,
                    content='repo_chunks', content_rowid='id', tokenize='porter unicode61');
                CREATE TRIGGER IF NOT EXISTS repo_chunks_insert AFTER INSERT ON repo_chunks BEGIN
                    INSERT INTO repo_search(rowid,index_text) VALUES(new.id,new.index_text);
                END;
                CREATE TRIGGER IF NOT EXISTS repo_chunks_delete AFTER DELETE ON repo_chunks BEGIN
                    INSERT INTO repo_search(repo_search,rowid,index_text) VALUES('delete',old.id,old.index_text);
                END;
                CREATE INDEX IF NOT EXISTS repo_chunks_lookup ON repo_chunks(repository_id,path,sha256);
                -- Sizes without reading content: a sync's missing-object check
                -- and the retained-sources total (the Core limits).
                CREATE INDEX IF NOT EXISTS repo_objects_size ON repo_objects(repository_id,sha256,bytes);
            
-- repository_experience.py

                CREATE TABLE IF NOT EXISTS repo_runs (
                    id TEXT PRIMARY KEY, repository_id TEXT NOT NULL REFERENCES repo_projects(id) ON DELETE CASCADE,
                    external_key TEXT NOT NULL, session_key TEXT NOT NULL, title TEXT NOT NULL,
                    start_snapshot TEXT NOT NULL, end_snapshot TEXT NOT NULL DEFAULT '',
                    state TEXT NOT NULL, dependencies TEXT NOT NULL DEFAULT '{}',
                    created_at TEXT NOT NULL, finished_at TEXT, UNIQUE(repository_id,external_key));
                CREATE TABLE IF NOT EXISTS repo_tool_events (
                    sequence INTEGER PRIMARY KEY AUTOINCREMENT,
                    run_id TEXT NOT NULL REFERENCES repo_runs(id) ON DELETE CASCADE,
                    event_key TEXT NOT NULL, data TEXT NOT NULL, created_at TEXT NOT NULL,
                    UNIQUE(run_id,event_key));
                CREATE TABLE IF NOT EXISTS repo_memories (
                    id TEXT PRIMARY KEY, repository_id TEXT NOT NULL REFERENCES repo_projects(id) ON DELETE CASCADE,
                    kind TEXT NOT NULL, revision INTEGER NOT NULL, state TEXT NOT NULL,
                    title TEXT NOT NULL, body TEXT NOT NULL, data TEXT NOT NULL,
                    created_at TEXT NOT NULL, updated_at TEXT NOT NULL);
                CREATE TABLE IF NOT EXISTS repo_memory_revisions (
                    memory_id TEXT NOT NULL REFERENCES repo_memories(id) ON DELETE CASCADE,
                    revision INTEGER NOT NULL, data TEXT NOT NULL, created_at TEXT NOT NULL,
                    PRIMARY KEY(memory_id,revision));
                CREATE INDEX IF NOT EXISTS repo_runs_lookup ON repo_runs(repository_id,session_key,state);
                CREATE INDEX IF NOT EXISTS repo_memories_lookup ON repo_memories(repository_id,kind,state);
            
-- repository_evidence.py

                CREATE TABLE IF NOT EXISTS repo_evidence_links (
                    id TEXT PRIMARY KEY,
                    repository_id TEXT NOT NULL REFERENCES repo_projects(id) ON DELETE CASCADE,
                    kind TEXT NOT NULL, resource_id TEXT NOT NULL, follow_head INTEGER NOT NULL,
                    created_at TEXT NOT NULL, UNIQUE(repository_id,kind,resource_id));
            
-- graph_projection.rs: bi-temporal projection of the typed code graph

                CREATE TABLE IF NOT EXISTS gf_episodes (
                    repository_id TEXT NOT NULL REFERENCES repo_projects(id) ON DELETE CASCADE,
                    episode TEXT NOT NULL, sequence INTEGER NOT NULL, created_at TEXT NOT NULL,
                    node_count INTEGER NOT NULL, edge_count INTEGER NOT NULL,
                    directed INTEGER NOT NULL DEFAULT 1, multigraph INTEGER NOT NULL DEFAULT 0,
                    attributes TEXT NOT NULL DEFAULT '{}',
                    -- The `latest` episode when this one was recorded: history is a DAG
                    -- of tree digests. Added later; Store::open backfills the column on
                    -- databases created before it existed (nullable, no default).
                    parent_episode TEXT,
                    PRIMARY KEY(repository_id,episode), UNIQUE(repository_id,sequence));
                CREATE TABLE IF NOT EXISTS gf_nodes (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    repository_id TEXT NOT NULL REFERENCES repo_projects(id) ON DELETE CASCADE,
                    node_id TEXT NOT NULL, attributes TEXT NOT NULL,
                    valid_from_episode TEXT NOT NULL, valid_to_episode TEXT,
                    created_at TEXT NOT NULL, expired_at TEXT);
                CREATE INDEX IF NOT EXISTS gf_nodes_live ON gf_nodes(repository_id,node_id)
                    WHERE valid_to_episode IS NULL;
                CREATE INDEX IF NOT EXISTS gf_nodes_episode ON gf_nodes(repository_id,valid_from_episode);
                -- `key` is the multigraph edge key. The projection builds the typed graph
                -- as a multigraph keyed by relation (`key = relation`), because the
                -- extractors do emit parallel edges with distinct relations between one
                -- pair (`calls`+`contains` for a nested function, `inherits`+`uses`,
                -- `imports`+`imports_from`) which the parity non-multigraph build folds.
                -- Episodes recorded by the earlier non-multigraph projection keep `key = ''`.
                CREATE TABLE IF NOT EXISTS gf_edges (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    repository_id TEXT NOT NULL REFERENCES repo_projects(id) ON DELETE CASCADE,
                    source TEXT NOT NULL, target TEXT NOT NULL, relation TEXT NOT NULL,
                    key TEXT NOT NULL DEFAULT '', attributes TEXT NOT NULL,
                    valid_from_episode TEXT NOT NULL, valid_to_episode TEXT,
                    created_at TEXT NOT NULL, expired_at TEXT);
                CREATE INDEX IF NOT EXISTS gf_edges_live ON gf_edges(repository_id,source,target,relation,key)
                    WHERE valid_to_episode IS NULL;
                CREATE INDEX IF NOT EXISTS gf_edges_episode ON gf_edges(repository_id,valid_from_episode);
                -- Origin of every persisted edge row: the file it was extracted from,
                -- the location inside it and the identity derivation (G03 provenance).
                CREATE TABLE IF NOT EXISTS gf_edge_sources (
                    repository_id TEXT NOT NULL REFERENCES repo_projects(id) ON DELETE CASCADE,
                    edge_row_id INTEGER NOT NULL REFERENCES gf_edges(id) ON DELETE CASCADE,
                    relpath TEXT NOT NULL DEFAULT '', source_location TEXT NOT NULL DEFAULT '',
                    derivation TEXT NOT NULL DEFAULT '',
                    PRIMARY KEY(repository_id,edge_row_id));
                CREATE INDEX IF NOT EXISTS gf_edge_sources_relpath ON gf_edge_sources(repository_id,relpath);
                -- Contradiction markers (graphicle-memory temporal rule) for relations with
                -- declared cardinality one: `subject` moved from `old_target` to
                -- `new_target` in `episode`; the old edge row was closed by the same
                -- projection. Multi-valued relations never appear here.
                CREATE TABLE IF NOT EXISTS gf_contradictions (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    repository_id TEXT NOT NULL REFERENCES repo_projects(id) ON DELETE CASCADE,
                    episode TEXT NOT NULL, relation TEXT NOT NULL, subject TEXT NOT NULL,
                    old_target TEXT NOT NULL, new_target TEXT NOT NULL,
                    old_edge_row_id INTEGER NOT NULL, new_edge_row_id INTEGER NOT NULL,
                    recorded_at TEXT NOT NULL);
                CREATE INDEX IF NOT EXISTS gf_contradictions_episode ON gf_contradictions(repository_id,episode);

-- pr_report.rs: human labels (commit sha, "main", "base", ...) for recorded episodes

                CREATE TABLE IF NOT EXISTS gf_episode_labels (
                    workspace TEXT NOT NULL, repo TEXT NOT NULL, label TEXT NOT NULL,
                    episode_id TEXT NOT NULL, created_at TEXT NOT NULL,
                    PRIMARY KEY(workspace,repo,label));
                CREATE INDEX IF NOT EXISTS gf_episode_labels_episode ON gf_episode_labels(repo,episode_id);

-- invalidation.rs: dependency index and staleness markers for derived artifacts

                CREATE TABLE IF NOT EXISTS gf_dependencies (
                    workspace TEXT NOT NULL, dependent_kind TEXT NOT NULL, dependent_id TEXT NOT NULL,
                    source_kind TEXT NOT NULL, source_id TEXT NOT NULL, recorded_at TEXT NOT NULL,
                    PRIMARY KEY(workspace,dependent_kind,dependent_id,source_kind,source_id));
                -- Covering: the invalidation walk reads the dependent columns straight
                -- from the index. Replaces the narrower `gf_dependencies_source`, which
                -- the planner passed over for the primary key (a full scan per source).
                DROP INDEX IF EXISTS gf_dependencies_source;
                CREATE INDEX IF NOT EXISTS gf_dependencies_by_source
                    ON gf_dependencies(workspace,source_kind,source_id,dependent_kind,dependent_id);
                CREATE TABLE IF NOT EXISTS gf_stale (
                    workspace TEXT NOT NULL, kind TEXT NOT NULL, id TEXT NOT NULL,
                    reason TEXT NOT NULL, since TEXT NOT NULL,
                    PRIMARY KEY(workspace,kind,id));
                CREATE INDEX IF NOT EXISTS gf_stale_workspace ON gf_stale(workspace,kind);

-- G11/G12 (wave 2)
-- learning.rs: grounded observations and consolidated mental models per (workspace, repository, scope)

                CREATE TABLE IF NOT EXISTS gf_observations (
                    id TEXT PRIMARY KEY, workspace TEXT NOT NULL,
                    repository_id TEXT NOT NULL REFERENCES repo_projects(id) ON DELETE CASCADE,
                    sequence INTEGER NOT NULL, scope TEXT NOT NULL, kind TEXT NOT NULL, text TEXT NOT NULL,
                    grounding TEXT NOT NULL, source TEXT NOT NULL, created_at TEXT NOT NULL, retracted_at TEXT,
                    UNIQUE(repository_id,sequence));
                CREATE INDEX IF NOT EXISTS gf_observations_scope
                    ON gf_observations(workspace,repository_id,scope,sequence);
                CREATE TABLE IF NOT EXISTS gf_mental_models (
                    id TEXT PRIMARY KEY, workspace TEXT NOT NULL,
                    repository_id TEXT NOT NULL REFERENCES repo_projects(id) ON DELETE CASCADE,
                    scope TEXT NOT NULL, body TEXT NOT NULL, document TEXT NOT NULL,
                    observations TEXT NOT NULL DEFAULT '[]', watermark_seq INTEGER NOT NULL,
                    refreshed_at TEXT NOT NULL, version INTEGER NOT NULL DEFAULT 1,
                    UNIQUE(repository_id,scope));

-- hints.rs: served evidence, task outcomes, held-out questions and learned retrieval hints

                CREATE TABLE IF NOT EXISTS gf_served (
                    request_hash TEXT PRIMARY KEY, workspace TEXT NOT NULL,
                    repository_id TEXT NOT NULL REFERENCES repo_projects(id) ON DELETE CASCADE,
                    question TEXT NOT NULL, served TEXT NOT NULL, created_at TEXT NOT NULL);
                CREATE INDEX IF NOT EXISTS gf_served_repository ON gf_served(workspace,repository_id,created_at);
                CREATE TABLE IF NOT EXISTS gf_outcomes (
                    id TEXT PRIMARY KEY, workspace TEXT NOT NULL,
                    repository_id TEXT NOT NULL REFERENCES repo_projects(id) ON DELETE CASCADE,
                    request_hash TEXT NOT NULL, outcome TEXT NOT NULL, targets TEXT NOT NULL,
                    note TEXT NOT NULL DEFAULT '', created_at TEXT NOT NULL);
                CREATE INDEX IF NOT EXISTS gf_outcomes_repository ON gf_outcomes(workspace,repository_id,created_at);
                CREATE TABLE IF NOT EXISTS gf_heldout (
                    id TEXT PRIMARY KEY, workspace TEXT NOT NULL,
                    repository_id TEXT NOT NULL REFERENCES repo_projects(id) ON DELETE CASCADE,
                    question TEXT NOT NULL, expected TEXT NOT NULL, created_at TEXT NOT NULL);
                CREATE INDEX IF NOT EXISTS gf_heldout_repository ON gf_heldout(workspace,repository_id);
                CREATE TABLE IF NOT EXISTS gf_hints (
                    id TEXT PRIMARY KEY, workspace TEXT NOT NULL,
                    repository_id TEXT NOT NULL REFERENCES repo_projects(id) ON DELETE CASCADE,
                    scope TEXT NOT NULL, target TEXT NOT NULL, weight REAL NOT NULL, state TEXT NOT NULL,
                    evidence TEXT NOT NULL DEFAULT '{}', created_at TEXT NOT NULL, updated_at TEXT NOT NULL,
                    UNIQUE(repository_id,scope,target));
                CREATE INDEX IF NOT EXISTS gf_hints_state ON gf_hints(workspace,repository_id,state);

-- G06/G07/G10 (wave 2)

                CREATE TABLE IF NOT EXISTS gf_anchor_edges (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    repository_id TEXT NOT NULL REFERENCES repo_projects(id) ON DELETE CASCADE,
                    episode TEXT NOT NULL, chunk_id TEXT NOT NULL, node_id TEXT,
                    rung TEXT NOT NULL, confidence REAL, attributes TEXT NOT NULL,
                    created_at TEXT NOT NULL,
                    UNIQUE(repository_id,episode,chunk_id));
                CREATE INDEX IF NOT EXISTS gf_anchor_edges_episode ON gf_anchor_edges(repository_id,episode,rung);
                CREATE TABLE IF NOT EXISTS gf_alias_proposals (
                    id TEXT PRIMARY KEY,
                    repository_id TEXT NOT NULL REFERENCES repo_projects(id) ON DELETE CASCADE,
                    episode TEXT NOT NULL, from_node TEXT NOT NULL, to_node TEXT NOT NULL,
                    rung TEXT NOT NULL, score REAL NOT NULL, state TEXT NOT NULL,
                    folded TEXT NOT NULL DEFAULT '{}', created_at TEXT NOT NULL, decided_at TEXT,
                    UNIQUE(repository_id,from_node,to_node));
                CREATE INDEX IF NOT EXISTS gf_alias_proposals_state ON gf_alias_proposals(repository_id,state);
                CREATE TABLE IF NOT EXISTS gf_communities (
                    repository_id TEXT NOT NULL REFERENCES repo_projects(id) ON DELETE CASCADE,
                    episode TEXT NOT NULL, community_id TEXT NOT NULL, members TEXT NOT NULL,
                    previous_id TEXT, overlap REAL, created_at TEXT NOT NULL,
                    PRIMARY KEY(repository_id,episode,community_id));
                CREATE TABLE IF NOT EXISTS gf_community_ids (
                    repository_id TEXT NOT NULL REFERENCES repo_projects(id) ON DELETE CASCADE,
                    source_state TEXT NOT NULL, community_id TEXT NOT NULL, stable_id TEXT NOT NULL,
                    previous_id TEXT, overlap REAL, created_at TEXT NOT NULL,
                    PRIMARY KEY(repository_id,source_state,community_id));
                CREATE INDEX IF NOT EXISTS gf_community_ids_state ON gf_community_ids(repository_id,source_state);
                CREATE TABLE IF NOT EXISTS gf_cross_edges (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
                    source_repository_id TEXT NOT NULL REFERENCES repo_projects(id) ON DELETE CASCADE,
                    source_episode TEXT NOT NULL, source_node_id TEXT NOT NULL, source_stable_id TEXT,
                    target_repository_id TEXT NOT NULL REFERENCES repo_projects(id) ON DELETE CASCADE,
                    target_episode TEXT NOT NULL, target_node_id TEXT NOT NULL, target_stable_id TEXT,
                    relation TEXT NOT NULL, confidence TEXT NOT NULL, derivation TEXT NOT NULL,
                    module_path TEXT, symbol TEXT, line INTEGER,
                    version_constraint TEXT, exporter_version TEXT, version_status TEXT,
                    created_at TEXT NOT NULL,
                    valid_to_episode TEXT, valid_to_repository_id TEXT);
                CREATE INDEX IF NOT EXISTS gf_cross_edges_target ON gf_cross_edges(target_repository_id,valid_to_episode,target_node_id);
                CREATE INDEX IF NOT EXISTS gf_cross_edges_pair ON gf_cross_edges(source_repository_id,target_repository_id,valid_to_episode);
                CREATE TABLE IF NOT EXISTS gf_cross_unresolved (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
                    source_repository_id TEXT NOT NULL REFERENCES repo_projects(id) ON DELETE CASCADE,
                    source_episode TEXT NOT NULL, source_node_id TEXT NOT NULL, source_stable_id TEXT,
                    relpath TEXT, line INTEGER, module_path TEXT NOT NULL, symbol TEXT, package TEXT,
                    reason TEXT NOT NULL, candidates TEXT NOT NULL DEFAULT '[]',
                    exporter_episodes TEXT NOT NULL DEFAULT '{}',
                    version_constraint TEXT, exporter_version TEXT, created_at TEXT NOT NULL,
                    valid_to_episode TEXT, valid_to_repository_id TEXT);
                CREATE INDEX IF NOT EXISTS gf_cross_unresolved_source ON gf_cross_unresolved(source_repository_id,valid_to_episode);
                CREATE TABLE IF NOT EXISTS gf_cross_packages (
                    workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
                    repository_id TEXT NOT NULL REFERENCES repo_projects(id) ON DELETE CASCADE,
                    episode TEXT NOT NULL, ecosystem TEXT NOT NULL, name TEXT NOT NULL, version TEXT,
                    created_at TEXT NOT NULL,
                    PRIMARY KEY(repository_id,episode,ecosystem,name));
                -- Incremental cross-repository linking: per project, the
                -- export index of the episode it last took part with, and
                -- the digest of what its own imports were last linked
                -- against, with the live row counts that link left.
                CREATE TABLE IF NOT EXISTS gf_cross_link_state (
                    repository_id TEXT PRIMARY KEY REFERENCES repo_projects(id) ON DELETE CASCADE,
                    workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
                    episode TEXT NOT NULL, format TEXT NOT NULL, export_index TEXT NOT NULL,
                    inputs TEXT, edges INTEGER, unresolved INTEGER, created_at TEXT NOT NULL);
                CREATE TABLE IF NOT EXISTS gf_repo_access (
                    workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
                    repository_id TEXT NOT NULL REFERENCES repo_projects(id) ON DELETE CASCADE,
                    credential_id TEXT NOT NULL, state TEXT NOT NULL, created_at TEXT NOT NULL,
                    PRIMARY KEY(repository_id,credential_id));
                -- An additive role per project. `pr_scratch`
                -- marks the projects `graphicle pr` registers for a commit
                -- range (graph_projection::PR_SCRATCH); no data moves.
                CREATE TABLE IF NOT EXISTS repo_project_roles (
                    repository_id TEXT PRIMARY KEY REFERENCES repo_projects(id) ON DELETE CASCADE,
                    role TEXT NOT NULL, created_at TEXT NOT NULL);
                -- The folders a local sync read, as aliases of
                -- the project it synced into (folders.rs). `folder_key` has
                -- the form of a local project key, `local:` and the digest of
                -- the canonical path; `path` is that path (local stores
                -- record it). A folder belongs to one
                -- project at a time.
                CREATE TABLE IF NOT EXISTS repo_folders (
                    workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
                    folder_key TEXT NOT NULL,
                    repository_id TEXT NOT NULL REFERENCES repo_projects(id) ON DELETE CASCADE,
                    path TEXT, created_at TEXT NOT NULL, updated_at TEXT NOT NULL,
                    PRIMARY KEY(workspace_id,folder_key));
                CREATE INDEX IF NOT EXISTS repo_folders_repository ON repo_folders(repository_id);
                -- The folder project of a PR scratch project.
                -- `graphicle pr` records commit trees in the scratch project,
                -- projected with the folder project's id as the identity
                -- scope, so a symbol has the same `stable_id` in both.
                CREATE TABLE IF NOT EXISTS repo_pr_scratch (
                    repository_id TEXT PRIMARY KEY REFERENCES repo_projects(id) ON DELETE CASCADE,
                    project_id TEXT NOT NULL REFERENCES repo_projects(id) ON DELETE CASCADE,
                    created_at TEXT NOT NULL);
                -- Reviewed memory anchored to code symbols
                -- (symbols.rs). The live-symbol table: the code symbols of
                -- the projected head set `gf_symbol_state` names, one row per
                -- `stable_id`, with the logical identity, the lines of the
                -- bytes its `revision` hashes and the file's sha256 there.
                -- A projection of the current head set keeps it (by changed
                -- file, or whole); nothing else writes it.
                CREATE TABLE IF NOT EXISTS gf_symbol_heads (
                    repository_id TEXT NOT NULL REFERENCES repo_projects(id) ON DELETE CASCADE,
                    stable_id TEXT NOT NULL, revision TEXT NOT NULL,
                    relpath TEXT NOT NULL, qualified_name TEXT NOT NULL, kind TEXT NOT NULL,
                    ordinal INTEGER NOT NULL, node_id TEXT NOT NULL,
                    line_start INTEGER NOT NULL, line_next INTEGER, sha256 TEXT NOT NULL,
                    PRIMARY KEY(repository_id,stable_id));
                CREATE INDEX IF NOT EXISTS gf_symbol_heads_file ON gf_symbol_heads(repository_id,relpath);
                CREATE TABLE IF NOT EXISTS gf_symbol_state (
                    repository_id TEXT PRIMARY KEY REFERENCES repo_projects(id) ON DELETE CASCADE,
                    episode TEXT NOT NULL, updated_at TEXT NOT NULL);
                -- Exact moves: a symbol that left the
                -- table while one symbol of identical revision, kind and name
                -- tail entered it in the same update. Removed once the
                -- moved-from id is live again.
                CREATE TABLE IF NOT EXISTS gf_symbol_moves (
                    repository_id TEXT NOT NULL REFERENCES repo_projects(id) ON DELETE CASCADE,
                    from_stable_id TEXT NOT NULL, to_stable_id TEXT NOT NULL,
                    revision TEXT NOT NULL, episode TEXT NOT NULL, created_at TEXT NOT NULL,
                    PRIMARY KEY(repository_id,from_stable_id));
                -- A memory's quote occurrence in a code file whose symbols
                -- were not projected when the memory was written: `pending`,
                -- then `pinned` to the symbols that contained it once a
                -- projection of that file content ran. Goes with its memory.
                CREATE TABLE IF NOT EXISTS gf_symbol_pins (
                    repository_id TEXT NOT NULL REFERENCES repo_projects(id) ON DELETE CASCADE,
                    memory_id TEXT NOT NULL REFERENCES repo_memories(id) ON DELETE CASCADE,
                    relpath TEXT NOT NULL, sha256 TEXT NOT NULL,
                    byte_start INTEGER NOT NULL, byte_end INTEGER NOT NULL,
                    state TEXT NOT NULL, covered INTEGER, symbols TEXT, episode TEXT,
                    created_at TEXT NOT NULL, pinned_at TEXT,
                    PRIMARY KEY(repository_id,memory_id,relpath,sha256,byte_start,byte_end));
                CREATE INDEX IF NOT EXISTS gf_symbol_pins_state ON gf_symbol_pins(repository_id,state);

-- pr_report.rs: execution coverage per test at a recorded episode (coverage.py
-- contexts or the simple test → file → line-range map), joined against changed
-- symbols' source ranges at the base episode for the `executed` evidence tier

                CREATE TABLE IF NOT EXISTS gf_test_coverage (
                    repository_id TEXT NOT NULL REFERENCES repo_projects(id) ON DELETE CASCADE,
                    episode TEXT NOT NULL, test_id TEXT NOT NULL, test_node_id TEXT,
                    relpath TEXT NOT NULL, line_start INTEGER NOT NULL, line_end INTEGER NOT NULL,
                    created_at TEXT NOT NULL);
                CREATE INDEX IF NOT EXISTS gf_test_coverage_episode
                    ON gf_test_coverage(repository_id,episode,relpath);
                -- One row per coverage upload: the artefact format that was
                -- detected (`source`) and how fine its attribution is
                -- (`granularity`: `test` = per-test rows, `file` = only the
                -- suite-level rows under the reserved test id).
                CREATE TABLE IF NOT EXISTS gf_coverage_uploads (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    repository_id TEXT NOT NULL REFERENCES repo_projects(id) ON DELETE CASCADE,
                    episode TEXT NOT NULL, source TEXT NOT NULL, granularity TEXT NOT NULL,
                    tests INTEGER NOT NULL, files INTEGER NOT NULL, ranges INTEGER NOT NULL,
                    created_at TEXT NOT NULL);
                CREATE INDEX IF NOT EXISTS gf_coverage_uploads_episode
                    ON gf_coverage_uploads(repository_id,episode);
                CREATE TABLE IF NOT EXISTS gf_coverage_inventory (
                    repository_id TEXT NOT NULL REFERENCES repo_projects(id) ON DELETE CASCADE,
                    episode TEXT NOT NULL, inventory TEXT NOT NULL,
                    PRIMARY KEY(repository_id,episode));

-- graph_projection.rs: receiver-ambiguous calls of an episode, moved out of
-- `gf_episodes.attributes` so retention can drop them per episode. `valid_to_episode`
-- is the episode that superseded this one (NULL for the current episode).

                CREATE TABLE IF NOT EXISTS gf_unresolved_calls (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    repository_id TEXT NOT NULL REFERENCES repo_projects(id) ON DELETE CASCADE,
                    episode TEXT NOT NULL, caller TEXT NOT NULL DEFAULT '', callee TEXT NOT NULL DEFAULT '',
                    call TEXT NOT NULL, valid_to_episode TEXT, created_at TEXT NOT NULL);
                CREATE INDEX IF NOT EXISTS gf_unresolved_calls_episode
                    ON gf_unresolved_calls(repository_id,episode);

-- pr_report.rs: wall-clock of every `impact.report`, for `graph.stats`

                CREATE TABLE IF NOT EXISTS gf_report_timings (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    repository_id TEXT NOT NULL REFERENCES repo_projects(id) ON DELETE CASCADE,
                    base_episode TEXT NOT NULL, head_episode TEXT NOT NULL,
                    elapsed_ms INTEGER NOT NULL, changed_files INTEGER NOT NULL,
                    created_at TEXT NOT NULL);
                CREATE INDEX IF NOT EXISTS gf_report_timings_repo
                    ON gf_report_timings(repository_id,id);
