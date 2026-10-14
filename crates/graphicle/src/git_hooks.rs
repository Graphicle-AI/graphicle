//! Freshness Git hooks (`graphicle setup`, step 5). The repository's
//! post-commit, post-merge and post-checkout hooks get a marked block that
//! runs `graphicle sync <folder> --quiet` in the background, so the project
//! follows the working tree. Existing hooks keep their own lines, the hooks
//! folder Git uses (`core.hooksPath`) is respected, and every path is quoted
//! for the shell. The block names the synced folder rather than `.`: Git
//! runs hooks at the repository's top level, and a project synced from a
//! subfolder is that subfolder's.
//!
//! Hooks are written only into the repository's own Git folder. A
//! `core.hooksPath` elsewhere (a hooks folder kept in the repository, such
//! as Husky's, or one shared by every repository) is left alone, and setup
//! prints the line to add there instead.
use crate::{Error, Result};
use std::path::{Path, PathBuf};

/// The hooks that get the block.
pub const HOOKS: [&str; 3] = ["post-commit", "post-merge", "post-checkout"];
/// The start of the block's first line; the synced folder follows it.
const START: &str = "# graphicle-sync-start ";
/// The block's last line.
const END: &str = "# graphicle-sync-end";

/// POSIX shell quoting.
pub fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// A Git repository, as `git` reports it for a folder.
#[derive(Debug, Clone)]
pub struct Repository {
    /// The working tree's top level.
    pub root: PathBuf,
    /// The hooks folder Git runs, when it is the repository's own; else
    /// why the hooks are not written there.
    pub hooks: std::result::Result<PathBuf, String>,
}

/// `git -C folder <args>`'s output, trimmed, when it succeeds with one line.
fn git(folder: &Path, args: &[&str]) -> Option<String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(folder)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    let text = String::from_utf8(output.stdout).ok()?;
    let text = text.trim_end_matches(['\n', '\r']);
    (output.status.success() && !text.is_empty() && !text.contains(['\n', '\0']))
        .then(|| text.to_string())
}

/// A path `git` printed, resolved against `folder` and made canonical.
fn resolved(folder: &Path, printed: &str) -> PathBuf {
    let path = folder.join(printed);
    path.canonicalize().unwrap_or(path)
}

/// The repository containing `folder` (`None` outside one, or without
/// `git`), and the hooks folder setup may write: the one Git runs
/// (`git rev-parse --git-path hooks`, which follows `core.hooksPath`) when
/// it lies in the repository's Git folder.
pub fn repository(folder: &Path) -> Option<Repository> {
    let root = resolved(folder, &git(folder, &["rev-parse", "--show-toplevel"])?);
    let common = git(&root, &["rev-parse", "--git-common-dir"]).map(|dir| resolved(&root, &dir));
    let hooks = match git(&root, &["rev-parse", "--git-path", "hooks"]) {
        None => Err("git did not report the hooks folder".to_string()),
        Some(printed) => {
            let hooks = resolved(&root, &printed);
            match &common {
                Some(common) if hooks.starts_with(common) => Ok(hooks),
                _ => Err(format!(
                    "core.hooksPath points to {}, outside this repository's Git folder",
                    hooks.display()
                )),
            }
        }
    };
    Some(Repository { root, hooks })
}

/// The block for `folder`: in a subshell, so that its variables and exits
/// stay its own. Its sync line reads `"$graphicle" … sync …`, so a reader
/// that looks for a `graphicle … sync` line finds it. It does nothing during a rebase, in another working tree of
/// the repository, or when no Graphicle program is found; it starts the
/// sync in the background with its output discarded, so Git never waits.
pub fn block(
    folder: &Path,
    root: &Path,
    program: &Path,
    data_dir: Option<&Path>,
) -> Result<String> {
    let text = |path: &Path| -> Result<String> {
        let text = path.to_string_lossy();
        if text.contains(['\n', '\r', '\0']) {
            return Err(Error::bad(format!(
                "A path with a line break cannot go into a Git hook: {}",
                path.display()
            )));
        }
        Ok(text.into_owned())
    };
    let folder_text = text(folder)?;
    let data = match data_dir {
        Some(directory) => format!(" --data-dir {}", quote(&text(directory)?)),
        None => String::new(),
    };
    Ok(format!(
        "{START}{folder_text}
# Added by graphicle setup: refreshes Graphicle's copy of this folder in the background. graphicle disconnect removes it.
(
  d=$(git rev-parse --git-dir 2>/dev/null) || exit 0
  [ -d \"$d/rebase-merge\" ] || [ -d \"$d/rebase-apply\" ] && exit 0
  t=$(git rev-parse --show-toplevel 2>/dev/null) && t=$(cd \"$t\" && pwd -P) || exit 0
  [ \"$t\" = {root} ] || exit 0
  graphicle={program}
  [ -x \"$graphicle\" ] || graphicle=$(command -v graphicle 2>/dev/null) || exit 0
  unset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE GIT_PREFIX
  \"$graphicle\"{data} sync {folder} --quiet </dev/null >/dev/null 2>&1 &
)
{END}",
        root = quote(&text(root)?),
        program = quote(&text(program)?),
        folder = quote(&folder_text),
    ))
}

/// The line to add by hand to a hooks folder setup does not write.
pub fn manual_line(folder: &Path, data_dir: Option<&Path>) -> String {
    let data = data_dir
        .map(|directory| format!(" --data-dir {}", quote(&directory.to_string_lossy())))
        .unwrap_or_default();
    format!(
        "graphicle{data} sync {} --quiet >/dev/null 2>&1 &",
        quote(&folder.to_string_lossy())
    )
}

/// The blocks of `text`: each one's lines `start..end` and folder.
fn blocks(lines: &[&str]) -> Vec<(usize, usize, PathBuf)> {
    let mut found = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if let Some(folder) = lines[i].trim_start().strip_prefix(START)
            && let Some(end) = (i + 1..lines.len()).find(|j| lines[*j].trim() == END)
        {
            found.push((i, end + 1, PathBuf::from(folder.trim_end())));
            i = end + 1;
        } else {
            i += 1;
        }
    }
    found
}

/// The folders whose blocks `text` holds.
pub fn folders(text: &str) -> Vec<PathBuf> {
    blocks(&text.split('\n').collect::<Vec<_>>())
        .into_iter()
        .map(|(_, _, folder)| folder)
        .collect()
}

/// Whether a hook file is a shell script Graphicle may add lines to: no
/// `#!` line, or one naming a POSIX-style shell.
fn shell_script(text: &str) -> bool {
    let Some(first) = text.lines().next() else {
        return true;
    };
    let Some(command) = first.strip_prefix("#!") else {
        return true;
    };
    let mut words = command.split_whitespace();
    let Some(program) = words.next() else {
        return false;
    };
    let mut name = Path::new(program)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    if name == "env" {
        name = words
            .find(|word| !word.starts_with('-'))
            .unwrap_or_default()
            .to_string();
    }
    matches!(
        name.as_str(),
        "sh" | "bash" | "zsh" | "dash" | "ksh" | "ash"
    )
}

/// What [`upsert`] did to a hook.
#[derive(Debug, PartialEq)]
pub enum Upserted {
    /// The new text, and `added` or `updated`.
    Changed(String, &'static str),
    /// The block is there as it is.
    Unchanged,
    /// The hook is not a shell script; it was left alone.
    NotShell,
}

/// A hook's text with `block` (for `folder`): brought up to date where it
/// is; else before a final `exit` or `exec` line, which would end the hook
/// before it; else at the end. `None` is a hook that does not exist yet.
pub fn upsert(existing: Option<&str>, block: &str, folder: &Path) -> Upserted {
    let Some(text) = existing else {
        return Upserted::Changed(format!("#!/bin/sh\n{block}\n"), "added");
    };
    let mut lines: Vec<&str> = text.split('\n').collect();
    let new: Vec<&str> = block.split('\n').collect();
    if let Some((start, end, _)) = blocks(&lines)
        .into_iter()
        .find(|(_, _, found)| found == folder)
    {
        if lines[start..end] == new[..] {
            return Upserted::Unchanged;
        }
        lines.splice(start..end, new);
        return Upserted::Changed(lines.join("\n"), "updated");
    }
    if !shell_script(text) {
        return Upserted::NotShell;
    }
    // The last line that runs something.
    let last = lines.iter().rposition(|line| {
        let line = line.trim();
        !line.is_empty() && !line.starts_with('#')
    });
    let ends_hook = last.is_some_and(|i| {
        let line = lines[i].trim();
        line == "exit" || line.starts_with("exit ") || line.starts_with("exec ")
    });
    if let (Some(at), true) = (last, ends_hook) {
        let mut insert = new.clone();
        insert.push("");
        lines.splice(at..at, insert);
        return Upserted::Changed(lines.join("\n"), "added");
    }
    let mut out = text.to_string();
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push('\n');
    out.push_str(block);
    out.push('\n');
    Upserted::Changed(out, "added")
}

/// `text` without the blocks for `folder` (every block when `None`), and
/// the blank line each one brought; `None` when there is none.
pub fn remove(text: &str, folder: Option<&Path>) -> Option<String> {
    let mut lines: Vec<&str> = text.split('\n').collect();
    let found: Vec<(usize, usize, PathBuf)> = blocks(&lines)
        .into_iter()
        .filter(|(_, _, found)| folder.is_none_or(|folder| found == folder))
        .collect();
    if found.is_empty() {
        return None;
    }
    for (start, end, _) in found.into_iter().rev() {
        let (mut start, mut end) = (start, end);
        if start > 0
            && lines[start - 1].trim().is_empty()
            && (end >= lines.len() || lines[end].trim().is_empty())
        {
            start -= 1;
        } else if end < lines.len() && lines[end].trim().is_empty() && start > 0 {
            // A block put before a final `exit` brought the blank line
            // after it.
            end += 1;
        }
        lines.drain(start..end);
    }
    Some(lines.join("\n"))
}

/// Whether a hook holds nothing but its `#!` line.
pub fn empty(text: &str) -> bool {
    text.lines()
        .enumerate()
        .all(|(i, line)| line.trim().is_empty() || (i == 0 && line.starts_with("#!")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> String {
        block(
            Path::new("/work/it's here/app"),
            Path::new("/work/it's here"),
            Path::new("/opt/homebrew/bin/graphicle"),
            Some(Path::new("/home/a/Library/Application Support/Graphicle")),
        )
        .unwrap()
    }

    #[test]
    fn the_block_quotes_every_path_and_never_blocks_git() {
        let block = sample();
        assert!(
            block.starts_with("# graphicle-sync-start /work/it's here/app\n"),
            "{block}"
        );
        assert!(block.ends_with(END));
        assert!(
            block.contains("[ \"$t\" = '/work/it'\\''s here' ] || exit 0"),
            "{block}"
        );
        assert!(
            block.contains(
                "\"$graphicle\" --data-dir '/home/a/Library/Application Support/Graphicle' sync '/work/it'\\''s here/app' --quiet </dev/null >/dev/null 2>&1 &"
            ),
            "{block}"
        );
        assert!(
            block
                .lines()
                .all(|line| !line.contains("graphicle-hook-start")),
            "never the engine's own marker"
        );
        assert!(super::block(Path::new("/a\nb"), Path::new("/a"), Path::new("/g"), None).is_err());
        assert_eq!(
            manual_line(Path::new("/w/app"), None),
            "graphicle sync '/w/app' --quiet >/dev/null 2>&1 &"
        );
    }

    #[test]
    fn hooks_keep_their_lines_and_get_the_block_once() {
        let folder = Path::new("/work/it's here/app");
        let block = sample();
        // A new hook.
        let Upserted::Changed(created, "added") = upsert(None, &block, folder) else {
            panic!()
        };
        assert!(created.starts_with("#!/bin/sh\n# graphicle-sync-start"));
        assert_eq!(upsert(Some(&created), &block, folder), Upserted::Unchanged);
        assert!(empty(&remove(&created, Some(folder)).unwrap()));
        // An existing hook keeps its lines; the block goes before its exit.
        let own = "#!/usr/bin/env bash\nset -e\nnpm run lint\nexit 0\n";
        let Upserted::Changed(text, "added") = upsert(Some(own), &block, folder) else {
            panic!()
        };
        assert!(
            text.starts_with("#!/usr/bin/env bash\nset -e\nnpm run lint\n# graphicle-sync-start")
        );
        assert!(text.ends_with(&format!("{END}\n\nexit 0\n")), "{text}");
        assert_eq!(folders(&text), vec![folder.to_path_buf()]);
        assert_eq!(remove(&text, Some(folder)).unwrap(), own);
        // A hook without an exit gets the block at its end.
        let plain = "#!/bin/sh\necho committed\n";
        let Upserted::Changed(text, _) = upsert(Some(plain), &block, folder) else {
            panic!()
        };
        assert!(text.starts_with(plain) && text.ends_with(&format!("{END}\n")));
        assert_eq!(remove(&text, None).unwrap(), plain);
        assert!(remove(plain, None).is_none());
        // Another folder's block stays when this folder's is removed.
        let other = block.replace("/work/it's here/app", "/work/it's here/api");
        let Upserted::Changed(both, _) =
            upsert(Some(&text), &other, Path::new("/work/it's here/api"))
        else {
            panic!()
        };
        let left = remove(&both, Some(folder)).unwrap();
        assert_eq!(folders(&left), vec![PathBuf::from("/work/it's here/api")]);
        // A hook in another language is left alone.
        assert_eq!(
            upsert(Some("#!/usr/bin/env python3\nprint(1)\n"), &block, folder),
            Upserted::NotShell
        );
    }
}
