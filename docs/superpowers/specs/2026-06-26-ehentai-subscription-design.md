# E-Hentai/ExHentai Gallery Subscription Feature Design

## Overview

Add e-hentai/exhentai gallery subscription support to pixivbot. Users subscribe via eh search syntax (tags, author, rating, etc.); the scheduler polls for new galleries, downloads the archive ZIP at a configured resolution, and sends it to the subscribed Telegram chat. An optional Telegraph upload creates a readable page and sends the link.

## Requirements

1. **Subscribe via eh search syntax** — tag, author, rating, category, page-count filters
2. **On update** — download archive ZIP at specified resolution, send to chat as document
3. **Optional Telegraph** — per-subscription toggle; when enabled, extract images from ZIP, upload to Telegraph, create page, send link
4. **Rating filter → 48h scan** — when subscription has a `min_rating` filter, scan all galleries from the last 48h (not just new ones since last poll), download undownloaded ones and send
5. **Direct download command** — `/edl <url|gid>` downloads a specific gallery by URL or GID and sends it immediately
6. **Download rate limiting** — max 10GB per 24h; excess download tasks are persisted as a queue in the database and drained when budget allows

## Architecture

Mirror the existing booru pattern:

```
eh_client/              ← new workspace crate (low-level eh API client)
  src/
    lib.rs              ← module exports
    client.rs           ← EhClient: search, metadata API, archive download
    models.rs           ← EhGallery, EhGalleryRef, EhCategory, EhConfig
    parser.rs           ← HTML parsing (search results, archiver.php response)
    telegraph.rs        ← Telegraph API client (createAccount, createPage, upload)
    error.rs            ← Error/Result types

migration/              ← new migration m20260626_add_ehentai

src/
  config.rs             ← EhentaiConfig section (includes rate_limit_gb, rate_limit_window_hours)
  db/
    entities/           ← reuse existing tasks/subscriptions entities
      eh_download_queue.rs  ← NEW: EhDownloadQueue entity (persistent download queue)
    types/
      task_type.rs      ← add TaskType::Ehentai
      state.rs          ← add SubscriptionState::EhTag(EhTagState)
      eh_filter.rs      ← NEW: EhFilter (rating, pages, telegraph toggle)
      eh_task_key.rs    ← NEW: EhTaskKey (task value encoding)
      eh_download.rs    ← NEW: EhDownloadRequest (queued download with chat_id, gid, token, source)
      mod.rs            ← register new types
    repo/
      subscriptions.rs  ← add upsert_eh_subscription
      eh_download_queue.rs  ← NEW: queue CRUD + rate-limit accounting
  scheduler/
    eh_engine.rs       ← NEW: EhEngine (poll, filter, download via queue, send)
    eh_download_processor.rs ← NEW: drains queue, enforces rate limit, downloads+sends
    mod.rs              ← register EhEngine
  bot/
    commands.rs         ← add ESub/EUnsub/EList/EDl commands
    handler.rs          ← add eh field to BotHandler, dispatch
    handlers/
      subscription/
        ehentai.rs     ← NEW: handle_esub/handle_eunsub/handle_elist
        eh_download.rs ← NEW: handle_edl (direct gallery download)
        mod.rs          ← register ehentai module
    notifier.rs         ← add notify_with_document, notify_with_text
    mod.rs              ← add has_ehentai flag, callback prefix
  main.rs               ← build EhClient, spawn EhEngine, pass to bot
```

## Components

### 1. `eh_client` Crate

#### `EhClient`

```rust
pub struct EhClient {
    http: reqwest::Client,        // cookie-aware, IPv4-bound for exhentai
    base_url: String,             // "https://e-hentai.org" or "https://exhentai.org"
    api_url: String,              // "https://api.e-hentai.org/api.php"
    cookies: EhCookies,
    image_resolution: String,    // "780x", "980x", "1280x", "1600x", "2400x", "original"
}

pub struct EhCookies {
    pub ipb_member_id: Option<String>,
    pub ipb_pass_hash: Option<String>,
    pub igneous: Option<String>,       // required for exhentai
    pub nw: bool,                       // always true
}
```

**Public API:**

- `new(config: &EhentaiConfig) -> Result<Self>` — builds reqwest client with cookie header, IPv4 binding for exhentai, User-Agent
- `search(&self, query: &str, cats: u32, page: u32) -> Result<Vec<EhGalleryRef>>` — HTML scrape search results page; returns `(gid, token)` pairs + basic info (title, url, category, posted_ts)
- `get_metadata(&self, gidlist: &[(u64, &str)]) -> Result<Vec<EhGallery>>` — POST to api.php (max 25 per request); returns full metadata
- `get_archiver_key(&self, gid: u64, token: &str) -> Result<String>` — GET gallery page, extract archiver_key from `<a href="...archiver.php?...&or={key}">`
- `download_archive(&self, gid: u64, token: &str, archiver_key: &str, dest: &Path) -> Result<PathBuf>` — POST to archiver.php, parse JS redirect, download ZIP

#### `EhGallery` / `EhGalleryRef`

```rust
pub struct EhGalleryRef {        // from search HTML
    pub gid: u64,
    pub token: String,
    pub title: String,
    pub url: String,
    pub posted_ts: i64,          // unix timestamp
}

pub struct EhGallery {           // from api.php metadata
    pub gid: u64,
    pub token: String,
    pub title: String,
    pub title_jpn: Option<String>,
    pub category: String,        // "Doujinshi", "Manga", etc.
    pub thumb: String,
    pub uploader: String,
    pub posted: i64,             // unix timestamp (from string in API)
    pub filecount: u32,
    pub filesize: u64,
    pub expunged: bool,
    pub rating: f64,             // parsed from string "4.64"
    pub tags: Vec<String>,       // namespace-prefixed if namespace=1
}
```

#### `EhCategory` enum

Doujinshi(1), Manga(2), ArtistCG(4), GameCG(8), Western(16), NonH(32), ImageSet(64), Cosplay(128), AsianPorn(256), Misc(512). Methods: `as_str()`, `from_str()`, `bitmask()`, `from_bitmask()`.

#### HTML Parser (`parser.rs`)

Uses `regex` crate (already a dependency) to extract:
- Search results: gallery URLs `https://e-hentai.org/g/{gid}/{token}/`, titles, posted timestamps
- Archiver.php response: JS redirect URL `document.location = "..."` → extract hath download URL
- Gallery page: `archiver.php?gid=...&token=...&or={archiver_key}` link

Regex approach is sufficient because eh HTML structure is stable and the patterns are distinctive. No new HTML parser dependency needed.

#### Telegraph Client (`telegraph.rs`)

```rust
pub struct TelegraphClient {
    http: reqwest::Client,
    access_token: String,
}

impl TelegraphClient {
    pub fn new(access_token: String) -> Self;
    pub async fn upload_image(&self, image_data: &[u8]) -> Result<String>;  // returns full URL
    pub async fn create_page(&self, title: &str, content: &[Node]) -> Result<String>;  // returns page URL
    pub async fn create_gallery_page(&self, title: &str, image_urls: &[String]) -> Result<String>;
}
```

- `upload_image`: POST to `https://telegra.ph/upload` (multipart), returns `https://telegra.ph{src}`
- `create_page`: POST to `https://api.telegra.ph/createPage`, content as JSON array
- `create_gallery_page`: uploads images, builds `<img>` node array, creates page. If content > 64KB, splits into multiple pages with "Next Page" links (reverse order creation).

### 2. Configuration

```toml
[ehentai]
# Omit this section to disable the feature
site = "e-hentai"                # "e-hentai" or "exhentai"
ipb_member_id = ""               # required for exhentai, recommended for e-hentai
ipb_pass_hash = ""                # required for exhentai
igneous = ""                     # required for exhentai
image_resolution = "780x"        # 780x, 980x, 1280x, 1600x, 2400x, original
min_interval_sec = 1800           # 30 min
max_interval_sec = 3600           # 1 hour
telegraph_access_token = ""       # optional, for Telegraph uploads
max_push_per_tick = 3             # max galleries to send per tick
max_retry_count = 3
scan_window_hours = 48            # 48h scan window for rating filters
download_rate_limit_gb = 10       # max GB downloaded per 24h window
download_rate_window_hours = 24   # rate-limit window duration
download_poll_interval_sec = 60   # how often the download processor drains the queue
```

```rust
pub struct EhentaiConfig {
    pub site: String,                      // "e-hentai" or "exhentai"
    pub ipb_member_id: Option<String>,
    pub ipb_pass_hash: Option<String>,
    pub igneous: Option<String>,
    pub image_resolution: String,          // default "780x"
    pub min_interval_sec: u64,             // default 1800
    pub max_interval_sec: u64,             // default 3600
    pub telegraph_access_token: Option<String>,
    pub max_push_per_tick: usize,          // default 3
    pub max_retry_count: u8,               // default 3
    pub scan_window_hours: u64,            // default 48
    pub download_rate_limit_gb: u64,       // default 10
    pub download_rate_window_hours: u64,  // default 24
    pub download_poll_interval_sec: u64,  // default 60
}

impl Default for EhentaiConfig {
    fn default() -> Self {
        Self {
            site: "e-hentai".into(),
            ipb_member_id: None,
            ipb_pass_hash: None,
            igneous: None,
            image_resolution: "780x".into(),
            min_interval_sec: 1800,
            max_interval_sec: 3600,
            telegraph_access_token: None,
            max_push_per_tick: 3,
            max_retry_count: 3,
            scan_window_hours: 48,
            download_rate_limit_gb: 10,
            download_rate_window_hours: 24,
            download_poll_interval_sec: 60,
        }
    }
}
```

`Config` struct gains `pub ehentai: EhentaiConfig` (defaults to disabled — empty site config). Feature is considered enabled when the `[ehentai]` section is present in config with a valid `site` value. For e-hentai, no auth cookies are strictly required (public galleries accessible). For exhentai, `ipb_member_id`, `ipb_pass_hash`, and `igneous` are all required.

### 3. Database

#### Migration `m20260626_add_ehentai`

- Add column `eh_filter` (JSON, nullable) to `subscriptions` table — stores `Option<EhFilter>`
- Create new table `eh_download_queue` for persistent download queue:

```sql
CREATE TABLE eh_download_queue (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    chat_id INTEGER NOT NULL,           -- destination chat
    gid INTEGER NOT NULL,               -- gallery id
    token TEXT NOT NULL,                -- gallery token
    title TEXT NOT NULL,                -- gallery title (for caption)
    telegraph INTEGER NOT NULL DEFAULT 0,  -- bool: do Telegraph upload?
    source TEXT NOT NULL,              -- "subscription" or "direct" or "subscription:<sub_id>"
    status TEXT NOT NULL DEFAULT 'pending',  -- pending | downloading | done | failed
    file_size INTEGER,                  -- actual downloaded bytes (for rate accounting)
    error TEXT,                         -- last error message if failed
    retry_count INTEGER NOT NULL DEFAULT 0,
    created_at TIMESTAMP NOT NULL,
    started_at TIMESTAMP,               -- when download began
    completed_at TIMESTAMP              -- when download finished (for rate window)
)
CREATE INDEX idx_eh_dlq_status ON eh_download_queue(status);
CREATE INDEX idx_eh_dlq_completed ON eh_download_queue(completed_at);
```

#### `TaskType::Ehentai`

Add variant `Ehentai("ehentai")` to `TaskType` enum.

#### `EhTaskKey`

```rust
pub struct EhTaskKey {
    pub query: String,
    pub category_bitmask: u32,
    pub filter_sig: String,
}

// to_task_value() → "eh:<query>|c=<bitmask>|f=<filter_sig>"
// parse() reverses: split on '|', first segment split_once(':') → ("eh", query)
```

Filter signature encoding (fixed order):
- `r{min_rating}` if min_rating present (e.g. `r4`)
- `p{min_pages}` if min_pages present (e.g. `p20`)
- `P{max_pages}` if max_pages present (e.g. `P500`)
- Empty string if no filter

#### `EhFilter`

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EhFilter {
    pub min_rating: Option<u8>,      // 2-5, triggers 48h scan if present
    pub min_pages: Option<u32>,
    pub max_pages: Option<u32>,
    pub telegraph: bool,             // enable Telegraph upload for this subscription
}

impl EhFilter {
    pub fn new() -> Self;
    pub fn is_empty(&self) -> bool;
    pub fn task_value_signature(&self) -> String;
    pub fn matches(&self, gallery: &EhGallery) -> bool;
    pub fn has_rating_filter(&self) -> bool;
    pub fn aggregate(filters: &[&EhFilter]) -> Self;  // loosest: min rating, min pages, max pages
    pub fn format_for_display(&self) -> String;
}
```

#### `EhTagState` in `SubscriptionState`

```rust
SubscriptionState::EhTag(EhTagState)

pub struct EhTagState {
    pub pushed_gids: Vec<u64>,              // galleries already sent (dedup)
    pub latest_posted_ts: i64,             // cursor for non-rating-filtered mode
    pub pending_queue: Vec<QueuedEhGallery>, // pending galleries to queue for download
    pub retry_count: u8,
}

pub struct QueuedEhGallery {
    pub gid: u64,
    pub token: String,
    pub title: String,
    pub title_jpn: Option<String>,
    pub category: String,
    pub thumb: String,
    pub uploader: String,
    pub posted: i64,
    pub filecount: u32,
    pub filesize: u64,
    pub rating: f64,
    pub tags: Vec<String>,
}
```

#### `EhDownloadRequest` entity

```rust
// src/db/entities/eh_download_queue.rs
pub enum Status { Pending, Downloading, Done, Failed }

pub struct Model {
    pub id: i32,
    pub chat_id: i64,
    pub gid: u64,
    pub token: String,
    pub title: String,
    pub telegraph: bool,
    pub source: String,         // "subscription" or "direct"
    pub status: String,          // pending|downloading|done|failed
    pub file_size: Option<u64>,
    pub error: Option<String>,
    pub retry_count: u8,
    pub created_at: DateTime,
    pub started_at: Option<DateTime>,
    pub completed_at: Option<DateTime>,
}
```

#### Repo Methods

```rust
// in src/db/repo/subscriptions.rs
pub async fn upsert_eh_subscription(
    &self, chat_id: i64, task_id: i32,
    filter_tags: TagFilter, eh_filter: Option<EhFilter>,
) -> Result<subscriptions::Model>;

// in src/db/repo/eh_download_queue.rs (NEW)
pub async fn enqueue_download(&self, chat_id: i64, gid: u64, token: &str, title: &str, telegraph: bool, source: &str) -> Result<()>;
pub async fn get_next_pending_download(&self) -> Result<Option<Model>>;
pub async fn mark_download_started(&self, id: i32) -> Result<()>;
pub async fn mark_download_done(&self, id: i32, file_size: u64) -> Result<()>;
pub async fn mark_download_failed(&self, id: i32, error: &str) -> Result<()>;
pub async fn get_downloaded_bytes_in_window(&self, window_start: DateTime) -> Result<u64>;
pub async fn count_pending_downloads(&self) -> Result<u64>;
pub async fn reset_stale_downloading(&self) -> Result<u64>;  // crash recovery: downloading→pending
```

### 4. EhEngine (`src/scheduler/eh_engine.rs`) — search & enqueue

```rust
pub struct EhEngine {
    repo: Arc<Repo>,
    client: Arc<EhClient>,
    config: Arc<EhentaiConfig>,
    tick_interval_sec: u64,
}
```

The EhEngine is now **search-only**. It polls for new galleries and enqueues download requests into `eh_download_queue`. A separate `EhDownloadProcessor` drains the queue with rate limiting.

#### Engine Flow (per tick)

```
1. Fetch one pending Ehentai task (repo.get_pending_tasks_by_type)
2. Parse EhTaskKey from task value
3. List subscriptions on task
4. Compute aggregate EhFilter across subs
5. Search for galleries:
   a. If aggregate filter has min_rating (48h scan mode):
      - Calculate cutoff_ts = now - scan_window_hours
      - Fetch search pages until oldest gallery.posted_ts < cutoff_ts
      - Dedup against pushed_gids from all subs' states
      - MAX_FETCH_PAGES = 5 (safety cap)
   b. If no rating filter (normal mode):
      - Fetch search page 0
      - Stop when gallery.posted_ts <= latest_posted_ts (oldest across subs)
6. Batch fetch metadata via api.php (max 25 per request)
7. Filter galleries through aggregate EhFilter (rating, pages)
8. For each new gallery, for each subscribed chat:
   - Enqueue EhDownloadRequest into eh_download_queue
   - Add gid to pushed_gids
   - Update latest_posted_ts if newer
9. Update subscription states (pushed_gids, latest_posted_ts, pending_queue)
10. Schedule next poll: random in [min_interval_sec, max_interval_sec]
```

### 4b. EhDownloadProcessor (`src/scheduler/eh_download_processor.rs`) — drain queue

```rust
pub struct EhDownloadProcessor {
    repo: Arc<Repo>,
    notifier: Arc<Notifier>,
    client: Arc<EhClient>,
    telegraph: Option<Arc<TelegraphClient>>,
    config: Arc<EhentaiConfig>,
    poll_interval_sec: u64,
}
```

#### Processor Flow (per poll)

```
1. Crash recovery: reset_stale_downloading() (any download stuck in "downloading" → "pending")
2. Check rate limit:
   - window_start = now - download_rate_window_hours
   - downloaded_bytes = repo.get_downloaded_bytes_in_window(window_start)
   - budget_remaining = (download_rate_limit_gb * 1_000_000_000) - downloaded_bytes
   - If budget_remaining <= 0: skip this poll, wait for next
3. Get next pending download: repo.get_next_pending_download()
   - If none: skip
4. Mark as "downloading" (started_at = now)
5. Fetch gallery metadata (for archiver_key + filesize estimate)
6. Download archive ZIP to tempfile
7. If downloaded_size > budget_remaining:
   - Mark as "failed" with error "rate limit exceeded"
   - Re-queue will happen on next poll when budget replenishes
   - Actually: check filesize from metadata BEFORE downloading; if > budget, skip and wait
8. Send ZIP to chat via notifier.notify_with_document()
9. If telegraph: extract images, upload, create page, send link
10. Mark as "done" (completed_at = now, file_size = actual bytes)
11. If send failed: mark_download_failed, increment retry_count, if retry_count < max → stays pending
```

#### Constants

```rust
const MAX_FETCH_PAGES: u32 = 5;          // safety cap for 48h scan
const MAX_METADATA_BATCH: usize = 25;    // api.php limit
const SEARCH_RATE_LIMIT_MS: u64 = 3500;  // 3s + buffer
```

#### Send Logic

The processor sends via Notifier:
- `notifier.notify_with_document(chat_id, zip_path, filename, caption)` — sends ZIP file
- `notifier.notify_with_text(chat_id, telegraph_url)` — sends Telegraph link

Caption format (MarkdownV2):
```
{title}
{category} | {rating}★ | {filecount}p | {filesize}
{tags} (first 10)
{gallery_url}
```

### 5. Bot Commands

```
/esub <query> [filter_args] [telegraph=on]
/eunsub <query>
/elist
/edl <url|gid> [telegraph=on]
```

#### `/esub` — Subscribe

Examples:
```
/esub female:elf
/esub artist:wlop$ rating>=4 pages>=20
/esub parody:touhou$ cat=doujinshi,manga telegraph=on
```

Filter args:
- `rating>=N` / `rating>N` (stored N+1) — min rating 2-5
- `pages>=N` / `pages>N` — min page count
- `pages<=N` / `pages<N` (stored N-1) — max page count
- `cat=<category>` — category filter (comma-separated: doujinshi, manga, artistcg, ...)
- `telegraph=on` / `telegraph=off` — enable Telegraph upload (default: off)

Parsing flow (mirrors `parse_booru_filter_args`):
1. `args::parse_args(&args_str)` — handle `ch=<channel>` target
2. Split remaining by whitespace
3. First arg = query (everything until first filter arg)
4. Classify args: `rating>`, `pages>`, `pages<`, `cat=`, `telegraph=` prefixes → filter args; bare tokens → query terms
5. Build EhFilter from filter args
6. Build EhTaskKey::new(query, category_bitmask, &eh_filter).to_task_value()
7. `repo.get_or_create_task(TaskType::Ehentai, &task_value, None)`
8. `repo.upsert_eh_subscription(chat_id, task_id, filter_tags, Some(eh_filter))`
9. Success message with MarkdownV2 escaping

#### `/eunsub` — Unsubscribe

Supports:
- `/eunsub <query>` — unsubscribe by query
- `/eunsub <internal_key>` — unsubscribe by task value (contains `|`)

#### `/elist` — List

Lists all eh subscriptions for the chat, showing query, filters, and telegraph status.

#### `/edl` — Direct Download

Download a specific gallery and send it immediately (enqueued into the download queue, processed when budget allows).

Examples:
```
/edl https://e-hentai.org/g/123456/abcdef0123/
/edl 123456/abcdef0123
/edl 123456 telegraph=on
```

Parsing flow:
1. Parse args: extract `telegraph=on/off` flag (default: off), `ch=<channel>` target
2. Extract gallery URL or `gid/token` from the first remaining arg
3. URL regex: `https?://(?:e-hentai|exhentai)\.org/g/(\d+)/([0-9a-f]+)/?`
4. If numeric `gid/token` format: split on `/`
5. Fetch gallery metadata via api.php to get title, category, rating, filecount, filesize, tags
6. Enqueue download request into `eh_download_queue` with source="direct"
7. Reply with confirmation: gallery title, estimated size, queue position, and rate-limit status

The actual download+send happens asynchronously via `EhDownloadProcessor`.

### 6. Notifier Extensions

Add to `Notifier`:

```rust
/// Send a document (file) to a chat. Returns message_id on success.
pub async fn notify_with_document(
    &self,
    chat_id: ChatId,
    path: &Path,
    filename: &str,
    caption: &str,           // MarkdownV2
) -> Result<i32>;

/// Send a text message to a chat. Returns message_id on success.
pub async fn notify_with_text(
    &self,
    chat_id: ChatId,
    text: &str,              // MarkdownV2
) -> Result<i32>;
```

Both set appropriate `ChatAction` before sending and use `ParseMode::MarkdownV2`.

### 7. Wiring

#### `src/main.rs`

```rust
// After booru setup:
let eh_client = if config.ehentai.site == "exhentai" {
    // exhentai requires all three cookies
    if config.ehentai.ipb_member_id.is_some()
        && config.ehentai.ipb_pass_hash.is_some()
        && config.ehentai.igneous.is_some()
    {
        Some(Arc::new(EhClient::new(&config.ehentai)?))
    } else {
        tracing::warn!("ExHentai enabled but missing required cookies (ipb_member_id, ipb_pass_hash, igneous). EH feature disabled.");
        None
    }
} else if config.ehentai.site == "e-hentai" {
    // e-hentai works without auth (public galleries)
    Some(Arc::new(EhClient::new(&config.ehentai)?))
} else {
    None
};

let telegraph_client = config.ehentai.telegraph_access_token.as_ref()
    .map(|token| Arc::new(TelegraphClient::new(token.clone())));

if let Some(ref client) = eh_client {
    let eh_engine = EhEngine::new(
        Arc::clone(&repo),
        Arc::clone(client),
        Arc::new(config.ehentai.clone()),
        config.scheduler.tick_interval_sec,
    );
    let handle = tokio::spawn(async move { eh_engine.run().await });
    task_handles.push(handle);

    let eh_processor = EhDownloadProcessor::new(
        Arc::clone(&repo),
        Arc::clone(&notifier),
        Arc::clone(client),
        telegraph_client.clone(),
        Arc::new(config.ehentai.clone()),
        config.ehentai.download_poll_interval_sec,
    );
    let handle = tokio::spawn(async move { eh_processor.run().await });
    task_handles.push(handle);
}
```

Pass `eh_client` to `bot::run` (like `booru_registry`).

#### `src/bot/mod.rs`

- Add `has_ehentai: bool` flag to `setup_commands`
- Add eh commands to `Command::user_commands/admin_commands/owner_commands` when `has_ehentai`
- Pass `eh_client: Option<Arc<EhClient>>` to `BotHandler::new`

#### `src/bot/handler.rs`

- Add `eh_client: Option<Arc<EhClient>>` field to `BotHandler`
- Add dispatch for `Command::ESub/EUnsub/EList/EDl` → `handle_esub/handle_eunsub/handle_elist/handle_edl`

## Data Flow Summary

```
User: /esub female:elf rating>=4 telegraph=on
  → BotHandler::handle_esub
  → EhTaskKey { query: "female:elf", cats: 0, filter_sig: "r4" }
  → repo.get_or_create_task(Ehentai, "eh:female:elf|c=0|f=r4")
  → repo.upsert_eh_subscription(chat, task, TagFilter::default(), EhFilter { min_rating: 4, telegraph: true })
  → Confirmation message

User: /edl https://e-hentai.org/g/123456/abcdef0123/ telegraph=on
  → BotHandler::handle_edl
  → Parse URL → gid=123456, token="abcdef0123"
  → client.get_metadata(123456, "abcdef0123") → EhGallery
  → repo.enqueue_download(chat_id, 123456, "abcdef0123", title, true, "direct")
  → Reply with title, size estimate, queue position

Scheduler EhEngine tick:
  → repo.get_pending_tasks_by_type(Ehentai, 1)
  → EhTaskKey::parse(task.value) → query="female:elf", filter_sig="r4"
  → List subs → aggregate EhFilter (min_rating=4)
  → has_rating_filter → 48h scan mode
  → client.search("female:elf", 0, page=0..5) until posted_ts < cutoff
  → Dedup against pushed_gids
  → client.get_metadata(new_galleries) → Vec<EhGallery>
  → Filter by rating >= 4
  → For each new gallery, for each sub:
    → repo.enqueue_download(chat_id, gid, token, title, sub.telegraph, "subscription")
    → Update pushed_gids, latest_posted_ts
  → repo.update_subscription_latest_data
  → repo.update_task_after_poll(next_poll)

Scheduler EhDownloadProcessor poll:
  → reset_stale_downloading() (crash recovery)
  → window_start = now - 24h
  → downloaded_bytes = repo.get_downloaded_bytes_in_window(window_start)
  → budget_remaining = 10GB - downloaded_bytes
  → if budget_remaining <= 0: skip, wait for next poll
  → download = repo.get_next_pending_download()
  → if none: skip
  → mark_download_started(id)
  → client.get_metadata(gid, token) → check filesize vs budget
  → if filesize > budget_remaining: skip, wait
  → client.get_archiver_key(gid, token)
  → client.download_archive(gid, token, key, tempfile)
  → notifier.notify_with_document(chat, zip_path, "title.zip", caption)
  → If telegraph: extract images, telegraph.upload_image each, telegraph.create_gallery_page, notify_with_text(chat, page_url)
  → mark_download_done(id, actual_bytes)
  → On failure: mark_download_failed(id, error), retry_count++, abandon if >= max
```

## Error Handling

- **Search/parse failures**: log with tracing, skip tick, reschedule
- **Metadata API errors**: skip individual galleries with error in response, continue with rest
- **Archive download failures**: mark download as "failed", increment retry_count, stays pending for retry; abandon after max_retry_count (status → "failed" permanently)
- **Telegram send failures**: mark download as "failed", will be retried on next processor poll
- **Rate limit exceeded**: download stays "pending", processor waits for budget to replenish
- **Crash recovery**: any download stuck in "downloading" state on startup is reset to "pending"
- **Telegraph upload failures**: skip individual images, create page with available images, log failures
- **User-facing messages**: short friendly text, never expose raw errors (per AGENTS.md Telegram Safety)

## Telegraph Image Extraction

When Telegraph upload is enabled for a subscription, after downloading the archive ZIP:

1. Extract images from ZIP using the `zip` crate (already a dependency)
2. Filter to image files only (`.jpg`, `.png`, `.gif`, `.webp` extensions)
3. Upload each image via `telegraph.upload_image()` (max ~6MB per image)
4. If content size would exceed 64KB, split into multiple pages with "Next Page" links
5. Create page with `<img>` nodes pointing to uploaded URLs
6. Send page URL to chat via `notifier.notify_with_text()`

Images are uploaded to Telegraph's `/upload` endpoint directly (not Catbox.moe). If Telegraph upload fails for an image, that image is skipped and the page is created with remaining images.

## Testing

- `eh_client::parser`: search result parsing, archiver redirect extraction, archiver_key extraction (unit tests with HTML fixtures)
- `eh_client::client`: mock HTTP responses for search/metadata/archive (integration tests)
- `eh_client::telegraph`: mock Telegraph API (unit tests)
- `EhTaskKey`: to_task_value/parse roundtrip, filter_sig encoding
- `EhFilter`: matches, aggregate, has_rating_filter, task_value_signature
- `EhTagState`: pushed_gids dedup, latest_posted_ts update
- `EhDownloadQueue` repo: enqueue, get_next_pending, mark_started/done/failed, rate-window accounting, reset_stale
- `EhEngine`: tick logic with mocked client (48h scan, enqueue, state transitions)
- `EhDownloadProcessor`: drain logic with mocked client (rate limit check, download, send, state transitions)
- Bot command parsing: `/esub` filter arg parsing, `/eunsub` internal key resolution, `/edl` URL/GID parsing

## Constraints

- **Search rate limit**: 1 search per 3 seconds — engine must sleep between searches
- **Metadata API**: max 25 per request, 4-5 sequential before 5s cooldown
- **Archive download**: original costs GP/credits; resample is free. Config `image_resolution` defaults to `780x` (free resample)
- **Image view limits**: 5000/day — archive download does NOT count against this (only image page views do)
- **Telegram document size**: 50MB bot limit. Large archives (>50MB) will fail to send — log and skip, rely on Telegraph fallback if enabled
- **Telegraph content limit**: 64KB serialized JSON per page — split into multiple pages if needed
- **CloudFlare**: both sites use CF; proper User-Agent and cookie headers required; exhentai may need IPv4
- **Download rate limit**: configurable (default 10GB/24h). Downloads exceeding budget are queued, not rejected. Budget calculated from `completed_at` timestamps in the rolling window.
