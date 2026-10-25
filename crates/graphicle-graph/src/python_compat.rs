// Adapted from Graphify 0.9.95 (92741d16), Apache-2.0; see NOTICE.md.
// Upstream: graphify/analyze.py, graphify/validate.py
//! Small native value helpers shared by translated modules, not a Python runtime.
use crate::python_repr_data::PRINTABLE;
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ScalarKey {
    Null,
    Integer(i128),
    Float(u64),
    Text(String),
}

pub fn scalar_key(value: &Value) -> Option<ScalarKey> {
    match value {
        Value::Null => Some(ScalarKey::Null),
        Value::Bool(value) => Some(ScalarKey::Integer(i128::from(*value))),
        Value::String(value) => Some(ScalarKey::Text(value.clone())),
        Value::Number(value) => {
            if let Some(value) = value.as_i64() {
                return Some(ScalarKey::Integer(value.into()));
            }
            if let Some(value) = value.as_u64() {
                return Some(ScalarKey::Integer(value.into()));
            }
            let value = value.as_f64()?;
            if value.fract() == 0.0 && value >= i128::MIN as f64 && value < i128::MAX as f64 {
                Some(ScalarKey::Integer(value as i128))
            } else {
                Some(ScalarKey::Float(value.to_bits()))
            }
        }
        _ => None,
    }
}

pub fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
        Value::Number(value) if value.is_f64() => "float",
        Value::Number(_) => "int",
    }
}

pub fn repr_string(value: &str) -> String {
    let quote = if value.contains('\'') && !value.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::from(quote);
    for c in value.chars() {
        if c == quote || c == '\\' {
            out.push('\\');
            out.push(c);
        } else {
            match c {
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                _ => {
                    let cp = c as u32;
                    let at = PRINTABLE.partition_point(|&(start, _)| start <= cp);
                    if at > 0 && cp <= PRINTABLE[at - 1].1 {
                        out.push(c);
                    } else if cp <= 0xff {
                        out.push_str(&format!("\\x{cp:02x}"));
                    } else if cp <= 0xffff {
                        out.push_str(&format!("\\u{cp:04x}"));
                    } else {
                        out.push_str(&format!("\\U{cp:08x}"));
                    }
                }
            }
        }
    }
    out.push(quote);
    out
}

fn float_repr(value: f64) -> String {
    let formatted = format!("{value:?}");
    if let Some((mantissa, exponent)) = formatted.split_once('e') {
        let exponent: i32 = exponent.parse().expect("Rust float exponent");
        format!(
            "{mantissa}e{}{abs:02}",
            if exponent < 0 { '-' } else { '+' },
            abs = exponent.unsigned_abs()
        )
    } else {
        formatted
    }
}

pub fn repr(value: &Value) -> String {
    match value {
        Value::Null => "None".into(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::String(value) => repr_string(value),
        Value::Number(value) if value.is_f64() => float_repr(value.as_f64().unwrap()),
        Value::Number(value) => value.to_string(),
        Value::Array(values) => format!(
            "[{}]",
            values.iter().map(repr).collect::<Vec<_>>().join(", ")
        ),
        Value::Object(values) => format!(
            "{{{}}}",
            values
                .iter()
                .map(|(key, value)| format!("{}: {}", repr_string(key), repr(value)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

pub fn string(value: &Value) -> String {
    value
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| repr(value))
}

/// The iteration order of the CPython 3.12 `set` built by adding `values` in
/// turn (a set comprehension), for ints and `None`: the members of the
/// upstream sets whose order reaches an output (the community ids a bridge
/// question lists). Ints hash to themselves below 2**61 - 1 and `None` has a
/// constant hash since 3.12, so the order is fixed by the insertion sequence.
pub fn int_set_order(values: impl IntoIterator<Item = Option<i64>>) -> Vec<Option<i64>> {
    fn hash(value: Option<i64>) -> i64 {
        match value {
            // Py_None's hash on 64-bit builds.
            None => 0xFCA8_6420,
            Some(v) => {
                // long_hash: the magnitude modulo 2**61 - 1, signed; -1 is
                // reserved and becomes -2.
                let modulus: i128 = (1 << 61) - 1;
                let magnitude = i128::from(v).abs() % modulus;
                let signed = if v < 0 { -magnitude } else { magnitude };
                if signed == -1 { -2 } else { signed as i64 }
            }
        }
    }
    set_order(values.into_iter().map(|v| (hash(v), v)))
}

/// CPython's `hash(str)` with hash randomization disabled, as under
/// `PYTHONHASHSEED=0`, which upstream's CLI sets for `extract`, `update`,
/// `cluster-only` and `label` (#3641): SipHash-1-3 with a zero key over the
/// string's PEP 393 buffer (Latin-1, UCS-2 or UCS-4, little-endian); -1 is
/// reserved and becomes -2.
pub fn str_hash_seed0(text: &str) -> i64 {
    let widest = text.chars().map(u32::from).max().unwrap_or(0);
    let mut bytes = Vec::with_capacity(text.len() * 4);
    for c in text.chars() {
        let code = u32::from(c);
        match widest {
            0..=0xff => bytes.push(code as u8),
            0x100..=0xffff => bytes.extend_from_slice(&(code as u16).to_le_bytes()),
            _ => bytes.extend_from_slice(&code.to_le_bytes()),
        }
    }
    if bytes.is_empty() {
        return 0;
    }
    match siphash13(0, 0, &bytes) as i64 {
        -1 => -2,
        hash => hash,
    }
}

/// SipHash-1-3 as CPython's Python/pyhash.c computes it.
fn siphash13(k0: u64, k1: u64, data: &[u8]) -> u64 {
    fn round(v: &mut [u64; 4]) {
        v[0] = v[0].wrapping_add(v[1]);
        v[2] = v[2].wrapping_add(v[3]);
        v[1] = v[1].rotate_left(13) ^ v[0];
        v[3] = v[3].rotate_left(16) ^ v[2];
        v[0] = v[0].rotate_left(32);
        v[2] = v[2].wrapping_add(v[1]);
        v[0] = v[0].wrapping_add(v[3]);
        v[1] = v[1].rotate_left(17) ^ v[2];
        v[3] = v[3].rotate_left(21) ^ v[0];
        v[2] = v[2].rotate_left(32);
    }
    let mut v = [
        k0 ^ 0x736f_6d65_7073_6575,
        k1 ^ 0x646f_7261_6e64_6f6d,
        k0 ^ 0x6c79_6765_6e65_7261,
        k1 ^ 0x7465_6462_7974_6573,
    ];
    let chunks = data.chunks_exact(8);
    let tail = chunks.remainder();
    for chunk in chunks {
        let m = u64::from_le_bytes(chunk.try_into().expect("an 8-byte chunk"));
        v[3] ^= m;
        round(&mut v);
        v[0] ^= m;
    }
    let mut last = (data.len() as u64) << 56;
    for (i, &byte) in tail.iter().enumerate() {
        last |= u64::from(byte) << (8 * i);
    }
    v[3] ^= last;
    round(&mut v);
    v[0] ^= last;
    v[2] ^= 0xff;
    round(&mut v);
    round(&mut v);
    round(&mut v);
    (v[0] ^ v[1]) ^ (v[2] ^ v[3])
}

/// The iteration order of the CPython 3.12 `set` built by adding `members`
/// in turn (a comprehension, `set(iterable)` of a non-set, `add` or
/// `update`), given each member's `hash()`; equal members are added once.
/// Replays `set_add_entry`, `set_table_resize` and `set_insert_clean`
/// (Objects/setobject.c); nothing is removed, so there are no dummies.
pub fn set_order<T: PartialEq>(members: impl IntoIterator<Item = (i64, T)>) -> Vec<T> {
    const MIN_SIZE: usize = 8; // PySet_MINSIZE
    const LINEAR_PROBES: usize = 9;
    const PERTURB_SHIFT: u32 = 5;
    fn next_start(i: usize, perturb: &mut u64, mask: usize) -> usize {
        *perturb >>= PERTURB_SHIFT;
        (i.wrapping_mul(5)
            .wrapping_add(1)
            .wrapping_add(*perturb as usize))
            & mask
    }
    // The slots one probe visits: ten in a row where the table has room.
    fn probe(i: usize, mask: usize) -> std::ops::RangeInclusive<usize> {
        i..=if i + LINEAR_PROBES <= mask {
            i + LINEAR_PROBES
        } else {
            i
        }
    }
    let mut table: Vec<Option<(u64, T)>> = (0..MIN_SIZE).map(|_| None).collect();
    let mut used = 0usize;
    for (hash, member) in members {
        // Probing uses the hash as a size_t.
        let hash = hash as u64;
        let mask = table.len() - 1;
        let mut perturb = hash;
        let mut i = (hash as usize) & mask;
        // set_add_entry: stop at the first empty slot, or at an equal member.
        let slot = loop {
            let hit = probe(i, mask).find_map(|j| match &table[j] {
                None => Some(Some(j)),
                Some((h, m)) if *h == hash && *m == member => Some(None),
                Some(_) => None,
            });
            if let Some(hit) = hit {
                break hit;
            }
            i = next_start(i, &mut perturb, mask);
        };
        let Some(slot) = slot else { continue };
        table[slot] = Some((hash, member));
        used += 1;
        if used * 5 >= mask * 3 {
            let min_used = if used > 50_000 { used * 2 } else { used * 4 };
            let mut size = MIN_SIZE;
            while size <= min_used {
                size <<= 1;
            }
            let old = std::mem::replace(&mut table, (0..size).map(|_| None).collect());
            let mask = size - 1;
            // set_insert_clean, in the old table's slot order.
            for (hash, member) in old.into_iter().flatten() {
                let mut perturb = hash;
                let mut i = (hash as usize) & mask;
                let slot = loop {
                    if let Some(j) = probe(i, mask).find(|&j| table[j].is_none()) {
                        break j;
                    }
                    i = next_start(i, &mut perturb, mask);
                };
                table[slot] = Some((hash, member));
            }
        }
    }
    table
        .into_iter()
        .flatten()
        .map(|(_, member)| member)
        .collect()
}

#[cfg(test)]
mod int_set_order_tests {
    use super::int_set_order;

    #[test]
    fn matches_cpython_3_12_orders() {
        // (values, list({x for x in values})) from CPython 3.12.13; null is None.
        let cases = [
            ("[]", "[]"),
            ("[3, 1, 2]", "[1, 2, 3]"),
            ("[9, 1, 17, null, 1]", "[null, 9, 1, 17]"),
            ("[0, 8, 16, 24, 32, 1, 9]", "[0, 32, 1, 8, 9, 16, 24]"),
            (
                "[37, 5, 69, 101, 133, 2, 34, 66, null, 98, 130, 7, 39, 71, -1, 103, 135, 1000003, 12, 44]",
                "[2, 130, 5, 133, 7, 135, 12, null, 34, 37, 39, 44, 66, 1000003, 69, 71, 98, 101, 103, -1]",
            ),
        ];
        for (values, expected) in cases {
            let values: Vec<Option<i64>> = serde_json::from_str(values).unwrap();
            let expected: Vec<Option<i64>> = serde_json::from_str(expected).unwrap();
            assert_eq!(
                int_set_order(values.iter().copied()),
                expected,
                "{values:?}"
            );
        }
    }

    /// Broad comparison against CPython: GRAPHICLE_SET_ORDER_CASES names a JSON
    /// file of `[values, list(set(values))]` pairs (null for None) written by
    /// the reference interpreter.
    #[test]
    #[ignore]
    fn matches_cpython_cases_file() {
        type Members = Vec<Option<i64>>;
        let path = std::env::var("GRAPHICLE_SET_ORDER_CASES").expect("GRAPHICLE_SET_ORDER_CASES");
        let cases: Vec<(Members, Members)> =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        for (values, expected) in &cases {
            assert_eq!(
                &int_set_order(values.iter().copied()),
                expected,
                "{values:?}"
            );
        }
        println!("{} cases", cases.len());
    }
}

#[cfg(test)]
mod str_hash_tests {
    use super::{set_order, str_hash_seed0};

    #[test]
    fn matches_cpython_3_12_hashes_without_randomization() {
        // `hash(s)` from CPython 3.12.13 under PYTHONHASHSEED=0: ASCII of every
        // tail length around the 8-byte block, Latin-1, UCS-2 and UCS-4
        // buffers, and a NUL.
        let cases = [
            ("", 0),
            ("a", 4644417185603328019),
            ("ab", 6148830537548944441),
            ("abc", -4594863902769663758),
            ("abcdefg", 7904145750247929094),
            ("abcdefgh", 4574395652268504554),
            ("abcdefghi", -532774252720507163),
            ("packages/excalidraw/scene/index.ts", -2788797300116589570),
            ("é", 6047309291227476195),
            ("café", 137524001917817222),
            ("日本", 6243316497235261705),
            ("a日", -1342748363036218313),
            ("🧪", -4123222599049301249),
            ("x🧪", 3850289482547715647),
            ("Große", 7067549704707432155),
            ("\0", 7541581120933061747),
            ("ÿ", 3476900567878811119),
            ("Ā", 75343234424780393),
            ("\u{ffff}", 2147057924765956750),
            ("\u{10000}", 8244560961346649228),
        ];
        for (text, expected) in cases {
            assert_eq!(str_hash_seed0(text), expected, "{text:?}");
        }
    }

    #[test]
    fn string_sets_iterate_in_cpython_order() {
        // list({p for p in values}) from CPython 3.12.13 under PYTHONHASHSEED=0.
        let values = [
            "café.tsx",
            "日本/models/views.c",
            "index/Große",
            "packages/core/core/packages.tsx",
            "main.tsx",
            "excalidraw/excalidraw/index.c",
            "main/🧪.c",
            "models/views/index/utils.py",
            "🧪/café",
            "excalidraw/main/src/core",
            "app/main",
            "core/excalidraw/element/café.tsx",
        ];
        let expected = [
            "main.tsx",
            "packages/core/core/packages.tsx",
            "日本/models/views.c",
            "app/main",
            "excalidraw/excalidraw/index.c",
            "core/excalidraw/element/café.tsx",
            "🧪/café",
            "excalidraw/main/src/core",
            "index/Große",
            "café.tsx",
            "main/🧪.c",
            "models/views/index/utils.py",
        ];
        let order = set_order(values.iter().map(|v| (str_hash_seed0(v), *v)));
        assert_eq!(order, expected);
    }

    /// Broad comparison: GRAPHICLE_STR_SET_CASES names a JSON file of
    /// `[values, list(set(values))]` string pairs written by CPython 3.12
    /// under PYTHONHASHSEED=0.
    #[test]
    #[ignore]
    fn matches_cpython_string_cases_file() {
        type Members = Vec<String>;
        let path = std::env::var("GRAPHICLE_STR_SET_CASES").expect("GRAPHICLE_STR_SET_CASES");
        let cases: Vec<(Members, Members)> =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        for (values, expected) in &cases {
            let order = set_order(values.iter().map(|v| (str_hash_seed0(v), v.clone())));
            assert_eq!(&order, expected, "{values:?}");
        }
        println!("{} cases", cases.len());
    }
}
