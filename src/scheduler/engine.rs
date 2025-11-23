use crate::db::repo::Repo;
use std::sync::Arc;

pub struct SchedulerEngine {
    repo: Arc<Repo>,
}

impl SchedulerEngine {
    pub fn new(repo: Arc<Repo>) -> Self {
        Self { repo }
    }

    pub async fn tick(&self) {
        // Find tasks due for execution
        // Execute task
        // Schedule next run
    }
}
