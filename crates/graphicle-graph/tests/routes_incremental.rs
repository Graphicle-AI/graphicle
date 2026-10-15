//! `--routes` (beyond the upstream port): an incremental rebuild keeps route paths
//! exact when the prefix a router is mounted with lives in another file.
//! Its own test binary, since `--routes` is process-wide.
use graphicle::watch::{RebuildOptions, rebuild};
use serde_json::Value;
use std::{fs, path::Path, sync::atomic::Ordering};

fn write(root: &Path, name: &str, body: &str) {
    let p = root.join(name);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, body).unwrap();
}

fn routes(root: &Path) -> Vec<String> {
    let text = fs::read_to_string(root.join(".graphicle/graph.json")).unwrap();
    let graph: Value = serde_json::from_str(&text).unwrap();
    let mut out = graph["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|n| n["node_kind"] == "route")
        .map(|n| n["label"].as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    out.sort();
    out
}

fn changed(root: &Path, name: &str) -> RebuildOptions {
    RebuildOptions {
        changed_paths: Some(vec![root.join(name)]),
        ..Default::default()
    }
}

const MAIN: &str = "from fastapi import FastAPI\nfrom .routers import items\n\napp = FastAPI()\n\
                    app.include_router(items.router, prefix=\"/v1\")\n";
const ITEMS: &str = "from fastapi import APIRouter\n\nrouter = APIRouter()\n\n\
                     @router.get(\"/items\")\ndef list_items():\n    return []\n";
const MORE: &str =
    "\n\n@router.get(\"/items/{item_id}\")\ndef read_item(item_id):\n    return item_id\n";

/// Extraction keeps process-wide caches: one run at a time.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// `canonicalize` in the engine's plain spelling: on Windows without the
/// verbatim prefix (`\\?\C:\x` is `C:\x`), as the command line resolves a
/// root; elsewhere `canonicalize` itself.
fn canonical(path: &Path) -> std::path::PathBuf {
    let path = path.canonicalize().unwrap();
    match path.to_str().and_then(|s| s.strip_prefix(r"\\?\")) {
        Some(plain) if cfg!(windows) && plain.as_bytes().get(1) == Some(&b':') => plain.into(),
        _ => path,
    }
}

fn corpus() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = canonical(dir.path());
    write(&root, "app/__init__.py", "");
    write(&root, "app/main.py", MAIN);
    write(&root, "app/routers/__init__.py", "");
    write(&root, "app/routers/items.py", ITEMS);
    (dir, root)
}

#[test]
fn incremental_rebuilds_keep_route_prefixes_from_other_files() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    graphicle::extractors::routes::ENABLED.store(true, Ordering::Relaxed);
    let (_dir, root) = corpus();
    assert!(rebuild(&root, &RebuildOptions::default()).unwrap().success);
    assert_eq!(routes(&root), ["GET /v1/items"]);

    // Only main.py changed, but the route in items.py moves: every file is
    // rebuilt.
    write(&root, "app/main.py", &MAIN.replace("/v1", "/v2"));
    let result = rebuild(&root, &changed(&root, "app/main.py")).unwrap();
    assert!(result.success, "{:?}", result.messages);
    assert!(
        result
            .messages
            .iter()
            .any(|m| m.contains("Route mounts changed")),
        "{:?}",
        result.messages
    );
    assert_eq!(routes(&root), ["GET /v2/items"]);

    // Only the router changed: an incremental rebuild that still applies the
    // prefix main.py gives (main.py is read as context).
    write(&root, "app/routers/items.py", &format!("{ITEMS}{MORE}"));
    let result = rebuild(&root, &changed(&root, "app/routers/items.py")).unwrap();
    assert!(result.success, "{:?}", result.messages);
    assert!(
        !result
            .messages
            .iter()
            .any(|m| m.contains("Route mounts changed")),
        "{:?}",
        result.messages
    );
    let incremental = routes(&root);
    assert_eq!(incremental, ["GET /v2/items", "GET /v2/items/{item_id}"]);

    // A full build from scratch agrees.
    fs::remove_dir_all(root.join(".graphicle")).unwrap();
    assert!(rebuild(&root, &RebuildOptions::default()).unwrap().success);
    assert_eq!(routes(&root), incremental);
}

fn extract(root: &Path) -> String {
    let out = graphicle::cli::run(&[
        "extract".to_string(),
        root.to_string_lossy().into_owned(),
        "--code-only".to_string(),
        "--routes".to_string(),
    ]);
    assert_eq!(out.exit_code, 0, "{}{}", out.stdout, out.stderr);
    out.stdout
}

#[test]
fn incremental_extract_keeps_route_prefixes_from_other_files() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_dir, root) = corpus();
    extract(&root);
    assert_eq!(routes(&root), ["GET /v1/items"]);
    write(&root, "app/main.py", &MAIN.replace("/v1", "/v2"));
    let stdout = extract(&root);
    assert!(stdout.contains("route mounts changed"), "{stdout}");
    assert_eq!(routes(&root), ["GET /v2/items"]);
    write(&root, "app/routers/items.py", &format!("{ITEMS}{MORE}"));
    let stdout = extract(&root);
    assert!(stdout.contains("changed (incremental)"), "{stdout}");
    assert!(!stdout.contains("route mounts changed"), "{stdout}");
    assert_eq!(routes(&root), ["GET /v2/items", "GET /v2/items/{item_id}"]);
}
