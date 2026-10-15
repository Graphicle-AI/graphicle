//! The environment the command-line suites (easy_setup, easy_flow,
//! easy_review, ux_terminal, freshness, ask, graph_command) give each child:
//! a fixture home in place of the user's profile, on every system.
//!
//! On Linux and macOS this is exactly what the suites set by hand before:
//! HOME, and with [`isolate`] a cleared environment whose PATH is the
//! fixture's program folders followed by the given system folders. On Windows
//! the profile folders (USERPROFILE, APPDATA, LOCALAPPDATA) point into the
//! fixture home too, and a cleared environment keeps what Windows programs
//! need to start (SystemRoot, ComSpec, PATHEXT, TEMP, ...) and the folders
//! that hold Git and the system's own programs.

use std::path::{Path, PathBuf};
use std::process::Command;

/// The program file name on this system (`graphicle.exe` on Windows).
pub fn program(name: &str) -> String {
    format!("{name}{}", std::env::consts::EXE_SUFFIX)
}

/// Points the child's home at `home`: HOME everywhere, and on Windows the
/// profile folders that `std::env::home_dir` and the data folders read.
pub fn home<'a>(command: &'a mut Command, home: &Path) -> &'a mut Command {
    command.env("HOME", home);
    #[cfg(windows)]
    command
        .env("USERPROFILE", home)
        .env("APPDATA", home.join("AppData").join("Roaming"))
        .env("LOCALAPPDATA", home.join("AppData").join("Local"));
    command
}

/// Clears the child's environment, then sets [`home`] and a PATH of
/// `programs` followed by the system's program folders: `unix_system` (as
/// PATH spells it, `/usr/bin:/bin`) on Linux and macOS.
pub fn isolate<'a>(
    command: &'a mut Command,
    home_folder: &Path,
    programs: &[&Path],
    unix_system: &str,
) -> &'a mut Command {
    command.env_clear();
    home(command, home_folder);
    let mut path: Vec<PathBuf> = programs.iter().map(|p| p.to_path_buf()).collect();
    #[cfg(unix)]
    path.extend(std::env::split_paths(unix_system));
    #[cfg(windows)]
    {
        let _ = unix_system;
        windows::keep_system(command, &mut path);
    }
    command.env(
        "PATH",
        std::env::join_paths(path).expect("program folders form a PATH"),
    );
    command
}

#[cfg(windows)]
mod windows {
    use std::path::{Path, PathBuf};
    use std::process::Command;

    /// Variables Windows programs and Git for Windows need to start.
    const KEEP: &[&str] = &[
        "SystemRoot",
        "SystemDrive",
        "windir",
        "ComSpec",
        "PATHEXT",
        "ProgramData",
        "ProgramFiles",
        "ProgramFiles(x86)",
        "ProgramW6432",
        "CommonProgramFiles",
        "NUMBER_OF_PROCESSORS",
        "PROCESSOR_ARCHITECTURE",
        "OS",
    ];

    /// Restores [`KEEP`], TEMP and TMP, and appends to `path` the system's
    /// own folders and the folders of this PATH that hold `git.exe`, the
    /// Windows counterpart of `/usr/bin:/bin`. Client programs elsewhere on
    /// this PATH (npm's `claude.cmd`, `codex.cmd`) stay out of reach.
    pub fn keep_system(command: &mut Command, path: &mut Vec<PathBuf>) {
        for name in KEEP {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        let temp = std::env::temp_dir();
        command.env("TEMP", &temp).env("TMP", &temp);
        path.extend(system_folders(
            std::env::var_os("SystemRoot").as_deref().map(Path::new),
            std::env::var_os("PATH").as_deref(),
        ));
    }

    /// `%SystemRoot%\System32` and `%SystemRoot%`, then each folder of
    /// `inherited` holding `git.exe`, once each.
    pub fn system_folders(
        system_root: Option<&Path>,
        inherited: Option<&std::ffi::OsStr>,
    ) -> Vec<PathBuf> {
        let mut folders = Vec::new();
        if let Some(root) = system_root {
            folders.push(root.join("System32"));
            folders.push(root.to_path_buf());
        }
        for folder in inherited.map(std::env::split_paths).into_iter().flatten() {
            if folder.join("git.exe").is_file() && !folders.contains(&folder) {
                folders.push(folder);
            }
        }
        folders
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of(command: &Command) -> Vec<(String, Option<String>)> {
        command
            .get_envs()
            .map(|(name, value)| {
                (
                    name.to_string_lossy().into_owned(),
                    value.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect()
    }

    /// On Linux and macOS the environment is the one the suites built by
    /// hand: HOME and `<programs>:/usr/bin:/bin`, nothing else.
    #[cfg(unix)]
    #[test]
    fn unix_children_get_home_and_the_given_path_only() {
        let mut command = Command::new("true");
        isolate(
            &mut command,
            Path::new("/fixture/home"),
            &[Path::new("/fixture/bin")],
            "/usr/bin:/bin",
        );
        let mut env = env_of(&command);
        env.sort();
        assert_eq!(
            env,
            vec![
                ("HOME".into(), Some("/fixture/home".into())),
                ("PATH".into(), Some("/fixture/bin:/usr/bin:/bin".into())),
            ]
        );
        let mut command = Command::new("true");
        isolate(
            &mut command,
            Path::new("/h"),
            &[],
            "/usr/bin:/bin:/usr/sbin:/sbin",
        );
        assert!(
            env_of(&command)
                .contains(&("PATH".into(), Some("/usr/bin:/bin:/usr/sbin:/sbin".into())))
        );
        assert_eq!(program("graphicle"), "graphicle");
    }

    /// On Windows the profile folders point into the fixture home, the
    /// system variables survive the clear, and PATH starts with the
    /// fixture's programs.
    #[cfg(windows)]
    #[test]
    fn windows_children_get_the_fixture_profile_and_the_system() {
        let home_folder = Path::new(r"C:\fixture\home");
        let mut command = Command::new("cmd");
        isolate(
            &mut command,
            home_folder,
            &[Path::new(r"C:\fixture\bin")],
            "/usr/bin:/bin",
        );
        let env = env_of(&command);
        let get = |name: &str| {
            env.iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(name))
                .and_then(|(_, value)| value.clone())
        };
        assert_eq!(get("HOME").as_deref(), Some(r"C:\fixture\home"));
        assert_eq!(get("USERPROFILE").as_deref(), Some(r"C:\fixture\home"));
        assert_eq!(
            get("APPDATA").as_deref(),
            Some(r"C:\fixture\home\AppData\Roaming")
        );
        assert_eq!(
            get("LOCALAPPDATA").as_deref(),
            Some(r"C:\fixture\home\AppData\Local")
        );
        for name in ["SystemRoot", "ComSpec", "PATHEXT"] {
            assert_eq!(get(name), std::env::var(name).ok(), "{name}");
        }
        assert!(get("TEMP").is_some() && get("TMP").is_some());
        let path = get("PATH").unwrap();
        assert!(path.starts_with(r"C:\fixture\bin;"), "{path}");
        assert!(!path.contains("/usr/bin"), "{path}");
        assert_eq!(program("graphicle"), "graphicle.exe");
    }

    #[cfg(windows)]
    #[test]
    fn windows_system_folders_keep_git_and_drop_other_programs() {
        let dir = tempfile::TempDir::new().unwrap();
        let git = dir.path().join("git");
        let npm = dir.path().join("npm");
        std::fs::create_dir_all(&git).unwrap();
        std::fs::create_dir_all(&npm).unwrap();
        std::fs::write(git.join("git.exe"), b"").unwrap();
        std::fs::write(npm.join("claude.cmd"), b"").unwrap();
        let inherited = std::env::join_paths([&npm, &git, &git]).unwrap();
        let folders =
            windows::system_folders(Some(Path::new(r"C:\Windows")), Some(inherited.as_os_str()));
        assert_eq!(
            folders,
            vec![
                PathBuf::from(r"C:\Windows\System32"),
                PathBuf::from(r"C:\Windows"),
                git
            ]
        );
    }
}
