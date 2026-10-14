// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

use std::convert::From;
use std::fs;
use std::io;

#[derive(Debug)]
pub enum NetworkError {
    EdgeFileFormatError,
    EmptyEdgeFileError,
    IoError(io::Error),
    IoReadError(io::Lines<io::BufReader<fs::File>>),
}

impl From<io::Error> for NetworkError {
    fn from(err: io::Error) -> NetworkError {
        NetworkError::IoError(err)
    }
}

impl From<io::Lines<io::BufReader<fs::File>>> for NetworkError {
    fn from(err: io::Lines<io::BufReader<fs::File>>) -> NetworkError {
        NetworkError::IoReadError(err)
    }
}

#[derive(Clone, Debug)]
pub enum CoreError {
    ClusterIndexingError,
    EmptyNetworkError,
    InternalNetworkIndexingError,
    ParameterRangeError,
    UnsafeInducementError,
    QueueError,
}
