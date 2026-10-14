// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

use std::slice::Iter;

pub struct NeighboringClusters {
    neighboring_clusters: Vec<usize>,
    neighbor_edge_weights_within_cluster: Vec<f64>,
    current_cluster: Option<usize>,
}

impl NeighboringClusters {
    pub fn with_capacity(length: usize) -> NeighboringClusters {
        NeighboringClusters {
            neighboring_clusters: Vec::with_capacity(length + 1),
            neighbor_edge_weights_within_cluster: vec![f64::NAN; length + 1],
            current_cluster: None,
        }
    }

    pub fn reset_for_current_cluster(&mut self, current_cluster: usize) {
        if let Some(current_cluster) = self.current_cluster {
            self.neighbor_edge_weights_within_cluster[current_cluster] = f64::NAN;
            for cluster in &self.neighboring_clusters {
                self.neighbor_edge_weights_within_cluster[*cluster] = f64::NAN;
            }
            self.neighboring_clusters.clear();
        }
        self.current_cluster = Some(current_cluster);
    }

    pub fn increase_cluster_weight(&mut self, cluster: usize, node_weight: f64) {
        if self.neighbor_edge_weights_within_cluster[cluster].is_nan() {
            // we've never seen this cluster before, we can safely add it to our "set" of neighboring clusters.
            self.neighboring_clusters.push(cluster);
            self.neighbor_edge_weights_within_cluster[cluster] = 0_f64;
        }
        self.neighbor_edge_weights_within_cluster[cluster] += node_weight;
    }

    pub fn freeze(&mut self) {
        // only set the weight for the current cluster if no other neighbors belong to it.
        if let Some(current_cluster) = self.current_cluster {
            if self.neighbor_edge_weights_within_cluster[current_cluster].is_nan() {
                self.neighbor_edge_weights_within_cluster[current_cluster] = 0_f64;
            }
        }
    }

    pub fn cluster_weight(&self, cluster: usize) -> f64 {
        self.neighbor_edge_weights_within_cluster[cluster]
    }

    pub fn iter(&self) -> Iter<'_, usize> {
        self.neighboring_clusters.iter()
    }
}
