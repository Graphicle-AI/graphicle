#!/usr/bin/env python3
"""Qualify the native SQL extractor and offset-preserving recovery scanner."""
import ast
import functools
import os
import random
import tempfile
import types
from pathlib import Path
from unittest.mock import patch
from parity import Native, SOURCE, upstream_tests
from upstream_fixes import oracle_source
import graphify.extract as facade
import graphify.extractors.sql as reference

# Expected values come from the pinned module with reviewed fixes applied in memory;
# the imported module keeps its identity for patching the extraction facade.
oracle = types.ModuleType('reference_sql'); oracle.__package__ = 'graphify.extractors'
exec(compile(oracle_source('graphify/extractors/sql.py'), '<pinned sql.py with reviewed fixes>', 'exec'), oracle.__dict__)
native = Native('sql')
original = reference.extract_sql
original_scan = oracle._scan_sql
original_norm = oracle._norm_ident
cases = helpers = 0

@functools.wraps(original)
def extract(path, content=None):
    global cases
    native.use_working_directory(os.getcwd())
    expected = oracle.extract_sql(path, content=content)
    actual = native.result('extract.sql', path=str(path), content=list(content) if isinstance(content, bytes) else content)
    cases += 1
    if actual != expected:
        native.fail(path=str(path), content=repr(content), source=path.read_bytes().hex() if path.is_file() else None, expected=expected, actual=actual)
    return actual

def scan(text):
    global helpers
    expected = original_scan(text)
    actual = native.result('sql.scan', text=text)
    actual = (actual[0], [tuple(span) for span in actual[1]])
    helpers += 1
    if actual != expected: native.fail(operation='scan', text=text, expected=expected, actual=actual)
    return actual

def mask(text):
    global helpers
    expected = original_scan(text)[0]
    actual = native.result('sql.mask', text=text)
    helpers += 1
    if actual != expected: native.fail(operation='mask', text=text, expected=expected, actual=actual)
    return actual

try:
    names = [n.name for n in ast.parse((SOURCE/'tests/test_multilang.py').read_text()).body
             if isinstance(n, ast.FunctionDef) and n.name.startswith(('test_sql_', 'test_mask_sql_'))]
    paths = ['tests/test_multilang.py::'+n for n in names]
    paths += ['tests/test_extract.py::test_extract_failed_sources_empty_when_sql_installed',
              'tests/test_extract.py::test_extract_no_missing_dep_warning_when_sql_installed']
    with patch.object(reference, 'extract_sql', extract), patch.object(facade, 'extract_sql', extract), \
         patch.dict(facade._DISPATCH, {k:extract for k,v in facade._DISPATCH.items() if v is original}), \
         patch.object(reference, '_scan_sql', scan), patch.object(reference, '_mask_sql_comments', mask):
        tests = upstream_tests(*paths)
    forms = ['', 'CREATE TABLE a(id INT); CREATE TABLE b(id INT REFERENCES a(id));',
        'CREATE TABLE public.a(id INT); CREATE TABLE private.a(id INT); CREATE TABLE b(id INT REFERENCES a(id));',
        'CREATE TABLE a(id INT REFERENCES b(id)); CREATE TABLE b(id INT);',
        'CREATE TABLE "public"."People"(id int); CREATE VIEW v AS SELECT * FROM public.People;',
        'CREATE VIEW v AS WITH RECURSIVE a(id) AS (SELECT id FROM t) SELECT * FROM a JOIN t USING(id);',
        'CREATE VIEW v AS SELECT * FROM a JOIN (WITH a AS (SELECT * FROM b) SELECT * FROM a) other ON true;',
        'ALTER TABLE missing ADD CONSTRAINT f FOREIGN KEY(id) REFERENCES other(id);',
        'BEGIN; CREATE TABLE x(id INT); CREATE INDEX "idx_name" ON x(id); COMMIT;',
        'CREATE OR ALTER PROC [dbo].[a]]b] AS BEGIN SELECT * FROM t; END;',
        'CREATE OR REPLACE FUNCTION "public"."a""b"() RETURNS void AS $$ BEGIN PERFORM 1; END $$ LANGUAGE plpgsql;',
        'CREATE FUNCTION IF NOT EXISTS func() RETURNS INT RETURN 1;',
        'CREATE PROCEDURE p AS BEGIN UPDATE a SET x=1; SELECT * FROM b JOIN c; END;',
        'SET TERM ^ ; CREATE TRIGGER tg FOR t AS BEGIN INSERT INTO log VALUES(1); END^ SET TERM ; ^',
        "SELECT 'CREATE PROCEDURE hidden';\nCREATE PROCEDURE visible AS BEGIN SELECT 1; END;",
        'SELECT [CREATE PROC hidden] FROM t;\nCREATE PROC visible AS BEGIN SELECT 1; END;',
        'SELECT "bad -- comment\n/* inner /* nested */ CREATE PROC hidden */\nCREATE PROC visible AS BEGIN SELECT 1; END;',
        'SELECT "é🙂";\n-- 日本語\nCREATE PROCEDURE [dbo].[café] AS BEGIN SELECT 1; END;',
        'AUTOCREATE PROCEDURE fake AS BEGIN SELECT 1; END;\nCREATE PROC real AS BEGIN SELECT 1; END;',
        'CREATE TABLE weird (a INT COMPUTED BY(1), b INT REFERENCES other);',
        'create\x1fprocedure p as begin select 1; end;',
        'CREATE FUNCTıON a() RETURNS void AS $$ BEGIN PERFORM 1; END $$ LANGUAGE plpgsql;',
        'CREATE PROCEDURE "a--b" AS BEGIN SELECT 1; END;',
        # PR #3020: a default-schema-qualified reference binds to an unqualified definition; other schemas still stub.
        'CREATE TABLE "companies"(id int primary key); CREATE TABLE "users"(id int, company_id int, FOREIGN KEY (company_id) REFERENCES "public"."companies"("id"));',
        'CREATE TABLE "companies"(id int primary key); CREATE TABLE "users"(id int, company_id int, FOREIGN KEY (company_id) REFERENCES "other"."companies"("id"));',
        'CREATE TABLE public.companies(id int); CREATE TABLE companies(id int); CREATE TABLE users(c int REFERENCES public.companies(id));',
        # Adapted PR #3020: a bare alias of another schema's table is not an unqualified definition.
        'CREATE TABLE "space name"."order" (id INT); CREATE VIEW "space name"."View" AS SELECT * FROM "public"."order";',
        'CREATE TABLE other.t(id int); CREATE TABLE t(id int); CREATE TABLE u(c int REFERENCES public.t(id)); CREATE TABLE w(c int REFERENCES t(id));',
        # Issue #2712 / PR #2716: bracket-delimited T-SQL names are labelled verbatim (`[dbo].[Alpha]`, not `dbo].[Alpha`).
        'CREATE TABLE [dbo].[Alpha](id INT); CREATE TABLE [dbo].[Beta](a_id INT REFERENCES [dbo].[Alpha](id), g INT REFERENCES [Gamma](id), d INT REFERENCES dbo.[Delta](id), e INT REFERENCES [dbo].Eps(id));',
        'CREATE TABLE [dbo].[Alpha](id INT); CREATE VIEW [dbo].[V] AS SELECT * FROM [dbo].[Alpha] JOIN [dbo].[Other] ON true; ALTER TABLE [dbo].[Alpha] ADD CONSTRAINT fk FOREIGN KEY(id) REFERENCES [dbo].[Zed](id);',
        'CREATE TABLE [dbo].[A]]B](id INT); CREATE TRIGGER [dbo].[trg] FOR [dbo].[A]]B] AS BEGIN SELECT 1; END; CREATE INDEX ix ON [dbo].[A]]B](id);',
        'CREATE TABLE [dbo].[Multi\nLine](id INT); CREATE VIEW v AS WITH [cte] AS (SELECT 1) SELECT * FROM [cte] JOIN [dbo].[Multi\nLine] ON true;']
    rng = random.Random(2026092331)
    for _ in range(380):
        table = rng.choice(['a','b','public.a','"a"','[A]','`a`','café','t$one','[dbo].[A]','dbo.[A]','[dbo].A'])
        target = rng.choice(['a','b','public.a','private.a','unknown','"a"','[dbo].[A]','[A]','[dbo] . [a]'])
        stmt = rng.choice([
            f'CREATE TABLE {table}(id INT, parent INT REFERENCES {target}(id));',
            f'CREATE TABLE {table}(id INT, CONSTRAINT fk FOREIGN KEY(id) REFERENCES {target}(id));',
            f'CREATE VIEW {table} AS SELECT * FROM {target} JOIN other ON true;',
            f'ALTER TABLE {table} ADD CONSTRAINT fk FOREIGN KEY(id) REFERENCES {target}(id);',
            f'CREATE UNIQUE INDEX idx ON {table}(id);',
            f'CREATE PROC {table} AS BEGIN SELECT * FROM {target}; END;',
            f'CREATE OR REPLACE FUNCTION {table}() RETURNS void AS $$ BEGIN PERFORM 1; END $$ LANGUAGE plpgsql;'])
        stmt = rng.choice(['','BEGIN;\n','-- comment\n','/* one\n two */\n']) + stmt
        if rng.random()<.3:stmt=stmt[:rng.randrange(len(stmt))]
        forms.append(stmt)
    with tempfile.TemporaryDirectory(prefix='graphify-sql-') as tmp:
        root = Path(tmp); path=root/'sample.sql'
        for text in forms:
            path.write_text(text); extract(path); scan(text)
        for source in (SOURCE/'tests/fixtures').rglob('*.sql'): extract(source)
        for raw in [b'\xffCREATE TABLE a(id INT);',b'\xef\xbb\xbfCREATE TABLE a(id INT);',b'CREATE TABLE a(id INT);\x00\r\n']:
            path.write_bytes(raw);extract(path);extract(path,raw)
        extract(root/'missing.sql');extract(root)
        for text in forms[:25]:extract(root/'virtual.sql',text)
        saved=os.getcwd()
        try:os.chdir(root);extract(Path('sample.sql'))
        finally:os.chdir(saved)
    for _ in range(1500):
        text=''.join(rng.choices('AazZ.$ _\t\n\x1f"`[]éİıſK🙂日本語',k=rng.randrange(60)))
        expected=original_norm(text); actual=native.result('sql.norm',text=text);helpers+=1
        if actual!=expected:native.fail(operation='norm',text=text,expected=expected,actual=actual)
    for _ in range(1200):
        scan(''.join(rng.choices("Aé🙂日本語[]'\"/*-\\\n\r\t",k=rng.randrange(150))))
finally:
    native.close()
native.receipt('graphify/extractors/sql.py', ['src/extractors/sql.rs',
    'vendor/tree-sitter-sql/src/lib.rs','vendor/tree-sitter-sql/build.rs',
    'vendor/tree-sitter-sql/grammar/parser.c','vendor/tree-sitter-sql/grammar/scanner.c',
    'vendor/tree-sitter-sql/grammar/tree_sitter/parser.h','vendor/tree-sitter-sql/SOURCE.json'],
    extraction_cases=cases, helper_cases=helpers, upstream_tests=tests,
    comparisons='Exact tables/views/functions/procedures/triggers/indexes, schema aliases and foreign keys, scoped CTE reads, recovery scanner offsets and masking, quoted routine recovery and UTF-8 input.',
    adaptations=['The exact pinned native SQL grammar is linked; Python missing-module/ABI-load mocks do not apply.',
        'Original corpus/CLI orchestration stays in Python for upstream integration checks; only the SQL API is substituted.',
        'Preserves upstream recovery limits for multiline dynamic SQL and unmodeled quoting dialects. Native regex failures return errors instead of partial graphs.',
        'The oracle is the pinned graphify/extractors/sql.py with PR 3020 and the issue #2712 / PR 2716 bracket-identifier fix applied in memory (object_reference names touched by T-SQL brackets are re-read verbatim from the source).'])
