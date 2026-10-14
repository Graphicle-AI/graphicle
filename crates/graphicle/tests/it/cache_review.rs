use graphicle_runtime::embedding_cache::Cache;
use rusqlite::{Connection, params};

#[test]
fn malformed_sqlite_values_are_cache_misses() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("vectors.sqlite");
    let cache = Cache::open(&path, "model").unwrap();
    let db = Connection::open(&path).unwrap();
    let text = "private source";
    db.execute(
        "INSERT INTO am_embedding_cache(model_key,text_sha256,vector) VALUES(?,?,?)",
        params![
            "passage:model",
            graphicle_memory::text::sha256(text.as_bytes()),
            "not a vector"
        ],
    )
    .unwrap();
    assert_eq!(cache.get(text).unwrap(), None);
}

#[cfg(unix)]
#[test]
fn an_existing_cache_and_its_sidecars_are_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("vectors.sqlite");
    let db = Connection::open(&path).unwrap();
    db.pragma_update(None, "journal_mode", "WAL").unwrap();
    db.execute_batch("CREATE TABLE retained(value); INSERT INTO retained VALUES(1)")
        .unwrap();
    let paths = [
        path.clone(),
        path.with_extension("sqlite-wal"),
        path.with_extension("sqlite-shm"),
    ];
    for path in &paths {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644)).unwrap();
    }
    let _cache = Cache::open(&path, "model").unwrap();
    for path in &paths {
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o077,
            0,
            "cache artifact is readable by other users: {}",
            path.display()
        );
    }
    assert_eq!(
        db.query_row("SELECT value FROM retained", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        1
    );
}

#[cfg(unix)]
#[test]
fn a_symlinked_sidecar_is_rejected_without_changing_its_target() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let directory = tempfile::tempdir().unwrap();
    let target = directory.path().join("unrelated");
    std::fs::write(&target, b"keep this file").unwrap();
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644)).unwrap();
    let path = directory.path().join("cache");
    symlink(&target, directory.path().join("cache-wal")).unwrap();
    assert!(Cache::open(&path, "model").is_err());
    assert_eq!(std::fs::read(&target).unwrap(), b"keep this file");
    assert_eq!(
        std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
        0o644
    );
}
