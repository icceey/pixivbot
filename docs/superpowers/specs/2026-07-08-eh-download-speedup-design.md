# EH Archive Download Speedup Design

## Goal

Speed up EH archive downloads by decoupling network reads from disk writes via a buffered stream, and replace the single hard-coded 300s total timeout with layered connect/read timeouts.

## Background

EH archive downloads run at ~78 KB/s in production while the same URL downloads at ~300 KB/s via browser/curl — a 4x gap matching a ~25% duty cycle.

Root cause: `download_archive_response_once` (client.rs L610-616) reads a chunk from the socket then awaits `file.write_all()` before reading the next chunk. `tokio::fs::File::write_all` dispatches to the blocking thread pool; during that await nobody reads the socket. The kernel TCP receive buffer fills, the receive window shrinks to zero, and EH pauses sending. When the write completes, the window reopens but requires an RTT for the server to resume — stop-and-go throughput.

## Non-Goals

- **Chunked parallel download**: EH supports HTTP Range, but multi-connection downloads risk IP bans (high RPS), may be rejected (one-time links), and may not help if EH applies a per-IP total rate cap. Community downloaders all parallelize *images*, not archive byte-ranges. Skip for now.
- **Configurable timeouts**: connect/read timeouts are hard-coded constants, not exposed in `EhentaiConfig`.
- **Application-layer speed monitoring**: `read_timeout(60s)` detects complete stalls. If EH sends data at 1 KB/s, each read succeeds and resets the timer — but this is an edge case, and the existing `made_progress` mechanism handles it at attempt boundaries. No `SpeedMonitoringReader` wrapper.
- **Changes to scheduler logic**: `EhDownloadWorker::tick()`, `defer_eh_download`, `schedule_eh_retry_from`, `DownloadInProgress` detection — all unchanged.

## Design

### 1. Stream Feature + Buffered Copy

**Problem**: Manual `while let Some(chunk) = resp.chunk().await? { file.write_all(&chunk).await?; }` serializes read and write. Each `write_all` blocks the read loop.

**Solution**: Use `resp.bytes_stream()` + `tokio_util::io::StreamReader` + `tokio::io::copy_buf` + `tokio::io::BufWriter(2MB)`.

```rust
use futures_util::StreamExt;  // for .map_err on Stream
use tokio_util::io::StreamReader;

// In download_archive_response_once, replace L610-616:

let file = options.open(temp_path).await?;
let mut writer = tokio::io::BufWriter::with_capacity(2 * 1024 * 1024, file);

// Convert reqwest stream → AsyncRead, mapping errors to io::Error
let stream = resp.bytes_stream()
    .map(|r| r.map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e)));
let mut reader = StreamReader::new(stream);

// copy_buf internally loops read_buf → write_buf.
// Writes to BufWriter are memcpy until 2MB buffer fills, then one bulk flush.
// During flush (~10ms for 2MB), the socket is briefly not read, but
// the 2MB buffer represents ~7s of download at 300KB/s — negligible duty cycle impact.
let copied = tokio::io::copy_buf(&mut reader, &mut writer).await?;
writer.flush().await?;
let written = if append { existing_len + copied } else { copied };
```

**Why this fixes the bottleneck**: `copy_buf` reads from `StreamReader` (backed by the socket) and writes to `BufWriter`. When `BufWriter` has space, `write_all` is a memory copy (no await on disk IO). The read loop runs continuously, keeping the TCP receive window open. Only when 2MB accumulates does a flush happen, and that flush takes ~10ms vs ~7s to fill the next 2MB — 99.9% duty cycle.

**Append mode**: When `append == true`, `BufWriter` wraps a file opened with `.append(true)`. `copy_buf` writes stream data after the existing content. `written = existing_len + copied`.

**Truncate mode**: When `append == false`, file is opened with `.truncate(true)`. `written = copied`.

**Cargo.toml change**:
```toml
reqwest = { version = "0.12.28", default-features = false, features = ["json", "rustls-tls", "multipart", "stream"] }
```

The `stream` feature pulls in `tokio-util` (provides `StreamReader`) and `futures-util` (provides `StreamExt`) automatically. No additional dependencies needed.

**Imports added to client.rs**:
```rust
use futures_util::StreamExt;
use tokio_util::io::StreamReader;
```

`AsyncWriteExt` import (L7) is still needed for `flush()`.

### 2. Layered Timeouts

**Problem**: `ARCHIVE_DOWNLOAD_TIMEOUT_SECS = 300` (L11) is a single total timeout applied via request-level `.timeout(300s)` (L558). It covers connect + header + body. A slow-but-progressing download hits 300s and gets cut off even if making progress.

**Solution**: Remove the total timeout. Use reqwest's `connect_timeout` and `read_timeout` on the `ClientBuilder`:

```rust
// In EhClient::new (L196-205):
let mut builder = reqwest::Client::builder()
    .user_agent(USER_AGENT_STR)
    .connect_timeout(std::time::Duration::from_secs(30))
    .read_timeout(std::time::Duration::from_secs(60));
```

**Constants**:
```rust
const ARCHIVE_CONNECT_TIMEOUT_SECS: u64 = 30;
const ARCHIVE_READ_TIMEOUT_SECS: u64 = 60;
```

**Remove**:
- `ARCHIVE_DOWNLOAD_TIMEOUT_SECS` constant (L11)
- `.timeout(60s)` on ClientBuilder (L198) — replaced by connect + read timeout
- `.timeout(ARCHIVE_DOWNLOAD_TIMEOUT_SECS)` on request builder (L558-560)

**Behavior**:
- `connect_timeout(30s)`: TCP + TLS handshake. If EH is unreachable, fails in 30s.
- `read_timeout(60s)`: Each read operation. If no data arrives for 60s (server stall, connection dropped), fails. Resets after each successful read. A slow-but-progressing download (1 KB/s) won't trigger this — each read succeeds within 60s.
- No total timeout: a 200MB archive at 300KB/s takes ~10 min. Without a total timeout, the download runs to completion as long as data keeps flowing.

**Impact on `made_progress`**: If `read_timeout` triggers (60s stall), the attempt ends with an `Http` error. `made_progress` checks bytes/elapsed — if the stall happened after some progress, `had_progress` may still be true, leading to `DownloadInProgress` → `defer_eh_download`. This is correct behavior.

## Affected Files

| File | Changes |
|---|---|
| `eh_client/Cargo.toml` | Add `"stream"` to reqwest features |
| `eh_client/src/client.rs` | Delete `ARCHIVE_DOWNLOAD_TIMEOUT_SECS`; add connect/read timeout constants + ClientBuilder config; replace chunk loop with `bytes_stream()` + `StreamReader` + `copy_buf` + `BufWriter(2MB)`; add imports |

## Testing

1. **Existing tests pass**: `test_download_archive_restarts_after_invalid_partial_on_416`, `test_download_archive_rejects_incomplete_content_range`, `test_download_archive_returns_download_in_progress_when_fast_partial`, `test_download_archive_returns_plain_error_when_no_progress` — all use wiremock and exercise the download path. The stream + BufWriter change is transparent to these tests (same wiremock setup, same assertions).
2. **Existing scheduler tests pass**: `test_download_worker_downloads_archive`, `test_download_worker_failure_schedules_retry`, `test_download_worker_permanent_failure_cleans_partial_archive`, `test_download_worker_progress_failure_defers_without_retry`.
3. **CI gate**: `cargo fmt --check` + `cargo clippy -- -D warnings` + `cargo test --workspace --all-targets` + `cargo build --release`.

## Risks

- **`bytes_stream()` error mapping**: `reqwest::Error` → `io::Error` conversion via `io::Error::new(Other, e)`. `StreamReader` requires `Item = Result<B, E>` where `E: Into<io::Error>`. The `.map_err` closure handles this. Loss of error detail is acceptable — the error is still logged via `tracing::warn!` in `download_archive_response_resumable`.
- **`copy_buf` and append mode**: `copy_buf` writes to the `BufWriter`, which writes to the file. With `.append(true)`, writes go to end of file. No seeking needed. `written = existing_len + copied` tracks total.
- **BufWriter flush on error**: If `copy_buf` returns Err, the `BufWriter` may have unflushed data. The `writer.flush().await?` after `copy_buf` ensures data is written. If `copy_buf` errors, we skip flush — but the `.part` file retains whatever was flushed so far, which is correct for resumption.
- **No total timeout means a stuck-but-not-stalled download could run indefinitely**: If EH sends 1 byte every 59s (just under read_timeout), the download never times out but makes no real progress. This is an extreme edge case. The `made_progress` check at attempt end catches it if the attempt eventually fails. If it never fails... the download never completes. Accepted risk — EH doesn't exhibit this behavior in practice.
