//! Ordered adjacency with inline single edges and indexed high-degree nodes.
use smallvec::SmallVec;
type Positions = hashbrown::HashMap<usize, usize, crate::graph::FastState>;
pub(crate) type EdgeIds = SmallVec<[usize; 1]>;
const INDEX_THRESHOLD: usize = 16;

#[derive(Clone, Debug, Default)]
pub(crate) struct Neighbors {
    entries: Vec<(usize, EdgeIds)>,
    // Most graph nodes have few neighbors. Only hubs need a separate index;
    // keep their hash table out of every ordinary node's inline storage.
    positions: Option<Box<Positions>>,
}

impl Neighbors {
    fn position(&self, neighbor: usize) -> Option<usize> {
        match &self.positions {
            Some(positions) => positions.get(&neighbor).copied(),
            None => self.entries.iter().position(|(node, _)| *node == neighbor),
        }
    }

    pub fn get(&self, neighbor: usize) -> Option<&EdgeIds> {
        self.position(neighbor).map(|index| &self.entries[index].1)
    }

    pub fn iter(&self) -> std::slice::Iter<'_, (usize, EdgeIds)> {
        self.entries.iter()
    }

    pub fn push_edge(&mut self, neighbor: usize, edge: usize) {
        if let Some(index) = self.position(neighbor) {
            self.entries[index].1.push(edge);
            return;
        }
        let index = self.entries.len();
        self.entries.push((neighbor, smallvec::smallvec![edge]));
        if let Some(positions) = &mut self.positions {
            positions.insert(neighbor, index);
        } else if self.entries.len() >= INDEX_THRESHOLD {
            self.reindex();
        }
    }

    pub fn remove_edge(&mut self, neighbor: usize, edge: usize) {
        let Some(index) = self.position(neighbor) else {
            return;
        };
        self.entries[index].1.retain(|id| *id != edge);
        if self.entries[index].1.is_empty() {
            self.entries.remove(index);
            self.reindex();
        }
    }

    pub fn remap(mut self, nodes: &[Option<usize>]) -> Self {
        self.entries.retain_mut(|(neighbor, _)| {
            if let Some(new) = nodes[*neighbor] {
                *neighbor = new;
                true
            } else {
                false
            }
        });
        self.reindex();
        self
    }

    /// A copy restricted to kept neighbors with node and edge ids remapped;
    /// neighbor and parallel-edge order are preserved.
    pub fn restricted(&self, nodes: &[Option<usize>], edges: impl Fn(usize) -> usize) -> Self {
        let mut row = Self {
            entries: self
                .entries
                .iter()
                .filter_map(|(neighbor, ids)| {
                    nodes[*neighbor].map(|new| (new, ids.iter().map(|id| edges(*id)).collect()))
                })
                .collect(),
            positions: None,
        };
        row.reindex();
        row
    }

    /// The caller has validated that order contains exactly these neighbors.
    pub fn reorder(&mut self, order: &[usize]) {
        let positions: Positions = order
            .iter()
            .enumerate()
            .map(|(i, &node)| (node, i))
            .collect();
        self.entries
            .sort_unstable_by_key(|(node, _)| positions[node]);
        self.reindex();
    }

    fn reindex(&mut self) {
        if self.entries.len() < INDEX_THRESHOLD {
            self.positions = None;
            return;
        }
        let positions = self.positions.get_or_insert_with(Default::default);
        positions.clear();
        positions.extend(
            self.entries
                .iter()
                .enumerate()
                .map(|(i, (node, _))| (*node, i)),
        );
    }

    /// Query snapshot encoding (`crate::snapshot`): neighbors and parallel
    /// edge ids in their stored order.
    pub(crate) fn write_snapshot(&self, w: &mut crate::snapshot::Writer<'_>) {
        w.usize(self.entries.len());
        for (neighbor, ids) in &self.entries {
            w.usize(*neighbor);
            w.usize(ids.len());
            for id in ids {
                w.usize(*id);
            }
        }
    }

    pub(crate) fn read_snapshot(
        r: &mut crate::snapshot::Reader<'_, '_>,
        nodes: usize,
        edges: usize,
    ) -> crate::Result<Self> {
        let n = r.len(2)?;
        let mut entries = Vec::with_capacity(n);
        for _ in 0..n {
            let neighbor = r.index(nodes)?;
            let k = r.len(1)?;
            let mut ids = EdgeIds::with_capacity(k);
            for _ in 0..k {
                ids.push(r.index(edges)?);
            }
            entries.push((neighbor, ids));
        }
        let mut row = Self {
            entries,
            positions: None,
        };
        row.reindex();
        Ok(row)
    }
}
