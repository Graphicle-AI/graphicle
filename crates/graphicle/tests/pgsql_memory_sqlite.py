#!/usr/bin/env python3
"""SQLite schema and SQL isolation smoke tests for credential-private memory.

Run: python3 crates/graphicle/tests/pgsql_memory_sqlite.py
Tests execute the checked-in v0.1.18 schema using stdlib sqlite3. They do NOT
replace Rust integration, real Postgres, API, or worker tests.
"""
import pathlib
import sqlite3
import unittest

SCHEMA = pathlib.Path(__file__).resolve().parents[1] / "src"
TABLE_COLUMNS = {
    "credentials": "workspace_id",
    "repo_memories": "repository_id",
    "repo_events": "repository_id",
    "repo_memory_revisions": "memory_id",
    "am_work_state": "repository_id",
    "am_claims": "repository_id",
    "am_entities": "repository_id",
    "am_entity_links": "repository_id",
    "am_claim_events": "repository_id",
    "am_alias_proposals": "repository_id",
    "gf_alias_proposals": "repository_id",
    "gf_observations": "repository_id",
    "gf_mental_models": "repository_id",
    "gf_episodes": "repository_id",
    "gf_episode_labels": "repo",
    "am_observer_notes": "repository_id",
    "am_observer_reflections": "repository_id",
    "repo_pr_scratch": "repository_id",
    "repo_runs": "repository_id",
    "repo_tool_events": "run_id",
    "am_jobs": "repository_id",
    "am_call_reservations": "job_id",
    "am_model_cache": "repository_id",
    "am_model_calls": "repository_id",
    "am_document_reads": "repository_id",
    "am_query_cache": "repository_id",
    "am_vectors": "repository_id",
}


def populated_db():
    db = sqlite3.connect(":memory:")
    db.execute("PRAGMA foreign_keys=ON")
    for sql in ["schema.sql", "workflow_schema.sql", "expansion_schema.sql",
                "projection_schema.sql", "temporal_schema.sql", "product_schema.sql"]:
        db.executescript((SCHEMA/sql).read_text())
    db.executescript("CREATE TABLE IF NOT EXISTS am_work_state("
                     "id INTEGER PRIMARY KEY,repository_id TEXT NOT NULL REFERENCES repo_projects(id) ON DELETE CASCADE,"
                     "list_name TEXT NOT NULL,fields TEXT NOT NULL,created_at TEXT NOT NULL);")
    for table, parent in TABLE_COLUMNS.items():
        known = {r[1] for r in db.execute(f"PRAGMA table_info({table})")}
        if not known:
            continue
        if "memory_namespace" not in known:
            db.execute(f"ALTER TABLE {table} ADD COLUMN memory_namespace TEXT")
        db.execute(f"CREATE INDEX IF NOT EXISTS {table}_namespace_lookup "
                   f"ON {table}({parent},memory_namespace)")
    db.execute("INSERT INTO workspaces(id,capability_hash,name,created_at) VALUES('ws','h','Shared','2026-11-15')")
    db.execute("INSERT INTO repo_projects(id,workspace_id,project_key,name,created_at,updated_at) "
               "VALUES('repo','ws','gin','gin','now','now')")
    for namespace in ('alice','bob'):
        db.execute("INSERT INTO am_memory_namespaces(workspace_id,name,self_review,created_at) "
                   "VALUES('ws',?,1,'now')", (namespace,))
    for namespace, who in [('alice','alice'), ('bob','bob'), (None,'legacy')]:
        db.execute("INSERT INTO repo_memories(id,repository_id,kind,revision,state,title,body,data,created_at,updated_at,memory_namespace) "
                   "VALUES(?, 'repo','note',1,'reviewed',?,?, '{}','now','now',?)",
                   (who,who,who,namespace))
        db.execute("INSERT INTO repo_memory_revisions(memory_id,revision,data,created_at,memory_namespace) "
                   "VALUES(?,1,'{}','now',?)", (who,namespace))
        db.execute("INSERT INTO am_work_state(repository_id,list_name,fields,created_at,memory_namespace) "
                   "VALUES('repo','todo',?,'now',?)", ('{"task":"'+who+'"}',namespace))
    return db


class ScopedSqlite(unittest.TestCase):
    def setUp(self):
        self.db = populated_db()
        self.addCleanup(self.db.close)

    def test_required_columns_and_indexes_exist(self):
        for table in TABLE_COLUMNS:
            columns = {r[1] for r in self.db.execute(f'PRAGMA table_info({table})')}
            self.assertIn('memory_namespace', columns, table)

    def test_legacy_and_private_memory_are_exclusive(self):
        for namespace, expected in [('alice', 'alice'), ('bob', 'bob'), (None, 'legacy')]:
            selected = self.db.execute("SELECT id FROM repo_memories WHERE repository_id=? "
                "AND memory_namespace IS NOT DISTINCT FROM ?", ('repo',namespace)).fetchall()
            self.assertEqual(selected, [(expected,)])
            self.assertEqual(self.db.execute("SELECT count(*) FROM am_work_state "
                "WHERE repository_id='repo' AND memory_namespace IS NOT DISTINCT FROM ?",
                (namespace,)).fetchone()[0], 1)
            self.assertEqual(self.db.execute("SELECT count(*) FROM repo_memory_revisions "
                "WHERE memory_namespace IS NOT DISTINCT FROM ?",(namespace,)).fetchone()[0],1)

    def test_cross_namespace_id_lookup_is_indistinguishable_from_missing(self):
        for identifier in ('bob', 'nonexistent', 'legacy'):
            self.assertIsNone(self.db.execute("SELECT body FROM repo_memories "
               "WHERE repository_id='repo' AND id=? AND memory_namespace IS NOT DISTINCT FROM 'alice'",
               (identifier,)).fetchone())

    def test_work_state_delete_is_scoped(self):
        self.db.execute("DELETE FROM am_work_state WHERE repository_id='repo' "
            "AND memory_namespace='alice'")
        self.assertEqual(self.db.execute("SELECT count(*) FROM am_work_state WHERE "
            "memory_namespace='bob'").fetchone()[0],1)
        self.assertEqual(self.db.execute("SELECT count(*) FROM am_work_state WHERE "
            "memory_namespace IS NULL").fetchone()[0],1)

    def test_project_cascade_cleans_every_namespace(self):
        self.db.execute("DELETE FROM repo_projects WHERE id='repo'")
        for table in ('repo_memories','repo_memory_revisions','am_work_state'):
            self.assertEqual(self.db.execute(f'SELECT count(*) FROM {table}').fetchone()[0],0)

    def test_model_call_and_reservation_inherit_job_scope(self):
        """Model bookkeeping cannot become a cross-tenant cache side channel."""
        for namespace in ('alice', 'bob'):
            self.db.execute("INSERT INTO am_jobs(id,repository_id,kind,input_hash,payload,principal_hash,"
                "state,not_before,created_at,updated_at,memory_namespace) "
                "VALUES(?,'repo','recall',?,'{}','hash','queued','now','now','now',?)",
                (namespace,namespace,namespace))
            self.db.execute("INSERT INTO am_call_reservations("
                "id,job_id,request_hash,state,input_reserved,output_reserved,created_at,memory_namespace) "
                "VALUES(?,?,?,'started',1,0,'now',(SELECT memory_namespace FROM am_jobs WHERE id=?2))",
                ('res-'+namespace,namespace,'request-'+namespace))
            self.db.execute("INSERT INTO am_model_calls("
                "id,repository_id,job_id,provider,model,request_hash,input_tokens,output_tokens,elapsed_ms,cached,created_at,memory_namespace) "
                "VALUES(?,'repo',?,'local','model',?,1,1,1,0,'now',(SELECT memory_namespace FROM am_jobs WHERE id=?2))",
                ('call-'+namespace,namespace,'request-'+namespace))
        for table in ('am_model_calls', 'am_call_reservations'):
            names = [r[0] for r in self.db.execute("SELECT id FROM " + table + " WHERE memory_namespace=?", ('alice',))]
            self.assertEqual(names, [('call-alice' if table == 'am_model_calls' else 'res-alice')])
        self.db.execute("DELETE FROM am_jobs WHERE id='alice'")
        self.assertEqual(self.db.execute("SELECT count(*) FROM am_call_reservations "
           "WHERE memory_namespace='alice'").fetchone()[0],0)
        self.assertEqual(self.db.execute("SELECT count(*) FROM am_call_reservations "
           "WHERE memory_namespace='bob'").fetchone()[0],1)

    def test_owner_export_predicates_are_sqlite_valid(self):
        """Every tenant export table is queryable after migrations."""
        owner = (SCHEMA/'namespace_owner.rs').read_text()
        import re
        entries = re.findall(r'^    \("(\w+)", "(\w+)"\),',owner,re.M)
        self.assertGreater(len(entries),20)
        for table,column in entries:
            self.assertIn(table,TABLE_COLUMNS)
            subquery={
                'memory_id': "SELECT m.id FROM repo_memories m JOIN repo_projects p ON p.id=m.repository_id WHERE p.workspace_id=?",
                'run_id': "SELECT r.id FROM repo_runs r JOIN repo_projects p ON p.id=r.repository_id WHERE p.workspace_id=?",
                'job_id': "SELECT j.id FROM am_jobs j JOIN repo_projects p ON p.id=j.repository_id WHERE p.workspace_id=?",
            }.get(column,"SELECT id FROM repo_projects WHERE workspace_id=?")
            self.db.execute(f"SELECT {table}.* FROM {table} WHERE "
                f"{table}.memory_namespace=? AND {table}.{column} IN ({subquery}) ORDER BY 1",
                ('alice','ws')).fetchall()

    def test_namespace_policies_are_workspace_keyed(self):
        self.assertEqual(self.db.execute("SELECT self_review FROM am_memory_namespaces "
            "WHERE workspace_id='ws' AND name='alice'").fetchone(),(1,))
        with self.assertRaises(sqlite3.IntegrityError):
            self.db.execute("INSERT INTO am_memory_namespaces(workspace_id,name,self_review,created_at) "
                            "VALUES('ws','alice',0,'now')")


if __name__ == '__main__':
    unittest.main(verbosity=2)
