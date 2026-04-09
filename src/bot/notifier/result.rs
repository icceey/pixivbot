#[derive(Debug, Clone)]
pub struct BatchSendResult {
    pub succeeded_indices: Vec<usize>,
    pub failed_indices: Vec<usize>,
    /// The first message ID from the batch (for tracking/reply purposes)
    pub first_message_id: Option<i32>,
}

impl BatchSendResult {
    pub(super) fn all_failed(total: usize) -> Self {
        Self {
            succeeded_indices: Vec::new(),
            failed_indices: (0..total).collect(),
            first_message_id: None,
        }
    }

    pub fn is_complete_success(&self) -> bool {
        self.failed_indices.is_empty()
    }

    pub fn is_complete_failure(&self) -> bool {
        self.succeeded_indices.is_empty()
    }
}
