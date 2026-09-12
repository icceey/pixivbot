use crate::config::EhentaiConfig;
use crate::db::{entities::eh_gallery_jobs, repo::Repo};
use anyhow::{Context, Result};
use eh_client::{rewrite_ipfs_gateway_nodes, TelegraphClient, TelegraphRewriteData};
use std::sync::Arc;
use tracing::{error, info, warn};

#[derive(serde::Deserialize)]
#[serde(untagged)]
enum PersistedTelegraphRewriteData {
    Single(TelegraphRewriteData),
    Batch(Vec<TelegraphRewriteData>),
}

pub struct EhTelegraphRewriteWorker {
    repo: Arc<Repo>,
    telegraph: Arc<TelegraphClient>,
    send_archive: bool,
    config: Arc<EhentaiConfig>,
}

impl EhTelegraphRewriteWorker {
    pub fn new(
        repo: Arc<Repo>,
        telegraph: Arc<TelegraphClient>,
        send_archive: bool,
        config: Arc<EhentaiConfig>,
    ) -> Self {
        Self {
            repo,
            telegraph,
            send_archive,
            config,
        }
    }

    pub async fn run(self) {
        let poll = self.config.download_poll_interval_sec.max(10);
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(poll));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            interval.tick().await;
            if let Err(e) = self.tick().await {
                error!("EhTelegraphRewriteWorker tick error: {:#}", e);
            }
        }
    }

    pub(super) async fn tick(&self) -> Result<()> {
        let job = self.repo.get_next_eh_job_for_telegraph_rewrite().await?;
        let Some(job) = job else {
            return Ok(());
        };
        let generation = job
            .telegraph_rewrite_started_at
            .context("claimed shared EH Telegraph rewrite is missing its generation")?;

        if let Err(e) = self.process(&job).await {
            error!("Telegraph rewrite failed for job {}: {:#}", job.id, e);
            let terminal = self
                .repo
                .schedule_eh_job_telegraph_rewrite_retry(
                    job.id,
                    generation,
                    &format!("{:#}", e),
                    self.config.max_retry_count,
                )
                .await?;
            if terminal {
                self.repo
                    .evaluate_eh_job_liveness(job.id, self.send_archive)
                    .await?;
                warn!(
                    "Telegraph rewrite permanently failed for job {} after retries",
                    job.id
                );
            }
            return Ok(());
        }

        if self
            .repo
            .mark_eh_job_telegraph_rewritten(job.id, generation)
            .await?
        {
            self.repo
                .evaluate_eh_job_liveness(job.id, self.send_archive)
                .await?;
            info!(
                "Rewrote Telegraph page URLs for shared EH gid={} job {}",
                job.gid, job.id
            );
        } else {
            warn!(
                "Ignoring stale completion for shared EH Telegraph rewrite job {}",
                job.id
            );
        }

        Ok(())
    }

    pub(super) async fn process(&self, job: &eh_gallery_jobs::Model) -> Result<()> {
        let data_json = job
            .telegraph_rewrite_data
            .as_deref()
            .context("shared job telegraph_rewrite_data missing for claimed rewrite")?;
        let data: PersistedTelegraphRewriteData = serde_json::from_str(data_json)
            .context("Failed to deserialize Telegraph rewrite data")?;
        let rewrites = match data {
            PersistedTelegraphRewriteData::Single(data) => vec![data],
            PersistedTelegraphRewriteData::Batch(data) => data,
        };

        let mut failed_rewrites = 0_usize;
        let mut first_error = None;
        for data in rewrites {
            for page in &data.pages {
                let content = rewrite_ipfs_gateway_nodes(
                    &page.content,
                    &data.preview_gateway_url,
                    &data.public_gateway_url,
                );
                if let Err(error) = self
                    .telegraph
                    .edit_page(&page.path, &page.title, &content)
                    .await
                    .with_context(|| format!("Failed to edit Telegraph page {}", page.path))
                {
                    failed_rewrites += 1;
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                    break;
                }
            }
        }
        if let Some(error) = first_error {
            return Err(error.context(format!(
                "{failed_rewrites} independent Telegraph rewrite payload(s) failed"
            )));
        }
        Ok(())
    }
}
