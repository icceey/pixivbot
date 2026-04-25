# Notifier Module

图片、媒体组和 ugoira 发送模块，负责把调度器或命令处理器准备好的资源通过 Telegram 推送出去。

此文件作用域覆盖 `src/bot/notifier.rs` 以及 `src/bot/notifier/` 目录下的子模块。

## 模块结构

```
src/bot/notifier.rs          # Notifier 结构体、公开 API、ThrottledBot 类型和 re-export
src/bot/notifier/batch.rs    # process_batch_send(): 下载 -> 分批 -> 发送 (单图/多图)
src/bot/notifier/caption.rs  # CaptionStrategy, shared/individual batch caption 生成
src/bot/notifier/media.rs    # send_media_batch(), send_photo_file_with_id(), send_animation_file()
src/bot/notifier/numbering.rs # ContinuationNumbering: 续传批次编号
src/bot/notifier/button.rs   # DownloadButtonConfig: Pixiv/Booru 下载按钮构建
src/bot/notifier/result.rs   # BatchSendResult: 发送结果追踪
src/bot/notifier/ugoira.rs   # ugoira ZIP -> MP4 后作为 animation 发送
```

## 责任边界

- Notifier 负责下载、Telegram API 发送、caption 应用、按钮应用、spoiler 应用和结果汇总。
- 调度状态、重试策略、订阅进度和消息记录属于 `src/scheduler`；不要把这些决策移动到 notifier。
- `Notifier` 持有 `ThrottledBot` 和 `Arc<Downloader>`；不要在这里新增手写 Telegram rate-limit sleep。
- 用户可见错误提示通常由调用方负责；notifier 内部失败用 `tracing` 记录并通过 `BatchSendResult` 返回。

## 关键不变量

### 所有发送路径必须经过 caption 函数

`batch.rs` 中的 `process_batch_send()` 有两条路径：

- **单图路径** (`total == 1`): 调用 `send_single_image()`
- **多图路径** (`total > 1`): 调用 `send_media_batch()`

**两条路径都必须通过 `caption.rs` 的 `shared_batch_caption()` / `individual_batch_caption()` 生成最终 caption。**

绝对不能在单图路径中直接使用原始 caption 绕过续传编号逻辑。否则当调度器重试推送"只剩最后 1 张图"时，会发送完整原文 caption 而非 `\(continued N/M\)` 格式，导致与多图重试行为不一致。

共享文案只应出现在每批第一张图；独立文案用于榜单等每张图不同 caption 的场景。

### ContinuationNumbering 流转

```
scheduler/helpers.rs        →  notifier (公开 API)        →  batch.rs
构建 ContinuationNumbering      传入 process_batch_send()       单图: shared_batch_caption()
(first_batch, total_batches)                                    多图: send_media_batch() → caption.rs
```

- `first_batch_number == 1` 且 `batch_idx == 0` → 使用原始 caption（首次发送首批）
- 其他情况 → 生成 `\(continued N/M\)` 替代原始 caption

`ContinuationNumbering::for_item_count()` 和 scheduler 侧的批次计算都依赖同一个媒体组上限；修改时必须同步检查。

### MAX_PER_GROUP = 10

Telegram 媒体组上限，定义在 `utils/caption.rs`。`numbering.rs` 和 `scheduler/helpers.rs` 都依赖此常量计算批次编号，三处必须一致。

### BatchSendResult 语义

- `succeeded_indices` / `failed_indices` 是本次 attempted URL 列表的索引，不是原始作品页码；scheduler 会映射回真实页码。
- `first_message_id` 是本次成功发送中第一条 Telegram message id，用于消息记录和后续引用；只有全部失败时才应为 `None`。
- `BatchSendResult::all_failed(total)` 必须标记 `0..total` 全部失败，调度器依赖它判断 complete failure。

### 下载按钮

- `DownloadButtonConfig` 支持 Pixiv 和 Booru callback data；格式分别由 `DOWNLOAD_CALLBACK_PREFIX` 和 `BOORU_DOWNLOAD_CALLBACK_PREFIX` 控制。
- Channel chat 不显示下载按钮；保持 `for_pixiv_chat()` / `for_booru_chat()` 的 channel 分支行为。
- `notify_with_individual_captions_and_button()` 接收按钮配置是为了 API 一致性，榜单推送通常仍使用默认无按钮配置。

### Ugoira

- `notify_ugoira()` 走 `download_ugoira_mp4()`，再通过 `send_animation_file()` 发送 MP4 animation。
- ugoira 是单项发送，成功结果应为 `succeeded_indices = [0]` 且携带 `first_message_id`。
- 保持 ugoira 的 caption、spoiler 和下载按钮处理与普通单图路径一致。

## 修改建议

- 发送行为变化优先补小型单元测试；现有测试覆盖续传 caption、下载按钮和 `BatchSendResult` 基本语义。
- Caption 字符串使用 MarkdownV2，测试会断言精确字符串；改标点或转义也算行为变化。
- 修改批次拆分、结果索引、`first_message_id` 或 continuation 逻辑时，同时检查 `src/scheduler/helpers.rs` 的映射和持久化状态。
- 新增 Telegram 发送方法时，确保支持 `has_spoiler`、MarkdownV2 caption、必要的 reply markup，并返回可追踪 message id。
