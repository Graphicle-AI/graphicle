//! Unsupported options, option values and job kinds.
//!
//! A request that names one is answered here instead of being silently
//! ignored:
//! - an unsupported option sent at an inert value is accepted and has no
//!   effect: absent, `null`, `false` or `0`, or an old default its entry
//!   names (such as `"full"`), because evaluation drivers send explicit off
//!   values (`note_links: false`, `ranked_turns: 0`, `notes_tier: "full"`);
//! - any other value is refused: "`<option>` is not supported by Graphicle"
//!   (the graph engine's [`graphicle::archived::message`]);
//! - an unsupported value of an option that stays (one mode of several) is
//!   refused the same way;
//! - a job of an unsupported kind is never leased by any edition: it stays
//!   queued, job status reports it under `archived`, and a new one is
//!   refused. A queued job whose arguments turn an unsupported option on
//!   fails with the same message when a worker reaches it.
//!
//! Stored data is never rewritten. A connector `observe` policy that names an
//! unsupported option at an inert value still validates (the option and the
//! fields it alone reads are left out of validation), and one that turns it
//! on stops automatic observation with the refusal as the reason.
use crate::{Error, Result};
use serde_json::Value;

/// Where callers send an option.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Surface {
    /// `context` (CLI, HTTP, MCP `recall_project`).
    Context,
    /// Queued workflow arguments: checked when a job is queued and again
    /// when a worker runs it.
    Workflow,
    /// `observer.context`.
    ObserverContext,
    /// A connector's `observe` policy: checked when it is set and each time
    /// automatic observation decides.
    ObservePolicy,
}

/// An option that was removed with its unit.
pub struct ArchivedOption {
    /// The option's name as callers send it.
    pub option: &'static str,
    /// Where callers sent it.
    pub surfaces: &'static [Surface],
    /// The values that are still accepted, with no effect (see [`off`]).
    pub inert: fn(&Value) -> bool,
    /// Options read only while this one was on: inert, whatever their value,
    /// unless this one is on.
    pub dependents: &'static [&'static str],
    /// The entry's unit name.
    pub unit: &'static str,
}

/// One value of an option that stays, removed with its unit.
pub struct ArchivedValue {
    pub option: &'static str,
    pub value: &'static str,
    pub surfaces: &'static [Surface],
    pub unit: &'static str,
}

/// A job kind that was removed with its unit.
pub struct ArchivedKind {
    pub kind: &'static str,
    pub unit: &'static str,
}

/// Unsupported options.
pub const OPTIONS: &[ArchivedOption] = &[
    // `false` opted out of the promoted-hint lookup; `true`, the old default,
    // asked for it. Neither changes anything now, so both are accepted.
    ArchivedOption {
        option: "learned_hints",
        surfaces: &[Surface::Context],
        inert: bool_or_off,
        dependents: &[],
        unit: "learning-and-hints",
    },
    ArchivedOption {
        option: "escalation_model",
        surfaces: &[Surface::Workflow],
        inert: off,
        dependents: &[],
        unit: "answer-cascade",
    },
    ArchivedOption {
        option: "second_look",
        surfaces: &[Surface::Workflow],
        inert: off,
        dependents: &[],
        unit: "answer-second-look",
    },
    ArchivedOption {
        option: "retrieval_policy",
        surfaces: &[Surface::Workflow],
        inert: off,
        dependents: &[],
        unit: "answer-retrieval-policy",
    },
    ArchivedOption {
        option: "query_expansion",
        surfaces: &[Surface::Workflow],
        inert: off,
        dependents: &[],
        unit: "answer-query-expansion",
    },
    // Evaluation drivers send `notes_tier: "full"`, the old default.
    ArchivedOption {
        option: "notes_tier",
        surfaces: &[Surface::Workflow],
        inert: full_or_off,
        dependents: &[],
        unit: "answer-notes-tiers",
    },
    // Evaluation drivers send `note_links: false`.
    ArchivedOption {
        option: "note_links",
        surfaces: &[Surface::Workflow, Surface::ObserverContext],
        inert: off,
        dependents: &[],
        unit: "answer-note-links",
    },
    // The combination arms sent no `profile_route`; `"sections"` was the
    // default reading of a reflection step that is not supported.
    ArchivedOption {
        option: "profile_route",
        surfaces: &[Surface::Workflow],
        inert: sections_or_off,
        dependents: &[],
        unit: "observer-reflection",
    },
    // A connector policy's automatic reflection round; the other fields it
    // alone read are inert unless it is on.
    ArchivedOption {
        option: "reflect",
        surfaces: &[Surface::ObservePolicy],
        inert: off,
        dependents: &[
            "reflect_model",
            "reflect_threshold_tokens",
            "reflect_tree",
            "max_items",
        ],
        unit: "observer-reflection",
    },
    // `context` (CLI, HTTP, MCP) and retrieval workflows took the profile.
    ArchivedOption {
        option: "local_colbert_profile",
        surfaces: &[Surface::Context, Surface::Workflow],
        inert: off,
        dependents: &[],
        unit: "rerank-colbert",
    },
    // The cross-encoder's extras on `context` and retrieval workflows.
    ArchivedOption {
        option: "adaptive_alpha",
        surfaces: &[Surface::Context, Surface::Workflow],
        inert: off,
        dependents: &[],
        unit: "rerank-extras",
    },
    ArchivedOption {
        option: "rerank_dates",
        surfaces: &[Surface::Context, Surface::Workflow],
        inert: off,
        dependents: &[],
        unit: "rerank-extras",
    },
    // One window, the old default, read a passage as one text.
    ArchivedOption {
        option: "rerank_windows",
        surfaces: &[Surface::Context, Surface::Workflow],
        inert: one_or_off,
        dependents: &[],
        unit: "rerank-extras",
    },
    // Retrieval options `context` forwarded to the memory crate's `rank`.
    ArchivedOption {
        option: "graph",
        surfaces: &[Surface::Context, Surface::Workflow],
        inert: off,
        dependents: &[],
        unit: "retrieval-graph-expansion",
    },
    // The temporal arm, `spread` and the recency modes: `true` or an object.
    ArchivedOption {
        option: "temporal",
        surfaces: &[Surface::Context, Surface::Workflow],
        inert: off,
        dependents: &[],
        unit: "retrieval-temporal-arm",
    },
    ArchivedOption {
        option: "abstraction",
        surfaces: &[Surface::Context, Surface::Workflow],
        inert: off,
        dependents: &[],
        unit: "retrieval-abstraction",
    },
    ArchivedOption {
        option: "entity",
        surfaces: &[Surface::Context, Surface::Workflow],
        inert: off,
        dependents: &[],
        unit: "retrieval-entity-boost",
    },
    ArchivedOption {
        option: "prf",
        surfaces: &[Surface::Context, Surface::Workflow],
        inert: off,
        dependents: &[],
        unit: "retrieval-prf-depth-gate",
    },
    ArchivedOption {
        option: "adaptive_depth",
        surfaces: &[Surface::Context, Surface::Workflow],
        inert: off,
        dependents: &[],
        unit: "retrieval-prf-depth-gate",
    },
    // `"always"`, the old default, fused the caller's variants every time.
    ArchivedOption {
        option: "query_variants_gate",
        surfaces: &[Surface::Context, Surface::Workflow],
        inert: always_or_off,
        dependents: &[],
        unit: "retrieval-prf-depth-gate",
    },
    // SmartSearch's grep arm: `true` or `{"boundary"}`.
    ArchivedOption {
        option: "grep",
        surfaces: &[Surface::Context, Surface::Workflow],
        inert: off,
        dependents: &[],
        unit: "retrieval-grep-arm",
    },
    // R4-post's `v2p_unified` sends `ranked_turns: 0`. `shown` only kept
    // turns the caller already held out of `ranked_turns`.
    ArchivedOption {
        option: "ranked_turns",
        surfaces: &[Surface::ObserverContext],
        inert: off,
        dependents: &["shown"],
        unit: "observer-ranked-turns",
    },
];
/// Unsupported values of options that stay.
pub const VALUES: &[ArchivedValue] = &[
    ArchivedValue {
        option: "extraction_mode",
        value: "compact",
        surfaces: &[Surface::Workflow],
        unit: "compact-extraction",
    },
    // Hindsight's date boosts after the cross-encoder; the other modes stay.
    ArchivedValue {
        option: "rerank_mode",
        value: "boosted",
        surfaces: &[Surface::Context, Surface::Workflow],
        unit: "retrieval-temporal-arm",
    },
    // Notes as the fact arm's cues; `"merge"` (and `true`) stays.
    ArchivedValue {
        option: "note_keys",
        value: "cues",
        surfaces: &[Surface::Context, Surface::Workflow],
        unit: "retrieval-abstraction",
    },
    // `rank`'s overview mode boosted the community reports; `auto`,
    // `balanced`, `procedures` and `relationships` stay.
    ArchivedValue {
        option: "mode",
        value: "overview",
        surfaces: &[Surface::Context, Surface::Workflow],
        unit: "community-reports",
    },
    // Observer profiles other than the default `conversation`: the
    // `observer_notes` job and a connector's `observe` policy take them.
    ArchivedValue {
        option: "profile",
        value: "conversation-v1",
        surfaces: &[Surface::Workflow, Surface::ObservePolicy],
        unit: "observer-extra-profiles",
    },
    ArchivedValue {
        option: "profile",
        value: "agent_trajectory",
        surfaces: &[Surface::Workflow, Surface::ObservePolicy],
        unit: "observer-extra-profiles",
    },
    ArchivedValue {
        option: "profile",
        value: "conversation-entities",
        surfaces: &[Surface::Workflow, Surface::ObservePolicy],
        unit: "observer-extra-profiles",
    },
    ArchivedValue {
        option: "profile",
        value: "conversation-preferences",
        surfaces: &[Surface::Workflow, Surface::ObservePolicy],
        unit: "observer-extra-profiles",
    },
    ArchivedValue {
        option: "profile",
        value: "conversation-entities-preferences",
        surfaces: &[Surface::Workflow, Surface::ObservePolicy],
        unit: "observer-extra-profiles",
    },
];
/// Unsupported job kinds.
pub const KINDS: &[ArchivedKind] = &[
    ArchivedKind {
        kind: "learn",
        unit: "task-recording",
    },
    ArchivedKind {
        kind: "identity",
        unit: "identity-review",
    },
    ArchivedKind {
        kind: "consolidate",
        unit: "learning-and-hints",
    },
    ArchivedKind {
        kind: "observer_reflection",
        unit: "observer-reflection",
    },
    ArchivedKind {
        kind: "prepare",
        unit: "prepare-workflow",
    },
    ArchivedKind {
        kind: "communities",
        unit: "community-reports",
    },
    ArchivedKind {
        kind: "global",
        unit: "community-reports",
    },
    ArchivedKind {
        kind: "drift",
        unit: "community-reports",
    },
    ArchivedKind {
        kind: "local",
        unit: "community-reports",
    },
];

/// The default inert test: `null`, `false` or zero (absent is always inert).
pub fn off(value: &Value) -> bool {
    value.is_null() || *value == Value::Bool(false) || value.as_f64() == Some(0.0)
}

/// Inert for a switch whose old default was on (`learned_hints`): `true` as
/// well as `false`.
pub fn bool_or_off(value: &Value) -> bool {
    off(value) || value.is_boolean()
}

/// Inert for an option whose old default was `"full"` (`notes_tier`).
pub fn full_or_off(value: &Value) -> bool {
    off(value) || value == "full"
}

/// Inert for an option whose old default was 1 (`rerank_windows`).
pub fn one_or_off(value: &Value) -> bool {
    off(value) || value.as_f64() == Some(1.0)
}

/// Inert for an option whose old default was `"always"` (`query_variants_gate`).
pub fn always_or_off(value: &Value) -> bool {
    off(value) || value == "always"
}

/// Inert for an option whose old default was `"sections"` (`profile_route`).
pub fn sections_or_off(value: &Value) -> bool {
    off(value) || value == "sections"
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

impl ArchivedKind {
    pub fn message(&self) -> String {
        archived(&format!("Job kind `{}`", self.kind), self.unit)
    }
    /// The kind's unit, for job status's `archived` field: "`<unit>`".
    pub fn listing(&self) -> String {
        graphicle::archived::listing(self.unit)
    }
    /// What a queued job of this kind waits for in job status.
    pub fn waiting(&self) -> String {
        format!(
            "{}; no edition of this release runs it, so it stays queued",
            self.message()
        )
    }
}

/// Refuse every unsupported option `args` turns on, or unsupported value it names,
/// for this surface.
pub fn check(surface: Surface, args: &Value) -> Result<()> {
    check_in(OPTIONS, VALUES, surface, args)
}

/// `args` without the unsupported options it holds at an inert value and
/// without their dependents: what a validator that refuses unknown fields
/// may see. Call [`check`] first.
pub fn without_inert(surface: Surface, args: &Value) -> Value {
    without_inert_in(OPTIONS, surface, args)
}

/// The unsupported kind `name`, if it is one.
pub fn kind(name: &str) -> Option<&'static ArchivedKind> {
    KINDS.iter().find(|entry| entry.kind == name)
}

fn check_in(
    options: &[ArchivedOption],
    values: &[ArchivedValue],
    surface: Surface,
    args: &Value,
) -> Result<()> {
    let Some(fields) = args.as_object() else {
        return Ok(());
    };
    for entry in options.iter().filter(|e| e.surfaces.contains(&surface)) {
        if fields.get(entry.option).is_some_and(|v| !(entry.inert)(v)) {
            return Err(Error::bad(entry.message()));
        }
    }
    for entry in values.iter().filter(|e| e.surfaces.contains(&surface)) {
        if fields.get(entry.option).and_then(Value::as_str) == Some(entry.value) {
            return Err(Error::bad(entry.message()));
        }
    }
    Ok(())
}

fn without_inert_in(options: &[ArchivedOption], surface: Surface, args: &Value) -> Value {
    let mut out = args.clone();
    let Some(fields) = out.as_object_mut() else {
        return out;
    };
    for entry in options.iter().filter(|e| e.surfaces.contains(&surface)) {
        if fields.get(entry.option).is_none_or(|v| (entry.inert)(v)) {
            fields.shift_remove(entry.option);
            for dependent in entry.dependents {
                fields.shift_remove(*dependent);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const SAMPLE: &[ArchivedOption] = &[
        ArchivedOption {
            option: "note_links",
            surfaces: &[Surface::Workflow, Surface::ObserverContext],
            inert: off,
            dependents: &[],
            unit: "answer-note-links",
        },
        ArchivedOption {
            option: "notes_tier",
            surfaces: &[Surface::Workflow],
            inert: full_or_off,
            dependents: &[],
            unit: "answer-notes-tiers",
        },
        ArchivedOption {
            option: "reflect",
            surfaces: &[Surface::ObservePolicy],
            inert: off,
            dependents: &["reflect_model", "reflect_tree"],
            unit: "observer-reflection",
        },
    ];
    const SAMPLE_VALUES: &[ArchivedValue] = &[ArchivedValue {
        option: "note_keys",
        value: "cues",
        surfaces: &[Surface::Context, Surface::Workflow],
        unit: "retrieval-abstraction",
    }];

    fn refused(surface: Surface, args: Value) -> String {
        check_in(SAMPLE, SAMPLE_VALUES, surface, &args)
            .unwrap_err()
            .message
    }

    #[test]
    fn inert_values_of_an_archived_option_are_accepted() {
        for args in [
            json!({"question":"q"}),
            json!({"note_links":null}),
            json!({"note_links":false}),
            json!({"note_links":0}),
            json!({"notes_tier":"full"}),
            json!({"notes_tier":false}),
            json!({"note_keys":"merge"}),
            json!({"note_keys":true}),
            json!("not an object"),
        ] {
            assert!(
                check_in(SAMPLE, SAMPLE_VALUES, Surface::Workflow, &args).is_ok(),
                "{args}"
            );
        }
    }

    #[test]
    fn other_values_are_refused_with_their_unit() {
        assert_eq!(
            refused(Surface::Workflow, json!({"note_links":true})),
            "`note_links` is not supported by Graphicle"
        );
        assert_eq!(
            refused(Surface::ObserverContext, json!({"note_links":"yes"})),
            "`note_links` is not supported by Graphicle"
        );
        for tier in [json!("adaptive"), json!({"router":{}}), json!(1)] {
            let message = refused(Surface::Workflow, json!({"notes_tier":tier}));
            assert!(
                message == "`notes_tier` is not supported by Graphicle",
                "{message}"
            );
        }
        assert_eq!(
            refused(Surface::Context, json!({"note_keys":"cues"})),
            "`note_keys: \"cues\"` is not supported by Graphicle"
        );
    }

    #[test]
    fn an_option_is_archived_only_where_callers_sent_it() {
        // `notes_tier` was a workflow option; elsewhere it was never read.
        assert!(
            check_in(
                SAMPLE,
                SAMPLE_VALUES,
                Surface::Context,
                &json!({"notes_tier":"adaptive"})
            )
            .is_ok()
        );
        assert!(
            check_in(
                SAMPLE,
                SAMPLE_VALUES,
                Surface::ObserverContext,
                &json!({"note_keys":"cues"})
            )
            .is_ok()
        );
    }

    #[test]
    fn validators_see_neither_inert_options_nor_the_fields_they_alone_read() {
        let policy =
            json!({"threshold_tokens":10,"reflect":false,"reflect_model":"m","reflect_tree":true});
        assert!(check_in(SAMPLE, SAMPLE_VALUES, Surface::ObservePolicy, &policy).is_ok());
        assert_eq!(
            without_inert_in(SAMPLE, Surface::ObservePolicy, &policy),
            json!({"threshold_tokens":10})
        );
        // Without the option its dependents are inert too.
        assert_eq!(
            without_inert_in(
                SAMPLE,
                Surface::ObservePolicy,
                &json!({"model":"m","reflect_tree":false})
            ),
            json!({"model":"m"})
        );
        // Turned on, the option is refused before any validator runs.
        assert_eq!(
            refused(
                Surface::ObservePolicy,
                json!({"reflect":true,"reflect_model":"m"})
            ),
            "`reflect` is not supported by Graphicle"
        );
        // Other surfaces and non-objects pass through unchanged.
        let args = json!({"reflect":false,"reflect_model":"m"});
        assert_eq!(without_inert_in(SAMPLE, Surface::Workflow, &args), args);
        assert_eq!(
            without_inert_in(SAMPLE, Surface::ObservePolicy, &json!(7)),
            json!(7)
        );
    }

    #[test]
    fn archived_kinds_name_their_unit() {
        let kind = ArchivedKind {
            kind: "prepare",
            unit: "prepare-workflow",
        };
        assert_eq!(
            kind.message(),
            "Job kind `prepare` is not supported by Graphicle"
        );
        assert!(kind.waiting().starts_with(&kind.message()));
        assert!(kind.waiting().ends_with("so it stays queued"));
    }

    /// The values each unsupported option accepts with no effect, besides
    /// absence, and a value that turns it on. Every entry of [`OPTIONS`] must
    /// be declared here.
    fn declared(option: &str) -> Option<(Vec<Value>, Value)> {
        let off = || vec![Value::Null, json!(false), json!(0)];
        let also = |old_default: Value| {
            let mut values = off();
            values.push(old_default);
            values
        };
        Some(match option {
            // `true` was the old default, the hint lookup; it is gone.
            "learned_hints" => (also(json!(true)), json!("on")),
            "notes_tier" => (also(json!("full")), json!("adaptive")),
            "profile_route" => (also(json!("sections")), json!("tree")),
            "rerank_windows" => (also(json!(1)), json!(4)),
            "query_variants_gate" => (also(json!("always")), json!("weak")),
            "escalation_model" | "local_colbert_profile" => (off(), json!("model")),
            "second_look" | "retrieval_policy" | "query_expansion" | "note_links" | "reflect"
            | "adaptive_alpha" | "rerank_dates" | "graph" | "temporal" | "abstraction"
            | "entity" | "prf" | "adaptive_depth" | "grep" | "ranked_turns" => (off(), json!(true)),
            _ => return None,
        })
    }

    /// Every entry declares its inert values and a value that turns it on;
    /// it accepts the first and refuses the second. Every unit an entry names
    /// is a well-formed unit name.
    #[test]
    fn every_entry_declares_its_inert_values_and_names_a_unit() {
        for entry in OPTIONS {
            let Some((inert, on)) = declared(entry.option) else {
                panic!("{}: declare its inert values in `declared`", entry.option);
            };
            assert!(!entry.surfaces.is_empty(), "{}", entry.option);
            for value in &inert {
                assert!((entry.inert)(value), "{}: {value} is inert", entry.option);
            }
            assert!(!(entry.inert)(&on), "{}: {on} turns it on", entry.option);
            if !inert.contains(&json!(true)) {
                assert!(!(entry.inert)(&json!(true)), "{}: true", entry.option);
            }
        }
        let units: Vec<&str> = OPTIONS
            .iter()
            .map(|e| e.unit)
            .chain(VALUES.iter().map(|e| e.unit))
            .chain(KINDS.iter().map(|e| e.unit))
            .collect();
        for unit in units {
            assert!(
                !unit.is_empty()
                    && unit
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'),
                "{unit}"
            );
        }
    }
}
