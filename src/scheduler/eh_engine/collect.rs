use crate::config::EhentaiConfig;
use crate::db::repo::eh_download_queue::SOURCE_SUBSCRIPTION;
use crate::db::repo::eh_gallery_jobs::EhGalleryVariant;
use crate::db::types::{
    EhFilter, EhPendingGallery, EhTagState, EhTaskKey, SubscriptionState, TaskType,
};
use crate::db::{entities::subscriptions, repo::Repo};
use crate::scheduler::helpers::eh_tag_subscription_state;
use anyhow::{Context, Result};
use chrono::Local;
use eh_client::{EhClient, EhGallery};
use rand::RngExt;
use std::{sync::Arc, time::Duration};
use tracing::{error, info, warn};

/// Maximum search pages to fetch per tick.
const MAX_FETCH_PAGES: u32 = 5;
/// Maximum metadata entries per api.php request.
const MAX_METADATA_BATCH: usize = 25;
/// Minimum delay between search requests (3s + buffer).
const SEARCH_RATE_LIMIT_MS: u64 = 3500;

pub struct EhEngine {
    repo: Arc<Repo>,
    client: Arc<EhClient>,
    config: Arc<EhentaiConfig>,
    telegraph_available: bool,
    tick_interval_sec: u64,
    pub(super) search_request_interval: Duration,
}

impl EhEngine {
    pub fn new(
        repo: Arc<Repo>,
        client: Arc<EhClient>,
        config: Arc<EhentaiConfig>,
        telegraph_available: bool,
        tick_interval_sec: u64,
    ) -> Self {
        Self {
            repo,
            client,
            config,
            telegraph_available,
            tick_interval_sec,
            search_request_interval: Duration::from_millis(SEARCH_RATE_LIMIT_MS),
        }
    }

    pub async fn run(self) {
        let mut interval =
            tokio::time::interval(tokio::time::Duration::from_secs(self.tick_interval_sec));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            interval.tick().await;
            if let Err(e) = self.tick().await {
                error!("EhEngine tick error: {:#}", e);
            }
        }
    }

    pub(super) async fn tick(&self) -> Result<()> {
        let tasks = self
            .repo
            .get_pending_tasks_by_type(TaskType::Ehentai, 1)
            .await
            .context("Failed to fetch pending eh tasks")?;

        if let Some(task) = tasks.into_iter().next() {
            if let Err(e) = self.execute_eh_task(&task).await {
                error!("Failed to execute eh task {}: {:#}", task.id, e);
                let backoff = Local::now() + chrono::Duration::hours(1);
                if let Err(e2) = self.repo.update_task_after_poll(task.id, backoff).await {
                    error!("Failed to backoff eh task {}: {:#}", task.id, e2);
                }
            }
        }

        Ok(())
    }

    async fn execute_eh_task(&self, task: &crate::db::entities::tasks::Model) -> Result<()> {
        let key = EhTaskKey::parse(&task.value).context("Failed to parse eh task value")?;

        let subs = self
            .repo
            .list_subscriptions_by_task(task.id)
            .await
            .context("Failed to list eh subscriptions")?;

        if subs.is_empty() {
            self.schedule_next_poll(task.id).await;
            return Ok(());
        }

        let mut prepared_subs = Vec::new();
        for sub in subs {
            let state = eh_tag_subscription_state(&sub).unwrap_or_else(EhTagState::cleared);
            if state.pending_galleries.is_empty() {
                prepared_subs.push((sub, self.config.max_push_per_tick));
                continue;
            }

            let telegraph_default = self.telegraph_default(sub.eh_filter.as_ref());
            let (updated_sub, updated_state, remaining_slots) = self
                .drain_pending_backlog(
                    &sub,
                    state,
                    self.config.max_push_per_tick,
                    telegraph_default,
                )
                .await?;
            if updated_state.pending_galleries.is_empty() && remaining_slots > 0 {
                prepared_subs.push((updated_sub, remaining_slots));
            }
        }

        if prepared_subs.is_empty() {
            self.schedule_next_poll(task.id).await;
            return Ok(());
        }

        // Compute aggregate filter across subs that still have per-tick capacity.
        let eh_filters: Vec<Option<&EhFilter>> = prepared_subs
            .iter()
            .map(|(s, _)| s.eh_filter.as_ref())
            .collect();
        let agg_filter = EhFilter::aggregate(&eh_filters);

        // Determine the oldest latest_posted_ts across subs (cursor)
        let oldest_ts = prepared_subs
            .iter()
            .filter_map(|(s, _)| eh_tag_subscription_state(s).map(|st| st.latest_posted_ts))
            .min()
            .unwrap_or(0);

        // Fetch gallery refs from search
        let refs = self
            .fetch_gallery_refs(&key.query, key.category_bitmask)
            .await?;

        if refs.is_empty() {
            for (sub, _) in &prepared_subs {
                self.update_sub_state_no_new(sub, oldest_ts).await;
            }
            self.schedule_next_poll(task.id).await;
            return Ok(());
        }

        // Batch fetch full metadata (gives us real posted timestamp)
        let gidlist: Vec<(u64, &str)> = refs.iter().map(|g| (g.gid, g.token.as_str())).collect();

        let mut all_metadata = Vec::new();
        for chunk in gidlist.chunks(MAX_METADATA_BATCH) {
            let metadata = self
                .client
                .get_metadata(chunk)
                .await
                .context("Failed to fetch gallery metadata")?;
            all_metadata.extend(metadata);
            if chunk.len() == MAX_METADATA_BATCH {
                tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
            }
        }

        // Filter by real posted timestamp + aggregate filter
        let now_ts = Local::now().timestamp();
        let scan_cutoff = now_ts - (self.config.scan_window_hours as i64 * 3600);

        let filtered: Vec<EhGallery> = all_metadata
            .into_iter()
            .filter(|g| {
                if oldest_ts > 0 && g.posted <= oldest_ts {
                    return false;
                }
                if agg_filter.has_rating_filter() && g.posted < scan_cutoff.max(oldest_ts) {
                    return false;
                }
                true
            })
            .filter(|g| agg_filter.matches(g))
            .collect();

        if filtered.is_empty() {
            for (sub, _) in &prepared_subs {
                self.update_sub_state_no_new(sub, oldest_ts).await;
            }
            self.schedule_next_poll(task.id).await;
            return Ok(());
        }

        // Process each subscription
        for (sub, remaining_slots) in &prepared_subs {
            self.process_eh_sub_with_slots(sub, &filtered, *remaining_slots)
                .await?;
        }

        self.schedule_next_poll(task.id).await;
        Ok(())
    }

    /// Fetch gallery refs from search. Returns all refs found (up to MAX_FETCH_PAGES).
    async fn fetch_gallery_refs(
        &self,
        query: &str,
        cats: u32,
    ) -> Result<Vec<eh_client::EhGalleryRef>> {
        let mut all_refs = Vec::new();

        for page in 0..MAX_FETCH_PAGES {
            // Rate limit between search requests (skip before the first request)
            if page > 0 {
                tokio::time::sleep(self.search_request_interval).await;
            }

            let refs = self
                .client
                .search(query, cats, page)
                .await
                .context("Failed to search eh galleries")?;

            if refs.is_empty() {
                break;
            }

            all_refs.extend(refs);
        }

        // Deduplicate search results by GID
        let mut seen_gids = std::collections::HashSet::new();
        all_refs.retain(|r| seen_gids.insert(r.gid));

        Ok(all_refs)
    }

    fn telegraph_default(&self, sub_filter: Option<&EhFilter>) -> bool {
        self.telegraph_available
            && (self.config.upload_telegraph || sub_filter.map(|f| f.telegraph).unwrap_or(false))
    }

    pub(super) async fn drain_pending_backlog(
        &self,
        sub: &subscriptions::Model,
        mut state: EhTagState,
        mut remaining_slots: usize,
        telegraph_default: bool,
    ) -> Result<(subscriptions::Model, EhTagState, usize)> {
        let variant = EhGalleryVariant::for_request(
            self.client.is_logged_in(),
            SOURCE_SUBSCRIPTION,
            self.config.as_ref(),
        );
        if !self.repo.subscription_exists(sub.id).await? {
            info!(
                "Skipping pending EH backlog for removed subscription {}",
                sub.id
            );
            return Ok((sub.clone(), state, 0));
        }
        let mut still_pending = Vec::new();
        let backlog: Vec<_> = state.pending_galleries.drain(..).collect();
        let mut backlog_iter = backlog.into_iter();
        while let Some(pending) = backlog_iter.next() {
            if remaining_slots == 0 {
                still_pending.push(pending);
                continue;
            }
            if !self.repo.subscription_exists(sub.id).await? {
                info!(
                    "Skipping pending EH gallery {} for removed subscription {}",
                    pending.gid, sub.id
                );
                continue;
            }
            let consumed_slot = match self
                .repo
                .enqueue_eh_subscription_download(
                    sub.chat_id,
                    sub.id,
                    pending.gid as i64,
                    &pending.token,
                    &pending.title,
                    telegraph_default,
                    &variant,
                    pending.fingerprint.as_deref(),
                    self.config.send_archive,
                )
                .await
            {
                Ok(Some(_)) => true,
                Ok(None) => false,
                Err(e) => {
                    if !self.repo.subscription_exists(sub.id).await? {
                        self.repo
                            .cancel_eh_subscription_queue_entries(sub.id, self.config.send_archive)
                            .await?;
                        info!(
                            "Skipping pending EH gallery {} for removed subscription {}",
                            pending.gid, sub.id
                        );
                        continue;
                    }
                    let failed_gid = pending.gid;
                    still_pending.push(pending);
                    still_pending.extend(backlog_iter);
                    state.pending_galleries = still_pending;
                    state.trim_pushed(self.config.pushed_cap);
                    self.repo
                        .update_subscription_latest_data(
                            sub.id,
                            Some(SubscriptionState::EhTag(state)),
                        )
                        .await
                        .context("Failed to persist eh pending backlog after enqueue failure")?;
                    return Err(e).with_context(|| {
                        format!("Failed to enqueue pending gallery {}", failed_gid)
                    });
                }
            };
            if !self.repo.subscription_exists(sub.id).await? {
                self.repo
                    .cancel_eh_subscription_queue_entries(sub.id, self.config.send_archive)
                    .await?;
                info!(
                    "Removed pending EH gallery {} owner for deleted subscription {}",
                    pending.gid, sub.id
                );
                continue;
            }
            state.add_pushed_gid(pending.gid);
            if consumed_slot {
                remaining_slots -= 1;
            }
        }

        state.pending_galleries = still_pending;
        if state.pending_galleries.is_empty() && state.pending_high_water_ts > 0 {
            state.latest_posted_ts = state.latest_posted_ts.max(state.pending_high_water_ts);
            state.pending_high_water_ts = 0;
        }
        state.trim_pushed(self.config.pushed_cap);
        if !self.repo.subscription_exists(sub.id).await? {
            self.repo
                .cancel_eh_subscription_queue_entries(sub.id, self.config.send_archive)
                .await?;
            return Ok((sub.clone(), state, 0));
        }
        let updated_sub = self
            .repo
            .update_subscription_latest_data(sub.id, Some(SubscriptionState::EhTag(state.clone())))
            .await
            .context("Failed to update eh subscription state")?;
        Ok((updated_sub, state, remaining_slots))
    }

    pub(super) async fn process_eh_sub_with_slots(
        &self,
        sub: &crate::db::entities::subscriptions::Model,
        galleries: &[EhGallery],
        max_push: usize,
    ) -> Result<()> {
        if !self.repo.subscription_exists(sub.id).await? {
            info!("Skipping EH collect for removed subscription {}", sub.id);
            return Ok(());
        }
        let mut state = eh_tag_subscription_state(sub).unwrap_or_else(EhTagState::cleared);
        let variant = EhGalleryVariant::for_request(
            self.client.is_logged_in(),
            SOURCE_SUBSCRIPTION,
            self.config.as_ref(),
        );

        let sub_filter = sub.eh_filter.as_ref();
        let mut remaining_slots = max_push;
        let telegraph_default = self.telegraph_default(sub_filter);

        // Step 1: Consume pending backlog first (galleries from previous overflow).
        if !state.pending_galleries.is_empty() {
            let (_updated_sub, updated_state, remaining) = self
                .drain_pending_backlog(sub, state, remaining_slots, telegraph_default)
                .await?;
            state = updated_state;
            remaining_slots = remaining;
            if !state.pending_galleries.is_empty() || remaining_slots == 0 {
                return Ok(());
            }
        }

        // Step 2: Pending backlog drained. Now process new filtered galleries.
        let eligible: Vec<EhPendingGallery> = galleries
            .iter()
            .filter(|g| !state.pushed_gids.contains(&g.gid))
            .filter(|g| sub_filter.map(|f| f.matches(g)).unwrap_or(true))
            .map(|g| EhPendingGallery {
                gid: g.gid,
                token: g.token.clone(),
                title: g.title.clone(),
                posted: g.posted,
                fingerprint: Some(g.source_fingerprint()),
            })
            .collect();

        // Record the high-water mark: max posted timestamp among eligible galleries
        // this tick. If some overflow, this prevents cursor advance beyond unconsumed.
        let max_eligible_posted = eligible
            .iter()
            .map(|g| g.posted)
            .max()
            .unwrap_or(state.pending_high_water_ts);
        state.pending_high_water_ts = state.pending_high_water_ts.max(max_eligible_posted);

        let mut eligible_iter = eligible.into_iter();
        let mut max_enqueued_posted = state.latest_posted_ts;
        while let Some(gallery) = eligible_iter.next() {
            if remaining_slots == 0 {
                // Overflow: store in pending backlog for next tick.
                state.pending_galleries.push(gallery);
                continue;
            }
            if !self.repo.subscription_exists(sub.id).await? {
                info!(
                    "Skipping EH gallery {} for removed subscription {}",
                    gallery.gid, sub.id
                );
                continue;
            }
            let consumed_slot = match self
                .repo
                .enqueue_eh_subscription_download(
                    sub.chat_id,
                    sub.id,
                    gallery.gid as i64,
                    &gallery.token,
                    &gallery.title,
                    telegraph_default,
                    &variant,
                    gallery.fingerprint.as_deref(),
                    self.config.send_archive,
                )
                .await
            {
                Ok(Some(_)) => true,
                Ok(None) => false,
                Err(e) => {
                    if !self.repo.subscription_exists(sub.id).await? {
                        self.repo
                            .cancel_eh_subscription_queue_entries(sub.id, self.config.send_archive)
                            .await?;
                        info!(
                            "Skipping EH gallery {} for removed subscription {}",
                            gallery.gid, sub.id
                        );
                        continue;
                    }
                    let failed_gid = gallery.gid;
                    state.pending_galleries.push(gallery);
                    state.pending_galleries.extend(eligible_iter);
                    state.trim_pushed(self.config.pushed_cap);
                    self.repo
                        .update_subscription_latest_data(
                            sub.id,
                            Some(SubscriptionState::EhTag(state)),
                        )
                        .await
                        .context("Failed to persist eh collect state after enqueue failure")?;
                    return Err(e).with_context(|| {
                        format!("Failed to enqueue download for gallery {}", failed_gid)
                    });
                }
            };
            if !self.repo.subscription_exists(sub.id).await? {
                self.repo
                    .cancel_eh_subscription_queue_entries(sub.id, self.config.send_archive)
                    .await?;
                info!(
                    "Removed EH gallery {} owner for deleted subscription {}",
                    gallery.gid, sub.id
                );
                continue;
            }
            state.add_pushed_gid(gallery.gid);
            max_enqueued_posted = max_enqueued_posted.max(gallery.posted);
            if consumed_slot {
                remaining_slots -= 1;
            }
        }

        // Step 3: If no overflow, safely advance cursor past the entire batch.
        if state.pending_galleries.is_empty() {
            state.latest_posted_ts = state
                .latest_posted_ts
                .max(max_enqueued_posted)
                .max(state.pending_high_water_ts);
            state.pending_high_water_ts = 0;
        }

        state.trim_pushed(self.config.pushed_cap);
        if !self.repo.subscription_exists(sub.id).await? {
            self.repo
                .cancel_eh_subscription_queue_entries(sub.id, self.config.send_archive)
                .await?;
            return Ok(());
        }

        self.repo
            .update_subscription_latest_data(sub.id, Some(SubscriptionState::EhTag(state)))
            .await
            .context("Failed to update eh subscription state")?;

        Ok(())
    }

    /// Update state when no new galleries were found.
    pub(super) async fn update_sub_state_no_new(
        &self,
        sub: &crate::db::entities::subscriptions::Model,
        latest_ts: i64,
    ) {
        let state = eh_tag_subscription_state(sub).unwrap_or_else(EhTagState::cleared);
        if state.latest_posted_ts == latest_ts {
            return;
        }
        let new_state = EhTagState {
            pushed_gids: state.pushed_gids,
            latest_posted_ts: if latest_ts > 0 {
                state.latest_posted_ts.max(latest_ts)
            } else {
                state.latest_posted_ts
            },
            pending_galleries: state.pending_galleries,
            pending_high_water_ts: state.pending_high_water_ts,
        };
        if let Err(e) = self
            .repo
            .update_subscription_latest_data(sub.id, Some(SubscriptionState::EhTag(new_state)))
            .await
        {
            warn!("Failed to update eh sub state: {:#}", e);
        }
    }

    async fn schedule_next_poll(&self, task_id: i32) {
        let min = self.config.min_interval_sec;
        let max = self.config.max_interval_sec;
        let delay = if max > min {
            rand::rng().random_range(min..=max)
        } else {
            max
        };
        let next = Local::now() + chrono::Duration::seconds(delay as i64);
        if let Err(e) = self.repo.update_task_after_poll(task_id, next).await {
            error!("Failed to schedule next eh poll: {:#}", e);
        }
    }
}
