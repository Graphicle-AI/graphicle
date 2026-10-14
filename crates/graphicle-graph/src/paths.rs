// Adapted from Graphify 0.9.88 (a9860f70), Apache-2.0; see NOTICE.md.
// Upstream: graphify/affected.py, graphify/paths.py
//! Port of graphify/paths.py: durable identity spelling and atomic output helpers.
pub use crate::security::{DEFAULT_OUT_DIR, OUT_DIR};
/// The configuration folder: `~/.graphicle-graph` (global graph, provider
/// settings, cloned repositories, grammar packs) and a project's
/// `.graphicle-graph/providers.json`. Not an output folder, although its name
/// starts with [`DEFAULT_OUT_DIR`].
pub const CONFIG_DIR: &str = ".graphicle-graph";
use crate::{Result, fsutil, graph::Graph, path_syntax, python_json, regex_compat};
use regex::Regex;
use serde_json::{Map, Value, json};
use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    sync::LazyLock,
};
use unicode_normalization::UnicodeNormalization;
pub const WINDOWS_MAX_PATH: i64 = 260;
pub const MIN_STEM_BUDGET: i64 = 16;
pub static OUT_DIR_NAME: LazyLock<String> = LazyLock::new(|| out_dir_name(&OUT_DIR));
static TEST_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        r"(?i)^test_.*",
        r"(?i).*_test\..+$",
        r"(?i).*\.test\..+$",
        r"(?i).*\.spec\..+$",
        r"(?i).*_spec\..+$",
        r"(?i).*\.tests\.ps1$",
        r".*Test\.java$",
        r".*Tests\.java$",
        r".*Tests\.cs$",
    ]
    .iter()
    .map(|pattern| regex_compat::compile(pattern))
    .collect()
});
pub fn normpath(path: &str) -> String {
    let mut parts = Vec::<String>::new();
    for part in path_syntax::posix_parts(path) {
        if part == ".." {
            if parts
                .last()
                .is_some_and(|p| !["..", "/", "//"].contains(&p.as_str()))
            {
                parts.pop();
            } else if parts
                .first()
                .is_none_or(|p| !["/", "//"].contains(&p.as_str()))
            {
                parts.push(part);
            }
        } else {
            parts.push(part);
        }
    }
    path_syntax::posix_join(&parts)
}
pub fn out_dir_name(output: &str) -> String {
    let p = normpath(output);
    p.rsplit('/').next().unwrap_or("").into()
}
pub fn out_path_with(output: &str, parts: &[String]) -> PathBuf {
    let mut path = PathBuf::from(output);
    for part in parts {
        path.push(part);
    }
    path
}
pub fn out_path(parts: &[String]) -> PathBuf {
    out_path_with(&OUT_DIR, parts)
}
pub fn default_graph_json() -> String {
    fsutil::path_string(&out_path(&["graph.json".into()]))
}
pub fn nfc(text: &str) -> String {
    text.nfc().collect()
}
pub fn is_test_path(path: &str) -> bool {
    let parts = path_syntax::posix_parts(&path.replace('\\', "/"));
    if parts.iter().any(|p| {
        ["tests", "test", "spec", "specs", "__tests__"].contains(&p.to_lowercase().as_str())
    }) {
        return true;
    }
    let name = path_syntax::posix_name(&parts);
    TEST_PATTERNS.iter().any(|pattern| pattern.is_match(name))
}
fn parent_parts(path: &str) -> Vec<String> {
    let mut parts = path_syntax::posix_parts(&path.replace('\\', "/"));
    if parts.last().is_some_and(|p| p != "/" && p != "//") {
        parts.pop();
    }
    parts
}
pub fn path_proximity_winner(call_site: &str, candidates: &Map<String, Value>) -> Option<String> {
    if call_site.is_empty() {
        return None;
    }
    let call = call_site.replace('\\', "/");
    let parent = parent_parts(&call);
    let files: Vec<_> = candidates
        .iter()
        .map(|(id, file)| (id, crate::python_compat::string(file).replace('\\', "/")))
        .collect();
    let same: Vec<_> = files.iter().filter(|(_, file)| file == &call).collect();
    if !same.is_empty() {
        return (same.len() == 1).then(|| same[0].0.clone());
    }
    let same: Vec<_> = files
        .iter()
        .filter(|(_, file)| parent_parts(file) == parent)
        .collect();
    if !same.is_empty() {
        return (same.len() == 1).then(|| same[0].0.clone());
    }
    let scored: Vec<_> = files
        .iter()
        .map(|(id, file)| {
            (
                *id,
                parent
                    .iter()
                    .zip(parent_parts(file))
                    .take_while(|(a, b)| a == &b)
                    .count(),
            )
        })
        .collect();
    let best = scored.iter().map(|(_, score)| *score).max()?;
    let winners: Vec<_> = scored.iter().filter(|(_, score)| *score == best).collect();
    (best > 0 && winners.len() == 1).then(|| winners[0].0.clone())
}
pub fn disambiguate_ambiguous_candidates(
    candidates: &[String],
    files: &Map<String, Value>,
    call: &str,
) -> Option<String> {
    if candidates.len() <= 1 {
        return candidates.first().cloned();
    }
    let source = |id: &String| {
        files
            .get(id)
            .map(crate::python_compat::string)
            .unwrap_or_default()
    };
    let test: Vec<_> = candidates
        .iter()
        .filter(|id| is_test_path(&source(id)))
        .cloned()
        .collect();
    let non: Vec<_> = candidates
        .iter()
        .filter(|id| !test.contains(id))
        .cloned()
        .collect();
    let survivors = if is_test_path(call) {
        let same: Vec<_> = test
            .iter()
            .filter(|id| source(id).replace('\\', "/") == call.replace('\\', "/"))
            .collect();
        if same.len() == 1 {
            return Some(same[0].clone());
        }
        if !test.is_empty() {
            test
        } else if !non.is_empty() {
            non
        } else {
            candidates.to_vec()
        }
    } else {
        non
    };
    if survivors.len() <= 1 {
        return survivors.first().cloned();
    }
    let files = survivors
        .iter()
        .map(|id| (id.clone(), json!(source(id))))
        .collect();
    path_proximity_winner(call, &files)
}
pub fn is_absolute_any_platform(path: Option<&str>) -> bool {
    let Some(path) = path else {
        return false;
    };
    if path.starts_with('/') {
        return true;
    }
    let path = path.replace('/', "\\");
    let c: Vec<char> = path.chars().collect();
    if c.get(1) == Some(&':') && c.get(2) == Some(&'\\') {
        return true;
    }
    if !path.starts_with("\\\\") {
        return false;
    }
    let start = if path
        .get(..8)
        .is_some_and(|s| s.eq_ignore_ascii_case("\\\\?\\UNC\\"))
    {
        8
    } else {
        2
    };
    if let Some(first) = path
        .get(start..)
        .and_then(|s| s.find('\\'))
        .map(|i| i + start)
        && path[first + 1..].contains('\\')
    {
        return true;
    }
    if path.ends_with('\\') {
        return false;
    }
    let parts: Vec<_> = path.split('\\').collect();
    (parts.len() == 4 && !"?.".contains(parts[2])) || parts.len() == 6
}
pub fn stem_filename_budget_with(
    absolute_dir: &str,
    windows: bool,
    reserve: i64,
    limit: i64,
) -> i64 {
    if !windows || absolute_dir.starts_with("\\\\?\\") {
        return limit;
    }
    MIN_STEM_BUDGET.max(
        limit.min(WINDOWS_MAX_PATH - 1 - absolute_dir.chars().count() as i64 - 1 - reserve - 3),
    )
}
pub fn stem_filename_budget(output_dir: &Path, reserve: i64, limit: i64) -> i64 {
    if !cfg!(windows) {
        return limit;
    }
    let base = if output_dir.is_absolute() {
        output_dir.to_path_buf()
    } else if let Ok(cwd) = std::env::current_dir() {
        cwd.join(output_dir)
    } else {
        return limit;
    };
    stem_filename_budget_with(&base.to_string_lossy(), true, reserve, limit)
}

pub struct ReplaceError {
    pub error: io::Error,
    pub winerror: Option<i32>,
}
pub trait ReplaceOps {
    fn replace(&self, src: &Path, dst: &Path) -> std::result::Result<(), ReplaceError>;
    fn rename(&self, src: &Path, dst: &Path) -> io::Result<()>;
}
pub struct SystemReplace;
impl ReplaceOps for SystemReplace {
    fn replace(&self, src: &Path, dst: &Path) -> std::result::Result<(), ReplaceError> {
        fs::rename(src, dst).map_err(|error| ReplaceError {
            winerror: if cfg!(windows) {
                error.raw_os_error()
            } else {
                None
            },
            error,
        })
    }
    fn rename(&self, src: &Path, dst: &Path) -> io::Result<()> {
        fs::rename(src, dst)
    }
}
fn same_spelling(src: &Path, dst: &Path) -> io::Result<bool> {
    let cwd = std::env::current_dir()?;
    let spell = |p: &Path| {
        let p = if p.is_absolute() {
            p.to_path_buf()
        } else {
            cwd.join(p)
        };
        let p = normpath(&p.to_string_lossy());
        if cfg!(windows) { p.to_lowercase() } else { p }
    };
    Ok(spell(src) == spell(dst))
}
pub fn os_replace_with_fallback_with(
    src: &Path,
    dst: &Path,
    ops: &impl ReplaceOps,
) -> io::Result<()> {
    match ops.replace(src, dst) {
        Ok(()) => return Ok(()),
        Err(failure) => {
            if failure.error.kind() != io::ErrorKind::PermissionDenied
                && failure.winerror != Some(17)
            {
                return Err(failure.error);
            }
        }
    }
    if same_spelling(src, dst)? {
        return Ok(());
    }
    let dir = dst
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let copied = tempfile::Builder::new()
        .prefix(".gfy-replace-")
        .suffix(".tmp")
        .tempfile_in(dir)?;
    let copied_path = copied.into_temp_path();
    fs::copy(src, &copied_path)?;
    if let Ok(metadata) = fs::metadata(src) {
        let mut times = fs::FileTimes::new();
        if let Ok(at) = metadata.accessed() {
            times = times.set_accessed(at);
        }
        if let Ok(at) = metadata.modified() {
            times = times.set_modified(at);
        }
        if let Ok(file) = fs::File::options().write(true).open(&copied_path) {
            let _ = file.set_times(times);
        }
    }
    let backup = if fs::symlink_metadata(dst).is_ok() {
        let backup = tempfile::Builder::new()
            .prefix(".gfy-replace-bak-")
            .suffix(".tmp")
            .tempfile_in(dir)?;
        let path = backup.path().to_path_buf();
        backup.close()?;
        ops.rename(dst, &path)?;
        Some(path)
    } else {
        None
    };
    if let Err(error) = ops.rename(&copied_path, dst) {
        if let Some(backup) = &backup {
            let _ = ops.rename(backup, dst);
        }
        return Err(error);
    }
    if let Some(backup) = backup {
        let _ = fs::remove_file(backup);
    }
    fs::remove_file(src)
}
pub fn os_replace_with_fallback(src: &Path, dst: &Path) -> io::Result<()> {
    os_replace_with_fallback_with(src, dst, &SystemReplace)
}
pub fn atomic_replace_with(
    path: &Path,
    write: impl FnOnce(&mut fs::File) -> io::Result<()>,
    ops: &impl ReplaceOps,
) -> io::Result<()> {
    let real = fsutil::resolve_missing(path)?;
    let dir = real.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(dir)?;
    let mut builder = tempfile::Builder::new();
    builder.prefix(".gfy-").suffix(".tmp");
    // Let the creation syscall apply umask; no process-global umask mutation.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(fs::Permissions::from_mode(0o666));
    }
    let mut temp = builder.tempfile_in(dir)?;
    let created_permissions = temp.as_file().metadata()?.permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // No content is written while the temporary file has its final mode.
        // It stays private until the write succeeds, as mkstemp does upstream.
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o600))?;
    }
    write(temp.as_file_mut())?;
    temp.as_file_mut().flush()?;
    if let Ok(metadata) = fs::metadata(&real) {
        let _ = fs::set_permissions(temp.path(), metadata.permissions());
    } else {
        let _ = fs::set_permissions(temp.path(), created_permissions);
    }
    let temp = temp.into_temp_path();
    // On Windows a replace refuses a read-only destination and the fallback
    // would rename it aside and clobber it: refuse here, for this generic
    // write path only (0.9.83). Install and cache callers of the fallback
    // still replace their own, possibly read-only, copies.
    #[cfg(windows)]
    if fs::metadata(&real).is_ok_and(|m| m.is_file() && m.permissions().readonly()) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "[Errno 13] destination is read-only: {}",
                crate::python_compat::repr_string(&fsutil::path_string(&real))
            ),
        ));
    }
    let result = os_replace_with_fallback_with(&temp, &real, ops);
    if result.is_err() {
        #[cfg(windows)]
        if let Ok(metadata) = fs::metadata(&temp) {
            let mut permissions = metadata.permissions();
            permissions.set_readonly(false);
            let _ = fs::set_permissions(&temp, permissions);
        }
    }
    result
}
pub fn write_text_atomic(path: &Path, text: &str) -> io::Result<()> {
    atomic_replace_with(path, |f| f.write_all(text.as_bytes()), &SystemReplace)
}
/// Preserve the inode and modification time when generated UTF-8 is unchanged.
pub fn write_text_atomic_if_changed(path: &Path, text: &str) -> io::Result<bool> {
    match fs::read(path) {
        Ok(existing) if existing == text.as_bytes() => return Ok(false),
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    write_text_atomic(path, text)?;
    Ok(true)
}
pub fn write_json_atomic(
    path: &Path,
    value: &Value,
    indent: Option<usize>,
    ensure_ascii: bool,
) -> io::Result<()> {
    write_serialize_atomic(path, value, indent, ensure_ascii)
}
/// `write_json_atomic(path, obj, compact=True)` (0.9.86, #3413): one line
/// with no padding after `,`/`:`, streamed into the temporary file.
pub fn write_json_atomic_compact<T: serde::Serialize>(
    path: &Path,
    value: &T,
    ensure_ascii: bool,
) -> io::Result<()> {
    write_json_atomic_with(path, value, None, ensure_ascii, true)
}
/// `write_json_atomic(path, obj, indent=..., ensure_ascii=..., compact=...)`.
pub fn write_json_atomic_with<T: serde::Serialize>(
    path: &Path,
    value: &T,
    indent: Option<usize>,
    ensure_ascii: bool,
    compact: bool,
) -> io::Result<()> {
    atomic_replace_with(
        path,
        |f| {
            let mut buffered = io::BufWriter::with_capacity(1 << 16, f);
            python_json::write_with(&mut buffered, value, indent, ensure_ascii, compact)?;
            buffered.flush()
        },
        &SystemReplace,
    )
}
/// The compact graph.json text watch writes and compares (0.9.86): raw UTF-8,
/// `separators=(",", ":")`, one trailing newline.
pub fn compact_json_text<T: serde::Serialize>(value: &T) -> io::Result<String> {
    let mut text = Vec::new();
    python_json::write_compact(&mut text, value, false)?;
    text.push(b'\n');
    String::from_utf8(text).map_err(io::Error::other)
}
/// Whether `path` holds one line: the compact graph.json layout (0.9.86).
pub fn is_single_line(path: &Path) -> io::Result<bool> {
    use std::io::BufRead;
    let mut reader = io::BufReader::new(fs::File::open(path)?);
    let mut line = Vec::new();
    reader.read_until(b'\n', &mut line)?;
    line.clear();
    Ok(reader.read_until(b'\n', &mut line)? == 0)
}
/// [`write_json_atomic`] for any serializable document, so large outputs can
/// be produced record by record instead of as one `Value` tree.
pub fn write_serialize_atomic<T: serde::Serialize>(
    path: &Path,
    value: &T,
    indent: Option<usize>,
    ensure_ascii: bool,
) -> io::Result<()> {
    atomic_replace_with(
        path,
        |f| {
            // serde_json emits one write per fragment; unbuffered, a large
            // graph.json costs tens of thousands of write(2) calls.
            let mut buffered = io::BufWriter::with_capacity(1 << 16, f);
            python_json::write(&mut buffered, value, indent, ensure_ascii)?;
            buffered.flush()
        },
        &SystemReplace,
    )
}
fn node_link_edge_field(data: &Value) -> &'static str {
    if data.get("links").is_none() && data.get("edges").is_some() {
        "edges"
    } else {
        "links"
    }
}
/// Load either graph.json writer, keeping every link's stored direction as
/// `_src`/`_tgt`; read direction from those markers, never from endpoint order.
pub fn load_node_link_graph(data_or_path: &Value) -> Result<Graph> {
    if data_or_path.is_object() {
        return Graph::from_node_link_with_arcs(data_or_path, node_link_edge_field(data_or_path));
    }
    let path = Path::new(data_or_path.as_str().ok_or("Expected graph data or path")?);
    crate::security::check_graph_file_size_cap(path)?;
    let text =
        fs::read_to_string(path).map_err(|error| crate::io_compat::read_error(&error, path))?;
    let data: Value = serde_json::from_str(&text).map_err(crate::Error::from)?;
    load_node_link_graph_owned(data)
}
/// [`load_node_link_graph`] of a parsed document the caller no longer needs.
pub fn load_node_link_graph_owned(data: Value) -> Result<Graph> {
    let edges_key = node_link_edge_field(&data);
    Graph::from_node_link_owned_with_arcs(data, edges_key)
}

pub fn dispatch(request: &Value) -> Result<Value> {
    let string = |field: &str| {
        request[field]
            .as_str()
            .ok_or_else(|| format!("Expected {field}"))
    };
    match request["operation"].as_str().unwrap_or("") {
        "paths.classify" => Ok(
            json!({"test":is_test_path(request["path"].as_str().unwrap_or("")),"absolute":is_absolute_any_platform(request["path"].as_str())}),
        ),
        "paths.disambiguate" => {
            let candidates: Vec<String> = serde_json::from_value(request["candidates"].clone())
                .map_err(|_| "Expected candidates")?;
            Ok(json!(disambiguate_ambiguous_candidates(
                &candidates,
                request["files"].as_object().ok_or("Expected file map")?,
                string("call_site")?
            )))
        }
        "paths.proximity" => Ok(json!(path_proximity_winner(
            string("call_site")?,
            request["files"].as_object().ok_or("Expected file map")?
        ))),
        "paths.stem_budget" => Ok(json!(stem_filename_budget_with(
            string("path")?,
            request["windows"].as_bool().unwrap_or(cfg!(windows)),
            request["reserve"].as_i64().unwrap_or(0),
            request["limit"].as_i64().unwrap_or(200)
        ))),
        "paths.output" => {
            let parts: Vec<String> =
                serde_json::from_value(request["parts"].clone()).map_err(|_| "Expected parts")?;
            let output = string("output")?;
            Ok(
                json!({"name":out_dir_name(output),"path":fsutil::path_string(&out_path_with(output,&parts))}),
            )
        }
        "paths.nfc" => Ok(json!(nfc(string("text")?))),
        "paths.write_text" => {
            write_text_atomic(Path::new(string("path")?), string("text")?)
                .map_err(crate::Error::from)?;
            Ok(Value::Null)
        }
        "paths.write_json" => {
            write_json_atomic_with(
                Path::new(string("path")?),
                &request["value"],
                request["indent"].as_i64().map(|n| n.max(0) as usize),
                request["ensure_ascii"].as_bool().unwrap_or(true),
                request["compact"].as_bool().unwrap_or(false),
            )
            .map_err(crate::Error::from)?;
            Ok(Value::Null)
        }
        "paths.out_dir" => {
            let (dir, warning) = crate::security::out_dir_from(request["value"].as_str());
            Ok(json!({"value": dir, "warning": warning}))
        }
        "paths.load_graph" => {
            Ok(load_node_link_graph(&request["data_or_path"])?.to_node_link("links"))
        }
        "paths.replace" => {
            os_replace_with_fallback(Path::new(string("src")?), Path::new(string("dst")?))
                .map_err(crate::Error::from)?;
            Ok(Value::Null)
        }
        _ => Err("Unknown paths operation".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    struct Fault {
        permission: bool,
        winerror: Option<i32>,
        rename_fail: usize,
        renames: Cell<usize>,
    }
    impl ReplaceOps for Fault {
        fn replace(&self, _: &Path, _: &Path) -> std::result::Result<(), ReplaceError> {
            Err(ReplaceError {
                error: io::Error::new(
                    if self.permission {
                        io::ErrorKind::PermissionDenied
                    } else {
                        io::ErrorKind::Other
                    },
                    "replace failure",
                ),
                winerror: self.winerror,
            })
        }
        fn rename(&self, src: &Path, dst: &Path) -> io::Result<()> {
            self.renames.set(self.renames.get() + 1);
            if self.renames.get() == self.rename_fail {
                return Err(io::Error::other("failed final rename"));
            }
            fs::rename(src, dst)
        }
    }
    fn fault(permission: bool, winerror: Option<i32>, rename_fail: usize) -> Fault {
        Fault {
            permission,
            winerror,
            rename_fail,
            renames: Cell::new(0),
        }
    }
    #[test]
    fn partial_writer_and_commit_failures_preserve_old_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("graph.json");
        fs::write(&path, "old").unwrap();
        assert!(
            atomic_replace_with(
                &path,
                |file| {
                    file.write_all(b"partial")?;
                    Err(io::Error::other("disk full"))
                },
                &SystemReplace
            )
            .is_err()
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), "old");
        assert!(
            atomic_replace_with(
                &path,
                |file| file.write_all(b"replacement"),
                &fault(false, None, 0)
            )
            .is_err()
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), "old");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }
    #[test]
    fn permission_and_winerror17_recover_without_leftover_files() {
        for error in [fault(true, None, 0), fault(false, Some(17), 0)] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("graph.json");
            fs::write(&path, "old").unwrap();
            atomic_replace_with(&path, |file| file.write_all(b"new"), &error).unwrap();
            assert_eq!(fs::read_to_string(&path).unwrap(), "new");
            assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
        }
    }
    #[test]
    fn failed_final_swap_restores_backup_and_keeps_source() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        let dst = dir.path().join("dst");
        fs::write(&src, "new").unwrap();
        fs::write(&dst, "old").unwrap();
        assert!(os_replace_with_fallback_with(&src, &dst, &fault(false, Some(17), 2)).is_err());
        assert_eq!(fs::read_to_string(&src).unwrap(), "new");
        assert_eq!(fs::read_to_string(&dst).unwrap(), "old");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 2);
        os_replace_with_fallback_with(&src, &src, &fault(true, None, 0)).unwrap();
        assert_eq!(fs::read_to_string(src).unwrap(), "new");
    }
    #[test]
    #[cfg(unix)]
    fn replace_moves_symlink_itself_but_atomic_write_follows_it() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        let link = dir.path().join("link");
        let src = dir.path().join("src");
        fs::write(&target, "original").unwrap();
        symlink(&target, &link).unwrap();
        write_text_atomic(&link, "through link").unwrap();
        assert!(link.is_symlink());
        assert_eq!(fs::read_to_string(&target).unwrap(), "through link");
        fs::write(&src, "replace link").unwrap();
        os_replace_with_fallback_with(&src, &link, &fault(true, None, 0)).unwrap();
        assert!(!link.is_symlink());
        assert_eq!(fs::read_to_string(&link).unwrap(), "replace link");
        assert_eq!(fs::read_to_string(&target).unwrap(), "through link");
    }
    #[test]
    #[cfg(unix)]
    fn temporary_file_is_private_and_name_is_bounded() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(format!("{}.json", "a".repeat(200)));
        atomic_replace_with(
            &path,
            |file| {
                let files = fs::read_dir(dir.path())?.collect::<io::Result<Vec<_>>>()?;
                assert_eq!(files.len(), 1);
                let name = files[0].file_name().to_string_lossy().into_owned();
                assert!(name.starts_with(".gfy-") && name.ends_with(".tmp") && name.len() <= 25);
                assert_eq!(file.metadata()?.permissions().mode() & 0o777, 0o600);
                file.write_all(b"private until commit")
            },
            &SystemReplace,
        )
        .unwrap();
    }
}
