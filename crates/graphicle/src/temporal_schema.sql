-- Append-only assertions separate world-valid time from server recording time.
CREATE TABLE IF NOT EXISTS gf_timeline (
 id INTEGER PRIMARY KEY AUTOINCREMENT,
 repository_id TEXT NOT NULL, branch TEXT NOT NULL, episode TEXT NOT NULL,
 valid_us INTEGER NOT NULL, recorded_us INTEGER NOT NULL, origin TEXT NOT NULL,
 FOREIGN KEY(repository_id,episode) REFERENCES gf_episodes(repository_id,episode) ON DELETE CASCADE,
 UNIQUE(repository_id,branch,episode,valid_us,recorded_us));
CREATE INDEX IF NOT EXISTS gf_timeline_at ON gf_timeline(repository_id,branch,valid_us,recorded_us);
CREATE TABLE IF NOT EXISTS gf_aliases (
 id INTEGER PRIMARY KEY AUTOINCREMENT,
 repository_id TEXT NOT NULL REFERENCES repo_projects(id) ON DELETE CASCADE,
 proposal_id TEXT NOT NULL, from_node TEXT NOT NULL, to_node TEXT NOT NULL,
 active INTEGER NOT NULL CHECK(active IN (0,1)), valid_us INTEGER NOT NULL, recorded_us INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS gf_aliases_at ON gf_aliases(repository_id,proposal_id,valid_us,recorded_us);
CREATE TABLE IF NOT EXISTS gf_hyperedges (
 id INTEGER PRIMARY KEY AUTOINCREMENT,
 repository_id TEXT NOT NULL REFERENCES repo_projects(id) ON DELETE CASCADE,
 hyperedge_id TEXT NOT NULL, episode TEXT NOT NULL, relation TEXT NOT NULL,
 members TEXT NOT NULL, attributes TEXT NOT NULL,
 active INTEGER NOT NULL CHECK(active IN (0,1)), valid_us INTEGER NOT NULL, recorded_us INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS gf_hyperedges_at ON gf_hyperedges(repository_id,hyperedge_id,valid_us,recorded_us);
CREATE TABLE IF NOT EXISTS gf_commits (
 repository_id TEXT NOT NULL REFERENCES repo_projects(id) ON DELETE CASCADE,
 oid TEXT NOT NULL, tree_oid TEXT NOT NULL, episode TEXT NOT NULL,
 committed_us INTEGER NOT NULL, recorded_us INTEGER NOT NULL,
 object BLOB NOT NULL, PRIMARY KEY(repository_id,oid));
CREATE TABLE IF NOT EXISTS gf_commit_parents (
 repository_id TEXT NOT NULL, commit_oid TEXT NOT NULL, parent_oid TEXT NOT NULL,
 position INTEGER NOT NULL, PRIMARY KEY(repository_id,commit_oid,position),
 FOREIGN KEY(repository_id,commit_oid) REFERENCES gf_commits(repository_id,oid) ON DELETE CASCADE);
