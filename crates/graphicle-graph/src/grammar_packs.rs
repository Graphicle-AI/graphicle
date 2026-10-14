//! Grammar packs: long-tail tree-sitter grammars as separate shared libraries.
//!
//! A default build links every grammar. A build without a `grammar-*`
//! feature loads that grammar at first use from a pack: a shared library
//! (`<pack>.dylib`, `.so` or `.dll`) exporting the grammar's
//! `tree_sitter_<name>` function, built from the same pinned grammar sources
//! by `tools/build_grammar_packs.py`. Packs are looked up in
//! `GRAPHICLE_GRAMMAR_DIR` (a path list), then `~/.graphicle-graph/grammars`, then a
//! `grammars` directory next to the executable. A pack is used only when the
//! grammar it provides has the pinned node-kind and parse-state counts, so a
//! pack from another grammar version is refused instead of changing output.
//!
//! Loading a shared library runs its code, so a pack is verified before it
//! is loaded: its SHA-256 must be one this binary was built to accept (the
//! `pack=sha256,...` list in `GRAPHICLE_PACK_DIGESTS` at build time, for
//! release builds that ship their packs), or, when the binary names none,
//! the digest recorded for it in the `manifest.json` that
//! `tools/build_grammar_packs.py` writes next to the packs. The manifest
//! check catches a corrupted or swapped pack, not a manifest rewritten with
//! it; only put packs you built or trust in these directories.
//! `GRAPHICLE_GRAMMAR_UNVERIFIED=1` skips the manifest check.
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{LazyLock, Mutex},
};
use tree_sitter::Language;

/// `pack=sha256` pairs accepted by this build (empty: use the manifest).
const BUILT_IN_DIGESTS: Option<&str> = option_env!("GRAPHICLE_PACK_DIGESTS");

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Checks the pack at `path` before it is loaded (see the module docs).
fn verify(pack: &str, path: &Path) -> Result<(), String> {
    let bytes = std::fs::read(path)
        .map_err(|e| format!("cannot read grammar pack {}: {e}", path.display()))?;
    let digest = hex(&Sha256::digest(&bytes));
    if let Some(list) = BUILT_IN_DIGESTS.filter(|l| !l.trim().is_empty()) {
        let accepted = list
            .split(',')
            .filter_map(|item| item.trim().split_once('='))
            .any(|(p, d)| p == pack && d.eq_ignore_ascii_case(&digest));
        return if accepted {
            Ok(())
        } else {
            Err(format!(
                "grammar pack {} (sha256 {digest}) is not one this build ships with",
                path.display()
            ))
        };
    }
    if std::env::var("GRAPHICLE_GRAMMAR_UNVERIFIED").is_ok_and(|v| v == "1") {
        return Ok(());
    }
    let manifest = path.with_file_name("manifest.json");
    let recorded = std::fs::read(&manifest)
        .ok()
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .and_then(|m| {
            let entry = m.get("packs")?.get(pack)?;
            let file = entry.get("file")?.as_str()?;
            let name = path.file_name()?.to_str()?;
            (file == name).then(|| entry.get("sha256")?.as_str().map(str::to_string))?
        });
    match recorded {
        Some(d) if d.eq_ignore_ascii_case(&digest) => Ok(()),
        Some(d) => Err(format!(
            "grammar pack {} has sha256 {digest}, but {} records {d}",
            path.display(),
            manifest.display()
        )),
        None => Err(format!(
            "grammar pack {} is not recorded in {} (written by tools/build_grammar_packs.py); \
             set GRAPHICLE_GRAMMAR_UNVERIFIED=1 to load it anyway",
            path.display(),
            manifest.display()
        )),
    }
}

/// (pack, symbol, node kinds, parse states) of every grammar a pack can
/// provide; counts are those of the pinned grammar crates.
pub const PACKS: &[(&str, &str, usize, usize)] = &[
    ("verilog", "tree_sitter_verilog", 1223, 20534),
    ("ocaml", "tree_sitter_ocaml", 470, 8356),
    ("ocaml", "tree_sitter_ocaml_interface", 469, 7337),
    ("fortran", "tree_sitter_fortran", 692, 9251),
    ("julia", "tree_sitter_julia", 245, 19618),
    ("objc", "tree_sitter_objc", 588, 6799),
    ("elixir", "tree_sitter_elixir", 235, 7001),
    ("powershell", "tree_sitter_powershell", 419, 2142),
    ("zig", "tree_sitter_zig", 274, 1852),
    ("dm", "tree_sitter_dm", 264, 1805),
    ("commonlisp", "tree_sitter_commonlisp", 182, 4165),
];

fn extension() -> &'static str {
    if cfg!(target_os = "windows") {
        "dll"
    } else if cfg!(target_os = "macos") {
        "dylib"
    } else {
        "so"
    }
}

/// Directories searched for packs, in order.
pub fn search_path() -> Vec<PathBuf> {
    let mut dirs = vec![];
    if let Some(v) = std::env::var_os("GRAPHICLE_GRAMMAR_DIR") {
        dirs.extend(std::env::split_paths(&v).filter(|p| !p.as_os_str().is_empty()));
    }
    if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
        dirs.push(
            PathBuf::from(home)
                .join(crate::paths::CONFIG_DIR)
                .join("grammars"),
        );
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        dirs.push(dir.join("grammars"));
    }
    dirs
}

static LOADED: LazyLock<Mutex<HashMap<&'static str, Result<Language, String>>>> =
    LazyLock::new(Mutex::default);

/// The grammar `symbol` from its pack, loaded once per process.
pub fn load(symbol: &'static str) -> Result<Language, String> {
    let mut loaded = LOADED.lock().unwrap_or_else(|e| e.into_inner());
    loaded
        .entry(symbol)
        .or_insert_with(|| load_uncached(symbol))
        .clone()
}

fn load_uncached(symbol: &str) -> Result<Language, String> {
    let (pack, _, kinds, states) = *PACKS
        .iter()
        .find(|p| p.1 == symbol)
        .ok_or_else(|| format!("{symbol} is not a grammar pack"))?;
    let file = format!("{pack}.{}", extension());
    let dirs = search_path();
    let path = dirs
        .iter()
        .map(|d| d.join(&file))
        .find(|p| p.is_file())
        .ok_or_else(|| {
            format!(
                "grammar pack `{file}` is not installed (this build does not link {pack}); \
                 build it with tools/build_grammar_packs.py and put it in one of: {}",
                dirs.iter()
                    .map(|d| d.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?;
    // The digest check comes first: loading runs the library's code.
    verify(pack, &path)?;
    // SAFETY: the library is a tree-sitter grammar built from the pinned
    // sources and verified above; it stays loaded for the life of the
    // process (leaked below) because every `Language` points into its
    // static data.
    let language = unsafe {
        let lib = libloading::Library::new(&path)
            .map_err(|e| format!("cannot load grammar pack {}: {e}", path.display()))?;
        let f: libloading::Symbol<unsafe extern "C" fn() -> *const ()> = lib
            .get(symbol.as_bytes())
            .map_err(|e| format!("{} does not export {symbol}: {e}", path.display()))?;
        let raw = *f;
        std::mem::forget(lib);
        Language::new(tree_sitter_language::LanguageFn::from_raw(raw))
    };
    if (language.node_kind_count(), language.parse_state_count()) != (kinds, states) {
        return Err(format!(
            "grammar pack {} is a different {pack} grammar ({} node kinds, {} parse states; \
             this build needs {kinds} and {states})",
            path.display(),
            language.node_kind_count(),
            language.parse_state_count()
        ));
    }
    Ok(language)
}

/// A grammar: linked in when its feature is enabled, else from its pack.
#[macro_export]
#[doc(hidden)]
macro_rules! grammar {
    ($feature:literal, $symbol:literal, $linked:expr) => {{
        #[cfg(feature = $feature)]
        let language: ::std::result::Result<::tree_sitter::Language, String> =
            Ok(::tree_sitter::Language::from($linked));
        #[cfg(not(feature = $feature))]
        let language = $crate::grammar_packs::load($symbol);
        language
    }};
}

/// `grammars`: where each long-tail grammar comes from in this build.
pub fn report() -> String {
    let mut lines = vec![];
    for (pack, symbol, _, _) in PACKS {
        let linked = linked(symbol);
        let state = if linked {
            "linked".to_string()
        } else {
            match load(symbol) {
                Ok(_) => "pack".into(),
                Err(e) => format!("missing: {e}"),
            }
        };
        lines.push(format!("{pack:<11} {symbol:<30} {state}"));
    }
    lines.push(format!(
        "pack search path: {}",
        search_path()
            .iter()
            .map(|d| d.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    ));
    lines.join("\n")
}

// The arms are cfg-dependent, so they are not a `matches!`.
#[allow(clippy::match_like_matches_macro)]
fn linked(symbol: &str) -> bool {
    match symbol {
        "tree_sitter_verilog" => cfg!(feature = "grammar-verilog"),
        "tree_sitter_ocaml" | "tree_sitter_ocaml_interface" => cfg!(feature = "grammar-ocaml"),
        "tree_sitter_fortran" => cfg!(feature = "grammar-fortran"),
        "tree_sitter_julia" => cfg!(feature = "grammar-julia"),
        "tree_sitter_objc" => cfg!(feature = "grammar-objc"),
        "tree_sitter_elixir" => cfg!(feature = "grammar-elixir"),
        "tree_sitter_powershell" => cfg!(feature = "grammar-powershell"),
        "tree_sitter_zig" => cfg!(feature = "grammar-zig"),
        "tree_sitter_dm" => cfg!(feature = "grammar-dm"),
        "tree_sitter_commonlisp" => cfg!(feature = "grammar-commonlisp"),
        _ => false,
    }
}

#[cfg(test)]
mod verify_tests {
    #[test]
    fn packs_must_match_their_manifest_digest() {
        let dir = tempfile::tempdir().unwrap();
        let pack = dir.path().join("zig.so");
        std::fs::write(&pack, b"not really a library").unwrap();
        assert!(
            super::verify("zig", &pack)
                .unwrap_err()
                .contains("not recorded")
        );
        let digest = super::hex(&<sha2::Sha256 as sha2::Digest>::digest(
            b"not really a library",
        ));
        std::fs::write(
            dir.path().join("manifest.json"),
            serde_json::json!({"packs": {"zig": {"file": "zig.so", "sha256": digest}}}).to_string(),
        )
        .unwrap();
        if super::BUILT_IN_DIGESTS.is_none_or(|l| l.trim().is_empty()) {
            super::verify("zig", &pack).unwrap();
            // A pack is checked against its own entry, not any entry.
            assert!(super::verify("dm", &pack).is_err());
        }
        std::fs::write(&pack, b"swapped").unwrap();
        assert!(super::verify("zig", &pack).is_err());
    }
}

#[cfg(all(test, feature = "grammars-extra"))]
mod tests {
    /// The pinned counts a pack must match are those of the linked grammars.
    #[test]
    fn pack_table_matches_the_linked_grammars() {
        let linked: [(&str, tree_sitter::Language); 11] = [
            ("tree_sitter_verilog", tree_sitter_verilog::LANGUAGE.into()),
            (
                "tree_sitter_ocaml",
                tree_sitter_ocaml::LANGUAGE_OCAML.into(),
            ),
            (
                "tree_sitter_ocaml_interface",
                tree_sitter_ocaml::LANGUAGE_OCAML_INTERFACE.into(),
            ),
            ("tree_sitter_fortran", tree_sitter_fortran::LANGUAGE.into()),
            ("tree_sitter_julia", tree_sitter_julia::LANGUAGE.into()),
            ("tree_sitter_objc", tree_sitter_objc::LANGUAGE.into()),
            ("tree_sitter_elixir", tree_sitter_elixir::LANGUAGE.into()),
            (
                "tree_sitter_powershell",
                tree_sitter_powershell::LANGUAGE.into(),
            ),
            ("tree_sitter_zig", tree_sitter_zig::LANGUAGE.into()),
            ("tree_sitter_dm", tree_sitter_dm::LANGUAGE.into()),
            (
                "tree_sitter_commonlisp",
                tree_sitter_commonlisp::LANGUAGE_COMMONLISP.into(),
            ),
        ];
        assert_eq!(linked.len(), super::PACKS.len());
        for ((symbol, language), (_, pack_symbol, kinds, states)) in linked.iter().zip(super::PACKS)
        {
            assert_eq!(symbol, pack_symbol);
            assert_eq!(
                (language.node_kind_count(), language.parse_state_count()),
                (*kinds, *states),
                "{symbol}"
            );
            assert!(super::linked(symbol));
        }
    }
}
