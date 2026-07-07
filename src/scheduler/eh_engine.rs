use crate::bot::notifier::Notifier;
use crate::config::EhentaiConfig;
use crate::db::entities::{eh_download_queue, subscriptions};
use crate::db::repo::Repo;
use crate::db::types::{
    EhFilter, EhPendingGallery, EhTagState, EhTaskKey, SubscriptionState, TaskType,
};
use crate::scheduler::helpers::{eh_tag_subscription_state, get_chat_if_should_notify};
use anyhow::{Context, Result};
use chrono::Local;
use eh_client::{EhClient, EhGallery, ImageUploadInput, ImageUploader, TelegraphClient};
use rand::RngExt;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use crate::db::repo::eh_download_queue::{
    EH_PUBLISH_CANCEL_LOCK, STATUS_DOWNLOADED, STATUS_PENDING, STATUS_UPLOADED,
};

/// Maximum search pages to fetch per tick (safety cap).
const MAX_FETCH_PAGES: u32 = 5;

/// Maximum metadata entries per api.php request.
const MAX_METADATA_BATCH: usize = 25;

/// Search rate limit: minimum delay between search requests (3s + buffer).
const SEARCH_RATE_LIMIT_MS: u64 = 3500;
const EH_UPLOAD_IMAGE_CHANNEL_CAPACITY: usize = 1;

// ============================================================================
// Stage 1: EhEngine — Collect (search → metadata → filter → enqueue downloads)
// ============================================================================

pub struct EhEngine {
    repo: Arc<Repo>,
    client: Arc<EhClient>,
    config: Arc<EhentaiConfig>,
    telegraph_available: bool,
    tick_interval_sec: u64,
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

    async fn tick(&self) -> Result<()> {
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
        let refs = if agg_filter.has_rating_filter() {
            self.fetch_galleries_48h(&key.query, key.category_bitmask, oldest_ts)
                .await?
        } else {
            self.fetch_galleries_since(&key.query, key.category_bitmask, oldest_ts)
                .await?
        };

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
    async fn fetch_galleries_since(
        &self,
        query: &str,
        cats: u32,
        _oldest_ts: i64,
    ) -> Result<Vec<eh_client::EhGalleryRef>> {
        let mut all_refs = Vec::new();

        for page in 0..MAX_FETCH_PAGES {
            // Rate limit between search requests (skip before the first request)
            if page > 0 {
                tokio::time::sleep(tokio::time::Duration::from_millis(SEARCH_RATE_LIMIT_MS)).await;
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

    /// 48h scan mode: same as normal mode — timestamp filtering done after metadata fetch.
    async fn fetch_galleries_48h(
        &self,
        query: &str,
        cats: u32,
        _oldest_ts: i64,
    ) -> Result<Vec<eh_client::EhGalleryRef>> {
        self.fetch_galleries_since(query, cats, 0).await
    }

    fn telegraph_default(&self, sub_filter: Option<&EhFilter>) -> bool {
        self.telegraph_available
            && (self.config.upload_telegraph || sub_filter.map(|f| f.telegraph).unwrap_or(false))
    }

    async fn drain_pending_backlog(
        &self,
        sub: &subscriptions::Model,
        mut state: EhTagState,
        mut remaining_slots: usize,
        telegraph_default: bool,
    ) -> Result<(subscriptions::Model, EhTagState, usize)> {
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
            if let Err(e) = self
                .repo
                .enqueue_eh_subscription_download(
                    sub.chat_id,
                    sub.id,
                    pending.gid as i64,
                    &pending.token,
                    &pending.title,
                    telegraph_default,
                )
                .await
            {
                if !self.repo.subscription_exists(sub.id).await? {
                    self.repo
                        .cancel_eh_subscription_queue_entries(sub.id)
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
                    .update_subscription_latest_data(sub.id, Some(SubscriptionState::EhTag(state)))
                    .await
                    .context("Failed to persist eh pending backlog after enqueue failure")?;
                return Err(e)
                    .with_context(|| format!("Failed to enqueue pending gallery {}", failed_gid));
            }
            if !self.repo.subscription_exists(sub.id).await? {
                self.repo
                    .cancel_eh_subscription_queue_entries(sub.id)
                    .await?;
                info!(
                    "Removed pending EH gallery {} owner for deleted subscription {}",
                    pending.gid, sub.id
                );
                continue;
            }
            state.add_pushed_gid(pending.gid);
            remaining_slots -= 1;
        }

        state.pending_galleries = still_pending;
        if state.pending_galleries.is_empty() && state.pending_high_water_ts > 0 {
            state.latest_posted_ts = state.latest_posted_ts.max(state.pending_high_water_ts);
            state.pending_high_water_ts = 0;
        }
        state.trim_pushed(self.config.pushed_cap);
        if !self.repo.subscription_exists(sub.id).await? {
            self.repo
                .cancel_eh_subscription_queue_entries(sub.id)
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

    async fn process_eh_sub_with_slots(
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
            if let Err(e) = self
                .repo
                .enqueue_eh_subscription_download(
                    sub.chat_id,
                    sub.id,
                    gallery.gid as i64,
                    &gallery.token,
                    &gallery.title,
                    telegraph_default,
                )
                .await
            {
                if !self.repo.subscription_exists(sub.id).await? {
                    self.repo
                        .cancel_eh_subscription_queue_entries(sub.id)
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
                    .update_subscription_latest_data(sub.id, Some(SubscriptionState::EhTag(state)))
                    .await
                    .context("Failed to persist eh collect state after enqueue failure")?;
                return Err(e).with_context(|| {
                    format!("Failed to enqueue download for gallery {}", failed_gid)
                });
            }
            if !self.repo.subscription_exists(sub.id).await? {
                self.repo
                    .cancel_eh_subscription_queue_entries(sub.id)
                    .await?;
                info!(
                    "Removed EH gallery {} owner for deleted subscription {}",
                    gallery.gid, sub.id
                );
                continue;
            }
            state.add_pushed_gid(gallery.gid);
            max_enqueued_posted = max_enqueued_posted.max(gallery.posted);
            remaining_slots -= 1;
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
                .cancel_eh_subscription_queue_entries(sub.id)
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
    async fn update_sub_state_no_new(
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

// ============================================================================
// Stage 2: EhDownloadWorker — Download archives from e-hentai, cache locally
// ============================================================================

pub struct EhDownloadWorker {
    repo: Arc<Repo>,
    client: Arc<EhClient>,
    config: Arc<EhentaiConfig>,
    cache_dir: std::path::PathBuf,
}

impl EhDownloadWorker {
    pub fn new(
        repo: Arc<Repo>,
        client: Arc<EhClient>,
        config: Arc<EhentaiConfig>,
        cache_dir: std::path::PathBuf,
    ) -> Self {
        Self {
            repo,
            client,
            config,
            cache_dir,
        }
    }

    pub async fn run(self) {
        // Clean orphan cache files on startup (stale entry reset is done in main.rs)
        let eh_cache = self.cache_dir.join("eh_cache");
        if let Err(e) = self.repo.cleanup_eh_cache_orphans(&eh_cache).await {
            warn!("Failed to cleanup eh cache orphans: {:#}", e);
        }

        let poll = self.config.download_poll_interval_sec.max(10);
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(poll));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            interval.tick().await;
            if let Err(e) = self.tick().await {
                error!("EhDownloadWorker tick error: {:#}", e);
            }
        }
    }

    async fn tick(&self) -> Result<()> {
        // Rate limit check
        let downloaded_bytes = self
            .repo
            .get_eh_downloaded_bytes_in_window(self.config.download_rate_window_hours)
            .await?;

        if downloaded_bytes >= self.config.download_rate_limit_bytes() as i64 {
            info!("EH download rate limit reached, skipping this tick");
            return Ok(());
        }

        let entry = self.repo.get_next_for_download().await?;
        let Some(entry) = entry else {
            return Ok(());
        };

        if let Err(e) = self.process(&entry).await {
            error!("Download failed for entry {}: {:#}", entry.id, e);

            // process() wraps errors with .context(); downcast_ref only checks the
            // outermost layer. Must traverse the error chain to find eh_client::Error.
            let is_in_progress = e
                .chain()
                .find_map(|c| c.downcast_ref::<eh_client::Error>())
                .map(|client_err| matches!(client_err, eh_client::Error::DownloadInProgress { .. }))
                .unwrap_or(false);

            if is_in_progress {
                // Transfer made real progress (>10KB/s): don't increment retry_count,
                // preserve .part file for resumption on the next tick.
                self.repo
                    .defer_eh_download(
                        entry.id,
                        STATUS_PENDING,
                        self.config.download_poll_interval_sec as i64,
                    )
                    .await?;
            } else {
                let (_, permanent) = self
                    .repo
                    .schedule_eh_retry_from(
                        entry.id,
                        &entry.status,
                        STATUS_PENDING,
                        &e.to_string(),
                        self.config.max_retry_count,
                    )
                    .await?;
                if permanent {
                    warn!("Permanent download failure for gid={}: {}", entry.gid, e);
                    // Delete partial ZIP if it exists — only on unrecoverable failure
                    self.cleanup_zip(&entry).await;
                }
            }
        }

        Ok(())
    }

    async fn process(&self, entry: &eh_download_queue::Model) -> Result<()> {
        let gid = entry.gid as u64;
        let token = &entry.token;

        // Check chat is enabled before downloading
        let chat = get_chat_if_should_notify(&self.repo, entry.chat_id).await?;
        if chat.is_none() {
            let _publish_cancel_guard = EH_PUBLISH_CANCEL_LOCK.lock().await;
            if !self
                .repo
                .eh_download_is_active(entry.id, &entry.status)
                .await?
            {
                info!("Skipping canceled EH download entry {}", entry.id);
                return Ok(());
            }
            info!(
                "Deferring download for gid={} — chat {} not notifiable",
                gid, entry.chat_id
            );
            // Defer (no retry increment): the entry stays pending until the chat
            // is available again.
            self.repo
                .defer_eh_download(
                    entry.id,
                    STATUS_PENDING,
                    self.config.download_poll_interval_sec as i64,
                )
                .await?;
            return Ok(());
        }

        let _publish_cancel_guard = EH_PUBLISH_CANCEL_LOCK.lock().await;
        if !self
            .repo
            .eh_download_is_active(entry.id, &entry.status)
            .await?
        {
            info!("Skipping canceled EH download entry {}", entry.id);
            return Ok(());
        }

        // Ensure cache dir exists
        let eh_cache = self.cache_dir.join("eh_cache");
        tokio::fs::create_dir_all(&eh_cache).await?;
        let zip_path = eh_cache.join(format!("{}_{}.zip", gid, token));
        let zip_path_str = zip_path.to_string_lossy().to_string();

        // Download
        let file_size = if self.client.is_logged_in() {
            let resolution = if entry.source == "direct" {
                &self.config.download_resolution
            } else {
                &self.config.subscription_resolution
            };

            let archive_request = self
                .client
                .prepare_archive_download(gid, token, resolution)
                .await
                .context("Failed to prepare archive download")?;

            self.client
                .download_archive_with_request(&archive_request, &zip_path)
                .await
                .context("Failed to download archive")?
        } else {
            info!("Not logged in, using direct image download for gid={}", gid);
            self.client
                .download_gallery_images(gid, token, &zip_path)
                .await
                .context("Failed to download gallery images")?
        };

        info!("Downloaded eh gallery gid={} size={} bytes", gid, file_size);

        self.repo
            .mark_eh_download_downloaded(entry.id, file_size as i64, &zip_path_str)
            .await?;

        Ok(())
    }

    /// Delete the ZIP/partial ZIP files for an entry if they exist (used on permanent failure).
    async fn cleanup_zip(&self, _entry: &eh_download_queue::Model) {
        // Download worker's ZIP path is constructed from gid+token, not stored yet on failure.
        // The ZIP or resumable .part may exist at the expected path if download started but failed mid-stream.
        let gid = _entry.gid as u64;
        let token = &_entry.token;
        let zip_path = self
            .cache_dir
            .join("eh_cache")
            .join(format!("{}_{}.zip", gid, token));
        for path in [&zip_path, &zip_path.with_extension("zip.part")] {
            if path.exists() {
                if let Err(e) = tokio::fs::remove_file(path).await {
                    warn!("Failed to delete partial zip {}: {}", path.display(), e);
                }
            }
        }
    }
}

// ============================================================================
// Stage 3: EhUploadWorker — Extract images from ZIP, upload images, create Telegraph page
// ============================================================================

pub struct EhUploadWorker {
    repo: Arc<Repo>,
    notifier: Notifier,
    telegraph: Arc<TelegraphClient>,
    image_uploader: Arc<dyn ImageUploader>,
    config: Arc<EhentaiConfig>,
}

struct ZipImageData {
    filename: String,
    data: Vec<u8>,
}

fn is_uploadable_zip_image_name(name: &str) -> bool {
    name.ends_with(".jpg")
        || name.ends_with(".jpeg")
        || name.ends_with(".png")
        || name.ends_with(".gif")
        || name.ends_with(".webp")
}

impl EhUploadWorker {
    pub fn new(
        repo: Arc<Repo>,
        notifier: Notifier,
        telegraph: Arc<TelegraphClient>,
        image_uploader: Arc<dyn ImageUploader>,
        config: Arc<EhentaiConfig>,
    ) -> Self {
        Self {
            repo,
            notifier,
            telegraph,
            image_uploader,
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
                error!("EhUploadWorker tick error: {:#}", e);
            }
        }
    }

    async fn tick(&self) -> Result<()> {
        let entry = self.repo.get_next_for_upload().await?;
        let Some(entry) = entry else {
            return Ok(());
        };

        if let Err(e) = self.process(&entry).await {
            error!("Upload failed for entry {}: {:#}", entry.id, e);

            // Check if this failure would be permanent
            let would_be_permanent = entry.retry_count + 1 > self.config.max_retry_count as i32;

            // Permanent failure fallback: if archive delivery is configured
            // and the ZIP file still exists, downgrade to archive-only instead
            // of marking the entry as failed.
            if would_be_permanent
                && self.config.send_archive
                && entry
                    .zip_path
                    .as_deref()
                    .is_some_and(|p| std::path::Path::new(p).exists())
            {
                info!(
                    "Upload permanently failed for entry {}, falling back to archive delivery",
                    entry.id
                );
                let _ = self
                    .repo
                    .fallback_eh_upload_to_archive(
                        entry.id,
                        &format!("Telegraph upload failed, falling back to archive: {:#}", e),
                    )
                    .await?;
                return Ok(());
            }

            let (_, permanent) = self
                .repo
                .schedule_eh_retry_from(
                    entry.id,
                    &entry.status,
                    STATUS_DOWNLOADED,
                    &e.to_string(),
                    self.config.max_retry_count,
                )
                .await?;
            if permanent {
                // Delete ZIP on permanent failure to free disk space
                self.cleanup_zip(&entry).await;
                let escaped_err = teloxide::utils::markdown::escape(&e.to_string());
                let title = teloxide::utils::markdown::escape(&entry.title);
                let msg = format!("⚠️ Telegraph 上传失败: {}\n\n📦 {}", escaped_err, title);
                let _ = self
                    .notifier
                    .send_text(teloxide::types::ChatId(entry.chat_id), &msg, false)
                    .await;
            }
        }

        Ok(())
    }

    async fn process(&self, entry: &eh_download_queue::Model) -> Result<()> {
        // Check chat is enabled before doing upload work (avoid wasting image upload quota)
        let chat = get_chat_if_should_notify(&self.repo, entry.chat_id).await?;
        if chat.is_none() {
            let _publish_cancel_guard = EH_PUBLISH_CANCEL_LOCK.lock().await;
            if !self
                .repo
                .eh_download_is_active(entry.id, &entry.status)
                .await?
            {
                info!("Skipping canceled EH upload entry {}", entry.id);
                return Ok(());
            }
            info!(
                "Deferring upload for entry {} — chat {} not notifiable",
                entry.id, entry.chat_id
            );
            self.repo
                .defer_eh_download(
                    entry.id,
                    STATUS_DOWNLOADED,
                    self.config.download_poll_interval_sec as i64,
                )
                .await?;
            return Ok(());
        }

        let _publish_cancel_guard = EH_PUBLISH_CANCEL_LOCK.lock().await;
        if !self
            .repo
            .eh_download_is_active(entry.id, &entry.status)
            .await?
        {
            info!("Skipping canceled EH upload entry {}", entry.id);
            return Ok(());
        }

        let zip_path = entry
            .zip_path
            .as_ref()
            .context("zip_path is None for downloaded entry")?;
        let zip_path = std::path::Path::new(zip_path);

        let (image_tx, mut image_rx) = mpsc::channel(EH_UPLOAD_IMAGE_CHANNEL_CAPACITY);
        let zip_path_owned = zip_path.to_path_buf();
        let reader = tokio::task::spawn_blocking(move || -> Result<()> {
            let zip_file = std::fs::File::open(&zip_path_owned).context("Failed to open zip")?;
            let mut archive =
                zip::ZipArchive::new(zip_file).context("Failed to read zip archive")?;

            for i in 0..archive.len() {
                let mut file = archive.by_index(i).context("Failed to read zip entry")?;
                let name = file.name().to_lowercase();
                if !is_uploadable_zip_image_name(&name) {
                    continue;
                }

                let mut data = Vec::new();
                std::io::Read::read_to_end(&mut file, &mut data)
                    .context("Failed to read image from zip")?;
                let filename = std::path::Path::new(file.name())
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("image.jpg")
                    .to_string();

                if image_tx
                    .blocking_send(ZipImageData { filename, data })
                    .is_err()
                {
                    return Ok(());
                }
            }

            Ok(())
        });

        let mut all_urls = Vec::new();
        while let Some(image) = image_rx.recv().await {
            let input = ImageUploadInput {
                filename: &image.filename,
                bytes: image.data.as_slice(),
            };
            let urls = self
                .image_uploader
                .upload_images(&[input])
                .await
                .context("Failed to upload images for Telegraph page")?;
            all_urls.extend(urls);
        }

        reader.await.context("spawn_blocking failed")??;

        if all_urls.is_empty() {
            anyhow::bail!("No images uploaded by configured image uploader");
        }

        // Create Telegraph gallery page
        let title = if entry.title.is_empty() {
            "Gallery"
        } else {
            &entry.title
        };

        let page_url = self
            .telegraph
            .create_gallery_page(title, &all_urls)
            .await
            .context("Failed to create telegraph page")?;

        info!("Created telegraph page for gid={}: {}", entry.gid, page_url);

        self.repo
            .mark_eh_download_uploaded(entry.id, &page_url)
            .await?;

        Ok(())
    }

    /// Delete the ZIP file for an entry if it exists (used on permanent failure).
    async fn cleanup_zip(&self, entry: &eh_download_queue::Model) {
        if let Some(ref zip_path) = entry.zip_path {
            let path = std::path::Path::new(zip_path);
            if path.exists() {
                if let Err(e) = tokio::fs::remove_file(path).await {
                    warn!("Failed to delete zip {}: {}", path.display(), e);
                }
            }
        }
    }
}

// ============================================================================
// Stage 4: EhPublishWorker — Send archive ZIP and/or Telegraph link to Telegram chat
// ============================================================================

/// Raised when the cached ZIP file required for archive delivery is missing.
#[derive(Debug)]
struct MissingEhZip;

impl std::fmt::Display for MissingEhZip {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("cached EH ZIP is missing")
    }
}

impl std::error::Error for MissingEhZip {}

pub struct EhPublishWorker {
    repo: Arc<Repo>,
    notifier: Notifier,
    client: Arc<EhClient>,
    config: Arc<EhentaiConfig>,
}

impl EhPublishWorker {
    pub fn new(
        repo: Arc<Repo>,
        notifier: Notifier,
        client: Arc<EhClient>,
        config: Arc<EhentaiConfig>,
    ) -> Self {
        Self {
            repo,
            notifier,
            client,
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
                error!("EhPublishWorker tick error: {:#}", e);
            }
        }
    }

    async fn tick(&self) -> Result<()> {
        let entry = self.repo.get_next_for_publish().await?;
        let Some(entry) = entry else {
            return Ok(());
        };

        if let Err(e) = self.process(&entry).await {
            error!("Publish failed for entry {}: {:#}", entry.id, e);

            // Missing ZIP: retry from STATUS_PUBLISHING back to STATUS_PENDING
            // so the download worker re-fetches the gallery.
            if e.downcast_ref::<MissingEhZip>().is_some() {
                let (updated, permanent) = self
                    .repo
                    .schedule_eh_retry_from(
                        entry.id,
                        &entry.status,
                        STATUS_PENDING,
                        &format!("cached EH ZIP is missing for {}", entry.title),
                        self.config.max_retry_count,
                    )
                    .await?;
                if permanent {
                    self.cleanup_zip(&updated).await;
                    let title = teloxide::utils::markdown::escape(&updated.title);
                    let msg = format!("⚠️ 下载失败: {}\n原因: cached EH ZIP is missing", title);
                    let _ = self
                        .notifier
                        .send_text(teloxide::types::ChatId(entry.chat_id), &msg, false)
                        .await;
                }
                return Ok(());
            }

            // Regular retry: go back to the pre-publish status
            let target = if entry.telegraph_url.is_some() {
                STATUS_UPLOADED
            } else {
                STATUS_DOWNLOADED
            };
            let (_, permanent) = self
                .repo
                .schedule_eh_retry_from(
                    entry.id,
                    &entry.status,
                    target,
                    &e.to_string(),
                    self.config.max_retry_count,
                )
                .await?;
            if permanent {
                // Delete ZIP on permanent failure to free disk space
                self.cleanup_zip(&entry).await;
                let escaped = teloxide::utils::markdown::escape(&e.to_string());
                let title = teloxide::utils::markdown::escape(&entry.title);
                let msg = format!("⚠️ 发布失败: {}\n\n📦 {}", escaped, title);
                let _ = self
                    .notifier
                    .send_text(teloxide::types::ChatId(entry.chat_id), &msg, false)
                    .await;
            }
        }

        Ok(())
    }

    async fn process(&self, entry: &eh_download_queue::Model) -> Result<()> {
        let chat = get_chat_if_should_notify(&self.repo, entry.chat_id).await?;
        if chat.is_none() {
            // Chat disabled — defer without retry increment.  Determine the
            // correct ready status so the entry is picked up again when the
            // chat becomes available.
            let target = if entry.telegraph_url.is_some() {
                STATUS_UPLOADED
            } else {
                STATUS_DOWNLOADED
            };
            info!(
                "Deferring publish for entry {} — chat {} not notifiable, releasing to {}",
                entry.id, entry.chat_id, target
            );
            self.repo
                .defer_eh_download(
                    entry.id,
                    target,
                    self.config.download_poll_interval_sec as i64,
                )
                .await?;
            return Ok(());
        }
        let chat_id = teloxide::types::ChatId(entry.chat_id);

        let _publish_cancel_guard = EH_PUBLISH_CANCEL_LOCK.lock().await;

        if !self.ensure_entry_active(entry).await? {
            return Ok(());
        }

        // Determine which surfaces need to be sent.
        let archive_required = self.config.send_archive && entry.archive_sent_at.is_none();
        let telegraph_required = entry.telegraph_url.is_some() && entry.telegraph_sent_at.is_none();

        // If both markers are already set, just mark done.
        if !archive_required && !telegraph_required {
            if entry.archive_sent_at.is_some() || entry.telegraph_sent_at.is_some() {
                // At least one marker is set — the work is complete.
                self.repo
                    .mark_eh_download_done(entry.id, entry.file_size)
                    .await?;
                self.cleanup_zip(entry).await;
                info!(
                    "Published eh gallery gid={} to chat {} (already sent, now done)",
                    entry.gid, entry.chat_id
                );
                return Ok(());
            }
            // Neither marker set and nothing to send — no publish surface.
            anyhow::bail!("no EH publish surface for queue entry {}", entry.id);
        }

        // Validate archive prerequisites
        if archive_required {
            let zip_path = entry.zip_path.as_deref().ok_or(MissingEhZip)?;
            if !std::path::Path::new(zip_path).exists() {
                return Err(MissingEhZip.into());
            }
        }

        // Send archive if required
        if archive_required {
            if !self.ensure_entry_active(entry).await? {
                return Ok(());
            }
            let zip_path = entry.zip_path.as_deref().expect("zip_path checked above");
            let zip_path = std::path::Path::new(zip_path);
            let caption = self.build_caption(entry);
            let filename = format!("{}.zip", sanitize_filename(&entry.title));
            self.notifier
                .send_document(chat_id, zip_path, &filename, &caption)
                .await
                .context("Failed to send archive document")?;
            if !self.ensure_entry_active(entry).await? {
                return Ok(());
            }
            self.repo.mark_eh_archive_sent(entry.id).await?;
        }

        // Send Telegraph link if required
        if telegraph_required {
            if !self.ensure_entry_active(entry).await? {
                return Ok(());
            }
            let telegraph_url = entry
                .telegraph_url
                .as_deref()
                .expect("telegraph_url checked above");
            let link_text = format!(
                "📄 [Telegraph 链接]({})",
                teloxide::utils::markdown::escape_link_url(telegraph_url)
            );
            self.notifier
                .send_text(chat_id, &link_text, false)
                .await
                .context("Failed to send telegraph link")?;
            if !self.ensure_entry_active(entry).await? {
                return Ok(());
            }
            self.repo.mark_eh_telegraph_sent(entry.id).await?;
        }

        // Both surfaces are now sent — mark done and clean up ZIP.
        if !self.ensure_entry_active(entry).await? {
            return Ok(());
        }
        self.repo
            .mark_eh_download_done(entry.id, entry.file_size)
            .await?;
        self.cleanup_zip(entry).await;
        info!(
            "Published eh gallery gid={} to chat {}",
            entry.gid, entry.chat_id
        );
        Ok(())
    }

    async fn ensure_entry_active(&self, entry: &eh_download_queue::Model) -> Result<bool> {
        let active = self
            .repo
            .eh_download_is_active(entry.id, &entry.status)
            .await?;
        if !active {
            info!(
                "Skipping canceled EH publish entry {} for chat {}",
                entry.id, entry.chat_id
            );
        }
        Ok(active)
    }

    async fn cleanup_zip(&self, entry: &eh_download_queue::Model) {
        if let Some(ref zip_path) = entry.zip_path {
            let path = std::path::Path::new(zip_path);
            if path.exists() {
                if let Err(e) = tokio::fs::remove_file(path).await {
                    warn!("Failed to delete zip {}: {}", path.display(), e);
                }
            }
        }
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

/// Sanitize a string for use as a filename.
fn sanitize_filename(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            _ => c,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sanitize_filename() {
        assert_eq!(sanitize_filename("test/file:name"), "test_file_name");
        assert_eq!(sanitize_filename("normal"), "normal");
        assert_eq!(sanitize_filename("a\\b/c|d*e?f"), "a_b_c_d_e_f");
    }

    #[test]
    fn test_backoff_delay() {
        assert_eq!(Repo::backoff_delay_secs(0), 60);
        assert_eq!(Repo::backoff_delay_secs(1), 60);
        assert_eq!(Repo::backoff_delay_secs(2), 300);
        assert_eq!(Repo::backoff_delay_secs(3), 900);
        assert_eq!(Repo::backoff_delay_secs(4), 3600);
        assert_eq!(Repo::backoff_delay_secs(99), 3600);
    }
}

#[cfg(test)]
mod integration_tests {
    use super::*;
    use crate::bot::notifier::Notifier;
    use crate::cache::FileCacheManager;
    use crate::config::EhentaiConfig;
    use crate::db::entities::eh_download_queue;
    use crate::db::entities::tasks;
    use crate::db::repo::eh_download_queue::{
        SOURCE_DIRECT, SOURCE_SUBSCRIPTION, STATUS_CANCELED, STATUS_DONE, STATUS_DOWNLOADED,
        STATUS_FAILED, STATUS_PENDING, STATUS_UPLOADED,
    };
    use crate::db::repo::tests_helpers;
    use crate::pixiv::downloader::Downloader;
    use eh_client::PixiUploader;
    use eh_client::{EhClientBuilder, EhCookies, TelegraphClient};
    use reqwest::Client;
    use sea_orm::sea_query::Expr;
    use sea_orm::{
        ActiveModelTrait, ColumnTrait, ConnectionTrait, DbBackend, EntityTrait, QueryFilter, Set,
        Statement,
    };
    use std::io::Write;
    use teloxide::requests::RequesterExt;
    use teloxide::Bot;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn make_notifier(tg_server: &MockServer) -> Notifier {
        let url = url::Url::parse(&tg_server.uri()).unwrap();
        let bot = Bot::new("fake_token").set_api_url(url);
        let throttled = bot.throttle(teloxide::adaptors::throttle::Limits::default());
        let http = Client::new();
        let cache = FileCacheManager::new("data/test_cache", 7);
        let downloader = Arc::new(Downloader::new(http, cache));
        Notifier::new(throttled, downloader)
    }

    fn make_eh_client(eh_server: &MockServer) -> Arc<EhClient> {
        Arc::new(
            EhClientBuilder::new()
                .base_url(&eh_server.uri())
                .api_url(&format!("{}/api.php", eh_server.uri()))
                .cookies(EhCookies {
                    ipb_member_id: Some("12345".into()),
                    ipb_pass_hash: Some("abc".into()),
                    igneous: None,
                    nw: true,
                })
                .build(),
        )
    }

    fn make_telegraph_client(tg_server: &MockServer) -> Arc<TelegraphClient> {
        Arc::new(TelegraphClient::new_with_urls(
            "test_token".to_string(),
            format!("{}/pixi/upload", tg_server.uri()),
            tg_server.uri(),
        ))
    }

    fn make_image_uploader(tg_server: &MockServer) -> Arc<dyn ImageUploader> {
        Arc::new(PixiUploader::new_with_url(format!(
            "{}/pixi/upload",
            tg_server.uri()
        )))
    }

    fn make_config() -> EhentaiConfig {
        EhentaiConfig {
            download_rate_limit_gb: 7,
            download_rate_window_hours: 168,
            download_poll_interval_sec: 60,
            max_push_per_tick: 3,
            max_retry_count: 3,
            send_archive: true,
            upload_telegraph: true,
            ..Default::default()
        }
    }

    fn create_test_zip(path: &std::path::Path, image_count: usize) {
        let file = std::fs::File::create(path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        for i in 0..image_count {
            let name = format!("page{:03}.jpg", i);
            let options = zip::write::SimpleFileOptions::default();
            zip.start_file(name, options).unwrap();
            let data = format!("fake_image_data_{}", i);
            zip.write_all(data.as_bytes()).unwrap();
        }
        zip.finish().unwrap();
    }

    fn create_test_zip_with_sizes(path: &std::path::Path, image_sizes: &[usize]) {
        let file = std::fs::File::create(path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        for (i, size) in image_sizes.iter().enumerate() {
            let name = format!("page{:03}.jpg", i);
            let options = zip::write::SimpleFileOptions::default();
            zip.start_file(name, options).unwrap();
            zip.write_all(&vec![b'a'; *size]).unwrap();
        }
        zip.finish().unwrap();
    }

    #[derive(Debug)]
    struct MultipartFileCount(usize);

    impl wiremock::Match for MultipartFileCount {
        fn matches(&self, request: &wiremock::Request) -> bool {
            let body = String::from_utf8_lossy(&request.body);
            body.matches("name=\"files[]\"").count() == self.0
        }
    }

    async fn mock_tg_send_document(server: &MockServer) {
        let body = serde_json::json!({
            "ok": true,
            "result": {"message_id": 42, "date": 1700000000, "chat": {"id": -100, "type": "private"}}
        });
        Mock::given(method("POST"))
            .and(path("/botfake_token/SendDocument"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(server)
            .await;
    }

    async fn mock_tg_send_message(server: &MockServer) {
        let body = serde_json::json!({
            "ok": true,
            "result": {"message_id": 43, "date": 1700000000, "chat": {"id": -100, "type": "private"}}
        });
        Mock::given(method("POST"))
            .and(path("/botfake_token/SendMessage"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(server)
            .await;
    }

    async fn mock_eh_gallery_page(server: &MockServer, gid: u64, token: &str) {
        let archiver_key = format!("{}--abc123def456", gid);
        let html = format!(
            r#"<html><body>
            <a onclick="return popUp('/archiver.php?gid={gid}&amp;token={token}',480,320)">Archive Download</a>
            </body></html>"#,
            gid = gid,
            token = token
        );
        Mock::given(method("GET"))
            .and(path(format!("/g/{}/{}/", gid, token)))
            .respond_with(ResponseTemplate::new(200).set_body_string(html))
            .mount(server)
            .await;

        let archiver_page_html = format!(
            r#"<html><body><input type="hidden" name="or" value="{}" /></body></html>"#,
            archiver_key
        );
        Mock::given(method("GET"))
            .and(path("/archiver.php"))
            .respond_with(ResponseTemplate::new(200).set_body_string(archiver_page_html))
            .mount(server)
            .await;
    }

    async fn mock_eh_archiver_post(server: &MockServer, download_url: &str) {
        let html = format!(
            r#"<html><script>function gotonext() {{ document.location = "{}?autostart=1"; }}</script></html>"#,
            download_url
        );
        Mock::given(method("POST"))
            .and(path("/archiver.php"))
            .respond_with(ResponseTemplate::new(200).set_body_string(html))
            .mount(server)
            .await;
    }

    async fn mock_eh_archive_download(server: &MockServer, path_str: &str, zip_bytes: Vec<u8>) {
        Mock::given(method("GET"))
            .and(path(path_str))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(zip_bytes))
            .mount(server)
            .await;
    }

    async fn mock_telegraph_upload(server: &MockServer, expected_requests: u64) {
        let body =
            serde_json::json!({"success": true, "direct_url": "https://i.pixi.mg/i/abc123.jpg"});
        Mock::given(method("POST"))
            .and(path("/pixi/upload"))
            .and(MultipartFileCount(1))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .expect(expected_requests)
            .mount(server)
            .await;
    }

    async fn mock_telegraph_create_page(server: &MockServer) {
        let body = serde_json::json!({"ok": true, "result": {"url": "https://telegra.ph/Test-Gallery-01-01"}});
        Mock::given(method("POST"))
            .and(path("/createPage"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(server)
            .await;
    }

    async fn setup_chat(repo: &Repo, chat_id: i64, enabled: bool) {
        repo.upsert_chat(chat_id, "private".into(), None, enabled, Default::default())
            .await
            .unwrap();
    }

    #[allow(clippy::too_many_arguments)]
    async fn insert_queue_entry(
        repo: &Repo,
        chat_id: i64,
        gid: i64,
        token: &str,
        title: &str,
        telegraph: bool,
        status: &str,
        zip_path: Option<&str>,
        telegraph_url: Option<&str>,
    ) -> eh_download_queue::Model {
        let now = Local::now().naive_local();
        let active = eh_download_queue::ActiveModel {
            chat_id: Set(chat_id),
            gid: Set(gid),
            token: Set(token.to_string()),
            title: Set(title.to_string()),
            telegraph: Set(telegraph),
            source: Set(SOURCE_DIRECT.to_string()),
            status: Set(status.to_string()),
            file_size: Set(0),
            error: Set(None),
            retry_count: Set(0),
            created_at: Set(now),
            started_at: Set(None),
            completed_at: Set(None),
            zip_path: Set(zip_path.map(|s| s.to_string())),
            telegraph_url: Set(telegraph_url.map(|s| s.to_string())),
            next_retry_at: Set(None),
            ..Default::default()
        };
        active.insert(repo.db()).await.unwrap()
    }

    #[allow(clippy::too_many_arguments)]
    async fn insert_subscription_queue_entry(
        repo: &Repo,
        chat_id: i64,
        subscription_ids: &str,
        gid: i64,
        token: &str,
        title: &str,
        telegraph: bool,
        status: &str,
        zip_path: Option<&str>,
        telegraph_url: Option<&str>,
    ) -> eh_download_queue::Model {
        let now = Local::now().naive_local();
        let active = eh_download_queue::ActiveModel {
            chat_id: Set(chat_id),
            gid: Set(gid),
            token: Set(token.to_string()),
            title: Set(title.to_string()),
            telegraph: Set(telegraph),
            source: Set(SOURCE_SUBSCRIPTION.to_string()),
            subscription_ids: Set(Some(subscription_ids.to_string())),
            status: Set(status.to_string()),
            file_size: Set(0),
            error: Set(None),
            retry_count: Set(0),
            created_at: Set(now),
            started_at: Set(None),
            completed_at: Set(None),
            zip_path: Set(zip_path.map(|s| s.to_string())),
            telegraph_url: Set(telegraph_url.map(|s| s.to_string())),
            next_retry_at: Set(None),
            ..Default::default()
        };
        active.insert(repo.db()).await.unwrap()
    }

    #[tokio::test]
    async fn test_collect_overflow_pending_enqueued_on_next_tick() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        setup_chat(&repo, -100, true).await;

        // Create task and subscription
        let task_key =
            crate::db::types::EhTaskKey::new("artist:test", 0, &crate::db::types::EhFilter::new());
        let task_value = task_key.to_task_value();
        let task = repo
            .get_or_create_task(
                crate::db::types::TaskType::Ehentai,
                task_value.clone(),
                None,
            )
            .await
            .unwrap();

        // Make the task immediately available (get_or_create_task sets next_poll_at 60s in future)
        let task_id = task.id;
        let mut active: tasks::ActiveModel = task.into();
        active.next_poll_at =
            Set(chrono::Local::now().naive_local() - chrono::Duration::seconds(1));
        active.update(repo.db()).await.unwrap();

        repo.upsert_eh_subscription(-100, task_id, crate::db::types::TagFilter::default(), None)
            .await
            .unwrap();

        let eh_server = MockServer::start().await;
        let _tg_server = MockServer::start().await;

        mock_eh_search_with_four_galleries(&eh_server).await;
        mock_eh_metadata_for_four_galleries(&eh_server).await;

        let config = Arc::new(make_config());
        let engine = EhEngine::new(
            Arc::clone(&repo),
            make_eh_client(&eh_server),
            Arc::clone(&config),
            true,
            60,
        );
        engine.tick().await.unwrap();

        let queued_after_first = repo.count_pending_eh_downloads().await.unwrap();
        assert_eq!(
            queued_after_first, 3,
            "first tick should enqueue 3 galleries (max_push_per_tick=3)"
        );

        // Second tick: should consume the pending backlog (4th gallery) without re-fetching
        // from cursor. The 4th gallery was overflow, not silently dropped.
        // Reset next_poll_at to make the task available again.
        let task_model = repo
            .get_task_by_type_value(crate::db::types::TaskType::Ehentai, &task_value)
            .await
            .unwrap()
            .unwrap();
        let mut active: tasks::ActiveModel = task_model.into();
        active.next_poll_at =
            Set(chrono::Local::now().naive_local() - chrono::Duration::seconds(1));
        active.update(repo.db()).await.unwrap();

        engine.tick().await.unwrap();
        let queued_after_second = repo.count_pending_eh_downloads().await.unwrap();
        assert_eq!(
            queued_after_second, 4,
            "second tick should drain pending backlog: 4 total enqueued"
        );

        let sub = repo
            .list_subscriptions_by_task(task_id)
            .await
            .unwrap()
            .pop()
            .unwrap();
        let state = eh_tag_subscription_state(&sub).unwrap();
        assert!(state.pending_galleries.is_empty());
        assert_eq!(state.latest_posted_ts, 400);
        assert_eq!(state.pending_high_water_ts, 0);
    }

    #[tokio::test]
    async fn test_collect_overflow_does_not_advance_cursor_until_pending_drained() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        setup_chat(&repo, -100, true).await;

        let task_key =
            crate::db::types::EhTaskKey::new("artist:test", 0, &crate::db::types::EhFilter::new());
        let task_value = task_key.to_task_value();
        let task = repo
            .get_or_create_task(
                crate::db::types::TaskType::Ehentai,
                task_value.clone(),
                None,
            )
            .await
            .unwrap();
        let task_id = task.id;
        let mut active: tasks::ActiveModel = task.into();
        active.next_poll_at =
            Set(chrono::Local::now().naive_local() - chrono::Duration::seconds(1));
        active.update(repo.db()).await.unwrap();

        repo.upsert_eh_subscription(-100, task_id, crate::db::types::TagFilter::default(), None)
            .await
            .unwrap();

        let eh_server = MockServer::start().await;
        mock_eh_search_with_four_galleries(&eh_server).await;
        mock_eh_metadata_for_four_galleries(&eh_server).await;

        let engine = EhEngine::new(
            Arc::clone(&repo),
            make_eh_client(&eh_server),
            Arc::new(make_config()),
            true,
            60,
        );
        engine.tick().await.unwrap();

        let sub = repo
            .list_subscriptions_by_task(task_id)
            .await
            .unwrap()
            .pop()
            .unwrap();
        let state = eh_tag_subscription_state(&sub).unwrap();
        assert_eq!(state.latest_posted_ts, 0);
        assert_eq!(state.pending_galleries.len(), 1);
        assert_eq!(state.pending_galleries[0].gid, 1004);
        assert_eq!(state.pending_high_water_ts, 400);
    }

    #[tokio::test]
    async fn test_collect_telegraph_subscription_without_token_enqueues_upload_intent() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        setup_chat(&repo, -100, true).await;

        let task_key =
            crate::db::types::EhTaskKey::new("artist:test", 0, &crate::db::types::EhFilter::new());
        let task_value = task_key.to_task_value();
        let task = repo
            .get_or_create_task(
                crate::db::types::TaskType::Ehentai,
                task_value.clone(),
                None,
            )
            .await
            .unwrap();
        let task_id = task.id;
        let mut active: tasks::ActiveModel = task.into();
        active.next_poll_at =
            Set(chrono::Local::now().naive_local() - chrono::Duration::seconds(1));
        active.update(repo.db()).await.unwrap();

        repo.upsert_eh_subscription(
            -100,
            task_id,
            crate::db::types::TagFilter::default(),
            Some(crate::db::types::EhFilter {
                telegraph: true,
                ..Default::default()
            }),
        )
        .await
        .unwrap();

        let eh_server = MockServer::start().await;
        mock_eh_search_with_four_galleries(&eh_server).await;
        mock_eh_metadata_for_four_galleries(&eh_server).await;

        let mut config = make_config();
        config.upload_telegraph = true;
        config.telegraph_access_token = None;
        let engine = EhEngine::new(
            Arc::clone(&repo),
            make_eh_client(&eh_server),
            Arc::new(config),
            true,
            60,
        );
        engine.tick().await.unwrap();

        let claimed_download = repo.get_next_for_download().await.unwrap().unwrap();
        assert!(claimed_download.telegraph);
        repo.mark_eh_download_downloaded(claimed_download.id, 100, "data/test_cache/archive.zip")
            .await
            .unwrap();

        let claimed_upload = repo.get_next_for_upload().await.unwrap().unwrap();
        assert_eq!(claimed_upload.gid, claimed_download.gid);
    }

    #[tokio::test]
    async fn test_collect_telegraph_unavailable_enqueues_archive_only() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        setup_chat(&repo, -100, true).await;

        let task_key =
            crate::db::types::EhTaskKey::new("artist:test", 0, &crate::db::types::EhFilter::new());
        let task_value = task_key.to_task_value();
        let task = repo
            .get_or_create_task(crate::db::types::TaskType::Ehentai, task_value, None)
            .await
            .unwrap();
        let task_id = task.id;
        let mut active: tasks::ActiveModel = task.into();
        active.next_poll_at =
            Set(chrono::Local::now().naive_local() - chrono::Duration::seconds(1));
        active.update(repo.db()).await.unwrap();

        repo.upsert_eh_subscription(
            -100,
            task_id,
            crate::db::types::TagFilter::default(),
            Some(crate::db::types::EhFilter {
                telegraph: true,
                ..Default::default()
            }),
        )
        .await
        .unwrap();

        let eh_server = MockServer::start().await;
        mock_eh_search_with_four_galleries(&eh_server).await;
        mock_eh_metadata_for_four_galleries(&eh_server).await;

        let mut config = make_config();
        config.upload_telegraph = true;
        config.telegraph_access_token = None;
        let engine = EhEngine::new(
            Arc::clone(&repo),
            make_eh_client(&eh_server),
            Arc::new(config),
            false,
            60,
        );
        engine.tick().await.unwrap();

        let claimed_download = repo.get_next_for_download().await.unwrap().unwrap();
        assert!(!claimed_download.telegraph);
        repo.mark_eh_download_downloaded(claimed_download.id, 100, "data/test_cache/archive.zip")
            .await
            .unwrap();

        assert!(repo.get_next_for_upload().await.unwrap().is_none());
        let claimed_publish = repo.get_next_for_publish().await.unwrap().unwrap();
        assert_eq!(claimed_publish.gid, claimed_download.gid);
    }

    #[tokio::test]
    async fn test_collect_drains_pending_backlog_when_search_empty() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        setup_chat(&repo, -100, true).await;

        let task_key =
            crate::db::types::EhTaskKey::new("artist:test", 0, &crate::db::types::EhFilter::new());
        let task_value = task_key.to_task_value();
        let task = repo
            .get_or_create_task(
                crate::db::types::TaskType::Ehentai,
                task_value.clone(),
                None,
            )
            .await
            .unwrap();
        let task_id = task.id;
        let mut active: tasks::ActiveModel = task.into();
        active.next_poll_at =
            Set(chrono::Local::now().naive_local() - chrono::Duration::seconds(1));
        active.update(repo.db()).await.unwrap();

        repo.upsert_eh_subscription(-100, task_id, crate::db::types::TagFilter::default(), None)
            .await
            .unwrap();
        let sub = repo
            .list_subscriptions_by_task(task_id)
            .await
            .unwrap()
            .pop()
            .unwrap();
        repo.update_subscription_latest_data(
            sub.id,
            Some(SubscriptionState::EhTag(EhTagState {
                pushed_gids: Vec::new(),
                latest_posted_ts: 0,
                pending_galleries: vec![EhPendingGallery {
                    gid: 2001,
                    token: "eeeeeeeeee".to_string(),
                    title: "Pending Gallery".to_string(),
                    posted: 500,
                }],
                pending_high_water_ts: 500,
            })),
        )
        .await
        .unwrap();

        let eh_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_string(""))
            .mount(&eh_server)
            .await;

        let engine = EhEngine::new(
            Arc::clone(&repo),
            make_eh_client(&eh_server),
            Arc::new(make_config()),
            true,
            60,
        );
        engine.tick().await.unwrap();

        assert_eq!(repo.count_pending_eh_downloads().await.unwrap(), 1);
        let sub = repo
            .list_subscriptions_by_task(task_id)
            .await
            .unwrap()
            .pop()
            .unwrap();
        let state = eh_tag_subscription_state(&sub).unwrap();
        assert!(state.pending_galleries.is_empty());
        assert_eq!(state.latest_posted_ts, 500);
        assert_eq!(state.pending_high_water_ts, 0);
    }

    #[tokio::test]
    async fn test_collect_drains_pending_backlog_before_search_failure() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        setup_chat(&repo, -100, true).await;

        let task_key =
            crate::db::types::EhTaskKey::new("artist:test", 0, &crate::db::types::EhFilter::new());
        let task_value = task_key.to_task_value();
        let task = repo
            .get_or_create_task(
                crate::db::types::TaskType::Ehentai,
                task_value.clone(),
                None,
            )
            .await
            .unwrap();
        let task_id = task.id;
        let mut active: tasks::ActiveModel = task.into();
        active.next_poll_at =
            Set(chrono::Local::now().naive_local() - chrono::Duration::seconds(1));
        active.update(repo.db()).await.unwrap();

        repo.upsert_eh_subscription(-100, task_id, crate::db::types::TagFilter::default(), None)
            .await
            .unwrap();
        let sub = repo
            .list_subscriptions_by_task(task_id)
            .await
            .unwrap()
            .pop()
            .unwrap();
        repo.update_subscription_latest_data(
            sub.id,
            Some(SubscriptionState::EhTag(EhTagState {
                pushed_gids: Vec::new(),
                latest_posted_ts: 0,
                pending_galleries: vec![EhPendingGallery {
                    gid: 2101,
                    token: "ffffffffff".to_string(),
                    title: "Pending Before Failure".to_string(),
                    posted: 600,
                }],
                pending_high_water_ts: 600,
            })),
        )
        .await
        .unwrap();

        let eh_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&eh_server)
            .await;

        let engine = EhEngine::new(
            Arc::clone(&repo),
            make_eh_client(&eh_server),
            Arc::new(make_config()),
            true,
            60,
        );
        engine.tick().await.unwrap();

        assert_eq!(repo.count_pending_eh_downloads().await.unwrap(), 1);
        let sub = repo
            .list_subscriptions_by_task(task_id)
            .await
            .unwrap()
            .pop()
            .unwrap();
        let state = eh_tag_subscription_state(&sub).unwrap();
        assert!(state.pending_galleries.is_empty());
        assert_eq!(state.latest_posted_ts, 600);
        let task = repo
            .get_task_by_type_value(crate::db::types::TaskType::Ehentai, &task_value)
            .await
            .unwrap()
            .unwrap();
        assert!(task.next_poll_at > chrono::Local::now().naive_local());
    }

    #[tokio::test]
    async fn test_collect_empty_search_does_not_write_zero_state_for_fresh_sub() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        setup_chat(&repo, -100, true).await;
        setup_chat(&repo, -200, true).await;

        let task_key =
            crate::db::types::EhTaskKey::new("artist:test", 0, &crate::db::types::EhFilter::new());
        let task_value = task_key.to_task_value();
        let task = repo
            .get_or_create_task(
                crate::db::types::TaskType::Ehentai,
                task_value.clone(),
                None,
            )
            .await
            .unwrap();
        let task_id = task.id;
        let mut active: tasks::ActiveModel = task.into();
        active.next_poll_at =
            Set(chrono::Local::now().naive_local() - chrono::Duration::seconds(1));
        active.update(repo.db()).await.unwrap();

        repo.upsert_eh_subscription(-100, task_id, crate::db::types::TagFilter::default(), None)
            .await
            .unwrap();
        repo.upsert_eh_subscription(-200, task_id, crate::db::types::TagFilter::default(), None)
            .await
            .unwrap();
        let subs = repo.list_subscriptions_by_task(task_id).await.unwrap();
        let existing = subs.iter().find(|s| s.chat_id == -100).unwrap();
        repo.update_subscription_latest_data(
            existing.id,
            Some(SubscriptionState::EhTag(EhTagState {
                pushed_gids: vec![999],
                latest_posted_ts: 500,
                pending_galleries: Vec::new(),
                pending_high_water_ts: 0,
            })),
        )
        .await
        .unwrap();

        let eh_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_string(""))
            .mount(&eh_server)
            .await;

        let engine = EhEngine::new(
            Arc::clone(&repo),
            make_eh_client(&eh_server),
            Arc::new(make_config()),
            true,
            60,
        );
        engine.tick().await.unwrap();

        let fresh = repo
            .list_subscriptions_by_task(task_id)
            .await
            .unwrap()
            .into_iter()
            .find(|s| s.chat_id == -200)
            .unwrap();
        let state = eh_tag_subscription_state(&fresh).unwrap();
        assert_eq!(state.latest_posted_ts, 500);
    }

    #[tokio::test]
    async fn test_collect_enqueue_failure_persists_failed_and_remaining_backlog() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        setup_chat(&repo, -100, true).await;

        repo.db()
            .execute(Statement::from_string(
                DbBackend::Sqlite,
                r#"
                CREATE TRIGGER fail_eh_enqueue_1002
                BEFORE INSERT ON eh_download_queue
                WHEN NEW.gid = 1002
                BEGIN
                    SELECT RAISE(FAIL, 'injected enqueue failure');
                END
                "#,
            ))
            .await
            .unwrap();

        let task_key =
            crate::db::types::EhTaskKey::new("artist:test", 0, &crate::db::types::EhFilter::new());
        let task_value = task_key.to_task_value();
        let task = repo
            .get_or_create_task(
                crate::db::types::TaskType::Ehentai,
                task_value.clone(),
                None,
            )
            .await
            .unwrap();
        let task_id = task.id;
        let mut active: tasks::ActiveModel = task.into();
        active.next_poll_at =
            Set(chrono::Local::now().naive_local() - chrono::Duration::seconds(1));
        active.update(repo.db()).await.unwrap();

        repo.upsert_eh_subscription(-100, task_id, crate::db::types::TagFilter::default(), None)
            .await
            .unwrap();

        let eh_server = MockServer::start().await;
        mock_eh_search_with_four_galleries(&eh_server).await;
        mock_eh_metadata_for_four_galleries(&eh_server).await;

        let engine = EhEngine::new(
            Arc::clone(&repo),
            make_eh_client(&eh_server),
            Arc::new(make_config()),
            true,
            60,
        );
        engine.tick().await.unwrap();

        assert_eq!(repo.count_pending_eh_downloads().await.unwrap(), 1);
        let sub = repo
            .list_subscriptions_by_task(task_id)
            .await
            .unwrap()
            .pop()
            .unwrap();
        let state = eh_tag_subscription_state(&sub).unwrap();
        assert_eq!(state.latest_posted_ts, 0);
        assert_eq!(state.pending_galleries.len(), 3);
        assert_eq!(state.pending_galleries[0].gid, 1002);
        assert_eq!(state.pending_galleries[1].gid, 1003);
        assert_eq!(state.pending_galleries[2].gid, 1004);
        assert_eq!(state.pending_high_water_ts, 400);
        let task = repo
            .get_task_by_type_value(crate::db::types::TaskType::Ehentai, &task_value)
            .await
            .unwrap()
            .unwrap();
        assert!(task.next_poll_at > chrono::Local::now().naive_local());
    }

    #[tokio::test]
    async fn test_update_sub_state_no_new_does_not_rewind_cursor() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        setup_chat(&repo, -100, true).await;
        let task = repo
            .get_or_create_task(
                crate::db::types::TaskType::Ehentai,
                "eh:artist:test".to_string(),
                None,
            )
            .await
            .unwrap();
        repo.upsert_eh_subscription(-100, task.id, crate::db::types::TagFilter::default(), None)
            .await
            .unwrap();
        let sub = repo
            .list_subscriptions_by_task(task.id)
            .await
            .unwrap()
            .pop()
            .unwrap();
        repo.update_subscription_latest_data(
            sub.id,
            Some(SubscriptionState::EhTag(EhTagState {
                pushed_gids: vec![1],
                latest_posted_ts: 500,
                pending_galleries: Vec::new(),
                pending_high_water_ts: 0,
            })),
        )
        .await
        .unwrap();
        let sub = repo
            .list_subscriptions_by_task(task.id)
            .await
            .unwrap()
            .pop()
            .unwrap();
        let engine = EhEngine::new(
            Arc::clone(&repo),
            make_eh_client(&MockServer::start().await),
            Arc::new(make_config()),
            true,
            60,
        );

        engine.update_sub_state_no_new(&sub, 100).await;

        let sub = repo
            .list_subscriptions_by_task(task.id)
            .await
            .unwrap()
            .pop()
            .unwrap();
        let state = eh_tag_subscription_state(&sub).unwrap();
        assert_eq!(state.latest_posted_ts, 500);
    }

    async fn mock_eh_search_with_four_galleries(server: &MockServer) {
        let html = r#"
        <div class="gl1t"><a href="https://e-hentai.org/g/1001/aaaaaaaaaa/"><div class="glink">Gallery 1</div></a></div>
        <div class="gl1t"><a href="https://e-hentai.org/g/1002/bbbbbbbbbb/"><div class="glink">Gallery 2</div></a></div>
        <div class="gl1t"><a href="https://e-hentai.org/g/1003/cccccccccc/"><div class="glink">Gallery 3</div></a></div>
        <div class="gl1t"><a href="https://e-hentai.org/g/1004/dddddddddd/"><div class="glink">Gallery 4</div></a></div>
        "#;
        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_string(html))
            .mount(server)
            .await;
    }

    async fn mock_eh_metadata_for_four_galleries(server: &MockServer) {
        Mock::given(method("POST"))
            .and(path("/api.php"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "gmetadata": [
                    {"gid": 1001, "token": "aaaaaaaaaa", "title": "Gallery 1", "title_jpn": null, "category": "Doujinshi", "thumb": "https://ehgt.org/t/1.jpg", "uploader": "tester", "posted": "100", "filecount": "10", "filesize": 1000, "expunged": false, "rating": "4.0", "tags": ["artist:test"]},
                    {"gid": 1002, "token": "bbbbbbbbbb", "title": "Gallery 2", "title_jpn": null, "category": "Doujinshi", "thumb": "https://ehgt.org/t/2.jpg", "uploader": "tester", "posted": "200", "filecount": "10", "filesize": 1000, "expunged": false, "rating": "4.0", "tags": ["artist:test"]},
                    {"gid": 1003, "token": "cccccccccc", "title": "Gallery 3", "title_jpn": null, "category": "Doujinshi", "thumb": "https://ehgt.org/t/3.jpg", "uploader": "tester", "posted": "300", "filecount": "10", "filesize": 1000, "expunged": false, "rating": "4.0", "tags": ["artist:test"]},
                    {"gid": 1004, "token": "dddddddddd", "title": "Gallery 4", "title_jpn": null, "category": "Doujinshi", "thumb": "https://ehgt.org/t/4.jpg", "uploader": "tester", "posted": "400", "filecount": "10", "filesize": 1000, "expunged": false, "rating": "4.0", "tags": ["artist:test"]}
                ]
            })))
            .mount(server)
            .await;
    }

    #[test]
    fn download_in_progress_downcasts_through_anyhow_context() {
        // Simulate the error propagation path in process():
        // eh_client::Error::DownloadInProgress → .context("...") → anyhow::Error
        let inner = eh_client::Error::Other("simulated failure".into());
        let client_err = eh_client::Error::DownloadInProgress {
            inner: Box::new(inner),
        };
        // Context trait is implemented on Result<T, E>, not bare E.
        // Wrap in Err to match how process() propagates the error.
        let result: eh_client::Result<()> = Err(client_err);
        let wrapped: anyhow::Error = result.context("Failed to download archive").unwrap_err();

        let found = wrapped
            .chain()
            .find_map(|c| c.downcast_ref::<eh_client::Error>())
            .map(|e| matches!(e, eh_client::Error::DownloadInProgress { .. }))
            .unwrap_or(false);
        assert!(
            found,
            "DownloadInProgress must be findable through anyhow error chain"
        );
    }

    // === Download Worker Tests ===

    #[tokio::test]
    async fn test_download_worker_downloads_archive() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let eh_server = MockServer::start().await;
        let temp = tempfile::tempdir().unwrap();

        setup_chat(&repo, -100, true).await;
        let entry = insert_queue_entry(
            &repo,
            -100,
            123456,
            "abcdef0123",
            "Test Gallery",
            false,
            "pending",
            None,
            None,
        )
        .await;

        mock_eh_gallery_page(&eh_server, 123456, "abcdef0123").await;
        let download_url = format!("{}/archive/123456/token/0", eh_server.uri());
        mock_eh_archiver_post(&eh_server, &download_url).await;

        let zip_temp = tempfile::tempdir().unwrap();
        let zip_path = zip_temp.path().join("test.zip");
        create_test_zip(&zip_path, 3);
        let zip_bytes = std::fs::read(&zip_path).unwrap();
        mock_eh_archive_download(&eh_server, "/archive/123456/token/0", zip_bytes).await;

        let worker = EhDownloadWorker::new(
            Arc::clone(&repo),
            make_eh_client(&eh_server),
            Arc::new(make_config()),
            temp.path().to_path_buf(),
        );

        worker.tick().await.unwrap();

        let updated = eh_download_queue::Entity::find_by_id(entry.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(updated.status, STATUS_DOWNLOADED);
        assert!(updated.zip_path.is_some());
        assert!(updated.file_size > 0);
        assert!(std::path::Path::new(updated.zip_path.as_ref().unwrap()).exists());
    }

    #[tokio::test]
    async fn test_download_worker_rate_limit_skips() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let eh_server = MockServer::start().await;
        let temp = tempfile::tempdir().unwrap();

        setup_chat(&repo, -100, true).await;

        // Pre-fill a done entry to hit rate limit
        let now = Local::now().naive_local();
        let big = eh_download_queue::ActiveModel {
            chat_id: Set(-100),
            gid: Set(999999),
            token: Set("x".into()),
            title: Set("Big".into()),
            telegraph: Set(false),
            source: Set(SOURCE_DIRECT.into()),
            status: Set("done".into()),
            file_size: Set(11_000_000_000),
            error: Set(None),
            retry_count: Set(0),
            created_at: Set(now),
            started_at: Set(Some(now)),
            completed_at: Set(Some(now)),
            zip_path: Set(None),
            telegraph_url: Set(None),
            next_retry_at: Set(None),
            ..Default::default()
        };
        big.insert(repo.db()).await.unwrap();

        let entry = insert_queue_entry(
            &repo,
            -100,
            123456,
            "abcdef0123",
            "Test",
            false,
            "pending",
            None,
            None,
        )
        .await;

        let worker = EhDownloadWorker::new(
            Arc::clone(&repo),
            make_eh_client(&eh_server),
            Arc::new(make_config()),
            temp.path().to_path_buf(),
        );
        worker.tick().await.unwrap();

        let updated = eh_download_queue::Entity::find_by_id(entry.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            updated.status, "pending",
            "should remain pending due to rate limit"
        );
    }

    #[tokio::test]
    async fn test_download_worker_chat_disabled_schedules_retry() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let eh_server = MockServer::start().await;
        let temp = tempfile::tempdir().unwrap();

        setup_chat(&repo, -100, false).await; // disabled
        let entry = insert_queue_entry(
            &repo,
            -100,
            123456,
            "abcdef0123",
            "Test",
            false,
            "pending",
            None,
            None,
        )
        .await;

        let worker = EhDownloadWorker::new(
            Arc::clone(&repo),
            make_eh_client(&eh_server),
            Arc::new(make_config()),
            temp.path().to_path_buf(),
        );
        worker.tick().await.unwrap();

        let updated = eh_download_queue::Entity::find_by_id(entry.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        // Chat disabled → entry goes back to pending with retry scheduled (not silently done)
        assert_eq!(
            updated.status, "pending",
            "should be pending for retry, not silently done"
        );
        assert_eq!(
            updated.retry_count, 0,
            "chat disabled defer should not increment retry_count"
        );
        assert!(
            updated.next_retry_at.is_some(),
            "should have next_retry_at set"
        );
    }

    #[tokio::test]
    async fn test_download_worker_failure_schedules_retry() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let eh_server = MockServer::start().await;
        let temp = tempfile::tempdir().unwrap();

        setup_chat(&repo, -100, true).await;
        let entry = insert_queue_entry(
            &repo,
            -100,
            123456,
            "abcdef0123",
            "Test",
            false,
            "pending",
            None,
            None,
        )
        .await;

        mock_eh_gallery_page(&eh_server, 123456, "abcdef0123").await;
        // archiver.php POST returns 500
        Mock::given(method("POST"))
            .and(path("/archiver.php"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&eh_server)
            .await;

        let worker = EhDownloadWorker::new(
            Arc::clone(&repo),
            make_eh_client(&eh_server),
            Arc::new(make_config()),
            temp.path().to_path_buf(),
        );
        worker.tick().await.unwrap();

        let updated = eh_download_queue::Entity::find_by_id(entry.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            updated.status, "pending",
            "should be back to pending for retry"
        );
        assert_eq!(updated.retry_count, 1);
        assert!(
            updated.next_retry_at.is_some(),
            "should have next_retry_at set"
        );
    }

    #[tokio::test]
    async fn test_download_worker_permanent_failure_cleans_partial_archive() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let eh_server = MockServer::start().await;
        let temp = tempfile::tempdir().unwrap();

        setup_chat(&repo, -100, true).await;
        let entry = insert_queue_entry(
            &repo,
            -100,
            123456,
            "abcdef0123",
            "Test",
            false,
            "pending",
            None,
            None,
        )
        .await;

        eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::RetryCount,
                Expr::value(make_config().max_retry_count as i32),
            )
            .filter(eh_download_queue::Column::Id.eq(entry.id))
            .exec(repo.db())
            .await
            .unwrap();

        let eh_cache = temp.path().join("eh_cache");
        std::fs::create_dir_all(&eh_cache).unwrap();
        let zip_path = eh_cache.join("123456_abcdef0123.zip");
        let part_path = zip_path.with_extension("zip.part");
        std::fs::write(&zip_path, b"PK\x03\x04stale").unwrap();
        std::fs::write(&part_path, b"PK\x03\x04partial").unwrap();

        mock_eh_gallery_page(&eh_server, 123456, "abcdef0123").await;
        Mock::given(method("POST"))
            .and(path("/archiver.php"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&eh_server)
            .await;

        let worker = EhDownloadWorker::new(
            Arc::clone(&repo),
            make_eh_client(&eh_server),
            Arc::new(make_config()),
            temp.path().to_path_buf(),
        );
        worker.tick().await.unwrap();

        let updated = eh_download_queue::Entity::find_by_id(entry.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(updated.status, STATUS_FAILED);
        assert!(!zip_path.exists(), "final ZIP should be cleaned");
        assert!(!part_path.exists(), "partial ZIP should be cleaned");
    }

    #[tokio::test]
    async fn test_download_worker_progress_failure_defers_without_retry() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let eh_server = MockServer::start().await;
        let temp = tempfile::tempdir().unwrap();

        setup_chat(&repo, -100, true).await;
        let entry = insert_queue_entry(
            &repo,
            -100,
            123456,
            "abcdef0123",
            "Test",
            false,
            "pending",
            None,
            None,
        )
        .await;

        mock_eh_gallery_page(&eh_server, 123456, "abcdef0123").await;
        let download_url = format!("{}/archive/123456/token/0", eh_server.uri());
        mock_eh_archiver_post(&eh_server, &download_url).await;

        // Pre-seed 1-byte .part so the 206 response takes the append path and
        // runs validate_content_range. Content-Range start=1 matches existing_len=1.
        let eh_cache = temp.path().join("eh_cache");
        tokio::fs::create_dir_all(&eh_cache).await.unwrap();
        let part_path = eh_cache.join("123456_abcdef0123.zip.part");
        tokio::fs::write(&part_path, b"x").await.unwrap();

        // 206 with valid Content-Range (start=1==existing_len, end+1==total → validate passes)
        // but body smaller than claimed (>10KB) → written < expected_total → error
        // after writing >10KB → made_progress=true → DownloadInProgress.
        // Note: the mock returns the same fixed Content-Range on every attempt. After the
        // first append the start no longer matches existing_len, so validate_content_range
        // fails before writing further bytes; only the first attempt appends 20000 bytes.
        let partial_body = vec![0u8; 20000];
        Mock::given(method("GET"))
            .and(path("/archive/123456/token/0"))
            .respond_with(
                ResponseTemplate::new(206)
                    .insert_header("Content-Range", "bytes 1-99999/100000")
                    .set_body_bytes(partial_body.clone()),
            )
            // 4 attempts per ARCHIVE_DOWNLOAD_MAX_ATTEMPTS
            .expect(4)
            .mount(&eh_server)
            .await;

        let worker = EhDownloadWorker::new(
            Arc::clone(&repo),
            make_eh_client(&eh_server),
            Arc::new(make_config()),
            temp.path().to_path_buf(),
        );
        worker.tick().await.unwrap();

        let updated = eh_download_queue::Entity::find_by_id(entry.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            updated.status, STATUS_PENDING,
            "should be pending for deferred retry"
        );
        assert_eq!(
            updated.retry_count, 0,
            "DownloadInProgress should NOT increment retry_count"
        );
        assert!(
            updated.next_retry_at.is_some(),
            "should have next_retry_at set by defer_eh_download"
        );

        // .part file should be preserved for resumption.
        assert!(
            part_path.exists(),
            ".part file should be preserved for resumption"
        );
        let part_size = std::fs::metadata(&part_path).unwrap().len();
        assert_eq!(
            part_size, 20001,
            ".part should contain 20001 bytes (1 pre-seeded + 20000 written on first attempt), got {}",
            part_size
        );
    }

    // === Upload Worker Tests ===

    #[tokio::test]
    async fn test_upload_worker_full_flow() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let tg_server = MockServer::start().await;

        setup_chat(&repo, -100, true).await;

        // Create a real ZIP with images
        let temp = tempfile::tempdir().unwrap();
        let zip_path = temp.path().join("gallery.zip");
        create_test_zip(&zip_path, 3);
        let zip_path_str = zip_path.to_string_lossy().to_string();

        let entry = insert_queue_entry(
            &repo,
            -100,
            123456,
            "abcdef0123",
            "Test Gallery",
            true,
            STATUS_DOWNLOADED,
            Some(&zip_path_str),
            None,
        )
        .await;

        mock_telegraph_upload(&tg_server, 3).await;
        mock_telegraph_create_page(&tg_server).await;

        let worker = EhUploadWorker::new(
            Arc::clone(&repo),
            make_notifier(&tg_server),
            make_telegraph_client(&tg_server),
            make_image_uploader(&tg_server),
            Arc::new(make_config()),
        );
        worker.tick().await.unwrap();

        let updated = eh_download_queue::Entity::find_by_id(entry.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(updated.status, STATUS_UPLOADED);
        assert!(updated.telegraph_url.is_some());
    }

    #[tokio::test]
    async fn test_upload_worker_includes_images_larger_than_six_mib() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let tg_server = MockServer::start().await;

        setup_chat(&repo, -100, true).await;

        let temp = tempfile::tempdir().unwrap();
        let zip_path = temp.path().join("large_gallery.zip");
        create_test_zip_with_sizes(&zip_path, &[1024, 6 * 1024 * 1024 + 1, 2048]);
        let zip_path_str = zip_path.to_string_lossy().to_string();

        let entry = insert_queue_entry(
            &repo,
            -100,
            123456,
            "abcdef0123",
            "Large Gallery",
            true,
            STATUS_DOWNLOADED,
            Some(&zip_path_str),
            None,
        )
        .await;

        let upload_body = serde_json::json!({
            "success": true,
            "direct_url": "https://i.pixi.mg/i/large.jpg"
        });
        Mock::given(method("POST"))
            .and(path("/pixi/upload"))
            .and(MultipartFileCount(1))
            .respond_with(ResponseTemplate::new(200).set_body_json(upload_body))
            .expect(3)
            .mount(&tg_server)
            .await;
        mock_telegraph_create_page(&tg_server).await;
        let worker = EhUploadWorker::new(
            Arc::clone(&repo),
            make_notifier(&tg_server),
            make_telegraph_client(&tg_server),
            make_image_uploader(&tg_server),
            Arc::new(make_config()),
        );
        worker.tick().await.unwrap();

        let updated = eh_download_queue::Entity::find_by_id(entry.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(updated.status, STATUS_UPLOADED);
    }

    #[tokio::test]
    async fn test_upload_worker_no_images_fails() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let tg_server = MockServer::start().await;

        setup_chat(&repo, -100, true).await;

        // Create ZIP with only .txt files
        let temp = tempfile::tempdir().unwrap();
        let zip_path = temp.path().join("no_images.zip");
        {
            let file = std::fs::File::create(&zip_path).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            zip.start_file("readme.txt", zip::write::SimpleFileOptions::default())
                .unwrap();
            zip.write_all(b"no images").unwrap();
            zip.finish().unwrap();
        }
        let zip_path_str = zip_path.to_string_lossy().to_string();

        let entry = insert_queue_entry(
            &repo,
            -100,
            123456,
            "abcdef0123",
            "Test",
            true,
            STATUS_DOWNLOADED,
            Some(&zip_path_str),
            None,
        )
        .await;

        let worker = EhUploadWorker::new(
            Arc::clone(&repo),
            make_notifier(&tg_server),
            make_telegraph_client(&tg_server),
            make_image_uploader(&tg_server),
            Arc::new(make_config()),
        );
        worker.tick().await.unwrap();

        let updated = eh_download_queue::Entity::find_by_id(entry.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            updated.status, STATUS_DOWNLOADED,
            "should be back to downloaded for retry"
        );
        assert_eq!(updated.retry_count, 1);
    }

    // === Publish Worker Tests ===

    #[tokio::test]
    async fn test_publish_worker_no_telegraph() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let tg_server = MockServer::start().await;

        setup_chat(&repo, -100, true).await;

        let temp = tempfile::tempdir().unwrap();
        let zip_path = temp.path().join("gallery.zip");
        create_test_zip(&zip_path, 2);
        let zip_path_str = zip_path.to_string_lossy().to_string();

        let entry = insert_queue_entry(
            &repo,
            -100,
            123456,
            "abcdef0123",
            "Test Gallery",
            false,
            STATUS_DOWNLOADED,
            Some(&zip_path_str),
            None,
        )
        .await;

        mock_tg_send_document(&tg_server).await;

        let eh_server = MockServer::start().await;
        let worker = EhPublishWorker::new(
            Arc::clone(&repo),
            make_notifier(&tg_server),
            make_eh_client(&eh_server),
            Arc::new(make_config()),
        );
        worker.tick().await.unwrap();

        let updated = eh_download_queue::Entity::find_by_id(entry.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(updated.status, "done");
        // ZIP should be deleted
        assert!(!zip_path.exists(), "ZIP should be deleted after publish");
    }

    #[tokio::test]
    async fn test_publish_worker_with_telegraph() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let tg_server = MockServer::start().await;

        setup_chat(&repo, -100, true).await;

        let temp = tempfile::tempdir().unwrap();
        let zip_path = temp.path().join("gallery.zip");
        create_test_zip(&zip_path, 2);
        let zip_path_str = zip_path.to_string_lossy().to_string();

        let entry = insert_queue_entry(
            &repo,
            -100,
            123456,
            "abcdef0123",
            "Test Gallery",
            true,
            STATUS_UPLOADED,
            Some(&zip_path_str),
            Some("https://telegra.ph/Test-01-01"),
        )
        .await;

        mock_tg_send_document(&tg_server).await;
        mock_tg_send_message(&tg_server).await;

        let eh_server = MockServer::start().await;
        let worker = EhPublishWorker::new(
            Arc::clone(&repo),
            make_notifier(&tg_server),
            make_eh_client(&eh_server),
            Arc::new(make_config()),
        );
        worker.tick().await.unwrap();

        let updated = eh_download_queue::Entity::find_by_id(entry.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(updated.status, "done");

        // Verify TG received both sendDocument and sendMessage
        let received = tg_server.received_requests().await.unwrap();
        assert!(received
            .iter()
            .any(|r| r.url.path().ends_with("/SendDocument")));
        assert!(received
            .iter()
            .any(|r| r.url.path().ends_with("/SendMessage")));
    }

    #[tokio::test]
    async fn test_publish_worker_chat_disabled_schedules_retry() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let tg_server = MockServer::start().await;

        setup_chat(&repo, -100, false).await; // disabled

        let temp = tempfile::tempdir().unwrap();
        let zip_path = temp.path().join("gallery.zip");
        create_test_zip(&zip_path, 2);
        let zip_path_str = zip_path.to_string_lossy().to_string();

        let entry = insert_queue_entry(
            &repo,
            -100,
            123456,
            "abcdef0123",
            "Test",
            false,
            STATUS_DOWNLOADED,
            Some(&zip_path_str),
            None,
        )
        .await;

        let eh_server = MockServer::start().await;
        let worker = EhPublishWorker::new(
            Arc::clone(&repo),
            make_notifier(&tg_server),
            make_eh_client(&eh_server),
            Arc::new(make_config()),
        );
        worker.tick().await.unwrap();

        let updated = eh_download_queue::Entity::find_by_id(entry.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        // Chat disabled → entry goes back to downloaded with retry (not silently done)
        assert_eq!(
            updated.status, STATUS_DOWNLOADED,
            "should be back to downloaded for retry"
        );
        assert_eq!(
            updated.retry_count, 0,
            "chat disabled defer should not increment retry_count"
        );
        assert!(
            updated.next_retry_at.is_some(),
            "should have next_retry_at set"
        );
    }

    #[tokio::test]
    async fn test_publish_retry_skips_archive_after_marker() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        setup_chat(&repo, -100, true).await;
        let tg_server = MockServer::start().await;
        let notifier = make_notifier(&tg_server);
        let config = Arc::new(make_config());
        let temp_dir = tempfile::tempdir().unwrap();
        let zip_path = temp_dir.path().join("501.zip");
        create_test_zip(&zip_path, 2);
        let entry = insert_queue_entry(
            &repo,
            -100,
            501,
            "tok",
            "Title",
            true,
            STATUS_UPLOADED,
            Some(zip_path.to_str().unwrap()),
            Some("https://telegra.ph/page"),
        )
        .await;

        // Pre-set archive_sent_at directly (bypassing the publishing guard for test setup)
        eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::ArchiveSentAt,
                Expr::value(Some(Local::now().naive_local())),
            )
            .filter(eh_download_queue::Column::Id.eq(entry.id))
            .exec(repo.db())
            .await
            .unwrap();

        // Only mock SendMessage (telegraph link); do NOT mock SendDocument (archive)
        mock_tg_send_message(&tg_server).await;

        let eh_server = MockServer::start().await;
        let worker = EhPublishWorker::new(
            Arc::clone(&repo),
            notifier,
            make_eh_client(&eh_server),
            config,
        );
        worker.tick().await.unwrap();

        let model = eh_download_queue::Entity::find_by_id(entry.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(model.status, STATUS_DONE);
        assert!(model.telegraph_sent_at.is_some());
    }

    #[tokio::test]
    async fn test_publish_missing_zip_retries_download_instead_of_done() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        setup_chat(&repo, -100, true).await;
        let tg_server = MockServer::start().await;
        let notifier = make_notifier(&tg_server);
        let config = Arc::new(make_config());
        let entry = insert_queue_entry(
            &repo,
            -100,
            502,
            "tok",
            "Title",
            false,
            STATUS_DOWNLOADED,
            Some("data/test_cache/missing.zip"),
            None,
        )
        .await;

        let eh_server = MockServer::start().await;
        let worker = EhPublishWorker::new(
            Arc::clone(&repo),
            notifier,
            make_eh_client(&eh_server),
            config,
        );
        worker.tick().await.unwrap();

        let model = eh_download_queue::Entity::find_by_id(entry.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(model.status, STATUS_PENDING);
        assert_eq!(model.retry_count, 1);
        assert!(model.next_retry_at.is_some());
    }

    #[tokio::test]
    async fn test_publish_skips_entry_canceled_after_claim() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        setup_chat(&repo, -100, true).await;
        let tg_server = MockServer::start().await;
        let notifier = make_notifier(&tg_server);
        let config = Arc::new(make_config());
        let temp_dir = tempfile::tempdir().unwrap();
        let zip_path = temp_dir.path().join("509.zip");
        create_test_zip(&zip_path, 2);
        let entry = insert_subscription_queue_entry(
            &repo,
            -100,
            "123",
            509,
            "tok",
            "Title",
            false,
            STATUS_DOWNLOADED,
            Some(zip_path.to_str().unwrap()),
            None,
        )
        .await;
        let claimed = repo.get_next_for_publish().await.unwrap().unwrap();
        assert_eq!(claimed.id, entry.id);
        repo.cancel_eh_subscription_queue_entries(123)
            .await
            .unwrap();

        let worker = EhPublishWorker::new(
            Arc::clone(&repo),
            notifier,
            make_eh_client(&MockServer::start().await),
            config,
        );
        worker.process(&claimed).await.unwrap();

        let model = eh_download_queue::Entity::find_by_id(entry.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(model.status, STATUS_CANCELED);
        assert!(model.archive_sent_at.is_none());
        assert!(zip_path.exists(), "canceled publish should not clean ZIP");
    }

    #[tokio::test]
    async fn test_publish_no_surface_fails_not_done() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        setup_chat(&repo, -100, true).await;
        let tg_server = MockServer::start().await;
        let notifier = make_notifier(&tg_server);
        let mut cfg = make_config();
        cfg.send_archive = false;
        cfg.max_retry_count = 0;
        let config = Arc::new(cfg);
        let worker = EhPublishWorker::new(
            Arc::clone(&repo),
            notifier,
            make_eh_client(&MockServer::start().await),
            config,
        );
        let temp_dir = tempfile::tempdir().unwrap();
        let zip_path = temp_dir.path().join("503.zip");
        create_test_zip(&zip_path, 2);
        let entry = insert_queue_entry(
            &repo,
            -100,
            503,
            "tok",
            "Title",
            false,
            STATUS_DOWNLOADED,
            Some(zip_path.to_str().unwrap()),
            None,
        )
        .await;

        worker.tick().await.unwrap();
        let model = eh_download_queue::Entity::find_by_id(entry.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(model.status, STATUS_FAILED);
        assert!(model.error.unwrap().contains("no EH publish surface"));
    }

    #[tokio::test]
    async fn test_chat_disabled_defer_does_not_increment_retry() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        setup_chat(&repo, -100, false).await; // disabled
        let tg_server = MockServer::start().await;
        let notifier = make_notifier(&tg_server);
        let config = Arc::new(make_config());
        let temp_dir = tempfile::tempdir().unwrap();
        let zip_path = temp_dir.path().join("504.zip");
        create_test_zip(&zip_path, 2);
        let entry = insert_queue_entry(
            &repo,
            -100,
            504,
            "tok",
            "Title",
            false,
            STATUS_DOWNLOADED,
            Some(zip_path.to_str().unwrap()),
            None,
        )
        .await;

        let eh_server = MockServer::start().await;
        let worker = EhPublishWorker::new(
            Arc::clone(&repo),
            notifier,
            make_eh_client(&eh_server),
            config,
        );
        worker.tick().await.unwrap();
        let model = eh_download_queue::Entity::find_by_id(entry.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(model.status, STATUS_DOWNLOADED);
        assert_eq!(model.retry_count, 0);
        assert!(model.next_retry_at.is_some());
    }

    #[tokio::test]
    async fn test_upload_permanent_failure_falls_back_to_archive() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        setup_chat(&repo, -100, true).await;
        let tg_server = MockServer::start().await;
        let notifier = make_notifier(&tg_server);
        let mut cfg = make_config();
        cfg.max_retry_count = 0;
        cfg.send_archive = true;
        let config = Arc::new(cfg);
        let temp_dir = tempfile::tempdir().unwrap();
        let zip_path = temp_dir.path().join("505.zip");
        create_test_zip(&zip_path, 2);
        let entry = insert_queue_entry(
            &repo,
            -100,
            505,
            "tok",
            "Title",
            true,
            STATUS_DOWNLOADED,
            Some(zip_path.to_str().unwrap()),
            None,
        )
        .await;

        let worker = EhUploadWorker::new(
            Arc::clone(&repo),
            notifier,
            make_telegraph_client(&tg_server),
            make_image_uploader(&tg_server),
            config,
        );
        worker.tick().await.unwrap();
        let model = eh_download_queue::Entity::find_by_id(entry.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(model.status, STATUS_DOWNLOADED);
        assert!(!model.telegraph);
        assert_eq!(model.retry_count, 0);
        assert!(model.next_retry_at.is_none());
        assert!(
            zip_path.exists(),
            "ZIP file should be preserved after archive fallback"
        );
    }

    #[tokio::test]
    async fn test_upload_permanent_failure_without_zip_does_not_fallback() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        setup_chat(&repo, -100, true).await;
        let tg_server = MockServer::start().await;
        let notifier = make_notifier(&tg_server);
        let mut cfg = make_config();
        cfg.max_retry_count = 0;
        cfg.send_archive = true;
        let config = Arc::new(cfg);
        let entry = insert_queue_entry(
            &repo,
            -100,
            506,
            "tok",
            "Title",
            true,
            STATUS_DOWNLOADED,
            Some("data/test_cache/missing_506.zip"),
            None,
        )
        .await;

        let worker = EhUploadWorker::new(
            Arc::clone(&repo),
            notifier,
            make_telegraph_client(&tg_server),
            make_image_uploader(&tg_server),
            config,
        );
        worker.tick().await.unwrap();
        let model = eh_download_queue::Entity::find_by_id(entry.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        // Missing ZIP file → no fallback; permanent failure path.
        assert_eq!(model.status, STATUS_FAILED);
        assert!(model.error.is_some(), "should have error set");
    }

    #[tokio::test]
    async fn test_upload_worker_chat_disabled_defer_does_not_increment_retry() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        setup_chat(&repo, -100, false).await; // disabled
        let tg_server = MockServer::start().await;
        let notifier = make_notifier(&tg_server);
        let config = Arc::new(make_config());
        let temp_dir = tempfile::tempdir().unwrap();
        let zip_path = temp_dir.path().join("507.zip");
        create_test_zip(&zip_path, 2);
        let entry = insert_queue_entry(
            &repo,
            -100,
            507,
            "tok",
            "Title",
            true,
            STATUS_DOWNLOADED,
            Some(zip_path.to_str().unwrap()),
            None,
        )
        .await;

        let worker = EhUploadWorker::new(
            Arc::clone(&repo),
            notifier,
            make_telegraph_client(&tg_server),
            make_image_uploader(&tg_server),
            config,
        );
        worker.tick().await.unwrap();
        let model = eh_download_queue::Entity::find_by_id(entry.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(model.status, STATUS_DOWNLOADED);
        assert_eq!(model.retry_count, 0);
        assert!(model.next_retry_at.is_some());
    }

    #[tokio::test]
    async fn test_publish_both_markers_already_set_skips_to_done() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        setup_chat(&repo, -100, true).await;
        // No mocks mounted on tg_server — any outbound request would hang/fail.
        let tg_server = MockServer::start().await;
        let notifier = make_notifier(&tg_server);
        let config = Arc::new(make_config());
        let temp_dir = tempfile::tempdir().unwrap();
        let zip_path = temp_dir.path().join("508.zip");
        create_test_zip(&zip_path, 2);
        let entry = insert_queue_entry(
            &repo,
            -100,
            508,
            "tok",
            "Title",
            true,
            STATUS_UPLOADED,
            Some(zip_path.to_str().unwrap()),
            Some("https://telegra.ph/508"),
        )
        .await;

        // Pre-set both markers to simulate a completed-but-not-done entry.
        eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::ArchiveSentAt,
                Expr::value(Some(Local::now().naive_local())),
            )
            .col_expr(
                eh_download_queue::Column::TelegraphSentAt,
                Expr::value(Some(Local::now().naive_local())),
            )
            .filter(eh_download_queue::Column::Id.eq(entry.id))
            .exec(repo.db())
            .await
            .unwrap();

        let eh_server = MockServer::start().await;
        let worker = EhPublishWorker::new(
            Arc::clone(&repo),
            notifier,
            make_eh_client(&eh_server),
            config,
        );
        worker.tick().await.unwrap();

        let model = eh_download_queue::Entity::find_by_id(entry.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(model.status, STATUS_DONE);
    }
}
