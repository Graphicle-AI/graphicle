// Adapted from Graphify 0.9.91 (10130e79), Apache-2.0; see NOTICE.md.
// Upstream: tests/test_pi_guard_install.py
//! `pi guard install|uninstall|path` (Graphify 0.9.89): the package is a
//! complete Pi extension, Pi owns its registration, and without Pi on PATH
//! nothing is written.
use serde_json::Value;
use std::{
    fs,
    path::Path,
    process::{Command, Output},
};

fn pi_guard(home: &Path, work: &Path, path: &str, action: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_graphicle-graph"))
        .args(["pi", "guard", action])
        .current_dir(work)
        .env_clear()
        .env("HOME", home)
        .env("PATH", path)
        .output()
        .unwrap()
}

#[test]
fn path_names_a_complete_package() {
    // test_pi_guard_path_is_a_complete_packaged_extension: every extension
    // the manifest lists is a file inside the package. Upstream's package is
    // the wheel's graphify/pi folder; a native build writes it under HOME.
    let dir = tempfile::tempdir().unwrap();
    let out = pi_guard(dir.path(), dir.path(), "", "path");
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let shown = String::from_utf8(out.stdout).unwrap();
    let package = Path::new(shown.trim_end());
    assert_eq!(package, dir.path().join(".graphicle-graph").join("pi"));
    let manifest: Value =
        serde_json::from_str(&fs::read_to_string(package.join("package.json")).unwrap()).unwrap();
    let entries = manifest["pi"]["extensions"].as_array().unwrap();
    assert!(!entries.is_empty(), "{manifest}");
    let root = package.canonicalize().unwrap();
    for entry in entries {
        let file = package.join(entry.as_str().unwrap());
        assert!(file.is_file(), "{}", file.display());
        assert!(file.canonicalize().unwrap().starts_with(&root));
    }
    // The extension hands each call to the running program's hook-guard.
    let index = fs::read_to_string(package.join("index.ts")).unwrap();
    assert!(index.contains("which(\"graphicle-graph\", dir)"), "{index}");
}

#[cfg(unix)]
#[test]
fn install_and_uninstall_hand_the_package_to_pi_and_keep_its_status() {
    // test_pi_guard_delegates_to_host_and_propagates_failure.
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let record = dir.path().join("argv");
    let pi = bin.join("pi");
    fs::write(
        &pi,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\nexit 23\n",
            record.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&pi, fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!("{}:/usr/bin:/bin", bin.display());
    for (action, verb) in [("install", "install"), ("uninstall", "remove")] {
        let out = pi_guard(dir.path(), dir.path(), &path, action);
        assert_eq!(
            out.status.code(),
            Some(23),
            "{action}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let argv = fs::read_to_string(&record).unwrap();
        let argv: Vec<&str> = argv.lines().collect();
        assert_eq!(argv.len(), 2, "{argv:?}");
        assert_eq!(argv[0], verb);
        assert_eq!(
            Path::new(argv[1]),
            dir.path().join(".graphicle-graph").join("pi")
        );
        assert!(Path::new(argv[1]).join("package.json").is_file());
    }
}

#[test]
fn without_pi_on_path_nothing_is_written() {
    // test_pi_guard_without_host_does_not_create_configuration.
    let dir = tempfile::tempdir().unwrap();
    let (home, work, empty) = (
        dir.path().join("home"),
        dir.path().join("work"),
        dir.path().join("empty"),
    );
    for folder in [&home, &work, &empty] {
        fs::create_dir(folder).unwrap();
    }
    for action in ["install", "uninstall"] {
        let out = pi_guard(&home, &work, empty.to_str().unwrap(), action);
        assert_eq!(out.status.code(), Some(1), "{action}");
        assert_eq!(
            String::from_utf8_lossy(&out.stderr),
            "error: install Pi (pi) and add it to PATH first\n"
        );
        assert_eq!(fs::read_dir(&work).unwrap().count(), 0, "{action}");
        assert_eq!(fs::read_dir(&home).unwrap().count(), 0, "{action}");
    }
}
