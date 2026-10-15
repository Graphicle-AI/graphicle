//! One project identity: the folder a command runs in decides
//! its project, for every command and hook.
//!
//! A local project is registered under the key `local:` and the digest of
//! its canonical folder ([`folder_key`]). Every local sync that commits also
//! records the folder it read as an alias of the project it synced into
//! (`repo_folders`, with the canonical path: local stores record paths;
//! projects created over HTTP record none). So a folder synced
//! with `--project` into a project created in the web app is found as well.
//!
//! - [`lookup`]: the nearest registered folder of a path and its project,
//!   through aliases and keys. Only a project that holds a snapshot counts:
//!   a project `graphicle pr` registered for a checkout is no folder's
//!   project until a sync fills it. The nearest registered folder decides
//!   even when the principal may not read its project; the lookup then says
//!   so instead of falling through to an ancestor's project.
//! - [`requested`]: a `--project` value, which may be a project id, a project
//!   key, a recorded folder key or a path.
//! - [`Store::resolve_project`]: `--project`, else the working directory,
//!   else the saved default project (only when the working directory
//!   resolves to nothing, as for a client started outside any folder).
//!
//! `graphicle pr` keeps its commit trees in a PR scratch project linked to
//! the folder's project ([`Store::pr_scratch_for`]), so the folder's project
//! keeps its heads and its episodes.
use crate::graph_projection::{PR_SCRATCH, repo_readable};
use crate::platform::CanonicalExt;
use crate::store::{Principal, Store, one, repo, s};
use crate::{Error, Result, digest, id, now};
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// The key of the local project of `folder` (a canonical path), and the key
/// its aliases are recorded under.
pub fn folder_key(folder: &Path) -> String {
    // Windows: a verbatim spelling (`\\?\C:\...`) shares its plain form's key.
    let folder = crate::platform::simplified(folder.to_path_buf());
    format!("local:{}", digest(&json!(folder.to_string_lossy())))
}

/// A folder's project.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    pub id: String,
    /// The registered folder: the path itself, or the ancestor that matched.
    pub folder: PathBuf,
    /// How it matched: `alias` (a recorded folder) or `key` (the project's
    /// own `local:` key).
    pub via: &'static str,
}

/// What the nearest registered folder of a path holds for a principal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Lookup {
    /// No registered folder at or above the path, or no such path.
    Nothing,
    /// The nearest registered folder and its project.
    Found(Found),
    /// The nearest registered folder, whose project the principal may not
    /// read. Folders above it are not consulted.
    Unreadable(Found),
}

impl Lookup {
    /// The readable project, `None` when no folder is registered, and an
    /// error naming the folder when the principal may not read its project.
    pub fn readable(self) -> Result<Option<Found>> {
        match self {
            Lookup::Nothing => Ok(None),
            Lookup::Found(found) => Ok(Some(found)),
            Lookup::Unreadable(found) => Err(unreadable(&found.folder)),
        }
    }
}

/// The refusal for a registered folder whose project the principal may not
/// read.
fn unreadable(folder: &Path) -> Error {
    let mut error = Error::denied();
    error.message = format!(
        "The project of {} is not readable with this access key; name another with --project",
        folder.display()
    );
    error
}

/// The project registered for exactly `folder` (canonical): its recorded
/// alias first, then a project keyed by the folder; with `synced`, only a
/// project that holds a snapshot. Returned with whether `p` may read it.
fn at_folder(
    db: &Connection,
    p: &Principal,
    folder: &Path,
    synced: bool,
) -> Result<Option<(Found, bool)>> {
    let key = folder_key(folder);
    let filter = if synced { " AND r.snapshot<>''" } else { "" };
    for (sql, via) in [
        (
            format!(
                "SELECT r.id FROM repo_folders f JOIN repo_projects r ON r.id=f.repository_id \
                 WHERE f.workspace_id=?1 AND f.folder_key=?2{filter}"
            ),
            "alias",
        ),
        (
            format!(
                "SELECT r.id FROM repo_projects r WHERE r.workspace_id=?1 AND r.project_key=?2{filter}"
            ),
            "key",
        ),
    ] {
        if let Some(row) = one(db, &sql, params![p.workspace, key])? {
            let id = s(&row, "id").to_string();
            let readable = repo_readable(db, p, &id)?;
            return Ok(Some((
                Found {
                    id,
                    folder: folder.to_path_buf(),
                    via,
                },
                readable,
            )));
        }
    }
    Ok(None)
}

/// The nearest registered ancestor of `path` (itself included) whose project
/// holds a snapshot, and whether `p` may read that project.
pub fn lookup(db: &Connection, p: &Principal, path: &Path) -> Result<Lookup> {
    let Ok(path) = path.canonical() else {
        return Ok(Lookup::Nothing);
    };
    for folder in path.ancestors() {
        if let Some((found, readable)) = at_folder(db, p, folder, true)? {
            return Ok(if readable {
                Lookup::Found(found)
            } else {
                Lookup::Unreadable(found)
            });
        }
    }
    Ok(Lookup::Nothing)
}

/// [`lookup`]'s project when `p` may read it: what the capture hooks and the
/// `graphicle graph` note use, which pass over a folder whose project the
/// principal may not read (they never fall through to an ancestor's).
pub fn project_for_path(db: &Connection, p: &Principal, path: &Path) -> Result<Option<Found>> {
    Ok(match lookup(db, p, path)? {
        Lookup::Found(found) => Some(found),
        Lookup::Nothing | Lookup::Unreadable(_) => None,
    })
}

/// Record `folder` (canonical) as a folder of `repository`. Syncing a folder
/// into another project moves its alias there. [`crate::local::sync`] calls
/// it in the transaction that completes a sync, so a failed sync moves
/// nothing.
pub fn record(db: &Connection, workspace: &str, repository: &str, folder: &Path) -> Result<()> {
    let stamp = now();
    db.execute(
        "INSERT INTO repo_folders(workspace_id,folder_key,repository_id,path,created_at,updated_at) VALUES(?,?,?,?,?,?) \
         ON CONFLICT(workspace_id,folder_key) DO UPDATE SET repository_id=excluded.repository_id,path=excluded.path,updated_at=excluded.updated_at",
        params![workspace, folder_key(folder), repository, folder.to_string_lossy(), stamp, stamp],
    )?;
    Ok(())
}

/// The folder `repository` was last synced from (canonical): its most
/// recently recorded alias. Earlier aliases (a moved folder's old path, or a
/// folder whose content a later `--project` sync of another folder
/// replaced) still resolve to the project, but only this one describes its
/// current paths.
pub fn current_folder(db: &Connection, repository: &str) -> Result<Option<PathBuf>> {
    Ok(one(
        db,
        "SELECT path FROM repo_folders WHERE repository_id=? AND path IS NOT NULL ORDER BY updated_at DESC,path LIMIT 1",
        [repository],
    )?
    .map(|row| PathBuf::from(s(&row, "path"))))
}

/// The project a `--project` value names, in this order: a project id, a
/// project key, a recorded folder key, or a path (relative to `cwd`). A path
/// resolves as the working directory does: through its nearest registered
/// folder. With `exact` (`sync --project`), the path must be a registered
/// folder itself, so a mistyped subfolder never names an ancestor's project,
/// whose files the sync would replace.
pub fn requested(
    db: &Connection,
    p: &Principal,
    value: &str,
    cwd: Option<&Path>,
    exact: bool,
) -> Result<String> {
    if value.is_empty() || value.len() > 4096 || value.contains('\0') {
        return Err(Error::bad("Invalid project: use an id, a key or a folder"));
    }
    if let Some(id) = by_name(db, p, value)? {
        return Ok(id);
    }
    let path = match cwd {
        Some(cwd) => cwd.join(value),
        None => PathBuf::from(value),
    };
    if !path.exists() {
        return Err(Error::bad(
            "Unknown project: use a project id, a project key or a synced folder",
        ));
    }
    if exact {
        let folder = path.canonical()?;
        return match at_folder(db, p, &folder, false)? {
            Some((found, true)) => Ok(found.id),
            Some((found, false)) => Err(unreadable(&found.folder)),
            None => Err(Error::bad(format!(
                "No project is registered for {} itself; sync it without --project to make it a project, or name the project by its id",
                folder.display()
            ))),
        };
    }
    match lookup(db, p, &path)?.readable()? {
        Some(found) => Ok(found.id),
        None => Err(Error::bad(format!(
            "No project is registered for {}; sync it first",
            path.display()
        ))),
    }
}

/// The project a project id, a project key or a recorded folder key names,
/// checked readable by `p`; `None` when none does.
fn by_name(db: &Connection, p: &Principal, value: &str) -> Result<Option<String>> {
    for sql in [
        "SELECT id FROM repo_projects WHERE workspace_id=? AND id=?",
        "SELECT id FROM repo_projects WHERE workspace_id=? AND project_key=?",
        "SELECT repository_id AS id FROM repo_folders WHERE workspace_id=? AND folder_key=?",
    ] {
        if let Some(row) = one(db, sql, params![p.workspace, value])? {
            return Ok(Some(s(&repo(db, p, s(&row, "id"))?, "id").to_string()));
        }
    }
    Ok(None)
}

/// The project an MCP tool's `repo` argument names: a
/// project id, a project key or a recorded folder key, and over stdio
/// (`cwd`, the folder `graphicle mcp` runs in) also a synced folder,
/// resolved as `--project` resolves a path. Over HTTP (`cwd` is `None`) no
/// path is read: a remote caller never names the server's folders. A project
/// the caller may not read is refused, whichever way it is named.
pub fn named(db: &Connection, p: &Principal, value: &str, cwd: Option<&Path>) -> Result<String> {
    if cwd.is_some() {
        return requested(db, p, value, cwd, false);
    }
    if value.is_empty() || value.len() > 4096 || value.contains('\0') {
        return Err(Error::bad("Invalid project: use a project id or key"));
    }
    by_name(db, p, value)?.ok_or_else(|| Error::bad("Unknown project: use a project id or key"))
}

/// The project a command acts on, and how it was chosen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolved {
    pub id: String,
    pub name: String,
    /// `requested` (`--project`), `folder` (the working directory) or
    /// `saved_default` (the project of the last local sync or login).
    pub source: &'static str,
    /// The registered folder that matched, for `folder`.
    pub folder: Option<PathBuf>,
}

/// The id of the PR scratch project's folder project, when `repository` is
/// a PR scratch project `graphicle pr` linked to one.
pub fn scratch_of(db: &Connection, repository: &str) -> Result<Option<String>> {
    Ok(one(
        db,
        "SELECT project_id FROM repo_pr_scratch WHERE repository_id=?",
        [repository],
    )?
    .map(|row| s(&row, "project_id").to_string()))
}

/// The PR scratch projects `graphicle pr` linked to `project`.
pub fn scratch_projects(db: &Connection, project: &str) -> Result<Vec<String>> {
    Ok(crate::store::rows(
        db,
        "SELECT repository_id FROM repo_pr_scratch WHERE project_id=? ORDER BY repository_id",
        [project],
    )?
    .iter()
    .map(|row| s(row, "repository_id").to_string())
    .collect())
}

impl Store {
    /// The project of a command: `requested` (an id, a key or a path), else
    /// the project of `cwd` (its nearest registered folder), else `fallback`
    /// (the saved default project).
    pub fn resolve_project(
        &self,
        workspace: &str,
        token: &str,
        requested: Option<&str>,
        cwd: Option<&Path>,
        fallback: Option<&str>,
    ) -> Result<Resolved> {
        self.find_project(workspace, token, requested, cwd, fallback)?
            .ok_or_else(|| Error::bad("Select --project or sync a local folder first"))
    }

    /// [`Store::resolve_project`], or `None` when nothing names a project:
    /// no `requested` value, no registered folder at or above `cwd` and no
    /// `fallback`. A registered folder whose project the principal may not
    /// read is refused, never passed over.
    pub fn find_project(
        &self,
        workspace: &str,
        token: &str,
        requested: Option<&str>,
        cwd: Option<&Path>,
        fallback: Option<&str>,
    ) -> Result<Option<Resolved>> {
        self.authorized_read(workspace, token, "read", |db, p| {
            let name = |id: &str| -> Result<String> {
                Ok(one(db, "SELECT name FROM repo_projects WHERE id=?", [id])?
                    .map(|row| s(&row, "name").to_string())
                    .unwrap_or_default())
            };
            if let Some(value) = requested {
                let id = self::requested(db, p, value, cwd, false)?;
                return Ok(Some(Resolved {
                    name: name(&id)?,
                    id,
                    source: "requested",
                    folder: None,
                }));
            }
            if let Some(cwd) = cwd
                && let Some(found) = lookup(db, p, cwd)?.readable()?
            {
                return Ok(Some(Resolved {
                    name: name(&found.id)?,
                    id: found.id,
                    source: "folder",
                    folder: Some(found.folder),
                }));
            }
            fallback
                .map(|id| {
                    Ok(Resolved {
                        name: name(id)?,
                        id: id.to_string(),
                        source: "saved_default",
                        folder: None,
                    })
                })
                .transpose()
        })
    }

    /// The nearest registered folder of `path` and its project, when the
    /// principal may read it (see [`project_for_path`]).
    pub fn project_for_path(
        &self,
        workspace: &str,
        token: &str,
        path: &Path,
    ) -> Result<Option<Found>> {
        self.authorized_read(workspace, token, "read", |db, p| {
            project_for_path(db, p, path)
        })
    }

    /// [`lookup`] for `path`.
    pub fn lookup_folder(&self, workspace: &str, token: &str, path: &Path) -> Result<Lookup> {
        self.authorized_read(workspace, token, "read", |db, p| lookup(db, p, path))
    }

    /// The project a local sync of `folder` (canonical) goes into, and
    /// whether it was created: `requested` (an id, a key, a recorded folder
    /// key, or a registered folder itself), else the project registered for
    /// exactly this folder, even one never synced (never an ancestor's: a
    /// subfolder synced alone is a project of its own), else a new project
    /// under the folder's `local:` key, named `name`. Nothing is recorded
    /// for the folder here: [`crate::local::sync`] records it once the sync
    /// has committed.
    pub fn folder_project(
        &self,
        workspace: &str,
        token: &str,
        folder: &Path,
        requested: Option<&str>,
        cwd: Option<&Path>,
        name: &str,
    ) -> Result<(String, bool)> {
        self.authorized(workspace, token, "collect", |db, p| {
            let (project, created) = match requested {
                Some(value) => (self::requested(db, p, value, cwd, true)?, false),
                None => match at_folder(db, p, folder, false)? {
                    Some((found, true)) => (found.id, false),
                    Some((found, false)) => return Err(unreadable(&found.folder)),
                    None => {
                        let identifier = id();
                        let stamp = now();
                        db.execute(
                            "INSERT INTO repo_projects(id,workspace_id,project_key,name,created_at,updated_at) VALUES(?,?,?,?,?,?)",
                            params![identifier, workspace, folder_key(folder), name, stamp, stamp],
                        )?;
                        (identifier, true)
                    }
                },
            };
            if is_pr_scratch(db, &project)? {
                return Err(Error::bad(
                    "That project holds graphicle pr commit trees; sync folders into their own project",
                ));
            }
            Ok((project, created))
        })
    }

    /// The PR scratch project of `project`: created on first
    /// use under the key `pr:` and the digest of the project's id (never the
    /// id itself), named as the project, marked `pr_scratch` and linked to
    /// `project`, whose id scopes its `stable_id`s. It is readable only while
    /// `project` is, and deleting `project` deletes it. A `project` that is
    /// itself a PR scratch project (one an earlier `graphicle pr` registered
    /// under a `git:` key, or a linked one) is its own scratch project.
    /// Returns `{id, project, created}`.
    pub fn pr_scratch_for(&self, workspace: &str, token: &str, project: &str) -> Result<Value> {
        self.authorized(workspace, token, "collect", |db, p| {
            let folder_project = repo(db, p, project)?;
            let project = s(&folder_project, "id");
            if is_pr_scratch(db, project)? {
                return Ok(json!({"id":project,"project":scratch_of(db, project)?,"created":false}));
            }
            let key = format!("pr:{}", digest(&json!(project)));
            let (identifier, created) = match one(
                db,
                "SELECT id FROM repo_projects WHERE workspace_id=? AND project_key=?",
                params![workspace, key],
            )? {
                Some(row) => (s(&repo(db, p, s(&row, "id"))?, "id").to_string(), false),
                None => {
                    let identifier = id();
                    let stamp = now();
                    db.execute(
                        "INSERT INTO repo_projects(id,workspace_id,project_key,name,created_at,updated_at) VALUES(?,?,?,?,?,?)",
                        params![identifier, workspace, key, s(&folder_project, "name"), stamp, stamp],
                    )?;
                    (identifier, true)
                }
            };
            db.execute(
                "INSERT OR IGNORE INTO repo_project_roles(repository_id,role,created_at) VALUES(?,?,?)",
                params![identifier, PR_SCRATCH, now()],
            )?;
            db.execute(
                "INSERT OR IGNORE INTO repo_pr_scratch(repository_id,project_id,created_at) VALUES(?,?,?)",
                params![identifier, project, now()],
            )?;
            Ok(json!({"id":identifier,"project":project,"created":created}))
        })
    }
}

/// Whether `repository` carries the `pr_scratch` role.
pub fn is_pr_scratch(db: &Connection, repository: &str) -> Result<bool> {
    Ok(one(
        db,
        "SELECT 1 AS present FROM repo_project_roles WHERE repository_id=? AND role=?",
        params![repository, PR_SCRATCH],
    )?
    .is_some())
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::folder_key;
    use std::path::Path;

    #[test]
    fn verbatim_and_plain_folders_share_a_key() {
        assert_eq!(
            folder_key(Path::new(r"\\?\C:\work\app")),
            folder_key(Path::new(r"C:\work\app"))
        );
    }
}
