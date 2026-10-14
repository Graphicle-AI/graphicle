//! Unsupported `rank` options and option values.
//!
//! [`rank`](crate::retrieval::rank) ignores keys it does not read, so a
//! request that names one of these options is answered here instead of being
//! silently ignored:
//! - an option sent at an inert value (`null`, `false` or `0`, or what its
//!   entry also allows) is accepted and has no effect;
//! - any other value is refused: "`<option>` is not supported by Graphicle"
//!   (the graph engine's [`graphicle::archived::message`]);
//! - an unsupported value of an option that stays (one mode of several) is
//!   refused the same way.
//!
//! The service refuses the same options on its own surfaces, with the same
//! message, before it calls `rank` (graphicle-runtime's `archived` module).
use crate::{Error, Result};
use serde_json::Value;

/// An unsupported `rank` option.
pub struct ArchivedOption {
    /// The option's name as callers send it.
    pub option: &'static str,
    /// The values that are still accepted, with no effect (see [`off`]).
    pub inert: fn(&Value) -> bool,
    /// The entry's unit name.
    pub unit: &'static str,
}

/// An unsupported value of a `rank` option that stays.
pub struct ArchivedValue {
    pub option: &'static str,
    pub value: &'static str,
    pub unit: &'static str,
}

/// Unsupported `rank` options.
pub const OPTIONS: &[ArchivedOption] = &[
    // Hindsight's entity link expansion: `true` or an object.
    ArchivedOption {
        option: "graph",
        inert: off,
        unit: "retrieval-graph-expansion",
    },
    // Hindsight's temporal arm, `spread` and the recency rules: an object
    // (`asked`, `arm`, `spread`, `recency`).
    ArchivedOption {
        option: "temporal",
        inert: off,
        unit: "retrieval-temporal-arm",
    },
    // Memora's fact index: `true` or `"attributed"`.
    ArchivedOption {
        option: "abstraction",
        inert: off,
        unit: "retrieval-abstraction",
    },
    // Mem0's entity boost: `true` or `{"weight"}`.
    ArchivedOption {
        option: "entity",
        inert: off,
        unit: "retrieval-entity-boost",
    },
    // SmartSearch's feedback hop: an object (`{}` took its defaults).
    ArchivedOption {
        option: "prf",
        inert: off,
        unit: "retrieval-prf-depth-gate",
    },
    // The deeper read of weak and counting questions: `true`.
    ArchivedOption {
        option: "adaptive_depth",
        inert: off,
        unit: "retrieval-prf-depth-gate",
    },
    // SmartSearch's grep arm: `true` or `{"boundary"}`.
    ArchivedOption {
        option: "grep",
        inert: off,
        unit: "retrieval-grep-arm",
    },
    // Learned hints, `{identity: weight}`; the service sent `{}` on every
    // call while no hint was promoted.
    ArchivedOption {
        option: "boosts",
        inert: empty_or_off,
        unit: "learning-and-hints",
    },
];

/// Unsupported values of `rank` options that stay.
pub const VALUES: &[ArchivedValue] = &[
    // The overview mode boosted `context`-kind memories, the community
    // reports; the other modes stay.
    ArchivedValue {
        option: "mode",
        value: "overview",
        unit: "community-reports",
    },
];

/// The default inert test: `null`, `false` or zero (absent is always inert).
pub fn off(value: &Value) -> bool {
    value.is_null() || *value == Value::Bool(false) || value.as_f64() == Some(0.0)
}

/// Inert for an option whose old default was an empty map (`boosts`).
pub fn empty_or_off(value: &Value) -> bool {
    off(value) || value.as_object().is_some_and(|map| map.is_empty())
}

fn archived(name: &str, unit: &str) -> String {
    graphicle::archived::message(name, unit)
}

impl ArchivedOption {
    pub fn message(&self) -> String {
        archived(&format!("`{}`", self.option), self.unit)
    }
}

impl ArchivedValue {
    pub fn message(&self) -> String {
        archived(&format!("`{}: \"{}\"`", self.option, self.value), self.unit)
    }
}

/// Refuse every unsupported option `request` turns on, or unsupported value
/// it names.
pub fn check(request: &Value) -> Result<()> {
    for entry in OPTIONS {
        if request.get(entry.option).is_some_and(|v| !(entry.inert)(v)) {
            return Err(Error::invalid(entry.message()));
        }
    }
    for entry in VALUES {
        if request.get(entry.option).and_then(Value::as_str) == Some(entry.value) {
            return Err(Error::invalid(entry.message()));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Every entry is refused when turned on, and names a well-formed unit
    /// that no other entry names.
    #[test]
    fn every_entry_is_refused_when_on_and_names_its_own_unit() {
        for entry in OPTIONS {
            assert!(
                !(entry.inert)(&json!(true)),
                "{} must be refused when turned on",
                entry.option
            );
        }
        let units: Vec<&str> = OPTIONS
            .iter()
            .map(|e| e.unit)
            .chain(VALUES.iter().map(|e| e.unit))
            .collect();
        for unit in &units {
            assert!(
                !unit.is_empty()
                    && unit
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'),
                "{unit}"
            );
        }
        let names: std::collections::BTreeSet<(&str, &str)> = OPTIONS
            .iter()
            .map(|e| (e.option, ""))
            .chain(VALUES.iter().map(|e| (e.option, e.value)))
            .collect();
        assert_eq!(names.len(), OPTIONS.len() + VALUES.len());
    }
}
