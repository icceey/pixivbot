use crate::utils::caption::MAX_PER_GROUP;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContinuationNumbering {
    pub first_batch_number: usize,
    pub total_batches: usize,
}

impl ContinuationNumbering {
    pub fn new(first_batch_number: usize, total_batches: usize) -> Self {
        Self {
            first_batch_number,
            total_batches,
        }
    }

    pub(super) fn for_item_count(total_items: usize) -> Self {
        Self {
            first_batch_number: 1,
            total_batches: total_items.div_ceil(MAX_PER_GROUP),
        }
    }

    pub(super) fn display_batch_number(self, batch_idx: usize) -> usize {
        self.first_batch_number + batch_idx
    }
}
