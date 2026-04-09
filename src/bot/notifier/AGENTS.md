# Notifier Module

图片批量发送模块，负责将 Pixiv 作品图片通过 Telegram 推送给订阅者。

## 模块结构

```
notifier/
├── mod.rs          # Notifier 结构体, 公开 API
├── batch.rs        # process_batch_send(): 下载 → 分批 → 发送 (单图/多图)
├── caption.rs      # CaptionStrategy, shared/individual batch caption 生成
├── media.rs        # send_media_batch(): InputMedia 构建 + API 调用
├── numbering.rs    # ContinuationNumbering: 续传批次编号
├── button.rs       # DownloadButtonConfig: 下载按钮构建
├── result.rs       # BatchSendResult: 发送结果追踪
└── ugoira.rs       # 动图 (ugoira) MP4 发送
```

## 关键不变量

### 所有发送路径必须经过 caption 函数

`batch.rs` 中的 `process_batch_send()` 有两条路径：

- **单图路径** (`total == 1`): 调用 `send_single_image()`
- **多图路径** (`total > 1`): 调用 `send_media_batch()`

**两条路径都必须通过 `caption.rs` 的 `shared_batch_caption()` / `individual_batch_caption()` 生成最终 caption。**

绝对不能在单图路径中直接使用原始 caption 绕过续传编号逻辑。否则当调度器重试推送"只剩最后 1 张图"时，会发送完整原文 caption 而非 `\(continued N/M\)` 格式，导致与多图重试行为不一致。

### ContinuationNumbering 流转

```
scheduler/helpers.rs        →  notifier (公开 API)        →  batch.rs
构建 ContinuationNumbering      传入 process_batch_send()       单图: shared_batch_caption()
(first_batch, total_batches)                                    多图: send_media_batch() → caption.rs
```

- `first_batch_number == 1` 且 `batch_idx == 0` → 使用原始 caption（首次发送首批）
- 其他情况 → 生成 `\(continued N/M\)` 替代原始 caption

### MAX_PER_GROUP = 10

Telegram 媒体组上限，定义在 `utils/caption.rs`。`numbering.rs` 和 `scheduler/helpers.rs` 都依赖此常量计算批次编号，三处必须一致。
