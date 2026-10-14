use std::{
    env, fs,
    path::{Path, PathBuf},
};

fn collect(path: &Path, files: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(path).expect("read producer source directory") {
        let path = entry.expect("producer source entry").path();
        if path.is_dir() {
            collect(&path, files);
        } else if path.extension().is_some_and(|s| s == "rs") {
            files.push(path);
        }
    }
}

fn main() {
    let root = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let src = root.join("src");
    println!("cargo:rerun-if-changed=src");
    let mut files = Vec::new();
    collect(&src.join("extractors"), &mut files);
    for entry in fs::read_dir(&src).unwrap() {
        let path = entry.unwrap().path();
        if path.is_file()
            && path.extension().is_some_and(|s| s == "rs")
            && path
                .file_stem()
                .unwrap()
                .to_string_lossy()
                .starts_with("extract")
        {
            files.push(path);
        }
    }
    files.sort();
    let mut code = String::from("const PRODUCER_SOURCES: &[(&str, &str)] = &[\n");
    for path in files {
        let name = path
            .strip_prefix(&root)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        code.push_str(&format!(
            "({name:?}, include_str!({:?})),\n",
            path.to_str().unwrap()
        ));
    }
    code.push_str("];\n");
    fs::write(
        PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("producer_sources.rs"),
        code,
    )
    .unwrap();
    // Query snapshots (src/snapshot.rs) are only valid for the source that
    // wrote them: hash every file under src/ with the package version.
    let mut all = Vec::new();
    collect_all(&src, &mut all);
    all.sort();
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut feed = |bytes: &[u8]| {
        for b in bytes {
            hash ^= u64::from(*b);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
    };
    feed(env::var("CARGO_PKG_VERSION").unwrap_or_default().as_bytes());
    for path in all {
        feed(
            path.strip_prefix(&root)
                .unwrap()
                .to_string_lossy()
                .as_bytes(),
        );
        feed(&[0]);
        feed(&fs::read(&path).expect("read source file"));
        feed(&[0]);
    }
    println!("cargo:rustc-env=GRAPHICLE_SNAPSHOT_CODE={hash:016x}");
}

fn collect_all(path: &Path, files: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(path).expect("read source directory") {
        let path = entry.expect("source entry").path();
        if path.is_dir() {
            collect_all(&path, files);
        } else {
            files.push(path);
        }
    }
}
