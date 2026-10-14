// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

use super::full_network_work_queue::FullNetworkWorkQueue;
use super::quality_value_increment;
use crate::clustering::Clustering;
use crate::errors::CoreError;
use crate::leiden::neighboring_clusters::NeighboringClusters;
use crate::network::prelude::*;
use rand::Rng;

pub fn full_network_clustering<T>(
    network: &CompactNetwork,
    clustering: &mut Clustering,
    adjusted_resolution: f64,
    rng: &mut T,
) -> Result<bool, CoreError>
where
    T: Rng,
{
    if network.num_nodes() <= 1 {
        return Ok(false);
    }
    let mut improved: bool = false;
    let (mut cluster_weights, mut num_nodes_per_cluster) =
        weights_and_counts_per_cluster(network, clustering)?;

    let (mut unused_clusters, mut num_unused_clusters) =
        unused_clusters(network, &num_nodes_per_cluster);

    // form a fairly random order of vertices to operate over in a circular queue-like fashion.
    // as we iterate through this queue
    let mut work_queue: FullNetworkWorkQueue =
        FullNetworkWorkQueue::items_in_random_order(network.num_nodes(), rng);

    let mut neighboring_clusters: NeighboringClusters =
        NeighboringClusters::with_capacity(network.num_nodes());

    while !work_queue.is_empty() {
        let current_node: usize = work_queue.pop_front()?;
        let current_cluster: usize = clustering.cluster_at(current_node)?;
        let current_node_weight: f64 = network.node_weight(current_node);

        // temporarily presume we're not in any cluster (we'll add this back later after we've
        // decided on the appropriate cluster to belong to)
        num_unused_clusters = leave_current_cluster(
            current_cluster,
            current_node_weight,
            &mut cluster_weights,
            &mut num_nodes_per_cluster,
            &mut unused_clusters,
            num_unused_clusters,
        );

        // Identify the neighboring clusters of the current node. An empty cluster is also included
        // in the set of neighboring cluster so that it is always possible that the current node
        // can be moved into an empty cluster if the quality value can be increased by it
        // belonging in its own empty cluster
        identify_neighboring_clusters(
            network,
            clustering,
            current_node,
            current_cluster,
            &mut neighboring_clusters,
            &unused_clusters,
            num_unused_clusters,
        )?;

        // Starting with the current cluster being the best cluster, iterate through all of the
        // neighbor clusters - including the empty one - and determine if our max quality function
        // would be increased by making the move or leaving it where it is.
        // if it is better to move it, make the move
        let best_cluster: usize = best_cluster_for(
            current_cluster,
            current_node_weight,
            adjusted_resolution,
            &neighboring_clusters,
            &cluster_weights,
        );

        // Add our current node weight to the current best cluster weight.  This may be the original
        // cluster, but we removed our current node weight from that cluster earlier in the while
        // loop.
        let last_unused_cluster: usize = unused_clusters[num_unused_clusters - 1];
        join_cluster(
            best_cluster,
            current_node_weight,
            &mut cluster_weights,
            &mut num_nodes_per_cluster,
            &mut num_unused_clusters,
            last_unused_cluster,
        );
        if best_cluster != current_cluster {
            improved = true;

            clustering.update_cluster_at(current_node, best_cluster)?;

            // identify any currently stable neighbors that belong to a different cluster than the best
            // cluster for this node and mark them to be checked again.
            //
            // it may be that this new cluster is a better home for our neighbor as well, so we
            // mark it as unstable and we make sure that our neighbor will be iterated over
            // in the node_order queue
            trigger_cluster_change(
                network,
                clustering,
                &mut work_queue,
                current_node,
                best_cluster,
            )?;
        }
    }
    // we may have empty clusters and we need to remove those and compact our numbering scheme to be
    // [0..count(clusters)), so we remove them and ensure our clustering is in optimal condition.
    if improved {
        clustering.remove_empty_clusters();
    }
    Ok(improved)
}

fn weights_and_counts_per_cluster(
    network: &CompactNetwork,
    clustering: &Clustering,
) -> Result<(Vec<f64>, Vec<usize>), CoreError> {
    let mut cluster_weights: Vec<f64> = vec![0_f64; network.num_nodes()];
    let mut num_nodes_per_cluster: Vec<usize> = vec![0; network.num_nodes()];

    for compact_node in network {
        let cluster_id: usize = clustering.cluster_at(compact_node.id)?;
        cluster_weights[cluster_id] += compact_node.weight;
        num_nodes_per_cluster[cluster_id] += 1;
    }
    Ok((cluster_weights, num_nodes_per_cluster))
}

fn unused_clusters(
    network: &CompactNetwork,
    num_nodes_per_cluster: &[usize],
) -> (Vec<usize>, usize) {
    let size: usize = network.num_nodes() - 1;
    let mut unused_clusters: Vec<usize> = vec![0; size];
    let mut num_unused_clusters: usize = 0;
    for i in (0..=size).rev() {
        if num_nodes_per_cluster[i] == 0 {
            unused_clusters[num_unused_clusters] = i;
            num_unused_clusters += 1;
        }
    }
    (unused_clusters, num_unused_clusters)
}

fn leave_current_cluster(
    cluster: usize,
    node_weight: f64,
    cluster_weights: &mut [f64],
    num_nodes_per_cluster: &mut [usize],
    unused_clusters: &mut [usize],
    num_unused_clusters: usize,
) -> usize {
    cluster_weights[cluster] -= node_weight;
    num_nodes_per_cluster[cluster] -= 1;

    if num_nodes_per_cluster[cluster] == 0 {
        unused_clusters[num_unused_clusters] = cluster;
        num_unused_clusters + 1
    } else {
        num_unused_clusters
    }
}

fn identify_neighboring_clusters(
    network: &CompactNetwork,
    clustering: &Clustering,
    current_node: usize,
    current_cluster: usize,
    neighboring_clusters: &mut NeighboringClusters,
    unused_clusters: &[usize],
    num_unused_clusters: usize,
) -> Result<(), CoreError> {
    neighboring_clusters.reset_for_current_cluster(current_cluster);
    let next_unused_cluster: usize = unused_clusters[num_unused_clusters - 1];
    neighboring_clusters.increase_cluster_weight(next_unused_cluster, 0_f64);

    for neighbor in network.neighbors_for(current_node) {
        let neighbor_cluster: usize = clustering.cluster_at(neighbor.id)?;
        neighboring_clusters.increase_cluster_weight(neighbor_cluster, neighbor.edge_weight);
    }
    neighboring_clusters.freeze();
    Ok(())
}

fn best_cluster_for(
    current_cluster: usize,
    current_node_weight: f64,
    adjusted_resolution: f64,
    neighboring_clusters: &NeighboringClusters,
    cluster_weights: &[f64],
) -> usize {
    let mut best_cluster: usize = current_cluster;
    let mut max_quality_value_increment: f64 = quality_value_increment::calculate(
        neighboring_clusters.cluster_weight(current_cluster),
        current_node_weight,
        cluster_weights[current_cluster],
        adjusted_resolution,
    );

    for test_cluster in neighboring_clusters.iter() {
        let test_cluster: usize = *test_cluster;
        let quality_value_increment: f64 = quality_value_increment::calculate(
            neighboring_clusters.cluster_weight(test_cluster),
            current_node_weight,
            cluster_weights[test_cluster],
            adjusted_resolution,
        );
        if quality_value_increment > max_quality_value_increment {
            best_cluster = test_cluster;
            max_quality_value_increment = quality_value_increment;
        }
    }
    best_cluster
}

fn join_cluster(
    cluster: usize,
    node_weight: f64,
    cluster_weights: &mut [f64],
    num_nodes_per_cluster: &mut [usize],
    num_unused_clusters: &mut usize,
    last_unused_cluster: usize,
) {
    cluster_weights[cluster] += node_weight;
    num_nodes_per_cluster[cluster] += 1;

    if cluster == last_unused_cluster {
        *num_unused_clusters -= 1
    }
}

fn trigger_cluster_change(
    network: &CompactNetwork,
    clustering: &Clustering,
    work_queue: &mut FullNetworkWorkQueue,
    node: usize,
    best_cluster: usize,
) -> Result<(), CoreError> {
    for neighbor in network.neighbors_for(node) {
        if clustering.cluster_at(neighbor.id)? != best_cluster {
            work_queue.push_back(neighbor.id);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::network::{Edge, LabeledNetwork};
    use crate::resolution;
    use rand::SeedableRng;
    use rand_xorshift::XorShiftRng;

    #[test]
    fn test_improve_initial_clustering() {
        let mut rng: XorShiftRng = XorShiftRng::seed_from_u64(1234);

        // generate same graph as in java, done via Network object not InternalNetwork, then
        // generate a InternalNetwork from it
        // we should expect 3 clusters, 2 with a light connection and 1 with no connection

        let edges: Vec<Edge> = vec![
            ("dwayne".into(), "nick".into(), 15.0),
            ("nick".into(), "jon".into(), 15.0),
            ("jon".into(), "carolyn".into(), 15.0),
            ("nick".into(), "carolyn".into(), 15.0),
            ("dwayne".into(), "jon".into(), 15.0),
            ("carolyn".into(), "amber".into(), 15.0),
            ("amber".into(), "chris".into(), 15.0),
            ("amber".into(), "nathan".into(), 15.0),
            ("nathan".into(), "chris".into(), 15.0),
            ("jarkko".into(), "thirteen".into(), 15.0),
        ];

        let mut builder: LabeledNetworkBuilder<String> = LabeledNetworkBuilder::new();
        let labeled_network: LabeledNetwork<String> = builder.build(edges.into_iter(), true);

        let mut clustering: Clustering = Clustering::as_self_clusters(labeled_network.num_nodes());

        let adjusted_resolution: f64 =
            resolution::adjust_resolution(Option::None, labeled_network.compact(), true);

        let improved = full_network_clustering(
            labeled_network.compact(),
            &mut clustering,
            adjusted_resolution,
            &mut rng,
        )
        .unwrap();

        assert!(improved);
        let nathan_cluster: usize = clustering
            .cluster_at(labeled_network.compact_id_for("nathan".into()).unwrap())
            .unwrap();
        let dwayne_cluster: usize = clustering
            .cluster_at(labeled_network.compact_id_for("dwayne".into()).unwrap())
            .unwrap();
        let jarkko_cluster: usize = clustering
            .cluster_at(labeled_network.compact_id_for("jarkko".into()).unwrap())
            .unwrap();

        assert_eq!(
            nathan_cluster,
            clustering
                .cluster_at(labeled_network.compact_id_for("chris".into()).unwrap())
                .unwrap(),
            "Expected chris in nathan cluster"
        );
        assert_eq!(
            nathan_cluster,
            clustering
                .cluster_at(labeled_network.compact_id_for("amber".into()).unwrap())
                .unwrap(),
            "Expected amber in nathan cluster"
        );
        assert_eq!(
            dwayne_cluster,
            clustering
                .cluster_at(labeled_network.compact_id_for("jon".into()).unwrap())
                .unwrap(),
            "Expected jon in dwayne cluster"
        );
        assert_eq!(
            dwayne_cluster,
            clustering
                .cluster_at(labeled_network.compact_id_for("nick".into()).unwrap())
                .unwrap(),
            "Expected nick in dwayne cluster"
        );
        assert_eq!(
            dwayne_cluster,
            clustering
                .cluster_at(labeled_network.compact_id_for("carolyn".into()).unwrap())
                .unwrap(),
            "Expected carolyn in dwayne cluster"
        );

        let nodes_per_cluster = clustering.num_nodes_per_cluster();
        assert_eq!(
            2, nodes_per_cluster[jarkko_cluster],
            "Jarkko cluster {} somehow had {} nodes in the cluster, but there should be 2",
            jarkko_cluster, nodes_per_cluster[jarkko_cluster]
        );
    }
}
