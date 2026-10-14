// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

/// A CompactNetwork is a tightly packed neighbor-optimized datastructure that defines a
/// *undirected* Network's edges.
///
/// The structure is optimized for:
///  - Fast lookups of all the neighbors (and the edges' weights) for a given Node
///  - Iteration through all the Nodes in the Network
///  - Iteration through all the neighbors (and edge weights) for each Node
///
/// To that end, the structure is primarily defined as two related collections:
///  - A vector whose indices are the NodeIds (usize) and whose values are the first EdgeId in the
///    second collection
///  - A second vector whose indices are the EdgeIds (usize) and whose values are a tuple of
///    (NodeId, weight (f64)).
///
///  The second vector's entries make two guarantees: that all of the neighbors for a given node
///  will be continuous, and that the neighbors will be sorted in ascending order as per NodeId.
///
/// This layout will hopefully minimize memory paging with cache misses by locating all of the
/// edges for a given node within the fewest pages possible vs. a conceptually simpler pointer based
/// approach.
///
/// This also means that the neighbors for a given NodeId `x` can be described roughly as:
///  - `second_vec[first_vec[x]..first_vec[x+1]]` (except when x = first_vec.len() - 1)
///
/// These Vectors also allow us to determine the number of nodes (first_vec.len()) and the
/// number of edges (second_vec.len() / 2) very quickly.
///
/// Desired behaviors:
///  - Iterate through CompactNetwork by a tuple of (NodeId, NeighborIterator)
///  - NeighborIterator must yield a tuple of:
///    - EdgeId (index of the edge in the array)
///    - Neighbor's NodeId
///    - Edge weight (f64)
///
/// e.g. a Network defined by the edges:
///   0,1,3.0
///   0,2,2.1
///   1,0,3.0
///   1,2,5.0
///   2,0,2.1
///   2,1,5.0
///
/// And the following code:
/// for (node_id, neighbor_iter) in &compact_network {
///   println!("Node ID: {}", node_id);
///   println!("Edges: ");
///   for (edge_id, (neighbor_node_id, edge_weight)) in neighbor_iter {
///     println!("\t{}: {} @ index {}", neighbor_node_id, edge_weight, edge_id);
///   }
/// }
///
/// Would print:
///   Node ID: 0
///   Edges:
///       1: 3.0 @ index 0
///       2: 2.1 @ index 1
///   Node ID: 1
///   Edges:
///       0: 3.0 @ index 2
///       2: 5.0 @ index 3
///   Node ID: 2
///   Edges:
///       0: 2.1 @ index 4
///       1: 5.0 @ index 5
///
/// The EdgeId is the direct index into the edges array for fast lookups of a specific edge,
/// which is primarily useful to the subnetwork generation functions
use super::networks::NetworkDetails;
use crate::clustering::Clustering;
use crate::errors::CoreError;
use crate::network::{LabeledNetwork, LabeledNetworkBuilder};
use std::collections::HashMap;
use std::ops::Range;

// Simple types that can be exposed
pub type CompactNodeId = usize;
pub type ClusterId = usize;
pub type ConnectionId = usize; // note: Maybe we shouldn't expose this?  It's not super pertinent outside of this module

// the combination of these tuples plus the vector index associated with them makes the public struct
pub type CompactNode = (f64, ConnectionId);
pub type CompactNeighbor = (CompactNodeId, f64);

#[derive(Debug)]
pub struct CompactNodeItem<'a> {
    pub id: CompactNodeId,
    pub weight: f64,
    compact_network: &'a CompactNetwork,
}

impl<'a> CompactNodeItem<'a> {
    pub fn neighbors(&self) -> NeighborIterator<'a> {
        // make the neighbor iterator here, not on compactnodeitem creation
        let neighbor_range: Range<ConnectionId> = self.compact_network.neighbor_range(self.id);
        let neighbor_start: ConnectionId = neighbor_range.start;
        NeighborIterator {
            compact_network: self.compact_network,
            neighbor_range,
            current_neighbor: neighbor_start,
        }
    }
}

#[derive(Debug)]
pub struct CompactNeighborItem {
    pub connection_id: ConnectionId, // may be unnecessary
    pub id: CompactNodeId,
    pub edge_weight: f64,
    pub node_weight: f64,
}

#[derive(Debug, PartialEq)]
pub struct CompactNetwork {
    nodes: Vec<CompactNode>,
    neighbors: Vec<CompactNeighbor>,
    total_self_links_edge_weight: f64,
}

#[derive(Debug)]
pub struct CompactSubnetworkItem<T> {
    pub id: ClusterId,
    pub subnetwork: LabeledNetwork<T>,
}

impl CompactNetwork {
    pub fn from(
        nodes: Vec<CompactNode>,
        neighbors: Vec<CompactNeighbor>,
        total_self_links_edge_weight: f64,
    ) -> CompactNetwork {
        CompactNetwork {
            nodes,
            neighbors,
            total_self_links_edge_weight,
        }
    }

    fn neighbor_range(&self, node_id: CompactNodeId) -> Range<CompactNodeId> {
        let (_, neighbor_start) = self.nodes[node_id];
        let end_range: ConnectionId = if node_id < self.nodes.len() - 1 {
            self.nodes[node_id + 1].1
        } else {
            self.neighbors.len()
        };
        return neighbor_start..end_range;
    }

    pub fn node(&self, id: CompactNodeId) -> CompactNodeItem<'_> {
        let weight: &f64 = &self.nodes[id].0;
        CompactNodeItem {
            id,
            weight: *weight,
            compact_network: self,
        }
    }

    pub fn neighbors_for(&self, id: CompactNodeId) -> NeighborIterator<'_> {
        let neighbor_range: Range<ConnectionId> = self.neighbor_range(id);
        let neighbor_start: ConnectionId = neighbor_range.start;
        NeighborIterator {
            compact_network: self,
            neighbor_range,
            current_neighbor: neighbor_start,
        }
    }

    pub fn node_weight(&self, id: CompactNodeId) -> f64 {
        self.nodes[id].0
    }

    pub fn node_weights(&self) -> Vec<f64> {
        self.nodes.iter().map(|(weight, _)| *weight).collect()
    }

    pub fn total_edge_weight_per_node(&self) -> Vec<f64> {
        // when using modularity, this should return the exact same as node_weights.
        self.nodes
            .iter()
            .map(|(_, node_id)| {
                self.neighbors_for(*node_id)
                    .map(|neighbor| neighbor.edge_weight)
                    .sum::<f64>()
            })
            .collect()
    }

    pub fn subnetworks_iter<'a, 'b>(
        &'a self,
        clustering: &Clustering,
        nodes_by_cluster: &'b Vec<Vec<CompactNodeId>>,
        subnetwork_minimum_size: Option<u32>,
    ) -> SubnetworkIterator<'a, 'b> {
        let clustering: Clustering = clustering.clone();

        SubnetworkIterator {
            compact_supernetwork: self,
            clustering,
            clustered_nodes: nodes_by_cluster,
            current_clustered_nodes_index: 0,
            builder: LabeledNetworkBuilder::new(),
            subnetwork_minimum_size,
        }
    }

    pub fn filtered_subnetworks<'a>(
        &'a self,
        clustering: &'a Clustering,
        nodes_by_cluster: &'a [Vec<CompactNodeId>],
        subnetwork_minimum_size: u32,
        use_modularity: bool,
    ) -> impl Iterator<Item = CompactSubnetworkItem<CompactNodeId>> + 'a {
        let mut labeled_network_builder: LabeledNetworkBuilder<CompactNodeId> =
            LabeledNetworkBuilder::new();
        let subnetwork_iterator = nodes_by_cluster
            .iter()
            .enumerate()
            .filter(move |(_cluster_id, nodes_in_cluster)| {
                nodes_in_cluster.len() >= subnetwork_minimum_size as usize
            })
            .map(move |(cluster_id, nodes_in_cluster)| {
                let subnetwork_edges = nodes_in_cluster.iter().flat_map(|node| {
                    self.neighbors_for(*node)
                        .filter(|neighbor| clustering[neighbor.id] == cluster_id)
                        .map(move |neighbor| (*node, neighbor.id, neighbor.edge_weight))
                });
                let subnetwork: LabeledNetwork<CompactNodeId> =
                    labeled_network_builder.build(subnetwork_edges, use_modularity);
                CompactSubnetworkItem {
                    subnetwork,
                    id: cluster_id,
                }
            });
        subnetwork_iterator
    }

    pub fn induce_clustering_network(
        &self,
        clustering: &Clustering,
    ) -> Result<CompactNetwork, CoreError> {
        let mut cluster_weights: Vec<f64> = vec![0_f64; clustering.next_cluster_id()];
        let mut cluster_total_self_links_edge_weight = self.total_self_links_edge_weight();

        let mut cluster_to_cluster_edges: HashMap<CompactNodeId, HashMap<CompactNodeId, f64>> =
            HashMap::new();

        for (node_id, (node_weight, _)) in self.nodes.iter().enumerate() {
            let node_cluster: CompactNodeId = clustering.cluster_at(node_id)?;
            cluster_weights[node_cluster] += node_weight;
            for neighbor in self.neighbors_for(node_id) {
                let neighbor_cluster: CompactNodeId = clustering.cluster_at(neighbor.id)?;
                if node_cluster == neighbor_cluster {
                    cluster_total_self_links_edge_weight += neighbor.edge_weight;
                } else {
                    *cluster_to_cluster_edges
                        .entry(node_cluster)
                        .or_default()
                        .entry(neighbor_cluster)
                        .or_insert(0_f64) += neighbor.edge_weight;
                }
            }
        }

        let mut cluster_nodes: Vec<CompactNode> = Vec::with_capacity(clustering.next_cluster_id());
        let mut cluster_neighbors: Vec<CompactNeighbor> = Vec::new();

        for (cluster, cluster_weight) in cluster_weights
            .iter()
            .take(clustering.next_cluster_id())
            .enumerate()
        {
            cluster_nodes.push((*cluster_weight, cluster_neighbors.len()));
            let mut neighbors: Vec<(&usize, &f64)> = cluster_to_cluster_edges
                .entry(cluster)
                .or_default()
                .iter()
                .collect();
            neighbors.sort_unstable_by(|a, b| a.0.cmp(b.0));
            cluster_neighbors.reserve(neighbors.len());
            for (neighbor_cluster, edge_weight) in neighbors {
                cluster_neighbors.push((*neighbor_cluster, *edge_weight));
            }
        }

        let induced: CompactNetwork = CompactNetwork::from(
            cluster_nodes,
            cluster_neighbors,
            cluster_total_self_links_edge_weight,
        );

        Ok(induced)
    }
}

impl NetworkDetails for CompactNetwork {
    fn num_nodes(&self) -> usize {
        self.nodes.len()
    }

    fn num_edges(&self) -> usize {
        (self.neighbors.len() as f64 / 2_f64) as usize
    }

    fn total_node_weight(&self) -> f64 {
        self.nodes.iter().map(|node| node.0).sum::<f64>()
    }

    fn total_edge_weight(&self) -> f64 {
        self.neighbors
            .iter()
            .map(|neighbor| neighbor.1)
            .sum::<f64>()
            / 2_f64
    }

    fn total_self_links_edge_weight(&self) -> f64 {
        self.total_self_links_edge_weight
    }
}

impl<'a> IntoIterator for &'a CompactNetwork {
    type Item = CompactNodeItem<'a>;
    type IntoIter = NodeIterator<'a>;

    fn into_iter(self) -> Self::IntoIter {
        NodeIterator {
            compact_network: self,
            current_node: 0,
        }
    }
}

pub struct NodeIterator<'a> {
    compact_network: &'a CompactNetwork,
    current_node: CompactNodeId,
}

impl<'a> Iterator for NodeIterator<'a> {
    type Item = CompactNodeItem<'a>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.current_node == self.compact_network.nodes.len() {
            None
        } else {
            let item = self.compact_network.node(self.current_node);
            self.current_node += 1;
            Some(item)
        }
    }
}

#[derive(Debug)]
pub struct NeighborIterator<'a> {
    compact_network: &'a CompactNetwork,
    neighbor_range: Range<ConnectionId>,
    current_neighbor: ConnectionId,
}

impl Iterator for NeighborIterator<'_> {
    type Item = CompactNeighborItem;

    fn next(&mut self) -> Option<Self::Item> {
        if self.neighbor_range.contains(&self.current_neighbor) {
            let (neighbor_id, edge_weight) = self.compact_network.neighbors[self.current_neighbor];
            let item = CompactNeighborItem {
                connection_id: self.current_neighbor,
                id: neighbor_id,
                edge_weight,
                node_weight: self.compact_network.nodes[neighbor_id].0,
            };
            self.current_neighbor += 1;
            Some(item)
        } else {
            None
        }
    }
}

pub struct SubnetworkIterator<'a, 'b> {
    compact_supernetwork: &'a CompactNetwork, // original data to project a new subnetwork from
    clustering: Clustering,
    clustered_nodes: &'b Vec<Vec<CompactNodeId>>,
    current_clustered_nodes_index: usize, // current clustered nodes to process.  Note that this index does not necessarily imply it is the ClusterId.
    builder: LabeledNetworkBuilder<CompactNodeId>,
    subnetwork_minimum_size: Option<u32>,
}

impl Iterator for SubnetworkIterator<'_, '_> {
    type Item = CompactSubnetworkItem<CompactNodeId>;
    fn next(&mut self) -> Option<Self::Item> {
        let next_valid_position: Option<usize> = match self.subnetwork_minimum_size {
            None => {
                if self.current_clustered_nodes_index == self.clustered_nodes.len() {
                    None
                } else {
                    Some(self.current_clustered_nodes_index)
                }
            }
            Some(subnetwork_minimum_size) => {
                let mut found: bool = false;
                let mut possibly_valid: usize = self.current_clustered_nodes_index;
                while possibly_valid != self.clustered_nodes.len() && !found {
                    if self.clustered_nodes[possibly_valid].len()
                        >= subnetwork_minimum_size as usize
                    {
                        found = true;
                    } else {
                        possibly_valid += 1;
                    }
                }
                if found {
                    Some(possibly_valid)
                } else {
                    None
                }
            }
        };
        match next_valid_position {
            Some(current) => {
                self.current_clustered_nodes_index = current;

                let nodes = &self.clustered_nodes[current];
                let cluster_id: usize = current;

                let edges: Vec<(CompactNodeId, CompactNodeId, f64)> = nodes
                    .iter()
                    .flat_map(|node_in_cluster| {
                        // get all neighbors that belong to it and belong in the same cluster
                        self.compact_supernetwork
                            .neighbors_for(*node_in_cluster)
                            .map(move |neighbor| {
                                (*node_in_cluster, neighbor.id, neighbor.edge_weight)
                            })
                            .filter(|edge| {
                                let edge: (CompactNodeId, CompactNodeId, f64) = *edge;
                                self.clustering[edge.1] == cluster_id
                            })
                    })
                    .collect();

                let labeled_network: LabeledNetwork<CompactNodeId> =
                    self.builder.build(edges.into_iter(), true);

                self.current_clustered_nodes_index += 1;
                Some(CompactSubnetworkItem {
                    id: cluster_id,
                    subnetwork: labeled_network,
                })
            }
            None => {
                self.current_clustered_nodes_index = self.clustered_nodes.len();
                None
            }
        }
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /*
       edges
       0, 1, 2.0
       0, 3, 1.0
       0, 4, 1.0
       1, 0, 2.0
       1, 2, 6.0
       1, 4, 1.0
       1, 5, 4.0
       1, 6, 3.0
       2, 1, 6.0
       2, 6, 3.0
       3, 0, 1.0
       3, 7, 11.0
       4, 0, 1.0
       4, 1, 1.0
       5, 1, 4.0
       6, 1, 3.0
       6, 2, 3.0
       7, 3, 11.0
    */

    fn fresh_network(self_links: f64) -> CompactNetwork {
        let nodes: Vec<CompactNode> = vec![
            (4_f64, 0),
            (16_f64, 3),
            (9_f64, 8),
            (12_f64, 10),
            (2_f64, 12),
            (4_f64, 14),
            (6_f64, 15),
            (11_f64, 17),
        ];
        let neighbors: Vec<CompactNeighbor> = vec![
            (1, 2_f64), // 0
            (3, 1_f64),
            (4, 1_f64),
            (0, 2_f64), // 1
            (2, 6_f64),
            (4, 1_f64),
            (5, 4_f64),
            (6, 3_f64),
            (1, 6_f64), // 2
            (6, 3_f64),
            (0, 1_f64), // 3
            (7, 11_f64),
            (0, 1_f64), // 4
            (1, 1_f64),
            (1, 4_f64), // 5
            (1, 3_f64), // 6
            (2, 3_f64),
            (3, 11_f64), // 7
        ];
        return CompactNetwork::from(nodes, neighbors, self_links);
    }

    #[test]
    fn test_node_iterator() {
        let network: CompactNetwork = fresh_network(0_f64);
        // testing the neighbor iterator is in another test, so we're only validating the IDs and the node weights in this case.
        let expected_like: Vec<(CompactNodeId, f64)> = vec![
            (0, 4_f64),
            (1, 16_f64),
            (2, 9_f64),
            (3, 12_f64),
            (4, 2_f64),
            (5, 4_f64),
            (6, 6_f64),
            (7, 11_f64),
        ];
        let mut index: usize = 0;
        for CompactNodeItem { id, weight, .. } in &network {
            let (expected_id, expected_weight) = expected_like[index];
            assert_eq!(expected_id, id);
            assert_eq!(expected_weight, weight);
            index += 1;
        }
        assert_eq!(8, index);
    }

    #[test]
    fn test_neighbor_iterator() {
        let network: CompactNetwork = fresh_network(0_f64);
        let expected_neighbors: Vec<(CompactNodeId, f64)> =
            vec![(0, 2_f64), (2, 6_f64), (4, 1_f64), (5, 4_f64), (6, 3_f64)];
        let node = network.node(1);
        let actual: Vec<CompactNeighborItem> = node.neighbors().collect();
        assert_eq!(5, actual.len());

        for (index, neighbor) in actual.into_iter().enumerate() {
            let (expected_neighbor_id, expected_edge_weight) = expected_neighbors[index];
            assert_eq!(expected_neighbor_id, neighbor.id);
            assert_eq!(expected_edge_weight, neighbor.edge_weight);
        }
    }

    // #[test]
    // fn test_subnetwork_iterator() {
    //     let network: CompactNetwork = fresh_network(1_f64);
    //     let node_to_cluster: Vec<usize> = vec![0, 1, 1, 0, 0, 1, 1, 0];
    //
    //     let clustering: Clustering = Clustering::as_defined(node_to_cluster, 2);
    //
    //     let subnetwork1: CompactNetwork = CompactNetwork {
    //         nodes: vec![(2_f64, 0), (12_f64, 2), (1_f64, 4), (11_f64, 5)],
    //         neighbors: vec![
    //             (1, 1_f64),
    //             (2, 1_f64),
    //             (0, 1_f64),
    //             (3, 11_f64),
    //             (0, 1_f64),
    //             (1, 11_f64),
    //         ],
    //         total_self_links_edge_weight: 0_f64,
    //     };
    //     let subnetwork2: CompactNetwork = CompactNetwork {
    //         nodes: vec![(13_f64, 0), (9_f64, 3), (4_f64, 5), (6_f64, 6)],
    //         neighbors: vec![
    //             (1, 6_f64),
    //             (2, 4_f64),
    //             (3, 3_f64),
    //             (0, 6_f64),
    //             (3, 3_f64),
    //             (0, 4_f64),
    //             (0, 3_f64),
    //             (1, 3_f64),
    //         ],
    //         total_self_links_edge_weight: 0_f64,
    //     };
    //     let expected: Vec<CompactSubnetworkItem<CompactNodeId>> = vec![
    //         CompactSubnetworkItem {
    //             id: 0,
    //             subnetwork: CompactSubnetwork {
    //                 compact_network: subnetwork1,
    //                 node_id_map: vec![0, 3, 4, 7],
    //             },
    //         },
    //         CompactSubnetworkItem {
    //             id: 1,
    //             subnetwork: CompactSubnetwork {
    //                 compact_network: subnetwork2,
    //                 node_id_map: vec![1, 2, 5, 6],
    //             },
    //         },
    //     ];
    //     let actual: Vec<CompactSubnetworkItem<CompactNodeId>> =
    //         network.subnetworks_iter(&clustering, None).collect();
    //     assert_eq!(expected, actual);
    // }
    //
    // #[test]
    // fn test_subnetwork_filters() {
    //     let network: CompactNetwork = fresh_network(1_f64);
    //     let node_to_cluster: Vec<usize> = vec![0, 1, 1, 0, 0, 1, 1, 2];
    //
    //     let clustering: Clustering = Clustering::as_defined(node_to_cluster, 3);
    //
    //     let subnetwork1: CompactNetwork = CompactNetwork {
    //         nodes: vec![(2_f64, 0), (1_f64, 2), (1_f64, 3)],
    //         neighbors: vec![(1, 1_f64), (2, 1_f64), (0, 1_f64), (0, 1_f64)],
    //         total_self_links_edge_weight: 0_f64,
    //     };
    //     let subnetwork2: CompactNetwork = CompactNetwork {
    //         nodes: vec![(13_f64, 0), (9_f64, 3), (4_f64, 5), (6_f64, 6)],
    //         neighbors: vec![
    //             (1, 6_f64),
    //             (2, 4_f64),
    //             (3, 3_f64),
    //             (0, 6_f64),
    //             (3, 3_f64),
    //             (0, 4_f64),
    //             (0, 3_f64),
    //             (1, 3_f64),
    //         ],
    //         total_self_links_edge_weight: 0_f64,
    //     };
    //     let expected: Vec<CompactSubnetworkItem<CompactNodeId>> = vec![
    //         CompactSubnetworkItem {
    //             id: 0,
    //             subnetwork: CompactSubnetwork {
    //                 compact_network: subnetwork1,
    //                 node_id_map: vec![0, 3, 4],
    //             },
    //         },
    //         CompactSubnetworkItem {
    //             id: 1,
    //             subnetwork: CompactSubnetwork {
    //                 compact_network: subnetwork2,
    //                 node_id_map: vec![1, 2, 5, 6],
    //             },
    //         },
    //     ];
    //     let actual: Vec<CompactSubnetworkItem<CompactNodeId>> =
    //         network.subnetworks_iter(&clustering, Some(3)).collect();
    //     assert_eq!(expected, actual);
    // }
}
