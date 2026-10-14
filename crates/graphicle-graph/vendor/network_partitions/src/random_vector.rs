// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

use rand::Rng;

/// Generates a Vec of length `length`, initially populated with values from 0..length.
/// Executes `length` number of swaps based on current position and an index chosen at random
pub fn random_permutation<T>(length: usize, rng: &mut T) -> Vec<usize>
where
    T: Rng,
{
    let mut permutation: Vec<usize> = Vec::with_capacity(length);
    for i in 0..length {
        permutation.push(i);
    }

    for i in 0..length {
        let random_index: usize = rng.gen_range(0..length);
        permutation.swap(i, random_index);
    }

    permutation
}
