// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

#[cfg(test)]
mod tests {
    use network_partitions::errors::NetworkError;
    use network_partitions::network::prelude::*;

    #[test]
    fn test_load_network_from_file() {
        let org_network_path = "tests/simple_org_graph.csv";
        let broken_network_path = "tests/broken_org_graph.csv";
        let labeled_network: LabeledNetwork<String> =
            LabeledNetwork::<String>::load_from(org_network_path, ",", 0, 1, Some(2), false, true)
                .expect("We should have gotten a properly loaded labeled network from this");
        assert_eq!(10, labeled_network.num_nodes());

        let result: Result<LabeledNetwork<String>, NetworkError> =
            LabeledNetwork::<String>::load_from(
                broken_network_path,
                ",",
                0,
                1,
                Some(2),
                false,
                true,
            );
        assert!(result.is_err());
        match result.err() {
            Some(NetworkError::EdgeFileFormatError) => assert!(true),
            Some(err) => assert!(
                false,
                "Actual NetworkError returned was not EdgeFileFormatError but an {:?}",
                err
            ),
            _ => assert!(
                false,
                "Somehow this file was parsed correctly, which is certainly wrong"
            ),
        }
    }
}
