//! A data folder's PostgreSQL store as `import-sqlite` and `restore` make
//! it. Run with `GRAPHICLE_DATABASE_URL` set; without it every test here is
//! skipped. A copy that is stopped (a kill, a reboot) never leaves a store
//! that opens empty, and only the process that creates a `postgres.json`
//! ever writes it.

use graphicle_runtime::{db::Database, import_sqlite, store::Store};
use std::path::{Path, PathBuf};
use tempfile::TempDir;

fn postgres_store() -> bool {
    if !graphicle_runtime::db::postgres_configured() {
        eprintln!("skipped: a PostgreSQL store; GRAPHICLE_DATABASE_URL is not set");
        return false;
    }
    true
}

/// A SQLite store at this program's tables, as `reviews.sqlite3` of
/// `folder` (made directly: with the URL set, `Store::open` makes a
/// PostgreSQL store).
fn sqlite_source(folder: &Path) -> PathBuf {
    std::fs::create_dir_all(folder).unwrap();
    let path = folder.join("reviews.sqlite3");
    let database = Database::sqlite(&path, true);
    graphicle_runtime::store::migrate_pool(&database).unwrap();
    path
}

fn schema_exists(store: &Store, schema: &str) -> bool {
    let count: i64 = store
        .connect()
        .unwrap()
        .query_row(
            "SELECT count(*) FROM pg_namespace WHERE nspname=?",
            [schema],
            |r| r.get(0),
        )
        .unwrap();
    count > 0
}

/// An empty `postgres.json` (a process stopped between creating it and
/// writing it) is refused by every opener and never filled by one: before,
/// two openers could each fill it and use a schema of their own.
/// `import-sqlite` replaces it.
#[test]
fn an_empty_store_file_is_refused_never_filled_and_replaced_by_an_import() {
    if !postgres_store() {
        return;
    }
    let dir = TempDir::new().unwrap();
    let folder = dir.path().join("data");
    let source = sqlite_source(&folder);
    let file = folder.join(graphicle_runtime::db::SCHEMA_FILE);
    std::fs::write(&file, b"").unwrap();
    let Err(refused) = Store::open(&folder) else {
        panic!("a folder with an empty postgres.json opened");
    };
    assert!(refused.to_string().contains("not finished"), "{refused}");
    assert!(Database::for_file(&file, true).is_err());
    assert!(Store::existing(&folder).is_err());
    assert_eq!(std::fs::metadata(&file).unwrap().len(), 0);
    let imported = import_sqlite::import(&source, &folder).unwrap();
    assert_eq!(imported["imported"], true, "{imported}");
    let store = Store::open(&folder).unwrap();
    assert_eq!(imported["schema"], store.schema());
}

/// What a kill during `import-sqlite`'s copy leaves: the store created and
/// migrated, nothing copied. No command opens it (`serve` would have served
/// an empty store beside the intact SQLite file), and `import-sqlite` again
/// replaces it, its schema dropped. A finished store is never replaced.
#[test]
fn an_import_stopped_before_its_copy_committed_is_refused_and_replaced() {
    if !postgres_store() {
        return;
    }
    let dir = TempDir::new().unwrap();
    let folder = dir.path().join("data");
    let source = sqlite_source(&folder);
    let stopped = Store::create_postgres(&folder).unwrap();
    let left = stopped.schema().to_string();
    drop(stopped);
    let Err(refused) = Store::open(&folder) else {
        panic!("an unfinished import opened");
    };
    assert!(refused.to_string().contains("not finished"), "{refused}");
    assert!(Store::existing(&folder).is_err());
    let imported = import_sqlite::import(&source, &folder).unwrap();
    assert_eq!(imported["imported"], true, "{imported}");
    assert_ne!(imported["schema"], left.as_str());
    let store = Store::open(&folder).unwrap();
    assert!(!schema_exists(&store, &left));
    let again = import_sqlite::import(&source, &folder);
    assert!(again.is_err());
    assert_eq!(Store::open(&folder).unwrap().schema(), store.schema());
}

/// An import that is still copying holds its store's copy lock, from
/// before the schema is made until it is done: a second `import-sqlite`
/// on the folder is refused and leaves the store, its file and the SQLite
/// file as they were, instead of dropping the store under the first. The
/// first still finishes, and its store opens.
#[test]
fn an_import_refuses_to_replace_a_store_whose_copy_is_still_running() {
    if !postgres_store() {
        return;
    }
    let dir = TempDir::new().unwrap();
    let folder = dir.path().join("data");
    let source = sqlite_source(&folder);
    let file = folder.join(graphicle_runtime::db::SCHEMA_FILE);
    let running = Store::create_postgres(&folder).unwrap();
    let schema = running.schema().to_string();
    let named = std::fs::read(&file).unwrap();
    let copied = std::fs::read(&source).unwrap();
    let refused = import_sqlite::import(&source, &folder).expect_err("replaced a running import");
    assert!(
        refused.to_string().contains("an import is still running"),
        "{refused}"
    );
    assert_eq!(std::fs::read(&file).unwrap(), named);
    assert_eq!(std::fs::read(&source).unwrap(), copied);
    assert!(schema_exists(&running, &schema));
    running.finish().unwrap();
    drop(running);
    assert_eq!(Store::open(&folder).unwrap().schema(), schema);
}

/// The same for `restore`, which loads into a store made the same way: one
/// it had not finished loading is refused; a finished one opens.
#[test]
fn a_restore_stopped_before_its_load_committed_is_refused() {
    if !postgres_store() {
        return;
    }
    let dir = TempDir::new().unwrap();
    let folder = dir.path().join("restored");
    let stopped = Store::create_postgres(&folder).unwrap();
    drop(stopped);
    let Err(refused) = Store::open(&folder) else {
        panic!("an unfinished restore opened");
    };
    assert!(refused.to_string().contains("not finished"), "{refused}");
    // A finished store opens.
    let finished = dir.path().join("finished");
    let store = Store::create_postgres(&finished).unwrap();
    store.finish().unwrap();
    assert_eq!(Store::open(&finished).unwrap().schema(), store.schema());
}

/// A column the SQLite store has and this version does not create (one a
/// newer Graphicle added) is refused, as an unknown table is, instead of
/// being left behind; nothing is created.
#[test]
fn an_import_refuses_a_column_this_version_does_not_know() {
    if !postgres_store() {
        return;
    }
    let dir = TempDir::new().unwrap();
    let folder = dir.path().join("data");
    let source = sqlite_source(&folder);
    rusqlite::Connection::open(&source)
        .unwrap()
        .execute_batch("ALTER TABLE repo_projects ADD COLUMN later_notes TEXT")
        .unwrap();
    let refused = import_sqlite::import(&source, &folder).expect_err("imported");
    assert!(refused.to_string().contains("later_notes"), "{refused}");
    assert!(!folder.join(graphicle_runtime::db::SCHEMA_FILE).exists());
}

/// A SQLite store that a newer Graphicle needs (its `store_readers`) is
/// refused before anything is created, as opening it is.
#[test]
fn an_import_refuses_a_store_that_needs_a_newer_reader() {
    if !postgres_store() {
        return;
    }
    let dir = TempDir::new().unwrap();
    let folder = dir.path().join("data");
    let source = sqlite_source(&folder);
    rusqlite::Connection::open(&source)
        .unwrap()
        .execute_batch(
            "INSERT INTO store_readers(reason,format,version,updated_at) VALUES('later',999999,'99.0.0','2026-11-08T00:00:00.000000Z')",
        )
        .unwrap();
    let refused = import_sqlite::import(&source, &folder).expect_err("imported");
    assert!(refused.to_string().contains("99.0.0"), "{refused}");
    assert!(!folder.join(graphicle_runtime::db::SCHEMA_FILE).exists());
}

/// With the URL set, a data folder with only `reviews.sqlite3` is refused
/// until it is imported by the opens that create nothing as well, instead
/// of silently using the SQLite store.
#[test]
fn a_sqlite_only_folder_is_refused_by_read_only_opens_too() {
    if !postgres_store() {
        return;
    }
    let dir = TempDir::new().unwrap();
    let folder = dir.path().join("data");
    sqlite_source(&folder);
    assert!(Store::existing(&folder).is_err());
    let Err(refused) = graphicle_runtime::db::open_existing(&folder) else {
        panic!("a SQLite-only folder opened with the URL set");
    };
    assert!(refused.to_string().contains("import-sqlite"), "{refused}");
    assert!(Store::open(&folder).is_err());
    assert!(!folder.join(graphicle_runtime::db::SCHEMA_FILE).exists());
}
