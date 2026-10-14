
                SELECT
                    con.conname AS constraint_name,
                    ns.nspname AS table_schema,
                    rel.relname AS table_name,
                    (SELECT ARRAY_AGG(att.attname ORDER BY k.ord)
                       FROM UNNEST(con.conkey) WITH ORDINALITY AS k(attnum, ord)
                       JOIN pg_catalog.pg_attribute att
                         ON att.attrelid = con.conrelid AND att.attnum = k.attnum
                    ) AS columns,
                    fns.nspname AS foreign_table_schema,
                    frel.relname AS foreign_table_name,
                    (SELECT ARRAY_AGG(att.attname ORDER BY k.ord)
                       FROM UNNEST(con.confkey) WITH ORDINALITY AS k(attnum, ord)
                       JOIN pg_catalog.pg_attribute att
                         ON att.attrelid = con.confrelid AND att.attnum = k.attnum
                    ) AS foreign_columns
                FROM pg_catalog.pg_constraint con
                JOIN pg_catalog.pg_class rel ON rel.oid = con.conrelid
                JOIN pg_catalog.pg_namespace ns ON ns.oid = rel.relnamespace
                JOIN pg_catalog.pg_class frel ON frel.oid = con.confrelid
                JOIN pg_catalog.pg_namespace fns ON fns.oid = frel.relnamespace
                WHERE con.contype = 'f'
                  AND ns.nspname NOT IN ('pg_catalog', 'information_schema')
                ORDER BY ns.nspname, rel.relname, con.conname;
            