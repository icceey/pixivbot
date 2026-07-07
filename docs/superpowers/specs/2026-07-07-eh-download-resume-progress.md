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
- 不引入 `thiserror`：`error.rs` 当前为手写 `impl fmt::Display` + `impl std::error::Error`，新增 `DownloadInProgress` 沿用此风格
- 不改 `EhUploadWorker` / `EhPublishWorker` 的 retry 语义：它们不涉及 `.part` 文件断点续传（upload/publish 阶段无传输速度判定需求）

## 设计

### 1. 新增 `Error::DownloadInProgress` 变体

`eh_client/src/error.rs`（当前为手写 `impl fmt::Display` L21-36 + `impl std::error::Error` L38-47，无 thiserror；新增变体沿用此风格）：

```rust
pub enum Error {
    // ... 既有变体 ...
    /// 归档包下载失败但本次 attempt 有实质进展（>10KB/s），应保留 .part 文件供下次续传
    /// 而非累计 retry_count
    DownloadInProgress {
        inner: Box<Error>,
    },
}
```

**手写 Display 分支**（在 L21-36 的 `match self` 中新增分支）：

```rust
Error::DownloadInProgress { inner } => {
    write!(f, "download failed but made progress: {}", inner)
}
```

**手写 source() 分支**（在 L38-47 的 `match self` 中新增分支，替代 `_ => None` 兜底）：

```rust
Error::DownloadInProgress { inner } => Some(inner.as_ref()),
```

- 字段名 `inner`（非 `source`），避免与 `std::error::Error::source()` trait 方法混淆
- `source()` 返回 `Some(self.inner.as_ref())`，保持错误链完整，使 anyhow `chain().find_map()` 能命中
- Display 文案包含 `inner` 错误信息，保留诊断价值
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

`download_archive_response_resumable` 改造（相对原代码的差异：新增 `before_len`/`start` 计时、`had_progress` 跟踪、attempt 间 1s sleep 避免 burst、最终错误包装）：

```rust
// 新增导入（use 区顶部）
use std::time::{Duration, Instant};
use tokio::time::sleep;
// tokio::fs 已在文件其他位置导入或可直接引用

pub async fn download_archive_response_resumable(
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

        match self.download_archive_response_once(download_url, temp_path).await {
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

- `sleep(Duration::from_secs(1))` 是新增：避免 4 次 attempt 全部立即失败时形成请求 burst；1s 足够短，不影响断点续传的快速重试。最坏情况 4 次全部立即失败（如 403/404）会耗时 ~3s，阻塞当前 worker tick（每 tick 只处理 1 个 entry），可接受
- 方法签名是 `&self` 方法（与原代码一致），spec 第 1 节伪代码漏了 `&self`，以此处为准
- `tracing::warn!` 输出本次 attempt 的 `new_bytes` / `elapsed` / `attempt_made_progress` 以及累积 `had_progress`，便于诊断哪一次 attempt 有进展
- `before_len` 读失败 fallback 到 0 是为了不阻塞下载，宁可误判为有进展也不放弃续传
- `after_len` 读失败 fallback 到 `before_len`（`new_bytes=0`），保守判定为无进展，测量失败不应误判为有进展
- **边界**：若服务器返回 200 拒绝续传（`download_archive_response_once` L508-534 会删 `.part` 并 truncate 重建），本次 attempt 的 `new_bytes` 测量可能为 0 或负值（saturating 到 0），即使本次实际下载了大量数据。此场景下 attempt 内部已丢弃旧 `.part`（重新下载），从"续传"语义看本就不算续传进展；下次 attempt 仍能正常累计

### 3. Scheduler 检测与 defer 路径

`EhDownloadWorker::tick()` 中 `process()` 失败时（替换原 L609-626 的 `if let Err(e)` 块）：

```rust
if let Err(e) = self.process(&entry).await {
    error!("Download failed for entry {}: {:#}", entry.id, e);

    // process() 用 .context() 包装 anyhow，downcast_ref 只检查最外层会 miss；
    // 必须用 chain().find_map() 遍历错误链
    let is_in_progress = e
        .chain()
        .find_map(|c| c.downcast_ref::<eh_client::Error>())
        .map(|client_err| matches!(client_err, eh_client::Error::DownloadInProgress { .. }))
        .unwrap_or(false);

    if is_in_progress {
        // 传输有实质进展：不累计 retry_count，保留 .part 文件供下次续传
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
            // Delete partial ZIP if it exists — 不可恢复的失败才删
            self.cleanup_zip(&entry).await;
        }
    }
}
```

关键点：
- `DownloadInProgress` 路径**不调 `cleanup_zip`**——保留 `.part` 文件供下次续传
- `defer_eh_download` 已存在（process L654 用于 chat 不可通知场景），复用它，retry_count 不变；其 CAS guard 要求当前状态为 `STATUS_DOWNLOADING`，而 `get_next_for_download` 正是将状态翻转为 `STATUS_DOWNLOADING`，匹配
- 原有 `schedule_eh_retry_from` permanent 时仍调 `cleanup_zip`（清掉不可恢复的损坏文件）
- **既有边界**：`defer_eh_download` CAS 失败（`rows_affected != 1`）会 `bail!`，entry 卡在 `STATUS_DOWNLOADING` 直到重启时 `reset_stale_eh_downloads` 修复。这是既有 defer 路径通病（process L654 也有同样问题），本次设计未引入新风险
- **既有边界**：`validate_complete_zip`（`download_archive_with_request` L443 调用）若判定 `.part` 完整但 ZIP 损坏会 `remove_file` 删掉 `.part`。这是既有行为，非本次设计引入；`DownloadInProgress` 路径在 `download_archive_response_resumable` 返回 `Err` 时不会走到 `validate_complete_zip`（L440 `?` 直接传播），下次 tick 若 `.part` 已完整会先 `Ok(())` 再 `validate_complete_zip`

### 4. 测试策略

1. **纯函数单元测试**（必做）：锁定 `made_progress` 阈值语义：
   - `new_bytes=10240, elapsed=1.0` → `false`（恰好 10KB/s，不算）
   - `new_bytes=10241, elapsed=1.0` → `true`
   - `new_bytes=0, elapsed=1.0` → `false`
   - `elapsed=0.0` → `false`（防除零）

2. **downcast 链路验证测试**（必做，**实施第一步**）：验证 `DownloadInProgress` 经 anyhow `.context()` 包装后 `chain().find_map()` 仍能命中。这是整个设计核心检测机制，必须先验证再写其他代码：

   ```rust
   #[test]
   fn download_in_progress_downcasts_through_anyhow_context() {
       let inner = eh_client::Error::Other("simulated".into());
       let err = eh_client::Error::DownloadInProgress { inner: Box::new(inner) };
       // 模拟 process() 中 .context("...") 包装
       let wrapped: anyhow::Error = err.context("Failed to download archive");
       let found = wrapped
           .chain()
           .find_map(|c| c.downcast_ref::<eh_client::Error>())
           .map(|e| matches!(e, eh_client::Error::DownloadInProgress { .. }))
           .unwrap_or(false);
       assert!(found);
   }
   ```

3. **eh_client 集成测试**（必做，wiremock）：模拟 HTTP 206 + 写入 >10KB 后连接断开，验证返回 `DownloadInProgress`；模拟写入 <10KB 后断开，验证返回原始错误

4. **scheduler 测试**（必做，wiremock）：用 wiremock 让 `download_archive_with_request` 的 archiver.php 返回成功重定向、下载 URL 返回 206 写入 >10KB 后断开连接，触发 `DownloadInProgress`；然后断言：
   - DB 中 `retry_count` 不变
   - `status == STATUS_PENDING`
   - `next_retry_at` 被设置
   - `.part` 文件仍存在（未调 `cleanup_zip`）
   - 对照组（写入 <10KB 后断开）`retry_count` +1

## 影响范围

| 文件 | 改动 |
|---|---|
| `eh_client/src/error.rs` | 新增 `DownloadInProgress` 变体 + `source()` 实现 |
| `eh_client/src/client.rs` | 新增 `made_progress` 纯函数 + 改造 `download_archive_response_resumable` |
| `src/scheduler/eh_engine.rs` | `tick()` 中 `process()` 失败时检测 `DownloadInProgress` 并 defer |

## 风险与缓解

- **风险**：`.part` 文件大小差值可能因文件系统缓存延迟不准确。
  **缓解**：tokio::fs 在写入后 flush，metadata 读取应反映实际大小；差值只用于 >10KB/s 阈值判定，小误差可接受。`before_len` 读失败 fallback 到 0 是为了不阻塞下载，宁可误判为有进展也不放弃续传；`after_len` 读失败 fallback 到 `before_len`（`new_bytes=0`）保守判定为无进展。
- **风险**：网络持续慢速（如 9KB/s）会无限 defer 不累计 retry_count。
  **缓解**：这是预期行为——慢速有进展即应续传；若需限制可后续引入超时上限，但当前 YAGNI。**副作用**：若大量 entry 同时进入此状态，会占用 worker tick 资源（每 tick 只处理 1 个 entry）；可接受，因慢速 defer 本身罕见。
- **风险**：`DownloadInProgress` 错误经 anyhow 传播后 downcast 失败。
  **缓解**：`process()` 用 `.context()` 包装 anyhow，`downcast_ref` 只检查最外层会 miss。**必须用 `e.chain().find_map(|c| c.downcast_ref::<eh_client::Error>())` 遍历错误链**。已在第 3 节代码示例体现，且第 4 节测试策略第 2 项要求作为实施第一步验证此 downcast 链路。
- **风险**：`defer_eh_download` CAS 失败时 entry 卡在 `STATUS_DOWNLOADING`。
  **缓解**：既有通病（process L654 既有 defer 路径同样有此问题），由 `reset_stale_eh_downloads` 在重启时修复。本次设计未引入新风险。
- **既有边界**（非本次设计引入）：`validate_complete_zip` 若判定 `.part` 完整但 ZIP 损坏会 `remove_file` 删掉 `.part`。`DownloadInProgress` 路径在 `download_archive_response_resumable` 返回 `Err` 时不会走到 `validate_complete_zip`（L440 `?` 直接传播），下次 tick 若 `.part` 已完整会先 `Ok(())` 再 `validate_complete_zip`。
