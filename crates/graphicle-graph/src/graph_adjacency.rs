//! Ordered adjacency rows: compact `(neighbour, edge)` pairs, parallel-edge
//! groups beside them, and a position index on high-degree rows.
//!
//! A row lists its neighbours in first-insertion order and, under each
//! neighbour, its edge ids in insertion order. That order is the output-order
//! contract (`iter_edges`, neighbour views and the Leiden input), so every
//! operation here keeps exactly the order the earlier `(usize, SmallVec)`
//! rows kept; only the storage is smaller.
use hashbrown::HashTable;
use smallvec::SmallVec;
use std::hash::BuildHasher;

/// Rows with at least this many neighbours keep a position index.
const INDEX_THRESHOLD: usize = 16;
/// An entry's edge slot at or above this value names a parallel-edge group
/// (`GROUP | group number`); below it, the slot is the neighbour's only edge.
/// Graph keeps edge ids below it.
pub(crate) const GROUP: u32 = 1 << 31;
/// Positions are looked up and never iterated, so a fixed hasher cannot
/// change any observable order.
const ROW_STATE: foldhash::fast::FixedState =
    foldhash::fast::FixedState::with_seed(0x243f_6a88_85a3_08d3);

fn hash(neighbor: u32) -> u64 {
    ROW_STATE.hash_one(neighbor)
}

#[derive(Clone, Debug, Default)]
pub(crate) struct Neighbors {
    /// `(neighbour, edge slot)` in neighbour insertion order. The common
    /// one-neighbour row needs no allocation.
    entries: SmallVec<[(u32, u32); 1]>,
    /// The hub index and parallel groups; absent on most rows.
    extra: Option<Box<Extra>>,
}

#[derive(Clone, Debug, Default)]
struct Extra {
    /// Position in `entries` of every neighbour while the row has
    /// `INDEX_THRESHOLD` or more neighbours; empty below that.
    positions: HashTable<u32>,
    /// Edge ids of each neighbour that has had parallel edges, in order.
    groups: Vec<Vec<u32>>,
}

impl Neighbors {
    fn indexed(&self) -> Option<&HashTable<u32>> {
        self.extra
            .as_deref()
            .map(|extra| &extra.positions)
            .filter(|positions| !positions.is_empty())
    }

    fn position(&self, neighbor: u32) -> Option<usize> {
        match self.indexed() {
            Some(positions) => positions
                .find(hash(neighbor), |&p| self.entries[p as usize].0 == neighbor)
                .map(|&p| p as usize),
            None => self.entries.iter().position(|&(n, _)| n == neighbor),
        }
    }

    /// The edge ids an entry's slot stands for.
    fn ids<'a>(&'a self, slot: &'a u32) -> &'a [u32] {
        if *slot < GROUP {
            return std::slice::from_ref(slot);
        }
        self.extra
            .as_deref()
            .and_then(|extra| extra.groups.get((*slot - GROUP) as usize))
            .map_or(&[], Vec::as_slice)
    }

    /// The edge ids to `neighbor`, in insertion order.
    pub fn get(&self, neighbor: usize) -> Option<&[u32]> {
        let p = self.position(u32::try_from(neighbor).ok()?)?;
        Some(self.ids(&self.entries[p].1))
    }

    /// `(neighbour, edge ids)` in neighbour insertion order.
    pub fn iter(&self) -> impl Iterator<Item = (usize, &[u32])> + '_ {
        self.entries
            .iter()
            .map(move |(neighbor, slot)| (*neighbor as usize, self.ids(slot)))
    }

    /// Appends `edge` to `neighbor`'s edges, adding the neighbour last when
    /// it is new. The graph keeps node ids within u32 and edge ids below
    /// `GROUP`.
    pub fn push_edge(&mut self, neighbor: usize, edge: usize) {
        let (neighbor, edge) = (neighbor as u32, edge as u32);
        debug_assert!(edge < GROUP);
        let Some(p) = self.position(neighbor) else {
            self.push_new(neighbor, edge);
            return;
        };
        let Self { entries, extra } = self;
        let extra = extra.get_or_insert_with(Default::default);
        let slot = entries[p].1;
        if slot >= GROUP {
            extra.groups[(slot - GROUP) as usize].push(edge);
        } else {
            entries[p].1 = GROUP | extra.groups.len() as u32;
            extra.groups.push(vec![slot, edge]);
        }
    }

    fn push_new(&mut self, neighbor: u32, edge: u32) {
        let entries = &mut self.entries;
        // Grow by an eighth instead of doubling: rows stay close to their
        // final size without a separate shrinking pass.
        if entries.len() == entries.capacity() {
            entries.reserve_exact(1 + entries.len() / 8);
        }
        let p = entries.len() as u32;
        entries.push((neighbor, edge));
        if entries.len() < INDEX_THRESHOLD {
            return;
        }
        if self.indexed().is_none() {
            // The row just reached the threshold: index all of it.
            self.reindex();
            return;
        }
        let Self { entries, extra } = self;
        if let Some(extra) = extra.as_deref_mut() {
            extra
                .positions
                .insert_unique(hash(neighbor), p, |&q| hash(entries[q as usize].0));
        }
    }

    pub fn remove_edge(&mut self, neighbor: usize, edge: usize) {
        let Ok(neighbor) = u32::try_from(neighbor) else {
            return;
        };
        let Some(p) = self.position(neighbor) else {
            return;
        };
        let slot = self.entries[p].1;
        if slot >= GROUP {
            let Self { entries, extra } = self;
            let Some(extra) = extra.as_deref_mut() else {
                return;
            };
            let g = (slot - GROUP) as usize;
            extra.groups[g].retain(|id| *id as usize != edge);
            if !extra.groups[g].is_empty() {
                return;
            }
            // Keep the group numbers dense: the last group takes g's number.
            let last = GROUP | (extra.groups.len() - 1) as u32;
            extra.groups.swap_remove(g);
            if let Some(moved) = entries.iter_mut().find(|(_, s)| *s == last) {
                moved.1 = slot;
            }
        } else if slot as usize != edge {
            return;
        }
        self.entries.remove(p);
        self.reindex();
    }

    /// Renumbers neighbours by `nodes`, dropping removed ones; order kept.
    pub fn remap(mut self, nodes: &[Option<usize>]) -> Self {
        self.entries.retain(|(neighbor, _)| {
            if let Some(new) = nodes[*neighbor as usize] {
                *neighbor = new as u32;
                true
            } else {
                false
            }
        });
        self.compact_groups();
        self.reindex();
        self
    }

    /// Renumbers the groups in entry order, dropping those of removed
    /// neighbours.
    fn compact_groups(&mut self) {
        let Self { entries, extra } = self;
        let Some(extra) = extra.as_deref_mut() else {
            return;
        };
        let mut groups = Vec::new();
        for (_, slot) in entries.iter_mut().filter(|(_, slot)| *slot >= GROUP) {
            let ids = std::mem::take(&mut extra.groups[(*slot - GROUP) as usize]);
            *slot = GROUP | groups.len() as u32;
            groups.push(ids);
        }
        extra.groups = groups;
    }

    /// A copy restricted to kept neighbors with node and edge ids remapped;
    /// neighbor and parallel-edge order are preserved.
    pub fn restricted(&self, nodes: &[Option<usize>], edges: impl Fn(usize) -> usize) -> Self {
        let mut row = Self::default();
        let kept = self
            .entries
            .iter()
            .filter(|(neighbor, _)| nodes[*neighbor as usize].is_some())
            .count();
        row.entries.reserve_exact(kept);
        for (neighbor, slot) in &self.entries {
            let Some(new) = nodes[*neighbor as usize] else {
                continue;
            };
            let ids = self.ids(slot);
            row.push_ids(new as u32, ids.iter().map(|id| edges(*id as usize) as u32));
        }
        row.reindex();
        row
    }

    /// Appends a neighbour with these edge ids (one id inline, any other
    /// count as a group). The caller reindexes.
    fn push_ids(&mut self, neighbor: u32, mut ids: impl ExactSizeIterator<Item = u32>) {
        if ids.len() == 1 {
            let id = ids.next().expect("one id");
            self.entries.push((neighbor, id));
            return;
        }
        let extra = self.extra.get_or_insert_with(Default::default);
        let slot = GROUP | extra.groups.len() as u32;
        extra.groups.push(ids.collect());
        self.entries.push((neighbor, slot));
    }

    /// The caller has validated that order contains exactly these neighbors.
    pub fn reorder(&mut self, order: &[usize]) {
        let positions: hashbrown::HashMap<usize, usize, foldhash::fast::FixedState> = order
            .iter()
            .enumerate()
            .map(|(i, &node)| (node, i))
            .collect();
        self.entries
            .sort_unstable_by_key(|(node, _)| positions[&(*node as usize)]);
        self.reindex();
    }

    fn reindex(&mut self) {
        let Self { entries, extra } = self;
        if entries.len() < INDEX_THRESHOLD {
            if let Some(boxed) = extra {
                if boxed.groups.is_empty() {
                    *extra = None;
                } else {
                    boxed.positions = HashTable::new();
                }
            }
            return;
        }
        let positions = &mut extra.get_or_insert_with(Default::default).positions;
        positions.clear();
        positions.reserve(entries.len(), |&q| hash(entries[q as usize].0));
        for (p, (neighbor, _)) in entries.iter().enumerate() {
            positions.insert_unique(hash(*neighbor), p as u32, |&q| hash(entries[q as usize].0));
        }
    }

    /// Releases spare capacity (rows, groups and the position index).
    pub fn shrink_to_fit(&mut self) {
        self.entries.shrink_to_fit();
        let Self { entries, extra } = self;
        if let Some(extra) = extra.as_deref_mut() {
            extra
                .positions
                .shrink_to(0, |&q| hash(entries[q as usize].0));
            for group in &mut extra.groups {
                group.shrink_to_fit();
            }
            extra.groups.shrink_to_fit();
        }
    }

    /// Query snapshot encoding (`crate::snapshot`): neighbors and parallel
    /// edge ids in their stored order, in the original usize layout.
    pub(crate) fn write_snapshot(&self, w: &mut crate::snapshot::Writer<'_>) {
        w.usize(self.entries.len());
        for (neighbor, slot) in &self.entries {
            w.usize(*neighbor as usize);
            let ids = self.ids(slot);
            w.usize(ids.len());
            for id in ids {
                w.usize(*id as usize);
            }
        }
    }

    /// Inverse of `write_snapshot`. `nodes` and `edges` bound the ids; the
    /// graph has checked that they fit the compact ids.
    pub(crate) fn read_snapshot(
        r: &mut crate::snapshot::Reader<'_, '_>,
        nodes: usize,
        edges: usize,
    ) -> crate::Result<Self> {
        let n = r.len(2)?;
        let mut row = Self::default();
        row.entries.reserve_exact(n);
        for _ in 0..n {
            let neighbor = r.index(nodes)? as u32;
            let k = r.len(1)?;
            let mut ids = Vec::with_capacity(k);
            for _ in 0..k {
                ids.push(r.index(edges)? as u32);
            }
            row.push_ids(neighbor, ids.into_iter());
        }
        row.reindex();
        Ok(row)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The earlier row layout, as the reference.
    #[derive(Default)]
    struct Reference(Vec<(usize, Vec<usize>)>);
    impl Reference {
        fn push_edge(&mut self, neighbor: usize, edge: usize) {
            match self.0.iter_mut().find(|(n, _)| *n == neighbor) {
                Some((_, ids)) => ids.push(edge),
                None => self.0.push((neighbor, vec![edge])),
            }
        }
        fn remove_edge(&mut self, neighbor: usize, edge: usize) {
            if let Some(i) = self.0.iter().position(|(n, _)| *n == neighbor) {
                self.0[i].1.retain(|id| *id != edge);
                if self.0[i].1.is_empty() {
                    self.0.remove(i);
                }
            }
        }
    }

    fn rows(row: &Neighbors) -> Vec<(usize, Vec<usize>)> {
        row.iter()
            .map(|(n, ids)| (n, ids.iter().map(|id| *id as usize).collect()))
            .collect()
    }

    #[test]
    fn rows_match_the_reference_layout_under_random_edits() {
        use rand::{Rng, SeedableRng};
        let mut rng = rand_xorshift::XorShiftRng::seed_from_u64(8191);
        for round in 0..300 {
            let (mut row, mut reference) = (Neighbors::default(), Reference::default());
            let width = if round % 3 == 0 { 60 } else { 12 };
            let mut next_edge = 0;
            for _ in 0..rng.gen_range(0..400) {
                let neighbor = rng.gen_range(0..width);
                if rng.gen_bool(0.7) {
                    row.push_edge(neighbor, next_edge);
                    reference.push_edge(neighbor, next_edge);
                    next_edge += 1;
                } else {
                    let edge = rng.gen_range(0..next_edge.max(1));
                    row.remove_edge(neighbor, edge);
                    reference.remove_edge(neighbor, edge);
                }
                assert_eq!(rows(&row), reference.0, "round {round}");
                for probe in 0..width + 2 {
                    let expected = reference
                        .0
                        .iter()
                        .find(|(n, _)| *n == probe)
                        .map(|(_, ids)| ids.clone());
                    let actual = row
                        .get(probe)
                        .map(|ids| ids.iter().map(|id| *id as usize).collect::<Vec<_>>());
                    assert_eq!(actual, expected, "round {round} probe {probe}");
                }
            }
            // Remapping drops and renumbers neighbours and keeps the order.
            let nodes: Vec<_> = (0..width)
                .map(|i| (i % 4 != 1).then_some(width - i))
                .collect();
            let restricted = row.restricted(&nodes, |id| id * 2);
            let remapped = row.clone().remap(&nodes);
            let expected: Vec<_> = reference
                .0
                .iter()
                .filter_map(|(n, ids)| nodes[*n].map(|new| (new, ids.clone())))
                .collect();
            assert_eq!(rows(&remapped), expected, "round {round} remap");
            let doubled: Vec<_> = expected
                .iter()
                .map(|(n, ids)| (*n, ids.iter().map(|id| id * 2).collect::<Vec<_>>()))
                .collect();
            assert_eq!(rows(&restricted), doubled, "round {round} restricted");
            let mut shrunk = remapped.clone();
            shrunk.shrink_to_fit();
            assert_eq!(rows(&shrunk), expected, "round {round} shrink");
            for (n, ids) in &expected {
                assert_eq!(
                    shrunk.get(*n).map(<[u32]>::to_vec),
                    Some(ids.iter().map(|id| *id as u32).collect()),
                    "round {round} lookup after remap"
                );
            }
        }
    }
}
