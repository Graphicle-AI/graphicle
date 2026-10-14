// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

use super::network::Network;
use crate::errors::NetworkError;
#[allow(unused_imports)]
use crate::log;

use std::boxed::Box;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufReader, Read};

type NodeWeightResolver = Box<fn(f64, f64) -> f64>;

fn modularity_node_weight_resolver(
    node_weight: f64,
    edge_weight: f64,
) -> f64 {
    return node_weight + edge_weight;
}

fn cpm_node_weight_resolver(
    _node_weight: f64,
    _edge_weight: f64,
) -> f64 {
    return 1_f64;
}

#[derive(Clone, Debug)]
pub struct NetworkBuilder {
    pub node_to_index: HashMap<String, usize>,
    pub index_to_node: Vec<String>,
    pub node_neighbors: Vec<HashSet<usize>>,
    pub edges: HashMap<(usize, usize), f64>,
    pub node_weights: Vec<f64>,
    pub total_edge_weight_self_links: f64,
    node_weight_resolver: NodeWeightResolver,
}

impl NetworkBuilder {
    pub fn for_modularity() -> NetworkBuilder {
        return NetworkBuilder::builder(true);
    }

    pub fn for_cpm() -> NetworkBuilder {
        return NetworkBuilder::builder(false);
    }

    pub fn builder(use_modularity: bool) -> NetworkBuilder {
        let node_weight_resolver: NodeWeightResolver = if use_modularity {
            Box::new(modularity_node_weight_resolver)
        } else {
            Box::new(cpm_node_weight_resolver)
        };
        return NetworkBuilder {
            node_to_index: HashMap::new(),
            index_to_node: Vec::new(),
            node_neighbors: Vec::new(),
            edges: HashMap::new(),
            node_weights: Vec::new(),
            total_edge_weight_self_links: 0_f64,
            node_weight_resolver,
        };
    }

    pub fn build(self) -> Network {
        return Network::from(self);
    }

    fn add_directed_edge(
        mut self,
        source_index: usize,
        target_index: usize,
        weight: f64,
    ) -> NetworkBuilder {
        if source_index != target_index {
            let edge_weight: &mut f64 = self
                .edges
                .entry((source_index, target_index))
                .or_insert(0_f64);
            self.node_neighbors[source_index].insert(target_index);
            *edge_weight += weight;
        } else {
            self.total_edge_weight_self_links += weight;
        }
        self.node_weights[source_index] =
            (*self.node_weight_resolver)(self.node_weights[source_index], weight);
        return self;
    }

    pub fn add(
        self,
        edge: (String, String, f64),
    ) -> NetworkBuilder {
        return self.add_edge(edge.0, edge.1, edge.2);
    }

    pub fn add_into(
        self,
        edge: (&str, &str, f64),
    ) -> NetworkBuilder {
        return self.add_edge_into(edge.0, edge.1, edge.2);
    }

    pub fn add_edge(
        mut self,
        source: String,
        target: String,
        weight: f64,
    ) -> NetworkBuilder {
        let source_index: usize = self.id_for(source);
        let target_index: usize = self.id_for(target);
        return self
            .add_directed_edge(source_index, target_index, weight)
            .add_directed_edge(target_index, source_index, weight);
    }

    pub fn add_edge_into(
        self,
        source: &str,
        target: &str,
        weight: f64,
    ) -> NetworkBuilder {
        return self.add_edge(source.into(), target.into(), weight);
    }

    fn id_for(
        &mut self,
        node: String,
    ) -> usize {
        let node_owned: String = node.clone();
        let index = match self.node_to_index.get(&node) {
            Some(found) => found.clone(),
            None => {
                let current_length: usize = self.index_to_node.len();
                self.node_to_index.insert(node, current_length);
                self.index_to_node.push(node_owned);
                self.node_neighbors.push(HashSet::new());
                self.node_weights.push(0_f64);
                current_length
            }
        };
        return index;
    }

    pub fn load_from_file(
        mut self,
        path: &str,
        separator: &str,
        source_index: usize,
        target_index: usize,
        weight_index: Option<usize>,
        skip_first_line: bool,
    ) -> Result<NetworkBuilder, NetworkError> {
        let minimum_required_length: usize = source_index
            .max(target_index)
            .max(weight_index.unwrap_or(target_index))
            + 1;
        let mut reader: BufReader<File> = BufReader::new(File::open(path)?);
        let mut contents = String::new();
        reader.read_to_string(&mut contents)?;
        for (line_number, line) in contents.lines().enumerate() {
            if !line.is_empty() && !(skip_first_line && line_number == 0) {
                let splits: Vec<&str> = line.split(separator).collect();
                if splits.len() < minimum_required_length {
                    return Err(NetworkError::EdgeFileFormatError);
                }
                let source: &str = splits[source_index];
                let target: &str = splits[target_index];
                let weight: f64 = match weight_index {
                    Some(weight_index) => splits[weight_index]
                        .parse::<f64>()
                        .map_err(|_err| NetworkError::EdgeFileFormatError)?,
                    None => 1_f64,
                };
                self = self.add_edge_into(source, target, weight);
            }
        }
        return Ok(self);
    }

    pub fn get_edge_weight(
        &self,
        source: &str,
        target: &str,
    ) -> Option<f64> {
        return self.node_to_index.get(source).and_then(|source_index| {
            self.node_to_index.get(target).and_then(|target_index| {
                self.edges
                    .get(&(source_index.clone(), target_index.clone()))
                    .cloned()
            })
        });
    }

    pub fn num_nodes(&self) -> usize {
        return self.node_weights.len();
    }

    pub fn from(
        mut self,
        edges: Vec<(String, String, f64)>,
    ) -> NetworkBuilder {
        for (source, target, weight) in edges {
            self = self.add_edge(source, target, weight);
        }
        return self;
    }
}

impl From<NetworkBuilder> for Network {
    fn from(builder: NetworkBuilder) -> Self {
        let node_to_index: HashMap<String, usize> = builder.node_to_index;
        let index_to_node: Vec<String> = builder.index_to_node;
        let node_weights: Vec<f64> = builder.node_weights;

        // Still need to figure out why some creation routines end up with total edge weight self
        // links as 0_f64 (subnetworks in specific)
        let total_edge_weight_self_links: f64 = if builder.total_edge_weight_self_links == 0_f64 {
            1_f64
        } else {
            builder.total_edge_weight_self_links
        };

        let mut node_to_neighbor_offsets: Vec<usize> = Vec::with_capacity(index_to_node.len());
        let mut contiguous_neighbors: Vec<usize> = Vec::with_capacity(builder.edges.len());
        let mut contiguous_edge_weights: Vec<f64> = Vec::with_capacity(builder.edges.len());

        for node in 0..index_to_node.len() {
            let mut neighbors: Vec<usize> = builder.node_neighbors[node].iter().cloned().collect();
            neighbors.sort_unstable();
            node_to_neighbor_offsets.push(contiguous_neighbors.len());
            for neighbor in neighbors {
                let weight: f64 = builder.edges.get(&(node, neighbor)).unwrap().clone();
                contiguous_neighbors.push(neighbor);
                contiguous_edge_weights.push(weight);
            }
        }

        return Network::new(
            node_to_neighbor_offsets,
            node_weights,
            contiguous_neighbors,
            contiguous_edge_weights,
            node_to_index,
            index_to_node,
            total_edge_weight_self_links,
        );
    }
}
