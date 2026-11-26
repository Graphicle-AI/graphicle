//! The data folder when GRAPHICLE_DATA_DIR or XDG_DATA_HOME is set but empty:
//! an empty value counts as unset, as it does for the Python SDK's
//! `default_data_dir` and the other folders the program reads from the
//! environment, so `graphicle init` writes to the platform's data folder.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tempfile::TempDir;

fn platform_default(home: &Path) -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        home.join("Library/Application Support/Graphicle")
    }
    #[cfg(windows)]
    {
        home.join("AppData/Local/Graphicle")
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        home.join(".local/share/graphicle")
    }
}

fn init(graphicle_data_dir: Option<&str>, empty_xdg_data_home: bool) {
    let home = TempDir::new().unwrap();
    let current = TempDir::new().unwrap();
    let expected = platform_default(home.path());
    let mut command = Command::new(env!("CARGO_BIN_EXE_graphicle"));
    crate::child_env::home(&mut command, home.path());
    command
        .current_dir(current.path())
        .env_remove("GRAPHICLE_DATA_DIR")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .arg("init");
    if let Some(value) = graphicle_data_dir {
        command.env("GRAPHICLE_DATA_DIR", value);
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    if empty_xdg_data_home {
        command.env("XDG_DATA_HOME", "");
    } else {
        command.env("XDG_DATA_HOME", home.path().join(".local/share"));
    }
    #[cfg(any(not(unix), target_os = "macos"))]
    let _ = empty_xdg_data_home;

    let output = crate::fork_guard::spawn(&mut command)
        .unwrap()
        .wait_with_output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(expected.is_dir(), "{} is missing", expected.display());
    let session = expected.join("local-session.json");
    assert!(session.is_file(), "{} is missing", session.display());
    assert!(!current.path().join("local-session.json").exists());
    let reply: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let printed = Path::new(reply["data_directory"].as_str().unwrap());
    assert_eq!(
        printed.canonicalize().unwrap(),
        expected.canonicalize().unwrap()
    );
}

/// An empty GRAPHICLE_DATA_DIR selects the platform default, not the path "".
#[test]
fn empty_graphicle_data_dir_is_unset() {
    init(Some(""), false);
}

/// An empty XDG_DATA_HOME selects HOME/.local/share, not `graphicle` in the current folder.
#[cfg(all(unix, not(target_os = "macos")))]
#[test]
fn empty_xdg_data_home_is_unset() {
    init(None, true);
}
