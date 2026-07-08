# IPFS Cache Warmup and Slow Download Background Retry Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add non-blocking IPFS public gateway cache warmup after ipfS3 uploads and add background slow-download retries for archive downloads that repeatedly fail with poor progress.

**Architecture:** The two fixes are independent and should be implemented in separate commits. IPFS cache warmup runs as fire-and-forget HEAD requests after upload URL generation and never blocks upload success. Slow-download background retries use a separate scheduler worker/table state so the main download queue keeps processing other entries.

**Tech Stack:** Rust 1.94, reqwest HEAD requests, SeaORM queue state, Tokio background workers, existing EH retry/backoff patterns.

---

## Task 1: IPFS public gateway cache warmup

**Files:**
- Modify: `eh_client/src/telegraph.rs`
- Modify: `src/config.rs`
- Modify: `config.toml.example`

- [ ] **Step 1: Add config flag**

Add to `IpfS3UploaderConfig`:

```rust
#[serde(default)]
pub warm_public_gateway_after_upload: bool,
```

Document in `config.toml.example`:

```toml
# Send non-blocking HEAD requests to gateway_url after ipfS3 upload to warm IPFS gateway cache.
# warm_public_gateway_after_upload = false
```

- [ ] **Step 2: Add warmup test**

Add a wiremock test in `eh_client/src/telegraph.rs` that mocks a public gateway `HEAD /ipfs/<cid>` and asserts upload returns before warmup failure affects result. Use a small timeout in the warmup helper test. The warmup request must use `HEAD`; the test should return a response with a large body and assert upload completion does not depend on reading that body.

- [ ] **Step 3: Implement non-blocking warmup**

Add a reusable HTTP client to the real uploader struct in `eh_client/src/telegraph.rs`:

```rust
pub struct IpfS3Uploader {
    bucket: Box<Bucket>,
    config: ResolvedIpfS3UploaderConfig,
    http: reqwest::Client,
}
```

Initialize it in `IpfS3Uploader::from_config()` with a short timeout, for example `reqwest::Client::builder().timeout(std::time::Duration::from_secs(10)).build()?` mapped into `Error::Other` on construction failure.

After extracting each CID in the existing `ImageUploader for IpfS3Uploader::upload_images()` implementation:

```rust
if self.config.warm_public_gateway_after_upload {
    let url = format!("{}/{}", self.config.gateway_url, cid);
    let http = self.http.clone();
    tokio::spawn(async move {
        if let Err(e) = http.head(&url).send().await {
            tracing::debug!("IPFS gateway warmup failed for {}: {}", url, e);
        }
    });
}
```

Do not await the spawned task in production upload flow. Do not fail upload if HEAD returns 4xx/5xx.

- [ ] **Step 4: Verify**

Run:

```powershell
cargo test -p eh_client ipfs3
cargo fmt --all -- --check
git diff --check
```

## Task 2: Slow resumable download background retry

**Files:**
- Modify: `eh_client/src/client.rs`
- Modify: `src/db/entities/eh_download_queue.rs`
- Create: migration file for same-table background retry columns
- Modify: `src/db/repo/eh_download_queue.rs`
- Modify: `src/scheduler/eh_engine.rs`
- Modify: `src/main.rs`

- [ ] **Step 1: Track download progress metrics**

Extend archive download result with progress metadata:

```rust
pub struct ArchiveDownloadOutcome {
    pub bytes_written: u64,
    pub elapsed: std::time::Duration,
    pub resumed_from: u64,
}
```

Keep existing `download_archive_with_request()` wrapper returning `u64`; add `download_archive_with_request_outcome()` for scheduler use.

- [ ] **Step 2: Add slow-failure decision test**

Add pure function near download worker:

```rust
fn should_schedule_background_download(failures: i32, bytes_delta: u64, elapsed: std::time::Duration) -> bool {
    failures > 3 && elapsed.as_secs() > 0 && bytes_delta / elapsed.as_secs() < 1024 * 1024
}
```

Tests:

```rust
assert!(should_schedule_background_download(4, 2 * 1024 * 1024, Duration::from_secs(5)));
assert!(!should_schedule_background_download(3, 2 * 1024 * 1024, Duration::from_secs(5)));
assert!(!should_schedule_background_download(4, 10 * 1024 * 1024, Duration::from_secs(5)));
```

- [ ] **Step 3: Persist background status**

Add same-table DB columns to `eh_download_queue` to identify entries being retried in the background. Do not create a companion table for this feature:

```text
background_download_status TEXT NULL              -- pending or running
background_download_started_at TIMESTAMP NULL
background_download_next_retry_at TIMESTAMP NULL
background_download_attempt_count INTEGER NOT NULL DEFAULT 0
background_download_error TEXT NULL
```

Use only two non-null background states: `pending` and `running`. If background attempts are exhausted, set the main row `status = failed`, set `error`, and clear all background columns; do not leave `background_download_status = 'failed'`.

The main row remains in the normal EH queue table. `get_next_for_download()` must exclude rows where `background_download_status IN ('pending', 'running')`. Background claim must additionally require the main row `status = 'pending'`; rows in `downloading`, `downloaded`, `uploading`, `uploaded`, `publishing`, `done`, `failed`, or `canceled` are not claimable by the background worker.

- [ ] **Step 4: Implement background worker**

Add config fields to `EhentaiConfig`:

```rust
#[serde(default = "default_eh_background_download_enabled")]
pub background_download_enabled: bool,
#[serde(default = "default_eh_background_download_concurrency")]
pub background_download_concurrency: usize,
#[serde(default = "default_eh_background_download_max_attempts")]
pub background_download_max_attempts: u8,
#[serde(default = "default_eh_background_download_stale_sec")]
pub background_download_stale_sec: u64,
```

Defaults: enabled `true`, concurrency `2`, max attempts `6`, stale seconds `3600`.

Add `EhBackgroundDownloadWorker` with configurable concurrency. It atomically claims rows with main `status = 'pending'`, `background_download_status = 'pending'`, and due `background_download_next_retry_at`, updates them to `background_download_status = 'running'`, and runs the same artifact download into the same expected ZIP path. Multiple background tasks may run at once, but the claim method must guard by row id, main status, and background status so only one background attempt can own a row.

On success, atomically mark the row `downloaded` only with guards `id = row.id`, main `status = 'pending'`, and `background_download_status = 'running'`, then clear background columns. On failure, store `background_download_error`, increment `background_download_attempt_count`, set `background_download_status = 'pending'` with backoff if attempts remain, or mark the row `failed` and clear background columns if attempts are exhausted.

If either success or failure update affects zero rows, re-read the row. If the main status is no longer `pending` or background status is no longer `running`, clear background columns only when the row is terminal/canceled or no longer needs background work; never overwrite a row that another worker advanced to `downloaded`, `uploading`, `uploaded`, `publishing`, `done`, `failed`, or `canceled`.

On startup, add `reset_stale_background_downloads(stale_sec)` to move `running` rows older than the threshold back to `pending` and clear `background_download_started_at`. Spawn the worker from `src/main.rs` only when EH is enabled and `background_download_enabled` is true.

- [ ] **Step 5: Schedule background work from main download failures**

Define the failure metrics contract in `eh_client/src/client.rs` before scheduling:

```rust
pub struct ArchiveDownloadProgress {
    pub attempts: usize,
    pub bytes_delta: u64,
    pub elapsed: std::time::Duration,
}

pub enum ArchiveDownloadResult {
    Complete(ArchiveDownloadOutcome),
    Failed { error: Error, progress: ArchiveDownloadProgress },
}
```

`download_archive_with_request_outcome()` returns `Result<ArchiveDownloadOutcome>` for existing callers, and an internal helper returns `ArchiveDownloadResult` so `EhDownloadWorker` can inspect progress when the operation fails. `bytes_delta` is the increase in `.zip.part` length during the failed high-level download call, not total file size. `elapsed` covers the same high-level call. `attempts` is the number of internal resumable GET attempts used during that call.

In `EhDownloadWorker::tick()`, first check `self.config.background_download_enabled`. If it is false, keep the existing retry/fail behavior and never set `background_download_status`.

When the feature is enabled and a download failure occurs with `entry.retry_count + 1 > 3`, compute average speed from `progress.bytes_delta / progress.elapsed`. If it is below 1 MiB/s, hand off to background instead of calling the normal retry helper for that failure:

1. Atomically set the row back to `status = 'pending'` from the claimed `downloading` status.
2. Set `background_download_status = 'pending'`, `background_download_next_retry_at = now`, clear prior background error, and set `next_retry_at = null`.
3. Do not increment the normal `retry_count` for this handoff and do not call `schedule_eh_retry_from()` for the same error.

This avoids the default `max_retry_count = 3` path immediately marking the row `failed`. Because `get_next_for_download()` excludes background-owned rows, the normal queue moves on to other entries while the background worker handles this one.

- [ ] **Step 6: Verify**

Run:

```powershell
cargo test -p pixivbot background_download
cargo test -p pixivbot test_download_worker_failure_schedules_retry
cargo test -p pixivbot test_get_next_for_download_skips_background_owned_rows
cargo fmt --all -- --check
git diff --check
$env:RUSTFLAGS = "-Dwarnings"; cargo clippy -p pixivbot --all-targets -- -D warnings
cargo check -p pixivbot --all-targets
```

## Plan self-review

- Spec coverage: Covers non-blocking IPFS cache warmup and independent background retries for slow repeated archive failures.
- Placeholder scan: No TBD/TODO placeholders remain.
- Type consistency: Uses explicit config flag and background worker concepts consistently.

## Execution handoff

Plan complete and saved to `docs/superpowers/plans/2026-07-07-ipfs-cache-warmup-and-slow-download.md`.

Implement IPFS warmup and slow background downloads as separate commits because they are independent fixes.
