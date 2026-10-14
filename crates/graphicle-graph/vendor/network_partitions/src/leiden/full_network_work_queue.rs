// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

use crate::errors::CoreError;
use std::collections::VecDeque;

use rand::Rng;

/// The FullNetworkWorkQueue is a composite class of a circular work queue and a vec of bools indicating
/// when a node should be treated as stable
///
/// Node stability is a prerequisite for being added to the work queue, for if it is unstable
/// it likewise means it is already on the work queue.
///
/// On `pop_front()`, presuming the work queue is not empty, the value is retrieved from the work queue,
/// and immediately marked as stable; this guarantees consistency within this object.
///
/// If a recoverable error occurs while processing this item, the onus is on the user to
/// reinsert the item via `push_front`.
#[derive(Debug, PartialEq)]
pub struct FullNetworkWorkQueue {
    work_queue: VecDeque<usize>,
    stable: Vec<bool>,
}

impl FullNetworkWorkQueue {
    #[allow(dead_code)]
    pub fn new() -> FullNetworkWorkQueue {
        FullNetworkWorkQueue {
            work_queue: VecDeque::new(),
            stable: Vec::new(),
        }
    }

    /// Generates a random order from [0..len) in the work queue, and initializes the stability
    /// vector to be full of unstable nodes.
    /// This is the primary constructor to be used.
    /// First we're going to allocate a Vector that is precisely 1 more than the length requested
    /// This is because the `impl From<Vec<T>> for VecDeque<T>` in the stdlib will forgo making a
    /// copy of the Vec's buffer if it is sized at least 1 more than the used length
    /// Warning: Rust tells us not to rely on this:
    /// > This avoids reallocating where possible, but the conditions for that are strict, and subject
    /// > to change, and so shouldn't be relied upon unless the Vec<T> came from From<VecDeque<T>>
    /// > and hasn't been reallocated.
    ///
    /// However, creation of this item is called infrequently, and our worst case scenario is 2 O(n)s
    /// instead of 1 O(n). We'll use the speed boost now, but this may be worth looking into for
    /// speed sake periodically to verify that the actual current Rust impl of the `From` trait for
    /// Vec<T> to VecDeque<T> hasn't changed the implementation.  As of the time of writing this,
    /// the place to check is https://doc.rust-lang.org/src/alloc/collections/vec_deque.rs.html#2742-2772
    pub fn items_in_random_order<T>(len: usize, rng: &mut T) -> FullNetworkWorkQueue
    where
        T: Rng,
    {
        let mut permutation: Vec<usize> = Vec::with_capacity(len + 1);
        for i in 0..len {
            permutation.push(i);
        }
        let mut stable: Vec<bool> = Vec::with_capacity(len);
        for i in 0..len {
            stable.push(false);
            let random_index: usize = rng.gen_range(0..len);
            permutation.swap(i, random_index);
        }
        let work_queue: VecDeque<usize> = VecDeque::from(permutation);
        FullNetworkWorkQueue { work_queue, stable }
    }

    /// Presuming the work queue contains a value, pops it from that queue, marks the node as stable,
    /// and returns it.
    /// If the work queue has no items on it, returns a CoreError::QueueError.
    pub fn pop_front(&mut self) -> Result<usize, CoreError> {
        let front: usize = self.work_queue.pop_front().ok_or(CoreError::QueueError)?;
        self.stable[front] = true;
        Ok(front)
    }

    /// If the item to be added to the work queue is not already on it, add it to the queue
    /// We determine if it is on the queue by first checking whether it is marked as stable or not
    pub fn push_back(&mut self, item: usize) {
        // check sizing
        if self.stable.len() <= item {
            // increase the size to at least include item+1, and set all of the values to be stable
            // this shouldn't be happening, and if it is, I need to know about it
            self.stable.resize(item + 1, true);
        }
        if self.stable[item] {
            self.stable[item] = false;
            self.work_queue.push_back(item);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.work_queue.is_empty()
    }

    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.work_queue.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::SeedableRng;
    use rand_xorshift::XorShiftRng;

    #[test]
    fn test_determinism() {
        let mut rng1: XorShiftRng = XorShiftRng::seed_from_u64(1234);
        let mut rng2: XorShiftRng = XorShiftRng::seed_from_u64(1234);
        let order_1: FullNetworkWorkQueue =
            FullNetworkWorkQueue::items_in_random_order(100000, &mut rng1);
        let order_2: FullNetworkWorkQueue =
            FullNetworkWorkQueue::items_in_random_order(100000, &mut rng2);
        assert_eq!(order_1, order_2);
    }
}
