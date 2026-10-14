pub trait NetworkDetails {
    fn num_nodes(&self) -> usize;

    fn num_edges(&self) -> usize;

    fn total_node_weight(&self) -> f64;

    fn total_edge_weight(&self) -> f64;

    fn total_self_links_edge_weight(&self) -> f64;
}
