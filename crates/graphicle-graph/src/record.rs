//! Compact ordered metadata for resident graph nodes and edges.
//!
//! Common keys are enums and the streaming loader reads directly into this
//! representation. Unknown JSON fields remain lossless. Small records need no
//! hash table; unusually large records acquire an index to avoid quadratic
//! insertion/lookup. Mutation preserves serde_json's preserve_order contract.
use rustc_hash::FxHashMap;
use serde::{
    Deserialize, Deserializer, Serialize, Serializer,
    de::{MapAccess, Visitor},
    ser::SerializeMap,
};
use serde_json::{Map, Value};
use std::{fmt, ops::Index};

macro_rules! fields {
    ($($variant:ident => $name:literal),* $(,)?) => {
        #[repr(u32)]
        enum Known { $($variant,)* }
        #[derive(Clone, Debug, PartialEq, Eq)]
        pub(crate) enum Field { $($variant,)* Other(Box<str>) }
        impl Field {
            fn known(name:&str)->Option<Self> {match name { $($name => Some(Self::$variant),)* _=>None }}
            pub(crate) fn new(name:String)->Self {Self::known(&name).unwrap_or_else(||Self::Other(name.into_boxed_str()))}
            pub(crate) fn borrowed(name:&str)->Self {Self::known(name).unwrap_or_else(||Self::Other(name.into()))}
            fn name(&self)->&str {match self {$(Self::$variant=>$name,)* Self::Other(s)=>s}}
            fn code(&self)->Option<u32> {match self {$(Self::$variant=>Some(Known::$variant as u32),)* Self::Other(_)=>None}}
            fn code_name(code:u32)->Option<&'static str> {const NAMES:&[&str]=&[$($name,)*]; NAMES.get(code as usize).copied()}
            fn from_code(code:u32)->Option<Self> {match code {$(n if n==Known::$variant as u32=>Some(Self::$variant),)* _=>None}}
        }
    }
}
fields! {
    Label => "label", Kind => "kind", Type => "type", FileType => "file_type",
    SourceFile => "source_file", SourceLocation => "source_location", DefinitionFile => "definition_file",
    QualifiedName => "qualified_name", Relpath => "relpath", Language => "language", Metadata => "metadata",
    StableId => "stable_id", Revision => "revision", Repo => "repo", Derivation => "derivation",
    Relation => "relation", Weight => "weight", Confidence => "confidence", ConfidenceScore => "confidence_score",
    Context => "context", Provenance => "provenance", Line => "line", EndLine => "end_line",
    Span => "span", ByteStart => "byte_start", ByteEnd => "byte_end", Community => "community",
    Description => "description", Name => "name", ReceiverEvidence => "receiver_evidence",
    ReceiverType => "receiver_type", IsTest => "is_test", LocalId => "local_id",
    AliasOf => "alias_of", TargetFile => "target_file", LinkType => "link_type",
    Origin => "_origin", Callable => "_callable", CallableClass => "_callable_class",
    Namespace => "namespace", LocalAlias => "local_alias", TestId => "test_id",
    NodeKind => "node_kind", NodeType => "node_type", RoutePath => "route_path", RouteMethod => "route_method",
    RouteMounts => "_route_mounts", RouteApps => "_route_apps", Resolved => "resolved",
    Exported => "exported", IsPublic => "is_public", IsAsync => "is_async", IsStatic => "is_static",
    Signature => "signature", Docstring => "docstring", Decorators => "decorators",
    Module => "module", Package => "package", Parent => "parent", Visibility => "visibility",
}
impl<'de> Deserialize<'de> for Field {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Key;
        impl Visitor<'_> for Key {
            type Value = Field;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a property name")
            }
            fn visit_str<E: serde::de::Error>(self, key: &str) -> Result<Field, E> {
                Ok(Field::borrowed(key))
            }
            fn visit_string<E: serde::de::Error>(self, key: String) -> Result<Field, E> {
                Ok(Field::new(key))
            }
        }
        d.deserialize_string(Key)
    }
}
const INDEX_THRESHOLD: usize = 64;
const CUSTOM_KEY: u32 = 1 << 31;

/// The common case stores only a four-byte key code beside each value.
/// Unrecognized names live in an optional side table. Freed slots are reused,
/// so repeated insertion/removal does not retain an unbounded name history.
#[derive(Clone, Debug, Default)]
struct CustomNames {
    names: Vec<Option<Box<str>>>,
    free: Vec<u32>,
}

#[derive(Clone, Debug, Default)]
pub struct Attributes {
    entries: Vec<(u32, Value)>,
    custom: Option<Box<CustomNames>>,
    index: Option<Box<FxHashMap<Box<str>, usize>>>,
}
/// Read-only interface shared by boundary JSON and native metadata records.
pub trait Properties {
    fn get(&self, key: &str) -> Option<&Value>;
}
impl Properties for Map<String, Value> {
    fn get(&self, key: &str) -> Option<&Value> {
        self.get(key)
    }
}
impl Properties for Attributes {
    fn get(&self, key: &str) -> Option<&Value> {
        self.get(key)
    }
}
impl Attributes {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
    fn name(&self, key: u32) -> &str {
        if let Some(name) = Field::code_name(key) {
            name
        } else {
            self.custom.as_ref().expect("custom key table").names[(key & !CUSTOM_KEY) as usize]
                .as_deref()
                .expect("live custom key")
        }
    }
    fn encode(&mut self, field: Field) -> u32 {
        if let Some(code) = field.code() {
            return code;
        }
        let Field::Other(name) = field else {
            unreachable!("known field has a code")
        };
        let custom = self.custom.get_or_insert_with(Default::default);
        let index = if let Some(index) = custom.free.pop() {
            custom.names[index as usize] = Some(name);
            index
        } else {
            // A record cannot approach this bound within the input byte cap.
            let index = u32::try_from(custom.names.len()).expect("too many property names");
            assert!(index < CUSTOM_KEY, "too many property names");
            custom.names.push(Some(name));
            index
        };
        CUSTOM_KEY | index
    }
    fn release_name(&mut self, code: u32) {
        if code & CUSTOM_KEY != 0 {
            let index = code & !CUSTOM_KEY;
            let custom = self.custom.as_mut().expect("custom key table");
            custom.names[index as usize] = None;
            custom.free.push(index);
            if custom.free.len() == custom.names.len() {
                self.custom = None;
            }
        }
    }
    fn position(&self, key: &str) -> Option<usize> {
        if let Some(index) = &self.index {
            return index.get(key).copied();
        }
        if let Some(known) = Field::known(key).and_then(|f| f.code()) {
            self.entries.iter().position(|(k, _)| *k == known)
        } else {
            self.entries
                .iter()
                .position(|(k, _)| *k & CUSTOM_KEY != 0 && self.name(*k) == key)
        }
    }
    fn refresh_index(&mut self) {
        self.index = (self.entries.len() >= INDEX_THRESHOLD).then(|| {
            Box::new(
                self.entries
                    .iter()
                    .enumerate()
                    .map(|(i, (k, _))| (self.name(*k).into(), i))
                    .collect(),
            )
        });
    }
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.position(key).map(|i| &self.entries[i].1)
    }
    pub fn get_mut(&mut self, key: &str) -> Option<&mut Value> {
        self.position(key).map(|i| &mut self.entries[i].1)
    }
    pub fn contains_key(&self, key: &str) -> bool {
        self.position(key).is_some()
    }
    pub fn insert(&mut self, key: String, value: Value) -> Option<Value> {
        self.insert_field(Field::new(key), value)
    }
    pub(crate) fn insert_field(&mut self, key: Field, value: Value) -> Option<Value> {
        // Streaming decode already classified this key. Avoid converting its
        // code to a string and classifying that string a second time.
        let position = if self.index.is_none() {
            if let Some(code) = key.code() {
                self.entries.iter().position(|(k, _)| *k == code)
            } else {
                self.position(key.name())
            }
        } else {
            self.position(key.name())
        };
        if let Some(i) = position {
            return Some(std::mem::replace(&mut self.entries[i].1, value));
        }
        if let Some(index) = &mut self.index {
            index.insert(key.name().into(), self.entries.len());
        }
        let code = self.encode(key);
        self.entries.push((code, value));
        if self.index.is_none() && self.entries.len() >= INDEX_THRESHOLD {
            self.refresh_index();
        }
        None
    }
    pub fn remove(&mut self, key: &str) -> Option<Value> {
        let i = self.position(key)?;
        let (code, value) = self.entries.swap_remove(i);
        let swapped = self
            .index
            .as_ref()
            .and_then(|_| self.entries.get(i))
            .map(|(key, _)| Box::<str>::from(self.name(*key)));
        if let Some(index) = &mut self.index {
            index.remove(key);
            if let Some(key) = swapped {
                index.insert(key, i);
            }
            if self.entries.len() < INDEX_THRESHOLD {
                self.index = None;
            }
        }
        self.release_name(code);
        Some(value)
    }
    pub fn shift_remove(&mut self, key: &str) -> Option<Value> {
        let i = self.position(key)?;
        let (code, value) = self.entries.remove(i);
        self.release_name(code);
        if self.index.is_some() {
            self.refresh_index();
        }
        Some(value)
    }
    pub(crate) fn shift_insert(
        &mut self,
        position: usize,
        key: String,
        value: Value,
    ) -> Option<Value> {
        assert!(
            position <= self.entries.len(),
            "attribute insertion index out of bounds"
        );
        let previous = self.shift_remove(&key);
        let code = self.encode(Field::new(key));
        self.entries
            .insert(position.min(self.entries.len()), (code, value));
        if self.entries.len() >= INDEX_THRESHOLD {
            self.refresh_index();
        }
        previous
    }
    pub fn entry(&mut self, key: impl Into<String>) -> Entry<'_> {
        Entry {
            attributes: self,
            key: key.into(),
        }
    }
    pub fn retain(&mut self, mut predicate: impl FnMut(&str, &mut Value) -> bool) {
        let mut released = vec![];
        let custom = &self.custom;
        self.entries.retain_mut(|(k, v)| {
            let name = Field::code_name(*k).unwrap_or_else(|| {
                custom.as_ref().expect("custom key table").names[(*k & !CUSTOM_KEY) as usize]
                    .as_deref()
                    .expect("live custom key")
            });
            let keep = predicate(name, v);
            if !keep {
                released.push(*k);
            }
            keep
        });
        for code in released {
            self.release_name(code);
        }
        if self.index.is_some() {
            self.refresh_index();
        }
    }
    pub fn iter(&self) -> impl ExactSizeIterator<Item = (&str, &Value)> {
        self.entries.iter().map(|(k, v)| (self.name(*k), v))
    }
    pub fn keys(&self) -> impl ExactSizeIterator<Item = &str> {
        self.entries.iter().map(|(k, _)| self.name(*k))
    }
    pub fn values(&self) -> impl ExactSizeIterator<Item = &Value> {
        self.entries.iter().map(|(_, v)| v)
    }
    pub fn compact(&mut self) {
        self.entries.shrink_to_fit();
        if let Some(index) = &mut self.index {
            index.shrink_to_fit();
        }
        if let Some(custom) = &mut self.custom {
            custom.names.shrink_to_fit();
            custom.free.shrink_to_fit();
        }
    }
    pub fn to_map(&self) -> Map<String, Value> {
        self.iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }
    pub fn extend_record(&mut self, other: Self) {
        for (key, value) in (IntoFields {
            entries: other.entries.into_iter(),
            custom: other.custom,
        }) {
            self.insert_field(key, value);
        }
    }
    /// Query snapshot encoding (`crate::snapshot`), in stored order.
    pub(crate) fn write_snapshot<'a>(&'a self, w: &mut crate::snapshot::Writer<'a>) {
        w.usize(self.entries.len());
        for (key, value) in self.iter() {
            w.key(key);
            w.value(value);
        }
    }
    pub(crate) fn read_snapshot(r: &mut crate::snapshot::Reader<'_, '_>) -> crate::Result<Self> {
        let n = r.len(2)?;
        let mut record = Self::new();
        record.entries.reserve(n);
        for _ in 0..n {
            let field = r.key()?;
            record.insert_field(field, r.value()?);
        }
        Ok(record)
    }
}
pub struct Entry<'a> {
    attributes: &'a mut Attributes,
    key: String,
}
impl<'a> Entry<'a> {
    pub fn or_insert(self, value: Value) -> &'a mut Value {
        self.or_insert_with(|| value)
    }
    pub fn or_insert_with(self, f: impl FnOnce() -> Value) -> &'a mut Value {
        let i = if let Some(i) = self.attributes.position(&self.key) {
            i
        } else {
            self.attributes.insert(self.key, f());
            self.attributes.len() - 1
        };
        &mut self.attributes.entries[i].1
    }
}
impl From<Map<String, Value>> for Attributes {
    fn from(map: Map<String, Value>) -> Self {
        let mut attributes = Self::new();
        attributes.entries.reserve(map.len());
        for (key, value) in map {
            attributes.insert(key, value);
        }
        attributes
    }
}
impl From<Attributes> for Map<String, Value> {
    fn from(record: Attributes) -> Self {
        record.into_iter().collect()
    }
}
impl From<Attributes> for Value {
    fn from(record: Attributes) -> Self {
        Self::Object(record.into())
    }
}
impl FromIterator<(String, Value)> for Attributes {
    fn from_iter<T: IntoIterator<Item = (String, Value)>>(items: T) -> Self {
        let mut out = Self::new();
        out.extend(items);
        out
    }
}
impl Extend<(String, Value)> for Attributes {
    fn extend<T: IntoIterator<Item = (String, Value)>>(&mut self, items: T) {
        for (k, v) in items {
            self.insert(k, v);
        }
    }
}
impl IntoIterator for Attributes {
    type Item = (String, Value);
    type IntoIter = IntoIter;
    fn into_iter(self) -> Self::IntoIter {
        IntoIter(IntoFields {
            entries: self.entries.into_iter(),
            custom: self.custom,
        })
    }
}
struct IntoFields {
    entries: std::vec::IntoIter<(u32, Value)>,
    custom: Option<Box<CustomNames>>,
}
impl Iterator for IntoFields {
    type Item = (Field, Value);
    fn next(&mut self) -> Option<Self::Item> {
        self.entries.next().map(|(code, value)| {
            let field = Field::from_code(code).unwrap_or_else(|| {
                Field::Other(
                    self.custom.as_mut().expect("custom key table").names
                        [(code & !CUSTOM_KEY) as usize]
                        .take()
                        .expect("live custom key"),
                )
            });
            (field, value)
        })
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.entries.size_hint()
    }
}
impl ExactSizeIterator for IntoFields {}
pub struct IntoIter(IntoFields);
impl Iterator for IntoIter {
    type Item = (String, Value);
    fn next(&mut self) -> Option<Self::Item> {
        self.0.next().map(|(field, value)| {
            let key = match field {
                Field::Other(s) => s.into_string(),
                other => other.name().into(),
            };
            (key, value)
        })
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }
}
impl ExactSizeIterator for IntoIter {}
impl Index<&str> for Attributes {
    type Output = Value;
    fn index(&self, key: &str) -> &Value {
        self.get(key).expect("no entry found for key")
    }
}
impl PartialEq for Attributes {
    fn eq(&self, other: &Self) -> bool {
        self.len() == other.len() && self.iter().all(|(k, v)| other.get(k) == Some(v))
    }
}
impl Serialize for Attributes {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut map = s.serialize_map(Some(self.len()))?;
        for (k, v) in self.iter() {
            map.serialize_entry(k, v)?;
        }
        map.end()
    }
}
impl<'de> Deserialize<'de> for Attributes {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Record;
        impl<'de> Visitor<'de> for Record {
            type Value = Attributes;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a property object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Attributes, A::Error> {
                let mut attrs = Attributes::new();
                while let Some((key, value)) = map.next_entry::<Field, Value>()? {
                    attrs.insert_field(key, value);
                }
                attrs.compact();
                Ok(attrs)
            }
        }
        d.deserialize_map(Record)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn custom_key_slots_are_reused_without_changing_other_records() {
        let mut record = Attributes::from(
            serde_json::from_value::<Map<String, Value>>(json!({
                "label":"known", "custom-a":{"nested":[1,true]}, "custom-b":-0.0
            }))
            .unwrap(),
        );
        let original = record.clone();
        let before = serde_json::to_string(&original).unwrap();
        for i in 0..1000 {
            let key = format!("temporary-{i}");
            record.insert(key.clone(), json!(i));
            assert_eq!(record.remove(&key), Some(json!(i)));
        }
        assert_eq!(record, original);
        assert_eq!(record.custom.as_ref().unwrap().names.len(), 3);
        record.shift_remove("custom-a");
        record.insert("replacement".into(), json!("kept"));
        *record.get_mut("custom-b").unwrap() = json!(7);
        assert_eq!(record["replacement"], "kept");
        assert_eq!(serde_json::to_string(&original).unwrap(), before);
        let consumed: Map<String, Value> = record.clone().into();
        assert_eq!(
            serde_json::to_string(&record).unwrap(),
            serde_json::to_string(&consumed).unwrap()
        );
        record.retain(|name, _| name == "label");
        assert!(record.custom.is_none());
    }
    #[test]
    fn large_and_small_records_preserve_order_duplicates_and_unknown_values() {
        let mut reference = Map::new();
        let mut record = Attributes::new();
        for i in 0..1000 {
            let key = format!("custom-{i}");
            let value = json!({"nested":[i,null,true,"évidence"]});
            reference.insert(key.clone(), value.clone());
            record.insert(key, value);
        }
        for (i, key) in ["label", "source_file", "custom-9", "_origin"]
            .iter()
            .enumerate()
        {
            reference.insert((*key).into(), json!(i));
            record.insert((*key).into(), json!(i));
        }
        for i in (0..1000).step_by(3) {
            let key = format!("custom-{i}");
            assert_eq!(reference.remove(&key), record.remove(&key));
        }
        for i in (0..1000).step_by(7) {
            let key = format!("custom-{i}");
            assert_eq!(reference.shift_remove(&key), record.shift_remove(&key));
        }
        record.retain(|k, _| !k.starts_with("custom-"));
        reference.retain(|k, _| !k.starts_with("custom-"));
        *record.entry("span").or_insert(json!({})) = json!([1, 2]);
        reference.insert("span".into(), json!([1, 2]));
        assert_eq!(
            serde_json::to_string(&record).unwrap(),
            serde_json::to_string(&reference).unwrap()
        );
        let text = r#"{"label":"before","custom":{"x":1},"label":"after","source_file":"a.py"}"#;
        let record: Attributes = serde_json::from_str(text).unwrap();
        let reference: Map<String, Value> = serde_json::from_str(text).unwrap();
        assert_eq!(
            serde_json::to_string(&record).unwrap(),
            serde_json::to_string(&reference).unwrap()
        );
        assert!(record.index.is_none());
    }
}
