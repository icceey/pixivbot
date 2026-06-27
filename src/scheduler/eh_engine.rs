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
        client: Arc<EhClient>,
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

        // Fetch gallery refs from search HTML (posted_ts is 0 from parser —
        // we'll fetch metadata to get real timestamps before filtering).
        let refs = if agg_filter.has_rating_filter() {
            self.fetch_galleries_48h(&key.query, key.category_bitmask, oldest_ts)
                .await?
        } else {
            self.fetch_galleries_since(&key.query, key.category_bitmask, oldest_ts)
                .await?
        };

        if refs.is_empty() {
            for sub in &subs {
                self.update_sub_state_no_new(sub, oldest_ts).await;
            }
            self.schedule_next_poll(task.id).await;
            return Ok(());
        }

        // Batch fetch full metadata (this gives us the real `posted` timestamp)
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

        // Now filter by real posted timestamp + aggregate filter
        let now_ts = Local::now().timestamp();
        let scan_cutoff = now_ts - (self.config.scan_window_hours as i64 * 3600);

        let filtered: Vec<EhGallery> = all_metadata
            .into_iter()
            .filter(|g| {
                // Timestamp cursor: on first run (oldest_ts==0), include all.
                // Normal mode: only newer than oldest_ts.
                if oldest_ts > 0 && g.posted <= oldest_ts {
                    return false;
                }
                // 48h scan mode: also enforce scan window cutoff.
                if agg_filter.has_rating_filter() && g.posted < scan_cutoff.max(oldest_ts) {
                    return false;
                }
                true
            })
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

    /// Fetch gallery refs from search. Returns all refs found (up to MAX_FETCH_PAGES).
    /// Timestamp filtering is done later using metadata API's `posted` field.
    async fn fetch_galleries_since(
        &self,
        query: &str,
        cats: u32,
        _oldest_ts: i64,
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

            all_refs.extend(refs);
        }

        Ok(all_refs)
    }

    /// 48h scan mode: fetch gallery refs (same as normal mode — timestamp
    /// filtering against the scan window is done after metadata fetch).
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

    /// Drain pending queue: pop one gallery's entry from the pending queue.
    /// Re-reads subscription state from DB to get the latest state.
    async fn drain_pending_queue(&self, sub: &subscriptions::Model) -> Result<()> {
        // Re-read subscription from DB to get the latest state (may have been just saved by process_eh_sub)
        let fresh_sub = self
            .repo
            .list_subscriptions_by_task(sub.task_id)
            .await
            .context("Failed to re-read subscriptions")?
            .into_iter()
            .find(|s| s.id == sub.id)
            .unwrap_or_else(|| sub.clone());

        let state = match eh_tag_subscription_state(&fresh_sub) {
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
        // On startup, reset stale and retry eligible failed downloads
        if let Err(e) = self.repo.reset_stale_eh_downloads().await {
            warn!("Failed to reset stale eh downloads: {:#}", e);
        }
        if let Err(e) = self
            .repo
            .retry_failed_eh_downloads(self.config.max_retry_count)
            .await
        {
            warn!("Failed to retry failed eh downloads: {:#}", e);
        }

        let poll_interval = self.config.download_poll_interval_sec.max(10);
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(poll_interval));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            interval.tick().await;
            // Periodically retry failed downloads
            if let Err(e) = self
                .repo
                .retry_failed_eh_downloads(self.config.max_retry_count)
                .await
            {
                warn!("Failed to retry failed eh downloads: {:#}", e);
            }
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

        // Download archive to temp file
        let temp_dir = tempfile::tempdir().context("Failed to create temp dir")?;
        let zip_path = temp_dir.path().join(format!("{}.zip", gid));

        // Choose download method: archive download requires login.
        // If not logged in, fall back to direct image download (scrape image pages).
        let file_size = if self.client.is_logged_in() {
            // Archive download via archiver.php
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
            // Direct image download (scrape gallery pages → download each image → ZIP)
            info!("Not logged in, using direct image download for gid={}", gid);
            self.client
                .download_gallery_images(gid, token, &zip_path)
                .await
                .context("Failed to download gallery images")?
        };

        info!("Downloaded eh gallery gid={} size={} bytes", gid, file_size);

        // Send to Telegram chat
        let caption = self.build_download_caption(entry);
        let filename = format!("{}.zip", sanitize_filename(&entry.title));

        let chat = get_chat_if_should_notify(&self.repo, entry.chat_id).await?;
        let chat_id = teloxide::types::ChatId(entry.chat_id);

        // Conditionally send the archive ZIP based on config
        if self.config.send_archive {
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
                    }
                }
            }
        } else if chat.is_some() {
            // send_archive is off — send just the caption as a text message
            let _ = self.notifier.send_text(chat_id, &caption, false).await;
        }

        // Optional Telegraph upload (config-level for subscriptions, entry-level for direct)
        let do_telegraph = if entry.source == "direct" {
            entry.telegraph
        } else {
            self.config.upload_telegraph
        };

        if do_telegraph {
            if let Err(e) = self.process_telegraph(entry, &zip_path).await {
                warn!("Telegraph upload failed for gid={}: {:#}", gid, e);
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

        // Extract images from ZIP in a blocking task to avoid blocking async executor
        let zip_path = zip_path.to_path_buf();
        let image_data_list: Vec<(String, Vec<u8>)> = tokio::task::spawn_blocking(move || {
            let zip_file = std::fs::File::open(&zip_path).context("Failed to open zip")?;
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

        // Upload images to Telegraph (async)
        let mut image_urls = Vec::new();
        for (filename, data) in image_data_list {
            match telegraph.upload_image(&data, &filename).await {
                Ok(url) => image_urls.push(url),
                Err(e) => warn!("Failed to upload image to telegraph: {:#}", e),
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

#[cfg(test)]
mod integration_tests {
    use super::*;
    use crate::config::EhentaiConfig;
    use crate::db::repo::tests_helpers;
    use crate::db::types::{
        EhFilter, EhTagState, EhTaskKey, SubscriptionState, TagFilter, TaskType,
    };
    use eh_client::EhClientBuilder;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Build a test EhEngine pointing at the given mock server.
    fn make_engine(
        repo: Arc<Repo>,
        server: &MockServer,
        config_overrides: Option<EhentaiConfig>,
    ) -> EhEngine {
        let client = Arc::new(
            EhClientBuilder::new()
                .base_url(&server.uri())
                .api_url(&format!("{}/api.php", server.uri()))
                .build(),
        );
        let mut config = EhentaiConfig::default();
        if let Some(c) = config_overrides {
            config = c;
        }
        // Use short intervals for tests
        config.min_interval_sec = 1;
        config.max_interval_sec = 2;
        config.max_push_per_tick = 3;
        config.max_retry_count = 3;
        config.scan_window_hours = 48;
        config.pushed_cap = 100;

        EhEngine::new(repo, client, Arc::new(config), 30, 3)
    }

    fn search_html(gid: u64, token: &str, title: &str) -> String {
        format!(
            r#"<div class="gl1t">
  <a href="https://e-hentai.org/g/{}/{}">
    <img src="https://ehgt.org/t/{}.jpg" />
  </a>
  <div class="gl3t"><div class="glink">{}</div></div>
</div>"#,
            gid, token, gid, title
        )
    }

    /// Token must be hex chars only (parser regex requirement)
    fn hex_token(i: u64) -> String {
        format!("abc{:04x}", i)
    }

    fn metadata_json(
        gid: u64,
        token: &str,
        title: &str,
        posted: i64,
        rating: &str,
        filecount: u32,
    ) -> serde_json::Value {
        serde_json::json!({
            "gmetadata": [{
                "gid": gid,
                "token": token,
                "title": title,
                "title_jpn": null,
                "category": "Doujinshi",
                "thumb": "https://ehgt.org/t/thumb.jpg",
                "uploader": "testuser",
                "posted": posted.to_string(),
                "filecount": filecount.to_string(),
                "filesize": 51210504,
                "expunged": false,
                "rating": rating,
                "tags": ["parody:test", "artist:test"]
            }]
        })
    }

    /// Helper: set up DB with a chat, an Ehentai task, and a subscription.
    async fn setup_subscription(repo: &Repo, filter: Option<EhFilter>) -> (i64, i32, i32) {
        let chat_id: i64 = -100;
        repo.upsert_chat(chat_id, "private".into(), None, true, Default::default())
            .await
            .unwrap();

        let eh_filter = EhFilter {
            min_rating: None,
            min_pages: None,
            max_pages: None,
            telegraph: false,
        };
        let key = EhTaskKey::new("female:elf", 0, &eh_filter);
        let task_value = key.to_task_value();

        let task = repo
            .get_or_create_task(TaskType::Ehentai, task_value, None)
            .await
            .unwrap();

        let sub = repo
            .upsert_eh_subscription(chat_id, task.id, TagFilter::default(), filter)
            .await
            .unwrap();

        (chat_id, task.id, sub.id)
    }

    #[tokio::test]
    async fn test_tick_no_pending_tasks() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let server = MockServer::start().await;
        let engine = make_engine(Arc::clone(&repo), &server, None);

        // No tasks → should return Ok
        engine.tick().await.unwrap();
        // Verify no mock was hit
    }

    #[tokio::test]
    async fn test_tick_no_subscriptions() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let server = MockServer::start().await;

        // Create task but no subscription
        let key = EhTaskKey::new("test", 0, &EhFilter::default());
        repo.get_or_create_task(TaskType::Ehentai, key.to_task_value(), None)
            .await
            .unwrap();

        let engine = make_engine(Arc::clone(&repo), &server, None);

        // Should schedule next poll and return without hitting the API
        engine.tick().await.unwrap();
    }

    #[tokio::test]
    async fn test_tick_finds_new_gallery_and_enqueues_download() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let server = MockServer::start().await;

        let (chat_id, task_id, _sub_id) = setup_subscription(&repo, None).await;

        // Mock search: return one gallery
        let search_html = search_html(123456, "abcdef0123", "Test Gallery");
        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_string(search_html))
            .mount(&server)
            .await;

        // Mock metadata API
        let meta = metadata_json(123456, "abcdef0123", "Test Gallery", 1700000000, "4.5", 20);
        Mock::given(method("POST"))
            .and(path("/api.php"))
            .respond_with(ResponseTemplate::new(200).set_body_json(meta))
            .mount(&server)
            .await;

        let engine = make_engine(Arc::clone(&repo), &server, None);

        // Set task to be pending (get_or_create_task sets next_poll to now+60s, make it past)
        let now = Local::now() - chrono::Duration::seconds(60);
        repo.update_task_after_poll(task_id, now).await.unwrap();

        engine.tick().await.unwrap();

        // Verify download was enqueued
        // Note: get_next_pending_eh_download marks it as DOWNLOADING, so we check via that
        let pending = repo.get_next_pending_eh_download().await.unwrap();
        assert!(pending.is_some(), "download should have been enqueued");
        let pending = pending.unwrap();
        assert_eq!(pending.gid, 123456);
        assert_eq!(pending.chat_id, chat_id);
        assert_eq!(pending.title, "Test Gallery");

        // Verify subscription state was updated
        let subs = repo.list_subscriptions_by_task(task_id).await.unwrap();
        assert_eq!(subs.len(), 1);
        let state = super::eh_tag_subscription_state(&subs[0]);
        assert!(state.is_some(), "state should be set");
        let state = state.unwrap();
        assert!(
            state.pushed_gids.contains(&123456),
            "gid should be in pushed_gids"
        );
        assert_eq!(state.latest_posted_ts, 1700000000);
        // pending_queue should be empty (drain popped it)
        assert!(
            state.pending_queue.is_empty(),
            "pending_queue should be drained"
        );
    }

    #[tokio::test]
    async fn test_tick_no_new_galleries() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let server = MockServer::start().await;

        let (_chat_id, task_id, _sub_id) = setup_subscription(&repo, None).await;

        // Set subscription state with a high latest_posted_ts (future)
        let subs = repo.list_subscriptions_by_task(task_id).await.unwrap();
        let state = SubscriptionState::EhTag(EhTagState {
            pushed_gids: vec![],
            latest_posted_ts: 9999999999, // far future
            pending_queue: vec![],
            retry_count: 0,
        });
        repo.update_subscription_latest_data(subs[0].id, Some(state))
            .await
            .unwrap();

        // Mock search: return one gallery with posted_ts=0 (always old)
        let search_html = search_html(123456, "abcdef0123", "Old Gallery");
        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_string(search_html))
            .mount(&server)
            .await;

        let engine = make_engine(Arc::clone(&repo), &server, None);

        let now = Local::now() - chrono::Duration::seconds(60);
        repo.update_task_after_poll(task_id, now).await.unwrap();

        engine.tick().await.unwrap();

        // No download should be enqueued (gallery is older than cursor)
        let pending = repo.count_pending_eh_downloads().await.unwrap();
        assert_eq!(
            pending, 0,
            "no downloads should be enqueued for old galleries"
        );
    }

    #[tokio::test]
    async fn test_tick_empty_search_results() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let server = MockServer::start().await;

        let (_, task_id, _) = setup_subscription(&repo, None).await;

        // Mock search: return empty HTML
        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_string("<html>no results</html>"))
            .mount(&server)
            .await;

        let engine = make_engine(Arc::clone(&repo), &server, None);

        let now = Local::now() - chrono::Duration::seconds(60);
        repo.update_task_after_poll(task_id, now).await.unwrap();

        engine.tick().await.unwrap();

        let pending = repo.count_pending_eh_downloads().await.unwrap();
        assert_eq!(pending, 0);
    }

    #[tokio::test]
    async fn test_tick_with_rating_filter_uses_48h_scan() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let server = MockServer::start().await;

        // Subscription with rating filter (triggers 48h scan mode)
        let filter = EhFilter {
            min_rating: Some(4),
            min_pages: None,
            max_pages: None,
            telegraph: false,
        };
        let (_, task_id, _) = setup_subscription(&repo, Some(filter)).await;

        // Mock search: return one gallery (posted_ts from search HTML is 0, but
        // metadata API will provide the real posted timestamp).
        let token = "abcdef0123";
        let search_html = search_html(999999, token, "Recent Gallery");
        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_string(search_html))
            .mount(&server)
            .await;

        // Mock metadata with rating=4.5 (passes min_rating=4) and recent timestamp
        let recent_ts = Local::now().timestamp() - 3600; // 1 hour ago
        let meta = metadata_json(999999, token, "Recent Gallery", recent_ts, "4.5", 20);
        Mock::given(method("POST"))
            .and(path("/api.php"))
            .respond_with(ResponseTemplate::new(200).set_body_json(meta))
            .mount(&server)
            .await;

        let engine = make_engine(Arc::clone(&repo), &server, None);

        let now = Local::now() - chrono::Duration::seconds(60);
        repo.update_task_after_poll(task_id, now).await.unwrap();

        engine.tick().await.unwrap();

        // With the fix, metadata is fetched first, then timestamp filtering
        // uses the real posted timestamp from the API. The gallery is 1h old,
        // which is within the 48h scan window, so it should be enqueued.
        let pending = repo.count_pending_eh_downloads().await.unwrap();
        assert_eq!(
            pending, 1,
            "gallery within 48h scan window should be enqueued"
        );
    }

    #[tokio::test]
    async fn test_tick_filters_by_rating() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let server = MockServer::start().await;

        // Subscription WITHOUT rating filter (normal mode)
        let (_, task_id, _) = setup_subscription(&repo, None).await;

        // Mock search: return one gallery
        let search_html = search_html(111222, "tok1111", "Gallery");
        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_string(search_html))
            .mount(&server)
            .await;

        // Mock metadata with rating=2.0 (low rating)
        let meta = metadata_json(111222, "tok1111", "Gallery", 1700000000, "2.0", 10);
        Mock::given(method("POST"))
            .and(path("/api.php"))
            .respond_with(ResponseTemplate::new(200).set_body_json(meta))
            .mount(&server)
            .await;

        // Now update the subscription to have a rating filter
        let subs = repo.list_subscriptions_by_task(task_id).await.unwrap();
        let filter = EhFilter {
            min_rating: Some(4),
            min_pages: None,
            max_pages: None,
            telegraph: false,
        };
        repo.upsert_eh_subscription(subs[0].chat_id, task_id, TagFilter::default(), Some(filter))
            .await
            .unwrap();

        let engine = make_engine(Arc::clone(&repo), &server, None);

        let now = Local::now() - chrono::Duration::seconds(60);
        repo.update_task_after_poll(task_id, now).await.unwrap();

        engine.tick().await.unwrap();

        // The aggregate filter has min_rating=4, but gallery rating is 2.0 → should be filtered out.
        // BUT: the engine first checks has_rating_filter() on the aggregate. If true → 48h scan mode.
        // In 48h scan mode, posted_ts=0 from parser → all galleries excluded.
        // So no download enqueued either way.
        let pending = repo.count_pending_eh_downloads().await.unwrap();
        assert_eq!(
            pending, 0,
            "gallery with rating 2.0 should not pass min_rating=4 filter"
        );
    }

    #[tokio::test]
    async fn test_tick_multiple_galleries_enqueues_up_to_max_push() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let server = MockServer::start().await;

        let (_, task_id, _) = setup_subscription(&repo, None).await;

        // Mock search
        let mut html = String::new();
        for i in 0..5u64 {
            html.push_str(&search_html(
                100 + i,
                &hex_token(i),
                &format!("Gallery {}", i),
            ));
        }
        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_string(html))
            .mount(&server)
            .await;

        // Mock metadata for all 5
        let mut gmetadata = Vec::new();
        for i in 0..5u64 {
            gmetadata.push(serde_json::json!({
                "gid": 100 + i,
                "token": hex_token(i),
                "title": format!("Gallery {}", i),
                "title_jpn": null,
                "category": "Manga",
                "thumb": "thumb.jpg",
                "uploader": "user",
                "posted": "1700000000",
                "filecount": "10",
                "filesize": 1000000,
                "expunged": false,
                "rating": "4.0",
                "tags": []
            }));
        }
        let meta = serde_json::json!({ "gmetadata": gmetadata });
        Mock::given(method("POST"))
            .and(path("/api.php"))
            .respond_with(ResponseTemplate::new(200).set_body_json(meta))
            .mount(&server)
            .await;

        let engine = make_engine(Arc::clone(&repo), &server, None);

        let now = Local::now() - chrono::Duration::seconds(60);
        repo.update_task_after_poll(task_id, now).await.unwrap();

        engine.tick().await.unwrap();

        // max_push_per_tick = 3
        let count = repo.count_pending_eh_downloads().await.unwrap();
        assert_eq!(
            count, 3,
            "should enqueue up to max_push_per_tick=3, got {}",
            count
        );

        // Verify state: pushed_gids should have 3 entries
        let subs = repo.list_subscriptions_by_task(task_id).await.unwrap();
        let state = super::eh_tag_subscription_state(&subs[0]).unwrap();
        assert_eq!(
            state.pushed_gids.len(),
            3,
            "pushed_gids should have 3 entries"
        );
    }

    #[tokio::test]
    async fn test_tick_error_backoff() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let server = MockServer::start().await;

        let (_, task_id, _) = setup_subscription(&repo, None).await;

        // Mock search: return 500 error
        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let engine = make_engine(Arc::clone(&repo), &server, None);

        let now = Local::now() - chrono::Duration::seconds(60);
        repo.update_task_after_poll(task_id, now).await.unwrap();

        // tick() should handle the error internally and backoff
        engine.tick().await.unwrap();

        // Task should have been rescheduled (next_poll_at should be ~1h in the future)
        let task = repo
            .get_task_by_type_value(
                TaskType::Ehentai,
                &EhTaskKey::new("female:elf", 0, &EhFilter::default()).to_task_value(),
            )
            .await
            .unwrap()
            .unwrap();
        // next_poll_at should be at least 30 minutes in the future (1h backoff)
        assert!(
            task.next_poll_at > Local::now().naive_local(),
            "task should be backed off to future"
        );
    }
}

#[cfg(test)]
mod download_processor_tests {
    use super::*;
    use crate::bot::notifier::Notifier;
    use crate::cache::FileCacheManager;
    use crate::config::EhentaiConfig;
    use crate::db::entities::eh_download_queue;
    use crate::db::repo::tests_helpers;
    use crate::db::types::{EhFilter, EhTaskKey, TagFilter, TaskType};
    use crate::pixiv::downloader::Downloader;
    use eh_client::{EhClientBuilder, EhCookies, TelegraphClient};
    use reqwest::Client;
    use sea_orm::{ActiveModelTrait, ColumnTrait, EntityTrait, PaginatorTrait, QueryFilter};
    use std::io::Write;
    use teloxide::requests::RequesterExt;
    use teloxide::Bot;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Build a Notifier whose Bot points at the given mock server (Telegram API mock).
    fn make_notifier(tg_server: &MockServer) -> Notifier {
        let url = url::Url::parse(&tg_server.uri()).unwrap();
        let bot = Bot::new("fake_token").set_api_url(url);
        let throttled = bot.throttle(teloxide::adaptors::throttle::Limits::default());
        let http = Client::new();
        let cache = FileCacheManager::new("data/test_cache", 7);
        let downloader = Arc::new(Downloader::new(http, cache));
        Notifier::new(throttled, downloader)
    }

    /// Build an EhClient pointing at the given mock server (e-hentai API mock).
    /// Includes login cookies so archive download path is used.
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

    /// Build a TelegraphClient pointing at the given mock server.
    fn make_telegraph_client(tg_server: &MockServer) -> Arc<TelegraphClient> {
        Arc::new(TelegraphClient::new_with_urls(
            "test_token".to_string(),
            format!("{}/telegraph/upload", tg_server.uri()),
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

    /// Create a ZIP file containing image entries for testing.
    fn create_test_zip(path: &std::path::Path, image_count: usize) {
        let file = std::fs::File::create(path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        for i in 0..image_count {
            let name = format!("page{:03}.jpg", i);
            let options = zip::write::SimpleFileOptions::default();
            zip.start_file(name, options).unwrap();
            // Write minimal JPEG header + dummy data
            let data = format!("fake_image_data_{}", i);
            zip.write_all(data.as_bytes()).unwrap();
        }
        zip.finish().unwrap();
    }

    /// Mock the Telegram sendDocument endpoint.
    async fn mock_tg_send_document(server: &MockServer) {
        let body = serde_json::json!({
            "ok": true,
            "result": {
                "message_id": 42,
                "date": 1700000000,
                "chat": {"id": -100, "type": "private"}
            }
        });
        Mock::given(method("POST"))
            .and(path("/botfake_token/SendDocument"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(server)
            .await;
    }

    /// Mock the Telegram sendMessage endpoint.
    async fn mock_tg_send_message(server: &MockServer) {
        let body = serde_json::json!({
            "ok": true,
            "result": {
                "message_id": 43,
                "date": 1700000000,
                "chat": {"id": -100, "type": "private"}
            }
        });
        Mock::given(method("POST"))
            .and(path("/botfake_token/SendMessage"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(server)
            .await;
    }

    /// Mock the e-hentai gallery page (contains archiver_key).
    async fn mock_eh_gallery_page(server: &MockServer, gid: u64, token: &str) {
        let archiver_key = format!("{}--abc123def456", gid);
        let html = format!(
            r#"<html><body>
            <a href="/archiver.php?gid={}&token={}&or={}">Archive Download</a>
            </body></html>"#,
            gid, token, archiver_key
        );
        Mock::given(method("GET"))
            .and(path(format!("/g/{}/{}/", gid, token)))
            .respond_with(ResponseTemplate::new(200).set_body_string(html))
            .mount(server)
            .await;
    }

    /// Mock the archiver.php POST endpoint (returns JS redirect HTML).
    async fn mock_eh_archiver_post(server: &MockServer, download_url: &str) {
        let html = format!(
            r#"<html><script>
            function gotonext() {{
                document.location = "{}?autostart=1";
            }}
            </script></html>"#,
            download_url
        );
        Mock::given(method("POST"))
            .and(path("/archiver.php"))
            .respond_with(ResponseTemplate::new(200).set_body_string(html))
            .mount(server)
            .await;
    }

    /// Mock the archive download URL (returns ZIP bytes).
    async fn mock_eh_archive_download(server: &MockServer, zip_bytes: Vec<u8>) {
        Mock::given(method("GET"))
            .and(path("/archive/123456/token/0"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(zip_bytes))
            .mount(server)
            .await;
    }

    /// Mock the Telegraph upload endpoint.
    async fn mock_telegraph_upload(server: &MockServer) {
        let body = serde_json::json!([{"src": "/file/abc123.jpg"}]);
        Mock::given(method("POST"))
            .and(path("/telegraph/upload"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(server)
            .await;
    }

    /// Mock the Telegraph createPage endpoint.
    async fn mock_telegraph_create_page(server: &MockServer) {
        let body = serde_json::json!({
            "ok": true,
            "result": {"url": "https://telegra.ph/Test-Gallery-01-01"}
        });
        Mock::given(method("POST"))
            .and(path("/createPage"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(server)
            .await;
    }

    /// Set up a test DB with a chat, task, subscription, and enqueued download.
    async fn setup_download_entry(repo: &Repo, telegraph: bool) -> i32 {
        let chat_id: i64 = -100;
        repo.upsert_chat(chat_id, "private".into(), None, true, Default::default())
            .await
            .unwrap();

        // Create task + subscription
        let eh_filter = EhFilter {
            min_rating: None,
            min_pages: None,
            max_pages: None,
            telegraph,
        };
        let key = EhTaskKey::new("female:elf", 0, &eh_filter);
        let task = repo
            .get_or_create_task(TaskType::Ehentai, key.to_task_value(), None)
            .await
            .unwrap();
        repo.upsert_eh_subscription(chat_id, task.id, TagFilter::default(), Some(eh_filter))
            .await
            .unwrap();

        // Enqueue a download
        let entry = repo
            .enqueue_eh_download(
                chat_id,
                123456,
                "abcdef0123",
                "Test Gallery",
                telegraph,
                SOURCE_SUBSCRIPTION,
            )
            .await
            .unwrap();
        entry.id
    }

    #[tokio::test]
    async fn test_download_processor_full_flow_without_telegraph() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let eh_server = MockServer::start().await;
        let tg_server = MockServer::start().await;

        let entry_id = setup_download_entry(&repo, false).await;

        // Mock e-hentai: gallery page → archiver.php → archive download
        mock_eh_gallery_page(&eh_server, 123456, "abcdef0123").await;
        let download_url = format!("{}/archive/123456/token/0", eh_server.uri());
        mock_eh_archiver_post(&eh_server, &download_url).await;

        // Create a real ZIP file and serve it
        let temp = tempfile::tempdir().unwrap();
        let zip_path = temp.path().join("test.zip");
        create_test_zip(&zip_path, 3);
        let zip_bytes = std::fs::read(&zip_path).unwrap();
        mock_eh_archive_download(&eh_server, zip_bytes).await;

        // Mock Telegram sendDocument
        mock_tg_send_document(&tg_server).await;

        let processor = EhDownloadProcessor::new(
            Arc::clone(&repo),
            make_notifier(&tg_server),
            make_eh_client(&eh_server),
            None,
            Arc::new(make_config()),
        );

        processor.tick().await.unwrap();

        // Verify download is marked as done
        let entry = eh_download_queue::Entity::find_by_id(entry_id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(entry.status, "done");
        assert!(entry.file_size > 0);
    }

    #[tokio::test]
    async fn test_download_processor_full_flow_with_telegraph() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let eh_server = MockServer::start().await;
        let tg_server = MockServer::start().await;

        let entry_id = setup_download_entry(&repo, true).await;

        // Mock e-hentai: gallery page → archiver.php → archive download
        mock_eh_gallery_page(&eh_server, 123456, "abcdef0123").await;
        let download_url = format!("{}/archive/123456/token/0", eh_server.uri());
        mock_eh_archiver_post(&eh_server, &download_url).await;

        // Create a real ZIP file and serve it
        let temp = tempfile::tempdir().unwrap();
        let zip_path = temp.path().join("test.zip");
        create_test_zip(&zip_path, 5);
        let zip_bytes = std::fs::read(&zip_path).unwrap();
        mock_eh_archive_download(&eh_server, zip_bytes).await;

        // Mock Telegram: sendDocument + sendMessage (for telegraph link)
        mock_tg_send_document(&tg_server).await;
        mock_tg_send_message(&tg_server).await;

        // Mock Telegraph: upload + createPage
        mock_telegraph_upload(&tg_server).await;
        mock_telegraph_create_page(&tg_server).await;

        let telegraph = make_telegraph_client(&tg_server);
        let processor = EhDownloadProcessor::new(
            Arc::clone(&repo),
            make_notifier(&tg_server),
            make_eh_client(&eh_server),
            Some(telegraph),
            Arc::new(make_config()),
        );

        processor.tick().await.unwrap();

        // Verify download is marked as done
        let entry = eh_download_queue::Entity::find_by_id(entry_id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(entry.status, "done");
        assert!(entry.file_size > 0);

        // Verify Telegram received: sendDocument + sendMessage (telegraph link)
        let received = tg_server.received_requests().await.unwrap();
        let has_send_document = received
            .iter()
            .any(|r| r.url.path().ends_with("/SendDocument"));
        let has_send_message = received
            .iter()
            .any(|r| r.url.path().ends_with("/SendMessage"));
        assert!(has_send_document, "should have called sendDocument");
        assert!(
            has_send_message,
            "should have called sendMessage for telegraph link"
        );
    }

    #[tokio::test]
    async fn test_download_processor_rate_limit_skips() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let eh_server = MockServer::start().await;
        let tg_server = MockServer::start().await;

        let entry_id = setup_download_entry(&repo, false).await;

        // Pre-fill a completed download to hit rate limit
        let now = Local::now().naive_local();
        let big_entry = eh_download_queue::ActiveModel {
            chat_id: sea_orm::Set(-100),
            gid: sea_orm::Set(999999),
            token: sea_orm::Set("dummy".into()),
            title: sea_orm::Set("Big Download".into()),
            telegraph: sea_orm::Set(false),
            source: sea_orm::Set(SOURCE_SUBSCRIPTION.into()),
            status: sea_orm::Set("done".into()),
            file_size: sea_orm::Set(11_000_000_000), // 11 GB > 10 GB limit
            error: sea_orm::Set(None),
            retry_count: sea_orm::Set(0),
            created_at: sea_orm::Set(now),
            started_at: sea_orm::Set(Some(now)),
            completed_at: sea_orm::Set(Some(now)),
            ..Default::default()
        };
        big_entry.insert(repo.db()).await.unwrap();

        let mut config = make_config();
        config.download_rate_limit_gb = 7; // 7 GB limit

        let processor = EhDownloadProcessor::new(
            Arc::clone(&repo),
            make_notifier(&tg_server),
            make_eh_client(&eh_server),
            None,
            Arc::new(config),
        );

        processor.tick().await.unwrap();

        // Entry should still be pending (not processed)
        let entry = eh_download_queue::Entity::find_by_id(entry_id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            entry.status, "pending",
            "download should remain pending due to rate limit"
        );
    }

    #[tokio::test]
    async fn test_download_processor_download_failure_marks_failed() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let eh_server = MockServer::start().await;
        let tg_server = MockServer::start().await;

        let entry_id = setup_download_entry(&repo, false).await;

        // Mock gallery page but archiver.php returns 500
        mock_eh_gallery_page(&eh_server, 123456, "abcdef0123").await;
        Mock::given(method("POST"))
            .and(path("/archiver.php"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&eh_server)
            .await;

        let processor = EhDownloadProcessor::new(
            Arc::clone(&repo),
            make_notifier(&tg_server),
            make_eh_client(&eh_server),
            None,
            Arc::new(make_config()),
        );

        processor.tick().await.unwrap();

        // Entry should be marked as failed
        let entry = eh_download_queue::Entity::find_by_id(entry_id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(entry.status, "failed");
        assert!(entry.error.is_some());
    }

    #[tokio::test]
    async fn test_download_processor_empty_zip_no_images() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let eh_server = MockServer::start().await;
        let tg_server = MockServer::start().await;

        // Create entry with telegraph=true
        let entry_id = setup_download_entry(&repo, true).await;

        mock_eh_gallery_page(&eh_server, 123456, "abcdef0123").await;
        let download_url = format!("{}/archive/123456/token/0", eh_server.uri());
        mock_eh_archiver_post(&eh_server, &download_url).await;

        // Create a ZIP with NO image files
        let temp = tempfile::tempdir().unwrap();
        let zip_path = temp.path().join("empty.zip");
        {
            let file = std::fs::File::create(&zip_path).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            zip.start_file("readme.txt", zip::write::SimpleFileOptions::default())
                .unwrap();
            zip.write_all(b"no images here").unwrap();
            zip.finish().unwrap();
        }
        let zip_bytes = std::fs::read(&zip_path).unwrap();
        mock_eh_archive_download(&eh_server, zip_bytes).await;

        // Mock Telegram
        mock_tg_send_document(&tg_server).await;
        mock_tg_send_message(&tg_server).await; // for error message

        let telegraph = make_telegraph_client(&tg_server);
        let processor = EhDownloadProcessor::new(
            Arc::clone(&repo),
            make_notifier(&tg_server),
            make_eh_client(&eh_server),
            Some(telegraph),
            Arc::new(make_config()),
        );

        processor.tick().await.unwrap();

        // Download should still be marked as done (the archive was downloaded successfully)
        // Telegraph upload failed, but that's a non-fatal error
        let entry = eh_download_queue::Entity::find_by_id(entry_id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            entry.status, "done",
            "archive download should succeed even if telegraph fails"
        );
    }

    #[tokio::test]
    async fn test_download_processor_no_pending_downloads() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let eh_server = MockServer::start().await;
        let tg_server = MockServer::start().await;

        // No downloads enqueued
        let processor = EhDownloadProcessor::new(
            Arc::clone(&repo),
            make_notifier(&tg_server),
            make_eh_client(&eh_server),
            None,
            Arc::new(make_config()),
        );

        // Should complete without error
        processor.tick().await.unwrap();
    }

    #[tokio::test]
    async fn test_download_processor_telegraph_failure_sends_error_message() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let eh_server = MockServer::start().await;
        let tg_server = MockServer::start().await;

        let entry_id = setup_download_entry(&repo, true).await;

        mock_eh_gallery_page(&eh_server, 123456, "abcdef0123").await;
        let download_url = format!("{}/archive/123456/token/0", eh_server.uri());
        mock_eh_archiver_post(&eh_server, &download_url).await;

        // Create ZIP with images
        let temp = tempfile::tempdir().unwrap();
        let zip_path = temp.path().join("test.zip");
        create_test_zip(&zip_path, 2);
        let zip_bytes = std::fs::read(&zip_path).unwrap();
        mock_eh_archive_download(&eh_server, zip_bytes).await;

        // Mock Telegram
        mock_tg_send_document(&tg_server).await;
        mock_tg_send_message(&tg_server).await;

        // Mock Telegraph upload to FAIL
        Mock::given(method("POST"))
            .and(path("/telegraph/upload"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&tg_server)
            .await;

        let telegraph = make_telegraph_client(&tg_server);
        let processor = EhDownloadProcessor::new(
            Arc::clone(&repo),
            make_notifier(&tg_server),
            make_eh_client(&eh_server),
            Some(telegraph),
            Arc::new(make_config()),
        );

        processor.tick().await.unwrap();

        // Download should still be marked done (telegraph failure is non-fatal)
        let entry = eh_download_queue::Entity::find_by_id(entry_id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(entry.status, "done");

        // Verify error message was sent to Telegram
        let received = tg_server.received_requests().await.unwrap();
        let has_error_message = received
            .iter()
            .any(|r| r.url.path().ends_with("/SendMessage"));
        assert!(has_error_message, "should have sent error message to chat");
    }

    /// Full pipeline test: EhEngine tick → enqueue → EhDownloadProcessor tick → send document
    #[tokio::test]
    async fn test_full_pipeline_engine_to_processor() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let eh_server = MockServer::start().await;
        let tg_server = MockServer::start().await;

        // Set up subscription (no telegraph for simplicity)
        let chat_id: i64 = -100;
        repo.upsert_chat(chat_id, "private".into(), None, true, Default::default())
            .await
            .unwrap();

        let eh_filter = EhFilter::default();
        let key = EhTaskKey::new("female:elf", 0, &eh_filter);
        let task_value = key.to_task_value();
        let task = repo
            .get_or_create_task(TaskType::Ehentai, task_value, None)
            .await
            .unwrap();
        repo.upsert_eh_subscription(chat_id, task.id, TagFilter::default(), None)
            .await
            .unwrap();

        // Make task pending
        let now = Local::now() - chrono::Duration::seconds(60);
        repo.update_task_after_poll(task.id, now).await.unwrap();

        // Mock e-hentai search
        let search_html = r#"<div class="gl1t">
  <a href="https://e-hentai.org/g/777888/abcdef0123">
    <img src="https://ehgt.org/t/777888.jpg" />
  </a>
  <div class="gl3t"><div class="glink">Pipeline Test Gallery</div></div>
</div>"#;
        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_string(search_html))
            .mount(&eh_server)
            .await;

        // Mock metadata API
        let meta = serde_json::json!({
            "gmetadata": [{
                "gid": 777888,
                "token": "abcdef0123",
                "title": "Pipeline Test Gallery",
                "title_jpn": null,
                "category": "Doujinshi",
                "thumb": "https://ehgt.org/t/thumb.jpg",
                "uploader": "testuser",
                "posted": "1700000000",
                "filecount": "10",
                "filesize": 1000000,
                "expunged": false,
                "rating": "4.5",
                "tags": ["parody:test"]
            }]
        });
        Mock::given(method("POST"))
            .and(path("/api.php"))
            .respond_with(ResponseTemplate::new(200).set_body_json(meta))
            .mount(&eh_server)
            .await;

        // Step 1: Run EhEngine tick to search and enqueue download
        let engine_client = make_eh_client(&eh_server);
        let mut engine_config = make_config();
        engine_config.min_interval_sec = 1;
        engine_config.max_interval_sec = 2;
        let engine = EhEngine::new(
            Arc::clone(&repo),
            Arc::clone(&engine_client),
            Arc::new(engine_config),
            30,
            3,
        );
        engine.tick().await.unwrap();

        // Verify download was enqueued
        let pending_count = repo.count_pending_eh_downloads().await.unwrap();
        assert_eq!(pending_count, 1, "EhEngine should have enqueued 1 download");

        // Step 2: Mock remaining e-hentai endpoints for download
        mock_eh_gallery_page(&eh_server, 777888, "abcdef0123").await;
        let download_url = format!("{}/archive/777888/token/0", eh_server.uri());
        mock_eh_archiver_post(&eh_server, &download_url).await;

        let temp = tempfile::tempdir().unwrap();
        let zip_path = temp.path().join("gallery.zip");
        create_test_zip(&zip_path, 3);
        let zip_bytes = std::fs::read(&zip_path).unwrap();

        // Need a separate mock for the archive download path
        Mock::given(method("GET"))
            .and(path("/archive/777888/token/0"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(zip_bytes))
            .mount(&eh_server)
            .await;

        // Mock Telegram sendDocument
        mock_tg_send_document(&tg_server).await;

        // Step 3: Run EhDownloadProcessor tick to download and send
        let processor = EhDownloadProcessor::new(
            Arc::clone(&repo),
            make_notifier(&tg_server),
            engine_client,
            None,
            Arc::new(make_config()),
        );
        processor.tick().await.unwrap();

        // Verify: download should be done
        let done_count = eh_download_queue::Entity::find()
            .filter(eh_download_queue::Column::Status.eq("done"))
            .count(repo.db())
            .await
            .unwrap();
        assert_eq!(done_count, 1, "download should be marked done");

        // Verify: Telegram received sendDocument
        let received = tg_server.received_requests().await.unwrap();
        let has_send_document = received
            .iter()
            .any(|r| r.url.path().ends_with("/SendDocument"));
        assert!(has_send_document, "should have sent document to Telegram");
    }
}
