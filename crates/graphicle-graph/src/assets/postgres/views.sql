
                SELECT table_schema, table_name, view_definition
                FROM information_schema.views
                WHERE table_schema NOT IN ('pg_catalog', 'information_schema')
                ORDER BY table_schema, table_name;
            