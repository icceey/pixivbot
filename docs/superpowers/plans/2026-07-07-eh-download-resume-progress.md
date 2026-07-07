# EH Download Resume Progress Implementation Plan

> **For agentic workers:** Use the subagent-driven-development skill to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** When an EH archive download attempt transfers data at >10KB/s but ultimately fails, do not increment `retry_count` and preserve the `.part` file for resumption.

**Architecture:** Add an `Error::DownloadInProgress` variant to `eh_client` that wraps the last failure error. Measure per-attempt progress in `download_archive_response_resumable` via `.part` file size delta + `Instant` timing. In `EhDownloadWorker::tick()`, detect `DownloadInProgress` through the anyhow error chain and call `defer_eh_download` (no retry increment, keep `.part`) instead of `schedule_eh_retry_from` (retry increment, cleanup on permanent).

**Tech Stack:** Rust 1.94, tokio, reqwest, wiremock 0.6 (dev), anyhow (pixivbot crate only)

**Global Constraints:**
- Do not change `ARCHIVE_DOWNLOAD_TIMEOUT_SECS` (300s) or `ARCHIVE_DOWNLOAD_MAX_ATTEMPTS` (4)
- Do not change `max_retry_count` default (3)
- Do not change `.part` file path or naming conventions
- Do not introduce config item for the threshold (hardcode 10KB/s = 10240 bytes/s)
- Do not introduce `thiserror` — `error.rs` uses handwritten `impl fmt::Display` + `impl std::error::Error`
- Do not change `EhUploadWorker` / `EhPublishWorker` retry semantics

---

## File Structure

| File | Responsibility |
|---|---|
| `eh_client/src/error.rs` | Add `DownloadInProgress` variant + `Display` + `source()` implementations |
| `eh_client/src/client.rs` | Add `made_progress` pure function + refactor `download_archive_response_resumable` to measure progress and wrap errors |
| `src/scheduler/eh_engine.rs` | Modify `EhDownloadWorker::tick()` to detect `DownloadInProgress` and defer instead of schedule-retry |

---

### Task 1: Add `DownloadInProgress` error variant + `made_progress` pure function + verify downcast chain

This task establishes the core building blocks: the error variant, the speed-threshold pure function, and a test proving the downcast chain works through anyhow `.context()` wrapping. The downcast verification is the **spec-mandated first step** — if it fails, the entire design is invalid.

**Files:**
- Modify: `eh_client/src/error.rs:3-47` (add variant + Display + source branches)
- Modify: `eh_client/src/client.rs:10-11` (add `made_progress` function after constants)
- Test: `eh_client/src/client.rs:1029-1091` (add `made_progress` unit tests in existing `#[cfg(test)] mod tests`)
- Test: `src/scheduler/eh_engine.rs` (add downcast verification test in `integration_tests` module — needs `anyhow` which `eh_client` crate lacks)

**Interfaces:**
- Produces: `eh_client::Error::DownloadInProgress { inner: Box<Error> }` — used by Task 2 to wrap final error
- Produces: `fn made_progress(new_bytes: u64, elapsed_secs: f64) -> bool` — used by Task 2 in `download_archive_response_resumable`

- [ ] **Step 1: Add `DownloadInProgress` variant to `Error` enum**

In `eh_client/src/error.rs`, add the new variant after `Other(String)` (before the closing `}` at line 19):

```rust
    Other(String),
    /// Archive download failed but this attempt made real progress (>10KB/s).
    /// Preserve `.part` file for resumption instead of incrementing retry_count.
    DownloadInProgress {
        inner: Box<Error>,
    },
```

- [ ] **Step 2: Add `Display` branch for `DownloadInProgress`**

In `eh_client/src/error.rs`, in the `impl fmt::Display` match block (L23-35), add a new arm before the closing `}` of the match. The `Error::Other(msg)` arm at L33 is currently the last arm. Add after it:

```rust
            Error::Other(msg) => write!(f, "{}", msg),
            Error::DownloadInProgress { inner } => {
                write!(f, "download failed but made progress: {}", inner)
            }
```

- [ ] **Step 3: Add `source()` branch for `DownloadInProgress`**

In `eh_client/src/error.rs`, in the `impl std::error::Error` match block (L40-45), replace the `_ => None` catch-all with explicit branches. Change:

```rust
            Error::Http(e) => Some(e),
            Error::Json(e) => Some(e),
            Error::Io(e) => Some(e),
            _ => None,
```

to:

```rust
            Error::Http(e) => Some(e),
            Error::Json(e) => Some(e),
            Error::Io(e) => Some(e),
            Error::DownloadInProgress { inner } => Some(inner.as_ref()),
            _ => None,
```

This ensures the error chain is complete so `anyhow::Error::chain()` can traverse into the inner error.

- [ ] **Step 4: Add `made_progress` pure function**

In `eh_client/src/client.rs`, add after the constants (after line 10, before `pub struct EhClient`):

```rust
/// Threshold for "made progress": strictly greater than 10 KiB/s.
const PROGRESS_THRESHOLD_BYTES_PER_SEC: f64 = 10_240.0;

/// Returns true if the attempt transferred data fast enough to count as real progress.
/// - 10 KiB/s = 10240 bytes/s; strictly greater than (not equal to).
/// - `elapsed_secs == 0.0` returns false (prevents division by zero).
fn made_progress(new_bytes: u64, elapsed_secs: f64) -> bool {
    elapsed_secs > 0.0 && (new_bytes as f64 / elapsed_secs) > PROGRESS_THRESHOLD_BYTES_PER_SEC
}
```

- [ ] **Step 5: Add `made_progress` unit tests**

In `eh_client/src/client.rs`, in the `#[cfg(test)] mod tests` block (starts at L1029, has `use super::*` at L1031), add these tests before the closing `}` at L1091:

```rust
    #[test]
    fn test_made_progress_threshold() {
        // Exactly 10KB/s (10240 bytes in 1.0s) → NOT progress (strictly greater)
        assert!(!made_progress(10240, 1.0));
        // One byte above threshold → progress
        assert!(made_progress(10241, 1.0));
        // Zero bytes → never progress
        assert!(!made_progress(0, 1.0));
        // Zero elapsed → false (prevents division by zero)
        assert!(!made_progress(99999, 0.0));
        // Large transfer, small elapsed → progress
        assert!(made_progress(20000, 0.5));
        // Small transfer, large elapsed → no progress
        assert!(!made_progress(100, 10.0));
    }
```

- [ ] **Step 6: Run `made_progress` tests to verify they pass**

Run: `cargo test -p eh_client --lib made_progress`
Expected: 1 passed

- [ ] **Step 7: Add downcast chain verification test**

This is the spec-mandated **first verification step**. It proves `DownloadInProgress` survives anyhow `.context()` wrapping and can be found via `chain().find_map()`.

**Important correction from spec:** The spec's test code calls `err.context("...")` on a bare `eh_client::Error`, but `anyhow::Context` is only implemented on `Result<T, E>`, not on bare `E`. The test must wrap the error in `Err(...)` first.

In `src/scheduler/eh_engine.rs`, in the `#[cfg(test)] mod integration_tests` block, add a new test. Place it right before the `// === Download Worker Tests ===` comment (search for that exact comment). The `integration_tests` module has `use super::*` which brings `anyhow::Context` into scope (from L9 `use anyhow::{Context, Result}`):

```rust
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
        let wrapped: anyhow::Error = result
            .context("Failed to download archive")
            .unwrap_err();

        let found = wrapped
            .chain()
            .find_map(|c| c.downcast_ref::<eh_client::Error>())
            .map(|e| matches!(e, eh_client::Error::DownloadInProgress { .. }))
            .unwrap_or(false);
        assert!(found, "DownloadInProgress must be findable through anyhow error chain");
    }
```

- [ ] **Step 8: Run downcast test to verify it passes**

Run: `cargo test -p pixivbot --lib download_in_progress_downcasts_through_anyhow_context`
Expected: 1 passed

If this test fails, STOP — the entire design premise is broken. Debug before proceeding.

- [ ] **Step 9: Run full fmt + clippy + check**

Run: `cargo fmt --all -- --check`
Expected: no output (passes)

Run: `$env:RUSTFLAGS="-Dwarnings"; cargo clippy --workspace --all-targets -- -D warnings`
Expected: no warnings

Run: `cargo check --workspace --all-targets`
Expected: success

- [ ] **Step 10: Commit**

```powershell
git add eh_client/src/error.rs eh_client/src/client.rs src/scheduler/eh_engine.rs
git commit -m "feat(eh_client): add DownloadInProgress error variant and made_progress threshold

- New Error::DownloadInProgress { inner: Box<Error> } wraps last failure
  when an attempt made real progress (>10KB/s).
- Handwritten Display and source() branches maintain error chain integrity.
- Pure function made_progress(new_bytes, elapsed_secs) for testable threshold.
- Downcast-through-anyhow-context test verifies scheduler detection path."
```

---

### Task 2: Refactor `download_archive_response_resumable` to track progress and wrap errors

This task modifies the retry loop to measure per-attempt data transfer speed, track whether any attempt made progress, and wrap the final error in `DownloadInProgress` when progress was detected.

**Files:**
- Modify: `eh_client/src/client.rs:1-6` (add imports)
- Modify: `eh_client/src/client.rs:459-483` (rewrite `download_archive_response_resumable`)
- Test: `eh_client/tests/integration.rs` (add wiremock integration tests)

**Interfaces:**
- Consumes: `eh_client::Error::DownloadInProgress` from Task 1
- Consumes: `fn made_progress(new_bytes: u64, elapsed_secs: f64) -> bool` from Task 1
- Produces: `download_archive_response_resumable` now returns `Err(Error::DownloadInProgress { inner })` when any attempt made progress

- [ ] **Step 1: Add imports for `Duration`, `Instant`, and `sleep`**

In `eh_client/src/client.rs`, the current imports (L1-6) are:

```rust
use crate::error::{Error, Result};
use crate::models::{EhCookies, EhGallery, EhGalleryRef, RawApiResponse, RawGalleryMetaEntry};
use crate::parser;
use reqwest::header::{CONTENT_LENGTH, CONTENT_RANGE, COOKIE, RANGE};
use std::path::Path;
use tokio::io::AsyncWriteExt;
```

Add `std::time::{Duration, Instant}` and `tokio::time::sleep` after the existing imports:

```rust
use crate::error::{Error, Result};
use crate::models::{EhCookies, EhGallery, EhGalleryRef, RawApiResponse, RawGalleryMetaEntry};
use crate::parser;
use reqwest::header::{CONTENT_LENGTH, CONTENT_RANGE, COOKIE, RANGE};
use std::path::Path;
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;
use tokio::time::sleep;
```

- [ ] **Step 2: Rewrite `download_archive_response_resumable`**

In `eh_client/src/client.rs`, replace the entire `download_archive_response_resumable` method (L459-483) with:

```rust
    async fn download_archive_response_resumable(
        &self,
        download_url: &str,
        temp_path: &Path,
    ) -> Result<()> {
        let mut had_progress = false;
        let mut last_error: Option<Error> = None;

        for attempt in 1..=ARCHIVE_DOWNLOAD_MAX_ATTEMPTS {
            let before_len = tokio::fs::metadata(temp_path)
                .await
                .map(|m| m.len())
                .unwrap_or(0);
            let start = Instant::now();

            match self
                .download_archive_response_once(download_url, temp_path)
                .await
            {
                Ok(()) => return Ok(()),
                Err(e) => {
                    let elapsed = start.elapsed().as_secs_f64();
                    let after_len = tokio::fs::metadata(temp_path)
                        .await
                        .map(|m| m.len())
                        .unwrap_or(before_len);
                    let new_bytes = after_len.saturating_sub(before_len);
                    let attempt_made_progress = made_progress(new_bytes, elapsed);
                    if attempt_made_progress {
                        had_progress = true;
                    }
                    tracing::warn!(
                        attempt,
                        max_attempts = ARCHIVE_DOWNLOAD_MAX_ATTEMPTS,
                        new_bytes,
                        elapsed_secs = elapsed,
                        attempt_made_progress,
                        had_progress,
                        error = %e,
                        "archive download attempt failed",
                    );
                    last_error = Some(e);
                    // Sleep between attempts to avoid request burst on immediate failures.
                    // Worst case: 4 immediate failures ≈ 3s extra, acceptable for a single worker tick.
                    sleep(Duration::from_secs(1)).await;
                }
            }
        }

        match last_error {
            Some(e) if had_progress => Err(Error::DownloadInProgress { inner: Box::new(e) }),
            Some(e) => Err(e),
            None => Err(Error::Other("archive download failed".into())),
        }
    }
```

Key differences from the original:
- Added `before_len` / `start` measurement before each attempt
- Added `after_len` / `new_bytes` / `attempt_made_progress` / `had_progress` tracking after each failure
- Replaced interpolated-string `warn!` with structured fields
- Added `sleep(Duration::from_secs(1))` between attempts
- Final error is wrapped in `DownloadInProgress` when `had_progress` is true

- [ ] **Step 3: Verify existing `test_download_archive_rejects_incomplete_content_range` still passes**

This existing test (in `eh_client/tests/integration.rs` L545-598) uses `.expect(4)` on the mock — 4 attempts are expected. With the new 1s sleep, the test takes ~3s longer but should still pass. The error returned will be a plain `Error::Other` (not `DownloadInProgress`) because `new_bytes=11` (the `rest` body is 11 bytes) is well below the 10240 threshold.

Run: `cargo test -p eh_client --test integration test_download_archive_rejects_incomplete_content_range`
Expected: 1 passed (may take ~3-4s due to inter-attempt sleep)

- [ ] **Step 4: Add wiremock integration test — progress detected → `DownloadInProgress`**

In `eh_client/tests/integration.rs`, add a new test after `test_download_archive_rejects_incomplete_content_range` (after L598). This test simulates a 206 response with a valid Content-Range claiming the full file, but a body smaller than claimed (>10KB), causing `written < expected_total` error after writing >10KB:

```rust
#[tokio::test]
async fn test_download_archive_returns_download_in_progress_when_fast_partial() {
    let server = MockServer::start().await;
    let redirect_html = format!(
        r#"<script>document.location = "{}/archive/123456/abcdef0123/abcdef0123/0?autostart=1";</script>"#,
        server.uri()
    );

    Mock::given(method("POST"))
        .and(path("/archiver.php"))
        .respond_with(ResponseTemplate::new(200).set_body_string(redirect_html))
        .expect(1)
        .mount(&server)
        .await;

    // Content-Range claims total=40000, end=39999, so end+1==total → validate_content_range
    // returns Ok(40000). But body is only 20000 bytes → written(20000) < expected_total(40000)
    // → Error::Other("archive download ended at 20000 bytes, expected 40000").
    // new_bytes=20000, elapsed likely <0.1s locally → made_progress=true → DownloadInProgress.
    let partial_body = vec![0u8; 20000];
    Mock::given(method("GET"))
        .and(path("/archive/123456/abcdef0123/abcdef0123/0"))
        .respond_with(
            ResponseTemplate::new(206)
                .insert_header("Content-Range", "bytes 0-39999/40000")
                .set_body_bytes(partial_body.clone()),
        )
        // 4 attempts (ARCHIVE_DOWNLOAD_MAX_ATTEMPTS)
        .expect(4)
        .mount(&server)
        .await;

    let client = client_at(&server);
    let temp_dir = tempfile::tempdir().unwrap();
    let dest = temp_dir.path().join("archive.zip");
    let part = dest.with_extension("zip.part");

    let err = client
        .download_archive(123456, "abcdef0123", "123456--abc123def456", "780x", &dest)
        .await
        .expect_err("fast partial download should fail after 4 attempts");

    assert!(
        matches!(err, eh_client::Error::DownloadInProgress { .. }),
        "expected DownloadInProgress, got: {:?}",
        err
    );
    // .part file should be preserved for resumption
    assert!(part.exists(), ".part file should be preserved for resumption");
    assert!(!dest.exists(), "final dest should not exist on failure");
}
```

- [ ] **Step 5: Run the new test**

Run: `cargo test -p eh_client --test integration test_download_archive_returns_download_in_progress_when_fast_partial`
Expected: 1 passed (~3-4s due to inter-attempt sleep)

- [ ] **Step 6: Add wiremock integration test — no progress → plain error**

In `eh_client/tests/integration.rs`, add after the previous test. This test simulates a 206 response with an invalid Content-Range (end+1 != total), which fails in `validate_content_range` before any bytes are written, so `new_bytes=0` → `made_progress=false` → plain error (not `DownloadInProgress`):

```rust
#[tokio::test]
async fn test_download_archive_returns_plain_error_when_no_progress() {
    let server = MockServer::start().await;
    let redirect_html = format!(
        r#"<script>document.location = "{}/archive/123456/abcdef0123/abcdef0123/0?autostart=1";</script>"#,
        server.uri()
    );

    Mock::given(method("POST"))
        .and(path("/archiver.php"))
        .respond_with(ResponseTemplate::new(200).set_body_string(redirect_html))
        .expect(1)
        .mount(&server)
        .await;

    // Content-Range: end=26, total=127, end+1=27 != 127 → validate_content_range fails
    // BEFORE any bytes are written → new_bytes=0 → made_progress=false → plain Error.
    let rest = b"zip_content";
    Mock::given(method("GET"))
        .and(path("/archive/123456/abcdef0123/abcdef0123/0"))
        .respond_with(
            ResponseTemplate::new(206)
                .insert_header(
                    "Content-Range",
                    "bytes 0-26/127",
                )
                .insert_header("Content-Length", rest.len().to_string())
                .set_body_bytes(rest.to_vec()),
        )
        .expect(4)
        .mount(&server)
        .await;

    let client = client_at(&server);
    let temp_dir = tempfile::tempdir().unwrap();
    let dest = temp_dir.path().join("archive.zip");

    let err = client
        .download_archive(123456, "abcdef0123", "123456--abc123def456", "780x", &dest)
        .await
        .expect_err("no-progress download should fail");

    assert!(
        !matches!(err, eh_client::Error::DownloadInProgress { .. }),
        "should NOT be DownloadInProgress when no progress was made, got: {:?}",
        err
    );
    assert!(
        err.to_string().contains("Content-Range ended"),
        "should contain Content-Range error message, got: {}",
        err
    );
}
```

- [ ] **Step 7: Run the new test**

Run: `cargo test -p eh_client --test integration test_download_archive_returns_plain_error_when_no_progress`
Expected: 1 passed (~3-4s due to inter-attempt sleep)

- [ ] **Step 8: Run all eh_client tests**

Run: `cargo test -p eh_client`
Expected: all passed

- [ ] **Step 9: Run fmt + clippy + check**

Run: `cargo fmt --all -- --check`
Expected: no output

Run: `$env:RUSTFLAGS="-Dwarnings"; cargo clippy --workspace --all-targets -- -D warnings`
Expected: no warnings

Run: `cargo check --workspace --all-targets`
Expected: success

- [ ] **Step 10: Commit**

```powershell
git add eh_client/src/client.rs eh_client/tests/integration.rs
git commit -m "feat(eh_client): track download progress and wrap fast failures

download_archive_response_resumable now measures per-attempt transfer
speed via .part file size delta + Instant timing. When any attempt
exceeds 10KB/s, the final error is wrapped in DownloadInProgress to
signal the scheduler to defer (not increment retry_count).

- Structured tracing::warn! with attempt/new_bytes/elapsed/had_progress
- 1s inter-attempt sleep to avoid request burst
- Wiremock tests verify DownloadInProgress on fast partial and plain
  error on no-progress failures"
```

---

### Task 3: Modify `EhDownloadWorker::tick()` to detect `DownloadInProgress` and defer

This task modifies the scheduler to route `DownloadInProgress` errors to `defer_eh_download` (no retry increment, preserve `.part`) instead of `schedule_eh_retry_from` (retry increment, cleanup on permanent).

**Files:**
- Modify: `src/scheduler/eh_engine.rs:609-626` (rewrite `if let Err(e)` block in `tick()`)
- Test: `src/scheduler/eh_engine.rs` (add scheduler wiremock tests in `integration_tests` module)

**Interfaces:**
- Consumes: `eh_client::Error::DownloadInProgress` from Task 1
- Consumes: `download_archive_response_resumable` returning `DownloadInProgress` from Task 2
- Consumes: `defer_eh_download(id, target_status, delay_secs)` — existing repo method (L654 in `process()`)

- [ ] **Step 1: Rewrite the `if let Err(e)` block in `tick()`**

In `src/scheduler/eh_engine.rs`, replace the block at L609-626. The current code is:

```rust
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
```

Replace with:

```rust
        if let Err(e) = self.process(&entry).await {
            error!("Download failed for entry {}: {:#}", entry.id, e);

            // process() wraps errors with .context(); downcast_ref only checks the
            // outermost layer. Must traverse the error chain to find eh_client::Error.
            let is_in_progress = e
                .chain()
                .find_map(|c| c.downcast_ref::<eh_client::Error>())
                .map(|client_err| {
                    matches!(client_err, eh_client::Error::DownloadInProgress { .. })
                })
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
```

- [ ] **Step 2: Verify existing download worker tests still pass**

The existing tests `test_download_worker_failure_schedules_retry` and `test_download_worker_permanent_failure_cleans_partial_archive` use POST `/archiver.php` returning 500, which fails before any download bytes are written → `new_bytes=0` → `made_progress=false` → plain error (not `DownloadInProgress`) → `schedule_eh_retry_from` path. These should still pass unchanged.

Run: `cargo test -p pixivbot --lib test_download_worker_failure_schedules_retry`
Expected: 1 passed

Run: `cargo test -p pixivbot --lib test_download_worker_permanent_failure_cleans_partial_archive`
Expected: 1 passed

- [ ] **Step 3: Add scheduler test — `DownloadInProgress` defers without incrementing retry_count**

In `src/scheduler/eh_engine.rs`, in the `integration_tests` module, add a new test after `test_download_worker_permanent_failure_cleans_partial_archive` (after L2494). This test uses the full mock chain (gallery page → archiver POST → download URL returning 206 partial with >10KB body):

```rust
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

        // 206 with valid Content-Range (end+1==total → validate_content_range passes)
        // but body smaller than claimed (>10KB) → written < expected_total → error
        // after writing >10KB → made_progress=true → DownloadInProgress
        let partial_body = vec![0u8; 20000];
        Mock::given(method("GET"))
            .and(path("/archive/123456/token/0"))
            .respond_with(
                ResponseTemplate::new(206)
                    .insert_header("Content-Range", "bytes 0-39999/40000")
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

        // .part file should be preserved for resumption
        let eh_cache = temp.path().join("eh_cache");
        let part_path = eh_cache.join("123456_abcdef0123.zip.part");
        assert!(
            part_path.exists(),
            ".part file should be preserved for resumption"
        );
        let part_size = std::fs::metadata(&part_path).unwrap().len();
        assert_eq!(
            part_size, 20000,
            ".part should contain the 20000 bytes written across attempts"
        );
    }
```

- [ ] **Step 4: Run the new scheduler test**

Run: `cargo test -p pixivbot --lib test_download_worker_progress_failure_defers_without_retry`
Expected: 1 passed (~3-4s due to inter-attempt sleep)

- [ ] **Step 5: Run all eh_engine tests**

Run: `cargo test -p pixivbot --lib eh_engine`
Expected: all passed

- [ ] **Step 6: Run fmt + clippy + check + test + build**

Run: `cargo fmt --all -- --check`
Expected: no output

Run: `$env:RUSTFLAGS="-Dwarnings"; cargo clippy --workspace --all-targets -- -D warnings`
Expected: no warnings

Run: `cargo check --workspace --all-targets`
Expected: success

Run: `cargo test --workspace --all-targets`
Expected: all passed

Run: `cargo build --release --workspace`
Expected: success

- [ ] **Step 7: Commit**

```powershell
git add src/scheduler/eh_engine.rs
git commit -m "feat(scheduler): defer EH download on progress instead of retry

EhDownloadWorker::tick() now detects DownloadInProgress through the
anyhow error chain and calls defer_eh_download (no retry_count
increment, .part preserved) instead of schedule_eh_retry_from.

- chain().find_map() traverses .context() wrapping
- Non-DownloadInProgress errors use the original schedule+cleanup path
- Scheduler wiremock test verifies retry_count=0 and .part preserved"
```
