# E-Hentai Decoupled Pipeline Design

## Problem

The current `EhDownloadProcessor.process_download()` is a monolithic function that couples
download → send → telegraph-upload → send-link into a single call. Issues:

1. **No retry on upload/publish failure**: download is marked `done` even if Telegram send
   or Telegraph upload fails.
2. **No 429 backoff for pixi.mg**: HTTP 429 causes immediate failure with no retry.
3. **Telegraph upload blocks the download processor**: a large gallery's sequential uploads
   (minutes) stall all other downloads.
4. **`pending_queue`/`retry_count` in EhTagState is dead code**: the engine pops the queue
   immediately after enqueuing, making the backpressure mechanism ineffective.

## Design: Four-Stage Decoupled Pipeline

### Stage Overview

```
Stage 1  Collect    (EhEngine)            → status=pending
Stage 2  Download   (EhDownloadWorker)    → status=downloaded (ZIP cached locally)
Stage 3  Upload     (EhUploadWorker)      → status=uploaded   (only telegraph=true)
Stage 4  Publish    (EhPublishWorker)     → status=done       (send ZIP + telegraph link)
```

Each stage runs as an independent tokio task with its own poll interval. Stages communicate
solely through the `eh_download_queue` DB table — no in-memory coupling.

### Status Flow

```
telegraph=false:
  pending → downloading → downloaded → publishing → done

telegraph=true:
  pending → downloading → downloaded → uploading → uploaded → publishing → done
```

Any stage failure → retry with exponential backoff:

| Retry # | Backoff  |
|---------|----------|
| 1       | 1 min    |
| 2       | 5 min    |
| 3       | 15 min   |
| >=4     | failed (permanent) |

Backoff is tracked via `next_retry_at` column. Workers skip entries where
`next_retry_at > now`.

### 429 Backoff (Upload Stage)

When pixi.mg returns HTTP 429:

1. Parse `Retry-After` header if present, else use exponential backoff:
   - 1st 429: wait 40s, retry the batch
   - 2nd 429: wait 80s
   - 3rd 429: wait 160s
2. If 3 consecutive 429s in one tick, abandon this entry → set `status=downloaded`,
   `next_retry_at = now + 5min` (will be retried in a future tick).
3. The worker does NOT process other entries while waiting (simple sequential model).
   This naturally throttles when pixi.mg is rate-limiting.

### Rate Limiting (Download Stage Only)

The 7GB/7days rate limit is enforced ONLY in the download stage:
- Before downloading: check `get_eh_downloaded_bytes_in_window()`.
- After downloading: the `file_size` is recorded.
- Overshoot is bounded by one archive size (acceptable — e-hentai's H@H limit is a soft
  threshold).

### File Cache

Downloaded ZIPs are stored in `{cache_dir}/eh_cache/{gid}_{token}.zip`:
- `cache_dir` comes from `SchedulerConfig.cache_dir` (default: `data/cache`).
- ZIP persists across stages (download writes it, publish reads it, publish deletes it).
- On startup: clean orphan files in `eh_cache/` that have no corresponding `downloaded`/
  `uploading`/`uploaded`/`publishing` queue entry.
- On permanent failure (`failed`): delete the ZIP.

### Failure Notification

When an entry reaches `failed` status (retry_count >= max_retry_count):
- Send a Telegram message to the chat: `⚠️ 下载失败: {title}\n原因: {error}`
- This happens in the worker that marks it failed (download/upload/publish).

## Database Changes

### New columns on `eh_download_queue`

| Column           | Type           | Default | Purpose                              |
|------------------|----------------|---------|--------------------------------------|
| `zip_path`       | Text, nullable | NULL    | Local path to downloaded ZIP         |
| `telegraph_url`  | Text, nullable | NULL    | Created Telegraph page URL           |
| `next_retry_at`  | Timestamp, nullable | NULL | Earliest time to retry this entry    |

### New status constants

```rust
pub const STATUS_DOWNLOADED: &str = "downloaded";
pub const STATUS_UPLOADING: &str = "uploading";
pub const STATUS_UPLOADED: &str = "uploaded";
pub const STATUS_PUBLISHING: &str = "publishing";
```

### Migration

New migration `m20260628_000000_eh_pipeline_decouple`:
- `ALTER TABLE eh_download_queue ADD COLUMN zip_path TEXT`
- `ALTER TABLE eh_download_queue ADD COLUMN telegraph_url TEXT`
- `ALTER TABLE eh_download_queue ADD COLUMN next_retry_at TIMESTAMP`
- `CREATE INDEX idx_eh_download_queue_status_retry ON eh_download_queue(status, next_retry_at)`
- Migrate existing data: `status='downloading'` → `status='pending'`; `status='done'` stays.

## Worker Design

### EhDownloadWorker

```
loop:
  reset_stale_downloads()           # downloading → pending (crash recovery)
  entry = get_next_for_download()    # status=pending, next_retry_at<=now or NULL
  if entry is None: sleep(poll_interval); continue
  if rate_limit_reached(): sleep(poll_interval); continue
  if chat_not_notifiable(entry): mark_skipped(); continue
  download_archive(entry) → save to cache
  if success: mark_downloaded(zip_path)
  if failure: mark_failed_or_retry(entry, error)
```

### EhUploadWorker

```
loop:
  entry = get_next_for_upload()      # status=downloaded, telegraph=true, next_retry_at<=now
  if entry is None: sleep(poll_interval); continue
  extract_images_from_zip(entry.zip_path)   # spawn_blocking
  upload_images_to_pixi(entry)              # with 429 backoff
  if success: create_telegraph_page() → mark_uploaded(telegraph_url)
  if failure: mark_failed_or_retry(entry, error)
```

### EhPublishWorker

```
loop:
  entry = get_next_for_publish()     # (downloaded, telegraph=false) OR (uploaded)
  if entry is None: sleep(poll_interval); continue
  if send_archive: send_document(chat, zip_path, caption)
  if telegraph_url: send_text(chat, telegraph_link)
  if success: mark_done(entry) → delete zip file
  if failure: mark_failed_or_retry(entry, error)
```

## Config Changes

No new config fields needed. Reuse existing:
- `download_rate_limit_gb` / `download_rate_window_hours` → download stage
- `download_poll_interval_sec` → all three workers (or split into three? Keep single for simplicity)
- `max_retry_count` → all stages

## Cleanup: Remove Dead Code

- Remove `pending_queue` and `retry_count` from `EhTagState` (download retry is fully
  handled by the queue table).
- Remove `drain_pending_queue()` from EhEngine.
- Remove `with_retry_increment()`, `should_abandon_queue()`, `popped_front()` from
  `EhTagState`.
- Simplify `EhTagState` to just `{ pushed_gids, latest_posted_ts }`.

## Testing Strategy

1. **Unit tests**: status transition logic, backoff calculation, 429 retry logic.
2. **Integration tests** (wiremock): each stage independently — download success/failure,
   upload success/429-retry/success, publish success/failure.
3. **Full pipeline test**: pending → downloaded → uploaded → done, with mock HTTP servers.
4. **Retry test**: fail stage 2, verify backoff + eventual success on retry.

## Files to Modify

| File | Change |
|------|--------|
| `migration/src/m20260628_000000_eh_pipeline_decouple.rs` | New migration |
| `migration/src/lib.rs` | Register migration |
| `src/db/entities/eh_download_queue.rs` | Add 3 new columns |
| `src/db/repo/eh_download_queue.rs` | New status constants + worker-specific query methods |
| `src/db/types/state.rs` | Simplify EhTagState (remove pending_queue, retry_count) |
| `src/scheduler/eh_engine.rs` | Rewrite: EhEngine (collect only) + 3 workers |
| `src/scheduler/mod.rs` | Export new worker structs |
| `src/main.rs` | Spawn 3 workers instead of 1 processor |
| `eh_client/src/telegraph.rs` | Add 429 detection + Retry-After parsing |
| `src/config.rs` | No changes (reuse existing fields) |
