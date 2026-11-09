-- SQL functions the store's queries use, in each store's schema. They keep
-- the results the store's SQLite queries had: a JSON value is read as SQL
-- text (a string unquoted, true and false as 1 and 0, null as NULL,
-- numbers, arrays and objects as their JSON text), and object keys keep
-- their written order (these read `json`, not `jsonb`).

-- '$.a.b[2]' as the path array '{a,b,2}'.
CREATE OR REPLACE FUNCTION graphicle_json_path(path text) RETURNS text[]
LANGUAGE sql IMMUTABLE STRICT PARALLEL SAFE AS $$
  SELECT CASE WHEN path = '$' THEN ARRAY[]::text[]
    ELSE string_to_array(
      regexp_replace(regexp_replace(substr(path, 2), '\[(\d+)\]', '.\1', 'g'), '^\.', ''), '.')
  END
$$;

-- `doc` as json, with its \u0000 escapes made literal text (six
-- characters): PostgreSQL's json functions refuse a NUL character, which
-- SQLite's returned.
CREATE OR REPLACE FUNCTION graphicle_json(doc text) RETURNS json
LANGUAGE sql IMMUTABLE STRICT PARALLEL SAFE AS $$
  -- Only an escape (a \u0000 after an even run of backslashes), so that
  -- text already made literal is left as it is.
  SELECT CASE WHEN strpos(doc, '\u0000') > 0
              THEN regexp_replace(doc, '(?<![\\])((\\\\)*)\\u0000', '\1\\\\u0000', 'g')::json
              ELSE doc::json END
$$;

-- A JSON value as SQL text, as SQLite's json functions return it.
CREATE OR REPLACE FUNCTION graphicle_json_sql(v json) RETURNS text
LANGUAGE sql IMMUTABLE PARALLEL SAFE AS $$
  SELECT CASE json_typeof(v)
    WHEN 'string' THEN v #>> '{}'
    WHEN 'boolean' THEN CASE WHEN v::text = 'true' THEN '1' ELSE '0' END
    WHEN 'null' THEN NULL
    ELSE v::text
  END
$$;

CREATE OR REPLACE FUNCTION json_extract(doc text, path text) RETURNS text
LANGUAGE sql IMMUTABLE PARALLEL SAFE AS $$
  SELECT graphicle_json_sql(CASE WHEN path = '$' THEN graphicle_json(doc) ELSE graphicle_json(doc) #> graphicle_json_path(path) END)
$$;

-- SQLite's json_type: null, true, false, integer, real, text, array,
-- object; NULL when the path is absent.
-- The SQLite json_type of a JSON value.
CREATE OR REPLACE FUNCTION graphicle_json_type(v json) RETURNS text
LANGUAGE sql IMMUTABLE PARALLEL SAFE AS $$
  SELECT CASE json_typeof(v)
    WHEN 'object' THEN 'object'
    WHEN 'array' THEN 'array'
    WHEN 'string' THEN 'text'
    WHEN 'boolean' THEN CASE WHEN v::text = 'true' THEN 'true' ELSE 'false' END
    WHEN 'null' THEN 'null'
    WHEN 'number' THEN CASE WHEN v::text ~ '^-?[0-9]+$' THEN 'integer' ELSE 'real' END
  END
$$;

CREATE OR REPLACE FUNCTION json_type(doc text, path text) RETURNS text
LANGUAGE sql IMMUTABLE PARALLEL SAFE AS $$
  SELECT graphicle_json_type(CASE WHEN path = '$' THEN graphicle_json(doc)
                                  ELSE graphicle_json(doc) #> graphicle_json_path(path) END)
$$;

CREATE OR REPLACE FUNCTION json_type(doc text) RETURNS text
LANGUAGE sql IMMUTABLE PARALLEL SAFE AS $$
  SELECT graphicle_json_type(graphicle_json(doc))
$$;

-- SQLite's json_each over an array or object: one row per element, in
-- order, with its key (the index for an array), its value as SQL text and
-- its json_type. The column "json" is quoted: since PostgreSQL 17, JSON is a
-- keyword, and a keyword of its kind can't name a parameter or a returned
-- column unquoted. Queries read it as before (j.json, or json).
DROP FUNCTION IF EXISTS json_each(text, text);
DROP FUNCTION IF EXISTS json_each(text);
CREATE OR REPLACE FUNCTION json_each(doc text)
RETURNS TABLE(key text, value text, type text, "json" text)
LANGUAGE sql IMMUTABLE PARALLEL SAFE AS $$
  WITH d AS (SELECT graphicle_json(doc) AS j)
  SELECT (a.n - 1)::text, graphicle_json_sql(a.v), graphicle_json_type(a.v), a.v::text
    FROM d, json_array_elements(CASE WHEN json_typeof(d.j) = 'array' THEN d.j ELSE '[]'::json END)
         WITH ORDINALITY AS a(v, n)
  UNION ALL
  SELECT o.key, graphicle_json_sql(o.value), graphicle_json_type(o.value), o.value::text
    FROM d, json_each(CASE WHEN json_typeof(d.j) = 'object' THEN d.j ELSE '{}'::json END) AS o
$$;

CREATE OR REPLACE FUNCTION json_each(doc text, path text)
RETURNS TABLE(key text, value text, type text, "json" text)
LANGUAGE sql IMMUTABLE PARALLEL SAFE AS $$
  SELECT * FROM json_each(CASE WHEN path = '$' THEN doc ELSE (graphicle_json(doc) #> graphicle_json_path(path))::text END)
$$;

-- SQLite's instr: the 1-based position of `needle` in `haystack`, 0 when
-- absent.
CREATE OR REPLACE FUNCTION instr(haystack text, needle text) RETURNS bigint
LANGUAGE sql IMMUTABLE STRICT PARALLEL SAFE AS $$
  SELECT strpos(haystack, needle)::bigint
$$;

-- Every table of the store has a `rowid`, as every SQLite table did: an
-- identity of its own, filled on insert and accepting a copied value (so
-- `INSERT INTO t SELECT * FROM copy_of_t` works). Run after the tables are
-- created; leaves tables that have one.
CREATE OR REPLACE FUNCTION graphicle_add_rowids() RETURNS void
LANGUAGE plpgsql AS $$
DECLARE
  t record;
BEGIN
  FOR t IN
    SELECT c.table_name
      FROM information_schema.tables c
     WHERE c.table_schema = current_schema() AND c.table_type = 'BASE TABLE'
       AND NOT EXISTS (SELECT 1 FROM information_schema.columns r
                       WHERE r.table_schema = c.table_schema AND r.table_name = c.table_name
                         AND r.column_name = 'rowid')
  LOOP
    EXECUTE format('ALTER TABLE %I ADD COLUMN rowid BIGINT GENERATED BY DEFAULT AS IDENTITY', t.table_name);
  END LOOP;
END
$$;

-- SQLite's json_array_length: the length of the array at `path`, 0 for
-- another JSON value, NULL when the path is absent.
CREATE OR REPLACE FUNCTION json_array_length(doc text, path text) RETURNS bigint
LANGUAGE sql IMMUTABLE PARALLEL SAFE AS $$
  SELECT CASE WHEN v IS NULL THEN NULL
              WHEN json_typeof(v) = 'array' THEN json_array_length(v)::bigint
              ELSE 0 END
  FROM (SELECT graphicle_json(doc) #> graphicle_json_path(path) AS v) found
$$;

CREATE OR REPLACE FUNCTION json_array_length(doc text) RETURNS bigint
LANGUAGE sql IMMUTABLE PARALLEL SAFE AS $$
  SELECT json_array_length(doc, '$')
$$;

-- SQLite's json(), json_set() and json_remove(), for statements that edit a
-- JSON text column in place. A text value is set as a JSON string, a
-- number as a JSON number, and json(text) as the JSON it holds.
CREATE OR REPLACE FUNCTION "json"(doc text) RETURNS jsonb
LANGUAGE sql IMMUTABLE PARALLEL SAFE AS $$ SELECT (doc::jsonb) $$;

CREATE OR REPLACE FUNCTION json_set(doc text, path text, value jsonb) RETURNS text
LANGUAGE sql IMMUTABLE PARALLEL SAFE AS $$
  SELECT jsonb_set((doc::jsonb), graphicle_json_path(path), coalesce(value, 'null'::jsonb), true)::text
$$;

CREATE OR REPLACE FUNCTION json_set(doc text, path text, value text) RETURNS text
LANGUAGE sql IMMUTABLE PARALLEL SAFE AS $$ SELECT json_set(doc, path, to_jsonb(value)) $$;

CREATE OR REPLACE FUNCTION json_set(doc text, path text, value bigint) RETURNS text
LANGUAGE sql IMMUTABLE PARALLEL SAFE AS $$ SELECT json_set(doc, path, to_jsonb(value)) $$;

CREATE OR REPLACE FUNCTION json_set(doc text, path text, value double precision) RETURNS text
LANGUAGE sql IMMUTABLE PARALLEL SAFE AS $$ SELECT json_set(doc, path, to_jsonb(value)) $$;

CREATE OR REPLACE FUNCTION json_remove(doc text, path text) RETURNS text
LANGUAGE sql IMMUTABLE PARALLEL SAFE AS $$ SELECT ((doc::jsonb) #- graphicle_json_path(path))::text $$;

-- `json(?)` can resolve as a cast to json rather than the function above.
CREATE OR REPLACE FUNCTION json_set(doc text, path text, value json) RETURNS text
LANGUAGE sql IMMUTABLE PARALLEL SAFE AS $$ SELECT json_set(doc, path, value::jsonb) $$;

-- Delete the dependency rows of each of `ids` (`dependent_kind` `kind`), one
-- primary-key lookup per id. A set-based delete (`= ANY(ids)`, a join) can be
-- planned as a scan of every dependent of the kind when the table has no
-- statistics yet, as during a store's first projection.
CREATE OR REPLACE FUNCTION graphicle_forget_dependents(ws text, kind text, ids text[]) RETURNS bigint
LANGUAGE plpgsql AS $$
DECLARE
  one text;
  gone bigint := 0;
  n bigint;
BEGIN
  FOREACH one IN ARRAY ids LOOP
    DELETE FROM gf_dependencies WHERE workspace = ws AND dependent_kind = kind AND dependent_id = one;
    GET DIAGNOSTICS n = ROW_COUNT;
    gone := gone + n;
  END LOOP;
  RETURN gone;
END
$$;
