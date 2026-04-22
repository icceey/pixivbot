use crate::booru::{BooruSite, BooruSiteRegistry};
use crate::bot::notifier::{DownloadButtonConfig, Notifier};
use crate::config::{BooruConfig, BooruSiteConfig};
use crate::db::repo::Repo;
use crate::db::types::{
    BooruFilter, BooruRankingMode, BooruRankingState, BooruTagState, BooruTaskKey, HotPost,
    OrderbyKind, PopularScale, QueuedBooruPost, SubscriptionState, TaskType,
};
use crate::scheduler::helpers::{
    booru_ranking_subscription_state, booru_tag_subscription_state, get_chat_if_should_notify,
    save_first_message_record, INTER_SUBSCRIPTION_DELAY_MS,
};
use crate::utils::{caption, sensitive};
use anyhow::{Context, Result};
use chrono::{Local, Utc};
use rand::RngExt;
use std::collections::HashSet;
use std::sync::Arc;
use teloxide::prelude::*;
use tokio::time::{sleep, Duration};
use tracing::{debug, error, info, warn};

const DRAIN_POLL_INTERVAL_SEC: u64 = 10;

const MAX_FETCH_PAGES: u32 = 5;

const MAX_GRACE_SEND_ATTEMPTS: u8 = 3;

pub struct BooruEngine {
    repo: Arc<Repo>,
    notifier: Notifier,
    tick_interval_sec: u64,
    max_retry_count: i32,
    registry: Arc<BooruSiteRegistry>,
    booru_config: Arc<BooruConfig>,
}

impl BooruEngine {
    pub fn new(
        repo: Arc<Repo>,
        notifier: Notifier,
        tick_interval_sec: u64,
        max_retry_count: i32,
        registry: Arc<BooruSiteRegistry>,
        booru_config: Arc<BooruConfig>,
    ) -> Self {
        Self {
            repo,
            notifier,
            tick_interval_sec,
            // Clamp to u8 range since retry_count in BooruTagState is u8.
            // Values > 255 would cause the counter to saturate and retry forever.
            max_retry_count: max_retry_count.min(255),
            registry,
            booru_config,
        }
    }

    pub async fn run(&self) {
        if self.registry.is_empty() {
            info!("No booru sites configured, booru engine disabled");
            return;
        }
        info!(
            "🚀 Booru engine started with {} site(s)",
            self.registry.len()
        );

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
        let tag_task = self
            .repo
            .get_pending_tasks_by_type(TaskType::BooruTag, 1)
            .await?
            .into_iter()
            .next();
        let ranking_task = self
            .repo
            .get_pending_tasks_by_type(TaskType::BooruRanking, 1)
            .await?
            .into_iter()
            .next();

        if let Some(task) = tag_task {
            debug!("⚙️  Executing booru tag task [{}] {}", task.id, task.value);
            if let Err(e) = self.execute_booru_tag_task(&task).await {
                error!("Booru tag task execution failed: {:#}", e);
                self.handle_tag_task_error(&task).await?;
            }
        }

        if let Some(task) = ranking_task {
            debug!(
                "⚙️  Executing booru ranking task [{}] {}",
                task.id, task.value
            );
            if let Err(e) = self.execute_booru_ranking_task(&task).await {
                error!("Booru ranking task execution failed: {:#}", e);
                let backoff = Local::now() + chrono::Duration::hours(1);
                self.repo.update_task_after_poll(task.id, backoff).await?;
            }
        }

        Ok(())
    }

    fn site_for_task_value(&self, task_value: &str) -> Option<&Arc<BooruSite>> {
        let core = task_value.split('|').next().unwrap_or(task_value);
        let site_name = core.split(':').next()?;
        self.registry.get(site_name)
    }

    fn parse_task_value(task_value: &str) -> (&str, &str) {
        let core = task_value.split('|').next().unwrap_or(task_value);
        match core.split_once(':') {
            Some((site, tags)) => (site, tags),
            None => (core, ""),
        }
    }

    async fn handle_tag_task_error(&self, task: &crate::db::entities::tasks::Model) -> Result<()> {
        if let Some(site) = self.site_for_task_value(&task.value) {
            self.schedule_next_poll(task.id, &site.config).await?;
        } else {
            warn!(
                "Task [{}] refers to unknown site '{}', scheduling backoff",
                task.id, task.value
            );
            let backoff = Local::now() + chrono::Duration::hours(1);
            self.repo.update_task_after_poll(task.id, backoff).await?;
        }
        Ok(())
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

        let (posts, reached_old) = if all_pending {
            debug!(
                "Task [{}]: all subscriptions have pending queues, skipping API fetch",
                task.id
            );
            (Vec::new(), true)
        } else {
            // Use the oldest latest_post_id across ALL subscriptions (including
            // those with pending queues) so that when a pending sub's queue drains,
            // it can still process newly fetched posts from its cursor position.
            let oldest_latest_id = subscriptions
                .iter()
                .filter_map(|sub| {
                    let state = booru_tag_subscription_state(sub)?;
                    Some(state.latest_post_id)
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
                    reached_old,
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

    async fn execute_booru_ranking_task(
        &self,
        task: &crate::db::entities::tasks::Model,
    ) -> Result<()> {
        let key = BooruTaskKey::parse(&task.value)
            .ok_or_else(|| anyhow::anyhow!("Invalid ranking task_value: {}", task.value))?;
        let site_ctx = self
            .registry
            .get(&key.site)
            .ok_or_else(|| anyhow::anyhow!("Unknown site in ranking task: {}", key.site))?;
        let mode = key
            .ranking
            .clone()
            .ok_or_else(|| anyhow::anyhow!("Ranking task missing mode: {}", task.value))?;

        let subscriptions = self.repo.list_subscriptions_by_task(task.id).await?;
        if subscriptions.is_empty() {
            self.schedule_ranking_next_poll(task.id, &mode).await?;
            return Ok(());
        }

        let posts = match &mode {
            BooruRankingMode::Orderby(kind) => {
                let order_tag = Self::orderby_tag_for(site_ctx.config.engine_type, *kind);
                let api_filter_tags: Vec<Option<&BooruFilter>> = subscriptions
                    .iter()
                    .map(|s| s.booru_filter.as_ref())
                    .collect();
                let aggregate = BooruFilter::aggregate(&api_filter_tags);

                let mut parts = Vec::new();
                if !key.tags.is_empty() {
                    parts.push(key.tags.clone());
                }
                parts.push(order_tag);
                for t in aggregate.to_api_tags(site_ctx.config.engine_type) {
                    parts.push(t);
                }

                let query = parts.join(" ");
                site_ctx
                    .client
                    .get_posts(&query, self.booru_config.ranking_top_n, 1)
                    .await
                    .context("Failed to fetch ranking posts")?
            }
            BooruRankingMode::Popular(scale) => site_ctx
                .client
                .get_popular_posts(*scale, self.booru_config.ranking_top_n)
                .await
                .context("Failed to fetch popular ranking posts")?,
            BooruRankingMode::Interval(_) => {
                let aggregate_filter = BooruFilter::aggregate(
                    &subscriptions
                        .iter()
                        .map(|s| s.booru_filter.as_ref())
                        .collect::<Vec<_>>(),
                );
                let mut query = key.tags.clone();
                if !aggregate_filter.is_empty() {
                    let api_tags = aggregate_filter.to_api_tags(site_ctx.config.engine_type);
                    if !query.is_empty() && !api_tags.is_empty() {
                        query.push(' ');
                    }
                    query.push_str(&api_tags.join(" "));
                }

                let mut posts = site_ctx
                    .client
                    .get_posts(&query, self.booru_config.ranking_top_n * 3, 1)
                    .await
                    .context("Failed to fetch interval ranking posts")?;
                use rand::seq::SliceRandom;
                let mut rng = rand::rng();
                posts.shuffle(&mut rng);
                posts.truncate(self.booru_config.ranking_top_n as usize);
                posts
            }
        };

        for sub in &subscriptions {
            let chat = match get_chat_if_should_notify(&self.repo, sub.chat_id).await {
                Ok(Some(c)) => c,
                Ok(None) => continue,
                Err(e) => {
                    error!("chat fetch failed: {:#}", e);
                    continue;
                }
            };

            let state =
                booru_ranking_subscription_state(sub).unwrap_or_else(|| BooruRankingState {
                    pushed_ids: Vec::new(),
                    retry_count: 0,
                    pending_post: None,
                });

            let post_refs: Vec<&booru_client::BooruPost> = posts.iter().collect();
            let filtered = self.apply_booru_filters(sub, &chat, &post_refs);
            let new_posts: Vec<&booru_client::BooruPost> = filtered
                .into_iter()
                .filter(|p| !state.pushed_ids.contains(&p.id))
                .collect();

            if new_posts.is_empty() {
                continue;
            }

            let mut new_state = state.clone();
            for post in &new_posts {
                let sent = self
                    .push_single_post(
                        ChatId(sub.chat_id),
                        sub.id,
                        post,
                        &chat,
                        &key.site,
                        &site_ctx.config.base_url,
                        site_ctx.config.engine_type,
                    )
                    .await;
                if sent {
                    new_state.pushed_ids.push(post.id);
                }
                sleep(Duration::from_millis(INTER_SUBSCRIPTION_DELAY_MS)).await;
            }

            new_state.pushed_ids.sort_unstable();
            new_state.pushed_ids.dedup();

            new_state.trim_pushed(self.booru_config.ranking_pushed_cap);
            if let Err(e) = self
                .repo
                .update_subscription_latest_data(
                    sub.id,
                    Some(SubscriptionState::BooruRanking(new_state)),
                )
                .await
            {
                error!("update sub state failed: {:#}", e);
            }
        }

        self.schedule_ranking_next_poll(task.id, &mode).await?;
        Ok(())
    }

    fn orderby_tag_for(engine: booru_client::BooruEngineType, kind: OrderbyKind) -> String {
        match engine {
            booru_client::BooruEngineType::Gelbooru => match kind {
                OrderbyKind::Score => "sort:score".into(),
                OrderbyKind::Fav => {
                    warn!("Gelbooru does not support order=fav; falling back to sort:score");
                    "sort:score".into()
                }
                OrderbyKind::Random => "sort:random".into(),
            },
            _ => match kind {
                OrderbyKind::Score => "order:score".into(),
                OrderbyKind::Fav => "order:favcount".into(),
                OrderbyKind::Random => "order:random".into(),
            },
        }
    }

    async fn schedule_ranking_next_poll(
        &self,
        task_id: i32,
        mode: &BooruRankingMode,
    ) -> Result<()> {
        let interval = match mode {
            BooruRankingMode::Orderby(_) => chrono::Duration::hours(2),
            BooruRankingMode::Popular(PopularScale::Day) => chrono::Duration::hours(12),
            BooruRankingMode::Popular(PopularScale::Week) => chrono::Duration::days(3),
            BooruRankingMode::Popular(PopularScale::Month) => chrono::Duration::days(7),
            BooruRankingMode::Interval(iso) => iso8601_duration::Duration::parse(iso)
                .ok()
                .and_then(|d| d.to_std())
                .and_then(|d| chrono::Duration::from_std(d).ok())
                .unwrap_or_else(|| {
                    warn!("Invalid ISO duration {}, defaulting to 6h", iso);
                    chrono::Duration::hours(6)
                }),
        };
        let next = Local::now() + interval;
        self.repo.update_task_after_poll(task_id, next).await?;
        Ok(())
    }

    /// Fetch posts since the given ID. Returns `(posts, reached_old)` where
    /// `reached_old` indicates whether we successfully fetched back to (or past)
    /// the `oldest_latest_id`. When `false`, some posts between the cursor and
    /// the oldest fetched post may have been missed due to the MAX_FETCH_PAGES limit.
    ///
    /// **Assumption**: The API returns posts sorted by ID descending (newest first).
    /// This is the default behavior for Moebooru, Danbooru, and Gelbooru.
    /// If a future engine returns differently-ordered results, the pagination
    /// termination logic (checking `p.id <= last_id`) will malfunction.
    async fn fetch_posts_since(
        &self,
        site: &BooruSite,
        tags: &str,
        oldest_latest_id: Option<u64>,
        api_filter_tags: &[String],
    ) -> Result<(Vec<booru_client::BooruPost>, bool)> {
        let mut all_posts = Vec::new();
        let limit = site.config.page_limit;
        let mut reached_old = false;

        let query_tags = if api_filter_tags.is_empty() {
            tags.to_string()
        } else {
            format!("{} {}", tags, api_filter_tags.join(" "))
        };

        for page in 1..=MAX_FETCH_PAGES {
            let posts = site
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
            error!(
                "Reached MAX_FETCH_PAGES ({}) for site '{}' tags '{}' without catching up to \
                 last known post (id: {:?}). Posts between the cursor and the oldest fetched \
                 post are permanently skipped.",
                MAX_FETCH_PAGES, site.config.name, tags, oldest_latest_id
            );
        }

        Ok((all_posts, reached_old))
    }

    #[allow(clippy::too_many_arguments)]
    async fn process_booru_tag_sub(
        &self,
        subscription: &crate::db::entities::subscriptions::Model,
        chat: &crate::db::entities::chats::Model,
        sub_state: Option<BooruTagState>,
        posts: &[booru_client::BooruPost],
        reached_old: bool,
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

        let has_score_fav_filter = subscription
            .booru_filter
            .as_ref()
            .is_some_and(|f| f.score_min.is_some() || f.fav_count_min.is_some());

        let has_existing_state = sub_state.is_some();
        let prev_state = sub_state.unwrap_or_else(|| BooruTagState::cleared(0));
        let latest_id = prev_state.latest_post_id;

        if !has_score_fav_filter {
            let new_posts: Vec<&booru_client::BooruPost> = if has_existing_state {
                posts.iter().filter(|p| p.id > latest_id).collect()
            } else {
                posts.iter().take(1).collect()
            };

            if new_posts.is_empty() {
                return Ok(None);
            }

            let newest_id = new_posts.iter().map(|p| p.id).max().unwrap_or(0);

            if !reached_old && has_existing_state {
                let oldest_fetched = new_posts.iter().map(|p| p.id).min().unwrap_or(0);
                warn!(
                    "Catch-up overflow for sub {} chat {}: advancing cursor from {} to {} \
                     but posts in range {}..{} may have been missed (MAX_FETCH_PAGES exhausted)",
                    subscription.id,
                    ChatId(subscription.chat_id),
                    latest_id,
                    newest_id,
                    latest_id + 1,
                    oldest_fetched.saturating_sub(1),
                );
            }

            let filtered = self.apply_booru_filters(subscription, chat, &new_posts);

            if filtered.is_empty() {
                return Ok(Some(BooruTagState::cleared(newest_id)));
            }

            // Queue remaining posts for later delivery (applies to all modes).
            // Filter out posts without any image URL at construction time to avoid
            // tick-by-tick drain of permanently unsendable posts.
            let queue: Vec<QueuedBooruPost> = filtered
                .iter()
                .skip(1)
                .filter(|p| {
                    p.sample_url.is_some() || p.file_url.is_some() || p.preview_url.is_some()
                })
                .map(|p| Self::post_to_queued(p))
                .collect();

            let Some(first) = filtered.first() else {
                return Ok(Some(BooruTagState::cleared(newest_id)));
            };

            // Check if the first post has a sendable image URL. If not, skip it
            // rather than queueing it for retry (it will never become sendable).
            let has_image = first.sample_url.is_some()
                || first.file_url.is_some()
                || first.preview_url.is_some();

            if !has_image {
                warn!(
                    "No image URL for booru post {} (sub {} chat {}), skipping",
                    first.id, subscription.id, chat_id
                );
                return Ok(Some(BooruTagState {
                    latest_post_id: newest_id,
                    pending_queue: queue,
                    retry_count: 0,
                    hot_posts: Vec::new(),
                }));
            }

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

            return Ok(Some(BooruTagState {
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
                hot_posts: Vec::new(),
            }));
        }

        let grace_window = chrono::Duration::hours(self.booru_config.grace_window_hours as i64);
        let now = Utc::now();
        let cutoff = now - grace_window;

        // GC: drop hot posts older than grace window. For pushed entries being
        // GC'd, advance the floor cursor to prevent re-push if they still appear
        // in API results after eviction (cursor may have been held back by them).
        let (live_hot, gc_hot): (Vec<HotPost>, Vec<HotPost>) = prev_state
            .hot_posts
            .into_iter()
            .partition(|h| h.first_seen >= cutoff);
        let gc_pushed_max = gc_hot
            .iter()
            .filter(|h| h.pushed)
            .map(|h| h.id)
            .max()
            .unwrap_or(0);
        let mut latest_id = latest_id.max(gc_pushed_max);
        let mut hot_posts = live_hot;

        let hot_ids: HashSet<u64> = hot_posts.iter().map(|h| h.id).collect();
        let candidate_posts: Vec<&booru_client::BooruPost> = posts
            .iter()
            .filter(|p| p.id > latest_id || hot_ids.contains(&p.id))
            .collect();

        let filtered_now = self.apply_booru_filters(subscription, chat, &candidate_posts);
        let filtered_set: HashSet<u64> = filtered_now.iter().map(|p| p.id).collect();

        let to_push: Vec<&booru_client::BooruPost> = candidate_posts
            .iter()
            .copied()
            .filter(|p| {
                filtered_set.contains(&p.id) && !hot_posts.iter().any(|h| h.id == p.id && h.pushed)
            })
            .collect();

        for post in &to_push {
            let sent = self
                .push_single_post(
                    ChatId(subscription.chat_id),
                    subscription.id,
                    post,
                    chat,
                    site_name,
                    base_url,
                    engine_type,
                )
                .await;
            if sent {
                if let Some(h) = hot_posts.iter_mut().find(|h| h.id == post.id) {
                    h.pushed = true;
                } else {
                    hot_posts.push(HotPost {
                        id: post.id,
                        first_seen: now,
                        pushed: true,
                        attempts: 0,
                    });
                }
            } else if let Some(h) = hot_posts.iter_mut().find(|h| h.id == post.id) {
                h.attempts = h.attempts.saturating_add(1);
                if h.attempts >= MAX_GRACE_SEND_ATTEMPTS {
                    h.pushed = true;
                }
            } else {
                hot_posts.push(HotPost {
                    id: post.id,
                    first_seen: now,
                    pushed: false,
                    attempts: 1,
                });
            }
            sleep(Duration::from_millis(INTER_SUBSCRIPTION_DELAY_MS)).await;
        }

        for post in &candidate_posts {
            if !filtered_set.contains(&post.id) && !hot_posts.iter().any(|h| h.id == post.id) {
                hot_posts.push(HotPost {
                    id: post.id,
                    first_seen: now,
                    pushed: false,
                    attempts: 0,
                });
            }
        }

        // Cap eviction: drop oldest by first_seen, but advance latest_id past any
        // dropped pushed entries to prevent re-push if API still returns them.
        if hot_posts.len() > self.booru_config.hot_posts_cap {
            hot_posts.sort_by_key(|h| h.first_seen);
            let drop = hot_posts.len() - self.booru_config.hot_posts_cap;
            let evicted: Vec<HotPost> = hot_posts.drain(0..drop).collect();
            let evicted_pushed_max = evicted
                .iter()
                .filter(|h| h.pushed)
                .map(|h| h.id)
                .max()
                .unwrap_or(0);
            latest_id = latest_id.max(evicted_pushed_max);
        }

        let new_latest_id = if let Some(min_hot) = hot_posts.iter().map(|h| h.id).min() {
            min_hot.saturating_sub(1).max(latest_id)
        } else {
            posts
                .iter()
                .map(|p| p.id)
                .max()
                .unwrap_or(latest_id)
                .max(latest_id)
        };

        Ok(Some(BooruTagState {
            latest_post_id: new_latest_id,
            pending_queue: Vec::new(),
            retry_count: 0,
            hot_posts,
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

        // Check retry limit: max_retry_count <= 0 means retry disabled
        if state.should_abandon_queue(self.max_retry_count) {
            if self.max_retry_count <= 0 {
                warn!(
                    "Retry disabled (max_retry_count={}), clearing pending queue for booru sub {} chat {}",
                    self.max_retry_count, subscription.id, chat_id
                );
            } else {
                warn!(
                    "Max retry count reached for booru sub {} chat {}, clearing queue",
                    subscription.id, chat_id
                );
            }
            return Ok(Some(BooruTagState::cleared(state.latest_post_id)));
        }

        let first = &state.pending_queue[0];
        let image_url = first
            .sample_url
            .as_deref()
            .or(first.file_url.as_deref())
            .or(first.preview_url.as_deref());

        let Some(url) = image_url else {
            warn!(
                "No image URL for queued booru post {} (sub {} chat {}), removing from queue",
                first.id, subscription.id, chat_id
            );
            return Ok(Some(state.popped_front()));
        };

        let caption_text = caption::build_booru_caption(
            &Self::queued_to_booru_post(first),
            site_name,
            base_url,
            engine_type,
        );

        let queued_rating = booru_client::BooruRating::from_short_str(&first.rating);
        let has_spoiler = sensitive::should_blur_booru(chat, &first.tags, queued_rating);

        let send_result = self
            .notifier
            .notify_with_images_and_button(
                chat_id,
                &[url.to_string()],
                Some(&caption_text),
                has_spoiler,
                &DownloadButtonConfig::for_booru_chat(site_name, first.id, chat),
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

            Ok(Some(state.popped_front()))
        } else {
            Ok(Some(state.with_retry_increment()))
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
        let has_spoiler = sensitive::should_blur_booru(chat, &post.tags, post.rating);

        let send_result = self
            .notifier
            .notify_with_images_and_button(
                chat_id,
                &[url.to_string()],
                Some(&caption_text),
                has_spoiler,
                &DownloadButtonConfig::for_booru_chat(site_name, post.id, chat),
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
    fn test_parse_task_value_strips_ranking_and_filter_suffixes() {
        let (site, tags) = BooruEngine::parse_task_value("konachan:cat|o=score|f=s");
        assert_eq!(site, "konachan");
        assert_eq!(tags, "cat");
    }

    #[test]
    fn test_parse_task_value_strips_filter_suffix() {
        let (site, tags) = BooruEngine::parse_task_value("konachan:cat|f=s");
        assert_eq!(site, "konachan");
        assert_eq!(tags, "cat");
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
            BooruRating::Sensitive,
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
