//! Per-file extraction results between parsing and assembly.
//!
//! A file's result is one JSON object (`nodes`, `edges`, `raw_calls` and a
//! few language keys). Its node and raw-call records live through most of
//! assembly, one serde map each: on Kubernetes about 320 thousand nodes and
//! 750 thousand raw calls, some 1.3 GB. [`FileRecords`] keeps those two
//! arrays as compact [`Attributes`] records (eight bytes per field, values
//! shared through the value table) and every other key as it was.
//!
//! The passes read either form through [`PerFile`], whose methods mean exactly
//! what the JSON reads they replace mean. [`PerList`] converts the whole list
//! between the forms, losslessly (stored key order, number texts and nested
//! values are kept), for a pass that only reads JSON.
use crate::{record::Attributes, resolver_registry::Rec};
use serde_json::{Map, Value};
use std::borrow::Cow;

static NULL: Value = Value::Null;

/// One file's extraction result, read the way the passes read the JSON object.
pub trait PerFile: Default + Send + Sync + 'static {
    /// A node or raw-call record of this form.
    type R: Rec + Default;
    /// `result["nodes"]` when it is an array.
    fn nodes(&self) -> Option<&[Self::R]>;
    /// `result["nodes"]` when it is an array, to change in place.
    fn nodes_mut(&mut self) -> Option<&mut Vec<Self::R>>;
    /// `result["raw_calls"]` when it is an array.
    fn raw_calls(&self) -> Option<&[Self::R]>;
    /// `result["raw_calls"]` when it is an array, to change in place.
    fn raw_calls_mut(&mut self) -> Option<&mut Vec<Self::R>>;
    /// `result.get(key)` for every key but `nodes` and `raw_calls`.
    fn key(&self, key: &str) -> Option<&Value>;
    /// `result.get_mut(key)` for every key but `nodes` and `raw_calls`.
    fn key_mut(&mut self, key: &str) -> Option<&mut Value>;
    /// `result[key]` (`Null` when absent) for every key but `nodes` and
    /// `raw_calls`.
    fn at(&self, key: &str) -> &Value {
        self.key(key).unwrap_or(&NULL)
    }
    /// The number of nodes, when `nodes` is an array.
    fn node_count(&self) -> Option<usize> {
        self.nodes().map(<[Self::R]>::len)
    }
    /// A node record holding exactly the JSON node `value` (an object).
    fn node_from(value: &Value) -> Self::R;
    /// The results as JSON objects: borrowed when they already are.
    fn json_slice(items: &[Self]) -> Cow<'_, [Value]>;
    /// `result.get("raw_calls")` when it is present but no array. Only a
    /// JSON result can hold one: a compact result's raw calls are an array
    /// of objects or absent.
    fn raw_calls_other(&self) -> Option<&Value> {
        None
    }
}

impl PerFile for Value {
    type R = Value;
    fn nodes(&self) -> Option<&[Value]> {
        self.get("nodes")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
    }
    fn nodes_mut(&mut self) -> Option<&mut Vec<Value>> {
        self.get_mut("nodes").and_then(Value::as_array_mut)
    }
    fn raw_calls(&self) -> Option<&[Value]> {
        self.get("raw_calls")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
    }
    fn raw_calls_mut(&mut self) -> Option<&mut Vec<Value>> {
        self.get_mut("raw_calls").and_then(Value::as_array_mut)
    }
    fn key(&self, key: &str) -> Option<&Value> {
        self.get(key)
    }
    fn key_mut(&mut self, key: &str) -> Option<&mut Value> {
        self.get_mut(key)
    }
    fn node_from(value: &Value) -> Value {
        value.clone()
    }
    fn json_slice(items: &[Self]) -> Cow<'_, [Value]> {
        Cow::Borrowed(items)
    }
    fn raw_calls_other(&self) -> Option<&Value> {
        self.get("raw_calls").filter(|v| !v.is_array())
    }
}

/// A file's result with its node and raw-call arrays as compact records.
///
/// `rest` is the object itself in its stored key order; where `nodes` and
/// `raw_calls` were, it holds a `Null` placeholder that [`Self::into_value`]
/// fills again, so the rebuilt object has its original key order.
#[derive(Clone, Default)]
pub struct FileRecords {
    rest: Map<String, Value>,
    nodes: Option<Vec<Attributes>>,
    raw_calls: Option<Vec<Attributes>>,
}

const NODES: &str = "nodes";
const RAW_CALLS: &str = "raw_calls";

impl FileRecords {
    /// The compact form of `value`, or `value` itself back when it is not an
    /// object whose `nodes` and `raw_calls` (where present) are arrays of
    /// objects: only those convert without loss.
    pub fn from_value(value: Value) -> Result<Self, Value> {
        let Value::Object(mut rest) = value else {
            return Err(value);
        };
        let convertible = |v: Option<&Value>| {
            v.is_none_or(|v| v.as_array().is_some_and(|a| a.iter().all(Value::is_object)))
        };
        if !convertible(rest.get(NODES)) || !convertible(rest.get(RAW_CALLS)) {
            return Err(Value::Object(rest));
        }
        let nodes = take_records(&mut rest, NODES);
        let raw_calls = take_records(&mut rest, RAW_CALLS);
        // The other values keep the exact-size maps they would have had as
        // part of the whole object.
        let mut tight = Map::with_capacity(rest.len());
        for (key, mut value) in rest {
            crate::extract::tighten(&mut value);
            tight.insert(key, value);
        }
        Ok(Self {
            rest: tight,
            nodes,
            raw_calls,
        })
    }
    /// The JSON object again, with its original key order.
    pub fn into_value(self) -> Value {
        let Self {
            mut rest,
            nodes,
            raw_calls,
        } = self;
        put_records(&mut rest, NODES, nodes, |r| Value::Object(Map::from(r)));
        put_records(&mut rest, RAW_CALLS, raw_calls, |r| {
            Value::Object(Map::from(r))
        });
        Value::Object(rest)
    }
    /// [`Self::into_value`] of a copy.
    pub fn to_value(&self) -> Value {
        let mut rest = self.rest.clone();
        put_records(&mut rest, NODES, self.nodes.as_ref(), Rec::to_value);
        put_records(&mut rest, RAW_CALLS, self.raw_calls.as_ref(), Rec::to_value);
        Value::Object(rest)
    }
}

/// Move the array at `key` (an array of objects) out as records, leaving
/// `Null` in its place.
fn take_records(rest: &mut Map<String, Value>, key: &str) -> Option<Vec<Attributes>> {
    let slot = rest.get_mut(key)?;
    let Value::Array(items) = std::mem::take(slot) else {
        unreachable!("checked to be an array")
    };
    let mut records = Vec::with_capacity(items.len());
    records.extend(items.into_iter().map(|item| match item {
        Value::Object(map) => Attributes::from(map),
        _ => unreachable!("checked to be objects"),
    }));
    Some(records)
}

/// Put `records` back as the array at `key`, in the placeholder's position.
fn put_records<I: IntoIterator>(
    rest: &mut Map<String, Value>,
    key: &str,
    records: Option<I>,
    to_value: impl Fn(I::Item) -> Value,
) {
    if let Some(records) = records {
        let items = records.into_iter().map(to_value).collect::<Vec<_>>();
        *rest.get_mut(key).expect("placeholder of a converted array") = Value::Array(items);
    }
}

impl PerFile for FileRecords {
    type R = Attributes;
    fn nodes(&self) -> Option<&[Attributes]> {
        self.nodes.as_deref()
    }
    fn nodes_mut(&mut self) -> Option<&mut Vec<Attributes>> {
        self.nodes.as_mut()
    }
    fn raw_calls(&self) -> Option<&[Attributes]> {
        self.raw_calls.as_deref()
    }
    fn raw_calls_mut(&mut self) -> Option<&mut Vec<Attributes>> {
        self.raw_calls.as_mut()
    }
    fn key(&self, key: &str) -> Option<&Value> {
        debug_assert!(key != NODES && key != RAW_CALLS, "read {key} as records");
        self.rest.get(key)
    }
    fn key_mut(&mut self, key: &str) -> Option<&mut Value> {
        debug_assert!(key != NODES && key != RAW_CALLS, "read {key} as records");
        self.rest.get_mut(key)
    }
    fn node_from(value: &Value) -> Attributes {
        match value {
            Value::Object(map) => Attributes::from(map.clone()),
            _ => unreachable!("a node is an object"),
        }
    }
    fn json_slice(items: &[Self]) -> Cow<'_, [Value]> {
        Cow::Owned(items.iter().map(Self::to_value).collect())
    }
}

/// Every file's result, in whichever form the current pass reads.
pub enum PerList {
    Values(Vec<Value>),
    Records(Vec<FileRecords>),
}

/// Run `$body` with `$p` bound to the list's results in their current form
/// (`&mut Vec<Value>` or `&mut Vec<FileRecords>`), for a pass generic over
/// [`PerFile`].
macro_rules! per_dispatch {
    ($per:expr, $p:ident => $body:expr) => {
        match $per {
            $crate::extract::per_file::PerList::Values($p) => $body,
            $crate::extract::per_file::PerList::Records($p) => $body,
        }
    };
}
pub(crate) use per_dispatch;

impl PerList {
    pub fn len(&self) -> usize {
        match self {
            Self::Values(v) => v.len(),
            Self::Records(r) => r.len(),
        }
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// The results as JSON objects, converting in place.
    pub fn values(&mut self) -> &mut Vec<Value> {
        if let Self::Records(records) = self {
            let records = std::mem::take(records);
            *self = Self::Values(convert(records, FileRecords::into_value));
        }
        match self {
            Self::Values(values) => values,
            Self::Records(_) => unreachable!("converted above"),
        }
    }
    /// The results as JSON objects.
    pub fn into_values(mut self) -> Vec<Value> {
        std::mem::take(self.values())
    }
    /// The results in compact form, converting in place; `None` (and the list
    /// unchanged) when some result does not convert ([`FileRecords::from_value`]).
    pub fn records(&mut self) -> Option<&mut Vec<FileRecords>> {
        if let Self::Values(values) = self {
            if crate::resolver_registry::json_only() || !values.iter().all(convertible) {
                return None;
            }
            let values = std::mem::take(values);
            *self = Self::Records(convert(values, |v| {
                FileRecords::from_value(v).unwrap_or_else(|_| unreachable!("checked above"))
            }));
        }
        match self {
            Self::Records(records) => Some(records),
            Self::Values(_) => None,
        }
    }
    /// Run `f` on result `i` as a JSON object. A compact result converts back
    /// afterwards; if the edited object no longer converts, the whole list
    /// turns JSON.
    pub fn edit_json<T>(&mut self, i: usize, f: impl FnOnce(&mut Value) -> T) -> T {
        match self {
            Self::Values(values) => f(&mut values[i]),
            Self::Records(records) => {
                let mut value = std::mem::take(&mut records[i]).into_value();
                let out = f(&mut value);
                match FileRecords::from_value(value) {
                    Ok(record) => records[i] = record,
                    Err(value) => {
                        let values = self.values();
                        values[i] = value;
                    }
                }
                out
            }
        }
    }
}

/// Whether `value` converts to [`FileRecords`].
fn convertible(value: &Value) -> bool {
    let Value::Object(map) = value else {
        return false;
    };
    [NODES, RAW_CALLS].iter().all(|key| {
        map.get(*key)
            .is_none_or(|v| v.as_array().is_some_and(|a| a.iter().all(Value::is_object)))
    })
}

/// `items` converted one by one, in order, on the run's workers: every item
/// converts on its own and the value table is shared, so only the time
/// changes with the split.
pub(crate) fn convert<T: Default + Send, U: Send>(
    mut items: Vec<T>,
    f: impl Fn(T) -> U + Sync,
) -> Vec<U> {
    let workers = crate::extractors::parse_cache::workers();
    if workers <= 1 || items.len() < 64 {
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Converting keeps the object's key order, the records' key order,
    /// number texts, nested values and odd keys; both forms read alike.
    #[test]
    fn file_records_round_trip_and_read_like_json() {
        let values = [
            json!({"nodes":[{"id":"a","label":"A","n":1.50,"metadata":{"k":[1,"2"]}}],"edges":[{"source":"a","target":"b","weight":1.0}],"raw_calls":[{"caller_nid":"a","callee":"b","is_member_call":false,"import_path":"x/y","z":-0.0}],"go_imports":{"y":"x/y"}}),
            json!({"go_imports":{},"raw_calls":[],"edges":[],"nodes":[]}),
            json!({"nodes":[],"edges":[],"skipped":"generated"}),
            json!({"edges":[],"nodes":[{"id":"é","custom-ü":null}],"parse_errors":{"multiline_error":true}}),
        ];
        for value in values {
            let text = serde_json::to_string(&value).unwrap();
            let record = FileRecords::from_value(value.clone()).ok().unwrap();
            assert_eq!(serde_json::to_string(&record.to_value()).unwrap(), text);
            assert_eq!(record.node_count(), value.node_count());
            for (a, b) in record
                .nodes()
                .into_iter()
                .flatten()
                .zip(value.nodes().into_iter().flatten())
            {
                assert_eq!(a.to_value(), *b);
            }
            assert_eq!(
                record.raw_calls().map(<[Attributes]>::len),
                value.raw_calls().map(<[Value]>::len)
            );
            for key in ["edges", "go_imports", "skipped", "parse_errors", "absent"] {
                assert_eq!(record.key(key), value.key(key), "{key}");
                assert_eq!(record.at(key), value.at(key), "{key}");
            }
            assert_eq!(serde_json::to_string(&record.into_value()).unwrap(), text);
        }
        // Only objects whose node and raw-call arrays hold objects convert.
        for odd in [
            json!(null),
            json!({"nodes": 1}),
            json!({"nodes": [1]}),
            json!({"raw_calls": [{"a": 1}, "b"]}),
        ] {
            assert_eq!(FileRecords::from_value(odd.clone()).err(), Some(odd));
        }
        let mut list = PerList::Values(vec![json!({"nodes":[{"id":"x"}]}), json!({"nodes":[2]})]);
        assert!(list.records().is_none());
        let mut list = PerList::Values(vec![json!({"nodes":[{"id":"x"}],"raw_calls":[]})]);
        assert!(list.records().is_some());
        // An edit that leaves an unconvertible result turns the list JSON.
        list.edit_json(0, |v| v["nodes"] = json!([3]));
        assert!(matches!(list, PerList::Values(_)));
        assert_eq!(
            list.into_values(),
            vec![json!({"nodes":[3],"raw_calls":[]})]
        );
    }
}
