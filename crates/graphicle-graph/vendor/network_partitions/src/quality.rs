// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

use super::resolution::adjust_resolution;
use crate::clustering::Clustering;
use crate::errors::CoreError;
use crate::network::prelude::*;

pub fn quality(
    network: &CompactNetwork,
    clustering: &Clustering,
    resolution: Option<f64>,
    use_modularity: bool,
) -> Result<f64, CoreError> {
    let adjusted_resolution: f64 = adjust_resolution(resolution, network, use_modularity);

    let mut quality: f64 = 0_f64;

    let mut cluster_weights: Vec<f64> = vec![0_f64; clustering.next_cluster_id()];

    for node in network {
        let node_cluster: usize = clustering.cluster_at(node.id)?;
        cluster_weights[node_cluster] += node.weight;
        for neighbor in node.neighbors() {
            let neighbor_cluster: usize = clustering.cluster_at(neighbor.id)?;
            if neighbor_cluster == node_cluster {
                quality += neighbor.edge_weight;
            }
        }
    }

    for cluster_weight in cluster_weights {
        quality -= cluster_weight.powi(2) * adjusted_resolution;
    }

    quality /= 2_f64 * network.total_edge_weight() + network.total_self_links_edge_weight();

    Ok(quality)
}
