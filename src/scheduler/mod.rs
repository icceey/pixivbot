pub mod engine;

use tokio_cron_scheduler::JobScheduler;
use crate::error::AppResult;

pub async fn init_scheduler() -> AppResult<JobScheduler> {
    let sched = JobScheduler::new().await?;
    
    // sched.add(job).await?;
    
    Ok(sched)
}
