use crate::bot::notifier::Notifier;
use crate::config::EhentaiConfig;
use crate::db::entities::subscriptions;
use crate::db::repo::eh_download_queue::SOURCE_SUBSCRIPTION;
use crate::db::repo::Repo;
use crate::db::types::{
    EhFilter, EhTagState, EhTaskKey, QueuedEhGallery, SubscriptionState, TaskType,
};
use crate::scheduler::helpers::{eh_tag_subscription_state, get_chat_if_should_notify};
use anyhow::{Context, Result};
use chrono::Local;
use eh_client::{EhClient, EhGallery, TelegraphClient};
use rand::RngExt;
use std::sync::Arc;
use tracing::{error, info, warn};

/// Interval for drain polls (when pending queue is non-empty).
const DRAIN_POLL_INTERVAL_SEC: u64 = 10;

/// Maximum search pages to fetch per tick (safety cap).
const MAX_FETCH_PAGES: u32 = 5;

/// Maximum metadata entries per api.php request.
const MAX_METADATA_BATCH: usize = 25;

/// Search rate limit: minimum delay between search requests (3s + buffer).
const SEARCH_RATE_LIMIT_MS: u64 = 3500;

pub struct EhEngine {
    repo: Arc<Repo>,
    client: Arc<EhClient>,
    config: Arc<EhentaiConfig>,
    tick_interval_sec: u64,
    max_retry_count: i32,
}

impl EhEngine {
    pub fn new(
        repo: Arc<Repo>,
        _notifier: Notifier,
        client: Arc<EhClient>,
        _telegraph: Option<Arc<TelegraphClient>>,
        config: Arc<EhentaiConfig>,
        tick_interval_sec: u64,
        max_retry_count: i32,
    ) -> Self {
        Self {
            repo,
            client,
            config,
            tick_interval_sec,
            max_retry_count,
        }
    }

    pub async fn run(self) {
        // On startup, reset any stale "downloading" entries
        if let Err(e) = self.repo.reset_stale_eh_downloads().await {
            warn!("Failed to reset stale eh downloads on startup: {:#}", e);
        }

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
        // Fetch one pending Ehentai task
        let tasks = self
            .repo
            .get_pending_tasks_by_type(TaskType::Ehentai, 1)
            .await
            .context("Failed to fetch pending eh tasks")?;

        if let Some(task) = tasks.into_iter().next() {
            if let Err(e) = self.execute_eh_task(&task).await {
                error!("Failed to execute eh task {}: {:#}", task.id, e);
                // Backoff 1 hour on error
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

        // List subscriptions on this task
        let subs = self
            .repo
            .list_subscriptions_by_task(task.id)
            .await
            .context("Failed to list eh subscriptions")?;

        if subs.is_empty() {
            // No subscriptions, schedule next poll
            self.schedule_next_poll(task.id).await;
            return Ok(());
        }

        // Check if all subs have pending queues (skip API fetch if so)
        let all_have_pending = subs.iter().all(|sub| {
            eh_tag_subscription_state(sub)
                .map(|s| !s.pending_queue.is_empty())
                .unwrap_or(false)
        });

        if all_have_pending {
            // Drain pending queues first
            for sub in &subs {
                if let Err(e) = self.drain_pending_queue(sub).await {
                    warn!("Failed to drain pending queue for sub {}: {:#}", sub.id, e);
                }
            }
            self.schedule_drain_poll(task.id).await;
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

        // Fetch galleries
        let galleries = if agg_filter.has_rating_filter() {
            // 48h scan mode: fetch all galleries within scan window
            self.fetch_galleries_48h(&key.query, key.category_bitmask, oldest_ts)
                .await?
        } else {
            // Normal mode: fetch newest galleries since last poll
            self.fetch_galleries_since(&key.query, key.category_bitmask, oldest_ts)
                .await?
        };

        if galleries.is_empty() {
            // No new galleries, schedule next poll
            for sub in &subs {
                self.update_sub_state_no_new(sub, oldest_ts).await;
            }
            self.schedule_next_poll(task.id).await;
            return Ok(());
        }

        // Batch fetch full metadata
        let gidlist: Vec<(u64, &str)> = galleries
            .iter()
            .map(|g| (g.gid, g.token.as_str()))
            .collect();

        let mut all_metadata = Vec::new();
        for chunk in gidlist.chunks(MAX_METADATA_BATCH) {
            let metadata = self
                .client
                .get_metadata(chunk)
                .await
                .context("Failed to fetch gallery metadata")?;
            all_metadata.extend(metadata);

            // Rate limit between metadata requests
            if chunk.len() == MAX_METADATA_BATCH {
                tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
            }
        }

        // Filter by aggregate filter
        let filtered: Vec<EhGallery> = all_metadata
            .into_iter()
            .filter(|g| agg_filter.matches(g))
            .collect();

        if filtered.is_empty() {
            for sub in &subs {
                self.update_sub_state_no_new(sub, oldest_ts).await;
            }
            self.schedule_next_poll(task.id).await;
            return Ok(());
        }

        // Process each subscription
        let mut any_has_pending = false;
        for sub in &subs {
            let result = self.process_eh_sub(sub, &filtered).await;
            if let Err(e) = &result {
                warn!("Failed to process eh sub {}: {:#}", sub.id, e);
            }

            // Check if sub now has pending queue
            if let Some(state) = eh_tag_subscription_state(sub) {
                if !state.pending_queue.is_empty() {
                    any_has_pending = true;
                }
            }
        }

        if any_has_pending {
            self.schedule_drain_poll(task.id).await;
        } else {
            self.schedule_next_poll(task.id).await;
        }

        Ok(())
    }

    /// Normal mode: fetch galleries newer than oldest_ts.
    async fn fetch_galleries_since(
        &self,
        query: &str,
        cats: u32,
        oldest_ts: i64,
    ) -> Result<Vec<eh_client::EhGalleryRef>> {
        let mut all_refs = Vec::new();

        for page in 0..MAX_FETCH_PAGES {
            tokio::time::sleep(tokio::time::Duration::from_millis(SEARCH_RATE_LIMIT_MS)).await;

            let refs = self
                .client
                .search(query, cats, page)
                .await
                .context("Failed to search eh galleries")?;

            if refs.is_empty() {
                break;
            }

            // Stop if we've gone past the cursor
            let all_old = refs.iter().all(|r| r.posted_ts <= oldest_ts);
            all_refs.extend(refs);
            if all_old {
                break;
            }
        }

        // Filter to only newer galleries
        let new_refs: Vec<eh_client::EhGalleryRef> = all_refs
            .into_iter()
            .filter(|r| r.posted_ts > oldest_ts)
            .collect();

        Ok(new_refs)
    }

    /// 48h scan mode: fetch all galleries within scan_window_hours.
    async fn fetch_galleries_48h(
        &self,
        query: &str,
        cats: u32,
        oldest_ts: i64,
    ) -> Result<Vec<eh_client::EhGalleryRef>> {
        let cutoff_ts = Local::now().timestamp() - (self.config.scan_window_hours as i64 * 3600);
        let effective_cutoff = cutoff_ts.max(oldest_ts);

        let mut all_refs = Vec::new();

        for page in 0..MAX_FETCH_PAGES {
            tokio::time::sleep(tokio::time::Duration::from_millis(SEARCH_RATE_LIMIT_MS)).await;

            let refs = self
                .client
                .search(query, cats, page)
                .await
                .context("Failed to search eh galleries (48h scan)")?;

            if refs.is_empty() {
                break;
            }

            let all_old = refs.iter().all(|r| r.posted_ts < effective_cutoff);
            all_refs.extend(refs);
            if all_old {
                break;
            }
        }

        // Filter to galleries within scan window
        let filtered: Vec<eh_client::EhGalleryRef> = all_refs
            .into_iter()
            .filter(|r| r.posted_ts >= effective_cutoff)
            .collect();

        Ok(filtered)
    }

    /// Process a single subscription: filter galleries, enqueue downloads, update state.
    async fn process_eh_sub(
        &self,
        sub: &subscriptions::Model,
        galleries: &[EhGallery],
    ) -> Result<()> {
        let mut state = eh_tag_subscription_state(sub).unwrap_or_else(|| {
            // First run: initialize with empty state
            EhTagState::cleared(0)
        });

        let sub_filter = sub.eh_filter.as_ref();
        let max_push = self.config.max_push_per_tick;

        // Filter galleries not already pushed and matching sub filter
        let new_galleries: Vec<&EhGallery> = galleries
            .iter()
            .filter(|g| !state.pushed_gids.contains(&g.gid))
            .filter(|g| sub_filter.map(|f| f.matches(g)).unwrap_or(true))
            .collect();

        // Enqueue downloads for new galleries (up to max_push per tick)
        let to_enqueue: Vec<&EhGallery> = if state.pending_queue.is_empty() {
            new_galleries.into_iter().take(max_push).collect()
        } else {
            // Already have pending items, don't add more this tick
            Vec::new()
        };

        // Enqueue download requests for each gallery
        for gallery in &to_enqueue {
            let telegraph = sub_filter.map(|f| f.telegraph).unwrap_or(false);

            if let Err(e) = self
                .repo
                .enqueue_eh_download(
                    sub.chat_id,
                    gallery.gid as i64,
                    &gallery.token,
                    &gallery.title,
                    telegraph,
                    SOURCE_SUBSCRIPTION,
                )
                .await
            {
                warn!(
                    "Failed to enqueue download for gallery {}: {:#}",
                    gallery.gid, e
                );
                continue;
            }

            // Add to pushed set
            state.add_pushed_gid(gallery.gid);

            // Update latest_posted_ts
            if gallery.posted > state.latest_posted_ts {
                state.latest_posted_ts = gallery.posted;
            }

            // Also add to pending_queue for state tracking
            state.pending_queue.push(gallery_to_queued(gallery));
        }

        // Trim pushed_gids to cap
        state.trim_pushed(self.config.pushed_cap);

        // Persist state
        self.repo
            .update_subscription_latest_data(sub.id, Some(SubscriptionState::EhTag(state)))
            .await
            .context("Failed to update eh subscription state")?;

        // Drain pending queue (send what we can)
        self.drain_pending_queue(sub).await?;

        Ok(())
    }

    /// Drain pending queue: send one gallery's download per call.
    async fn drain_pending_queue(&self, sub: &subscriptions::Model) -> Result<()> {
        let state = match eh_tag_subscription_state(sub) {
            Some(s) if !s.pending_queue.is_empty() => s,
            _ => return Ok(()),
        };

        if state.should_abandon_queue(self.max_retry_count) {
            warn!(
                "Abandoning eh pending queue for sub {} after {} retries",
                sub.id, state.retry_count
            );
            let new_state = EhTagState::cleared(state.latest_posted_ts);
            self.repo
                .update_subscription_latest_data(sub.id, Some(SubscriptionState::EhTag(new_state)))
                .await?;
            return Ok(());
        }

        // The actual download is handled by EhDownloadProcessor via the queue.
        // Here we just pop from the pending_queue since the download was already enqueued.
        let new_state = state.popped_front();
        self.repo
            .update_subscription_latest_data(sub.id, Some(SubscriptionState::EhTag(new_state)))
            .await?;

        Ok(())
    }

    /// Update state when no new galleries were found.
    async fn update_sub_state_no_new(&self, sub: &subscriptions::Model, latest_ts: i64) {
        let state =
            eh_tag_subscription_state(sub).unwrap_or_else(|| EhTagState::cleared(latest_ts));
        // Only update if there's a meaningful change
        if state.pending_queue.is_empty() && state.latest_posted_ts == latest_ts {
            return;
        }
        let new_state = EhTagState {
            pushed_gids: state.pushed_gids,
            latest_posted_ts: if latest_ts > 0 {
                latest_ts
            } else {
                state.latest_posted_ts
            },
            pending_queue: state.pending_queue,
            retry_count: state.retry_count,
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

    async fn schedule_drain_poll(&self, task_id: i32) {
        let next = Local::now() + chrono::Duration::seconds(DRAIN_POLL_INTERVAL_SEC as i64);
        if let Err(e) = self.repo.update_task_after_poll(task_id, next).await {
            error!("Failed to schedule eh drain poll: {:#}", e);
        }
    }
}

/// Convert EhGallery to QueuedEhGallery for pending queue storage.
fn gallery_to_queued(g: &EhGallery) -> QueuedEhGallery {
    QueuedEhGallery {
        gid: g.gid,
        token: g.token.clone(),
        title: g.title.clone(),
        title_jpn: g.title_jpn.clone(),
        category: g.category.clone(),
        thumb: g.thumb.clone(),
        uploader: g.uploader.clone(),
        posted: g.posted,
        filecount: g.filecount,
        filesize: g.filesize,
        rating: g.rating,
        tags: g.tags.clone(),
    }
}

/// EhDownloadProcessor: drains the download queue with rate-limit enforcement.
///
/// This runs as a separate task from EhEngine. It fetches pending download
/// requests, downloads archives, sends them to Telegram chats, and optionally
/// creates Telegraph pages.
pub struct EhDownloadProcessor {
    repo: Arc<Repo>,
    notifier: Notifier,
    client: Arc<EhClient>,
    telegraph: Option<Arc<TelegraphClient>>,
    config: Arc<EhentaiConfig>,
}

impl EhDownloadProcessor {
    pub fn new(
        repo: Arc<Repo>,
        notifier: Notifier,
        client: Arc<EhClient>,
        telegraph: Option<Arc<TelegraphClient>>,
        config: Arc<EhentaiConfig>,
    ) -> Self {
        Self {
            repo,
            notifier,
            client,
            telegraph,
            config,
        }
    }

    pub async fn run(self) {
        let poll_interval = self.config.download_poll_interval_sec.max(10);
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(poll_interval));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            interval.tick().await;
            if let Err(e) = self.tick().await {
                error!("EhDownloadProcessor tick error: {:#}", e);
            }
        }
    }

    async fn tick(&self) -> Result<()> {
        // Check rate limit
        let downloaded_bytes = self
            .repo
            .get_eh_downloaded_bytes_in_window(self.config.download_rate_window_hours)
            .await
            .context("Failed to check download rate limit")?;

        if downloaded_bytes >= self.config.download_rate_limit_bytes() as i64 {
            info!("EH download rate limit reached, skipping this tick");
            return Ok(());
        }

        // Get next pending download
        let entry = self
            .repo
            .get_next_pending_eh_download()
            .await
            .context("Failed to get next pending eh download")?;

        let Some(entry) = entry else {
            return Ok(());
        };

        if let Err(e) = self.process_download(&entry).await {
            error!("Failed to process eh download {}: {:#}", entry.id, e);
            // Mark as failed
            if let Err(e2) = self
                .repo
                .mark_eh_download_failed(entry.id, &e.to_string())
                .await
            {
                error!("Failed to mark download as failed: {:#}", e2);
            }
        }

        Ok(())
    }

    async fn process_download(
        &self,
        entry: &crate::db::entities::eh_download_queue::Model,
    ) -> Result<()> {
        let gid = entry.gid as u64;
        let token = &entry.token;

        // Get archiver key from gallery page
        let archiver_key = self
            .client
            .get_archiver_key(gid, token)
            .await
            .context("Failed to get archiver key")?;

        // Download archive to temp file
        let temp_dir = tempfile::tempdir().context("Failed to create temp dir")?;
        let zip_path = temp_dir.path().join(format!("{}.zip", gid));

        let file_size = self
            .client
            .download_archive(gid, token, &archiver_key, &zip_path)
            .await
            .context("Failed to download archive")?;

        info!("Downloaded eh archive gid={} size={} bytes", gid, file_size);

        // Send to Telegram chat
        let caption = self.build_download_caption(entry);
        let filename = format!("{}.zip", sanitize_filename(&entry.title));

        let chat = get_chat_if_should_notify(&self.repo, entry.chat_id).await?;
        let chat_id = teloxide::types::ChatId(entry.chat_id);

        if let Some(_chat) = &chat {
            match self
                .notifier
                .send_document(chat_id, &zip_path, &filename, &caption)
                .await
            {
                Ok(msg_id) => {
                    info!(
                        "Sent eh archive to chat {} msg_id={}",
                        entry.chat_id, msg_id
                    );
                }
                Err(e) => {
                    warn!(
                        "Failed to send eh archive to chat {}: {:#}",
                        entry.chat_id, e
                    );
                    // Don't fail the whole download — mark as done but log the send failure
                }
            }
        }

        // Optional Telegraph upload
        if entry.telegraph {
            if let Err(e) = self.process_telegraph(entry, &zip_path).await {
                warn!("Telegraph upload failed for gid={}: {:#}", gid, e);
                // Send error message to chat
                if chat.is_some() {
                    let escaped_err = teloxide::utils::markdown::escape(&e.to_string());
                    let _ = self
                        .notifier
                        .send_text(
                            chat_id,
                            &format!("⚠️ Telegraph 上传失败: {}", escaped_err),
                            false,
                        )
                        .await;
                }
            }
        }

        // Mark as done
        self.repo
            .mark_eh_download_done(entry.id, file_size as i64)
            .await
            .context("Failed to mark download as done")?;

        // Clean up temp dir
        let _ = temp_dir.close();

        Ok(())
    }

    async fn process_telegraph(
        &self,
        entry: &crate::db::entities::eh_download_queue::Model,
        zip_path: &std::path::Path,
    ) -> Result<()> {
        let telegraph = self
            .telegraph
            .as_ref()
            .context("Telegraph client not configured")?;

        // Extract images from ZIP
        let zip_file = std::fs::File::open(zip_path).context("Failed to open zip")?;
        let mut archive = zip::ZipArchive::new(zip_file).context("Failed to read zip archive")?;

        let mut image_urls = Vec::new();

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

            // Skip files larger than 6MB (Telegraph limit)
            if data.len() > 6 * 1024 * 1024 {
                warn!(
                    "Skipping image {} (too large: {} bytes)",
                    file.name(),
                    data.len()
                );
                continue;
            }

            let filename = std::path::Path::new(file.name())
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("image.jpg")
                .to_string();

            match telegraph.upload_image(&data, &filename).await {
                Ok(url) => {
                    image_urls.push(url);
                }
                Err(e) => {
                    warn!(
                        "Failed to upload image {} to telegraph: {:#}",
                        file.name(),
                        e
                    );
                }
            }
        }

        if image_urls.is_empty() {
            anyhow::bail!("No images uploaded to Telegraph");
        }

        // Create gallery page
        let title = if entry.title.is_empty() {
            "Gallery"
        } else {
            &entry.title
        };

        let page_url = telegraph
            .create_gallery_page(title, &image_urls)
            .await
            .context("Failed to create telegraph page")?;

        // Send link to chat
        let link_text = format!(
            "📄 [Telegraph 链接]({})",
            teloxide::utils::markdown::escape_link_url(&page_url)
        );

        self.notifier
            .send_text(teloxide::types::ChatId(entry.chat_id), &link_text, false)
            .await
            .context("Failed to send telegraph link")?;

        info!("Created telegraph page for gid={}: {}", entry.gid, page_url);

        Ok(())
    }

    fn build_download_caption(
        &self,
        entry: &crate::db::entities::eh_download_queue::Model,
    ) -> String {
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
    fn test_gallery_to_queued() {
        let gallery = EhGallery {
            gid: 123,
            token: "abc".to_string(),
            title: "Test".to_string(),
            title_jpn: Some("テスト".to_string()),
            category: "Manga".to_string(),
            thumb: "thumb.jpg".to_string(),
            uploader: "user".to_string(),
            posted: 1000,
            filecount: 20,
            filesize: 1000,
            expunged: false,
            rating: 4.5,
            tags: vec!["tag1".to_string()],
        };

        let queued = gallery_to_queued(&gallery);
        assert_eq!(queued.gid, 123);
        assert_eq!(queued.token, "abc");
        assert_eq!(queued.title, "Test");
        assert_eq!(queued.title_jpn, Some("テスト".to_string()));
        assert_eq!(queued.category, "Manga");
        assert_eq!(queued.filecount, 20);
        assert!((queued.rating - 4.5).abs() < 0.001);
    }
}
