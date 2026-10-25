// Adapted from Graphify 0.9.95 (92741d16), Apache-2.0; see NOTICE.md.
// Upstream: graphify/resolver_registry.py
//! Port of graphify/resolver_registry.py: ordered, suffix-gated mutable passes.
//!
//! Assembly hands the late passes its edge list in the compact record form
//! ([`Attributes`]: eight bytes per field, values shared through the value
//! table) instead of one serde map per edge. A resolver that reads records
//! natively ([`register_native`]) gets them as they are; one written against
//! [`Rec`] and [`PerFile`](crate::extract::per_file::PerFile)
//! ([`register_any_form`], [`attach_any_form`]) runs on both lists in whatever
//! form they are in;
//! any other one sees the JSON objects it always saw, converted losslessly
//! for its run ([`EdgeList`], [`PerList`]), so its result is unchanged.
//!
//! With `GRAPHICLE_PROFILE=1` a whole-list conversion shows as its own span,
//! `assemble.resolver.<name>.to_json` or `.to_records`.
use crate::{
    Result,
    extract::per_file::PerList,
    record::{Attributes, Field},
    vtab::{self, VId},
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::{
    collections::HashSet,
    path::PathBuf,
    sync::{Arc, LazyLock, Mutex, RwLock},
};

pub type Resolve =
    dyn Fn(&mut Vec<Value>, &mut Vec<Value>, &mut Vec<Value>) -> Result<()> + Send + Sync;
/// A resolver pass over compact edge records, with the per-file results in
/// whichever form assembly holds them (nodes stay JSON).
pub type ResolveRecords =
    dyn Fn(&mut PerList, &mut Vec<Value>, &mut Vec<Attributes>) -> Result<()> + Send + Sync;
/// A resolver pass over the edge list and the per-file results in whichever
/// forms assembly holds them: written against [`Rec`] and `PerFile`, it needs
/// neither list converted ([`edge_dispatch`], `per_dispatch`).
pub type ResolveAny =
    dyn Fn(&mut PerList, &mut Vec<Value>, &mut EdgeList) -> Result<()> + Send + Sync;

/// Run `$body` with `$e` bound to the list's edges in their current form
/// (`&mut Vec<Value>` or `&mut Vec<Attributes>`), for a pass generic over
/// [`Rec`].
macro_rules! edge_dispatch {
    ($edges:expr, $e:ident => $body:expr) => {
        match $edges {
            $crate::resolver_registry::EdgeList::Values($e) => $body,
            $crate::resolver_registry::EdgeList::Records($e) => $body,
        }
    };
}
pub(crate) use edge_dispatch;

/// The [`ResolveAny`] form of a pass written generically over
/// [`PerFile`](crate::extract::per_file::PerFile) and [`Rec`]:
/// `any_form!(|per, nodes, edges| body)` runs `body` with `per` bound to the
/// per-file results as a slice and `edges` to the edge vector, each in the
/// form assembly holds it, and `nodes` to the node list.
macro_rules! any_form {
    (|$p:ident, $n:ident, $e:ident| $body:expr) => {{
        let run: std::sync::Arc<$crate::resolver_registry::ResolveAny> = std::sync::Arc::new(
            |per: &mut $crate::extract::per_file::PerList,
             $n: &mut Vec<serde_json::Value>,
             edges: &mut $crate::resolver_registry::EdgeList| {
                $crate::extract::per_file::per_dispatch!(per, $p => {
                    let $p = &$p[..];
                    $crate::resolver_registry::edge_dispatch!(edges, $e => $body)
                })
            },
        );
        run
    }};
}
pub(crate) use any_form;

/// One edge (or node) record of assembly, either a JSON value or its compact
/// [`Attributes`] form. The methods mean exactly what the JSON operations the
/// passes used mean, so a pass written against this trait gives the same
/// result, key order and number texts included, on either form.
pub trait Rec: Clone + Send + Sync + 'static {
    /// `record.get(key)`: `None` when absent or when the record is no object.
    fn rec_get(&self, key: &str) -> Option<&Value>;
    /// `record[key] = value`: replaced in place when present, else appended.
    fn rec_set(&mut self, key: &str, value: Value);
    /// `record[key] = json!(text)`, without building the string when the
    /// value table already holds it.
    fn rec_set_str(&mut self, key: &str, text: &str);
    /// `record.shift_remove(key)` on an object record.
    fn rec_shift_remove(&mut self, key: &str) -> Option<Value>;
    /// The record of a JSON object.
    fn from_object(map: Map<String, Value>) -> Self;
    /// The record as a JSON value (a copy).
    fn to_value(&self) -> Value;
    /// A list of records as JSON values: borrowed when they already are.
    fn json_slice(items: &[Self]) -> std::borrow::Cow<'_, [Value]> {
        let workers = crate::extractors::parse_cache::workers();
        std::borrow::Cow::Owned(crate::par::map(items, workers, Self::to_value))
    }
    /// Make room for exactly one more key when `key` is absent (a JSON map
    /// would otherwise double its storage; a record grows one slot anyway).
    fn rec_reserve_key(&mut self, _key: &str) {}
    /// `record == value` as serde compares JSON: an object equals another
    /// with the same keys and equal values, in any order.
    fn rec_eq_value(&self, value: &Value) -> bool;
}
impl Rec for Value {
    // The passes' own helpers: a scan of the few keys of an edge, which is
    // quicker than hashing the key.
    fn rec_get(&self, key: &str) -> Option<&Value> {
        crate::extractors::resolution::field(self, key)
    }
    fn rec_set(&mut self, key: &str, value: Value) {
        match crate::extractors::resolution::field_mut(self, key) {
            Some(slot) => *slot = value,
            None => self[key] = value,
        }
    }
    fn rec_set_str(&mut self, key: &str, text: &str) {
        self.rec_set(key, Value::String(text.to_owned()));
    }
    fn rec_shift_remove(&mut self, key: &str) -> Option<Value> {
        self.as_object_mut()?.shift_remove(key)
    }
    fn from_object(map: Map<String, Value>) -> Self {
        Value::Object(map)
    }
    fn to_value(&self) -> Value {
        self.clone()
    }
    fn json_slice(items: &[Self]) -> std::borrow::Cow<'_, [Value]> {
        std::borrow::Cow::Borrowed(items)
    }
    fn rec_reserve_key(&mut self, key: &str) {
        if let Value::Object(map) = self
            && !map.contains_key(key)
        {
            let mut grown = Map::with_capacity(map.len() + 1);
            grown.extend(std::mem::take(map));
            *map = grown;
        }
    }
    fn rec_eq_value(&self, value: &Value) -> bool {
        self == value
    }
}
impl Rec for Attributes {
    fn rec_get(&self, key: &str) -> Option<&Value> {
        self.get(key)
    }
    fn rec_set(&mut self, key: &str, value: Value) {
        self.insert_field(Field::borrowed(key), value);
    }
    fn rec_set_str(&mut self, key: &str, text: &str) {
        self.set_id(key, vtab::intern_str(text));
    }
    fn rec_shift_remove(&mut self, key: &str) -> Option<Value> {
        self.shift_remove(key)
    }
    fn from_object(map: Map<String, Value>) -> Self {
        Attributes::from(map)
    }
    fn to_value(&self) -> Value {
        Value::Object(self.to_map())
    }
    fn rec_eq_value(&self, value: &Value) -> bool {
        value.as_object().is_some_and(|map| {
            map.len() == self.len() && self.iter().all(|(k, v)| map.get(k) == Some(v))
        })
    }
}
/// The string at `key`, or `""` (the `s` helper of the resolution passes).
pub fn rs<'a, R: Rec>(record: &'a R, key: &str) -> &'a str {
    record.rec_get(key).and_then(Value::as_str).unwrap_or("")
}
static NULL: Value = Value::Null;
/// `record[key]`: the value at `key`, `Null` when absent or when the record
/// is no object (what indexing a JSON value gives).
pub fn at<'a, R: Rec>(record: &'a R, key: &str) -> &'a Value {
    record.rec_get(key).unwrap_or(&NULL)
}
/// Append the edge literal `edge` (an object) to `edges` in their form.
pub fn push_edge<E: Rec>(edges: &mut Vec<E>, edge: Value) {
    edges.push(edge_record(edge));
}
/// The edge literal `edge` (an object) in the form `E`.
pub fn edge_record<E: Rec>(edge: Value) -> E {
    match edge {
        Value::Object(map) => E::from_object(map),
        _ => unreachable!("an edge literal is an object"),
    }
}
/// Append `added` to `edges`, growing the list by exactly that many: a pass
/// that adds a handful of edges does not double a list of millions.
pub fn append_edges<E>(edges: &mut Vec<E>, added: Vec<E>) {
    edges.reserve_exact(added.len());
    edges.extend(added);
}
/// The table id of an interned string, for records that store the same text
/// many times (repointed import targets).
pub fn string_id(text: &str) -> VId {
    vtab::intern_str(text)
}

/// Assembly's edge list in whichever form the current pass reads. Converting
/// is a lossless move: a record keeps its stored key order and the exact
/// text of every number, and a JSON object converts back to equal bytes.
pub enum EdgeList {
    Values(Vec<Value>),
    Records(Vec<Attributes>),
}
impl EdgeList {
    pub fn len(&self) -> usize {
        match self {
            Self::Values(v) => v.len(),
            Self::Records(r) => r.len(),
        }
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// The edges as JSON values, converting records in place.
    pub fn values(&mut self) -> &mut Vec<Value> {
        if let Self::Records(records) = self {
            let records = std::mem::take(records);
            *self = Self::Values(convert_all(records, |r| Value::Object(Map::from(r))));
        }
        match self {
            Self::Values(values) => values,
            Self::Records(_) => unreachable!("converted above"),
        }
    }
    /// The edges as compact records, converting JSON objects in place; `None`
    /// (and the list unchanged) when some edge is not an object, which only
    /// the JSON form can hold.
    pub fn records(&mut self) -> Option<&mut Vec<Attributes>> {
        if let Self::Values(values) = self {
            if json_only() || !values.iter().all(Value::is_object) {
                return None;
            }
            let values = std::mem::take(values);
            *self = Self::Records(convert_all(values, |v| match v {
                Value::Object(map) => Attributes::from(map),
                _ => unreachable!("checked above"),
            }));
        }
        match self {
            Self::Records(records) => Some(records),
            Self::Values(_) => None,
        }
    }
    /// The edges as JSON values, converted on this thread: at the end of
    /// assembly the pages of the per-file records it just freed are this
    /// thread's, and the rebuilt maps reuse them (built on the workers they
    /// would take new pages).
    pub fn into_values(self) -> Vec<Value> {
        match self {
            Self::Values(values) => values,
            Self::Records(records) => {
                let mut values = Vec::with_capacity(records.len());
                values.extend(records.into_iter().map(|r| Value::Object(Map::from(r))));
                values
            }
        }
    }
}

#[cfg(test)]
thread_local! {
    /// Tests: keep this thread's edge lists in their JSON form, so the record
    /// path can be compared with the JSON one.
    pub(crate) static JSON_ONLY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}
pub(crate) fn json_only() -> bool {
    #[cfg(test)]
    {
        JSON_ONLY.with(std::cell::Cell::get)
    }
    #[cfg(not(test))]
    {
        false
    }
}

/// `items` converted one by one, in order, on the run's workers: every item
/// converts on its own and the value table is shared, so only the time
/// changes with the split.
fn convert_all<T: Default + Send, U: Send>(mut items: Vec<T>, f: impl Fn(T) -> U + Sync) -> Vec<U> {
    let workers = crate::extractors::parse_cache::workers();
    if workers <= 1 || items.len() < 4096 {
        let mut out = Vec::with_capacity(items.len());
        out.extend(items.into_iter().map(f));
        return out;
    }
    let chunk = items.len().div_ceil(workers);
    let parts = std::thread::scope(|scope| {
        let handles = items
            .chunks_mut(chunk)
            .map(|part| {
                let f = &f;
                scope.spawn(move || {
                    let mut out = Vec::with_capacity(part.len());
                    out.extend(part.iter_mut().map(|item| f(std::mem::take(item))));
                    out
                })
            })
            .collect::<Vec<_>>();
        handles
            .into_iter()
            .map(|h| h.join().unwrap_or_else(|e| std::panic::resume_unwind(e)))
            .collect::<Vec<Vec<U>>>()
    });
    drop(items);
    let mut out = Vec::with_capacity(parts.iter().map(Vec::len).sum());
    for part in parts {
        out.extend(part);
    }
    out
}

/// The native form of a registered resolver.
#[derive(Clone)]
enum Native {
    /// Runs on compact edge records; the edge list converts to them first
    /// when it is JSON (the per-file results stay as they are).
    Records(Arc<ResolveRecords>),
    /// Runs on both lists in their current forms; nothing converts.
    Any(Arc<ResolveAny>),
}
/// Native implementations of registered resolvers, keyed by the address of
/// the registered JSON closure: the registry entry itself is unchanged, and a
/// resolver registered under the same name by anyone else is never mistaken
/// for it.
type NativeTable = Vec<(usize, Native)>;
static NATIVE: LazyLock<RwLock<NativeTable>> = LazyLock::new(|| RwLock::new(Vec::new()));
fn closure_address(resolve: &Arc<Resolve>) -> usize {
    Arc::as_ptr(resolve).cast::<()>() as usize
}
fn register_with(resolver: LanguageResolver, native: Native) -> LanguageResolver {
    NATIVE
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .push((closure_address(&resolver.resolve), native));
    register(resolver)
}
/// [`register`] a resolver together with a record-native form of the same
/// pass, which must give the same result as `resolver.resolve` on the JSON
/// form of the records.
pub fn register_native(
    resolver: LanguageResolver,
    records: Arc<ResolveRecords>,
) -> LanguageResolver {
    register_with(resolver, Native::Records(records))
}
/// [`register`] a resolver together with a form of the same pass that runs
/// on the edges and the per-file results in either form ([`ResolveAny`]); on
/// the JSON forms it must give what `resolver.resolve` gives, and on the
/// compact forms the same result.
pub fn register_any_form(resolver: LanguageResolver, any: Arc<ResolveAny>) -> LanguageResolver {
    register_with(resolver, Native::Any(any))
}
/// Give the pass registered last under `name` a form over either list form
/// (`any`, usually built with `any_form!`), as [`register_any_form`] does at
/// registration. The registry entry itself is unchanged.
///
/// # Panics
/// When no pass of that name is registered.
pub fn attach_any_form(name: &str, any: Arc<ResolveAny>) {
    let address = REGISTRY
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .rev()
        .find(|r| r.name == name)
        .map(|r| closure_address(&r.resolve))
        .unwrap_or_else(|| panic!("no resolver {name} is registered"));
    NATIVE
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .push((address, Native::Any(any)));
}
fn native(resolver: &LanguageResolver) -> Option<Native> {
    let address = closure_address(&resolver.resolve);
    NATIVE
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .find(|(a, _)| *a == address)
        .map(|(_, f)| f.clone())
}
#[derive(Clone)]
pub struct LanguageResolver {
    pub name: String,
    pub suffixes: HashSet<String>,
    pub resolve: Arc<Resolve>,
}
static REGISTRY: LazyLock<RwLock<Vec<LanguageResolver>>> =
    LazyLock::new(|| RwLock::new(Vec::new()));

pub fn register(resolver: LanguageResolver) -> LanguageResolver {
    REGISTRY
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .push(resolver.clone());
    resolver
}
pub fn registered_resolvers() -> Vec<LanguageResolver> {
    REGISTRY.read().unwrap_or_else(|e| e.into_inner()).clone()
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResolverWarning {
    pub name: String,
    pub error: String,
    pub message: String,
}

/// Returned warnings supplement the original log-and-continue behavior.
pub fn run_language_resolvers(
    paths: &[PathBuf],
    per_file: &mut Vec<Value>,
    all_nodes: &mut Vec<Value>,
    all_edges: &mut Vec<Value>,
    resolvers: Option<&[LanguageResolver]>,
) -> Vec<ResolverWarning> {
    let mut edges = EdgeList::Values(std::mem::take(all_edges));
    let mut per = PerList::Values(std::mem::take(per_file));
    let warnings = run_language_resolvers_on(paths, &mut per, all_nodes, &mut edges, resolvers);
    *per_file = per.into_values();
    *all_edges = edges.into_values();
    warnings
}

/// [`run_language_resolvers`] over an [`EdgeList`] and a [`PerList`]: a
/// resolver registered with [`register_any_form`] runs on both lists as they
/// are, a record-native one on the edge records, any other on the JSON forms
/// (a list converts only when the form a resolver reads changes).
pub fn run_language_resolvers_on(
    paths: &[PathBuf],
    per_file: &mut PerList,
    all_nodes: &mut Vec<Value>,
    all_edges: &mut EdgeList,
    resolvers: Option<&[LanguageResolver]>,
) -> Vec<ResolverWarning> {
    // Project-level remaps wake the resolvers of the declared language
    // (#2961): a `.inc` declared PHP runs the PHP passes, not Pascal's.
    let suffixes: HashSet<String> = paths.iter().map(crate::rcfile::effective_suffix).collect();
    let mut warnings = Vec::new();
    let mut i = 0;
    loop {
        // Release the registry lock before the callback, which may register a
        // subsequent pass. This preserves the Python list's append semantics.
        let resolver = if let Some(resolvers) = resolvers {
            resolvers.get(i).cloned()
        } else {
            REGISTRY
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .get(i)
                .cloned()
        };
        let Some(resolver) = resolver else {
            break;
        };
        i += 1;
        if resolver.suffixes.is_disjoint(&suffixes) {
            continue;
        }
        let started = timing().then(std::time::Instant::now);
        let profiled = crate::profile::span_with(|| format!("assemble.resolver.{}", resolver.name));
        // Tests that compare the two paths keep everything JSON
        // (`json_only`): there every pass runs its registered JSON closure.
        let outcome = match native(&resolver).filter(|_| !json_only()) {
            Some(Native::Any(run)) => run(per_file, all_nodes, all_edges),
            Some(Native::Records(run)) => {
                let span = conversion_span(
                    matches!(all_edges, EdgeList::Values(_)),
                    &resolver.name,
                    "to_records",
                );
                let edges = all_edges.records();
                drop(span);
                match edges {
                    Some(edges) => run(per_file, all_nodes, edges),
                    None => json_pass(&resolver, per_file, all_nodes, all_edges),
                }
            }
            None => json_pass(&resolver, per_file, all_nodes, all_edges),
        };
        if let Err(error) = outcome {
            let message = format!("{} resolution failed, skipping: {}", resolver.name, error);
            log::warn!("{message}");
            warnings.push(ResolverWarning {
                name: resolver.name.clone(),
                error: error.to_string(),
                message,
            });
        }
        if let Some(started) = started {
            eprintln!(
                "[resolver timing] {}={:.1}ms",
                resolver.name,
                started.elapsed().as_secs_f64() * 1000.0
            );
        }
        drop(profiled);
    }
    warnings
}

/// Run `resolver`'s JSON closure, converting both lists to JSON first (they
/// stay JSON for the passes after it).
fn json_pass(
    resolver: &LanguageResolver,
    per_file: &mut PerList,
    all_nodes: &mut Vec<Value>,
    all_edges: &mut EdgeList,
) -> Result<()> {
    let span = conversion_span(
        matches!(per_file, PerList::Records(_)) || matches!(all_edges, EdgeList::Records(_)),
        &resolver.name,
        "to_json",
    );
    let per_file = per_file.values();
    let all_edges = all_edges.values();
    drop(span);
    (resolver.resolve)(per_file, all_nodes, all_edges)
}

/// With `GRAPHICLE_PROFILE=1`, a span for a whole-list conversion made for
/// the pass `name`, when `converts`.
fn conversion_span(converts: bool, name: &str, form: &str) -> Option<crate::profile::Span> {
    if converts {
        crate::profile::span_with(|| format!("assemble.resolver.{name}.{form}"))
    } else {
        None
    }
}

/// `GRAPHICLE_WATCH_TIMING=1` also times each language resolver.
fn timing() -> bool {
    std::env::var("GRAPHICLE_WATCH_TIMING").is_ok_and(|v| v == "1")
}

/// Native scenario boundary for differential tests of arbitrary registry passes.
pub fn check_scenario(request: &Value) -> Result<Value> {
    let trace = Arc::new(Mutex::new(Vec::<String>::new()));
    let mut resolvers = Vec::new();
    for spec in request["resolvers"]
        .as_array()
        .ok_or("Expected resolvers")?
    {
        let name = spec["name"]
            .as_str()
            .ok_or("Expected resolver name")?
            .to_owned();
        let suffixes =
            serde_json::from_value(spec["suffixes"].clone()).map_err(|_| "Expected suffix set")?;
        let spec = spec.clone();
        let trace = Arc::clone(&trace);
        let called = name.clone();
        resolvers.push(LanguageResolver {
            name,
            suffixes,
            resolve: Arc::new(move |per_file, nodes, edges| {
                trace.lock().unwrap().push(called.clone());
                for (field, target) in [
                    ("append_per_file", per_file),
                    ("append_nodes", nodes),
                    ("append_edges", edges),
                ] {
                    if let Some(items) = spec[field].as_array() {
                        target.extend(items.iter().cloned());
                    }
                }
                if let Some(error) = spec["error"].as_str() {
                    Err((error.to_owned()).into())
                } else {
                    Ok(())
                }
            }),
        });
    }
    let paths: Vec<PathBuf> =
        serde_json::from_value(request["paths"].clone()).map_err(|_| "Expected paths")?;
    let list = |name: &str| request[name].as_array().cloned().unwrap_or_default();
    let (mut per_file, mut nodes, mut edges) = (list("per_file"), list("nodes"), list("edges"));
    let warnings = run_language_resolvers(
        &paths,
        &mut per_file,
        &mut nodes,
        &mut edges,
        Some(&resolvers),
    );
    let trace = trace.lock().unwrap().clone();
    Ok(json!({"trace":trace,"per_file":per_file,"nodes":nodes,"edges":edges,"warnings":warnings}))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ordered_suffix_gating_preserves_mutations_after_failure() {
        let result=check_scenario(&json!({"paths":["a.rb"],"resolvers":[
            {"name":"skipped","suffixes":[".go"],"append_edges":[{"bad":true}]},
            {"name":"failing","suffixes":[".rb"],"append_nodes":[{"id":"n"}],"error":"boom"},
            {"name":"later","suffixes":[".rb"],"append_edges":[{"source":"x","target":"y","relation":"calls"}]}
        ]})).unwrap();
        assert_eq!(result["trace"], json!(["failing", "later"]));
        assert_eq!(result["nodes"], json!([{"id":"n"}]));
        assert_eq!(
            result["edges"],
            json!([{"source":"x","target":"y","relation":"calls"}])
        );
        assert_eq!(
            result["warnings"][0]["message"],
            "failing resolution failed, skipping: boom"
        );
    }
    /// The two edge forms answer and change alike, and converting between
    /// them keeps key order, number texts, nested values and custom keys.
    #[test]
    fn record_and_json_edges_agree_and_round_trip() {
        let edges = vec![
            json!({"source":"a","target":"b","relation":"calls","confidence_score":1.0,"weight":1.0,"source_location":"L1"}),
            json!({"relation":"imports_from","target":"go_pkg_x","source":"f","custom-é":{"k":[1,"2",null]},"n":1.50,"z":-0.0,"_rust_use_group":true}),
            json!({"source":"c","target":"","local_alias":"m","_python_plain_import":true,"x":"1.0","y":1e-7}),
        ];
        let text = serde_json::to_string(&edges).unwrap();
        let mut list = EdgeList::Values(edges.clone());
        let records = list.records().unwrap().clone();
        assert_eq!(serde_json::to_string(list.values()).unwrap(), text);
        let mut values = edges;
        let mut records = records;
        for (value, record) in values.iter_mut().zip(records.iter_mut()) {
            for key in ["source", "target", "relation", "custom-é", "missing", "n"] {
                assert_eq!(value.rec_get(key), record.rec_get(key), "{key}");
                assert_eq!(rs(&*value, key), rs(&*record, key), "{key}");
            }
            value.rec_set_str("target", "t-ü");
            record.rec_set_str("target", "t-ü");
            value.rec_set("extra", json!([1, {"b": 2}]));
            record.rec_set("extra", json!([1, {"b": 2}]));
            assert_eq!(
                value.rec_shift_remove("relation"),
                record.rec_shift_remove("relation")
            );
            assert_eq!(value.rec_shift_remove("absent"), None);
            assert_eq!(record.rec_shift_remove("absent"), None);
            value.rec_reserve_key("_origin");
            record.rec_reserve_key("_origin");
            value.rec_set_str("_origin", "ast");
            record.rec_set_str("_origin", "ast");
            assert_eq!(
                serde_json::to_string(&*value).unwrap(),
                serde_json::to_string(&record.to_value()).unwrap()
            );
        }
        let mut list = EdgeList::Records(records);
        assert_eq!(
            serde_json::to_string(list.values()).unwrap(),
            serde_json::to_string(&values).unwrap()
        );
        // A non-object edge keeps the whole list in its JSON form.
        let mut odd = EdgeList::Values(vec![json!({"a": 1}), json!(null)]);
        assert!(odd.records().is_none());
        assert_eq!(odd.into_values(), vec![json!({"a": 1}), json!(null)]);
    }

    /// The passes that run on the lists in either form (Ruby, Markdown,
    /// Terraform, VB.NET, R, Solidity, Erlang, Pascal and the C# and Swift
    /// dispatch) give byte for byte the extraction of their JSON closures on
    /// JSON lists, cold, from the cache and with the intermediate output.
    #[test]
    fn any_form_passes_extract_exactly_like_their_json_closures() {
        use crate::extract::{ExtractOptions, extract};
        let _serial = crate::extractors::parse_cache::test_serial();
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let write = |rel: &str, text: &str| {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, text).unwrap();
            path
        };
        let mut paths = vec![
            write(
                "rb/greeting.rb",
                "module Greeting\n  def hello\n    1\n  end\nend\n",
            ),
            write("rb/base.rb", "class Base\n  def run\n    2\n  end\nend\n"),
            write(
                "rb/greeter.rb",
                "class Greeter < Base\n  include Greeting\n  def go\n    run\n  end\n  def self.make\n    Greeter.new\n  end\nend\n",
            ),
            write(
                "rb/app.rb",
                "class App\n  def main\n    Greeter.make\n    Greeter.new.go\n  end\nend\n",
            ),
            write("py/pkg.py", "def helper():\n    return 1\n"),
            write(
                "docs/guide.md",
                "# Guide\n\nCall `pkg.helper` or `Greeter.make` to start.\n",
            ),
            write("tf/main.tf", "module \"m\" {\n  source = \"./mod\"\n}\n"),
            write("tf/mod/main.tf", "variable \"x\" {}\n"),
            write(
                "vb/P1.vb",
                "Partial Public Class Widget\n Public Sub Run()\n  Helper()\n End Sub\nEnd Class\n",
            ),
            write(
                "vb/P2.vb",
                "Partial Public Class Widget\n Public Sub Helper()\n End Sub\nEnd Class\n",
            ),
            write("r/lib.R", "helper <- function(x) {\n  x + 1\n}\n"),
            write(
                "r/main.R",
                "source(\"lib.R\")\nmain <- function() {\n  helper(2)\n}\n",
            ),
            write(
                "sol/Base.sol",
                "pragma solidity ^0.8.0;\ncontract Base {\n    function f() public {}\n}\n",
            ),
            write(
                "sol/Child.sol",
                "pragma solidity ^0.8.0;\nimport \"./Base.sol\";\ncontract Child is Base {\n    function g() public { f(); }\n}\n",
            ),
            write(
                "erl/worker.erl",
                "-module(worker).\n-export([run/1]).\nrun(X) -> X.\n",
            ),
            write(
                "erl/boss.erl",
                "-module(boss).\n-export([go/0]).\ngo() -> worker:run(1).\n",
            ),
            write(
                "pas/shapes.pas",
                "unit Shapes;\ninterface\ntype\n  TBase = class\n    procedure Draw;\n  end;\n  TChild = class(TBase)\n    procedure Paint;\n  end;\nimplementation\nprocedure TBase.Draw;\nbegin\nend;\nprocedure TChild.Paint;\nbegin\n  Draw;\nend;\nend.\n",
            ),
            write(
                "cs/IRepo.cs",
                "namespace App {\n  public interface IRepo {\n    void Save();\n  }\n}\n",
            ),
            write(
                "cs/Repo.cs",
                "namespace App {\n  public class Repo : IRepo {\n    public void Save() { }\n  }\n}\n",
            ),
            write("sw/Store.swift", "protocol Store {\n    func save()\n}\n"),
            write(
                "sw/Disk.swift",
                "final class DiskStore: Store {\n    func save() {}\n}\n",
            ),
        ];
        paths.sort();
        let run = |json_only: bool, cache: &str, use_cache: bool, include_intermediate: bool| {
            JSON_ONLY.with(|f| f.set(json_only));
            let out = extract(
                &paths,
                &ExtractOptions {
                    root: Some(root.clone()),
                    cache_root: Some(root.join(cache)),
                    use_cache,
                    include_intermediate,
                    parallel: false,
                    ..Default::default()
                },
            );
            JSON_ONLY.with(|f| f.set(false));
            serde_json::to_string(&out.unwrap()).unwrap()
        };
        for (use_cache, intermediate) in
            [(false, false), (true, false), (true, false), (false, true)]
        {
            let json = run(true, "cache-json", use_cache, intermediate);
            let records = run(false, "cache-records", use_cache, intermediate);
            assert_eq!(
                records, json,
                "cache {use_cache}, intermediate {intermediate}"
            );
        }
        // The fixture reaches the passes: each adds or repoints edges.
        let out: Value = serde_json::from_str(&run(false, "cache-check", false, false)).unwrap();
        let edges = out["edges"].as_array().unwrap();
        let has = |relation: &str, context: &str| {
            edges
                .iter()
                .any(|e| e["relation"] == relation && e["context"] == context)
        };
        assert!(has("mixes_in", "mixin"), "ruby mixin");
        assert!(has("references", "pkg.helper"), "markdown mention");
        assert!(
            edges.iter().any(|e| e["relation"] == "module_source"),
            "terraform module source"
        );
        assert!(has("calls", "partial_type_call"), "vb.net partial call");
        assert!(has("calls", "sourced_call"), "r sourced call");
        assert!(has("calls", "remote_call"), "erlang remote call");
        assert!(has("dispatches_to", "call"), "interface dispatch");
        // Each of these passes has its form over either list form.
        let registered = registered_resolvers();
        for name in [
            "terraform_modules",
            "ruby_member_calls",
            "vbnet_partial_calls",
            "r_sourced_calls",
            "solidity_type_references",
            "solidity_inherited_calls",
            "erlang_remote_calls",
            "pascal_inherited_calls",
            "csharp_interface_dispatch",
            "swift_protocol_dispatch",
            "markdown_mentions",
        ] {
            let resolver = registered.iter().rev().find(|r| r.name == name).unwrap();
            assert!(
                matches!(native(resolver), Some(Native::Any(_))),
                "{name} runs on either form"
            );
        }
    }

    #[test]
    fn register_returns_original_and_snapshots_are_independent() {
        let before = registered_resolvers().len();
        let value = register(LanguageResolver {
            name: "test_registration".into(),
            suffixes: HashSet::new(),
            resolve: Arc::new(|_, _, _| Ok(())),
        });
        let mut snapshot = registered_resolvers();
        assert_eq!(snapshot.len(), before + 1);
        assert_eq!(snapshot.last().unwrap().name, value.name);
        snapshot.clear();
        assert_eq!(registered_resolvers().len(), before + 1);
    }
}
