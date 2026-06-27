# E-Hentai Decoupled Pipeline Implementation Plan

> **For agentic workers:** Use the subagent-driven-development skill to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Decouple the monolithic `EhDownloadProcessor` into four independent stages (collect, download, upload, publish) with per-stage retry, exponential backoff, and 429 backoff for pixi.mg uploads.

**Architecture:** Three independent workers (EhDownloadWorker, EhUploadWorker, EhPublishWorker) communicate solely through the `eh_download_queue` DB table. EhEngine (collect stage) remains mostly unchanged but drops dead `pending_queue` code.

**Tech Stack:** Rust, SeaORM, tokio, wiremock (tests), reqwest, zip crate

---

## File Structure

| File | Responsibility |
|------|---------------|
| `migration/src/m20260628_000000_eh_pipeline_decouple.rs` | Add zip_path, telegraph_url, next_retry_at columns + composite index |
| `migration/src/lib.rs` | Register new migration |
| `src/db/entities/eh_download_queue.rs` | Add 3 new fields to Model |
| `src/db/repo/eh_download_queue.rs` | New status constants + stage-specific repo methods + backoff helper |
| `src/db/types/state.rs` | Simplify EhTagState — remove pending_queue, retry_count, related methods |
| `src/scheduler/eh_engine.rs` | Rewrite: EhEngine (collect only, simplified) + 3 worker structs |
| `src/scheduler/mod.rs` | Export 3 new worker structs |
| `src/main.rs` | Spawn 3 workers instead of 1 processor |
| `eh_client/src/telegraph.rs` | Add 429 detection + Retry-After parsing on upload |
| `src/db/repo.rs` | Update test_helpers schema for new columns |

---

### Task 1: Database Migration — Add Pipeline Columns

**Files:**
- Create: `migration/src/m20260628_000000_eh_pipeline_decouple.rs`
- Modify: `migration/src/lib.rs`
- Modify: `src/db/entities/eh_download_queue.rs`
- Modify: `src/db/repo.rs` (test_helpers schema)

- [ ] **Step 1: Create migration file**

```rust
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // Add zip_path column
        manager
            .alter_table(
                Table::alter()
                    .table(EhDownloadQueue::Table)
                    .add_column(ColumnDef::new(EhDownloadQueue::ZipPath).text().null())
                    .to_owned(),
            )
            .await?;

        // Add telegraph_url column
        manager
            .alter_table(
                Table::alter()
                    .table(EhDownloadQueue::Table)
                    .add_column(ColumnDef::new(EhDownloadQueue::TelegraphUrl).text().null())
                    .to_owned(),
            )
            .await?;

        // Add next_retry_at column
        manager
            .alter_table(
                Table::alter()
                    .table(EhDownloadQueue::Table)
                    .add_column(ColumnDef::new(EhDownloadQueue::NextRetryAt).timestamp().null())
                    .to_owned(),
            )
            .await?;

        // Composite index for worker queries: (status, next_retry_at)
        manager
            .create_index(
                Index::create()
                    .name("idx_eh_download_queue_status_retry")
                    .table(EhDownloadQueue::Table)
                    .col(EhDownloadQueue::Status)
                    .col(EhDownloadQueue::NextRetryAt)
                    .to_owned(),
            )
            .await?;

        // Migrate stale data: any 'downloading' entries → 'pending'
        manager
            .exec_stmt(
                sea_orm::sea_query::Statement::update()
                    .table(EhDownloadQueue::Table)
                    .values([(EhDownloadQueue::Status, "pending".into())])
                    .and_where(sea_orm::sea_query::Expr::col(EhDownloadQueue::Status).eq("downloading"))
                    .to_owned(),
            )
            .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_index(
                Index::drop()
                    .name("idx_eh_download_queue_status_retry")
                    .to_owned(),
            )
            .await?;
        for col in [EhDownloadQueue::NextRetryAt, EhDownloadQueue::TelegraphUrl, EhDownloadQueue::ZipPath] {
            manager
                .alter_table(
                    Table::alter()
                        .table(EhDownloadQueue::Table)
                        .drop_column(col)
                        .to_owned(),
                )
                .await?;
        }
        Ok(())
    }
}

#[derive(DeriveIden)]
enum EhDownloadQueue {
    Table,
    Status,
    ZipPath,
    TelegraphUrl,
    NextRetryAt,
}
```

- [ ] **Step 2: Register migration in lib.rs**

Add `pub mod m20260628_000000_eh_pipeline_decouple;` and add `Box::new(Migration)` to the migrations vec in `migration/src/lib.rs`.

- [ ] **Step 3: Update entity model**

Add to `src/db/entities/eh_download_queue.rs` Model struct (after `completed_at`):

```rust
    /// Local path to the downloaded ZIP file (set by download stage).
    #[sea_orm(nullable)]
    pub zip_path: Option<String>,
    /// Telegraph page URL (set by upload stage, only for telegraph=true entries).
    #[sea_orm(nullable)]
    pub telegraph_url: Option<String>,
    /// Earliest time to retry this entry (for backoff).
    #[sea_orm(nullable)]
    pub next_retry_at: Option<DateTime>,
```

- [ ] **Step 4: Update test_helpers schema in repo.rs**

In `src/db/repo.rs`, the `setup_test_db()` function's raw SQL for `eh_download_queue` must include the 3 new columns. Add `zip_path TEXT, telegraph_url TEXT, next_retry_at TIMESTAMP` to the CREATE TABLE statement.

- [ ] **Step 5: Verify compilation**

Run: `cargo check -p migration -p pixivbot`
Expected: compiles clean (may have dead_code warnings for new columns — those will be used in later tasks)

- [ ] **Step 6: Commit**

```bash
git add migration/src/m20260628_000000_eh_pipeline_decouple.rs migration/src/lib.rs src/db/entities/eh_download_queue.rs src/db/repo.rs
git commit -m "feat: add eh_pipeline_decouple migration with zip_path, telegraph_url, next_retry_at columns"
```

---

### Task 2: Repo Layer — New Status Constants + Stage-Specific Queries

**Files:**
- Modify: `src/db/repo/eh_download_queue.rs`

- [ ] **Step 1: Add new status constants**

Add after existing `STATUS_FAILED`:

```rust
pub const STATUS_DOWNLOADED: &str = "downloaded";
pub const STATUS_UPLOADING: &str = "uploading";
pub const STATUS_UPLOADED: &str = "uploaded";
pub const STATUS_PUBLISHING: &str = "publishing";
```

- [ ] **Step 2: Add backoff calculation helper**

```rust
/// Calculate exponential backoff delay for a given retry count.
/// Returns seconds: 1min, 5min, 15min for retries 1, 2, 3.
pub fn backoff_delay_secs(retry_count: i32) -> i64 {
    match retry_count {
        0 | 1 => 60,
        2 => 300,
        3 => 900,
        _ => 3600, // 1h for anything beyond max
    }
}
```

- [ ] **Step 3: Add `mark_downloaded` method**

```rust
/// Mark a download as downloaded (ZIP saved to cache). Transitions to `downloaded` status.
pub async fn mark_eh_download_downloaded(
    &self,
    id: i32,
    file_size: i64,
    zip_path: &str,
) -> Result<eh_download_queue::Model> {
    let entry = eh_download_queue::Entity::find_by_id(id)
        .one(&self.db)
        .await
        .context("Failed to fetch eh download")?
        .ok_or_else(|| anyhow::anyhow!("EH download {} not found", id))?;

    let now = Local::now().naive_local();
    let mut active: eh_download_queue::ActiveModel = entry.into();
    active.status = Set(STATUS_DOWNLOADED.to_string());
    active.file_size = Set(file_size);
    active.zip_path = Set(Some(zip_path.to_string()));
    active.completed_at = Set(Some(now));
    active.started_at = Set(None);
    active.error = Set(None);
    active.next_retry_at = Set(None);
    active
        .update(&self.db)
        .await
        .context("Failed to mark eh download as downloaded")
}
```

- [ ] **Step 4: Add `mark_uploaded` method**

```rust
/// Mark a download as uploaded (Telegraph page created). Transitions to `uploaded` status.
pub async fn mark_eh_download_uploaded(
    &self,
    id: i32,
    telegraph_url: &str,
) -> Result<eh_download_queue::Model> {
    let entry = eh_download_queue::Entity::find_by_id(id)
        .one(&self.db)
        .await
        .context("Failed to fetch eh download")?
        .ok_or_else(|| anyhow::anyhow!("EH download {} not found", id))?;

    let now = Local::now().naive_local();
    let mut active: eh_download_queue::ActiveModel = entry.into();
    active.status = Set(STATUS_UPLOADED.to_string());
    active.telegraph_url = Set(Some(telegraph_url.to_string()));
    active.completed_at = Set(Some(now));
    active.error = Set(None);
    active.next_retry_at = Set(None);
    active
        .update(&self.db)
        .await
        .context("Failed to mark eh download as uploaded")
}
```

- [ ] **Step 5: Add `get_next_for_download` method**

```rust
/// Get next entry for the download stage: status=pending, next_retry_at is NULL or <= now.
/// Atomically marks it as 'downloading'.
pub async fn get_next_for_download(&self) -> Result<Option<eh_download_queue::Model>> {
    let now = Local::now().naive_local();
    let entry = eh_download_queue::Entity::find()
        .filter(eh_download_queue::Column::Status.eq(STATUS_PENDING))
        .filter(
            eh_download_queue::Column::NextRetryAt
                .is_null()
                .or(eh_download_queue::Column::NextRetryAt.lte(now)),
        )
        .order_by(eh_download_queue::Column::CreatedAt, Order::Asc)
        .one(&self.db)
        .await
        .context("Failed to fetch next for download")?;

    if let Some(model) = entry {
        let now = Local::now().naive_local();
        let mut active: eh_download_queue::ActiveModel = model.into();
        active.status = Set(STATUS_DOWNLOADING.to_string());
        active.started_at = Set(Some(now));
        active.next_retry_at = Set(None);
        let updated = active
            .update(&self.db)
            .await
            .context("Failed to mark as downloading")?;
        Ok(Some(updated))
    } else {
        Ok(None)
    }
}
```

- [ ] **Step 6: Add `get_next_for_upload` method**

```rust
/// Get next entry for the upload stage: status=downloaded, telegraph=true, next_retry_at ok.
/// Atomically marks it as 'uploading'.
pub async fn get_next_for_upload(&self) -> Result<Option<eh_download_queue::Model>> {
    let now = Local::now().naive_local();
    let entry = eh_download_queue::Entity::find()
        .filter(eh_download_queue::Column::Status.eq(STATUS_DOWNLOADED))
        .filter(eh_download_queue::Column::Telegraph.eq(true))
        .filter(
            eh_download_queue::Column::NextRetryAt
                .is_null()
                .or(eh_download_queue::Column::NextRetryAt.lte(now)),
        )
        .order_by(eh_download_queue::Column::CreatedAt, Order::Asc)
        .one(&self.db)
        .await
        .context("Failed to fetch next for upload")?;

    if let Some(model) = entry {
        let mut active: eh_download_queue::ActiveModel = model.into();
        active.status = Set(STATUS_UPLOADING.to_string());
        active.started_at = Set(Some(now));
        active.next_retry_at = Set(None);
        let updated = active
            .update(&self.db)
            .await
            .context("Failed to mark as uploading")?;
        Ok(Some(updated))
    } else {
        Ok(None)
    }
}
```

- [ ] **Step 7: Add `get_next_for_publish` method**

```rust
/// Get next entry for the publish stage: either (downloaded, telegraph=false) or (uploaded).
/// Atomically marks it as 'publishing'.
pub async fn get_next_for_publish(&self) -> Result<Option<eh_download_queue::Model>> {
    let now = Local::now().naive_local();
    let entry = eh_download_queue::Entity::find()
        .filter(
            sea_orm::Condition::any()
                .and(
                    eh_download_queue::Column::Status.eq(STATUS_DOWNLOADED)
                        .and(eh_download_queue::Column::Telegraph.eq(false)),
                )
                .or(eh_download_queue::Column::Status.eq(STATUS_UPLOADED)),
        )
        .filter(
            eh_download_queue::Column::NextRetryAt
                .is_null()
                .or(eh_download_queue::Column::NextRetryAt.lte(now)),
        )
        .order_by(eh_download_queue::Column::CreatedAt, Order::Asc)
        .one(&self.db)
        .await
        .context("Failed to fetch next for publish")?;

    if let Some(model) = entry {
        let mut active: eh_download_queue::ActiveModel = model.into();
        active.status = Set(STATUS_PUBLISHING.to_string());
        active.started_at = Set(Some(now));
        active.next_retry_at = Set(None);
        let updated = active
            .update(&self.db)
            .await
            .context("Failed to mark as publishing")?;
        Ok(Some(updated))
    } else {
        Ok(None)
    }
}
```

- [ ] **Step 8: Add `schedule_retry` method**

```rust
/// Schedule a retry for an entry: set status back to a target status, increment retry_count,
/// set next_retry_at to now + backoff. If retry_count exceeds max, set status=failed.
/// Returns (model, is_permanent_failure).
pub async fn schedule_eh_retry(
    &self,
    id: i32,
    target_status: &str,
    error: &str,
    max_retry_count: u8,
) -> Result<(eh_download_queue::Model, bool)> {
    let entry = eh_download_queue::Entity::find_by_id(id)
        .one(&self.db)
        .await
        .context("Failed to fetch eh download")?
        .ok_or_else(|| anyhow::anyhow!("EH download {} not found", id))?;

    let new_retry_count = entry.retry_count + 1;
    let is_permanent = new_retry_count > max_retry_count as i32;
    let now = Local::now().naive_local();

    let mut active: eh_download_queue::ActiveModel = entry.into();
    if is_permanent {
        active.status = Set(STATUS_FAILED.to_string());
        active.completed_at = Set(Some(now));
    } else {
        let delay = backoff_delay_secs(new_retry_count);
        active.status = Set(target_status.to_string());
        active.next_retry_at = Set(Some(now + chrono::Duration::seconds(delay)));
        active.started_at = Set(None);
    }
    active.error = Set(Some(error.to_string()));
    active.retry_count = Set(new_retry_count);
    let model = active
        .update(&self.db)
        .await
        .context("Failed to schedule retry")?;
    Ok((model, is_permanent))
}
```

- [ ] **Step 9: Add `cleanup_eh_cache_orphans` method**

```rust
/// Delete ZIP files in the eh_cache directory that have no corresponding queue entry
/// in an active status (downloaded, uploading, uploaded, publishing).
pub async fn cleanup_eh_cache_orphans(&self, cache_dir: &std::path::Path) -> Result<()> {
    if !cache_dir.exists() {
        return Ok(());
    }

    // Get all zip_paths from active entries
    let active_paths: std::collections::HashSet<String> = eh_download_queue::Entity::find()
        .filter(
            eh_download_queue::Column::Status.is_in([
                STATUS_DOWNLOADED,
                STATUS_UPLOADING,
                STATUS_UPLOADED,
                STATUS_PUBLISHING,
            ]),
        )
        .all(&self.db)
        .await?
        .into_iter()
        .filter_map(|e| e.zip_path)
        .collect();

    // Scan cache dir and delete orphans
    for entry in std::fs::read_dir(cache_dir).context("Failed to read eh_cache dir")? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("zip") {
            let path_str = path.to_string_lossy().to_string();
            if !active_paths.contains(&path_str) {
                if let Err(e) = std::fs::remove_file(&path) {
                    warn!("Failed to remove orphan zip {}: {}", path.display(), e);
                }
            }
        }
    }

    Ok(())
}
```

- [ ] **Step 10: Run tests + commit**

Run: `cargo test -p pixivbot -- eh_download_queue`
Expected: existing tests still pass (new methods not yet tested, will be in integration tests)

```bash
git add src/db/repo/eh_download_queue.rs
git commit -m "feat: add stage-specific repo methods for decoupled EH pipeline"
```

---

### Task 3: Simplify EhTagState — Remove Dead Code

**Files:**
- Modify: `src/db/types/state.rs`

- [ ] **Step 1: Simplify EhTagState struct**

Remove `pending_queue` and `retry_count` fields. Remove `popped_front`, `with_retry_increment`, `should_abandon_queue` methods. Keep `pushed_gids`, `latest_posted_ts`, `cleared`, `add_pushed_gid`, `trim_pushed`.

New struct:
```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "state")]
pub struct EhTagState {
    pub pushed_gids: Vec<u64>,
    pub latest_posted_ts: i64,
}
```

- [ ] **Step 2: Update methods**

```rust
impl EhTagState {
    pub fn cleared(latest_posted_ts: i64) -> Self {
        Self {
            pushed_gids: Vec::new(),
            latest_posted_ts,
        }
    }

    pub fn add_pushed_gid(&mut self, gid: u64) {
        if !self.pushed_gids.contains(&gid) {
            self.pushed_gids.push(gid);
        }
    }

    pub fn trim_pushed(&mut self, cap: usize) {
        while self.pushed_gids.len() > cap {
            self.pushed_gids.remove(0);
        }
    }
}
```

- [ ] **Step 3: Remove QueuedEhGallery struct**

It's no longer needed since pending_queue is removed. Delete the struct and all references.

- [ ] **Step 4: Update tests**

Remove tests for `popped_front`, `with_retry_increment`, `should_abandon_queue`. Keep tests for `cleared`, `add_pushed_gid`, `trim_pushed`.

- [ ] **Step 5: Run tests + commit**

Run: `cargo test -p pixivbot -- eh_tag_state`
Expected: PASS

```bash
git add src/db/types/state.rs
git commit -m "refactor: simplify EhTagState — remove dead pending_queue and retry_count"
```

---

### Task 4: Telegraph Client — 429 Detection + Backoff

**Files:**
- Modify: `eh_client/src/telegraph.rs`
- Modify: `eh_client/src/error.rs`

- [ ] **Step 1: Add RateLimited error variant**

In `eh_client/src/error.rs`, add:

```rust
/// HTTP 429 Too Many Requests with optional retry-after hint (seconds).
#[error("rate limited (429), retry after {retry_after_secs:?}s")]
RateLimited { retry_after_secs: Option<u64> },
```

- [ ] **Step 2: Update upload_images_batch to detect 429**

In `eh_client/src/telegraph.rs`, in `upload_images_batch`, after sending the request and before checking `status.is_success()`:

```rust
let resp = self.http.post(&self.upload_url).multipart(form).send().await?;
let status = resp.status();

// Detect 429 rate limiting
if status.as_u16() == 429 {
    let retry_after = resp
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok());
    return Err(Error::RateLimited { retry_after_secs: retry_after });
}

if !status.is_success() {
    let body = resp.text().await.unwrap_or_default();
    return Err(Error::Api {
        message: format!("pixi.mg upload returned {}: {}", status, body),
        status: status.as_u16(),
    });
}
```

- [ ] **Step 3: Add `upload_images_with_retry` method**

Add a new method on `TelegraphClient` that wraps `upload_images_batch` with 429 backoff:

```rust
/// Upload images with automatic 429 backoff. Retries up to `max_retries` times
/// on HTTP 429, waiting exponentially longer each time.
/// Returns the uploaded URLs (all images in one batch).
pub async fn upload_images_with_retry(
    &self,
    images: &[&[u8]],
    max_retries: u32,
) -> Result<Vec<String>> {
    let mut attempt = 0u32;
    loop {
        match self.upload_images_batch(images).await {
            Ok(urls) => return Ok(urls),
            Err(Error::RateLimited { retry_after_secs }) => {
                if attempt >= max_retries {
                    return Err(Error::RateLimited { retry_after_secs });
                }
                let wait = retry_after_secs
                    .unwrap_or_else(|| 40u64 * 2u64.pow(attempt));
                tracing::warn!(
                    "pixi.mg returned 429, waiting {}s before retry (attempt {}/{})",
                    wait, attempt + 1, max_retries
                );
                tokio::time::sleep(tokio::time::Duration::from_secs(wait)).await;
                attempt += 1;
            }
            Err(e) => return Err(e),
        }
    }
}
```

- [ ] **Step 4: Run tests + commit**

Run: `cargo test -p eh_client`
Expected: existing tests pass (new 429 behavior will be tested in integration tests)

```bash
git add eh_client/src/telegraph.rs eh_client/src/error.rs
git commit -m "feat: add 429 rate-limit detection and exponential backoff for pixi.mg uploads"
```

---

### Task 5: Rewrite EhEngine — Collect Stage Only

**Files:**
- Modify: `src/scheduler/eh_engine.rs` (rewrite the EhEngine portion, remove EhDownloadProcessor)

- [ ] **Step 1: Rewrite EhEngine struct and execute_eh_task**

The EhEngine stays mostly the same but:
1. Remove `drain_pending_queue()` entirely
2. Remove `process_eh_sub()`'s call to `drain_pending_queue()`
3. Remove `all_have_pending` check (no pending_queue anymore)
4. Remove `schedule_drain_poll()`
5. Remove `DRAIN_POLL_INTERVAL_SEC`
6. Remove `max_retry_count` field (not needed in collect stage)
7. Simplify `process_eh_sub`: just add to pushed_gids, update state, enqueue download. No pending_queue manipulation.
8. Remove `gallery_to_queued()` function (QueuedEhGallery no longer exists).

New `process_eh_sub`:
```rust
async fn process_eh_sub(
    &self,
    sub: &subscriptions::Model,
    galleries: &[EhGallery],
) -> Result<()> {
    let mut state = eh_tag_subscription_state(sub).unwrap_or_else(|| EhTagState::cleared(0));
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
            warn!("Failed to enqueue download for gallery {}: {:#}", gallery.gid, e);
        }
    }

    Ok(())
}
```

New `execute_eh_task` — remove `all_have_pending` block, remove `any_has_pending` tracking, always call `schedule_next_poll`:

```rust
async fn execute_eh_task(&self, task: &crate::db::entities::tasks::Model) -> Result<()> {
    // ... (same parsing, listing, aggregate filter, fetch, metadata, filter logic) ...

    if filtered.is_empty() {
        for sub in &subs {
            self.update_sub_state_no_new(sub, oldest_ts).await;
        }
        self.schedule_next_poll(task.id).await;
        return Ok(());
    }

    for sub in &subs {
        if let Err(e) = self.process_eh_sub(sub, &filtered).await {
            warn!("Failed to process eh sub {}: {:#}", sub.id, e);
        }
    }

    self.schedule_next_poll(task.id).await;
    Ok(())
}
```

- [ ] **Step 2: Remove EhDownloadProcessor struct entirely**

Delete the entire `EhDownloadProcessor` struct and impl block (lines ~453-778 in current file). This will be replaced by 3 workers in the next task.

- [ ] **Step 3: Remove dead code**

Remove: `gallery_to_queued`, `sanitize_filename`, `build_download_caption` (these move to workers).
Remove imports that are no longer needed (`Notifier`, `TelegraphClient`, etc. — keep only what EhEngine uses).

- [ ] **Step 4: Update EhEngine::new signature**

Remove `max_retry_count` parameter (collect stage doesn't retry):

```rust
pub fn new(
    repo: Arc<Repo>,
    client: Arc<EhClient>,
    config: Arc<EhentaiConfig>,
    tick_interval_sec: u64,
) -> Self {
    Self { repo, client, config, tick_interval_sec }
}
```

- [ ] **Step 5: Update existing integration tests**

The integration tests in `eh_engine.rs` reference `max_retry_count` in `make_engine`. Remove that param. Tests that checked `pending_queue.is_empty()` need updating. The `download_processor_tests` module will be deleted entirely (replaced by worker tests in Task 7).

- [ ] **Step 6: Run tests + commit**

Run: `cargo test -p pixivbot -- eh_engine::integration_tests`
Expected: collect-stage tests pass

```bash
git add src/scheduler/eh_engine.rs src/scheduler/helpers.rs
git commit -m "refactor: rewrite EhEngine as collect-only stage, remove EhDownloadProcessor"
```

---

### Task 6: Implement Three Workers

**Files:**
- Modify: `src/scheduler/eh_engine.rs` (add 3 new worker structs)
- Modify: `src/scheduler/mod.rs`

- [ ] **Step 1: Add EhDownloadWorker**

```rust
/// Stage 2: Download worker — fetches archives from e-hentai and caches them locally.
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
        Self { repo, client, config, cache_dir }
    }

    pub async fn run(self) {
        // Startup: reset stale + clean orphan cache files
        let _ = self.repo.reset_stale_eh_downloads().await;
        let eh_cache = self.cache_dir.join("eh_cache");
        let _ = self.repo.cleanup_eh_cache_orphans(&eh_cache).await;

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
        let downloaded_bytes = self.repo
            .get_eh_downloaded_bytes_in_window(self.config.download_rate_window_hours)
            .await?;
        if downloaded_bytes >= self.config.download_rate_limit_bytes() as i64 {
            return Ok(());
        }

        let entry = self.repo.get_next_for_download().await?;
        let Some(entry) = entry else { return Ok(()) };

        if let Err(e) = self.process(&entry).await {
            error!("Download failed for entry {}: {:#}", entry.id, e);
            let (_, permanent) = self.repo
                .schedule_eh_retry(entry.id, STATUS_PENDING, &e.to_string(), self.config.max_retry_count)
                .await?;
            if permanent {
                self.notify_failure(&entry, &e.to_string()).await;
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
            // Chat disabled — mark as done without downloading (skip)
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
            let archiver_key = self.client.get_archiver_key(gid, token).await
                .context("Failed to get archiver key")?;
            let resolution = if entry.source == "direct" {
                &self.config.download_resolution
            } else {
                &self.config.subscription_resolution
            };
            self.client.download_archive(gid, token, &archiver_key, resolution, &zip_path).await
                .context("Failed to download archive")?
        } else {
            self.client.download_gallery_images(gid, token, &zip_path).await
                .context("Failed to download gallery images")?
        };

        info!("Downloaded eh gallery gid={} size={} bytes", gid, file_size);

        self.repo.mark_eh_download_downloaded(entry.id, file_size as i64, &zip_path_str).await?;
        Ok(())
    }

    async fn notify_failure(&self, entry: &eh_download_queue::Model, error: &str) {
        // Best-effort notification — requires Notifier, but we don't have one here.
        // The publish worker or a separate notification mechanism handles this.
        // For now, just log.
        warn!("Permanent download failure for gid={}: {}", entry.gid, error);
    }
}
```

- [ ] **Step 2: Add EhUploadWorker**

```rust
/// Stage 3: Upload worker — extracts images from ZIP, uploads to pixi.mg, creates Telegraph page.
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
        Self { repo, notifier, telegraph, config }
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
        let Some(entry) = entry else { return Ok(()) };

        if let Err(e) = self.process(&entry).await {
            error!("Upload failed for entry {}: {:#}", entry.id, e);
            let (_, permanent) = self.repo
                .schedule_eh_retry(entry.id, STATUS_DOWNLOADED, &e.to_string(), self.config.max_retry_count)
                .await?;
            if permanent {
                // Notify chat about permanent failure
                let escaped = teloxide::utils::markdown::escape(&e.to_string());
                let title = teloxide::utils::markdown::escape(&entry.title);
                let msg = format!("⚠️ Telegraph 上传失败: {}\n\n📦 {}", escaped, title);
                let _ = self.notifier.send_text(
                    teloxide::types::ChatId(entry.chat_id), &msg, false
                ).await;
            }
        }
        Ok(())
    }

    async fn process(&self, entry: &eh_download_queue::Model) -> Result<()> {
        let zip_path = entry.zip_path.as_ref()
            .context("zip_path is None for downloaded entry")?;
        let zip_path = std::path::Path::new(zip_path);

        // Extract images in spawn_blocking
        let zip_path_owned = zip_path.to_path_buf();
        let image_data_list: Vec<(String, Vec<u8>)> = tokio::task::spawn_blocking(move || {
            let zip_file = std::fs::File::open(&zip_path_owned).context("Failed to open zip")?;
            let mut archive = zip::ZipArchive::new(zip_file).context("Failed to read zip")?;
            let mut images = Vec::new();
            for i in 0..archive.len() {
                let mut file = archive.by_index(i).context("Failed to read zip entry")?;
                let name = file.name().to_lowercase();
                if !name.ends_with(".jpg") && !name.ends_with(".jpeg")
                    && !name.ends_with(".png") && !name.ends_with(".gif")
                    && !name.ends_with(".webp") { continue; }
                let mut data = Vec::new();
                std::io::Read::read_to_end(&mut file, &mut data)
                    .context("Failed to read image")?;
                if data.len() > 6 * 1024 * 1024 { continue; }
                let filename = std::path::Path::new(file.name())
                    .file_name().and_then(|n| n.to_str())
                    .unwrap_or("image.jpg").to_string();
                images.push((filename, data));
            }
            Ok::<_, anyhow::Error>(images)
        }).await.context("spawn_blocking failed")??;

        if image_data_list.is_empty() {
            anyhow::bail!("No images found in ZIP");
        }

        // Upload images to pixi.mg with 429 backoff
        let mut all_urls = Vec::new();
        for chunk in image_data_list.chunks(5) {
            let refs: Vec<&[u8]> = chunk.iter().map(|(_, d)| d.as_slice()).collect();
            let urls = self.telegraph.upload_images_with_retry(&refs, 3).await
                .context("Failed to upload images to pixi.mg")?;
            all_urls.extend(urls);
        }

        if all_urls.is_empty() {
            anyhow::bail!("No images uploaded to pixi.mg");
        }

        // Create Telegraph gallery page
        let title = if entry.title.is_empty() { "Gallery" } else { &entry.title };
        let page_url = self.telegraph.create_gallery_page(title, &all_urls).await
            .context("Failed to create telegraph page")?;

        info!("Created telegraph page for gid={}: {}", entry.gid, page_url);
        self.repo.mark_eh_download_uploaded(entry.id, &page_url).await?;
        Ok(())
    }
}
```

- [ ] **Step 3: Add EhPublishWorker**

```rust
/// Stage 4: Publish worker — sends the archive ZIP and/or Telegraph link to the Telegram chat.
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
        Self { repo, notifier, client, config }
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
        let Some(entry) = entry else { return Ok(()) };

        if let Err(e) = self.process(&entry).await {
            error!("Publish failed for entry {}: {:#}", entry.id, e);
            // Retry: go back to the pre-publish status
            let target = if entry.telegraph_url.is_some() { STATUS_UPLOADED } else { STATUS_DOWNLOADED };
            let (_, permanent) = self.repo
                .schedule_eh_retry(entry.id, target, &e.to_string(), self.config.max_retry_count)
                .await?;
            if permanent {
                let escaped = teloxide::utils::markdown::escape(&e.to_string());
                let title = teloxide::utils::markdown::escape(&entry.title);
                let msg = format!("⚠️ 发布失败: {}\n\n📦 {}", escaped, title);
                let _ = self.notifier.send_text(
                    teloxide::types::ChatId(entry.chat_id), &msg, false
                ).await;
            }
        }
        Ok(())
    }

    async fn process(&self, entry: &eh_download_queue::Model) -> Result<()> {
        let chat = get_chat_if_should_notify(&self.repo, entry.chat_id).await?;
        if chat.is_none() {
            // Chat disabled — just mark done
            self.cleanup_zip(entry).await;
            self.repo.mark_eh_download_done(entry.id, entry.file_size).await?;
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
                    self.notifier.send_document(chat_id, zip_path, &filename, &caption).await
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
            self.notifier.send_text(chat_id, &link_text, false).await
                .context("Failed to send telegraph link")?;
        }

        // Mark done and clean up ZIP
        self.cleanup_zip(entry).await;
        self.repo.mark_eh_download_done(entry.id, entry.file_size).await?;
        info!("Published eh gallery gid={} to chat {}", entry.gid, entry.chat_id);
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
        let gallery_url = format!("{}/g/{}/{}", base_url.trim_end_matches('/'), entry.gid, entry.token);
        let url_escaped = teloxide::utils::markdown::escape_link_url(&gallery_url);
        format!("📦 {}\n\n🔗 [来源]({})", title, url_escaped)
    }
}

fn sanitize_filename(s: &str) -> String {
    s.chars().map(|c| match c {
        '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
        _ => c,
    }).collect()
}
```

- [ ] **Step 4: Update scheduler/mod.rs exports**

```rust
pub use eh_engine::{EhDownloadWorker, EhEngine, EhPublishWorker, EhUploadWorker};
```

- [ ] **Step 5: Run compilation check**

Run: `cargo check -p pixivbot`
Expected: compiles clean (warnings OK for unused code until main.rs wiring in Task 8)

```bash
git add src/scheduler/eh_engine.rs src/scheduler/mod.rs
git commit -m "feat: implement 3 decoupled workers (download, upload, publish) for EH pipeline"
```

---

### Task 7: Integration Tests for Workers

**Files:**
- Modify: `src/scheduler/eh_engine.rs` (add test modules)

- [ ] **Step 1: Write download worker tests**

Tests to add:
- `test_download_worker_full_flow`: pending → downloaded (ZIP exists on disk)
- `test_download_worker_rate_limit_skips`: pre-fill rate limit, entry stays pending
- `test_download_worker_chat_disabled_skips`: chat disabled → marked done without download
- `test_download_worker_failure_retries`: archiver 500 → entry back to pending with retry_count=1

- [ ] **Step 2: Write upload worker tests**

Tests to add:
- `test_upload_worker_full_flow`: downloaded → uploaded (telegraph_url set)
- `test_upload_worker_429_retry_success`: first upload returns 429, second succeeds
- `test_upload_worker_no_images_fails`: ZIP with no images → entry back to downloaded
- `test_upload_worker_permanent_failure_notifies`: retry_count exceeds max → failed + message sent

- [ ] **Step 3: Write publish worker tests**

Tests to add:
- `test_publish_worker_no_telegraph`: downloaded(telegraph=false) → done (sendDocument called, ZIP deleted)
- `test_publish_worker_with_telegraph`: uploaded → done (sendDocument + sendMessage called, ZIP deleted)
- `test_publish_worker_chat_disabled`: chat disabled → done without sending
- `test_publish_worker_send_failure_retries`: sendDocument 500 → entry back to downloaded/uploaded

- [ ] **Step 4: Write full pipeline test**

`test_full_pipeline_4_stage`: EhEngine tick (enqueue) → EhDownloadWorker tick (download) → EhUploadWorker tick (upload) → EhPublishWorker tick (publish + done). Verify final status=done and Telegram received both sendDocument + sendMessage.

- [ ] **Step 5: Run all tests + commit**

Run: `cargo test -p pixivbot -- eh_engine`
Expected: all tests pass

```bash
git add src/scheduler/eh_engine.rs
git commit -m "test: add integration tests for decoupled EH pipeline workers"
```

---

### Task 8: Wire Up Workers in main.rs

**Files:**
- Modify: `src/main.rs`

- [ ] **Step 1: Update EhEngine::new call**

Remove `max_retry_count` parameter:
```rust
let eh_engine = scheduler::EhEngine::new(
    repo.clone(),
    std::sync::Arc::clone(eh_client),
    std::sync::Arc::new(config.ehentai.clone()),
    scheduler_config.tick_interval_sec,
);
```

- [ ] **Step 2: Replace EhDownloadProcessor with 3 workers**

Replace the `eh_download_processor_handle` block with:

```rust
let cache_dir = std::path::PathBuf::from(&config.scheduler.cache_dir);

let eh_download_worker_handle = if let Some(ref eh_client) = eh_client {
    let worker = scheduler::EhDownloadWorker::new(
        repo.clone(),
        std::sync::Arc::clone(eh_client),
        std::sync::Arc::new(config.ehentai.clone()),
        cache_dir.clone(),
    );
    info!("✅ E-Hentai download worker initialized");
    Some(tokio::spawn(async move { worker.run().await }))
} else {
    None
};

let eh_upload_worker_handle = if let Some(ref eh_client) = eh_client {
    if let Some(ref telegraph) = telegraph_client {
        let worker = scheduler::EhUploadWorker::new(
            repo.clone(),
            notifier.clone(),
            std::sync::Arc::clone(telegraph),
            std::sync::Arc::new(config.ehentai.clone()),
        );
        info!("✅ E-Hentai upload worker initialized");
        Some(tokio::spawn(async move { worker.run().await }))
    } else {
        info!("E-Hentai upload worker disabled (no telegraph token)");
        None
    }
} else {
    None
};

let eh_publish_worker_handle = if let Some(ref eh_client) = eh_client {
    let worker = scheduler::EhPublishWorker::new(
        repo.clone(),
        notifier.clone(),
        std::sync::Arc::clone(eh_client),
        std::sync::Arc::new(config.ehentai.clone()),
    );
    info!("✅ E-Hentai publish worker initialized");
    Some(tokio::spawn(async move { worker.run().await }))
} else {
    None
};
```

- [ ] **Step 3: Update shutdown handles**

Replace `eh_download_processor_handle` abort with the 3 new handles:
```rust
if let Some(handle) = eh_download_worker_handle { handle.abort(); }
if let Some(handle) = eh_upload_worker_handle { handle.abort(); }
if let Some(handle) = eh_publish_worker_handle { handle.abort(); }
```

- [ ] **Step 4: Run compilation + commit**

Run: `cargo check -p pixivbot`
Expected: compiles clean

```bash
git add src/main.rs
git commit -m "feat: wire up 3 decoupled EH workers in main.rs"
```

---

### Task 9: CI Verification + Final Cleanup

**Files:**
- All

- [ ] **Step 1: Run fmt**

Run: `cargo fmt --all`
Expected: clean

- [ ] **Step 2: Run clippy**

Run: `cargo clippy --all-targets -p pixivbot -p eh_client -p migration -- -Dwarnings`
Expected: clean (fix any warnings)

- [ ] **Step 3: Run all tests**

Run: `cargo test -p pixivbot -p eh_client -p migration`
Expected: all pass

- [ ] **Step 4: Update config.toml.example if needed**

No new config fields, but verify the existing `[ehentai]` section docs are still accurate.

- [ ] **Step 5: Final commit**

```bash
git add -A
git commit -m "chore: fmt + clippy fixes for decoupled EH pipeline"
```
