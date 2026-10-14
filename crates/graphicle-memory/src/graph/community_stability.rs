//! Stable community ids across episodes and bounded report context (plan item
//! G10).
//!
//! Community detection is re-run after every ingestion episode; the numbering
//! it produces is arbitrary. [`remap_communities_to_previous`] carries the
//! previous episode's ids forward by member overlap so reports, anchors and
//! memories that name a community keep pointing at the same neighbourhood.
//!
//! Report context is packed by [`sort_context`]: leaf-first, greedy, under a
//! hard token ceiling. Nothing is truncated; whatever does not fit is listed
//! by id so the caller can say what was left out.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// `(community id, member node ids)`.
pub type NamedCommunities = Vec<(String, Vec<String>)>;

fn jaccard(a: &BTreeSet<&str>, b: &BTreeSet<&str>) -> f64 {
    let union = a.union(b).count();
    if union == 0 {
        return 0.0;
    }
    a.intersection(b).count() as f64 / union as f64
}

/// Give each current community the id of the previous community it overlaps
/// most (Jaccard), one-to-one and greedy by overlap; ties fall to the smaller
/// previous id, then the earlier current community. Unmatched current
/// communities get fresh `c<n>` ids that collide with nothing. Output keeps
/// the order of `current`; members are sorted.
pub fn remap_communities_to_previous(
    previous: &[(String, Vec<String>)],
    current: &[(String, Vec<String>)],
) -> NamedCommunities {
    let previous_sets: Vec<(&str, BTreeSet<&str>)> = previous
        .iter()
        .map(|(id, members)| (id.as_str(), members.iter().map(String::as_str).collect()))
        .collect();
    let current_sets: Vec<BTreeSet<&str>> = current
        .iter()
        .map(|(_, members)| members.iter().map(String::as_str).collect())
        .collect();
    let mut overlaps: Vec<(f64, &str, usize)> = Vec::new();
    for (old_id, old) in &previous_sets {
        for (index, new) in current_sets.iter().enumerate() {
            let score = jaccard(old, new);
            if score > 0.0 {
                overlaps.push((score, old_id, index));
            }
        }
    }
    overlaps.sort_by(|a, b| {
        b.0.total_cmp(&a.0)
            .then_with(|| a.1.cmp(b.1))
            .then_with(|| a.2.cmp(&b.2))
    });
    let mut assigned: BTreeMap<usize, String> = BTreeMap::new();
    let mut used: BTreeSet<String> = previous.iter().map(|(id, _)| id.clone()).collect();
    let mut taken: BTreeSet<&str> = BTreeSet::new();
    for (_, old_id, index) in overlaps {
        if taken.contains(old_id) || assigned.contains_key(&index) {
            continue;
        }
        taken.insert(old_id);
        assigned.insert(index, old_id.to_string());
    }
    let mut next = 0usize;
    for index in 0..current.len() {
        if assigned.contains_key(&index) {
            continue;
        }
        let fresh = loop {
            let candidate = format!("c{next}");
            next += 1;
            if !used.contains(&candidate) {
                break candidate;
            }
        };
        used.insert(fresh.clone());
        assigned.insert(index, fresh);
    }
    current
        .iter()
        .enumerate()
        .map(|(index, (_, members))| {
            let mut members = members.clone();
            members.sort();
            members.dedup();
            (assigned[&index].clone(), members)
        })
        .collect()
}

/// One unit of report context.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextItem {
    pub id: String,
    pub text: String,
    pub tokens: usize,
    /// Node or community degree; higher is packed first within a level.
    pub degree: usize,
    /// Hierarchy depth; higher (leaf) is packed first.
    pub level: usize,
}

/// The packed context and an explicit account of what was left out.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Packed {
    pub included: Vec<ContextItem>,
    /// Ids omitted, in packing order: oversized items and items the budget
    /// could not accommodate.
    pub omitted: Vec<String>,
    /// Ids whose `tokens` alone exceed the ceiling (a subset of `omitted`).
    pub oversized: Vec<String>,
    pub tokens: usize,
}

/// Leaf-first (`level` descending), then `degree` descending, then `id`.
pub fn context_order(a: &ContextItem, b: &ContextItem) -> std::cmp::Ordering {
    b.level
        .cmp(&a.level)
        .then_with(|| b.degree.cmp(&a.degree))
        .then_with(|| a.id.cmp(&b.id))
}

/// Greedy leaf-first packing under a hard `max_input_tokens` ceiling. An item
/// that does not fit is skipped, never truncated, and reported in `omitted`;
/// an item that could never fit is also reported in `oversized`.
pub fn sort_context(mut items: Vec<ContextItem>, max_input_tokens: usize) -> Packed {
    items.sort_by(context_order);
    let mut packed = Packed::default();
    for item in items {
        if item.tokens > max_input_tokens {
            packed.oversized.push(item.id.clone());
            packed.omitted.push(item.id);
        } else if packed.tokens + item.tokens <= max_input_tokens {
            packed.tokens += item.tokens;
            packed.included.push(item);
        } else {
            packed.omitted.push(item.id);
        }
    }
    packed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn community(id: &str, members: &[&str]) -> (String, Vec<String>) {
        (
            id.to_string(),
            members.iter().map(|m| (*m).to_string()).collect(),
        )
    }

    #[test]
    fn remap_keeps_ids_of_the_best_overlapping_previous_community() {
        let previous = vec![
            community("alpha", &["a", "b", "c"]),
            community("beta", &["x", "y"]),
        ];
        let current = vec![
            community("7", &["x", "y", "z"]),
            community("3", &["c", "b", "a", "d"]),
        ];
        let remapped = remap_communities_to_previous(&previous, &current);
        assert_eq!(
            remapped,
            vec![
                community("beta", &["x", "y", "z"]),
                community("alpha", &["a", "b", "c", "d"]),
            ]
        );
    }

    #[test]
    fn remap_is_one_to_one_and_greedy_by_overlap() {
        // A split: one old community becomes two; the larger half keeps the id.
        let previous = vec![community("alpha", &["a", "b", "c", "d", "e"])];
        let current = vec![
            community("0", &["a", "b"]),
            community("1", &["c", "d", "e"]),
        ];
        let remapped = remap_communities_to_previous(&previous, &current);
        assert_eq!(remapped[1].0, "alpha");
        assert_eq!(remapped[0].0, "c0");
        // A merge: two old communities become one; the larger overlap wins.
        let previous = vec![
            community("small", &["a"]),
            community("big", &["b", "c", "d"]),
        ];
        let current = vec![community("9", &["a", "b", "c", "d"])];
        let remapped = remap_communities_to_previous(&previous, &current);
        assert_eq!(remapped[0].0, "big");
    }

    #[test]
    fn fresh_ids_never_collide_with_previous_or_assigned_ids() {
        let previous = vec![community("c0", &["a"]), community("c2", &["b"])];
        let current = vec![
            community("x", &["q"]),
            community("y", &["r"]),
            community("z", &["a"]),
        ];
        let remapped = remap_communities_to_previous(&previous, &current);
        let ids: Vec<&str> = remapped.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids, vec!["c1", "c3", "c0"]);
        let unique: BTreeSet<&str> = ids.iter().copied().collect();
        assert_eq!(unique.len(), ids.len());
    }

    #[test]
    fn remap_is_deterministic_under_ties() {
        let previous = vec![community("p", &["a", "b"]), community("q", &["c", "d"])];
        // Both current communities overlap both previous ones equally.
        let current = vec![community("0", &["a", "c"]), community("1", &["b", "d"])];
        let first = remap_communities_to_previous(&previous, &current);
        let second = remap_communities_to_previous(&previous, &current);
        assert_eq!(first, second);
        assert_eq!(first[0].0, "p");
        assert_eq!(first[1].0, "q");
        // Empty inputs are fine.
        assert!(remap_communities_to_previous(&[], &[]).is_empty());
        let fresh = remap_communities_to_previous(&[], &current);
        assert_eq!(fresh[0].0, "c0");
        assert_eq!(fresh[1].0, "c1");
    }

    fn item(id: &str, tokens: usize, degree: usize, level: usize) -> ContextItem {
        ContextItem {
            id: id.to_string(),
            text: format!("text of {id}"),
            tokens,
            degree,
            level,
        }
    }

    #[test]
    fn sort_context_packs_leaf_first_then_degree_then_id() {
        let items = vec![
            item("root", 10, 99, 0),
            item("leaf-low", 10, 1, 2),
            item("leaf-high", 10, 5, 2),
            item("mid-b", 10, 3, 1),
            item("mid-a", 10, 3, 1),
        ];
        let packed = sort_context(items, 1000);
        let order: Vec<&str> = packed.included.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(
            order,
            vec!["leaf-high", "leaf-low", "mid-a", "mid-b", "root"]
        );
        assert_eq!(packed.tokens, 50);
        assert!(packed.omitted.is_empty());
    }

    #[test]
    fn sort_context_enforces_a_hard_ceiling_with_explicit_omissions() {
        let items = vec![
            item("leaf-a", 40, 2, 2),
            item("leaf-b", 40, 1, 2),
            item("mid", 15, 1, 1),
            item("root", 30, 9, 0),
        ];
        let packed = sort_context(items, 100);
        let order: Vec<&str> = packed.included.iter().map(|i| i.id.as_str()).collect();
        // leaf-a (40) + leaf-b (40) fit; mid (15) fits; root (30) would exceed.
        assert_eq!(order, vec!["leaf-a", "leaf-b", "mid"]);
        assert_eq!(packed.tokens, 95);
        assert!(packed.tokens <= 100);
        assert_eq!(packed.omitted, vec!["root".to_string()]);
        assert!(packed.oversized.is_empty());
    }

    #[test]
    fn oversized_items_are_omitted_and_reported_never_truncated() {
        let items = vec![item("huge", 500, 1, 2), item("small", 5, 1, 2)];
        let packed = sort_context(items, 100);
        assert_eq!(packed.included.len(), 1);
        assert_eq!(packed.included[0].id, "small");
        assert_eq!(packed.included[0].text, "text of small");
        assert_eq!(packed.omitted, vec!["huge".to_string()]);
        assert_eq!(packed.oversized, vec!["huge".to_string()]);
        // A zero budget includes nothing and reports everything.
        let packed = sort_context(vec![item("a", 1, 0, 0), item("b", 2, 0, 0)], 0);
        assert!(packed.included.is_empty());
        assert_eq!(packed.omitted, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(packed.oversized.len(), 2);
    }
}
