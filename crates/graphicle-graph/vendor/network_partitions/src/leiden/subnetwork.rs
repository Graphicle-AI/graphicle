// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

use rand::Rng;

use super::quality_value_increment::calculate;
use crate::clustering::Clustering;
use crate::errors::CoreError;
use crate::network::prelude::*;

pub const DEFAULT_RANDOMNESS: f64 = 1e-2;

/// Unlike full network clustering, the subnetwork clusterer cannot perform well at large scale
/// if it requests new memory from the heap for each subnetwork.
/// Since the collections used to execute the subnetwork clustering selection algorithm are maintained
/// between subnetworks, they have been purposefully isolated to this class, whose lifespan is for
/// some number of subnetworks.
#[derive(Debug)]
pub struct SubnetworkClusteringGenerator {
    node_processing_order: Vec<usize>,
    neighboring_clusters: Vec<usize>,
    neighboring_cluster_edge_weights: Vec<f64>,
    singleton_clusters: Vec<bool>,
    summed_qvi_records: Vec<f64>,
}

impl SubnetworkClusteringGenerator {
    pub fn with_capacity(capacity: usize) -> SubnetworkClusteringGenerator {
        let node_processing_order: Vec<usize> = Vec::with_capacity(capacity);
        let neighboring_clusters: Vec<usize> = Vec::with_capacity(capacity);
        let neighboring_cluster_edge_weights: Vec<f64> = Vec::with_capacity(capacity);
        let singleton_clusters: Vec<bool> = Vec::with_capacity(capacity);
        let summed_qvi_records: Vec<f64> = Vec::with_capacity(capacity);
        SubnetworkClusteringGenerator {
            node_processing_order,
            neighboring_clusters,
            neighboring_cluster_edge_weights,
            singleton_clusters,
            summed_qvi_records,
        }
    }

    pub fn subnetwork_clustering<T>(
        &mut self,
        subnetwork: &CompactNetwork,
        use_modularity: bool,
        adjusted_resolution: f64,
        randomness: f64,
        rng: &mut T,
    ) -> Result<Clustering, CoreError>
    where
        T: Rng,
    {
        let mut clustering: Clustering = Clustering::as_self_clusters(subnetwork.num_nodes());
        if subnetwork.num_nodes() == 1 {
            return Ok(clustering);
        }
        self.subnetwork_reset(subnetwork.num_nodes(), rng);

        let mut improved: bool = false;

        let mut cluster_weights: Vec<f64> = subnetwork.node_weights();
        let mut external_edge_weight_per_cluster: Vec<f64> = if use_modularity {
            subnetwork.node_weights()
        } else {
            subnetwork.total_edge_weight_per_node()
        };
        let total_node_weight: f64 = subnetwork.total_node_weight();

        let neighboring_clusters: &mut Vec<usize> = self.neighboring_clusters.as_mut();
        let neighboring_cluster_edge_weights: &mut Vec<f64> =
            self.neighboring_cluster_edge_weights.as_mut();
        let summed_qvi_records: &mut Vec<f64> = self.summed_qvi_records.as_mut();
        let singleton_clusters: &mut Vec<bool> = self.singleton_clusters.as_mut();

        for node in &self.node_processing_order {
            let node: usize = *node;
            if node_can_move(
                node,
                &cluster_weights,
                &external_edge_weight_per_cluster,
                total_node_weight,
                singleton_clusters,
                adjusted_resolution,
            ) {
                node_reset(
                    neighboring_clusters,
                    neighboring_cluster_edge_weights,
                    summed_qvi_records,
                    node,
                ); // resets most processing arrays

                cluster_weights[node] = 0_f64;
                external_edge_weight_per_cluster[node] = 0_f64;

                for neighbor in subnetwork.neighbors_for(node) {
                    let neighbor_cluster: usize = clustering.cluster_at(neighbor.id)?;
                    if neighboring_cluster_edge_weights[neighbor_cluster] == 0_f64 {
                        neighboring_clusters.push(neighbor_cluster);
                    }
                    neighboring_cluster_edge_weights[neighbor_cluster] += neighbor.edge_weight;
                }

                let chosen_cluster: usize = best_cluster_for_node(
                    node,
                    subnetwork.node(node).weight,
                    neighboring_clusters,
                    neighboring_cluster_edge_weights,
                    &cluster_weights,
                    &external_edge_weight_per_cluster,
                    total_node_weight,
                    summed_qvi_records,
                    adjusted_resolution,
                    randomness,
                    rng,
                );
                cluster_weights[chosen_cluster] += subnetwork.node_weight(node);

                // TODO: Literally none of this makes sense.  Why would we decrement the edge
                // weight of all nodes in a cluster IF they match, otherwise we increment them?
                // this just doesn't make sense
                for neighbor in subnetwork.neighbors_for(node) {
                    if clustering.cluster_at(neighbor.id)? == chosen_cluster {
                        external_edge_weight_per_cluster[chosen_cluster] -= neighbor.edge_weight;
                    } else {
                        external_edge_weight_per_cluster[chosen_cluster] += neighbor.edge_weight;
                    }
                }

                if chosen_cluster != node {
                    clustering.update_cluster_at(node, chosen_cluster)?;
                    singleton_clusters[chosen_cluster] = false;
                    improved = true;
                }
            }
        }

        if improved {
            clustering.remove_empty_clusters();
        }
        Ok(clustering)
    }

    fn subnetwork_reset<T>(&mut self, length: usize, rng: &mut T)
    where
        T: Rng,
    {
        self.node_processing_order.clear();
        self.neighboring_clusters.clear();
        self.neighboring_cluster_edge_weights.clear();
        self.neighboring_cluster_edge_weights.resize(length, 0_f64);
        self.singleton_clusters.clear();
        self.singleton_clusters.resize(length, true);
        self.summed_qvi_records.clear();

        // set a new node order based on the length requested.
        for i in 0..length {
            self.node_processing_order.push(i);
        }

        for i in 0..length {
            let random_index: usize = rng.gen_range(0..length);
            self.node_processing_order.swap(i, random_index);
        }
    }
}

fn node_can_move(
    node: usize,
    cluster_weights: &[f64],
    external_edge_weight_per_cluster: &[f64],
    total_node_weight: f64,
    singleton_clusters: &[bool],
    adjusted_resolution: f64,
) -> bool {
    let connectivity_threshold: f64 =
        cluster_weights[node] * (total_node_weight - cluster_weights[node]) * adjusted_resolution;
    singleton_clusters[node] && external_edge_weight_per_cluster[node] >= connectivity_threshold
}

fn node_reset(
    neighboring_clusters: &mut Vec<usize>,
    neighboring_cluster_edge_weights: &mut [f64],
    summed_qvi_records: &mut Vec<f64>,
    node: usize,
) {
    for neighboring_cluster in neighboring_clusters.iter() {
        neighboring_cluster_edge_weights[*neighboring_cluster] = 0_f64;
    }
    neighboring_clusters.clear();
    neighboring_clusters.push(node);
    summed_qvi_records.clear();
}

fn best_cluster_for_node<T>(
    node: usize,
    node_weight: f64,
    neighboring_clusters: &[usize],
    neighboring_cluster_edge_weights: &mut [f64],
    cluster_weights: &[f64],
    external_edge_weight_per_cluster: &[f64],
    total_node_weight: f64,
    summed_qvi_records: &mut Vec<f64>,
    adjusted_resolution: f64,
    randomness: f64,
    rng: &mut T,
) -> usize
where
    T: Rng,
{
    let mut best_cluster: usize = node;
    let mut max_qvi: f64 = 0_f64;
    let mut total_adjusted_qvi: f64 = 0_f64;

    for neighboring_cluster in neighboring_clusters {
        let neighboring_cluster: usize = *neighboring_cluster;
        let external_edge_weight: f64 = external_edge_weight_per_cluster[neighboring_cluster];
        let cluster_weight: f64 = cluster_weights[neighboring_cluster];
        if external_edge_weight
            >= cluster_weight * (total_node_weight - cluster_weight) * adjusted_resolution
        {
            let qvi: f64 = calculate(
                neighboring_cluster_edge_weights[neighboring_cluster],
                node_weight,
                cluster_weight,
                adjusted_resolution,
            );
            if qvi > max_qvi {
                best_cluster = neighboring_cluster;
                max_qvi = qvi;
            }
            if qvi >= 0_f64 {
                let adjusted_qvi: f64 = approximate_exponent(qvi / randomness);
                total_adjusted_qvi += adjusted_qvi;
            }
        }
        if !total_adjusted_qvi.is_nan() {
            summed_qvi_records.push(total_adjusted_qvi);
        }
        neighboring_cluster_edge_weights[neighboring_cluster] = 0_f64;
    }

    let chosen_cluster: usize = if total_adjusted_qvi.is_finite() {
        let randomized_transform_qv: f64 = total_adjusted_qvi * rng.gen::<f64>(); // rng.gen will return a number [0.0, 1.0)
        let bin_search_result: Result<usize, usize> = summed_qvi_records
            .binary_search_by(|probe: &f64| probe.partial_cmp(&randomized_transform_qv).unwrap());
        let location: usize = match bin_search_result {
            Ok(location) => location,
            Err(location) => location,
        };
        neighboring_clusters[location]
    } else {
        best_cluster
    };
    chosen_cluster
}

/// Approximate the .exp() function, and more importantly, reduce the amount of times we will get infinite return values
fn approximate_exponent(result: f64) -> f64 {
    if result < -256_f64 {
        0_f64
    } else {
        let mut result = 1_f64 + result / 256_f64;
        result *= result;
        result *= result;
        result *= result;
        result *= result;
        result *= result;
        result *= result;
        result *= result;
        result *= result;
        result
    }
}
