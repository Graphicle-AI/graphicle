// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

use super::errors::CoreError;
use super::safe_vectors::SafeVectors;
use crate::network::{CompactNodeId, LabeledNetwork};
use std::collections::HashMap;
use std::ops::Index;

pub struct ClusterItem {
    pub node_id: usize,
    pub cluster: usize,
}

/// Clustering is not a great abstraction; the details of it are purposefully spilled to the
/// clustering algorithm for optimal computational runtime, but it's important to note that the
/// next_cluster_id may or may not reference the actual count of clusters in the Clustering;
/// to guarantee next_cluster_id is also the correct number of clusters, you must first run
/// `remove_empty_clusters` at a minimum.
#[derive(Debug, Clone, PartialEq)]
pub struct Clustering {
    /// The next safe cluster id that can be used. This means, at best, that no other nodes
    /// have used this cluster id.  In some circumstances it may also refer to the total number of
    /// clusters in a Clustering, but only if there are no empty clusters. To guarantee this means
    /// the total number of clusters, first call `remove_empty_clusters`
    next_cluster_id: usize,
    node_to_cluster_mapping: Vec<usize>,
}

impl Default for Clustering {
    fn default() -> Self {
        Self::new()
    }
}

impl Clustering {
    /// Creates an empty Clustering with no nodes.  Can be added to by calling `update_node_cluster`.
    pub fn new() -> Clustering {
        Clustering {
            next_cluster_id: 0,
            node_to_cluster_mapping: Vec::new(),
        }
    }

    /// Creates a Clustering with `num_nodes` entries in the `node_to_cluster_mapping` vector and
    /// a next_cluster_id of `num_nodes`.
    pub fn as_self_clusters(num_nodes: usize) -> Clustering {
        let mut identity_mapping: Vec<usize> = Vec::with_capacity(num_nodes);
        identity_mapping.extend(0..num_nodes);
        Clustering {
            next_cluster_id: num_nodes,
            node_to_cluster_mapping: identity_mapping,
        }
    }

    /// Creates a clustering (with ZERO sanity checking) of the values stored in Clustering.
    /// Use responsibly.
    pub fn as_defined(node_to_cluster_mapping: Vec<usize>, next_cluster_id: usize) -> Clustering {
        Clustering {
            next_cluster_id,
            node_to_cluster_mapping,
        }
    }

    /// The actual number of nodes in this Clustering
    pub fn num_nodes(&self) -> usize {
        self.node_to_cluster_mapping.len()
    }

    pub fn next_cluster_id(&self) -> usize {
        self.next_cluster_id
    }

    pub fn cluster_at(&self, node: usize) -> Result<usize, CoreError> {
        self.node_to_cluster_mapping
            .get_or_err(node, CoreError::ClusterIndexingError)
    }

    pub fn update_cluster_at(&mut self, node: usize, cluster: usize) -> Result<(), CoreError> {
        if self.node_to_cluster_mapping.is_safe_access(node) {
            self.node_to_cluster_mapping[node] = cluster;
            self.next_cluster_id = self.next_cluster_id.max(cluster + 1);
            Ok(())
        } else {
            Err(CoreError::ClusterIndexingError)
        }
    }

    /// Generates a vector of nodes for each cluster with the index referencing the cluster and the
    /// value being a count from 0 upward.
    pub fn num_nodes_per_cluster(&self) -> Vec<u64> {
        let mut nodes_per_cluster: Vec<u64> = vec![0_u64; self.next_cluster_id];
        for i in 0..self.node_to_cluster_mapping.len() {
            nodes_per_cluster[self.node_to_cluster_mapping[i]] += 1;
        }
        nodes_per_cluster
    }

    /// Generates a vector containing every node id for every cluster id. The outer vector index
    /// corresponds to the cluster id, and the values in the inner vectors correspond to the node ids.
    pub fn nodes_per_cluster(&self) -> Vec<Vec<CompactNodeId>> {
        let number_nodes_per_cluster: Vec<u64> = self.num_nodes_per_cluster();
        let mut nodes_per_cluster: Vec<Vec<CompactNodeId>> =
            Vec::with_capacity(self.next_cluster_id);
        for nodes_in_cluster in number_nodes_per_cluster.iter().take(self.next_cluster_id) {
            nodes_per_cluster.push(Vec::with_capacity(*nodes_in_cluster as usize));
        }
        for (node_id, cluster) in self.node_to_cluster_mapping.iter().enumerate() {
            nodes_per_cluster[*cluster].push(node_id);
        }
        nodes_per_cluster
    }

    /// This method compacts the Clustering, removing empty clusters and applying new cluster IDs
    /// to all the clusters that came out afterward, so as to guarantee that:
    /// - Our clustering starts at 0
    /// - Our clustering has no empty clusters
    /// - Our clustering number scheme is continuous
    ///
    /// If this method is called prior to next_cluster_id then next_cluster_id also is the
    /// total number of clusters in this Clustering
    pub fn remove_empty_clusters(&mut self) {
        let mut non_empty_clusters: Vec<bool> = vec![false; self.next_cluster_id];

        for i in 0..self.node_to_cluster_mapping.len() {
            non_empty_clusters[self.node_to_cluster_mapping[i]] = true;
        }

        let mut new_index: usize = 0;
        let mut new_cluster_lookup: Vec<usize> = vec![0; self.next_cluster_id];

        for i in 0..self.next_cluster_id {
            if non_empty_clusters[i] {
                new_cluster_lookup[i] = new_index;
                new_index += 1;
            }
        }

        self.next_cluster_id = new_index;

        for i in 0..self.node_to_cluster_mapping.len() {
            self.node_to_cluster_mapping[i] = new_cluster_lookup[self.node_to_cluster_mapping[i]];
        }
    }

    pub fn reset_next_cluster_id(&mut self) {
        self.next_cluster_id = 0;
    }

    pub fn merge_subnetwork_clustering(
        &mut self,
        subnetwork: &LabeledNetwork<CompactNodeId>,
        subnetwork_clustering: &Clustering,
    ) {
        for (new_id, old_id) in subnetwork.labeled_ids() {
            self.node_to_cluster_mapping[*old_id] =
                self.next_cluster_id + subnetwork_clustering.node_to_cluster_mapping[new_id];
        }
        self.next_cluster_id += subnetwork_clustering.next_cluster_id;
    }

    /// This method defines a new clustering scheme for our *clusters*. The `other` Clustering is
    /// actually describing a relationship between old Cluster IDs to new Cluster Ids, and thus the
    /// `other` Clustering is likely to be far smaller (and instead other.num_nodes contains
    /// precisely the number of clusters in the current object.
    pub fn merge_clustering(&mut self, other: &Clustering) {
        for i in 0..self.node_to_cluster_mapping.len() {
            self.node_to_cluster_mapping[i] =
                other.node_to_cluster_mapping[self.node_to_cluster_mapping[i]];
        }
        self.next_cluster_id = other.next_cluster_id;
    }
}

impl From<Clustering> for HashMap<usize, usize> {
    fn from(clustering: Clustering) -> Self {
        let mut map: HashMap<usize, usize> = HashMap::with_capacity(clustering.num_nodes());
        for i in 0..clustering.node_to_cluster_mapping.len() {
            map.insert(i, clustering.node_to_cluster_mapping[i]);
        }
        map
    }
}

pub struct ClusterIterator<'a> {
    cluster_ref: &'a Clustering,
    next_cluster_id: usize,
}

impl Iterator for ClusterIterator<'_> {
    type Item = ClusterItem;

    fn next(&mut self) -> Option<Self::Item> {
        if self.next_cluster_id == self.cluster_ref.node_to_cluster_mapping.len() {
            None
        } else {
            let item = ClusterItem {
                node_id: self.next_cluster_id,
                cluster: self.cluster_ref[self.next_cluster_id],
            };
            self.next_cluster_id += 1;
            Some(item)
        }
    }
}

impl<'a> IntoIterator for &'a Clustering {
    type Item = ClusterItem;
    type IntoIter = ClusterIterator<'a>;

    fn into_iter(self) -> Self::IntoIter {
        ClusterIterator {
            cluster_ref: self,
            next_cluster_id: 0,
        }
    }
}

impl Index<usize> for Clustering {
    type Output = usize;

    fn index(&self, index: usize) -> &Self::Output {
        &self.node_to_cluster_mapping[index]
    }
}

#[cfg(test)]
mod tests {
    use super::Clustering;

    #[test]
    pub fn test_remove_empty_clusters() {
        let mut clustering: Clustering = Clustering {
            node_to_cluster_mapping: vec![3, 3, 5, 1, 2, 2, 9, 0],
            next_cluster_id: 10,
        };
        let expected: Clustering = Clustering {
            node_to_cluster_mapping: vec![3, 3, 4, 1, 2, 2, 5, 0],
            next_cluster_id: 6,
        };
        clustering.remove_empty_clusters();
        assert_eq!(clustering, expected);

        let mut clustering: Clustering = Clustering {
            node_to_cluster_mapping: Vec::new(),
            next_cluster_id: 0,
        };
        let expected: Clustering = Clustering {
            node_to_cluster_mapping: Vec::new(),
            next_cluster_id: 0,
        };
        clustering.remove_empty_clusters();
        assert_eq!(clustering, expected);
    }

    #[test]
    pub fn test_merge_clusters() {
        let mut clustering: Clustering = Clustering {
            node_to_cluster_mapping: vec![1, 1, 4, 3, 0, 0, 5, 2],
            next_cluster_id: 6,
        };
        let other: Clustering = Clustering {
            node_to_cluster_mapping: vec![0, 2, 2, 3, 4, 4],
            next_cluster_id: 5,
        };
        let expected: Clustering = Clustering {
            node_to_cluster_mapping: vec![2, 2, 4, 3, 0, 0, 4, 2],
            next_cluster_id: 5,
        };
        clustering.merge_clustering(&other);
        assert_eq!(clustering, expected);
    }

    #[test]
    fn test_num_nodes_per_cluster() {
        let clustering: Clustering = Clustering {
            node_to_cluster_mapping: vec![1, 1, 4, 3, 0, 0, 5, 2],
            next_cluster_id: 6,
        };
        let expected: Vec<u64> = vec![2, 2, 1, 1, 1, 1];
        assert_eq!(expected, clustering.num_nodes_per_cluster());
        let clustering: Clustering = Clustering {
            node_to_cluster_mapping: vec![],
            next_cluster_id: 0,
        };
        let expected: Vec<u64> = Vec::new();
        assert_eq!(expected, clustering.num_nodes_per_cluster());
    }

    #[test]
    fn test_nodes_per_cluster() {
        let clustering: Clustering = Clustering {
            node_to_cluster_mapping: vec![1, 1, 4, 3, 0, 0, 5, 2],
            next_cluster_id: 6,
        };
        let expected: Vec<Vec<usize>> =
            vec![vec![4, 5], vec![0, 1], vec![7], vec![3], vec![2], vec![6]];
        assert_eq!(expected, clustering.nodes_per_cluster());

        let clustering: Clustering = Clustering {
            node_to_cluster_mapping: vec![],
            next_cluster_id: 0,
        };
        let expected: Vec<Vec<usize>> = Vec::new();
        assert_eq!(expected, clustering.nodes_per_cluster());
    }
}
