// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

#[allow(non_upper_case_globals)]
pub const calculate: fn(f64, f64, f64, f64) -> f64 = as_reference_impl;

#[allow(dead_code)] // there will inherently be dead code in this module
pub fn as_reference_impl(
    cluster_edge_weights: f64,
    node_weight: f64,
    cluster_weight: f64,
    adjusted_resolution: f64,
) -> f64 {
    cluster_edge_weights - (node_weight * cluster_weight * adjusted_resolution)
}

#[allow(dead_code)] // there will inherently be dead code in this module
pub fn as_paper(
    cluster_edge_weights: f64,
    node_weight: f64,
    cluster_weight: f64,
    adjusted_resolution: f64,
) -> f64 {
    cluster_edge_weights - ((node_weight + cluster_weight) * adjusted_resolution)
}
