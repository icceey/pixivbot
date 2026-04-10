# Booru 多站点订阅支持 — 实施计划

## 概述

为 PixivBot 添加可配置的多站点 Booru 图站订阅支持。优先实现 Moebooru（yande.re / konachan），后续扩展 Danbooru / Gelbooru。

### 用户需求

1. 订阅 booru 标签 — 空标签 = 所有帖子
2. `score:>=N` / `favcount:>=N` 过滤（混合策略：API 支持时用 API，否则本地过滤）
3. 订阅指定 pool ID，追踪新增帖子
4. 多站点通过 TOML `[[booru.sites]]` 配置
5. 自建 API 客户端（不用现有 crate）
6. 空标签订阅：队列制，不丢弃
7. Rating：集成现有 `blur_sensitive_tags` + 独立 per-subscription 过滤
8. 图片：Telegram 发 sample_url，下载缓存原图
9. Bot 消息语言：中文

---

## Phase 1: `booru_client` 工作区 crate

**目标**：创建独立的 Booru API 客户端 crate，遵循 `pixiv_client/` 的结构。

### 文件结构

```
booru_client/
├── Cargo.toml
├── src/
│   ├── lib.rs          # 公开 API 导出
│   ├── error.rs        # BooruError 错误类型
│   ├── models.rs       # BooruPost, BooruPool 统一模型
│   ├── client.rs       # BooruClient 实现（按站点类型分发）
│   └── engine_type.rs  # BooruEngineType 枚举 (Moebooru/Danbooru/Gelbooru)
```

### 数据模型 (`models.rs`)

```rust
/// 统一的 Booru 帖子模型（从各引擎 JSON 规范化）
pub struct BooruPost {
    pub id: u64,
    pub tags: String,              // 空格分隔的标签字符串
    pub score: i32,
    pub fav_count: i32,
    pub file_url: Option<String>,  // 原图（可能被站点限制）
    pub sample_url: Option<String>,// 中等尺寸（用于 Telegram 推送）
    pub preview_url: Option<String>,
    pub rating: BooruRating,       // Safe/Questionable/Explicit/General
    pub width: u32,
    pub height: u32,
    pub md5: Option<String>,       // 用于缓存去重
    pub source: Option<String>,
    pub created_at: Option<DateTime<Utc>>,
    pub file_size: Option<u64>,
    pub file_ext: Option<String>,
    pub status: Option<String>,    // active/deleted/flagged
}

pub enum BooruRating {
    General,     // Danbooru 'g'
    Safe,        // Moebooru 's'
    Questionable,// 'q'
    Explicit,    // 'e'
}

pub struct BooruPoolInfo {
    pub id: u64,
    pub name: String,
    pub post_count: u32,
    pub post_ids: Vec<u64>,
    pub description: Option<String>,
    pub created_at: Option<DateTime<Utc>>,
}
```

### 引擎类型 (`engine_type.rs`)

```rust
pub enum BooruEngineType {
    Moebooru,   // yande.re, konachan.com
    Danbooru,   // danbooru.donmai.us
    Gelbooru,   // gelbooru.com
}
```

### 客户端 (`client.rs`)

单一 `BooruClient` struct，通过 `BooruEngineType` 枚举分发不同的 URL 构造和响应解析逻辑。**不使用 trait 多态。**

```rust
pub struct BooruClient {
    http: reqwest::Client,
    base_url: String,
    engine_type: BooruEngineType,
    api_key: Option<String>,
    login: Option<String>,
    user_agent: String,
}

impl BooruClient {
    pub async fn get_posts(&self, tags: &str, limit: u32, since_id: Option<u64>) -> Result<Vec<BooruPost>>;
    pub async fn get_pool(&self, pool_id: u64) -> Result<BooruPoolInfo>;
    pub async fn get_pool_posts(&self, pool_id: u64, page: u32) -> Result<Vec<BooruPost>>;
}
```

**API 差异分发（以 get_posts 为例）**：

| 引擎 | 端点 | 分页参数 | tags 字段映射 | md5 字段 |
|---|---|---|---|---|
| Moebooru | `/post.json` | `page` (1-indexed) | `tags` | `md5` |
| Danbooru | `/posts.json` | `page` (1-indexed) | `tag_string` | `md5` |
| Gelbooru | `/index.php?page=dapi&s=post&q=index&json=1` | `pid` (0-indexed) | `tags` | `hash` |

`since_id` 统一通过在 tags 中追加 `id:>N` 实现（三个引擎均支持）。

### Cargo.toml

```toml
[package]
name = "booru_client"
version.workspace = true
edition.workspace = true

[dependencies]
anyhow = "1.0.102"
chrono = { version = "0.4.44", features = ["serde"] }
reqwest = { version = "0.12.28", default-features = false, features = ["json", "rustls-tls"] }
serde = { version = "1.0.228", features = ["derive"] }
serde_json = "1.0.149"
tracing = "0.1.44"
```

### QA 场景

**QA-1.1: Moebooru JSON 反序列化**
- 工具: `cargo test -p booru_client`
- 步骤: 在 `booru_client/src/models.rs` 的 `#[cfg(test)]` 中编写测试，使用 yande.re 真实 JSON 响应 fixture（硬编码 JSON 字符串）
- 预期: `serde_json::from_str::<Vec<MoebooruRawPost>>(fixture)` 成功，字段值与 fixture 一致
- 覆盖: id, tags, score, fav_count, file_url, sample_url, rating, md5, created_at

**QA-1.2: BooruRating 解析**
- 工具: `cargo test -p booru_client -- rating`
- 步骤: 测试 `BooruRating::from_moebooru("s")`, `from_moebooru("q")`, `from_moebooru("e")` 及各引擎格式
- 预期: `Safe`, `Questionable`, `Explicit` 对应正确；未知值返回 `Safe` 默认

**QA-1.3: 缺失字段容错**
- 工具: `cargo test -p booru_client -- missing`
- 步骤: 测试 JSON 中缺少 `file_url`、`source`、`md5` 字段的反序列化
- 预期: 对应字段为 `None`，不 panic

**QA-1.4: URL 构造**
- 工具: `cargo test -p booru_client -- url`
- 步骤: 测试 `BooruClient::build_posts_url("landscape", Some(12345))` 对 Moebooru 引擎
- 预期: URL 为 `https://yande.re/post.json?tags=landscape+id:>12345&limit=100`

**验证**: `make ci` 通过（需先将 `booru_client` 加入 workspace members 和 root crate dependencies）。

---

## Phase 2: 数据库类型扩展 + Migration

### TaskType 扩展 (`src/db/types/task_type.rs`)

```rust
#[sea_orm(string_value = "booru_tag")]
BooruTag,
#[sea_orm(string_value = "booru_pool")]
BooruPool,
```

TaskType 是 `String(20)` 存储，新增枚举值无需 DB migration。需更新 `Display` impl。

### SubscriptionState 扩展 (`src/db/types/state.rs`)

```rust
BooruTag(BooruTagState),
BooruPool(BooruPoolState),

pub struct BooruTagState {
    /// 已从 API 获取到的最新帖子 ID（用于 since_id 轮询）
    pub latest_post_id: u64,
    /// 推送队列：存储完整帖子数据，确保不丢弃
    /// 每次轮询将所有新帖加入队列尾部，每 tick 从头部取 max_posts_per_poll 个推送
    /// 只有队列为空时才执行下一次 API 拉取
    pub pending_queue: Vec<QueuedBooruPost>,
    /// 当前正在重试的推送（发送失败时记录）
    pub pending_post: Option<PendingBooruPost>,
}

/// 队列中缓存的完整帖子数据（避免只存 ID 后无法重新获取的问题）
pub struct QueuedBooruPost {
    pub id: u64,
    pub sample_url: Option<String>,
    pub file_url: Option<String>,
    pub tags: String,
    pub score: i32,
    pub fav_count: i32,
    pub rating: String, // "s"/"q"/"e"/"g" — 序列化友好
    pub source: Option<String>,
    pub md5: Option<String>,
}

pub struct BooruPoolState {
    pub known_post_ids: Vec<u64>,
    pub pending_post: Option<PendingBooruPost>,
}

pub struct PendingBooruPost {
    pub post_id: u64,
    pub retry_count: u8,
}
```

由于 `SubscriptionState` 使用 `#[serde(tag = "type", content = "state")]`，新增变体天然向后兼容（已有的 `Author` / `Ranking` JSON 仍可反序列化）。

### BooruFilter 新类型 (`src/db/types/booru_filter.rs`)

```rust
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, FromJsonQueryResult)]
pub struct BooruFilter {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_score: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_favcount: Option<i32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_ratings: Vec<String>,  // ["s", "q"] 等
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude_tags: Vec<String>,
}

impl BooruFilter {
    pub fn matches(&self, post: &BooruPost) -> bool;
    pub fn to_api_tags(&self, engine: BooruEngineType) -> String; // 将数值过滤器编码为 API 查询标签
    pub fn parse_from_args(args: &[&str]) -> Self;
    pub fn format_for_display(&self) -> String;
    pub fn is_empty(&self) -> bool;
}
```

`to_api_tags()` 方法实现混合过滤策略：
- Moebooru: `score:>=N` 可在 API 级别使用（经测试支持），favcount 本地过滤
- Danbooru: `score:>=N` 和 `favcount:>=N` 都可作为 API 标签（注意 2 标签限制）
- Gelbooru: `score:>=N` 可用，favcount 本地过滤

### tasks.value 编码约定

- `BooruTag` task: `"site_name::tag_query"` — 例 `"yandere::landscape"`, `"danbooru::"`（空标签）
- `BooruPool` task: `"site_name::pool_id"` — 例 `"yandere::12345"`

解析工具函数:

```rust
pub fn parse_booru_task_value(value: &str) -> Option<(&str, &str)> {
    value.split_once("::")
}
```

唯一索引 `(type, value)` 天然保证每个站点+标签/池组合只有一个 task。

### Migration

新增 `booru_filter` nullable JSON 列到 `subscriptions` 表：

```rust
// migration/src/m2026XXXX_add_booru_filter.rs
manager.alter_table(
    Table::alter()
        .table(Subscriptions::Table)
        .add_column(ColumnDef::new(Subscriptions::BooruFilter).json())
        .to_owned(),
).await?;
```

entity `subscriptions.rs` 新增字段:

```rust
pub booru_filter: Option<BooruFilter>,
```

### 匹配穷举性

添加 `TaskType` / `SubscriptionState` 新变体后，需更新所有 match 分支。用 `ast_grep_search` 和 `lsp_diagnostics` 定位。

### QA 场景

**QA-2.1: TaskType 新变体 string_value**
- 工具: `cargo test -p pixivbot -- task_type`
- 步骤: 在 `src/db/types/task_type.rs` 测试模块中验证 `TaskType::BooruTag` 的 `to_value()` 和 `try_from_value("booru_tag")`
- 预期: 双向转换一致，Display 输出为 `"booru_tag"`

**QA-2.2: SubscriptionState 向后兼容**
- 工具: `cargo test -p pixivbot -- subscription_state`
- 步骤: 反序列化已有 `AuthorState` JSON `{"type":"Author","state":{"latest_illust_id":123}}`
- 预期: 成功反序列化为 `SubscriptionState::Author(AuthorState { latest_illust_id: 123, ... })`
- 追加: 反序列化新 `BooruTag` JSON 同样成功

**QA-2.3: BooruFilter::matches()**
- 工具: `cargo test -p pixivbot -- booru_filter`
- 步骤: 构造 `BooruFilter { min_score: Some(50), .. }` 和 score=30 / score=80 的 mock BooruPost
- 预期: score=30 返回 false，score=80 返回 true

**QA-2.4: BooruFilter::to_api_tags() 混合策略**
- 工具: `cargo test -p pixivbot -- to_api_tags`
- 步骤: `BooruFilter { min_score: Some(100), min_favcount: Some(10) }` 对 Moebooru 引擎
- 预期: 返回 `"score:>=100"`（score 作为 API 标签），favcount 不包含（Moebooru 不支持，本地过滤）

**QA-2.5: parse_booru_task_value() 边界**
- 工具: `cargo test -p pixivbot -- parse_booru_task`
- 步骤: 测试 `"yandere::landscape"`, `"danbooru::"`, `"yandere::tag::with::colons"`, `"nocolon"`
- 预期: 分别返回 `Some(("yandere","landscape"))`, `Some(("danbooru",""))`, `Some(("yandere","tag::with::colons"))`, `None`

**QA-2.6: Migration 正确性**
- 工具: `cargo test -p pixivbot -- migration` + 手动验证
- 步骤: 在空 DB 上运行 `migration::Migrator::up(&db, None).await`，然后检查 `subscriptions` 表有 `booru_filter` 列
- 预期: 列存在，类型为 JSON nullable，不影响已有数据

**QA-2.7: match 穷举性**
- 工具: `lsp_diagnostics` 对 `src/db/types/`, `src/scheduler/`, `src/bot/`
- 步骤: 添加新 TaskType/SubscriptionState 变体后，运行诊断
- 预期: 0 errors（所有 match 分支已补齐）

**验证**: `make ci` 通过。

---

## Phase 3: 配置层

### Config 结构 (`src/config.rs`)

```rust
#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    // ... 现有字段
    #[serde(default)]
    pub booru: BooruConfig,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct BooruConfig {
    #[serde(default)]
    pub sites: Vec<BooruSiteConfig>,
    /// Booru 轮询间隔（秒），默认 1800（30分钟）
    #[serde(default = "default_booru_poll_interval_sec")]
    pub poll_interval_sec: u64,
    /// 每次轮询最多推送帖子数，默认 20
    #[serde(default = "default_booru_max_posts_per_poll")]
    pub max_posts_per_poll: u32,
}

#[derive(Debug, Deserialize, Clone)]
pub struct BooruSiteConfig {
    pub name: String,               // 唯一标识 (e.g., "yandere", "konachan", "danbooru")
    pub base_url: String,           // e.g., "https://yande.re"
    pub engine_type: BooruEngineType, // moebooru / danbooru / gelbooru
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub login: Option<String>,
    #[serde(default)]
    pub password_hash: Option<String>,
}
```

### config.toml.example 新增

```toml
# Booru 图站订阅（可选）
# [[booru.sites]]
# name = "yandere"
# base_url = "https://yande.re"
# engine_type = "moebooru"
#
# [[booru.sites]]
# name = "konachan"
# base_url = "https://konachan.com"
# engine_type = "moebooru"
#
# [booru]
# poll_interval_sec = 1800
# max_posts_per_poll = 20
```

### QA 场景

**QA-3.1: 无 booru 配置时默认行为**
- 工具: `cargo test -p pixivbot -- config`
- 步骤: 测试无 `[booru]` section 的 TOML 反序列化
- 预期: `config.booru.sites` 为空 Vec，`poll_interval_sec` 为 1800

**QA-3.2: 多站点 TOML 解析**
- 工具: `cargo test -p pixivbot -- config`
- 步骤: 包含两个 `[[booru.sites]]` 的 TOML 字符串反序列化
- 预期: `config.booru.sites.len() == 2`，各字段正确

**验证**: `make ci` 通过。Config 无配置 `[booru]` 时 `Default` 正常工作（空 sites 列表 → 不启动 BooruEngine）。

---

## Phase 4: Repo 层扩展

### `src/db/repo/subscriptions.rs` 新增

```rust
/// Upsert booru subscription with BooruFilter
pub async fn upsert_booru_subscription(
    &self,
    chat_id: i64,
    task_id: i32,
    booru_filter: BooruFilter,
) -> Result<subscriptions::Model>;

/// Get booru_filter for a subscription
pub async fn get_subscription_booru_filter(
    &self,
    subscription_id: i32,
) -> Result<Option<BooruFilter>>;
```

现有 `list_subscriptions_by_chat/task`、`delete_subscription`、`update_subscription_latest_data` 方法无需修改，已支持 booru task types。

### QA 场景

**QA-4.1: upsert_booru_subscription**
- 工具: `cargo test -p pixivbot -- upsert_booru`
- 步骤: 创建 in-memory SQLite DB，运行 migration，调用 `upsert_booru_subscription(chat_id=1, task_id=1, filter)`
- 预期: 返回 `subscriptions::Model`，`booru_filter` 字段匹配输入。重复调用更新 filter。

**验证**: `make ci` 通过。 Scheduler — BooruEngine

### 文件: `src/scheduler/booru_engine.rs`

遵循 `AuthorEngine` 的 Orchestrator-Dispatcher-Worker 模式：

```rust
pub struct BooruEngine {
    repo: Arc<Repo>,
    booru_clients: HashMap<String, BooruClient>, // site_name → client
    notifier: Notifier,
    poll_interval_sec: u64,
    max_posts_per_poll: u32,
    max_retry_count: i32,
    sensitive_tags: Vec<String>,
}

impl BooruEngine {
    pub async fn run(&self);        // 主循环
    async fn tick(&self);           // 获取 1 个待处理 BooruTag/BooruPool task
    async fn execute_booru_tag_task(&self, task: tasks::Model);
    async fn execute_booru_pool_task(&self, task: tasks::Model);
    async fn process_single_booru_sub(
        &self,
        sub: &subscriptions::Model,
        posts: &[BooruPost],
        site_name: &str,
    );
}
```

### 轮询流程（Tag 订阅）— 队列排空模型

核心原则：**先排空队列，再拉取新帖。确保零丢弃。**

1. `tick()` 取一个到期的 `BooruTag` / `BooruPool` 类型 task
2. `execute_booru_tag_task()`:
   a. 解析 `task.value` → `(site_name, tags)`
   b. 获取对应 `BooruClient`
   c. 遍历该 task 的所有 subscriptions，对每个调用 `process_single_booru_sub()`
3. `process_single_booru_sub()`:
   a. 从 `latest_data` 取 `BooruTagState`（首次为 None → 初始化空状态）
   b. **Step 1 — 排空现有队列**:
      - 如果 `pending_queue` 非空，从头部取最多 `max_posts_per_poll` 个 `QueuedBooruPost`
      - 逐个推送（sample_url → Telegram，file_url → 缓存原图）
      - 推送成功的从队列移除，失败的设置 `pending_post` 用于重试
      - 保存更新后的 state，**本 tick 结束**（不拉取新帖）
   c. **Step 2 — 拉取新帖**（仅当 `pending_queue` 为空时执行）:
      - 调用 `get_posts(tags + booru_filter.to_api_tags(), limit=100, since_id=latest_post_id)`
      - 对返回的帖子应用 `BooruFilter::matches()` 本地过滤
      - 过滤后的帖子转换为 `QueuedBooruPost`，全部追加到 `pending_queue`
      - 更新 `latest_post_id` 为本批次最大 post.id
      - 从 `pending_queue` 头部取最多 `max_posts_per_poll` 个推送
      - 剩余的留在 `pending_queue` 等待下一个 tick
      - 保存 state

**关键保证**：
- `latest_post_id` 仅在 API 拉取后更新，不依赖推送进度
- `pending_queue` 存储完整帖子数据（`QueuedBooruPost`），不是仅 ID
- 即使 bot 重启，队列中的帖子数据已持久化在 `subscriptions.latest_data` JSON 中
- 活跃站点空标签订阅：每 tick 排空 `max_posts_per_poll` 个，直到队列为空后再拉取下一批

### 轮询流程（Pool 订阅）

1. `execute_booru_pool_task()`:
   a. 解析 `task.value` → `(site_name, pool_id)`
   b. 调用 `get_pool(pool_id)` 获取池信息
   c. 对比 `BooruPoolState.known_post_ids` 找出新增帖子
   d. 获取新帖详情，推送，更新 `known_post_ids`

### Rating 敏感内容处理

```rust
fn is_sensitive(post: &BooruPost, sensitive_tags: &[String]) -> bool {
    // 方式 1: Rating 级别判断
    matches!(post.rating, BooruRating::Questionable | BooruRating::Explicit)
    // 方式 2: 与现有 sensitive_tags 列表交叉
    || post.tags.split_whitespace().any(|t| sensitive_tags.iter().any(|st| st.eq_ignore_ascii_case(t)))
}
```

如果帖子是敏感内容且该 chat 启用了 `blur_sensitive_tags`，发送时使用 `has_spoiler = true`。

### 图片推送

- 发送 `sample_url`（Telegram 展示用）
- 下载 `file_url`（原图，缓存到 `FileCacheManager`，用于 `/download`）
- Booru 帖子是单图（与 Pixiv 多图不同），不需要批量分组逻辑
- Caption 格式:

```
*{site_name}* \#{post_id}
Score: {score} ⭐ {fav_count}
Tags: {top_5_tags}...
[Source]({source_url})
```

### 新增到 `src/scheduler/mod.rs`

导出 `BooruEngine`。

### 新增到 `src/main.rs`

```rust
// 如果配置了 booru sites，启动 BooruEngine
if !config.booru.sites.is_empty() {
    let booru_engine = scheduler::BooruEngine::new(
        repo.clone(),
        &config.booru,
        notifier.clone(),
        config.scheduler.max_retry_count,
        config.content.sensitive_tags.clone(),
    );
    let booru_engine_handle = tokio::spawn(async move {
        booru_engine.run().await;
    });
    // ... 加入 shutdown abort
}
```

### QA 场景

**QA-5.1: 队列排空行为**
- 工具: `cargo test -p pixivbot -- queue_drain`
- 步骤: 构造 `BooruTagState { pending_queue: [post1, post2, ..., post25], latest_post_id: 100 }`（25 个帖子），`max_posts_per_poll = 10`
- 预期: 第一次 tick 推送 10 个（queue 剩 15），第二次 tick 推送 10 个（queue 剩 5），第三次 tick 推送 5 个（queue 空），第四次 tick 执行 API 拉取

**QA-5.2: 首次轮询（无状态）**
- 工具: `cargo test -p pixivbot -- first_poll`
- 步骤: `latest_data = None`，模拟 API 返回 5 个帖子
- 预期: `latest_post_id` 设为最大 post.id，5 个帖子全部入 `pending_queue` 然后推送（因 < max_posts_per_poll）

**QA-5.3: rating 敏感判断**
- 工具: `cargo test -p pixivbot -- is_sensitive`
- 步骤: 测试 rating=Explicit 的帖子，以及 rating=Safe 但 tags 含 "R-18" 的帖子
- 预期: 两者都返回 `true`（两种判断方式的组合）

**QA-5.4: Pool 新帖检测**
- 工具: `cargo test -p pixivbot -- pool_new_posts`
- 步骤: `known_post_ids = [1,2,3]`，API 返回 pool.post_ids = [1,2,3,4,5]
- 预期: 新帖 = [4, 5]

**QA-5.5: pending_post 重试**
- 工具: `cargo test -p pixivbot -- pending_retry`
- 步骤: 构造 `pending_post = Some(PendingBooruPost { post_id: 42, retry_count: 2 })`，max_retry = 3
- 预期: 重试推送 post 42；若再次失败，retry_count 变 3 → 下次 tick 放弃该帖子

**验证**: `make ci` 通过。

---

## Phase 6: Bot 命令 + Handler

### 命令定义 (`src/bot/commands.rs`)

```rust
#[command(description = "订阅 Booru 标签")]
BooruSub(String),    // /boorusub site_name [tags] [score:>=N] [favcount:>=N] [-exclude_tag]
#[command(description = "订阅 Booru 合集")]
BooruPool(String),   // /boorupool site_name pool_id
#[command(description = "取消 Booru 订阅")]
BooruUnsub(String),  // /booruunsub site_name [tags|pool_id]
#[command(description = "查看 Booru 订阅列表")]
BooruList(String),   // /boorulist [site_name]
```

注册到 `user_commands()` 列表。

### 命令参数解析

```
/boorusub yandere landscape score:>=50 -R-18
         ^site   ^tags      ^filter    ^exclude
```

解析规则:
- 第一个参数: site_name（必须与 config 中某个站点匹配）
- `score:>=N`: min_score 过滤器
- `favcount:>=N`: min_favcount 过滤器
- `-tag`: 排除标签
- 其他: 订阅标签

### Handler 文件

新建 `src/bot/handlers/booru.rs`（或根据已有拆分模式拆入 `subscription/` 子目录）:

```rust
pub async fn handle_booru_sub(handler: &BotHandler, msg: &Message, ctx: &UserChatContext, args: String);
pub async fn handle_booru_pool(handler: &BotHandler, msg: &Message, ctx: &UserChatContext, args: String);
pub async fn handle_booru_unsub(handler: &BotHandler, msg: &Message, ctx: &UserChatContext, args: String);
pub async fn handle_booru_list(handler: &BotHandler, msg: &Message, ctx: &UserChatContext, args: String);
```

### 用户交互消息（中文）

```
✅ 已订阅 yandere 标签: landscape (score>=50)
❌ 未找到站点: xxx，可用站点: yandere, konachan
✅ 已订阅 yandere 合集 #12345: "Pool Name"
📋 Booru 订阅列表:
  1. [yandere] landscape (score>=50)
  2. [yandere] 合集 #12345 "Pool Name"
```

### `/list` 命令集成

在现有 `/list` 命令输出中追加 booru 订阅（如果有的话），按 task type 分组。

### `/unsubthis` 支持

当用户回复 booru 推送消息时，`/unsubthis` 应能识别并取消对应的 booru 订阅。需要在消息记录中存储 task 信息。

### QA 场景

**QA-6.1: 命令参数解析**
- 工具: `cargo test -p pixivbot -- booru_parse_args`
- 步骤: 解析 `"yandere landscape score:>=50 -R-18"`
- 预期: `site_name = "yandere"`, tags = `"landscape"`, `min_score = Some(50)`, `exclude_tags = ["R-18"]`

**QA-6.2: 无效站点名称**
- 工具: `cargo test -p pixivbot -- booru_invalid_site`
- 步骤: 在 config 只有 `["yandere"]` 时，解析 `"unknown_site landscape"`
- 预期: 返回错误消息 `"未找到站点: unknown_site，可用站点: yandere"`

**QA-6.3: /list 集成**
- 工具: `cargo test -p pixivbot -- list_booru`
- 步骤: 创建 1 个 author sub + 1 个 booru_tag sub，调用 list handler
- 预期: 输出同时包含 author 和 booru 订阅，按类型分组

**QA-6.4: /booruunsub 正确删除**
- 工具: `cargo test -p pixivbot -- booruunsub`
- 步骤: 创建 booru subscription，然后调用 unsub handler
- 预期: subscription 从 DB 删除，task 无其他 subscriber 时也被清理

**验证**: `make ci` 通过。

---

## Phase 7: 集成测试 + 配置示例完善

### QA 场景

**QA-7.1: 端到端流程**
- 工具: `cargo test -p pixivbot -- booru_e2e`
- 步骤:
  1. 构造 `BooruConfig` 包含 1 个 Moebooru 站点
  2. 创建 BooruEngine，调用 `tick()`
  3. 模拟 API 返回 3 个帖子（直接构造 `Vec<BooruPost>`）
  4. 验证 DB 中 subscription.latest_data 已更新，pending_queue 处理正确
- 预期: `BooruTagState.latest_post_id` 等于最大 post.id，推送方法被调用 3 次

**QA-7.2: 无 booru 配置时不启动引擎**
- 工具: 代码审查 `src/main.rs`
- 步骤: 确认 `if !config.booru.sites.is_empty()` 守卫正确
- 预期: 空 sites 时 BooruEngine 不被创建/spawned

**QA-7.3: config.toml.example 完整性**
- 工具: 人工检查
- 步骤: 确认 `config.toml.example` 包含 `[[booru.sites]]` 示例和所有字段注释
- 预期: 配置示例可直接取消注释使用

**验证**: `make ci` 全量通过。

---

## 关键约束

### MUST NOT

- **不修改 `TagFilter`**：Booru 过滤是独立的 `BooruFilter` 类型
- **不修改 `process_illust_push()` / `AuthorContext`**：Booru 有独立的推送逻辑
- **不创建 `trait BooruSite` 多态**：用单一 `BooruClient` + `BooruEngineType` 枚举分发
- **不创建按引擎拆分的 `TaskType`**（如 `BooruDanbooru`）：站点信息编码在 `task.value` 中
- **不引入外部 booru crate**：自建客户端

### MUST

- 每个 commit 通过 `make ci`
- 所有用户消息使用 `MarkdownV2` + `markdown::escape()`
- 错误处理遵循现有模式：`error!()` 记录详情，用户只看到 `❌ 操作失败`
- `BooruPost` 模型使用 `#[serde(default)]` 容忍缺失字段
- 新增 `TaskType` / `SubscriptionState` 变体后检查所有 match 穷举性

### 优先级

Moebooru (yande.re / konachan) > Danbooru > Gelbooru

初始实现专注 Moebooru，确保端到端流通后再扩展其他引擎。Danbooru/Gelbooru 的 JSON 反序列化 + URL 构造差异在 `BooruClient` 中通过 `match engine_type` 分发处理。

---

## Commit 策略

```
C1: booru_client crate scaffold + Moebooru models + 反序列化测试
C2: booru_client Moebooru API 实现 (get_posts, get_pool)
C3: DB types (TaskType/SubscriptionState 扩展) + BooruFilter + migration
C4: Config (BooruConfig/BooruSiteConfig) + config.toml.example
C5: Repo 层 booru 订阅 CRUD
C6: BooruEngine scheduler (tag + pool 轮询 + 推送)
C7: Bot commands + handlers (boorusub/boorupool/booruunsub/boorulist)
C8: 集成 (/list 集成, /unsubthis 支持, sensitive 处理)
C9: Danbooru 引擎支持 (client + 测试)
C10: Gelbooru 引擎支持 (client + 测试)
```
