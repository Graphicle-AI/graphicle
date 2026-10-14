// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

use super::errors::CoreError;

pub trait SafeVectors<T> {
    fn is_safe_access(&self, index: usize) -> bool;

    fn is_valid_range(&self, index: usize) -> bool;

    fn get_or_err(&self, index: usize, err: CoreError) -> Result<T, CoreError>;
}

impl<T: Clone> SafeVectors<T> for Vec<T> {
    fn is_safe_access(&self, index: usize) -> bool {
        index < self.len()
    }

    fn is_valid_range(&self, index: usize) -> bool {
        index <= self.len()
    }

    fn get_or_err(&self, index: usize, err: CoreError) -> Result<T, CoreError> {
        self.get(index).cloned().ok_or(err)
    }
}
