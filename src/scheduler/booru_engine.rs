use crate::bot::notifier::{DownloadButtonConfig, Notifier};
use crate::config::BooruSiteConfig;
use crate::db::repo::Repo;
use crate::db::types::{BooruFilter, BooruTagState, QueuedBooruPost, SubscriptionState, TaskType};
use crate::scheduler::helpers::{
    booru_tag_subscription_state, get_chat_if_should_notify, save_first_message_record,
    INTER_SUBSCRIPTION_DELAY_MS,
};
use crate::utils::{caption, sensitive};
use anyhow::{Context, Result};
use booru_client::BooruClient;
use chrono::Local;
use rand::RngExt;
use std::collections::HashMap;
use std::sync::Arc;
use teloxide::prelude::*;
use tokio::time::{sleep, Duration};
use tracing::{debug, error, info, warn};

const DRAIN_POLL_INTERVAL_SEC: u64 = 10;
/// Maximum number of pages to fetch when catching up on missed posts.
const MAX_FETCH_PAGES: u32 = 5;

pub struct BooruEngine {
    repo: Arc<Repo>,
    notifier: Notifier,
    tick_interval_sec: u64,
    max_retry_count: i32,
    sites: HashMap<String, SiteContext>,
}

struct SiteContext {
    client: BooruClient,
    config: BooruSiteConfig,
}

impl BooruEngine {
    pub fn new(
        repo: Arc<Repo>,
        notifier: Notifier,
        tick_interval_sec: u64,
        max_retry_count: i32,
        site_configs: Vec<BooruSiteConfig>,
    ) -> Self {
        let mut sites = HashMap::new();
        for cfg in site_configs {
            let client = match BooruClient::new(&cfg.base_url, cfg.engine_type) {
                Ok(c) => c,
                Err(e) => {
                    error!("Failed to create BooruClient for {}: {:#}", cfg.name, e);
                    continue;
                }
            };
            let client = match (&cfg.username, &cfg.api_key) {
                (Some(user), Some(key)) => client.with_auth(user, key),
                _ => client,
            };
            sites.insert(
                cfg.name.to_lowercase(),
                SiteContext {
                    client,
                    config: cfg,
                },
            );
        }

        Self {
            repo,
            notifier,
            tick_interval_sec,
            max_retry_count,
            sites,
        }
    }

    pub async fn run(&self) {
        if self.sites.is_empty() {
            info!("No booru sites configured, booru engine disabled");
            return;
        }
        info!("🚀 Booru engine started with {} site(s)", self.sites.len());

        let mut interval = tokio::time::interval(Duration::from_secs(self.tick_interval_sec));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            interval.tick().await;
            if let Err(e) = self.tick().await {
                error!("Booru engine tick error: {:#}", e);
            }
        }
    }

    async fn tick(&self) -> Result<()> {
        let tasks = self
            .repo
            .get_pending_tasks_by_type(TaskType::BooruTag, 1)
            .await?;

        let task = match tasks.first() {
            Some(t) => t,
            None => return Ok(()),
        };

        debug!("⚙️  Executing booru tag task [{}] {}", task.id, task.value);

        let result = self.execute_booru_tag_task(task).await;

        if let Err(e) = result {
            error!("Booru tag task execution failed: {:#}", e);
            if let Some(site_ctx) = self.site_for_task_value(&task.value) {
                self.schedule_next_poll(task.id, &site_ctx.config).await?;
            } else {
                warn!(
                    "Task [{}] refers to unknown site '{}', scheduling backoff",
                    task.id, task.value
                );
                let backoff = Local::now() + chrono::Duration::hours(1);
                self.repo.update_task_after_poll(task.id, backoff).await?;
            }
        }

        Ok(())
    }

    fn site_for_task_value(&self, task_value: &str) -> Option<&SiteContext> {
        let site_name = task_value.split(':').next()?;
        self.sites.get(site_name)
    }

    fn parse_task_value(task_value: &str) -> (&str, &str) {
        match task_value.split_once(':') {
            Some((site, tags)) => (site, tags),
            None => (task_value, ""),
        }
    }

    async fn execute_booru_tag_task(&self, task: &crate::db::entities::tasks::Model) -> Result<()> {
        let (site_name, tags) = Self::parse_task_value(&task.value);

        let site_ctx = self
            .site_for_task_value(&task.value)
            .ok_or_else(|| anyhow::anyhow!("Unknown booru site: {}", site_name))?;

        let subscriptions = self.repo.list_subscriptions_by_task(task.id).await?;
        if subscriptions.is_empty() {
            self.schedule_next_poll(task.id, &site_ctx.config).await?;
            return Ok(());
        }

        let any_pending = subscriptions.iter().any(|sub| {
            booru_tag_subscription_state(sub).is_some_and(|s| !s.pending_queue.is_empty())
        });

        let all_pending = any_pending
            && subscriptions.iter().all(|sub| {
                booru_tag_subscription_state(sub).is_some_and(|s| !s.pending_queue.is_empty())
            });

        let posts = if all_pending {
            debug!(
                "Task [{}]: all subscriptions have pending queues, skipping API fetch",
                task.id
            );
            Vec::new()
        } else {
            let oldest_latest_id = subscriptions
                .iter()
                .filter_map(|sub| {
                    let state = booru_tag_subscription_state(sub)?;
                    if state.pending_queue.is_empty() {
                        Some(state.latest_post_id)
                    } else {
                        None
                    }
                })
                .min();

            let booru_filters: Vec<Option<&BooruFilter>> = subscriptions
                .iter()
                .filter(|sub| {
                    booru_tag_subscription_state(sub).is_none_or(|s| s.pending_queue.is_empty())
                })
                .map(|sub| sub.booru_filter.as_ref())
                .collect();
            let aggregate = BooruFilter::aggregate(&booru_filters);
            let api_tags = aggregate.to_api_tags(site_ctx.config.engine_type);

            self.fetch_posts_since(site_ctx, tags, oldest_latest_id, &api_tags)
                .await?
        };

        if posts.is_empty() && !any_pending {
            self.schedule_next_poll(task.id, &site_ctx.config).await?;
            return Ok(());
        }

        let mut has_pending_queue = false;

        for subscription in &subscriptions {
            let sub_state = booru_tag_subscription_state(subscription);

            if posts.is_empty()
                && sub_state
                    .as_ref()
                    .is_none_or(|s| s.pending_queue.is_empty())
            {
                continue;
            }

            let chat = match get_chat_if_should_notify(&self.repo, subscription.chat_id).await {
                Ok(Some(chat)) => chat,
                Ok(None) => continue,
                Err(e) => {
                    error!("Failed to process chat {}: {:#}", subscription.chat_id, e);
                    continue;
                }
            };

            match self
                .process_booru_tag_sub(
                    subscription,
                    &chat,
                    sub_state,
                    &posts,
                    site_name,
                    &site_ctx.config.base_url,
                    site_ctx.config.engine_type,
                )
                .await
            {
                Ok(Some(new_state)) => {
                    if !new_state.pending_queue.is_empty() {
                        has_pending_queue = true;
                    }
                    if let Err(e) = self
                        .repo
                        .update_subscription_latest_data(
                            subscription.id,
                            Some(SubscriptionState::BooruTag(new_state)),
                        )
                        .await
                    {
                        error!(
                            "Failed to update subscription {} state: {:#}",
                            subscription.id, e
                        );
                    }
                }
                Ok(None) => {}
                Err(e) => {
                    error!(
                        "Failed to process booru subscription {}: {:#}",
                        subscription.id, e
                    );
                }
            }

            sleep(Duration::from_millis(INTER_SUBSCRIPTION_DELAY_MS)).await;
        }

        if has_pending_queue {
            self.schedule_drain_poll(task.id).await?;
        } else {
            self.schedule_next_poll(task.id, &site_ctx.config).await?;
        }
        Ok(())
    }

    async fn fetch_posts_since(
        &self,
        site_ctx: &SiteContext,
        tags: &str,
        oldest_latest_id: Option<u64>,
        api_filter_tags: &[String],
    ) -> Result<Vec<booru_client::BooruPost>> {
        let mut all_posts = Vec::new();
        let limit = site_ctx.config.page_limit;
        let mut reached_old = false;

        let query_tags = if api_filter_tags.is_empty() {
            tags.to_string()
        } else {
            format!("{} {}", tags, api_filter_tags.join(" "))
        };

        for page in 1..=MAX_FETCH_PAGES {
            let posts = site_ctx
                .client
                .get_posts(&query_tags, limit, page)
                .await
                .context("Failed to fetch posts from booru")?;

            if posts.is_empty() {
                reached_old = true;
                break;
            }

            reached_old = match oldest_latest_id {
                Some(last_id) => posts.iter().any(|p| p.id <= last_id),
                None => true,
            };

            all_posts.extend(posts);

            if reached_old {
                break;
            }

            debug!(
                "All posts on page {} are new, fetching page {}",
                page,
                page + 1
            );
        }

        if !reached_old {
            warn!(
                "Reached MAX_FETCH_PAGES ({}) without catching up to last known post (id: {:?}). Some posts may be skipped.",
                MAX_FETCH_PAGES, oldest_latest_id
            );
        }

        Ok(all_posts)
    }

    #[allow(clippy::too_many_arguments)]
    async fn process_booru_tag_sub(
        &self,
        subscription: &crate::db::entities::subscriptions::Model,
        chat: &crate::db::entities::chats::Model,
        sub_state: Option<BooruTagState>,
        posts: &[booru_client::BooruPost],
        site_name: &str,
        base_url: &str,
        engine_type: booru_client::BooruEngineType,
    ) -> Result<Option<BooruTagState>> {
        let chat_id = ChatId(subscription.chat_id);

        if let Some(ref state) = sub_state {
            if !state.pending_queue.is_empty() {
                return self
                    .drain_pending_queue(
                        subscription,
                        chat,
                        state,
                        site_name,
                        base_url,
                        engine_type,
                    )
                    .await;
            }
        }

        let latest_id = sub_state.as_ref().map(|s| s.latest_post_id);

        let new_posts: Vec<&booru_client::BooruPost> = if let Some(last_id) = latest_id {
            posts.iter().filter(|p| p.id > last_id).collect()
        } else {
            posts.iter().take(1).collect()
        };

        if new_posts.is_empty() {
            return Ok(None);
        }

        let newest_id = new_posts.iter().map(|p| p.id).max().unwrap_or(0);

        let filtered = self.apply_booru_filters(subscription, chat, &new_posts);

        if filtered.is_empty() {
            return Ok(Some(BooruTagState {
                latest_post_id: newest_id,
                pending_queue: Vec::new(),
                retry_count: 0,
            }));
        }

        // Queue remaining posts for later delivery (applies to all modes)
        let queue: Vec<QueuedBooruPost> = filtered
            .iter()
            .skip(1)
            .map(|p| Self::post_to_queued(p))
            .collect();

        let first = filtered.first().unwrap();
        let send_ok = self
            .push_single_post(
                chat_id,
                subscription.id,
                first,
                chat,
                site_name,
                base_url,
                engine_type,
            )
            .await;

        Ok(Some(BooruTagState {
            latest_post_id: newest_id,
            pending_queue: if send_ok {
                queue
            } else {
                // Failed: put the unsent post back at the front of the queue
                let mut full_queue = vec![Self::post_to_queued(first)];
                full_queue.extend(queue);
                full_queue
            },
            retry_count: if send_ok { 0 } else { 1 },
        }))
    }

    async fn drain_pending_queue(
        &self,
        subscription: &crate::db::entities::subscriptions::Model,
        chat: &crate::db::entities::chats::Model,
        state: &BooruTagState,
        site_name: &str,
        base_url: &str,
        engine_type: booru_client::BooruEngineType,
    ) -> Result<Option<BooruTagState>> {
        let chat_id = ChatId(subscription.chat_id);

        if self.max_retry_count > 0 && (state.retry_count as i32) >= self.max_retry_count {
            warn!(
                "Max retry count reached for booru sub {} chat {}, clearing queue",
                subscription.id, chat_id
            );
            return Ok(Some(BooruTagState {
                latest_post_id: state.latest_post_id,
                pending_queue: Vec::new(),
                retry_count: 0,
            }));
        }

        let first = &state.pending_queue[0];
        let image_url = first
            .sample_url
            .as_deref()
            .or(first.file_url.as_deref())
            .or(first.preview_url.as_deref());

        let Some(url) = image_url else {
            let mut remaining = state.pending_queue.clone();
            remaining.remove(0);
            return Ok(Some(BooruTagState {
                latest_post_id: state.latest_post_id,
                pending_queue: remaining,
                retry_count: 0,
            }));
        };

        let caption_text = caption::build_booru_caption(
            &Self::queued_to_booru_post(first),
            site_name,
            base_url,
            engine_type,
        );

        let queued_rating = booru_client::BooruRating::from_short_str(&first.rating);
        let has_spoiler = sensitive::should_blur_booru_tags(chat, &first.tags)
            || (chat.blur_sensitive_tags && queued_rating.is_nsfw());

        let send_result = self
            .notifier
            .notify_with_images_and_button(
                chat_id,
                &[url.to_string()],
                Some(&caption_text),
                has_spoiler,
                &DownloadButtonConfig::default(),
            )
            .await;

        if send_result.is_complete_success() {
            save_first_message_record(
                &self.repo,
                chat_id,
                subscription.id,
                send_result.first_message_id,
                None,
            )
            .await;

            let mut remaining = state.pending_queue.clone();
            remaining.remove(0);
            Ok(Some(BooruTagState {
                latest_post_id: state.latest_post_id,
                pending_queue: remaining,
                retry_count: 0,
            }))
        } else {
            Ok(Some(BooruTagState {
                latest_post_id: state.latest_post_id,
                pending_queue: state.pending_queue.clone(),
                retry_count: state.retry_count.saturating_add(1),
            }))
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn push_single_post(
        &self,
        chat_id: ChatId,
        subscription_id: i32,
        post: &booru_client::BooruPost,
        chat: &crate::db::entities::chats::Model,
        site_name: &str,
        base_url: &str,
        engine_type: booru_client::BooruEngineType,
    ) -> bool {
        let image_url = post
            .sample_url
            .as_deref()
            .or(post.file_url.as_deref())
            .or(post.preview_url.as_deref());

        let Some(url) = image_url else {
            warn!("No image URL for booru post {}", post.id);
            return false;
        };

        let caption_text = caption::build_booru_caption(post, site_name, base_url, engine_type);
        let has_spoiler = sensitive::should_blur_booru_tags(chat, &post.tags)
            || (chat.blur_sensitive_tags && post.rating.is_nsfw());

        let send_result = self
            .notifier
            .notify_with_images_and_button(
                chat_id,
                &[url.to_string()],
                Some(&caption_text),
                has_spoiler,
                &DownloadButtonConfig::default(),
            )
            .await;

        if send_result.is_complete_success() {
            save_first_message_record(
                &self.repo,
                chat_id,
                subscription_id,
                send_result.first_message_id,
                None,
            )
            .await;
            info!("✅ Sent booru post {} to chat {}", post.id, chat_id);
            true
        } else {
            error!(
                "❌ Failed to send booru post {} to chat {}",
                post.id, chat_id
            );
            false
        }
    }

    fn apply_booru_filters<'a>(
        &self,
        subscription: &crate::db::entities::subscriptions::Model,
        chat: &crate::db::entities::chats::Model,
        posts: &[&'a booru_client::BooruPost],
    ) -> Vec<&'a booru_client::BooruPost> {
        let chat_filter = crate::db::types::TagFilter::from_excluded_tags(&chat.excluded_tags);
        let combined_tag_filter = subscription.filter_tags.merged(&chat_filter);

        posts
            .iter()
            .filter(|post| {
                let tag_refs: Vec<&str> = post.tags.split_whitespace().collect();
                if !combined_tag_filter.is_empty()
                    && !combined_tag_filter.matches_tag_strings(&tag_refs)
                {
                    return false;
                }

                if let Some(ref bf) = subscription.booru_filter {
                    if !bf.matches(post.score, post.fav_count, &post.rating) {
                        return false;
                    }
                }

                true
            })
            .copied()
            .collect()
    }

    fn post_to_queued(post: &booru_client::BooruPost) -> QueuedBooruPost {
        QueuedBooruPost {
            id: post.id,
            tags: post.tags.clone(),
            score: post.score,
            fav_count: post.fav_count,
            file_url: post.file_url.clone(),
            sample_url: post.sample_url.clone(),
            preview_url: post.preview_url.clone(),
            rating: post.rating.as_short_str().to_string(),
            width: post.width,
            height: post.height,
            source: post.source.clone(),
        }
    }

    fn queued_to_booru_post(queued: &QueuedBooruPost) -> booru_client::BooruPost {
        booru_client::BooruPost {
            id: queued.id,
            tags: queued.tags.clone(),
            score: queued.score,
            fav_count: queued.fav_count,
            file_url: queued.file_url.clone(),
            sample_url: queued.sample_url.clone(),
            preview_url: queued.preview_url.clone(),
            rating: booru_client::BooruRating::from_short_str(&queued.rating),
            width: queued.width,
            height: queued.height,
            md5: None,
            source: queued.source.clone(),
            created_at: None,
            file_size: None,
            file_ext: None,
            status: None,
        }
    }

    async fn schedule_next_poll(&self, task_id: i32, site_config: &BooruSiteConfig) -> Result<()> {
        let min = site_config.min_interval_sec;
        let max = site_config.max_interval_sec.max(min);
        let random_interval_sec = rand::rng().random_range(min..=max);
        let next_poll = Local::now() + chrono::Duration::seconds(random_interval_sec as i64);
        self.repo.update_task_after_poll(task_id, next_poll).await?;
        Ok(())
    }

    async fn schedule_drain_poll(&self, task_id: i32) -> Result<()> {
        let next_poll = Local::now() + chrono::Duration::seconds(DRAIN_POLL_INTERVAL_SEC as i64);
        self.repo.update_task_after_poll(task_id, next_poll).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use booru_client::{BooruPost, BooruRating};

    fn make_post(id: u64, tags: &str, score: i32, rating: BooruRating) -> BooruPost {
        BooruPost {
            id,
            tags: tags.to_string(),
            score,
            fav_count: 0,
            file_url: Some(format!("https://example.com/{}.jpg", id)),
            sample_url: Some(format!("https://example.com/sample/{}.jpg", id)),
            preview_url: None,
            rating,
            width: 800,
            height: 600,
            md5: None,
            source: Some("https://source.example".to_string()),
            created_at: None,
            file_size: None,
            file_ext: None,
            status: None,
        }
    }

    #[test]
    fn test_parse_task_value_with_tags() {
        let (site, tags) = BooruEngine::parse_task_value("konachan:landscape sky");
        assert_eq!(site, "konachan");
        assert_eq!(tags, "landscape sky");
    }

    #[test]
    fn test_parse_task_value_empty_tags() {
        let (site, tags) = BooruEngine::parse_task_value("konachan:");
        assert_eq!(site, "konachan");
        assert_eq!(tags, "");
    }

    #[test]
    fn test_parse_task_value_no_colon() {
        let (site, tags) = BooruEngine::parse_task_value("konachan");
        assert_eq!(site, "konachan");
        assert_eq!(tags, "");
    }

    #[test]
    fn test_post_to_queued_roundtrip() {
        let post = make_post(42, "landscape sky", 100, BooruRating::Safe);
        let queued = BooruEngine::post_to_queued(&post);
        let recovered = BooruEngine::queued_to_booru_post(&queued);

        assert_eq!(recovered.id, post.id);
        assert_eq!(recovered.tags, post.tags);
        assert_eq!(recovered.score, post.score);
        assert_eq!(recovered.fav_count, post.fav_count);
        assert_eq!(recovered.file_url, post.file_url);
        assert_eq!(recovered.sample_url, post.sample_url);
        assert_eq!(recovered.rating, post.rating);
        assert_eq!(recovered.width, post.width);
        assert_eq!(recovered.height, post.height);
        assert_eq!(recovered.source, post.source);
    }

    #[test]
    fn test_queued_rating_roundtrip() {
        for rating in [
            BooruRating::Safe,
            BooruRating::General,
            BooruRating::Questionable,
            BooruRating::Explicit,
        ] {
            let post = make_post(1, "", 0, rating);
            let queued = BooruEngine::post_to_queued(&post);
            let recovered = BooruEngine::queued_to_booru_post(&queued);
            assert_eq!(
                recovered.rating, rating,
                "Rating roundtrip failed for {:?}",
                rating
            );
        }
    }
}
