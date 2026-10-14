
                SELECT routine_schema, routine_name, routine_type,
                       routine_definition, external_language
                FROM information_schema.routines
                WHERE routine_schema NOT IN ('pg_catalog', 'information_schema')
                ORDER BY routine_schema, routine_name;
            