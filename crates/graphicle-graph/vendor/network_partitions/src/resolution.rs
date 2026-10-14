// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

use crate::network::prelude::*;

pub const DEFAULT_RESOLUTION: f64 = 1_f64;

/// The original implementation of the Leiden enhancement to Louvain used the same quality value
/// term measurements whether they were using CPM or Modularity for the maximization function.
///
/// If use_modularity is true, we need to scale resolution so that it's appropriate when used in
/// the maximization functions.  We scale it by 1 / (2 * (total_edge_weight + total_edge_weight_self_links)).
///
/// If use_modularity is false, we do nothing.
///
/// In either case, if the user doesn't specify a resolution, we use a default of 1.0 (though it may be scaled for modularity)
pub fn adjust_resolution(
    resolution: Option<f64>,
    network: &CompactNetwork,
    use_modularity: bool,
) -> f64 {
    let resolution: f64 = resolution.unwrap_or(DEFAULT_RESOLUTION);
    if use_modularity {
        // Note: this is adjusted from the version @
        // https://github.com/CWTSLeiden/networkanalysis/blob/master/src/cwts/networkanalysis/run/RunNetworkClustering.java#L331
        // which seems to be a bug since this resolution factor when used for modularity is
        // `resolution / 2m`, where m is the total edge weights of the graph.
        resolution
            / (2_f64 * (network.total_edge_weight() + network.total_self_links_edge_weight()))
    } else {
        resolution
    }
}
