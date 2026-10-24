use crate::bot::notifier::Notifier;
use crate::config::SchedulerConfig;
use crate::db::entities::{chats, subscriptions, tasks};
use crate::db::repo::pixiv_tag::{TagDelivery, TagScan, TagSettings};
use crate::db::repo::Repo;
use crate::db::types::TaskType;
use crate::pixiv::client::{PixivClient, SEARCH_TIMEZONE};
use crate::scheduler::helpers::{
    apply_subscription_tag_filter, get_chat_if_should_notify, process_illust_push, PushResult,
    INTER_SUBSCRIPTION_DELAY_MS,
};
use anyhow::Result;
use chrono::{DateTime, Days, Local, NaiveDate, Utc};
use pixiv_client::{Illust, ImageSize};
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::time::{sleep, Duration};
use tracing::{error, warn};

#[cfg(test)]
mod tests;

pub struct PixivTagEngine {
    repo: Arc<Repo>,
    pixiv: Arc<RwLock<PixivClient>>,
    notifier: Notifier,
    tick_interval_sec: u64,
    window_days: u16,
    window_hours: i64,
    scan_target_hours: u16,
    scan_pages: u32,
    min_interval_sec: i64,
    max_interval_sec: i64,
    image_size: ImageSize,
}

impl PixivTagEngine {
    pub fn new(
        repo: Arc<Repo>,
        pixiv: Arc<RwLock<PixivClient>>,
        notifier: Notifier,
        config: &SchedulerConfig,
        image_size: ImageSize,
    ) -> Self {
        Self {
            repo,
            pixiv,
            notifier,
            tick_interval_sec: config.tick_interval_sec,
            window_days: config.pixiv_tag_window_days,
            window_hours: i64::from(config.pixiv_tag_window_days) * 24,
            scan_target_hours: config.pixiv_tag_scan_target_hours,
            scan_pages: config.pixiv_tag_scan_pages,
            min_interval_sec: i64::from(config.pixiv_tag_scan_min_interval_minutes) * 60,
            max_interval_sec: i64::from(config.pixiv_tag_scan_max_interval_minutes) * 60,
            image_size,
        }
    }

    pub async fn run(&self) {
        let mut interval =
            tokio::time::interval(Duration::from_secs(self.tick_interval_sec.max(1)));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            if let Err(e) = self.tick().await {
                error!("Pixiv tag scheduler failed: {:#}", e);
            }
        }
    }

    async fn tick(&self) -> Result<()> {
        for task in self
            .repo
            .get_pending_tasks_by_type(TaskType::PixivTag, None)
            .await?
        {
            let result: Result<()> = async {
                // Reserve a retry delay before network work, including on restart.
                self.repo
                    .update_task_after_poll(
                        task.id,
                        Local::now() + chrono::Duration::seconds(self.min_interval_sec),
                    )
                    .await?;
                let now = Utc::now();
                let oldest = now.with_timezone(&SEARCH_TIMEZONE).date_naive()
                    - Days::new(u64::from(self.window_days));
                let mut scan = self
                    .repo
                    .pixiv_tag_scan(task.id, oldest, self.window_days)
                    .await?;
                let previous = scan.clone();
                let result = self.execute_task(&task, &mut scan, now).await;
                let interval = if result.is_err() {
                    chrono::Duration::seconds(self.min_interval_sec)
                } else {
                    self.scan_interval(&previous, &scan, now)
                };
                self.repo
                    .update_task_after_poll(task.id, Local::now() + interval)
                    .await?;
                result
            }
            .await;
            if let Err(e) = result {
                error!("Pixiv tag task {} failed: {:#}", task.id, e);
            }
        }
        self.send_due().await?;
        self.repo
            .prune_pixiv_push_history(Utc::now().timestamp() - self.window_hours * 3600)
            .await
    }

    fn scan_interval(
        &self,
        previous: &TagScan,
        scan: &TagScan,
        now: DateTime<Utc>,
    ) -> chrono::Duration {
        let window_seconds = self.window_hours * 3600;
        let window_start = now.timestamp() - window_seconds;
        let end = if scan.complete {
            now.timestamp()
        } else {
            scan.cursor_time
                .unwrap_or(window_start)
                .min(now.timestamp())
        };
        let start = previous.cursor_time.unwrap_or_else(|| {
            if previous.cursor_offset == 0 {
                previous
                    .cursor_date
                    .and_hms_opt(0, 0, 0)
                    .unwrap()
                    .and_local_timezone(SEARCH_TIMEZONE)
                    .unwrap()
                    .timestamp()
            } else {
                // An existing cursor without a timestamp needs one page checkpoint.
                end
            }
        });
        let covered_seconds = (end - start.max(window_start)).clamp(0, window_seconds);
        let target_seconds = i64::from(self.scan_target_hours) * 3600;
        // Account for works published while the scan is in progress.
        let seconds = (covered_seconds as f64 * target_seconds as f64
            / (window_seconds + target_seconds) as f64)
            .clamp(self.min_interval_sec as f64, self.max_interval_sec as f64);
        chrono::Duration::seconds(seconds.ceil() as i64)
    }

    async fn execute_task(
        &self,
        task: &tasks::Model,
        scan: &mut TagScan,
        now: DateTime<Utc>,
    ) -> Result<()> {
        let mut subscriptions = Vec::new();
        for sub in self.repo.list_subscriptions_by_task(task.id).await? {
            if let Some(settings) = self.repo.pixiv_tag_settings(sub.id).await? {
                subscriptions.push((sub, settings));
            }
        }
        if subscriptions.is_empty() {
            return Ok(());
        }

        // New subscriptions and lower thresholds can qualify cached works.
        for candidate in self.repo.pixiv_tag_candidates(task.id).await? {
            let illust = serde_json::from_str(&candidate.illust)?;
            self.enqueue_eligible(
                &subscriptions,
                &illust,
                candidate.created_at,
                now.timestamp(),
            )
            .await?;
        }

        let scan_result = self.scan(task, &subscriptions, scan, now).await;
        self.repo
            .prune_pixiv_tag_candidates(task.id, Utc::now().timestamp() - self.window_hours * 3600)
            .await?;

        scan_result
    }

    async fn send_due(&self) -> Result<()> {
        for sub in self
            .repo
            .pixiv_tag_subscriptions_due(Utc::now().timestamp())
            .await?
        {
            let result: Result<()> = async {
                let Some(chat) = get_chat_if_should_notify(&self.repo, sub.chat_id).await? else {
                    return Ok(());
                };
                for delivery in self
                    .repo
                    .pixiv_tag_deliveries_due(sub.id, Utc::now().timestamp())
                    .await?
                {
                    if !self.repo.subscription_exists(sub.id).await? {
                        break;
                    }
                    self.send_delivery(&sub, &chat, delivery).await?;
                }
                sleep(Duration::from_millis(INTER_SUBSCRIPTION_DELAY_MS)).await;
                Ok(())
            }
            .await;
            if let Err(e) = result {
                error!(
                    "Pixiv tag delivery for subscription {} failed: {:#}",
                    sub.id, e
                );
            }
        }
        Ok(())
    }

    async fn enqueue_eligible(
        &self,
        subscriptions: &[(subscriptions::Model, TagSettings)],
        illust: &Illust,
        created_at: i64,
        now: i64,
    ) -> Result<()> {
        if !illust.visible || created_at > now || now - created_at > self.window_hours * 3600 {
            return Ok(());
        }
        for (sub, settings) in subscriptions {
            if illust.total_bookmarks < settings.min_bookmarks as u64
                || settings
                    .max_pages
                    .is_some_and(|limit| i64::from(illust.page_count) > limit)
            {
                continue;
            }
            let Some(chat) = get_chat_if_should_notify(&self.repo, sub.chat_id).await? else {
                continue;
            };
            if !apply_subscription_tag_filter(sub, &chat, [illust]).is_empty() {
                self.repo
                    .enqueue_pixiv_tag_delivery(sub.id, illust, now)
                    .await?;
            }
        }
        Ok(())
    }

    async fn scan(
        &self,
        task: &tasks::Model,
        subscriptions: &[(subscriptions::Model, TagSettings)],
        scan: &mut TagScan,
        now: DateTime<Utc>,
    ) -> Result<()> {
        let end_date = now.with_timezone(&SEARCH_TIMEZONE).date_naive();
        for _ in 0..self.scan_pages {
            if scan.complete {
                break;
            }
            let (illusts, next) = self
                .pixiv
                .read()
                .await
                .search_illusts(
                    &task.value,
                    scan.cursor_date,
                    end_date,
                    u32::try_from(scan.cursor_offset)?,
                )
                .await?;
            let checked_at = Utc::now().timestamp();
            let mut observations = Vec::new();
            for illust in illusts {
                let created_at = DateTime::parse_from_rfc3339(&illust.create_date)?.timestamp();
                observations.push((illust, created_at));
            }
            let checkpoint = advance_scan(scan, &observations, next, end_date)?;
            for (illust, created_at) in &observations {
                self.enqueue_eligible(subscriptions, illust, *created_at, checked_at)
                    .await?;
            }
            self.repo
                .save_pixiv_tag_page(task.id, &observations, &checkpoint)
                .await?;
            *scan = checkpoint;
        }
        Ok(())
    }

    async fn send_delivery(
        &self,
        sub: &subscriptions::Model,
        chat: &chats::Model,
        mut delivery: TagDelivery,
    ) -> Result<()> {
        let illust: Illust = serde_json::from_str(&delivery.illust)?;
        let Some(settings) = self.repo.pixiv_tag_settings(sub.id).await? else {
            return Ok(());
        };
        if apply_subscription_tag_filter(sub, chat, [&illust]).is_empty()
            || settings
                .max_pages
                .is_some_and(|limit| i64::from(illust.page_count) > limit)
        {
            self.repo
                .save_pixiv_tag_delivery(&delivery, true, Utc::now().timestamp())
                .await?;
            return Ok(());
        }
        let result = process_illust_push(
            &self.repo,
            &self.notifier,
            &self.pixiv,
            chat,
            &illust,
            sub.id,
            self.image_size,
        )
        .await;
        let done = match result {
            Ok(PushResult::Success { .. }) => true,
            Ok(PushResult::Partial { .. } | PushResult::Failure { .. }) => false,
            Err(e) => {
                warn!("Tag push failed for {}: {:#}", illust.id, e);
                false
            }
        };
        delivery.attempts = if done {
            0
        } else {
            delivery.attempts.saturating_add(1)
        };
        let delay = if !done {
            (300_i64 * (1_i64 << delivery.attempts.saturating_sub(1).min(7))).min(21600)
        } else {
            0
        };
        self.repo
            .save_pixiv_tag_delivery(&delivery, done, Utc::now().timestamp() + delay)
            .await
    }
}

/// Rebase the offset when a page crosses a date boundary. Reusing the old
/// range's offset with a newer start date would skip the newer day's works.
fn advance_scan(
    scan: &TagScan,
    observations: &[(Illust, i64)],
    next: Option<u32>,
    end_date: NaiveDate,
) -> Result<TagScan> {
    anyhow::ensure!(
        !observations.is_empty() || next.is_none(),
        "Empty search page with continuation"
    );
    let dates = observations
        .iter()
        .map(|(_, timestamp)| {
            DateTime::from_timestamp(*timestamp, 0)
                .unwrap()
                .with_timezone(&SEARCH_TIMEZONE)
                .date_naive()
        })
        .collect::<Vec<_>>();
    anyhow::ensure!(
        dates
            .iter()
            .all(|date| *date >= scan.cursor_date && *date <= end_date)
            && observations.windows(2).all(|pair| pair[0].1 <= pair[1].1),
        "Pixiv search did not return the requested ascending date range"
    );
    let mut checkpoint = scan.clone();
    if let Some((_, timestamp)) = observations.last() {
        checkpoint.cursor_time = Some(*timestamp);
    }
    checkpoint.complete = next.is_none();
    if let Some(next) = next {
        checkpoint.cursor_offset = i64::from(next);
        if let Some(&last_date) = dates.last().filter(|date| **date > scan.cursor_date) {
            checkpoint.cursor_date = last_date;
            checkpoint.cursor_offset = i64::try_from(
                dates
                    .iter()
                    .rev()
                    .take_while(|date| **date == last_date)
                    .count(),
            )?;
        }
    }
    Ok(checkpoint)
}
