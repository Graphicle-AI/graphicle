use rand::Rng;

use super::leiden_clustering::leiden;
use crate::clustering::{ClusterItem, Clustering};
use crate::errors::CoreError;
use crate::network::prelude::*;
use std::collections::HashSet;
use std::collections::VecDeque;

struct HierarchicalWork {
    subnetwork: LabeledNetwork<CompactNodeId>,
    parent_cluster: ClusterId,
}

pub struct HierarchicalCluster {
    pub node: CompactNodeId,
    pub cluster: ClusterId,
    pub level: u32,
    pub parent_cluster: Option<ClusterId>,
    pub is_final_cluster: bool,
}

struct HierarchicalClustering {
    hierarchical_clusterings: Vec<HierarchicalCluster>,
    cluster_range: Vec<(usize, usize)>,
}

impl HierarchicalClustering {
    pub fn new(initial_clustering: &Clustering) -> Self {
        let mut cluster_range: Vec<(usize, usize)> =
            Vec::with_capacity(initial_clustering.next_cluster_id());
        let mut hierarchical: Vec<HierarchicalCluster> =
            Vec::with_capacity(initial_clustering.num_nodes());

        let ordered_cluster_items: Vec<ClusterItem> = initial_clustering.sorted_cluster_items();

        let mut range_start: usize = 0;
        let mut previous_cluster: usize = 0;
        for cluster_item in ordered_cluster_items {
            let hierarchical_cluster = HierarchicalCluster {
                node: cluster_item.node_id,
                cluster: cluster_item.cluster,
                level: 0,
                parent_cluster: None,
                is_final_cluster: true,
            };
            if cluster_item.cluster != previous_cluster {
                cluster_range.push((range_start, hierarchical.len()));
                range_start = hierarchical.len();
            }
            hierarchical.push(hierarchical_cluster);
            previous_cluster = cluster_item.cluster;
        }
        cluster_range.push((range_start, hierarchical.len()));

        HierarchicalClustering {
            hierarchical_clusterings: hierarchical,
            cluster_range,
        }
    }

    pub fn insert_subnetwork_clustering(
        &mut self,
        subnetwork: &LabeledNetwork<CompactNodeId>,
        subnetwork_clustering: &Clustering,
        parent_cluster: ClusterId,
        starting_cluster_id: ClusterId,
        level: u32,
    ) -> usize {
        let ordered_cluster_items: Vec<ClusterItem> = subnetwork_clustering.sorted_cluster_items();

        let mut range_start: usize = self.hierarchical_clusterings.len();
        let mut iter_cluster_prev: ClusterId = 0;
        let mut final_cluster_id: usize = 0;

        for cluster_item in ordered_cluster_items {
            final_cluster_id = cluster_item.cluster + 1;
            let hierarchical_cluster = HierarchicalCluster {
                node: *subnetwork.label_for(cluster_item.node_id),
                cluster: cluster_item.cluster + starting_cluster_id,
                level,
                parent_cluster: Some(parent_cluster),
                is_final_cluster: true,
            };
            if cluster_item.cluster != iter_cluster_prev {
                self.cluster_range
                    .push((range_start, self.hierarchical_clusterings.len()));
                range_start = self.hierarchical_clusterings.len();
            }
            self.hierarchical_clusterings.push(hierarchical_cluster);
            iter_cluster_prev = cluster_item.cluster;
        }
        self.cluster_range
            .push((range_start, self.hierarchical_clusterings.len()));

        let (start, end) = self.cluster_range[parent_cluster];

        for old_hierarchical_cluster_entry in start..end {
            self.hierarchical_clusterings[old_hierarchical_cluster_entry].is_final_cluster = false;
        }

        final_cluster_id
    }
}

trait OrderedClustering {
    fn sorted_cluster_items(&self) -> Vec<ClusterItem>;
}

impl OrderedClustering for Clustering {
    fn sorted_cluster_items(&self) -> Vec<ClusterItem> {
        let mut ordered_cluster_items: Vec<ClusterItem> = self.into_iter().collect();
        ordered_cluster_items
            .sort_by(|a, b| a.cluster.cmp(&b.cluster).then(a.node_id.cmp(&b.node_id)));
        ordered_cluster_items
    }
}

pub fn hierarchical_leiden<T>(
    network: &CompactNetwork,
    clustering: Option<Clustering>,
    iterations: Option<usize>,
    resolution: Option<f64>,
    randomness: Option<f64>,
    rng: &mut T,
    use_modularity: bool,
    max_cluster_size: u32,
) -> Result<Vec<HierarchicalCluster>, CoreError>
where
    T: Rng + Clone + Send,
{
    let (_, mut updated_clustering) = leiden(
        network,
        clustering,
        iterations,
        resolution,
        randomness,
        rng,
        use_modularity,
    )?;

    let mut hierarchical_clustering: HierarchicalClustering =
        HierarchicalClustering::new(&updated_clustering);
    let mut work_queue: VecDeque<HierarchicalWork> = VecDeque::new();
    let mut level: u32 = 1;
    let mut clusters_that_did_not_split: HashSet<usize> = HashSet::new();

    let nodes_by_cluster: Vec<Vec<CompactNodeId>> = updated_clustering.nodes_per_cluster();

    for subnetwork in network.filtered_subnetworks(
        &updated_clustering,
        &nodes_by_cluster,
        max_cluster_size,
        use_modularity,
    ) {
        work_queue.push_back(HierarchicalWork {
            subnetwork: subnetwork.subnetwork,
            parent_cluster: subnetwork.id,
        });
    }

    while !work_queue.is_empty() {
        let work_item: HierarchicalWork = work_queue.pop_front().unwrap();
        let subnetwork: LabeledNetwork<CompactNodeId> = work_item.subnetwork;
        let (_, subnetwork_clustering) = leiden(
            subnetwork.compact(),
            None,
            iterations,
            resolution,
            randomness,
            rng,
            use_modularity,
        )?;
        let offset: usize = updated_clustering.next_cluster_id();

        if subnetwork_clustering.next_cluster_id() == 1 {
            // we couldn't break this cluster down any further.
            clusters_that_did_not_split.insert(work_item.parent_cluster);
        } else {
            hierarchical_clustering.insert_subnetwork_clustering(
                &subnetwork,
                &subnetwork_clustering,
                work_item.parent_cluster,
                offset,
                level,
            );
            for clustering_item in &subnetwork_clustering {
                let new_cluster_id: ClusterId = clustering_item.cluster + offset;
                let old_node_id: CompactNodeId = *subnetwork.label_for(clustering_item.node_id);
                updated_clustering.update_cluster_at(old_node_id, new_cluster_id)?;
            }
        }

        if work_queue.is_empty() {
            level += 1;
            let nodes_by_cluster: Vec<Vec<CompactNodeId>> = updated_clustering.nodes_per_cluster();
            for subnetwork in network.subnetworks_iter(
                &updated_clustering,
                &nodes_by_cluster,
                Some(max_cluster_size),
            ) {
                // Push the current subgraph onto the work_queue if-and-only-if the subgraph
                // has more than one node AND the parent graph has more than one subgraph.
                if nodes_by_cluster[subnetwork.id].len() > 1
                    && !clusters_that_did_not_split.contains(&subnetwork.id)
                {
                    work_queue.push_back(HierarchicalWork {
                        subnetwork: subnetwork.subnetwork,
                        parent_cluster: subnetwork.id,
                    });
                }
            }
        }
    }
    Ok(hierarchical_clustering.hierarchical_clusterings)
}
