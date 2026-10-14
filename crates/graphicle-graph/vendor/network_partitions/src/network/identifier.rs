// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

use std::collections::HashMap;
use std::hash::Hash;

pub struct Identifier<T> {
    original_to_new: HashMap<T, usize>,
    new_to_original: Vec<T>,
}

impl<T> Default for Identifier<T>
where
    T: Clone + Hash + Eq,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Identifier<T>
where
    T: Clone + Hash + Eq,
{
    pub fn new() -> Self {
        let map: HashMap<T, usize> = HashMap::new();
        let id: Identifier<T> = Identifier {
            original_to_new: map,
            new_to_original: Vec::new(),
        };
        id
    }

    pub fn identify(&mut self, original: T) -> usize {
        match self.original_to_new.get(&original) {
            Some(id) => *id,
            None => {
                let new_id: usize = self.new_to_original.len();
                self.original_to_new.insert(original.clone(), new_id);
                self.new_to_original.push(original);
                new_id
            }
        }
    }

    pub fn identity_map(&self) -> Vec<T> {
        self.new_to_original.clone()
    }

    pub fn clear(&mut self) {
        self.new_to_original.clear();
        self.original_to_new.clear();
    }

    pub fn finish(&mut self) -> (HashMap<T, usize>, Vec<T>) {
        let id_to_label: HashMap<T, usize> = self.original_to_new.clone();
        let label_to_id: Vec<T> = self.new_to_original.clone();

        self.original_to_new.clear();
        self.new_to_original.clear();

        (id_to_label, label_to_id)
    }
}
