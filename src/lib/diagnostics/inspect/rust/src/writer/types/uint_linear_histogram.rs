// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::writer::{
    ArithmeticArrayProperty, ArrayProperty, HistogramProperty, InspectType, Node, UintArrayProperty,
};
use diagnostics_hierarchy::{ArrayFormat, LinearHistogramParams};
use inspect_format::constants;
use log::error;
use std::borrow::Cow;

#[derive(Debug, Default)]
/// A linear histogram property for unsigned integer values.
pub struct UintLinearHistogramProperty {
    array: UintArrayProperty,
    floor: u64,
    slots: usize,
    step_size: u64,
}

impl InspectType for UintLinearHistogramProperty {}

impl UintLinearHistogramProperty {
    pub(crate) fn new(
        name: Cow<'_, str>,
        params: LinearHistogramParams<u64>,
        parent: &Node,
    ) -> Self {
        let slots = params.buckets + constants::LINEAR_HISTOGRAM_EXTRA_SLOTS;
        let array = parent.create_uint_array_internal(name, slots, ArrayFormat::LinearHistogram);
        array.set(0, params.floor);
        array.set(1, params.step_size);
        Self { floor: params.floor, step_size: params.step_size, slots, array }
    }

    fn get_index(&self, value: u64) -> usize {
        let mut current_floor = self.floor;
        // Start in the underflow index.
        let mut index = constants::LINEAR_HISTOGRAM_EXTRA_SLOTS - 2;
        while value >= current_floor && index < self.slots - 1 {
            current_floor += self.step_size;
            index += 1;
        }
        index
    }
}

impl HistogramProperty for UintLinearHistogramProperty {
    type Type = u64;

    fn insert(&self, value: u64) {
        self.insert_multiple(value, 1);
    }

    fn insert_multiple(&self, value: u64, count: usize) {
        self.array.add(self.get_index(value), count as u64);
    }

    fn clear(&self) {
        if let Some(ref inner_ref) = self.array.inner.inner_ref() {
            // Ensure we don't delete the array slots that contain histogram metadata.
            inner_ref
                .state
                .try_lock()
                .and_then(|mut state| {
                    // -2 = the overflow and underflow slots which still need to be cleared.
                    state.clear_array(
                        inner_ref.block_index,
                        constants::LINEAR_HISTOGRAM_EXTRA_SLOTS - 2,
                    )
                })
                .unwrap_or_else(|err| {
                    error!(err:?; "Failed to clear property");
                });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::writer::testing_utils::GetBlockExt;
    use crate::writer::Inspector;
    use inspect_format::{Array, Uint};

    #[fuchsia::test]
    fn uint_linear_histogram() {
        let inspector = Inspector::default();
        let root = inspector.root();
        let node = root.create_child("node");
        {
            let uint_histogram = node.create_uint_linear_histogram(
                "uint-histogram",
                LinearHistogramParams { floor: 10, step_size: 5, buckets: 5 },
            );
            uint_histogram.insert_multiple(0, 2); // underflow
            uint_histogram.insert(25);
            uint_histogram.insert(500); // overflow
            uint_histogram.array.get_block::<_, Array<Uint>>(|block| {
                for (i, value) in [10, 5, 2, 0, 0, 0, 1, 0, 1].iter().enumerate() {
                    assert_eq!(block.get(i).unwrap(), *value);
                }
            });

            uint_histogram.clear();
            uint_histogram.array.get_block::<_, Array<Uint>>(|block| {
                for (i, value) in [10, 5, 0, 0, 0, 0, 0, 0, 0].iter().enumerate() {
                    assert_eq!(*value, block.get(i).unwrap());
                }
            });

            node.get_block::<_, inspect_format::Node>(|node_block| {
                assert_eq!(node_block.child_count(), 1);
            });
        }
        node.get_block::<_, inspect_format::Node>(|node_block| {
            assert_eq!(node_block.child_count(), 0);
        });
    }
}
