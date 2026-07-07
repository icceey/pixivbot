# EH 归档包下载断点续传速度判定

## 背景与目标

EH 归档包下载采用 `eh_client` 内部多 attempt 重试（`ARCHIVE_DOWNLOAD_MAX_ATTEMPTS = 4`）+ `.part` 文件断点续传。当前所有失败都经 `schedule_eh_retry_from` 累计 `retry_count`，当 `retry_count > max_retry_count`（默认 3）时判定为永久失败并删除 `.zip`/`.zip.part`。

问题：网络波动导致的可恢复中断（如连接断开但已写入大量数据）会快速耗尽 `retry_count`，并可能因 `cleanup_zip` 删除已有进展的 `.part` 文件。

目标：当某次 attempt 传输速度 >10KB/s（即有实质进展）时，最终失败不计入 `retry_count`，并保留 `.part` 文件供下次续传。

## 非目标

- 不改 `ARCHIVE_DOWNLOAD_TIMEOUT_SECS`（300s）或 `ARCHIVE_DOWNLOAD_MAX_ATTEMPTS`（4）
- 不改 `max_retry_count` 默认值（3）
- 不改 `.part` 文件路径或命名约定
- 不引入配置项控制阈值（硬编码 10KB/s）

## 设计

### 1. 新增 `Error::DownloadInProgress` 变体

`eh_client/src/error.rs`：

```rust
#[error("download failed but made progress")]
DownloadInProgress {
    inner: Box<Error>,
},
```

- 字段名 `inner`（非 `source`），避免与 `std::error::Error::source()` trait 方法混淆
- `source()` 实现补充此变体返回 `Some(self.inner.as_ref())`，保持错误链完整
- scheduler 只需二值判定（有进展/无进展），不需要速度数值

### 2. 速度测量逻辑

**决策：不改 `download_archive_response_once`**。改为在 `download_archive_response_resumable` 中通过 `.part` 文件大小差值 + `Instant` 计时来测量。

理由：
- `download_archive_response_once` 内部 `?` 传播逻辑不变，零侵入
- 即使 attempt 中途崩溃，文件系统上 `.part` 大小仍反映已写入字节
- 测量逻辑集中在一处

阈值函数提取为纯函数便于测试：

```rust
fn made_progress(new_bytes: u64, elapsed_secs: f64) -> bool {
    elapsed_secs > 0.0 && (new_bytes as f64 / elapsed_secs) > 10_240.0
}
```

- 10KB/s = 10240 bytes/s
- `>` 严格大于，恰好 10KB/s 不算进展
- `elapsed_secs == 0.0` 返回 `false`（防除零）

`download_archive_response_resumable` 改造：

```rust
pub async fn download_archive_response_resumable(
    download_url: &str,
    temp_path: &Path,
) -> Result<(), Error> {
    let mut had_progress = false;
    let mut last_error: Option<Error> = None;

    for _ in 1..=ARCHIVE_DOWNLOAD_MAX_ATTEMPTS {
        let before_len = tokio::fs::metadata(temp_path)
            .await
            .map(|m| m.len())
            .unwrap_or(0);
        let start = Instant::now();

        match download_archive_response_once(download_url, temp_path).await {
            Ok(()) => return Ok(()),
            Err(e) => {
                let elapsed = start.elapsed().as_secs_f64();
                let after_len = tokio::fs::metadata(temp_path)
                    .await
                    .map(|m| m.len())
                    .unwrap_or(before_len);
                let new_bytes = after_len.saturating_sub(before_len);
                if made_progress(new_bytes, elapsed) {
                    had_progress = true;
                }
                last_error = Some(e);
                sleep(Duration::from_secs(1)).await;
            }
        }
    }

    match last_error {
        Some(e) if had_progress => Err(Error::DownloadInProgress { inner: Box::new(e) }),
        Some(e) => Err(e),
        None => Err(Error::Other("no attempts made".to_string())),
    }
}
```

### 3. Scheduler 检测与 defer 路径

`EhDownloadWorker::tick()` 中 `process()` 失败时：

```rust
let is_in_progress = e
    .downcast_ref::<eh_client::Error>()
    .map(|client_err| matches!(client_err, eh_client::Error::DownloadInProgress { .. }))
    .unwrap_or(false);

if is_in_progress {
    // 不累计 retry_count，保留 .part 文件供续传
    defer_eh_download(id, STATUS_PENDING, download_poll_interval_sec);
} else {
    schedule_eh_retry_from(
        id,
        &entry.status,
        STATUS_PENDING,
        &e.to_string(),
        max_retry_count,
    );
    // permanent 时仍调 cleanup_zip
}
```

关键点：
- `DownloadInProgress` 路径**不调 `cleanup_zip`**——保留 `.part` 文件供下次续传
- `defer_eh_download` 已存在（process L654 用于 chat 不可通知场景），复用它，retry_count 不变
- 原有 `schedule_eh_retry_from` permanent 时仍调 `cleanup_zip`（清掉不可恢复的损坏文件）

### 4. 测试策略

1. **纯函数单元测试**（必做）：锁定 `made_progress` 阈值语义：
   - `new_bytes=10240, elapsed=1.0` → `false`（恰好 10KB/s，不算）
   - `new_bytes=10241, elapsed=1.0` → `true`
   - `new_bytes=0, elapsed=1.0` → `false`
   - `elapsed=0.0` → `false`（防除零）

2. **eh_client 集成测试**（必做，wiremock）：模拟 HTTP 206 + 写入 >10KB 后连接断开，验证返回 `DownloadInProgress`；模拟写入 <10KB 后断开，验证返回原始错误

3. **scheduler 测试**（必做）：构造 `DownloadInProgress` 错误，验证 `retry_count` 不变（defer 路径）vs 非 progress 错误 `retry_count` +1（retry 路径）

## 影响范围

| 文件 | 改动 |
|---|---|
| `eh_client/src/error.rs` | 新增 `DownloadInProgress` 变体 + `source()` 实现 |
| `eh_client/src/client.rs` | 新增 `made_progress` 纯函数 + 改造 `download_archive_response_resumable` |
| `src/scheduler/eh_engine.rs` | `tick()` 中 `process()` 失败时检测 `DownloadInProgress` 并 defer |

## 风险与缓解

- **风险**：`.part` 文件大小差值可能因文件系统缓存延迟不准确。
  **缓解**：tokio::fs 在写入后 flush，metadata 读取应反映实际大小；差值只用于 >10KB/s 阈值判定，小误差可接受。
- **风险**：网络持续慢速（如 9KB/s）会无限 defer 不累计 retry_count。
  **缓解**：这是预期行为——慢速有进展即应续传；若需限制可后续引入超时上限，但当前 YAGNI。
- **风险**：`DownloadInProgress` 错误经 anyhow 传播后 downcast 失败。
  **缓解**：`process()` 用 `.context()` 包装 anyhow，但底层 `eh_client::Error` 仍在错误链中，downcast_ref 应能命中。
