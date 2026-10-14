// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

use std::collections::HashMap;

use super::super::clustering::Clustering;
use super::super::errors::CoreError;
use super::super::safe_vectors::SafeVectors;

#[derive(Debug, PartialEq)]
pub struct Network {
    node_to_neighbor_offsets: Vec<usize>,
    node_weights: Vec<f64>,
    contiguous_neighbors: Vec<usize>,
    contiguous_edge_weights: Vec<f64>,
    /// these are the bidirectional lookups from the original Network to this representation
    node_to_index: HashMap<String, usize>,
    index_to_node: Vec<String>,
    total_edge_weight_self_links: f64,
}

impl Network {
    pub fn new(
        node_to_neighbor_offsets: Vec<usize>,
        node_weights: Vec<f64>,
        contiguous_neighbors: Vec<usize>,
        contiguous_edge_weights: Vec<f64>,
        node_to_index: HashMap<String, usize>,
        index_to_node: Vec<String>,
        total_edge_weight_self_links: f64,
    ) -> Network {
        return Network {
            node_to_neighbor_offsets,
            node_weights,
            contiguous_neighbors,
            contiguous_edge_weights,
            node_to_index,
            index_to_node,
            total_edge_weight_self_links,
        };
    }

    pub fn num_nodes(&self) -> usize {
        return self.node_to_neighbor_offsets.len();
    }

    pub fn num_edges(&self) -> usize {
        return self.contiguous_neighbors.len() / 2 as usize;
    }

    pub fn node_weight_at(
        &self,
        node: usize,
    ) -> Result<f64, CoreError> {
        return self
            .node_weights
            .get_or_err(node, CoreError::InternalNetworkIndexingError);
    }

    pub fn neighbor_range(
        &self,
        node_index: usize,
    ) -> Result<(usize, usize), CoreError> {
        let start_of_contiguous_neighbors: usize = self
            .node_to_neighbor_offsets
            .get_or_err(node_index, CoreError::InternalNetworkIndexingError)?;
        let end_of_contiguous_neighbors: usize =
            if node_index + 1 == self.node_to_neighbor_offsets.len() {
                self.contiguous_neighbors.len()
            } else {
                self.node_to_neighbor_offsets[node_index + 1]
            };
        return Ok((start_of_contiguous_neighbors, end_of_contiguous_neighbors));
    }

    fn internal_neighbor_range(
        &self,
        node_index: usize,
    ) -> (usize, usize) {
        // this can only be called internally, so we can avoid our result checks since we know
        // we are build correctly
        let start_of_contiguous_neighbors: usize = self.node_to_neighbor_offsets[node_index];
        let end_of_contiguous_neighbors: usize =
            if node_index + 1 == self.node_to_neighbor_offsets.len() {
                self.contiguous_neighbors.len()
            } else {
                self.node_to_neighbor_offsets[node_index + 1]
            };
        return (start_of_contiguous_neighbors, end_of_contiguous_neighbors);
    }

    pub fn edges_for(
        &self,
        node_index: usize,
    ) -> Result<(&[usize], &[f64]), CoreError> {
        let (start_of_contiguous_neighbors, end_of_contiguous_neighbors) =
            self.neighbor_range(node_index)?;

        return if self
            .contiguous_neighbors
            .is_valid_range(end_of_contiguous_neighbors)
            && self
                .contiguous_edge_weights
                .is_valid_range(end_of_contiguous_neighbors)
        {
            let neighbor_indices: &[usize] = &self.contiguous_neighbors
                [start_of_contiguous_neighbors..end_of_contiguous_neighbors];
            let edge_weights: &[f64] = &self.contiguous_edge_weights
                [start_of_contiguous_neighbors..end_of_contiguous_neighbors];
            Ok((neighbor_indices, edge_weights))
        } else {
            Err(CoreError::InternalNetworkIndexingError)
        };
    }

    pub fn neighbors_for(
        &self,
        node_index: usize,
    ) -> Result<&[usize], CoreError> {
        let (start_of_contiguous_neighbors, end_of_contiguous_neighbors) =
            self.neighbor_range(node_index)?;
        let neighbor_indices: &[usize] =
            &self.contiguous_neighbors[start_of_contiguous_neighbors..end_of_contiguous_neighbors];
        return Ok(neighbor_indices);
    }

    pub fn weights_for(
        &self,
        node_index: usize,
    ) -> Result<&[f64], CoreError> {
        let (start_of_contiguous_neighbors, end_of_contiguous_neighbors) =
            self.neighbor_range(node_index)?;
        let edge_weights: &[f64] = &self.contiguous_edge_weights
            [start_of_contiguous_neighbors..end_of_contiguous_neighbors];
        return Ok(edge_weights);
    }

    pub fn edge_at(
        &self,
        node_index: usize,
    ) -> Result<(usize, f64), CoreError> {
        return if self.contiguous_neighbors.is_safe_access(node_index)
            && self.contiguous_edge_weights.is_safe_access(node_index)
        {
            Ok((
                self.contiguous_neighbors[node_index].clone(),
                self.contiguous_edge_weights[node_index].clone(),
            ))
        } else {
            Err(CoreError::InternalNetworkIndexingError)
        };
    }

    pub fn neighbor_at(
        &self,
        neighbor_node_index: usize,
    ) -> Result<usize, CoreError> {
        return self
            .contiguous_neighbors
            .get_or_err(neighbor_node_index, CoreError::InternalNetworkIndexingError);
    }

    pub fn weight_at(
        &self,
        neighbor_node_index: usize,
    ) -> Result<f64, CoreError> {
        return self
            .contiguous_edge_weights
            .get_or_err(neighbor_node_index, CoreError::InternalNetworkIndexingError);
    }

    /// Based on the provided clustering, create InternalNetwork for each cluster based on the nodes
    /// only in that cluster and this InternalNetwork topology
    pub fn subnetworks_for_clustering(
        &self,
        clustering: &Clustering,
    ) -> Result<Vec<Network>, CoreError> {
        // the only nodes that appear within a subnetwork are nodes within that specific cluster
        let mut subnetworks: Vec<Network> = Vec::with_capacity(clustering.next_cluster_id());
        let nodes_per_cluster: Vec<Vec<usize>> = clustering.nodes_per_cluster();
        let total_edge_weight_self_links: f64 = 0_f64; // for some reason, the java version always set the subnetwork self loops to 0. it's unclear why.
        for cluster_index in 0..clustering.next_cluster_id() {
            let mut node_to_index: HashMap<String, usize> = HashMap::new();
            let nodes_within_cluster: &Vec<usize> = &nodes_per_cluster[cluster_index];

            let mut index_to_node: Vec<String> = Vec::with_capacity(nodes_within_cluster.len());

            let mut node_to_neighbor_offsets: Vec<usize> = vec![0; nodes_within_cluster.len()];
            let mut node_weights: Vec<f64> = vec![0.0; nodes_within_cluster.len()];
            let mut contiguous_neighbors: Vec<usize> = Vec::new();
            let mut contiguous_edge_weights: Vec<f64> = Vec::new();

            let mut old_index_lookup: HashMap<usize, usize> = HashMap::new();
            for i in 0..nodes_within_cluster.len() {
                old_index_lookup.insert(nodes_within_cluster[i], i);
            }
            for i in 0..nodes_within_cluster.len() {
                let node_within_cluster: usize = nodes_within_cluster[i];
                let node: &str = self.index_to_node[node_within_cluster].as_str();
                let node_label: String = String::from(node);
                node_to_index.insert(node_label, i);
                let node_label: String = String::from(node);
                index_to_node.push(node_label);
                node_to_neighbor_offsets[i] = contiguous_neighbors.len();
                node_weights[i] = self.node_weights[node_within_cluster];
                let (neighbors, weights): (&[usize], &[f64]) =
                    self.edges_for(node_within_cluster)?;
                for i in 0..neighbors.len() {
                    let neighbor: usize = neighbors[i];
                    let weight: f64 = weights[i];
                    if clustering.cluster_at(neighbor)? == cluster_index {
                        let updated_index: usize = old_index_lookup.get(&neighbor).unwrap().clone();
                        contiguous_neighbors.push(updated_index);
                        contiguous_edge_weights.push(weight);
                    }
                }
            }
            subnetworks.push(Network {
                node_to_index,
                node_weights,
                node_to_neighbor_offsets,
                contiguous_neighbors,
                contiguous_edge_weights,
                index_to_node,
                total_edge_weight_self_links,
            });
        }
        return Ok(subnetworks);
    }

    pub fn induce_clustering_network(
        &self,
        clustering: &Clustering,
    ) -> Result<Network, CoreError> {
        let mut node_to_index: HashMap<String, usize> = HashMap::new();

        // nodes per cluster is a mapping of cluster: [node1,node2,...] in that cluster.
        let nodes_per_cluster: Vec<Vec<usize>> = clustering.nodes_per_cluster();
        let cluster_count: usize = clustering.next_cluster_id();
        if nodes_per_cluster.len() != cluster_count {
            return Err(CoreError::UnsafeInducementError);
        }

        let mut node_to_neighbor_offsets: Vec<usize> = vec![0; cluster_count];
        let mut node_weights: Vec<f64> = vec![0.0; cluster_count];
        let mut index_to_node: Vec<String> = Vec::with_capacity(cluster_count);
        // we can't pre-allocate the neighbors because we have no idea how many edges we'll have yet
        let mut contiguous_neighbors: Vec<usize> = Vec::new();
        let mut contiguous_edge_weights: Vec<f64> = Vec::new();

        let mut total_edge_weight_self_links: f64 = self.total_edge_weight_self_links.clone();

        for cluster_index in 0..cluster_count {
            node_to_neighbor_offsets[cluster_index] = contiguous_neighbors.len();
            let node_label: String = cluster_index.to_string();
            node_to_index.insert(node_label.clone(), cluster_index);
            index_to_node.push(node_label.clone());

            let mut cluster_to_cluster_edge_weights: HashMap<usize, f64> = HashMap::new();

            let cluster_nodes: &Vec<usize> = &nodes_per_cluster[cluster_index];

            for i in 0..cluster_nodes.len() {
                let node_in_cluster: usize = cluster_nodes[i];
                node_weights[cluster_index] += self.node_weights[node_in_cluster];
                // get all neighbors of node in cluster
                let (neighbors, weights): (&[usize], &[f64]) = self.edges_for(node_in_cluster)?;
                for j in 0..neighbors.len() {
                    let neighbor: usize = neighbors[j];
                    let weight: f64 = weights[j];
                    let neighbor_node_cluster: usize = clustering.cluster_at(neighbor)?;

                    if cluster_index != neighbor_node_cluster {
                        let cluster_edge_weight: f64 = *cluster_to_cluster_edge_weights
                            .entry(neighbor_node_cluster)
                            .or_insert(0.0);
                        cluster_to_cluster_edge_weights
                            .insert(neighbor_node_cluster, cluster_edge_weight + weight);
                    } else {
                        total_edge_weight_self_links += weight;
                    }
                }
            }
            let cluster_neighbors_count: usize = cluster_to_cluster_edge_weights.len();
            contiguous_neighbors.reserve(cluster_neighbors_count);
            contiguous_edge_weights.reserve(cluster_neighbors_count);
            let mut sorted_neighbor_clusters: Vec<(usize, f64)> =
                cluster_to_cluster_edge_weights.into_iter().collect();
            sorted_neighbor_clusters.sort_unstable_by(|a, b| a.0.cmp(&b.0));
            for (neighbor_node_cluster, weight) in sorted_neighbor_clusters {
                contiguous_neighbors.push(neighbor_node_cluster);
                contiguous_edge_weights.push(weight);
            }
        }
        return Ok(Network {
            node_to_index,
            node_weights,
            node_to_neighbor_offsets,
            contiguous_neighbors,
            contiguous_edge_weights,
            index_to_node,
            total_edge_weight_self_links,
        });
    }

    pub fn total_edge_weight(&self) -> f64 {
        return self.contiguous_edge_weights.iter().sum::<f64>() / 2_f64;
    }

    pub fn total_edge_weight_self_links(&self) -> f64 {
        return self.total_edge_weight_self_links;
    }

    pub fn total_node_weight(&self) -> f64 {
        return self.node_weights.iter().sum::<f64>();
    }

    pub fn node_weights(&self) -> Vec<f64> {
        return self.node_weights.clone();
    }

    pub fn total_edge_weight_per_node(&self) -> Vec<f64> {
        let mut per_node: Vec<f64> = Vec::with_capacity(self.num_nodes());
        for i in 0..self.num_nodes() {
            let (start_neighbor_index, end_neighbor_index) = self.internal_neighbor_range(i);
            let weight_slice: &[f64] =
                &self.contiguous_edge_weights[start_neighbor_index..end_neighbor_index];
            per_node.push(weight_slice.iter().sum::<f64>());
        }
        return per_node;
    }

    pub fn node_name(
        &self,
        index: usize,
    ) -> Result<String, CoreError> {
        return self
            .index_to_node
            .get_or_err(index, CoreError::InternalNetworkIndexingError);
    }

    pub fn cloned_node_to_index(&self) -> HashMap<String, usize> {
        return self.node_to_index.clone();
    }

    pub fn index_for_name(
        &self,
        node: &str,
    ) -> Option<usize> {
        return self.node_to_index.get(node).cloned();
    }

    pub fn edges(&self) -> Vec<(String, String, f64)> {
        let mut edge_vec: Vec<(String, String, f64)> =
            Vec::with_capacity(self.contiguous_neighbors.len());
        for (source_node, source_index) in &self.node_to_index {
            let (start_neighbor_index, end_neighbor_index) =
                self.internal_neighbor_range(*source_index);
            for neighbor_index in start_neighbor_index..end_neighbor_index {
                let target_index: usize = self.contiguous_neighbors[neighbor_index];
                let target_node: &str = &self.index_to_node[target_index];
                let weight: f64 = self.contiguous_edge_weights[neighbor_index];
                let edge: (String, String, f64) = (
                    String::from(source_node.clone()),
                    String::from(target_node),
                    weight.clone(),
                );
                edge_vec.push(edge);
            }
        }
        return edge_vec;
    }
}

#[cfg(test)]
pub mod tests {
    use super::super::NetworkBuilder;
    use super::Network;
    use crate::clustering::Clustering;
    use std::collections::HashMap;

    #[test]
    fn test_from_builder() {
        let fast: Network = NetworkBuilder::for_modularity()
            .add_edge_into("jon", "nick", 10.0)
            .add_edge_into("dwayne", "nick", 2.0)
            .add_edge_into("carolyn", "nick", 5.0)
            .add_edge_into("carolyn", "amber", 1.0)
            .add_edge_into("chris", "david", 8.0)
            .add_edge_into("chris", "nathan", 12.0)
            .build();

        assert!(
            fast.node_to_index.get("dwayne").is_some(),
            "There should have been a dwayne node in the lookup"
        );
        assert!(
            fast.node_to_index.get("nick").is_some(),
            "There should have been a nick node in the lookup"
        );
        assert!(
            fast.node_to_index.get("bob").is_none(),
            "There wasn't supposed to be a bob in the lookup"
        );
    }

    fn insert_node(
        node_label: &str,
        node_to_index: &mut HashMap<String, usize>,
        index_to_node: &mut Vec<String>,
    ) {
        node_to_index.insert(String::from(node_label), index_to_node.len());
        index_to_node.push(String::from(node_label));
    }

    fn insert_all(
        labels: Vec<&str>,
        node_to_index: &mut HashMap<String, usize>,
        index_to_node: &mut Vec<String>,
    ) {
        for label in labels {
            insert_node(label, node_to_index, index_to_node);
        }
    }

    fn make_fast_network() -> Network {
        let mut node_to_index: HashMap<String, usize> = HashMap::new();
        let mut index_to_node: Vec<String> = Vec::with_capacity(9);
        insert_all(
            vec![
                "jon", "nick", "dwayne", "carolyn", "amber", "chris", "david", "nathan",
            ],
            &mut node_to_index,
            &mut index_to_node,
        );
        // edge("jon", "nick", 10.0); 0, 1
        // edge("dwayne", "nick", 2.0); 2, 1
        // edge("carolyn", "nick", 5.0); 3, 1
        // edge("carolyn", "amber", 1.0); 3, 4
        // edge("chris", "david", 8.0); 5, 6
        // edge("chris", "nathan", 12.0); 5, 7
        // edge("chris", "amber", 4.0); 5, 4
        let node_to_neighbor_offsets: Vec<usize> = vec![
            0,  // 0 - jon
            1,  // 0 - nick
            4,  // 1 - dwayne
            5,  // 0 - carolyn
            7,  // 2 - amber
            9,  // 2 - chris
            12, // 2 - david
            13, // 2 - nathan
        ];
        let node_weights: Vec<f64> = vec![10.0, 17.0, 2.0, 6.0, 5.0, 24.0, 8.0, 12.0];
        let contiguous_neighbors: Vec<usize> = vec![
            1, // jon to nick, cluster 0 to 0
            0, 2, 3, // nick to jon, dwayne, and carolyn, cluster 0 to 0 and 0 to 1
            1, // dwayne to nick, cluster 1 to 0
            1, 4, // carolyn to nick and amber, cluster 0 to 0 and 0 to 2
            3, 5, // amber to carolyn and chris, cluster 2 to 2 and 2 to 0
            4, 6, 7, // chris to amber, david, and nathan, cluster 2 to 2
            5, // david to chris, cluster 2 to 2
            5, // nathan to chris, cluster 2 to 2
        ];
        let contiguous_edge_weights: Vec<f64> = vec![
            10.0, // jon to nick
            10.0, 2.0, 5.0, // nick to jon, dwayne, and carolyn
            2.0, // dwayne to nick
            5.0, 1.0, // carolyn to nick and amber
            1.0, 4.0, // amber to carolyn and chris
            4.0, 8.0, 12.0, // chris to amber, david, and nathan
            8.0,  // david to chris
            12.0, // nathan to chris
        ];
        let fast_network: Network = Network {
            node_to_index,
            index_to_node,
            node_to_neighbor_offsets,
            contiguous_neighbors,
            contiguous_edge_weights,
            node_weights,
            total_edge_weight_self_links: 1_f64,
        };
        return fast_network;
    }

    #[test]
    fn test_induce_clustering_network() {
        let fast_network: Network = make_fast_network();
        let clustering: Clustering = Clustering::as_defined(vec![0, 0, 1, 0, 2, 2, 2, 2], 3);
        // ensure that the induced graph includes a singleton cluster for the disconnected component, 2 reasonable clusters, and a cluster with a single node in it
        let induced = fast_network.induce_clustering_network(&clustering).unwrap();
        assert_eq!(3, induced.num_nodes());

        let mut expected: Vec<HashMap<usize, f64>> = Vec::with_capacity(3);
        let expected_weights: Vec<f64> = vec![33.0, 2.0, 49.0];
        let mut edges: HashMap<usize, f64> = HashMap::new();

        edges.insert(1, 2.0);
        edges.insert(2, 1.0);
        expected.push(edges);
        // 30 + 48 = 78.0

        edges = HashMap::new();
        edges.insert(0, 2.0);
        expected.push(edges);

        edges = HashMap::new();
        edges.insert(0, 1.0);
        expected.push(edges);

        assert_eq!(induced.node_weights, expected_weights);
        assert_eq!(induced.total_edge_weight_self_links, 79_f64);

        for i in 0..induced.num_nodes() {
            let expected_edges: &HashMap<usize, f64> = &expected[i];
            assert_eq!(induced.node_weights[i], expected_weights[i]);
            let (neighbors, weights) = induced.edges_for(i).unwrap();
            let mut actual: HashMap<usize, f64> = HashMap::new();
            for i in 0..neighbors.len() {
                actual.insert(neighbors[i], weights[i]);
            }
            assert_eq!(
                expected_edges, &actual,
                "Cluster {}, expected edges {:?} and got {:?}.",
                i, expected_edges, &actual
            );
        }
    }

    #[test]
    fn test_subnetworks_for_clustering() {
        let fast_network: Network = make_fast_network();
        let clustering: Clustering = Clustering::as_defined(vec![0, 0, 1, 0, 2, 2, 2, 2], 3);
        let subnetworks: Vec<Network> = fast_network
            .subnetworks_for_clustering(&clustering)
            .unwrap();

        let mut expected_1_vi: HashMap<String, usize> = HashMap::new();
        let mut expected_1_iv: Vec<String> = Vec::new();
        insert_all(
            vec!["jon", "nick", "carolyn"],
            &mut expected_1_vi,
            &mut expected_1_iv,
        );

        let mut expected_2_vi: HashMap<String, usize> = HashMap::new();
        let mut expected_2_iv: Vec<String> = Vec::new();
        insert_node("dwayne", &mut expected_2_vi, &mut expected_2_iv);

        let mut expected_3_vi: HashMap<String, usize> = HashMap::new();
        let mut expected_3_iv: Vec<String> = Vec::new();
        insert_all(
            vec!["amber", "chris", "david", "nathan"],
            &mut expected_3_vi,
            &mut expected_3_iv,
        );

        let expected: Vec<Network> = vec![
            Network {
                node_to_index: expected_1_vi,
                index_to_node: expected_1_iv,
                node_weights: vec![10.0, 17.0, 6.0],
                node_to_neighbor_offsets: vec![0, 1, 3],
                contiguous_neighbors: vec![1, 0, 2, 1],
                contiguous_edge_weights: vec![10.0, 10.0, 5.0, 5.0],
                total_edge_weight_self_links: 0_f64,
            },
            Network {
                node_to_index: expected_2_vi,
                index_to_node: expected_2_iv,
                node_weights: vec![2.0],
                node_to_neighbor_offsets: vec![0],
                contiguous_neighbors: vec![],
                contiguous_edge_weights: vec![],
                total_edge_weight_self_links: 0_f64,
            },
            Network {
                node_to_index: expected_3_vi,
                index_to_node: expected_3_iv,
                node_weights: vec![5.0, 24.0, 8.0, 12.0],
                node_to_neighbor_offsets: vec![0, 1, 4, 5],
                contiguous_neighbors: vec![1, 0, 2, 3, 1, 1],
                contiguous_edge_weights: vec![4.0, 4.0, 8.0, 12.0, 8.0, 12.0],
                total_edge_weight_self_links: 0_f64,
            },
        ];
        assert_eq!(subnetworks, expected);

        assert_eq!(3, subnetworks[0].num_nodes());
        assert_eq!(1, subnetworks[1].num_nodes());
        assert_eq!(4, subnetworks[2].num_nodes());
    }
}
