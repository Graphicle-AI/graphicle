use graphicle_memory::{
    recall::{self, MatchMode, Metadata, Query, Record, State},
    temporal,
};

fn metadata(name: &str) -> Metadata {
    Metadata {
        name: name.into(),
        aliases: vec![],
        scopes: vec![],
        state: State::Active,
        valid_from: None,
        valid_to: None,
        superseded_by: None,
    }
}
fn at(s: &str) -> chrono::DateTime<chrono::Utc> {
    temporal::instant(s).unwrap()
}
const NOW: &str = "2026-02-15T00:00:00Z";

#[test]
fn aliases_are_whole_words_and_do_not_fill_the_remaining_budget() {
    let mut routing = metadata("incident_routing");
    routing.aliases = vec!["pager route".into(), "IR".into()];
    let other = metadata("invoice");
    let records = [
        Record {
            id: "r",
            text: "Mira carries the pager.",
            metadata: &routing,
        },
        Record {
            id: "i",
            text: "An invoice mentions pager route in a quotation.",
            metadata: &other,
        },
    ];
    for (question, ids) in [
        ("Who owns the pager route?", vec!["r"]),
        ("IR", vec!["r"]),
        ("bird", vec![]),
        ("incident routing", vec!["r"]),
    ] {
        let selected = recall::select(
            &records,
            &Query {
                question: question.into(),
                ..Default::default()
            },
            at(NOW),
        )
        .unwrap();
        assert_eq!(
            selected
                .hits
                .iter()
                .map(|h| h.id.as_str())
                .collect::<Vec<_>>(),
            ids
        );
    }
}

#[test]
fn programming_language_names_do_not_collapse_into_the_same_identity() {
    let descriptors = [metadata("C"), metadata("C++"), metadata("C#")];
    let records: Vec<_> = descriptors
        .iter()
        .map(|metadata| Record {
            id: &metadata.name,
            text: "Language owner",
            metadata,
        })
        .collect();
    for name in ["C", "C++", "C#"] {
        let result = recall::select(
            &records,
            &Query {
                question: format!("Who maintains the {name} bindings?"),
                ..Default::default()
            },
            at(NOW),
        )
        .unwrap();
        assert_eq!(result.hits.len(), 1);
        assert_eq!(result.hits[0].id, name);
        assert!(result.ambiguous_names.is_empty());
    }
}

#[test]
fn scope_and_validity_filter_before_budget_and_ambiguity() {
    let mut a = metadata("release owner");
    a.scopes = vec!["project:larch".into()];
    let mut b = a.clone();
    b.scopes = vec!["project:cedar".into()];
    let mut old = a.clone();
    old.valid_to = Some("2026-02-01T00:00:00Z".into());
    let records = [
        Record {
            id: "a",
            text: "Mira",
            metadata: &a,
        },
        Record {
            id: "b",
            text: "Otto",
            metadata: &b,
        },
        Record {
            id: "old",
            text: "Luz",
            metadata: &old,
        },
    ];
    let query = Query {
        question: "release owner".into(),
        scopes: Some(vec!["project:larch".into()]),
        limit: 1,
        ..Default::default()
    };
    let result = recall::select(&records, &query, at(NOW)).unwrap();
    assert_eq!(result.hits[0].id, "a");
    assert!(result.ambiguous_names.is_empty());
    assert_eq!((result.excluded_scope, result.excluded_validity), (1, 1));
}

#[test]
fn a_supersession_uses_effective_time_not_ingestion_order() {
    let mut old = metadata("release owner");
    old.valid_from = Some("2026-01-01T00:00:00Z".into());
    old.valid_to = Some("2026-03-01T00:00:00Z".into());
    old.superseded_by = Some("new".into());
    let mut new = metadata("release owner");
    new.valid_from = old.valid_to.clone();
    let records = [
        Record {
            id: "new",
            text: "Otto",
            metadata: &new,
        },
        Record {
            id: "old",
            text: "Mira",
            metadata: &old,
        },
    ];
    for (date, id) in [(NOW, "old"), ("2026-03-01T00:00:00Z", "new")] {
        let query = Query {
            question: "release owner".into(),
            as_of: Some(date.into()),
            ..Default::default()
        };
        let result = recall::select(&records, &query, at(NOW)).unwrap();
        assert_eq!(result.hits.len(), 1);
        assert_eq!(result.hits[0].id, id);
    }
}

#[test]
fn state_does_not_turn_a_question_or_retired_belief_into_guidance() {
    let mut m = metadata("rotation");
    for state in [State::Historical, State::OpenQuestion, State::Resolved] {
        m.state = state;
        let records = [Record {
            id: "r",
            text: "Undecided",
            metadata: &m,
        }];
        let mut q = Query {
            question: "rotation".into(),
            ..Default::default()
        };
        assert!(
            recall::select(&records, &q, at(NOW))
                .unwrap()
                .hits
                .is_empty()
        );
        q.include_history = true;
        assert_eq!(recall::select(&records, &q, at(NOW)).unwrap().hits.len(), 1);
    }
}

#[test]
fn ambiguous_aliases_remain_separate_and_universal_is_explicit() {
    let a = metadata("owner");
    let mut b = a.clone();
    b.scopes = vec!["restricted-topic".into()];
    let records = [
        Record {
            id: "a",
            text: "A",
            metadata: &a,
        },
        Record {
            id: "b",
            text: "B",
            metadata: &b,
        },
    ];
    let mut q = Query {
        question: "owner".into(),
        ..Default::default()
    };
    assert_eq!(
        recall::select(&records, &q, at(NOW))
            .unwrap()
            .ambiguous_names["owner"],
        vec!["a", "b"]
    );
    q.scopes = Some(vec![]);
    let result = recall::select(&records, &q, at(NOW)).unwrap();
    assert_eq!(result.hits.len(), 1);
    assert_eq!(result.hits[0].id, "a");
}

#[test]
fn lexical_discovery_requires_a_real_term_and_is_not_an_identity_lookup() {
    let m = metadata("incident policy");
    let records = [Record {
        id: "r",
        text: "Mira carries the pager.",
        metadata: &m,
    }];
    let mut q = Query {
        question: "Who carries the pager?".into(),
        ..Default::default()
    };
    assert!(
        recall::select(&records, &q, at(NOW))
            .unwrap()
            .hits
            .is_empty()
    );
    q.match_mode = MatchMode::Lexical;
    assert_eq!(recall::select(&records, &q, at(NOW)).unwrap().hits.len(), 1);
    q.question = "what is my".into();
    assert!(
        recall::select(&records, &q, at(NOW))
            .unwrap()
            .hits
            .is_empty()
    );
}

#[test]
fn invalid_intervals_duplicates_and_zero_budgets_are_not_silently_repaired() {
    let mut m = metadata("owner");
    m.valid_from = Some(NOW.into());
    m.valid_to = Some(NOW.into());
    assert!(m.validate().is_err());
    m.valid_to = None;
    let record = || Record {
        id: "r",
        text: "A",
        metadata: &m,
    };
    let q = Query {
        question: "owner".into(),
        limit: 0,
        ..Default::default()
    };
    assert!(
        recall::select(&[record()], &q, at(NOW))
            .unwrap()
            .hits
            .is_empty()
    );
    assert!(recall::select(&[record(), record()], &q, at(NOW)).is_err());
}
