use crate::bot::notifier::Notifier;
use crate::config::EhentaiConfig;
use crate::db::entities::eh_download_queue;
use crate::db::repo::eh_download_queue::SOURCE_SUBSCRIPTION;
use crate::db::repo::Repo;
use crate::db::types::{EhFilter, EhTagState, EhTaskKey, SubscriptionState, TaskType};
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
            for sub in &subs {
                self.update_sub_state_no_new(sub, oldest_ts).await;
            }
            self.schedule_next_poll(task.id).await;
            return Ok(());
        }

        // Process each subscription
        for sub in &subs {
            if let Err(e) = self.process_eh_sub(sub, &filtered).await {
                warn!("Failed to process eh sub {}: {:#}", sub.id, e);
            }
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
        let mut state =
            eh_tag_subscription_state(sub).unwrap_or_else(|| EhTagState::cleared(0));

        let sub_filter = sub.eh_filter.as_ref();
        let max_push = self.config.max_push_per_tick;

        let new_galleries: Vec<&EhGallery> = galleries
            .iter()
            .filter(|g| !state.pushed_gids.contains(&g.gid))
            .filter(|g| sub_filter.map(|f| f.matches(g)).unwrap_or(true))
            .collect();

        let to_enqueue: Vec<&EhGallery> = new_galleries.into_iter().take(max_push).collect();

        // Update state FIRST (mark as pushed), THEN enqueue downloads.
        for gallery in &to_enqueue {
            state.add_pushed_gid(gallery.gid);
            if gallery.posted > state.latest_posted_ts {
                state.latest_posted_ts = gallery.posted;
            }
        }
        state.trim_pushed(self.config.pushed_cap);

        self.repo
            .update_subscription_latest_data(sub.id, Some(SubscriptionState::EhTag(state)))
            .await
            .context("Failed to update eh subscription state")?;

        // Enqueue download requests
        let telegraph_default = sub_filter.map(|f| f.telegraph).unwrap_or(false);
        for gallery in &to_enqueue {
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
                warn!(
                    "Failed to enqueue download for gallery {}: {:#}",
                    gallery.gid, e
                );
            }
        }

        Ok(())
    }

    /// Update state when no new galleries were found.
    async fn update_sub_state_no_new(&self, sub: &crate::db::entities::subscriptions::Model, latest_ts: i64) {
        let state =
            eh_tag_subscription_state(sub).unwrap_or_else(|| EhTagState::cleared(latest_ts));
        if state.latest_posted_ts == latest_ts {
            return;
        }
        let new_state = EhTagState {
            pushed_gids: state.pushed_gids,
            latest_posted_ts: if latest_ts > 0 {
                latest_ts
            } else {
                state.latest_posted_ts
            },
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
        // Startup: reset stale + clean orphan cache files
        if let Err(e) = self.repo.reset_stale_eh_downloads().await {
            warn!("Failed to reset stale eh downloads: {:#}", e);
        }
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
                .schedule_eh_retry(
                    entry.id,
                    STATUS_PENDING,
                    &e.to_string(),
                    self.config.max_retry_count,
                )
                .await?;
            if permanent {
                warn!(
                    "Permanent download failure for gid={}: {}",
                    entry.gid, e
                );
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
                "Skipping download for gid={} — chat {} not notifiable",
                gid, entry.chat_id
            );
            self.repo.mark_eh_download_done(entry.id, 0).await?;
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

        info!(
            "Downloaded eh gallery gid={} size={} bytes",
            gid, file_size
        );

        self.repo
            .mark_eh_download_downloaded(entry.id, file_size as i64, &zip_path_str)
            .await?;

        Ok(())
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
                .schedule_eh_retry(
                    entry.id,
                    STATUS_DOWNLOADED,
                    &e.to_string(),
                    self.config.max_retry_count,
                )
                .await?;
            if permanent {
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

        info!(
            "Created telegraph page for gid={}: {}",
            entry.gid, page_url
        );

        self.repo
            .mark_eh_download_uploaded(entry.id, &page_url)
            .await?;

        Ok(())
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
                .schedule_eh_retry(
                    entry.id,
                    target,
                    &e.to_string(),
                    self.config.max_retry_count,
                )
                .await?;
            if permanent {
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
            // Chat disabled — just mark done and clean up
            self.cleanup_zip(entry).await;
            self.repo
                .mark_eh_download_done(entry.id, entry.file_size)
                .await?;
            return Ok(());
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
