//! Compact ordered metadata for resident graph nodes and edges.
//!
//! Common keys are enums and the streaming loader reads directly into this
//! representation. Scalar values are shared through the process-wide value
//! table (`crate::vtab`), so a field costs eight bytes. Unknown JSON fields
//! remain lossless. Small records need no hash table; unusually large records
//! acquire an index to avoid quadratic insertion/lookup. Mutation preserves
//! serde_json's preserve_order contract.
use crate::vtab::{self, VId};
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
            fn known_code(name:&str)->Option<u32> {match name { $($name => Some(Known::$variant as u32),)* _=>None }}
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
    // Every built edge stores its endpoints' direction (build.rs) and every
    // loaded node its export `norm_label`: as known keys they need no
    // per-record custom-name table (about 128 bytes per edge).
    Src => "_src", Tgt => "_tgt", NormLabel => "norm_label",
    // Keys of extraction records (nodes, edges, raw calls), appended so the
    // codes above keep their values.
    Id => "id", Source => "source", Target => "target", Callee => "callee",
    CallerNid => "caller_nid", IsMemberCall => "is_member_call", OriginFile => "origin_file",
    Resolution => "resolution", Receiver => "receiver", RefToken => "ref_token",
    Arity => "arity", Key => "key", TargetFqn => "target_fqn",
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
/// A slot value with this bit set indexes the record's own arrays and objects
/// (`Extra::complex`); otherwise it is a [`VId`] of the shared scalar table.
const LOCAL: u32 = 1 << 31;
const _: () = assert!(VId::LIMIT == LOCAL);

/// One field: a key code (a known [`Field`], or `CUSTOM_KEY` and an index into
/// the custom names) and a value (a [`VId`], or `LOCAL` and an index into the
/// complex values). Eight bytes, against an 80-byte map entry plus the value's
/// own heap block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Slot {
    key: u32,
    val: u32,
}

/// What only some records need: unrecognized key names, the lookup index of
/// large records and arrays/objects. Freed name and value slots are reused, so
/// repeated insertion/removal does not retain an unbounded history.
#[derive(Clone, Debug, Default)]
struct Extra {
    names: Vec<Option<Box<str>>>,
    free: Vec<u32>,
    index: Option<FxHashMap<Box<str>, usize>>,
    complex: Vec<Value>,
    complex_free: Vec<u32>,
}
impl Extra {
    fn idle(&self) -> bool {
        self.free.len() == self.names.len()
            && self.index.is_none()
            && self.complex_free.len() == self.complex.len()
    }
}

/// A node's or edge's ordered attributes: serde_json `preserve_order` map
/// semantics (insert replaces in place, `remove` swap-removes, `shift_remove`
/// shifts) over 8-byte slots whose scalar values live once in the process-wide
/// table ([`crate::vtab`]). Reads hand out the same `&Value` a map would.
#[derive(Clone, Default)]
pub struct Attributes {
    slots: Vec<Slot>,
    extra: Option<Box<Extra>>,
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
/// A record the build edits before it becomes a graph edge: a JSON object or
/// a compact [`Attributes`] record. Both keep serde_json's `preserve_order`
/// rules, so the same edits give the same keys in the same order.
pub(crate) trait Record: Properties + Clone + Into<Attributes> {
    /// `record[key] = value`: replaced in place when present, else appended.
    fn put(&mut self, key: &str, value: Value);
    /// `record.shift_remove(key)`.
    fn take_key(&mut self, key: &str) -> Option<Value>;
}
impl Record for Map<String, Value> {
    fn put(&mut self, key: &str, value: Value) {
        match self.iter_mut().find(|(k, _)| k.as_str() == key) {
            Some((_, slot)) => *slot = value,
            None => {
                self.insert(key.into(), value);
            }
        }
    }
    fn take_key(&mut self, key: &str) -> Option<Value> {
        self.shift_remove(key)
    }
}
impl Record for Attributes {
    fn put(&mut self, key: &str, value: Value) {
        self.insert_field(Field::borrowed(key), value);
    }
    fn take_key(&mut self, key: &str) -> Option<Value> {
        self.shift_remove(key)
    }
}
/// One item of an extraction's edge list: a JSON value, which is a record
/// only when it is an object, or a compact record. The build reads and edits
/// edges through this, so an extract hands its assembled records to the
/// graph without a JSON copy of every edge.
pub(crate) trait EdgeItem: Clone + Default {
    type Record: Record;
    fn record(&self) -> Option<&Self::Record>;
    fn record_mut(&mut self) -> Option<&mut Self::Record>;
    fn into_record(self) -> Option<Self::Record>;
}
impl EdgeItem for Value {
    type Record = Map<String, Value>;
    fn record(&self) -> Option<&Self::Record> {
        self.as_object()
    }
    fn record_mut(&mut self) -> Option<&mut Self::Record> {
        self.as_object_mut()
    }
    fn into_record(self) -> Option<Self::Record> {
        match self {
            Value::Object(map) => Some(map),
            _ => None,
        }
    }
}
impl EdgeItem for Attributes {
    type Record = Self;
    fn record(&self) -> Option<&Self> {
        Some(self)
    }
    fn record_mut(&mut self) -> Option<&mut Self> {
        Some(self)
    }
    fn into_record(self) -> Option<Self> {
        Some(self)
    }
}
/// `item[key]` as serde indexes a JSON value: the value, or null when the
/// key is absent or the item is no record.
pub(crate) fn item_field<'a, E: EdgeItem>(item: &'a E, key: &str) -> &'a Value {
    static NULL: Value = Value::Null;
    item.record().and_then(|r| r.get(key)).unwrap_or(&NULL)
}
fn name_in(extra: &Option<Box<Extra>>, key: u32) -> &str {
    if let Some(name) = Field::code_name(key) {
        name
    } else {
        extra.as_ref().expect("custom key table").names[(key & !CUSTOM_KEY) as usize]
            .as_deref()
            .expect("live custom key")
    }
}
fn value_in(extra: &Option<Box<Extra>>, val: u32) -> &Value {
    if val & LOCAL != 0 {
        &extra.as_ref().expect("complex values").complex[(val & !LOCAL) as usize]
    } else {
        vtab::resolve(VId::from_raw(val))
    }
}
impl Attributes {
    pub fn new() -> Self {
        Self::default()
    }
    /// The record holding only `key` = `value`, allocated at exactly one
    /// entry (the clustering copies keep one such record, the `weight`, per
    /// edge).
    pub(crate) fn single(key: &str, value: Value) -> Self {
        let mut record = Self {
            slots: Vec::with_capacity(1),
            extra: None,
        };
        record.insert_field(Field::borrowed(key), value);
        record
    }
    pub fn len(&self) -> usize {
        self.slots.len()
    }
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }
    fn name(&self, key: u32) -> &str {
        name_in(&self.extra, key)
    }
    fn load(&self, val: u32) -> &Value {
        value_in(&self.extra, val)
    }
    fn extra_mut(&mut self) -> &mut Extra {
        self.extra.get_or_insert_with(Default::default)
    }
    fn has_index(&self) -> bool {
        self.extra.as_ref().is_some_and(|e| e.index.is_some())
    }
    fn tidy(&mut self) {
        if self.extra.as_ref().is_some_and(|e| e.idle()) {
            self.extra = None;
        }
    }
    fn encode(&mut self, field: Field) -> u32 {
        if let Some(code) = field.code() {
            return code;
        }
        let Field::Other(name) = field else {
            unreachable!("known field has a code")
        };
        let extra = self.extra_mut();
        let index = if let Some(index) = extra.free.pop() {
            extra.names[index as usize] = Some(name);
            index
        } else {
            // A record cannot approach this bound within the input byte cap.
            let index = u32::try_from(extra.names.len()).expect("too many property names");
            assert!(index < CUSTOM_KEY, "too many property names");
            extra.names.push(Some(name));
            index
        };
        CUSTOM_KEY | index
    }
    fn release_name(&mut self, code: u32) {
        if code & CUSTOM_KEY != 0 {
            let index = code & !CUSTOM_KEY;
            let extra = self.extra.as_mut().expect("custom key table");
            extra.names[index as usize] = None;
            extra.free.push(index);
            if extra.free.len() == extra.names.len() {
                extra.names = Vec::new();
                extra.free = Vec::new();
            }
        }
    }
    /// The slot value for `value`: its table id, or a local array/object.
    fn store(&mut self, value: Value) -> u32 {
        match vtab::intern(value) {
            Ok(id) => id.raw(),
            Err(value) => {
                vtab::note_spill();
                self.spill(value)
            }
        }
    }
    fn spill(&mut self, value: Value) -> u32 {
        let extra = self.extra_mut();
        let index = if let Some(index) = extra.complex_free.pop() {
            extra.complex[index as usize] = value;
            index
        } else {
            let index = u32::try_from(extra.complex.len()).expect("too many values");
            assert!(index < LOCAL, "too many values");
            extra.complex.push(value);
            index
        };
        LOCAL | index
    }
    /// The value of a slot that is being removed (a clone of a shared scalar).
    fn take(&mut self, val: u32) -> Value {
        if val & LOCAL != 0 {
            let index = val & !LOCAL;
            let extra = self.extra.as_mut().expect("complex values");
            extra.complex_free.push(index);
            std::mem::take(&mut extra.complex[index as usize])
        } else {
            vtab::resolve(VId::from_raw(val)).clone()
        }
    }
    /// Drop the value of a slot that is being removed.
    fn release(&mut self, val: u32) {
        if val & LOCAL != 0 {
            let index = val & !LOCAL;
            let extra = self.extra.as_mut().expect("complex values");
            extra.complex[index as usize] = Value::Null;
            extra.complex_free.push(index);
            if extra.complex_free.len() == extra.complex.len() {
                extra.complex = Vec::new();
                extra.complex_free = Vec::new();
            }
        }
    }
    fn position(&self, key: &str) -> Option<usize> {
        if let Some(index) = self.extra.as_ref().and_then(|e| e.index.as_ref()) {
            return index.get(key).copied();
        }
        if let Some(known) = Field::known_code(key) {
            self.slots.iter().position(|s| s.key == known)
        } else {
            self.slots
                .iter()
                .position(|s| s.key & CUSTOM_KEY != 0 && self.name(s.key) == key)
        }
    }
    fn field_position(&self, key: &Field) -> Option<usize> {
        // Streaming decode already classified this key. Avoid converting its
        // code to a string and classifying that string a second time.
        match key.code() {
            Some(code) if !self.has_index() => self.slots.iter().position(|s| s.key == code),
            _ => self.position(key.name()),
        }
    }
    fn refresh_index(&mut self) {
        let index = (self.slots.len() >= INDEX_THRESHOLD).then(|| {
            self.slots
                .iter()
                .enumerate()
                .map(|(i, s)| (self.name(s.key).into(), i))
                .collect()
        });
        if index.is_some() {
            self.extra_mut().index = index;
        } else if let Some(extra) = &mut self.extra {
            extra.index = None;
        }
    }
    /// Append a slot, growing small records one slot at a time: the records
    /// of a resident graph keep exactly the room they use.
    fn push_slot(&mut self, slot: Slot) {
        if self.slots.len() == self.slots.capacity() && self.slots.len() < INDEX_THRESHOLD {
            self.slots.reserve_exact(1);
        }
        self.slots.push(slot);
    }
    fn push_field(&mut self, key: Field, val: u32) {
        let at = self.slots.len();
        if let Some(index) = self.extra.as_mut().and_then(|e| e.index.as_mut()) {
            index.insert(key.name().into(), at);
        }
        let code = self.encode(key);
        self.push_slot(Slot { key: code, val });
        if !self.has_index() && self.slots.len() >= INDEX_THRESHOLD {
            self.refresh_index();
        }
    }
    /// Set `key` to an already stored slot value: in place when present, else
    /// appended. The replaced value is dropped.
    fn set_raw(&mut self, key: Field, val: u32) {
        if let Some(i) = self.field_position(&key) {
            let old = std::mem::replace(&mut self.slots[i].val, val);
            self.release(old);
        } else {
            self.push_field(key, val);
        }
    }
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.position(key).map(|i| self.load(self.slots[i].val))
    }
    /// A string value, borrowed from the shared table (no allocation).
    pub fn get_str(&self, key: &str) -> Option<&'static str> {
        let val = self.slots[self.position(key)?].val;
        (val & LOCAL == 0)
            .then(|| vtab::resolve(VId::from_raw(val)).as_str())
            .flatten()
    }
    /// The table id of a scalar value (`None` when absent, array or object).
    pub fn get_id(&self, key: &str) -> Option<VId> {
        let val = self.slots[self.position(key)?].val;
        (val & LOCAL == 0).then(|| VId::from_raw(val))
    }
    /// A mutable view of the value; a change is written back when the guard
    /// drops.
    pub fn get_mut(&mut self, key: &str) -> Option<ValueMut<'_>> {
        let i = self.position(key)?;
        Some(ValueMut::new(self, i))
    }
    pub fn contains_key(&self, key: &str) -> bool {
        self.position(key).is_some()
    }
    pub fn insert(&mut self, key: String, value: Value) -> Option<Value> {
        let key = Field::new(key);
        if let Some(i) = self.field_position(&key) {
            let new = self.store(value);
            let old = std::mem::replace(&mut self.slots[i].val, new);
            return Some(self.take(old));
        }
        let val = self.store(value);
        self.push_field(key, val);
        None
    }
    /// `insert` without returning the replaced value.
    pub(crate) fn insert_field(&mut self, key: Field, value: Value) {
        let val = self.store(value);
        self.set_raw(key, val);
    }
    /// Set `key` to an interned scalar, with no clone and no lookup.
    pub fn set_id(&mut self, key: &str, id: VId) {
        self.set_raw(Field::borrowed(key), id.raw());
    }
    pub fn remove(&mut self, key: &str) -> Option<Value> {
        let slot = self.swap_remove_slot(key)?;
        let value = self.take(slot.val);
        self.tidy();
        Some(value)
    }
    /// `remove` without materializing the value; whether `key` was present.
    pub fn discard(&mut self, key: &str) -> bool {
        let Some(slot) = self.swap_remove_slot(key) else {
            return false;
        };
        self.release(slot.val);
        self.tidy();
        true
    }
    fn swap_remove_slot(&mut self, key: &str) -> Option<Slot> {
        let i = self.position(key)?;
        let slot = self.slots.swap_remove(i);
        if self.has_index() {
            let swapped = self
                .slots
                .get(i)
                .map(|s| Box::<str>::from(self.name(s.key)));
            let len = self.slots.len();
            let extra = self.extra.as_mut().expect("index");
            let index = extra.index.as_mut().expect("index");
            index.remove(key);
            if let Some(key) = swapped {
                index.insert(key, i);
            }
            if len < INDEX_THRESHOLD {
                extra.index = None;
            }
        }
        self.release_name(slot.key);
        Some(slot)
    }
    fn shift_remove_slot(&mut self, key: &str) -> Option<Slot> {
        let i = self.position(key)?;
        let slot = self.slots.remove(i);
        self.release_name(slot.key);
        if self.has_index() {
            self.refresh_index();
        }
        Some(slot)
    }
    pub fn shift_remove(&mut self, key: &str) -> Option<Value> {
        let slot = self.shift_remove_slot(key)?;
        let value = self.take(slot.val);
        self.tidy();
        Some(value)
    }
    pub(crate) fn shift_insert(
        &mut self,
        position: usize,
        key: String,
        value: Value,
    ) -> Option<Value> {
        assert!(
            position <= self.slots.len(),
            "attribute insertion index out of bounds"
        );
        let previous = self.shift_remove_slot(&key).map(|slot| self.take(slot.val));
        let code = self.encode(Field::new(key));
        let val = self.store(value);
        if self.slots.len() == self.slots.capacity() && self.slots.len() < INDEX_THRESHOLD {
            self.slots.reserve_exact(1);
        }
        self.slots
            .insert(position.min(self.slots.len()), Slot { key: code, val });
        if self.slots.len() >= INDEX_THRESHOLD {
            self.refresh_index();
        }
        self.tidy();
        previous
    }
    pub fn entry(&mut self, key: impl Into<String>) -> Entry<'_> {
        Entry {
            attributes: self,
            key: key.into(),
        }
    }
    pub fn retain(&mut self, mut predicate: impl FnMut(&str, &Value) -> bool) {
        let mut released = vec![];
        let Self { slots, extra } = self;
        slots.retain(|slot| {
            let keep = predicate(name_in(extra, slot.key), value_in(extra, slot.val));
            if !keep {
                released.push(*slot);
            }
            keep
        });
        for slot in released {
            self.release_name(slot.key);
            self.release(slot.val);
        }
        if self.has_index() {
            self.refresh_index();
        }
        self.tidy();
    }
    pub fn iter(&self) -> impl ExactSizeIterator<Item = (&str, &Value)> {
        self.slots
            .iter()
            .map(|s| (self.name(s.key), self.load(s.val)))
    }
    pub fn keys(&self) -> impl ExactSizeIterator<Item = &str> {
        self.slots.iter().map(|s| self.name(s.key))
    }
    pub fn values(&self) -> impl ExactSizeIterator<Item = &Value> {
        self.slots.iter().map(|s| self.load(s.val))
    }
    pub fn compact(&mut self) {
        self.slots.shrink_to_fit();
        if let Some(extra) = &mut self.extra {
            if let Some(index) = &mut extra.index {
                index.shrink_to_fit();
            }
            extra.names.shrink_to_fit();
            extra.free.shrink_to_fit();
            extra.complex.shrink_to_fit();
            extra.complex_free.shrink_to_fit();
        }
    }
    pub fn to_map(&self) -> Map<String, Value> {
        self.iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }
    /// Merge `other` in, as successive `insert`s; shared scalars move by id.
    pub fn extend_record(&mut self, other: Self) {
        let Self { slots, mut extra } = other;
        for slot in slots {
            let field = take_field(&mut extra, slot.key);
            let val = if slot.val & LOCAL != 0 {
                let value = std::mem::take(
                    &mut extra.as_mut().expect("complex values").complex
                        [(slot.val & !LOCAL) as usize],
                );
                self.spill(value)
            } else {
                slot.val
            };
            self.set_raw(field, val);
        }
    }
    /// Query snapshot encoding (`crate::snapshot`), in stored order.
    pub(crate) fn write_snapshot<'a>(&'a self, w: &mut crate::snapshot::Writer<'a>) {
        w.usize(self.slots.len());
        for (key, value) in self.iter() {
            w.key(key);
            w.value(value);
        }
    }
    pub(crate) fn read_snapshot(r: &mut crate::snapshot::Reader<'_, '_>) -> crate::Result<Self> {
        let n = r.len(2)?;
        let mut record = Self::new();
        record.slots.reserve_exact(n);
        for _ in 0..n {
            let field = r.key()?;
            record.insert_field(field, r.value()?);
        }
        Ok(record)
    }
}
fn take_field(extra: &mut Option<Box<Extra>>, code: u32) -> Field {
    Field::from_code(code).unwrap_or_else(|| {
        Field::Other(
            extra.as_mut().expect("custom key table").names[(code & !CUSTOM_KEY) as usize]
                .take()
                .expect("live custom key"),
        )
    })
}
/// A mutable view of one attribute value (`get_mut`, `entry`). The value is
/// written back, re-interned, when the guard drops after a change.
pub struct ValueMut<'a> {
    attributes: &'a mut Attributes,
    slot: usize,
    value: Value,
    local: bool,
    dirty: bool,
}
impl<'a> ValueMut<'a> {
    fn new(attributes: &'a mut Attributes, slot: usize) -> Self {
        let val = attributes.slots[slot].val;
        let value = attributes.take(val);
        Self {
            attributes,
            slot,
            value,
            local: val & LOCAL != 0,
            dirty: false,
        }
    }
}
impl std::ops::Deref for ValueMut<'_> {
    type Target = Value;
    fn deref(&self) -> &Value {
        &self.value
    }
}
impl std::ops::DerefMut for ValueMut<'_> {
    fn deref_mut(&mut self) -> &mut Value {
        self.dirty = true;
        &mut self.value
    }
}
impl Drop for ValueMut<'_> {
    fn drop(&mut self) {
        if !(self.dirty || self.local) {
            return;
        }
        if self.dirty {
            vtab::note_write_back();
        }
        let value = std::mem::take(&mut self.value);
        let val = match vtab::intern(value) {
            Ok(id) => id.raw(),
            Err(value) => self.attributes.spill(value),
        };
        self.attributes.slots[self.slot].val = val;
        self.attributes.tidy();
    }
}
impl fmt::Debug for ValueMut<'_> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.value.fmt(f)
    }
}
pub struct Entry<'a> {
    attributes: &'a mut Attributes,
    key: String,
}
impl<'a> Entry<'a> {
    pub fn or_insert(self, value: Value) -> ValueMut<'a> {
        self.or_insert_with(|| value)
    }
    pub fn or_insert_with(self, f: impl FnOnce() -> Value) -> ValueMut<'a> {
        let i = if let Some(i) = self.attributes.position(&self.key) {
            i
        } else {
            self.attributes.insert_field(Field::new(self.key), f());
            self.attributes.len() - 1
        };
        ValueMut::new(self.attributes, i)
    }
}
impl From<Map<String, Value>> for Attributes {
    fn from(map: Map<String, Value>) -> Self {
        let mut attributes = Self::new();
        attributes.slots.reserve_exact(map.len());
        for (key, value) in map {
            attributes.insert_field(Field::new(key), value);
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
            self.insert_field(Field::new(k), v);
        }
    }
}
impl IntoIterator for Attributes {
    type Item = (String, Value);
    type IntoIter = IntoIter;
    fn into_iter(self) -> Self::IntoIter {
        IntoIter(IntoFields {
            slots: self.slots.into_iter(),
            extra: self.extra,
        })
    }
}
struct IntoFields {
    slots: std::vec::IntoIter<Slot>,
    extra: Option<Box<Extra>>,
}
impl Iterator for IntoFields {
    type Item = (Field, Value);
    fn next(&mut self) -> Option<Self::Item> {
        let slot = self.slots.next()?;
        let field = take_field(&mut self.extra, slot.key);
        let value = if slot.val & LOCAL != 0 {
            std::mem::take(
                &mut self.extra.as_mut().expect("complex values").complex
                    [(slot.val & !LOCAL) as usize],
            )
        } else {
            vtab::resolve(VId::from_raw(slot.val)).clone()
        };
        Some((field, value))
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.slots.size_hint()
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
        self.len() == other.len()
            && self.slots.iter().all(|slot| {
                let name = self.name(slot.key);
                other.position(name).is_some_and(|j| {
                    let theirs = other.slots[j].val;
                    // Equal ids are equal values; otherwise compare the values.
                    (slot.val & LOCAL == 0 && slot.val == theirs)
                        || self.load(slot.val) == other.load(theirs)
                })
            })
    }
}
impl fmt::Debug for Attributes {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_map().entries(self.iter()).finish()
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
        assert_eq!(record.extra.as_ref().unwrap().names.len(), 3);
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
        assert!(record.extra.is_none());
    }
    #[test]
    fn edge_direction_markers_need_no_custom_name_table() {
        let map = serde_json::from_value::<Map<String, Value>>(json!({
            "relation":"calls", "_src":"a", "weight":1.0, "_tgt":"b", "norm_label":"x", "custom":[1]
        }))
        .unwrap();
        let mut record = Attributes::from(map.clone());
        // Only the unrecognized key takes a custom-name slot.
        assert_eq!(record.extra.as_ref().unwrap().names.len(), 1);
        assert_eq!(
            serde_json::to_string(&record).unwrap(),
            serde_json::to_string(&map).unwrap()
        );
        assert_eq!(record.get("_src"), Some(&json!("a")));
        assert_eq!(record.shift_remove("_tgt"), Some(json!("b")));
        record.retain(|name, _| name != "custom");
        assert!(record.extra.is_none());
        let keys = record.keys().collect::<Vec<_>>();
        assert_eq!(keys, ["relation", "_src", "weight", "norm_label"]);
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
        assert!(!record.has_index());
    }

    fn sample_value(rng: &mut impl rand::Rng) -> Value {
        const TEXTS: &[&str] = &[
            "1",
            "1.0",
            "-0.0",
            "0",
            "1e-7",
            "0.55",
            "18446744073709551616",
            "-3",
        ];
        match rng.gen_range(0..9) {
            0 => Value::Null,
            1 => json!(rng.gen_bool(0.5)),
            2 | 3 => serde_json::from_str(TEXTS[rng.gen_range(0..TEXTS.len())]).unwrap(),
            4 => json!(TEXTS[rng.gen_range(0..TEXTS.len())]),
            5 => json!(["é", rng.gen_range(0..3), {"k": null}]),
            6 => json!({"nested": {"x": rng.gen_range(0..3)}}),
            _ => json!(format!("s-{}-日本", rng.gen_range(0..20))),
        }
    }
    fn sample_key(rng: &mut impl rand::Rng, wide: bool) -> String {
        const KEYS: &[&str] = &[
            "label",
            "source_file",
            "_src",
            "_tgt",
            "relation",
            "weight",
            "id",
            "callee",
            "custom-a",
            "custom-b",
            "ключ",
            "x y",
            "",
        ];
        if wide && rng.gen_bool(0.5) {
            format!("wide-{}", rng.gen_range(0..90))
        } else {
            KEYS[rng.gen_range(0..KEYS.len())].to_string()
        }
    }

    /// Random edit sequences against serde_json's preserve_order Map, compared
    /// byte for byte after every step (small records and records past the
    /// index threshold).
    #[test]
    fn random_edits_match_serde_map_byte_for_byte() {
        use rand::{Rng, SeedableRng};
        for seed in 0..60u64 {
            let mut rng = rand_xorshift::XorShiftRng::seed_from_u64(seed);
            let wide = seed % 3 == 0;
            let mut reference = Map::new();
            let mut record = Attributes::new();
            for step in 0..400 {
                let key = sample_key(&mut rng, wide);
                match rng.gen_range(0..12) {
                    0..=3 => {
                        let value = sample_value(&mut rng);
                        assert_eq!(
                            record.insert(key.clone(), value.clone()),
                            reference.insert(key, value)
                        );
                    }
                    4 => assert_eq!(record.remove(&key), reference.swap_remove(&key)),
                    5 => assert_eq!(record.discard(&key), reference.swap_remove(&key).is_some()),
                    6 => assert_eq!(record.shift_remove(&key), reference.shift_remove(&key)),
                    7 => {
                        let value = sample_value(&mut rng);
                        let at = rng.gen_range(0..=record.len());
                        let previous = reference.shift_remove(&key);
                        let at_ref = at.min(reference.len());
                        reference.shift_insert(at_ref, key.clone(), value.clone());
                        assert_eq!(record.shift_insert(at, key, value), previous);
                    }
                    8 => {
                        let other: Vec<(String, Value)> = (0..rng.gen_range(0..4))
                            .map(|_| (sample_key(&mut rng, wide), sample_value(&mut rng)))
                            .collect();
                        if rng.gen_bool(0.5) {
                            record.extend(other.clone());
                        } else {
                            record.extend_record(other.iter().cloned().collect());
                        }
                        reference.extend(other);
                    }
                    9 => {
                        let value = sample_value(&mut rng);
                        if let Some(mut slot) = record.get_mut(&key) {
                            *slot = value.clone();
                        }
                        if let Some(slot) = reference.get_mut(&key) {
                            *slot = value;
                        }
                    }
                    10 => {
                        let value = sample_value(&mut rng);
                        let got = record.entry(key.clone()).or_insert(value.clone()).clone();
                        let want = reference.entry(key).or_insert(value).clone();
                        assert_eq!(got, want);
                    }
                    _ => {
                        let cut = rng.gen_range(0..4);
                        record.retain(|k, v| k.len() % 4 != cut || v.is_null());
                        reference.retain(|k, v| k.len() % 4 != cut || v.is_null());
                    }
                }
                assert_eq!(
                    serde_json::to_string(&record).unwrap(),
                    serde_json::to_string(&reference).unwrap(),
                    "seed {seed} step {step}"
                );
                assert_eq!(record.len(), reference.len());
                for (k, v) in &reference {
                    assert_eq!(record.get(k), Some(v));
                    assert_eq!(record[k.as_str()], *v);
                    assert_eq!(record.get_str(k), v.as_str());
                }
            }
            let copy = record.clone();
            assert_eq!(copy, record);
            assert_eq!(record.to_map(), reference);
            let round: Attributes =
                serde_json::from_str(&serde_json::to_string(&record).unwrap()).unwrap();
            assert_eq!(round, record);
            assert_eq!(Map::from(record), reference);
            assert_eq!(
                serde_json::to_string(&Attributes::from(reference.clone())).unwrap(),
                serde_json::to_string(&reference).unwrap()
            );
        }
    }

    #[test]
    fn number_texts_and_kinds_round_trip_exactly() {
        let text = r#"{"a":1,"b":1.0,"c":-0.0,"d":1e-7,"e":0.55,"f":"1.0","g":"1","h":1E-7,"i":100000000000000000000001,"j":"日本","k":"é","l":null,"m":false}"#;
        let record: Attributes = serde_json::from_str(text).unwrap();
        let reference: Map<String, Value> = serde_json::from_str(text).unwrap();
        assert_eq!(
            serde_json::to_string(&record).unwrap(),
            serde_json::to_string(&reference).unwrap()
        );
        assert_ne!(record.get_id("a"), record.get_id("b"));
        assert_ne!(record.get_id("b"), record.get_id("f"));
        assert_ne!(record.get_id("a"), record.get_id("g"));
        // Ids are equal exactly when the written texts are.
        for (a, va) in reference.iter() {
            for (b, vb) in reference.iter() {
                let same = serde_json::to_string(va).unwrap() == serde_json::to_string(vb).unwrap();
                assert_eq!(same, record.get_id(a) == record.get_id(b), "{a} {b}");
            }
        }
        let mut record = Attributes::single("weight", json!(0.55));
        assert_eq!(
            serde_json::to_string(&record).unwrap(),
            r#"{"weight":0.55}"#
        );
        record.set_id("_src", vtab::intern_str("node-ü"));
        assert_eq!(record.get_str("_src"), Some("node-ü"));
        assert_eq!(record.get_id("_src"), Some(vtab::intern_str("node-ü")));
    }

    #[test]
    fn write_back_guard_keeps_position_and_complex_values() {
        let mut record = Attributes::from(
            serde_json::from_value::<Map<String, Value>>(json!({
                "a": [1, 2], "b": "x", "custom": {"k": 1}
            }))
            .unwrap(),
        );
        // An unchanged guard writes nothing back and loses nothing.
        drop(record.get_mut("a"));
        drop(record.get_mut("b"));
        record
            .get_mut("a")
            .unwrap()
            .as_array_mut()
            .unwrap()
            .push(json!(3));
        *record.get_mut("b").unwrap() = json!({"now": "complex"});
        *record.get_mut("custom").unwrap() = json!("now scalar");
        assert_eq!(
            serde_json::to_string(&record).unwrap(),
            r#"{"a":[1,2,3],"b":{"now":"complex"},"custom":"now scalar"}"#
        );
        record.retain(|_, v| v.is_string());
        assert_eq!(
            serde_json::to_string(&record).unwrap(),
            r#"{"custom":"now scalar"}"#
        );
        // Only the custom key name is left beside the slots.
        let extra = record.extra.as_ref().unwrap();
        assert!(extra.complex.is_empty() && extra.complex_free.is_empty());
        record.shift_remove("custom");
        assert!(record.extra.is_none());
    }

    #[test]
    fn repeated_loads_of_the_same_values_do_not_grow_the_table() {
        let text = r#"{"label":"vtab-growth-label","source_file":"vtab/growth.go","weight":1.0,"custom":"vtab-growth-custom","n":7.25e3}"#;
        let first: Attributes = serde_json::from_str(text).unwrap();
        let ids: Vec<_> = ["label", "source_file", "weight", "custom", "n"]
            .iter()
            .map(|k| first.get_id(k).unwrap())
            .collect();
        for _ in 0..100 {
            let again: Attributes = serde_json::from_str(text).unwrap();
            let again_ids: Vec<_> = ["label", "source_file", "weight", "custom", "n"]
                .iter()
                .map(|k| again.get_id(k).unwrap())
                .collect();
            assert_eq!(again_ids, ids);
        }
    }
}
