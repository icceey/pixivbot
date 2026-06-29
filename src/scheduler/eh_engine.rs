use crate::bot::notifier::Notifier;
use crate::config::EhentaiConfig;
use crate::db::entities::eh_download_queue;
use crate::db::repo::eh_download_queue::SOURCE_SUBSCRIPTION;
use crate::db::repo::Repo;
use crate::db::types::{
    EhFilter, EhPendingGallery, EhTagState, EhTaskKey, SubscriptionState, TaskType,
};
use crate::scheduler::helpers::{eh_tag_subscription_state, get_chat_if_should_notify};
use anyhow::{Context, Result};
use chrono::Local;
use eh_client::{EhClient, EhGallery, TelegraphClient};
use rand::RngExt;
use std::sync::Arc;
use tracing::{error, info, warn};

use crate::db::repo::eh_download_queue::{STATUS_DOWNLOADED, STATUS_PENDING, STATUS_UPLOADED};

/// Maximum search pages to fetch per tick (safety cap).
const MAX_FETCH_PAGES: u32 = 5;

/// Maximum metadata entries per api.php request.
const MAX_METADATA_BATCH: usize = 25;

/// Search rate limit: minimum delay between search requests (3s + buffer).
const SEARCH_RATE_LIMIT_MS: u64 = 3500;

// ============================================================================
// Stage 1: EhEngine — Collect (search → metadata → filter → enqueue downloads)
// ============================================================================

pub struct EhEngine {
    repo: Arc<Repo>,
    client: Arc<EhClient>,
    config: Arc<EhentaiConfig>,
    tick_interval_sec: u64,
}

impl EhEngine {
    pub fn new(
        repo: Arc<Repo>,
        client: Arc<EhClient>,
        config: Arc<EhentaiConfig>,
        tick_interval_sec: u64,
    ) -> Self {
        Self {
            repo,
            client,
            config,
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

        // Compute aggregate filter across subs
        let eh_filters: Vec<Option<&EhFilter>> =
            subs.iter().map(|s| s.eh_filter.as_ref()).collect();
        let agg_filter = EhFilter::aggregate(&eh_filters);

        // Determine the oldest latest_posted_ts across subs (cursor)
        let oldest_ts = subs
            .iter()
            .filter_map(|s| eh_tag_subscription_state(s).map(|st| st.latest_posted_ts))
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
            for sub in &subs {
                self.process_eh_sub(sub, &[]).await?;
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
            for sub in &subs {
                self.process_eh_sub(sub, &[]).await?;
            }
            self.schedule_next_poll(task.id).await;
            return Ok(());
        }

        // Process each subscription
        for sub in &subs {
            self.process_eh_sub(sub, &filtered).await?;
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

    /// Process a single subscription: filter galleries, enqueue downloads, update state.
    async fn process_eh_sub(
        &self,
        sub: &crate::db::entities::subscriptions::Model,
        galleries: &[EhGallery],
    ) -> Result<()> {
        let mut state = eh_tag_subscription_state(sub).unwrap_or_else(EhTagState::cleared);

        let sub_filter = sub.eh_filter.as_ref();
        let max_push = self.config.max_push_per_tick;
        let mut remaining_slots = max_push;

        // Telegraph gating: only set telegraph if upload token is configured.
        let telegraph_available = self.config.telegraph_access_token.is_some();
        let telegraph_default = telegraph_available
            && (self.config.upload_telegraph || sub_filter.map(|f| f.telegraph).unwrap_or(false));

        // Step 1: Consume pending backlog first (galleries from previous overflow).
        let mut still_pending = Vec::new();
        let backlog: Vec<_> = state.pending_galleries.drain(..).collect();
        let mut backlog_iter = backlog.into_iter();
        while let Some(pending) = backlog_iter.next() {
            if remaining_slots == 0 {
                still_pending.push(pending);
                continue;
            }
            if let Err(e) = self
                .repo
                .enqueue_eh_download(
                    sub.chat_id,
                    pending.gid as i64,
                    &pending.token,
                    &pending.title,
                    telegraph_default,
                    SOURCE_SUBSCRIPTION,
                )
                .await
            {
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
            state.add_pushed_gid(pending.gid);
            remaining_slots -= 1;
        }
        state.pending_galleries = still_pending;

        // If we still have pending backlog after consuming up to the cap, save state
        // and return — do NOT advance cursor until all pending are drained.
        if !state.pending_galleries.is_empty() {
            state.trim_pushed(self.config.pushed_cap);
            self.repo
                .update_subscription_latest_data(sub.id, Some(SubscriptionState::EhTag(state)))
                .await
                .context("Failed to update eh subscription state")?;
            return Ok(());
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
        while let Some(gallery) = eligible_iter.next() {
            if remaining_slots == 0 {
                // Overflow: store in pending backlog for next tick.
                state.pending_galleries.push(gallery);
                continue;
            }
            if let Err(e) = self
                .repo
                .enqueue_eh_download(
                    sub.chat_id,
                    gallery.gid as i64,
                    &gallery.token,
                    &gallery.title,
                    telegraph_default,
                    SOURCE_SUBSCRIPTION,
                )
                .await
            {
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
            state.add_pushed_gid(gallery.gid);
            state.latest_posted_ts = state.latest_posted_ts.max(gallery.posted);
            remaining_slots -= 1;
        }

        // Step 3: If no overflow, safely advance cursor past the entire batch.
        if state.pending_galleries.is_empty() {
            state.latest_posted_ts = state.latest_posted_ts.max(state.pending_high_water_ts);
            state.pending_high_water_ts = 0;
        }

        state.trim_pushed(self.config.pushed_cap);

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
                // Delete partial ZIP if it exists
                self.cleanup_zip(&entry).await;
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
            info!(
                "Skipping download for gid={} — chat {} not notifiable, will retry later",
                gid, entry.chat_id
            );
            // Don't mark done — schedule a retry with backoff so the gallery isn't lost.
            // The entry goes back to pending with a future next_retry_at.
            self.repo
                .schedule_eh_retry_from(
                    entry.id,
                    &entry.status,
                    STATUS_PENDING,
                    "chat not notifiable",
                    self.config.max_retry_count,
                )
                .await?;
            return Ok(());
        }

        // Ensure cache dir exists
        let eh_cache = self.cache_dir.join("eh_cache");
        tokio::fs::create_dir_all(&eh_cache).await?;
        let zip_path = eh_cache.join(format!("{}_{}.zip", gid, token));
        let zip_path_str = zip_path.to_string_lossy().to_string();

        // Download
        let file_size = if self.client.is_logged_in() {
            let archiver_key = self
                .client
                .get_archiver_key(gid, token)
                .await
                .context("Failed to get archiver key")?;

            let resolution = if entry.source == "direct" {
                &self.config.download_resolution
            } else {
                &self.config.subscription_resolution
            };

            self.client
                .download_archive(gid, token, &archiver_key, resolution, &zip_path)
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

    /// Delete the ZIP file for an entry if it exists (used on permanent failure).
    async fn cleanup_zip(&self, _entry: &eh_download_queue::Model) {
        // Download worker's ZIP path is constructed from gid+token, not stored yet on failure.
        // The ZIP may exist at the expected path if download started but failed mid-stream.
        let gid = _entry.gid as u64;
        let token = &_entry.token;
        let zip_path = self
            .cache_dir
            .join("eh_cache")
            .join(format!("{}_{}.zip", gid, token));
        if zip_path.exists() {
            if let Err(e) = tokio::fs::remove_file(&zip_path).await {
                warn!("Failed to delete partial zip {}: {}", zip_path.display(), e);
            }
        }
    }
}

// ============================================================================
// Stage 3: EhUploadWorker — Extract images from ZIP, upload to pixi.mg, create Telegraph page
// ============================================================================

pub struct EhUploadWorker {
    repo: Arc<Repo>,
    notifier: Notifier,
    telegraph: Arc<TelegraphClient>,
    config: Arc<EhentaiConfig>,
}

impl EhUploadWorker {
    pub fn new(
        repo: Arc<Repo>,
        notifier: Notifier,
        telegraph: Arc<TelegraphClient>,
        config: Arc<EhentaiConfig>,
    ) -> Self {
        Self {
            repo,
            notifier,
            telegraph,
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
        // Check chat is enabled before doing upload work (avoid wasting pixi.mg quota)
        let chat = get_chat_if_should_notify(&self.repo, entry.chat_id).await?;
        if chat.is_none() {
            anyhow::bail!("chat {} not notifiable", entry.chat_id);
        }

        let zip_path = entry
            .zip_path
            .as_ref()
            .context("zip_path is None for downloaded entry")?;
        let zip_path = std::path::Path::new(zip_path);

        // Extract images in spawn_blocking to avoid blocking async executor
        let zip_path_owned = zip_path.to_path_buf();
        let image_data_list: Vec<(String, Vec<u8>)> = tokio::task::spawn_blocking(move || {
            let zip_file = std::fs::File::open(&zip_path_owned).context("Failed to open zip")?;
            let mut archive =
                zip::ZipArchive::new(zip_file).context("Failed to read zip archive")?;

            let mut images = Vec::new();
            for i in 0..archive.len() {
                let mut file = archive.by_index(i).context("Failed to read zip entry")?;
                let name = file.name().to_lowercase();
                if !name.ends_with(".jpg")
                    && !name.ends_with(".jpeg")
                    && !name.ends_with(".png")
                    && !name.ends_with(".gif")
                    && !name.ends_with(".webp")
                {
                    continue;
                }

                let mut data = Vec::new();
                std::io::Read::read_to_end(&mut file, &mut data)
                    .context("Failed to read image from zip")?;

                if data.len() > 6 * 1024 * 1024 {
                    continue;
                }

                let filename = std::path::Path::new(file.name())
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("image.jpg")
                    .to_string();
                images.push((filename, data));
            }
            Ok::<_, anyhow::Error>(images)
        })
        .await
        .context("spawn_blocking failed")??;

        if image_data_list.is_empty() {
            anyhow::bail!("No images found in ZIP");
        }

        // Upload images to pixi.mg with 429 backoff (batch up to 5 at a time)
        let mut all_urls = Vec::new();
        for chunk in image_data_list.chunks(5) {
            let refs: Vec<&[u8]> = chunk.iter().map(|(_, d)| d.as_slice()).collect();
            let urls = self
                .telegraph
                .upload_images_with_retry(&refs, 3)
                .await
                .context("Failed to upload images to pixi.mg")?;
            all_urls.extend(urls);
        }

        if all_urls.is_empty() {
            anyhow::bail!("No images uploaded to pixi.mg");
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
            // Retry: go back to the pre-publish status
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
            // Chat disabled — return error to trigger retry path.
            // The gallery was already downloaded; don't silently mark done and lose it.
            anyhow::bail!("chat {} not notifiable", entry.chat_id);
        }
        let chat_id = teloxide::types::ChatId(entry.chat_id);

        // Send archive if configured
        if self.config.send_archive {
            if let Some(zip_path_str) = &entry.zip_path {
                let zip_path = std::path::Path::new(zip_path_str);
                if zip_path.exists() {
                    let caption = self.build_caption(entry);
                    let filename = format!("{}.zip", sanitize_filename(&entry.title));
                    self.notifier
                        .send_document(chat_id, zip_path, &filename, &caption)
                        .await
                        .context("Failed to send archive document")?;
                }
            }
        }

        // Send Telegraph link if available
        if let Some(ref telegraph_url) = entry.telegraph_url {
            let link_text = format!(
                "📄 [Telegraph 链接]({})",
                teloxide::utils::markdown::escape_link_url(telegraph_url)
            );
            self.notifier
                .send_text(chat_id, &link_text, false)
                .await
                .context("Failed to send telegraph link")?;
        }

        // Mark done and clean up ZIP
        self.cleanup_zip(entry).await;
        self.repo
            .mark_eh_download_done(entry.id, entry.file_size)
            .await?;
        info!(
            "Published eh gallery gid={} to chat {}",
            entry.gid, entry.chat_id
        );
        Ok(())
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
    use crate::db::repo::eh_download_queue::{SOURCE_DIRECT, STATUS_DOWNLOADED, STATUS_UPLOADED};
    use crate::db::repo::tests_helpers;
    use crate::pixiv::downloader::Downloader;
    use eh_client::{EhClientBuilder, EhCookies, TelegraphClient};
    use reqwest::Client;
    use sea_orm::{ActiveModelTrait, EntityTrait, Set};
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

    async fn mock_telegraph_upload(server: &MockServer) {
        let body =
            serde_json::json!({"success": true, "direct_url": "https://i.pixi.mg/i/abc123.jpg"});
        Mock::given(method("POST"))
            .and(path("/pixi/upload"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
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

        mock_telegraph_upload(&tg_server).await;
        mock_telegraph_create_page(&tg_server).await;

        let worker = EhUploadWorker::new(
            Arc::clone(&repo),
            make_notifier(&tg_server),
            make_telegraph_client(&tg_server),
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
        assert!(
            updated.next_retry_at.is_some(),
            "should have next_retry_at set"
        );
    }
}
