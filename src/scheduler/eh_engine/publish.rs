use crate::bot::notifier::Notifier;
use crate::config::EhentaiConfig;
use crate::db::repo::eh_download_queue::{EhDeliveryClaim, EH_CHAT_LOCKS, STATUS_PUBLISHING};
use crate::db::repo::eh_gallery_jobs::{
    EhMissingZipResetOutcome, JOB_STATUS_DOWNLOADED, TELEGRAPH_STATUS_READY,
};
use crate::db::{
    entities::{eh_download_queue, eh_gallery_jobs},
    repo::Repo,
};
use crate::scheduler::helpers::get_chat_if_should_notify;
use anyhow::{Context, Result};
use eh_client::EhClient;
use std::sync::Arc;
use tokio::task::JoinSet;
use tracing::{error, info, warn};

pub struct EhPublishWorker {
    repo: Arc<Repo>,
    notifier: Notifier,
    client: Arc<EhClient>,
    rewrite_delay_sec: Option<u64>,
    config: Arc<EhentaiConfig>,
    #[cfg(test)]
    publish_send_hook: Option<EhPublishSendHook>,
}

#[cfg(test)]
#[derive(Clone)]
pub(super) struct EhPublishSendHook {
    pub(super) entered: Arc<tokio::sync::Notify>,
    pub(super) release: Arc<tokio::sync::Notify>,
    pub(super) after_done: Option<EhPublishCompletionHook>,
}

#[cfg(test)]
#[derive(Clone)]
pub(super) struct EhPublishCompletionHook {
    pub(super) entered: Arc<tokio::sync::Notify>,
    pub(super) release: Arc<tokio::sync::Notify>,
}

impl EhPublishWorker {
    pub fn new(
        repo: Arc<Repo>,
        notifier: Notifier,
        client: Arc<EhClient>,
        rewrite_delay_sec: Option<u64>,
        config: Arc<EhentaiConfig>,
    ) -> Self {
        Self {
            repo,
            notifier,
            client,
            rewrite_delay_sec,
            config,
            #[cfg(test)]
            publish_send_hook: None,
        }
    }

    #[cfg(test)]
    pub(super) fn with_test_send_hook(mut self, publish_send_hook: EhPublishSendHook) -> Self {
        self.publish_send_hook = Some(publish_send_hook);
        self
    }

    pub async fn run(self) {
        let poll = self.config.download_poll_interval_sec.max(10);
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(poll));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            interval.tick().await;
            if let Err(e) = self.tick().await {
                error!("EhPublishWorker tick error: {:#}", e);
            }
        }
    }

    pub(super) async fn tick(&self) -> Result<()> {
        let concurrency = self.config.publish_concurrency_clamped();
        let mut tasks = JoinSet::new();
        let mut no_more_claims = false;
        let mut first_error = None;

        loop {
            while tasks.len() < concurrency && !no_more_claims {
                match self
                    .repo
                    .get_next_eh_delivery_for_publish(self.config.send_archive)
                    .await
                {
                    Ok(Some(claim)) => {
                        let worker = Self {
                            repo: Arc::clone(&self.repo),
                            notifier: self.notifier.clone(),
                            client: Arc::clone(&self.client),
                            rewrite_delay_sec: self.rewrite_delay_sec,
                            config: Arc::clone(&self.config),
                            #[cfg(test)]
                            publish_send_hook: self.publish_send_hook.clone(),
                        };
                        tasks.spawn(async move { worker.process_claimed(claim).await });
                    }
                    Ok(None) => {
                        no_more_claims = true;
                        break;
                    }
                    Err(error) => {
                        error!("Failed to claim shared EH publish delivery: {:#}", error);
                        if first_error.is_none() {
                            first_error = Some(error);
                        }
                        no_more_claims = true;
                        break;
                    }
                };
            }

            let Some(result) = tasks.join_next().await else {
                break;
            };
            match result {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    error!("Shared EH publish delivery task failed: {:#}", error);
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                }
                Err(error) => {
                    let error =
                        anyhow::Error::new(error).context("shared EH publish task join failed");
                    error!("Shared EH publish delivery task join failed: {:#}", error);
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                }
            }
        }

        if let Some(error) = first_error {
            Err(error.context("one or more shared EH publish deliveries failed"))
        } else {
            Ok(())
        }
    }

    pub(super) async fn process_claimed(&self, claim: EhDeliveryClaim) -> Result<()> {
        let _chat_guard = EH_CHAT_LOCKS.lock_chat(claim.delivery.chat_id).await;
        let Some(EhDeliveryClaim { delivery, job }) = self
            .repo
            .get_eh_delivery_publish_claim(claim.delivery.id, self.config.send_archive)
            .await?
        else {
            return Ok(());
        };
        if !self
            .repo
            .eh_delivery_is_active(delivery.id, STATUS_PUBLISHING, self.config.send_archive)
            .await?
        {
            info!(
                "Skipping inactive shared EH publish delivery {} for chat {}",
                delivery.id, delivery.chat_id
            );
            return Ok(());
        }
        if get_chat_if_should_notify(&self.repo, delivery.chat_id)
            .await?
            .is_none()
        {
            self.repo
                .defer_eh_delivery_publish(
                    delivery.id,
                    self.config.download_poll_interval_sec as i64,
                )
                .await?;
            info!(
                "Deferred shared EH publish delivery {} because chat {} is not notifiable",
                delivery.id, delivery.chat_id
            );
            return Ok(());
        }

        let archive_required = self.config.send_archive && delivery.archive_sent_at.is_none();
        let telegraph_required = delivery.telegraph && delivery.telegraph_sent_at.is_none();
        if telegraph_required
            && (job.telegraph_status != TELEGRAPH_STATUS_READY || job.telegraph_url.is_none())
        {
            self.repo
                .defer_eh_delivery_publish(
                    delivery.id,
                    self.config.download_poll_interval_sec as i64,
                )
                .await?;
            return Ok(());
        }
        if archive_required && (job.status != JOB_STATUS_DOWNLOADED || job.zip_path.is_none()) {
            self.repo
                .defer_eh_delivery_publish(
                    delivery.id,
                    self.config.download_poll_interval_sec as i64,
                )
                .await?;
            return Ok(());
        }

        if archive_required {
            let zip_path = job.zip_path.as_deref().expect("archive path checked above");
            if !std::path::Path::new(zip_path).exists() {
                self.handle_missing_zip(&delivery, &job).await?;
                return Ok(());
            }
        }

        let chat_id = teloxide::types::ChatId(delivery.chat_id);
        if archive_required {
            let zip_path = std::path::Path::new(
                job.zip_path
                    .as_deref()
                    .expect("archive path checked before send"),
            );
            let caption = self.build_caption(&delivery);
            let filename = format!(
                "{}.zip",
                delivery
                    .title
                    .replace(['/', '\\', ':', '*', '?', '"', '<', '>', '|'], "_")
            );
            #[cfg(test)]
            if let Some(hook) = &self.publish_send_hook {
                hook.entered.notify_one();
                hook.release.notified().await;
            }
            if let Err(error) = self
                .notifier
                .send_document(chat_id, zip_path, &filename, &caption)
                .await
                .context("Failed to send archive document")
            {
                self.retry_delivery_after_send_failure(&delivery, error)
                    .await?;
                return Ok(());
            }
            self.repo.mark_eh_archive_delivery_sent(delivery.id).await?;
        }

        if telegraph_required {
            let telegraph_url = job
                .telegraph_url
                .as_deref()
                .expect("Telegraph readiness checked before send");
            let link_text = format!(
                "📄 [Telegraph 链接]({})",
                teloxide::utils::markdown::escape_link_url(telegraph_url)
            );
            if let Err(error) = self
                .notifier
                .send_text(chat_id, &link_text, false)
                .await
                .context("Failed to send Telegraph link")
            {
                self.retry_delivery_after_send_failure(&delivery, error)
                    .await?;
                return Ok(());
            }
            self.repo
                .mark_eh_telegraph_delivery_sent(
                    delivery.id,
                    job.id,
                    self.rewrite_delay_sec.map(|delay| delay as i64),
                )
                .await?;
        }

        self.repo
            .mark_eh_delivery_done(delivery.id, job.id, self.config.send_archive)
            .await?;
        #[cfg(test)]
        if let Some(hook) = self
            .publish_send_hook
            .as_ref()
            .and_then(|hook| hook.after_done.as_ref())
        {
            hook.entered.notify_one();
            hook.release.notified().await;
        }
        info!(
            "Published shared EH gallery gid={} job={} to chat {}",
            job.gid, job.id, delivery.chat_id
        );
        Ok(())
    }

    pub(super) async fn handle_missing_zip(
        &self,
        delivery: &eh_download_queue::Model,
        job: &eh_gallery_jobs::Model,
    ) -> Result<()> {
        self.repo
            .defer_eh_delivery_publish(delivery.id, self.config.download_poll_interval_sec as i64)
            .await?;
        let expected_zip_path = job
            .zip_path
            .as_deref()
            .context("Missing shared EH ZIP reset requires a persisted path")?;
        let expected_started_at = job
            .started_at
            .context("Missing shared EH ZIP reset requires a persisted generation")?;
        let outcome = self
            .repo
            .reset_eh_job_for_missing_zip(
                job.id,
                expected_started_at,
                expected_zip_path,
                self.config.max_retry_count,
            )
            .await?;
        match outcome {
            EhMissingZipResetOutcome::Reset => {
                warn!(
                    "Reset shared EH job {} after its cached ZIP disappeared during delivery {}",
                    job.id, delivery.id
                );
            }
            EhMissingZipResetOutcome::Exhausted => {
                // Terminal download failures stay log + /estatus: no user
                // notification is sent here.
                error!(
                    "Shared EH job {} for gid {} exhausted missing-ZIP retries; failing it and its active deliveries",
                    job.id, job.gid
                );
            }
            EhMissingZipResetOutcome::Stale => {}
        }
        Ok(())
    }

    async fn retry_delivery_after_send_failure(
        &self,
        delivery: &eh_download_queue::Model,
        error: anyhow::Error,
    ) -> Result<()> {
        let (_updated, terminal) = self
            .repo
            .schedule_eh_delivery_retry(
                delivery.id,
                &format!("{:#}", error),
                self.config.max_retry_count,
                self.config.send_archive,
            )
            .await?;
        if terminal {
            warn!(
                "Shared EH publish delivery {} for chat {} exhausted retries: {:#}",
                delivery.id, delivery.chat_id, error
            );
        } else {
            warn!(
                "Shared EH publish delivery {} for chat {} will retry: {:#}",
                delivery.id, delivery.chat_id, error
            );
        }
        Ok(())
    }

    fn build_caption(&self, entry: &eh_download_queue::Model) -> String {
        let title = teloxide::utils::markdown::escape(&entry.title);
        let base_url = self.client.base_url();
        let gallery_url = format!(
            "{}/g/{}/{}",
            base_url.trim_end_matches('/'),
            entry.gid,
            entry.token
        );
        let url_escaped = teloxide::utils::markdown::escape_link_url(&gallery_url);
        format!("📦 {}\n\n🔗 [来源]({})", title, url_escaped)
    }
}
