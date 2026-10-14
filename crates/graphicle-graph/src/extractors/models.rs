// Adapted from Graphify 0.9.88 (a9860f70), Apache-2.0; see NOTICE.md.
// Upstream: graphify/extractors/models.py
//! Native records and callback contracts from graphify/extractors/models.py.
use crate::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{Arc, LazyLock, Mutex},
};
use tree_sitter::Node;
pub static WORKSPACE_PACKAGE_CACHE: LazyLock<Mutex<BTreeMap<String, BTreeMap<String, PathBuf>>>> =
    LazyLock::new(|| Mutex::new(BTreeMap::new()));
pub const JS_CACHE_BYPASS_SUFFIXES: &[&str] = &[
    ".js", ".jsx", ".mjs", ".cjs", ".ts", ".tsx", ".mts", ".cts", ".vue", ".svelte", ".ejs",
];
pub type ImportHandler = Arc<
    dyn for<'t> Fn(
            Node<'t>,
            &[u8],
            &str,
            &str,
            &mut Vec<Value>,
            &str,
            Option<&[String]>,
            // Scan root for scan-root-aware resolution (Python, #3729).
            Option<&Path>,
        ) -> Option<Vec<(String, String)>>
        + Send
        + Sync,
>;
pub type NameResolver = Arc<dyn for<'t> Fn(Node<'t>, &[u8]) -> Option<String> + Send + Sync>;
pub type SymbolSanitizer = Arc<dyn Fn(&str) -> String + Send + Sync>;
/// The pinned engine never invokes extra_walk_fn. Retain a callable slot for
/// native extensions without claiming it is wired into generic traversal.
pub type ExtraWalk = Arc<dyn for<'t> Fn(Node<'t>, &[u8], &mut Value) -> Result<()> + Send + Sync>;
#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LanguageConfig {
    pub ts_module: String,
    pub ts_language_fn: String,
    pub class_types: BTreeSet<String>,
    pub function_types: BTreeSet<String>,
    pub import_types: BTreeSet<String>,
    pub call_types: BTreeSet<String>,
    pub static_prop_types: BTreeSet<String>,
    pub helper_fn_names: BTreeSet<String>,
    pub container_bind_methods: BTreeSet<String>,
    pub event_listener_properties: BTreeSet<String>,
    pub name_field: String,
    pub name_fallback_child_types: Vec<String>,
    pub body_field: String,
    pub body_fallback_child_types: Vec<String>,
    pub call_function_field: String,
    pub call_accessor_node_types: BTreeSet<String>,
    pub call_accessor_field: String,
    pub call_accessor_object_field: String,
    pub function_boundary_types: BTreeSet<String>,
    #[serde(skip)]
    pub import_handler: Option<ImportHandler>,
    #[serde(skip)]
    pub resolve_function_name_fn: Option<NameResolver>,
    #[serde(skip)]
    pub sanitize_symbol_name_fn: Option<SymbolSanitizer>,
    pub function_label_parens: bool,
    #[serde(skip)]
    pub extra_walk_fn: Option<ExtraWalk>,
}
impl Default for LanguageConfig {
    fn default() -> Self {
        Self {
            ts_module: String::new(),
            ts_language_fn: "language".into(),
            class_types: BTreeSet::new(),
            function_types: BTreeSet::new(),
            import_types: BTreeSet::new(),
            call_types: BTreeSet::new(),
            static_prop_types: BTreeSet::new(),
            helper_fn_names: BTreeSet::new(),
            container_bind_methods: BTreeSet::new(),
            event_listener_properties: BTreeSet::new(),
            name_field: "name".into(),
            name_fallback_child_types: vec![],
            body_field: "body".into(),
            body_fallback_child_types: vec![],
            call_function_field: "function".into(),
            call_accessor_node_types: BTreeSet::new(),
            call_accessor_field: "attribute".into(),
            call_accessor_object_field: String::new(),
            function_boundary_types: BTreeSet::new(),
            import_handler: None,
            resolve_function_name_fn: None,
            sanitize_symbol_name_fn: None,
            function_label_parens: true,
            extra_walk_fn: None,
        }
    }
}
impl LanguageConfig {
    pub fn new(module: impl Into<String>) -> Self {
        Self {
            ts_module: module.into(),
            ..Default::default()
        }
    }
}
macro_rules! record {($name:ident {$($field:ident:$ty:ty),* $(,)?})=>{
    #[derive(Clone,Debug,PartialEq,Eq,Hash,Serialize,Deserialize)]
    pub struct $name {$(pub(crate) $field:$ty),*}
    impl $name {pub fn new($($field:$ty),*)->Self{Self{$($field),*}}$(pub fn $field(&self)->&$ty{&self.$field})*}
};}
record!(SymbolDeclarationFact {
    file_path: PathBuf,
    name: String,
    line: i64
});
record!(SymbolImportFact {
    file_path: PathBuf,
    local_name: String,
    target_path: PathBuf,
    imported_name: String,
    line: i64
});
record!(SymbolAliasFact {
    file_path: PathBuf,
    alias: String,
    target_name: String,
    line: i64
});
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SymbolExportFact {
    pub(crate) file_path: PathBuf,
    pub(crate) exported_name: String,
    pub(crate) line: i64,
    #[serde(default)]
    pub(crate) local_name: Option<String>,
    #[serde(default)]
    pub(crate) target_path: Option<PathBuf>,
    #[serde(default)]
    pub(crate) target_name: Option<String>,
    #[serde(default)]
    pub(crate) type_only: bool,
}
impl SymbolExportFact {
    pub fn new(file_path: PathBuf, exported_name: String, line: i64) -> Self {
        Self {
            file_path,
            exported_name,
            line,
            local_name: None,
            target_path: None,
            target_name: None,
            type_only: false,
        }
    }
    pub fn file_path(&self) -> &PathBuf {
        &self.file_path
    }
    pub fn exported_name(&self) -> &str {
        &self.exported_name
    }
    pub fn line(&self) -> i64 {
        self.line
    }
    pub fn local_name(&self) -> Option<&str> {
        self.local_name.as_deref()
    }
    pub fn target_path(&self) -> Option<&std::path::Path> {
        self.target_path.as_deref()
    }
    pub fn target_name(&self) -> Option<&str> {
        self.target_name.as_deref()
    }
    pub fn type_only(&self) -> bool {
        self.type_only
    }
    pub fn reexport(
        file_path: PathBuf,
        exported_name: String,
        line: i64,
        local_name: Option<String>,
        target_path: Option<PathBuf>,
        target_name: Option<String>,
        type_only: bool,
    ) -> Self {
        Self {
            file_path,
            exported_name,
            line,
            local_name,
            target_path,
            target_name,
            type_only,
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct StarExportFact {
    pub(crate) file_path: PathBuf,
    pub(crate) target_path: PathBuf,
    pub(crate) line: i64,
    #[serde(default)]
    pub(crate) type_only: bool,
}
impl StarExportFact {
    pub fn new(file_path: PathBuf, target_path: PathBuf, line: i64, type_only: bool) -> Self {
        Self {
            file_path,
            target_path,
            line,
            type_only,
        }
    }
    pub fn file_path(&self) -> &PathBuf {
        &self.file_path
    }
    pub fn target_path(&self) -> &PathBuf {
        &self.target_path
    }
    pub fn line(&self) -> i64 {
        self.line
    }
    pub fn type_only(&self) -> bool {
        self.type_only
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NamespaceExportFact {
    pub(crate) file_path: PathBuf,
    pub(crate) exported_name: String,
    pub(crate) target_path: PathBuf,
    pub(crate) line: i64,
    #[serde(default)]
    pub(crate) type_only: bool,
}
impl NamespaceExportFact {
    pub fn new(
        file_path: PathBuf,
        exported_name: String,
        target_path: PathBuf,
        line: i64,
        type_only: bool,
    ) -> Self {
        Self {
            file_path,
            exported_name,
            target_path,
            line,
            type_only,
        }
    }
    pub fn file_path(&self) -> &PathBuf {
        &self.file_path
    }
    pub fn exported_name(&self) -> &str {
        &self.exported_name
    }
    pub fn target_path(&self) -> &PathBuf {
        &self.target_path
    }
    pub fn line(&self) -> i64 {
        self.line
    }
    pub fn type_only(&self) -> bool {
        self.type_only
    }
}
record!(SymbolUseFact {
    file_path: PathBuf,
    source_id: String,
    local_name: String,
    relation: String,
    context: String,
    line: i64
});
#[derive(Default, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SymbolResolutionFacts {
    pub declarations: Vec<SymbolDeclarationFact>,
    pub imports: Vec<SymbolImportFact>,
    pub aliases: Vec<SymbolAliasFact>,
    pub exports: Vec<SymbolExportFact>,
    pub star_exports: Vec<StarExportFact>,
    pub namespace_exports: Vec<NamespaceExportFact>,
    pub uses: Vec<SymbolUseFact>,
    pub module_imports: Vec<(PathBuf, PathBuf, i64, String)>,
}
fn roundtrip<T: for<'de> Deserialize<'de> + Serialize>(v: &Value) -> Result<Value> {
    let typed: T = serde_json::from_value(v.clone()).map_err(crate::Error::from)?;
    serde_json::to_value(typed).map_err(crate::Error::from)
}
pub fn dispatch(r: &Value) -> Result<Value> {
    let v = &r["value"];
    match r["name"].as_str().unwrap_or("") {
        "LanguageConfig" => {
            if !v["ts_module"].is_string() {
                return Err("LanguageConfig requires ts_module".into());
            }
            for k in [
                "import_handler",
                "resolve_function_name_fn",
                "sanitize_symbol_name_fn",
                "extra_walk_fn",
            ] {
                if !v[k].is_null() {
                    return Err(
                        format!("{k} must be configured using the native callback API").into(),
                    );
                }
            }
            let mut out = roundtrip::<LanguageConfig>(v)?;
            for k in [
                "import_handler",
                "resolve_function_name_fn",
                "sanitize_symbol_name_fn",
                "extra_walk_fn",
            ] {
                out[k] = Value::Null;
            }
            Ok(out)
        }
        "_SymbolDeclarationFact" => roundtrip::<SymbolDeclarationFact>(v),
        "_SymbolImportFact" => roundtrip::<SymbolImportFact>(v),
        "_SymbolAliasFact" => roundtrip::<SymbolAliasFact>(v),
        "_SymbolExportFact" => roundtrip::<SymbolExportFact>(v),
        "_StarExportFact" => roundtrip::<StarExportFact>(v),
        "_NamespaceExportFact" => roundtrip::<NamespaceExportFact>(v),
        "_SymbolUseFact" => roundtrip::<SymbolUseFact>(v),
        "_SymbolResolutionFacts" => roundtrip::<SymbolResolutionFacts>(v),
        "suffixes" => Ok(json!(JS_CACHE_BYPASS_SUFFIXES)),
        "workspace_cache" => {
            let mut cache = WORKSPACE_PACKAGE_CACHE.lock().map_err(crate::Error::from)?;
            if r["clear"].as_bool() == Some(true) {
                cache.clear()
            }
            if let Some(key) = r["key"].as_str()
                && let Some(value) = r.get("value")
            {
                cache.insert(
                    key.into(),
                    serde_json::from_value(value.clone()).map_err(crate::Error::from)?,
                );
            }
            Ok(serde_json::to_value(&*cache).map_err(crate::Error::from)?)
        }
        _ => Err("Unknown extraction model".into()),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn callbacks_receive_real_nodes_and_mutable_output() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_json::LANGUAGE.into())
            .unwrap();
        let source = b"{\"name\":1}";
        let tree = parser.parse(source, None).unwrap();
        let root = tree.root_node();
        let mut config = LanguageConfig::new("tree_sitter_json");
        config.import_handler = Some(Arc::new(|n, source, file, stem, edges, path, scope, _| {
            edges.push(json!({"source":file,"target":stem,"source_file":path,"scope_chain":scope,"start":n.start_byte(),"bytes":source.len()}));
            Some(vec![("imported".into(), "label".into())])
        }));
        config.resolve_function_name_fn = Some(Arc::new(|n, source| {
            Some(String::from_utf8_lossy(&source[n.byte_range()]).into_owned())
        }));
        config.sanitize_symbol_name_fn = Some(Arc::new(|s| s.trim_end_matches('!').into()));
        config.extra_walk_fn = Some(Arc::new(|n, _, state| {
            state["children"] = json!(n.named_child_count());
            Ok(())
        }));
        let mut edges = vec![];
        let scope = vec!["outer".into()];
        assert_eq!(
            (config.import_handler.as_ref().unwrap())(
                root,
                source,
                "file",
                "stem",
                &mut edges,
                "file.json",
                Some(&scope),
                None
            ),
            Some(vec![("imported".into(), "label".into())])
        );
        assert_eq!(edges[0]["scope_chain"], json!(scope));
        assert_eq!(
            (config.resolve_function_name_fn.as_ref().unwrap())(root, source),
            Some("{\"name\":1}".into())
        );
        assert_eq!(
            (config.sanitize_symbol_name_fn.as_ref().unwrap())("save!"),
            "save"
        );
        let mut state = json!({});
        (config.extra_walk_fn.as_ref().unwrap())(root, source, &mut state).unwrap();
        assert_eq!(state["children"], 1);
    }
}
