# EH Review Fixes Implementation Plan

> **For agentic workers:** Use the subagent-driven-development skill to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix the reviewed E-Hentai pipeline correctness issues while keeping EH enabled by default and adding an explicit `ehentai.enabled=false` opt-out.

**Architecture:** Keep the existing collect/download/upload/publish pipeline. Add the smallest durable state needed: subscription overflow backlog, publish sent markers, merge-safe queue upserts, and config/bot gating for Telegraph availability.

**Tech Stack:** Rust 1.94, SeaORM migrations/entities, SQLite-backed repo tests, teloxide MarkdownV2, wiremock scheduler/client tests, Makefile CI.

---

## File Structure

- Modify `src/db/types/state.rs`: add `EhPendingGallery`, pending backlog fields, and state tests.
- Modify `src/config.rs` and `config.toml.example`: add `ehentai.enabled` defaulting to true and update tests/docs.
- Modify `migration/src/m20260628_000100_eh_unique_constraint.rs`: improve duplicate-row keeper selection before unique index creation.
- Create `migration/src/m20260629_000000_eh_review_fixes.rs`: add `archive_sent_at` and `telegraph_sent_at` columns.
- Modify `migration/src/lib.rs`: register the new migration after the existing EH unique-constraint migration.
- Modify `src/db/entities/eh_download_queue.rs`: add nullable sent-marker fields.
- Modify `src/db/repo.rs`: add sent-marker columns to test schema.
- Modify `src/db/repo/eh_download_queue.rs`: merge enqueue requests, add sent-marker/defer helpers, broaden quota, preserve markers through stale reset, clear markers on terminal reset.
- Modify `src/db/repo/eh_integration_tests.rs`: add quota and lifecycle coverage using new states/markers.
- Modify `src/scheduler/eh_engine.rs`: consume pending backlog, disable Telegraph enqueue without token, defer not-notifiable chats without retry increment, make publish idempotent, handle missing ZIP as retry/re-download, fallback Telegraph permanent failure to archive-only.
- Modify `eh_client/src/client.rs` and `eh_client/tests/integration.rs`: make direct image fallback all-or-error.
- Modify `eh_client/src/telegraph.rs`: reserve continuation-link bytes during page splitting.
- Modify `src/bot/mod.rs`, `src/bot/handler.rs`, `src/bot/commands.rs`, `src/bot/handlers/subscription/ehentai.rs`, `src/bot/handlers/subscription/helpers.rs`, `src/bot/handlers/subscription/list.rs`, and `src/bot/handlers/download.rs`: add Telegraph availability gating, EH MarkdownV2 escaping, `/download` ambiguity rejection, enqueue error reporting, URL-only `/edl` help, and `telegraph=on` persistence.

---

### Task 1: State, Schema, and Explicit EH Disable Foundations

**Files:**
- Modify: `src/db/types/state.rs`
- Modify: `src/config.rs`
- Modify: `config.toml.example`
- Modify: `src/db/entities/eh_download_queue.rs`
- Modify: `src/db/repo.rs`
- Modify: `migration/src/m20260628_000100_eh_unique_constraint.rs`
- Create: `migration/src/m20260629_000000_eh_review_fixes.rs`
- Modify: `migration/src/lib.rs`

- [ ] **Step 1: Write failing state/config/schema tests**

Add these tests before implementation:

```rust
// src/db/types/state.rs, inside existing #[cfg(test)] mod tests
#[test]
fn test_eh_tag_state_pending_defaults_empty() {
    let state = EhTagState::cleared();
    assert!(state.pending_galleries.is_empty());
    assert_eq!(state.pending_high_water_ts, 0);
}

#[test]
fn test_eh_pending_gallery_roundtrip() {
    let state = EhTagState {
        pushed_gids: vec![1],
        latest_posted_ts: 100,
        pending_galleries: vec![EhPendingGallery {
            gid: 2,
            token: "tok".to_string(),
            title: "Title".to_string(),
            posted: 200,
        }],
        pending_high_water_ts: 200,
    };
    let json = serde_json::to_string(&state).unwrap();
    let decoded: EhTagState = serde_json::from_str(&json).unwrap();
    assert_eq!(decoded.pending_galleries[0].gid, 2);
    assert_eq!(decoded.pending_high_water_ts, 200);
}

// src/config.rs, inside an existing or new #[cfg(test)] mod tests
#[test]
fn test_eh_enabled_defaults_true() {
    let cfg = EhentaiConfig::default();
    assert!(cfg.enabled);
    assert!(cfg.is_enabled());
}

#[test]
fn test_eh_enabled_false_disables_supported_site() {
    let cfg = EhentaiConfig {
        enabled: false,
        site: "e-hentai".to_string(),
        ..Default::default()
    };
    assert!(!cfg.is_enabled());
}
```

Also add one repo smoke test that inserts a queue row with the new marker columns set to `NULL` using `tests_helpers::setup_test_db()`:

```rust
// src/db/repo/eh_download_queue.rs, inside existing tests module
#[tokio::test]
async fn test_queue_schema_has_publish_marker_columns() {
    let repo = tests_helpers::setup_test_db().await.unwrap();
    let entry = repo
        .enqueue_eh_download(-100, 42, "tok", "Title", false, SOURCE_DIRECT)
        .await
        .unwrap();
    assert!(entry.archive_sent_at.is_none());
    assert!(entry.telegraph_sent_at.is_none());
}
```

- [ ] **Step 2: Run tests and verify they fail for missing symbols/columns**

Run:

```powershell
cargo test -p pixivbot --no-default-features eh_tag_state_pending_defaults_empty
cargo test -p pixivbot --no-default-features eh_enabled_false_disables_supported_site
cargo test -p pixivbot --no-default-features test_queue_schema_has_publish_marker_columns
```

Expected: FAIL with compile errors mentioning `EhPendingGallery`, `pending_galleries`, `pending_high_water_ts`, `enabled`, or `archive_sent_at`/`telegraph_sent_at`.

- [ ] **Step 3: Add state/config/entity/migration implementation**

Implement these exact model changes:

```rust
// src/db/types/state.rs
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct EhPendingGallery {
    pub gid: u64,
    pub token: String,
    pub title: String,
    pub posted: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct EhTagState {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pushed_gids: Vec<u64>,
    #[serde(default)]
    pub latest_posted_ts: i64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_galleries: Vec<EhPendingGallery>,
    #[serde(default)]
    pub pending_high_water_ts: i64,
}

impl EhTagState {
    pub fn cleared() -> Self {
        Self {
            pushed_gids: Vec::new(),
            latest_posted_ts: 0,
            pending_galleries: Vec::new(),
            pending_high_water_ts: 0,
        }
    }
}
```

```rust
// src/config.rs
#[derive(Debug, Clone, Deserialize)]
pub struct EhentaiConfig {
    #[serde(default = "default_eh_enabled")]
    pub enabled: bool,
    // keep existing fields below
}

fn default_eh_enabled() -> bool {
    true
}

impl EhentaiConfig {
    pub fn is_enabled(&self) -> bool {
        self.enabled && matches!(self.site.as_str(), "exhentai" | "e-hentai")
    }
}
```

Update `EhentaiConfig::default()` to set `enabled: default_eh_enabled()`. Update `config.toml.example` to document `enabled = true` and `enabled = false` as explicit opt-out; remove wording that says omitting `[ehentai]` disables EH.

```rust
// src/db/entities/eh_download_queue.rs Model
pub archive_sent_at: Option<DateTime>,
pub telegraph_sent_at: Option<DateTime>,
```

Update `src/db/repo.rs::tests_helpers::setup_test_db()` `eh_download_queue` DDL with:

```sql
archive_sent_at TIMESTAMP NULL,
telegraph_sent_at TIMESTAMP NULL
```

Create `migration/src/m20260629_000000_eh_review_fixes.rs`:

```rust
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(EhDownloadQueue::Table)
                    .add_column(ColumnDef::new(EhDownloadQueue::ArchiveSentAt).timestamp().null())
                    .add_column(ColumnDef::new(EhDownloadQueue::TelegraphSentAt).timestamp().null())
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(EhDownloadQueue::Table)
                    .drop_column(EhDownloadQueue::TelegraphSentAt)
                    .drop_column(EhDownloadQueue::ArchiveSentAt)
                    .to_owned(),
            )
            .await
    }
}

#[derive(DeriveIden)]
enum EhDownloadQueue {
    Table,
    ArchiveSentAt,
    TelegraphSentAt,
}
```

Register it last in `migration/src/lib.rs`.

Modify `migration/src/m20260628_000100_eh_unique_constraint.rs` duplicate cleanup SQL to keep highest-progress row:

```sql
DELETE FROM eh_download_queue
WHERE id NOT IN (
    SELECT id FROM (
        SELECT id,
               ROW_NUMBER() OVER (
                   PARTITION BY chat_id, gid
                   ORDER BY
                     CASE status
                       WHEN 'publishing' THEN 1
                       WHEN 'uploaded' THEN 2
                       WHEN 'uploading' THEN 3
                       WHEN 'downloaded' THEN 4
                       WHEN 'downloading' THEN 5
                       WHEN 'pending' THEN 6
                       WHEN 'done' THEN 7
                       WHEN 'failed' THEN 8
                       ELSE 9
                     END,
                     COALESCE(completed_at, started_at, created_at) DESC,
                     id DESC
               ) AS rn
        FROM eh_download_queue
    ) ranked
    WHERE rn = 1
)
```

- [ ] **Step 4: Run focused tests**

Run:

```powershell
cargo test -p pixivbot --no-default-features eh_tag_state_pending_defaults_empty
cargo test -p pixivbot --no-default-features eh_pending_gallery_roundtrip
cargo test -p pixivbot --no-default-features eh_enabled_defaults_true
cargo test -p pixivbot --no-default-features eh_enabled_false_disables_supported_site
cargo test -p pixivbot --no-default-features test_queue_schema_has_publish_marker_columns
```

Expected: PASS.

- [ ] **Step 5: Commit Task 1**

Run:

```powershell
git add src/db/types/state.rs src/config.rs config.toml.example src/db/entities/eh_download_queue.rs src/db/repo.rs migration/src/m20260628_000100_eh_unique_constraint.rs migration/src/m20260629_000000_eh_review_fixes.rs migration/src/lib.rs src/db/repo/eh_download_queue.rs; git commit -m "fix: add EH recovery state foundations" -m "Add EH pending backlog state, explicit enabled config, publish marker schema, and safer queue duplicate cleanup."
```

Expected: commit succeeds.

---

### Task 2: Queue Repo Merge Semantics, Markers, Quota, and Defer Helpers

**Files:**
- Modify: `src/db/repo/eh_download_queue.rs`
- Modify: `src/db/repo/eh_integration_tests.rs`

- [ ] **Step 1: Write failing queue tests**

Add tests to `src/db/repo/eh_download_queue.rs`:

```rust
#[tokio::test]
async fn test_enqueue_merges_telegraph_and_direct_source() {
    let repo = tests_helpers::setup_test_db().await.unwrap();
    let first = repo
        .enqueue_eh_download(-100, 10, "old", "Old", false, SOURCE_SUBSCRIPTION)
        .await
        .unwrap();
    let merged = repo
        .enqueue_eh_download(-100, 10, "new", "New", true, SOURCE_DIRECT)
        .await
        .unwrap();

    assert_eq!(first.id, merged.id);
    assert!(merged.telegraph);
    assert_eq!(merged.source, SOURCE_DIRECT);
    assert_eq!(merged.token, "new");
    assert_eq!(merged.title, "New");
}

#[tokio::test]
async fn test_downloaded_bytes_window_counts_all_downloaded_states() {
    let repo = tests_helpers::setup_test_db().await.unwrap();
    for (gid, status, size) in [
        (1, STATUS_DOWNLOADED, 100),
        (2, STATUS_UPLOADING, 200),
        (3, STATUS_UPLOADED, 300),
        (4, STATUS_PUBLISHING, 400),
        (5, STATUS_DONE, 500),
    ] {
        let model = repo
            .enqueue_eh_download(-100, gid, "tok", "Title", false, SOURCE_DIRECT)
            .await
            .unwrap();
        Entity::update_many()
            .col_expr(Column::Status, Expr::value(status))
            .col_expr(Column::FileSize, Expr::value(size))
            .col_expr(Column::CompletedAt, Expr::value(Utc::now().naive_utc()))
            .filter(Column::Id.eq(model.id))
            .exec(&repo.db)
            .await
            .unwrap();
    }

    let bytes = repo.get_eh_downloaded_bytes_in_window(24).await.unwrap();
    assert_eq!(bytes, 1500);
}

#[tokio::test]
async fn test_publish_markers_survive_stale_reset_and_clear_on_terminal_reset() {
    let repo = tests_helpers::setup_test_db().await.unwrap();
    let model = repo
        .enqueue_eh_download(-100, 20, "tok", "Title", false, SOURCE_DIRECT)
        .await
        .unwrap();
    repo.mark_eh_archive_sent(model.id).await.unwrap();
    repo.defer_eh_download(model.id, STATUS_PUBLISHING, 60).await.unwrap();
    repo.reset_stale_eh_downloads(0).await.unwrap();
    let preserved = Entity::find_by_id(model.id).one(&repo.db).await.unwrap().unwrap();
    assert!(preserved.archive_sent_at.is_some());

    repo.mark_eh_download_failed(model.id, "failed").await.unwrap();
    let reset = repo
        .enqueue_eh_download(-100, 20, "new", "New", false, SOURCE_DIRECT)
        .await
        .unwrap();
    assert!(reset.archive_sent_at.is_none());
    assert!(reset.telegraph_sent_at.is_none());
}

#[tokio::test]
async fn test_defer_does_not_increment_retry_count() {
    let repo = tests_helpers::setup_test_db().await.unwrap();
    let model = repo
        .enqueue_eh_download(-100, 30, "tok", "Title", false, SOURCE_DIRECT)
        .await
        .unwrap();
    repo.defer_eh_download(model.id, STATUS_PENDING, 60).await.unwrap();
    let deferred = Entity::find_by_id(model.id).one(&repo.db).await.unwrap().unwrap();
    assert_eq!(deferred.status, STATUS_PENDING);
    assert_eq!(deferred.retry_count, 0);
    assert!(deferred.next_retry_at.is_some());
}
```

Use existing imports in the test module; add `use chrono::Utc;`, `use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};`, and `use sea_orm::sea_query::Expr;` if absent.

- [ ] **Step 2: Run tests and verify failure**

Run:

```powershell
cargo test -p pixivbot --no-default-features test_enqueue_merges_telegraph_and_direct_source
cargo test -p pixivbot --no-default-features test_downloaded_bytes_window_counts_all_downloaded_states
cargo test -p pixivbot --no-default-features test_publish_markers_survive_stale_reset_and_clear_on_terminal_reset
cargo test -p pixivbot --no-default-features test_defer_does_not_increment_retry_count
```

Expected: FAIL because marker/defer methods do not exist and quota still only counts `done`.

- [ ] **Step 3: Implement repo methods and merge logic**

Implement these public methods in `impl Repo` in `src/db/repo/eh_download_queue.rs`:

```rust
pub async fn mark_eh_archive_sent(&self, id: i32) -> Result<()> {
    Entity::update_many()
        .col_expr(Column::ArchiveSentAt, Expr::value(Utc::now().naive_utc()))
        .filter(Column::Id.eq(id))
        .exec(&self.db)
        .await?;
    Ok(())
}

pub async fn mark_eh_telegraph_sent(&self, id: i32) -> Result<()> {
    Entity::update_many()
        .col_expr(Column::TelegraphSentAt, Expr::value(Utc::now().naive_utc()))
        .filter(Column::Id.eq(id))
        .exec(&self.db)
        .await?;
    Ok(())
}

pub async fn defer_eh_download(&self, id: i32, target_status: &str, delay_secs: i64) -> Result<()> {
    Entity::update_many()
        .col_expr(Column::Status, Expr::value(target_status))
        .col_expr(Column::NextRetryAt, Expr::value(Utc::now().naive_utc() + chrono::Duration::seconds(delay_secs)))
        .filter(Column::Id.eq(id))
        .exec(&self.db)
        .await?;
    Ok(())
}
```

Update `enqueue_eh_download()` so existing non-terminal rows are updated instead of returned unchanged:

```rust
let merged_telegraph = existing.telegraph || telegraph;
let merged_source = if existing.source == SOURCE_DIRECT || source == SOURCE_DIRECT {
    SOURCE_DIRECT
} else {
    SOURCE_SUBSCRIPTION
};
let source_upgraded_to_direct = existing.source != SOURCE_DIRECT && merged_source == SOURCE_DIRECT;
let telegraph_upgraded = !existing.telegraph && merged_telegraph;
let reset_for_new_requirement = source_upgraded_to_direct
    || (telegraph_upgraded && matches!(existing.status.as_str(), STATUS_UPLOADED | STATUS_PUBLISHING));
```

When `reset_for_new_requirement` is true, set `status=STATUS_PENDING`, clear `file_size`, `zip_path`, `telegraph_url`, `archive_sent_at`, `telegraph_sent_at`, `started_at`, `completed_at`, `next_retry_at`, `error`, and `retry_count`. Otherwise preserve status/progress and update `telegraph/source/token/title`.

Wrap the insert path so a unique violation reselects by `(chat_id, gid)` and applies the same merge. Use the existing read-then-insert path first; if insert returns `DbErr`, reselect and merge before returning. Do not swallow unrelated database errors when the row still cannot be found after insert failure.

Update `get_eh_downloaded_bytes_in_window()` filter to include:

```rust
Column::Status.is_in([
    STATUS_DOWNLOADED,
    STATUS_UPLOADING,
    STATUS_UPLOADED,
    STATUS_PUBLISHING,
    STATUS_DONE,
])
```

Ensure terminal reset clears `archive_sent_at` and `telegraph_sent_at`. Ensure `reset_stale_eh_downloads()` preserves sent markers.

- [ ] **Step 4: Run queue tests**

Run:

```powershell
cargo test -p pixivbot --no-default-features eh_download_queue
```

Expected: PASS.

- [ ] **Step 5: Commit Task 2**

Run:

```powershell
git add src/db/repo/eh_download_queue.rs src/db/repo/eh_integration_tests.rs; git commit -m "fix: make EH download queue resumable" -m "Merge duplicate queue requests, track publish markers, count downloaded bytes across active states, and add defer helpers without retry increments."
```

Expected: commit succeeds.

---

### Task 3: Scheduler Collect Backlog and Telegraph Enqueue Gating

**Files:**
- Modify: `src/scheduler/eh_engine.rs`
- Modify: `src/db/types/state.rs`

- [ ] **Step 1: Write failing scheduler collect tests**

Add an integration test in `src/scheduler/eh_engine.rs` `integration_tests` module:

```rust
#[tokio::test]
async fn test_collect_overflow_pending_enqueued_on_next_tick() {
    let repo = tests_helpers::setup_test_db().await.unwrap();
    setup_chat(&repo, -100, true).await;
    let eh_server = MockServer::start().await;
    let notifier_server = MockServer::start().await;
    let eh_client = make_eh_client(&eh_server);
    let notifier = make_notifier(&notifier_server);
    let config = Arc::new(make_config());

    repo.upsert_eh_subscription(-100, "artist:test", 0, None)
        .await
        .unwrap();

    mock_eh_search_with_four_galleries(&eh_server).await;
    mock_eh_metadata_for_four_galleries(&eh_server).await;

    let engine = EhEngine::new(repo.clone(), eh_client, notifier, config);
    engine.tick().await.unwrap();

    let queued_after_first = repo.count_pending_eh_downloads().await.unwrap();
    assert_eq!(queued_after_first, 3);

    engine.tick().await.unwrap();
    let queued_after_second = repo.count_pending_eh_downloads().await.unwrap();
    assert_eq!(queued_after_second, 4);
}
```

Add these local wiremock helpers in the same test module:

```rust
async fn mock_eh_search_with_four_galleries(server: &MockServer) {
    let html = r#"
    <div class="gl1t"><a href="https://e-hentai.org/g/1001/aaaaaaaaaa/"><div class="glink">Gallery 1</div></a></div>
    <div class="gl1t"><a href="https://e-hentai.org/g/1002/bbbbbbbbbb/"><div class="glink">Gallery 2</div></a></div>
    <div class="gl1t"><a href="https://e-hentai.org/g/1003/cccccccccc/"><div class="glink">Gallery 3</div></a></div>
    <div class="gl1t"><a href="https://e-hentai.org/g/1004/dddddddddd/"><div class="glink">Gallery 4</div></a></div>
    "#;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(html))
        .mount(server)
        .await;
}

async fn mock_eh_metadata_for_four_galleries(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/api.php"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "gmetadata": [
                {"gid": 1001, "token": "aaaaaaaaaa", "title": "Gallery 1", "title_jpn": null, "category": "Doujinshi", "thumb": "https://ehgt.org/t/1.jpg", "uploader": "tester", "posted": "100", "filecount": "10", "filesize": 1000, "expunged": false, "rating": "4.0", "tags": ["artist:test"]},
                {"gid": 1002, "token": "bbbbbbbbbb", "title": "Gallery 2", "title_jpn": null, "category": "Doujinshi", "thumb": "https://ehgt.org/t/2.jpg", "uploader": "tester", "posted": "200", "filecount": "10", "filesize": 1000, "expunged": false, "rating": "4.0", "tags": ["artist:test"]},
                {"gid": 1003, "token": "cccccccccc", "title": "Gallery 3", "title_jpn": null, "category": "Doujinshi", "thumb": "https://ehgt.org/t/3.jpg", "uploader": "tester", "posted": "300", "filecount": "10", "filesize": 1000, "expunged": false, "rating": "4.0", "tags": ["artist:test"]},
                {"gid": 1004, "token": "dddddddddd", "title": "Gallery 4", "title_jpn": null, "category": "Doujinshi", "thumb": "https://ehgt.org/t/4.jpg", "uploader": "tester", "posted": "400", "filecount": "10", "filesize": 1000, "expunged": false, "rating": "4.0", "tags": ["artist:test"]}
            ]
        })))
        .mount(server)
        .await;
}
```

Add a state unit test in `src/db/types/state.rs`:

```rust
#[test]
fn test_eh_tag_state_pending_prevents_cursor_advance() {
    let mut state = EhTagState::cleared();
    state.pending_galleries.push(EhPendingGallery {
        gid: 4,
        token: "tok4".to_string(),
        title: "Fourth".to_string(),
        posted: 400,
    });
    state.pending_high_water_ts = 400;
    assert_eq!(state.latest_posted_ts, 0);
    assert_eq!(state.pending_galleries.len(), 1);
}
```

- [ ] **Step 2: Run tests and verify failure**

Run:

```powershell
cargo test -p pixivbot --no-default-features test_collect_overflow_pending_enqueued_on_next_tick
cargo test -p pixivbot --no-default-features test_eh_tag_state_pending_prevents_cursor_advance
```

Expected: scheduler test FAILS because the fourth gallery is filtered out by cursor advancement.

- [ ] **Step 3: Implement backlog consumption in `process_eh_sub()`**

Refactor `EhEngine::process_eh_sub()` to use this algorithm:

```rust
let max_push = self.config.max_push_per_tick;
let mut remaining_slots = max_push;
let mut state = eh_tag_subscription_state(sub).unwrap_or_else(EhTagState::cleared);

let mut still_pending = Vec::new();
for pending in state.pending_galleries.drain(..) {
    if remaining_slots == 0 {
        still_pending.push(pending);
        continue;
    }
    self.repo
        .enqueue_eh_download(sub.chat_id, pending.gid as i64, &pending.token, &pending.title, telegraph_default, SOURCE_SUBSCRIPTION)
        .await?;
    state.add_pushed_gid(pending.gid);
    remaining_slots -= 1;
}
state.pending_galleries = still_pending;
if !state.pending_galleries.is_empty() {
    self.repo.update_subscription_state(sub.id, SubscriptionState::EhTag(state)).await?;
    return Ok(());
}

let eligible: Vec<EhPendingGallery> = new_galleries
    .into_iter()
    .filter(|g| !state.pushed_gids.contains(&g.gid))
    .map(|g| EhPendingGallery { gid: g.gid, token: g.token, title: g.title, posted: g.posted })
    .collect();

state.pending_high_water_ts = eligible.iter().map(|g| g.posted).max().unwrap_or(state.pending_high_water_ts);
for gallery in eligible.into_iter() {
    if remaining_slots == 0 {
        state.pending_galleries.push(gallery);
        continue;
    }
    self.repo
        .enqueue_eh_download(
            sub.chat_id,
            gallery.gid as i64,
            &gallery.token,
            &gallery.title,
            telegraph_default,
            SOURCE_SUBSCRIPTION,
        )
        .await?;
    state.add_pushed_gid(gallery.gid);
    state.latest_posted_ts = state.latest_posted_ts.max(gallery.posted);
    remaining_slots -= 1;
}
if state.pending_galleries.is_empty() {
    state.latest_posted_ts = state.latest_posted_ts.max(state.pending_high_water_ts);
    state.pending_high_water_ts = 0;
}
state.trim_pushed(self.config.pushed_gids_cap);
```

Keep existing filter behavior for rating/pages. Compute `telegraph_default` as:

```rust
let telegraph_available = self.config.telegraph_access_token.is_some();
let telegraph_default = telegraph_available
    && (self.config.upload_telegraph || sub_filter.map(|f| f.telegraph).unwrap_or(false));
```

This prevents scheduler-created Telegraph queue entries when no upload worker can exist.

- [ ] **Step 4: Run collect tests**

Run:

```powershell
cargo test -p pixivbot --no-default-features test_collect_overflow_pending_enqueued_on_next_tick
```

Expected: PASS.

- [ ] **Step 5: Commit Task 3**

Run:

```powershell
git add src/scheduler/eh_engine.rs src/db/types/state.rs; git commit -m "fix: preserve EH collect overflow" -m "Persist subscription overflow galleries and avoid Telegraph enqueue when no upload token is configured."
```

Expected: commit succeeds.

---

### Task 4: Scheduler Worker Recovery, Publish Idempotency, Missing ZIP Handling, and Archive Fallback

**Files:**
- Modify: `src/scheduler/eh_engine.rs`
- Modify: `src/db/repo/eh_download_queue.rs`

- [ ] **Step 1: Write failing worker tests**

Add tests to `src/scheduler/eh_engine.rs` `integration_tests`:

```rust
#[tokio::test]
async fn test_publish_retry_skips_archive_after_marker() {
    let repo = tests_helpers::setup_test_db().await.unwrap();
    setup_chat(&repo, -100, true).await;
    let tg_server = MockServer::start().await;
    let notifier = make_notifier(&tg_server);
    let config = Arc::new(make_config());
    let temp_dir = tempfile::tempdir().unwrap();
    let zip_path = temp_dir.path().join("501.zip");
    create_test_zip(&zip_path, 2);
    let entry = insert_queue_entry(&repo, -100, 501, "tok", "Title", true, STATUS_UPLOADED, Some(zip_path.to_str().unwrap()), Some("https://telegra.ph/page")).await;

    repo.mark_eh_archive_sent(entry.id).await.unwrap();
    mock_tg_send_message(&tg_server).await;

    let worker = EhPublishWorker::new(repo.clone(), notifier, config);
    worker.tick().await.unwrap();

    let model = Entity::find_by_id(entry.id).one(&repo.db).await.unwrap().unwrap();
    assert_eq!(model.status, STATUS_DONE);
    assert!(model.telegraph_sent_at.is_some());
}

#[tokio::test]
async fn test_publish_missing_zip_retries_download_instead_of_done() {
    let repo = tests_helpers::setup_test_db().await.unwrap();
    setup_chat(&repo, -100, true).await;
    let tg_server = MockServer::start().await;
    let notifier = make_notifier(&tg_server);
    let config = Arc::new(make_config());
    let entry = insert_queue_entry(&repo, -100, 502, "tok", "Title", false, STATUS_DOWNLOADED, Some("data/test_cache/missing.zip"), None).await;

    let worker = EhPublishWorker::new(repo.clone(), notifier, config);
    worker.tick().await.unwrap();

    let model = Entity::find_by_id(entry.id).one(&repo.db).await.unwrap().unwrap();
    assert_eq!(model.status, STATUS_PENDING);
    assert_eq!(model.retry_count, 1);
    assert!(model.next_retry_at.is_some());
}

#[tokio::test]
async fn test_publish_no_surface_fails_not_done() {
    let repo = tests_helpers::setup_test_db().await.unwrap();
    setup_chat(&repo, -100, true).await;
    let tg_server = MockServer::start().await;
    let notifier = make_notifier(&tg_server);
    let mut cfg = make_config();
    cfg.send_archive = false;
    let worker = EhPublishWorker::new(repo.clone(), notifier, Arc::new(cfg));
    let temp_dir = tempfile::tempdir().unwrap();
    let zip_path = temp_dir.path().join("503.zip");
    create_test_zip(&zip_path, 2);
    let entry = insert_queue_entry(&repo, -100, 503, "tok", "Title", false, STATUS_DOWNLOADED, Some(zip_path.to_str().unwrap()), None).await;

    worker.tick().await.unwrap();
    let model = Entity::find_by_id(entry.id).one(&repo.db).await.unwrap().unwrap();
    assert_ne!(model.status, STATUS_DONE);
    assert!(model.error.unwrap().contains("no EH publish surface"));
}

#[tokio::test]
async fn test_chat_disabled_defer_does_not_increment_retry() {
    let repo = tests_helpers::setup_test_db().await.unwrap();
    setup_chat(&repo, -100, false).await;
    let tg_server = MockServer::start().await;
    let notifier = make_notifier(&tg_server);
    let config = Arc::new(make_config());
    let temp_dir = tempfile::tempdir().unwrap();
    let zip_path = temp_dir.path().join("504.zip");
    create_test_zip(&zip_path, 2);
    let entry = insert_queue_entry(&repo, -100, 504, "tok", "Title", false, STATUS_DOWNLOADED, Some(zip_path.to_str().unwrap()), None).await;

    let worker = EhPublishWorker::new(repo.clone(), notifier, config);
    worker.tick().await.unwrap();
    let model = Entity::find_by_id(entry.id).one(&repo.db).await.unwrap().unwrap();
    assert_eq!(model.status, STATUS_DOWNLOADED);
    assert_eq!(model.retry_count, 0);
    assert!(model.next_retry_at.is_some());
}
```

Add or update an upload test:

```rust
#[tokio::test]
async fn test_upload_permanent_failure_falls_back_to_archive() {
    let repo = tests_helpers::setup_test_db().await.unwrap();
    setup_chat(&repo, -100, true).await;
    let tg_server = MockServer::start().await;
    let notifier = make_notifier(&tg_server);
    let telegraph = make_telegraph_client(&tg_server);
    let mut cfg = make_config();
    cfg.max_retry_count = 0;
    cfg.send_archive = true;
    let temp_dir = tempfile::tempdir().unwrap();
    let zip_path = temp_dir.path().join("505.zip");
    create_test_zip(&zip_path, 2);
    let entry = insert_queue_entry(&repo, -100, 505, "tok", "Title", true, STATUS_DOWNLOADED, Some(zip_path.to_str().unwrap()), None).await;

    let worker = EhUploadWorker::new(repo.clone(), telegraph, notifier, Arc::new(cfg));
    worker.tick().await.unwrap();
    let model = Entity::find_by_id(entry.id).one(&repo.db).await.unwrap().unwrap();
    assert_eq!(model.status, STATUS_DOWNLOADED);
    assert!(!model.telegraph);
}
```

- [ ] **Step 2: Run tests and verify failure**

Run:

```powershell
cargo test -p pixivbot --no-default-features test_publish_missing_zip_retries_download_instead_of_done
cargo test -p pixivbot --no-default-features test_publish_retry_skips_archive_after_marker
cargo test -p pixivbot --no-default-features test_chat_disabled_defer_does_not_increment_retry
cargo test -p pixivbot --no-default-features test_upload_permanent_failure_falls_back_to_archive
```

Expected: FAIL because publish markers are ignored, missing ZIP is marked done, disabled chat consumes retry, or upload failure does not fallback.

- [ ] **Step 3: Implement worker recovery behavior**

In `EhDownloadWorker::process`, `EhUploadWorker::process`, and `EhPublishWorker::process`, treat `get_chat_if_should_notify()` returning `None` as defer:

```rust
self.repo.defer_eh_download(entry.id, STATUS_DOWNLOADED, self.config.download_poll_interval_sec as i64).await?;
return Ok(());
```

Use the correct ready status for each worker: download → `STATUS_PENDING`, upload → `STATUS_DOWNLOADED`, publish → `STATUS_UPLOADED` when `telegraph_url.is_some()` else `STATUS_DOWNLOADED`.

In `EhUploadWorker::tick`, when a failure would exceed retry budget and `self.config.send_archive && entry.zip_path.is_some()`, update the row instead of marking failed:

```rust
Entity::update_many()
    .col_expr(Column::Telegraph, Expr::value(false))
    .col_expr(Column::Status, Expr::value(STATUS_DOWNLOADED))
    .col_expr(Column::Error, Expr::value(format!("Telegraph upload failed, falling back to archive: {err:#}")))
    .col_expr(Column::NextRetryAt, Expr::value(Option::<NaiveDateTime>::None))
    .filter(Column::Id.eq(entry.id))
    .exec(&self.repo.db)
    .await?;
```

In `EhPublishWorker::process`, compute surface requirements before sending:

```rust
let archive_required = self.config.send_archive && entry.archive_sent_at.is_none();
let telegraph_required = entry.telegraph_url.is_some() && entry.telegraph_sent_at.is_none();
if archive_required {
    let zip_path = entry.zip_path.as_deref().ok_or_else(|| MissingEhZip)?;
    if !Path::new(zip_path).exists() {
        return Err(MissingEhZip.into());
    }
}
if !archive_required && !telegraph_required && entry.archive_sent_at.is_none() && entry.telegraph_sent_at.is_none() {
    bail!("no EH publish surface for queue entry {}", entry.id);
}
```

After successful document send, call `self.repo.mark_eh_archive_sent(entry.id).await?`. After successful link send, call `self.repo.mark_eh_telegraph_sent(entry.id).await?`. Before sending, skip surfaces whose markers are already present. In `tick`, if the error is `MissingEhZip`, call:

```rust
let (updated, permanent) = self
    .repo
    .schedule_eh_retry(
        entry.id,
        STATUS_PENDING,
        &format!("cached EH ZIP is missing for {}", entry.title),
        self.config.max_retry_count,
    )
    .await?;
if permanent {
    self.cleanup_zip(&updated).await;
    self.notifier
        .send_text(entry.chat_id, &format!("⚠️ 下载失败: {}\n原因: cached EH ZIP is missing", entry.title))
        .await?;
}
```

For non-`MissingEhZip` errors, keep the existing target status logic: `STATUS_UPLOADED` when `entry.telegraph_url.is_some()`, otherwise `STATUS_DOWNLOADED`.

Add a small custom error type near the publish worker:

```rust
#[derive(Debug, thiserror::Error)]
#[error("cached EH ZIP is missing")]
struct MissingEhZip;
```

- [ ] **Step 4: Run worker tests**

Run:

```powershell
cargo test -p pixivbot --no-default-features eh_engine
```

Expected: PASS for scheduler EH tests.

- [ ] **Step 5: Commit Task 4**

Run:

```powershell
git add src/scheduler/eh_engine.rs src/db/repo/eh_download_queue.rs; git commit -m "fix: make EH workers recoverable" -m "Avoid retry-budget loss on deferred chats, resume publish via sent markers, retry missing ZIPs through download, and fallback Telegraph failures to archive delivery."
```

Expected: commit succeeds.

---

### Task 5: EH Client All-or-Error Direct Image Fallback and Telegraph Split Budget

**Files:**
- Modify: `eh_client/src/client.rs`
- Modify: `eh_client/src/parser.rs`
- Modify: `eh_client/tests/integration.rs`
- Modify: `eh_client/src/telegraph.rs`

- [ ] **Step 1: Write failing EH client tests**

Add tests in `eh_client/tests/integration.rs`:

```rust
#[tokio::test]
async fn test_download_gallery_images_fails_when_one_page_fetch_fails() {
    let server = MockServer::start().await;
    let client = client_at(&server);
    Mock::given(method("GET"))
        .and(path("/g/123/abc/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"<a href="/s/1/123-1">1</a><a href="/s/2/123-2">2</a>"#))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/s/1/123-1"))
        .respond_with(ResponseTemplate::new(200).set_body_string(format!(r#"<img id="img" src="{}/img/1.jpg">"#, server.uri())))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/s/2/123-2"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/img/1.jpg"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![1, 2, 3]))
        .mount(&server)
        .await;

    let dest = tempfile::tempdir().unwrap().path().join("gallery.zip");
    let err = client.download_gallery_images(123, "abc", &dest).await.unwrap_err();
    assert!(err.to_string().contains("failed to download all gallery images"));
}

#[tokio::test]
async fn test_download_gallery_images_fails_when_one_image_fetch_fails() {
    let server = MockServer::start().await;
    let client = client_at(&server);
    Mock::given(method("GET"))
        .and(path("/g/123/abc/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"<a href="/s/1/123-1">1</a><a href="/s/2/123-2">2</a>"#))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/s/1/123-1"))
        .respond_with(ResponseTemplate::new(200).set_body_string(format!(r#"<img id="img" src="{}/img/1.jpg">"#, server.uri())))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/s/2/123-2"))
        .respond_with(ResponseTemplate::new(200).set_body_string(format!(r#"<img id="img" src="{}/img/2.jpg">"#, server.uri())))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/img/1.jpg"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![1, 2, 3]))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/img/2.jpg"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;

    let temp_dir = tempfile::tempdir().unwrap();
    let dest = temp_dir.path().join("gallery.zip");
    let err = client.download_gallery_images(123, "abc", &dest).await.unwrap_err();
    assert!(err.to_string().contains("failed to download all gallery images"));
}
```

Add a Telegraph unit test in `eh_client/src/telegraph.rs`:

```rust
#[test]
fn test_split_for_pages_reserves_next_link_budget() {
    let url = format!("https://img.example/{}", "x".repeat(1900));
    let urls = vec![url; 40];
    let pages = split_for_pages(&urls);
    for (idx, page) in pages.iter().enumerate() {
        let mut nodes: Vec<Node> = page.iter().map(|u| Node::image(u)).collect();
        if idx + 1 < pages.len() {
            nodes.push(Node::paragraph(vec![Node::link("Next Page →", "https://telegra.ph/next")]));
        }
        assert!(estimate_content_size(&nodes) <= MAX_CONTENT_SIZE);
    }
}
```

- [ ] **Step 2: Run tests and verify failure**

Run:

```powershell
cargo test -p eh_client download_gallery_images_fails
cargo test -p eh_client split_for_pages_reserves_next_link_budget
```

Expected: FAIL because partial image fallback currently succeeds if at least one image downloads, and split budgeting ignores continuation links.

- [ ] **Step 3: Implement all-or-error fallback and split budget**

In `eh_client/src/parser.rs`, update `parse_image_page_urls()` to accept both absolute EH URLs and relative `/s/{hash}/{gid}-{page}` links:

```rust
fn image_page_urls_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"<a\s+href=\"((?:https?://(?:e-hentai|exhentai)\.org)?/s/[^\"]+)\""#)
            .expect("invalid image_page_urls regex")
    })
}
```

In `EhClient::get_gallery_image_urls()` or immediately after `parser::parse_image_page_urls(&html)`, normalize relative image page URLs using the client base URL:

```rust
let urls = parser::parse_image_page_urls(&html)
    .into_iter()
    .map(|url| {
        if url.starts_with('/') {
            format!("{}{}", self.base_url, url)
        } else {
            url
        }
    })
    .collect::<Vec<_>>();
```

Then in `EhClient::download_gallery_images()`:

```rust
let total_pages = image_page_urls.len();
let mut images_downloaded = 0usize;
for (idx, page_url) in image_page_urls.iter().enumerate() {
    let page_resp = self.http.get(page_url).send().await?;
    if !page_resp.status().is_success() {
        return Err(Error::Other(format!("failed to download all gallery images: page {}/{} returned {}", idx + 1, total_pages, page_resp.status())));
    }
    let page_html = page_resp.text().await?;
    let img_url = parser::parse_image_src(&page_html)
        .ok_or_else(|| Error::Parse(format!("failed to download all gallery images: no image src on page {}/{}", idx + 1, total_pages)))?;
    let image_resp = self.http.get(&img_url).send().await?;
    if !image_resp.status().is_success() {
        return Err(Error::Other(format!("failed to download all gallery images: image {}/{} returned {}", idx + 1, total_pages, image_resp.status())));
    }
    let bytes = image_resp.bytes().await?;
    zip.start_file(format!("{:04}.jpg", idx + 1), options)?;
    zip.write_all(&bytes)?;
    images_downloaded += 1;
}
if images_downloaded != total_pages {
    return Err(Error::Other(format!("failed to download all gallery images: downloaded {images_downloaded}/{total_pages}")));
}
```

In `eh_client/src/telegraph.rs`, reserve link budget in splitting:

```rust
const CONTINUATION_LINK_BUDGET: usize = 512;
let max_page_size = MAX_CONTENT_SIZE - CONTINUATION_LINK_BUDGET;
```

Use `max_page_size` for non-empty page chunk decisions so adding the “Next Page” paragraph remains below `MAX_CONTENT_SIZE`.

- [ ] **Step 4: Run EH client tests**

Run:

```powershell
cargo test -p eh_client
```

Expected: PASS.

- [ ] **Step 5: Commit Task 5**

Run:

```powershell
git add eh_client/src/client.rs eh_client/src/parser.rs eh_client/tests/integration.rs eh_client/src/telegraph.rs; git commit -m "fix: reject partial EH image downloads" -m "Require direct image fallback to fetch every discovered page and reserve Telegraph continuation link budget."
```

Expected: commit succeeds.

---

### Task 6: Bot, Command, Markdown, and Telegraph Availability Fixes

**Files:**
- Modify: `src/bot/mod.rs`
- Modify: `src/bot/handler.rs`
- Modify: `src/bot/commands.rs`
- Modify: `src/bot/handlers/subscription/ehentai.rs`
- Modify: `src/bot/handlers/subscription/helpers.rs`
- Modify: `src/bot/handlers/subscription/list.rs`
- Modify: `src/bot/handlers/download.rs`
- Modify: `src/db/types/eh_filter.rs`
- Modify: `src/main.rs`

- [ ] **Step 1: Write failing bot/config tests**

Add command visibility test in `src/bot/commands.rs`:

```rust
#[test]
fn edl_help_is_url_only() {
    let commands = Command::user_commands(true, false);
    let edl = commands.into_iter().find(|cmd| cmd.command == "edl").unwrap();
    assert!(edl.description.contains("<url>"));
    assert!(!edl.description.contains("url|gid"));
}
```

Add EH filter test in `src/db/types/eh_filter.rs`:

```rust
#[test]
fn test_telegraph_only_filter_is_not_empty() {
    let filter = EhFilter { telegraph: true, ..Default::default() };
    assert!(!filter.is_empty());
}
```

Add parsing/Markdown tests in `src/bot/handlers/subscription/ehentai.rs`:

```rust
#[test]
fn test_esub_success_label_is_markdown_safe() {
    let label = markdown::escape("E-Hentai");
    assert_eq!(label, "E\\-Hentai");
}
```

Add download extraction tests in `src/bot/handlers/download.rs`:

```rust
#[test]
fn test_extract_eh_galleries_finds_multiple_links() {
    let text = "https://e-hentai.org/g/1/aaa/ https://e-hentai.org/g/2/bbb/";
    let galleries = extract_eh_galleries_from_text(text);
    assert_eq!(galleries.len(), 2);
    assert_eq!(galleries[0], (1, "aaa".to_string()));
    assert_eq!(galleries[1], (2, "bbb".to_string()));
}
```

- [ ] **Step 2: Run tests and verify failure**

Run:

```powershell
cargo test -p pixivbot --no-default-features edl_help_is_url_only
cargo test -p pixivbot --no-default-features telegraph_only_filter_is_not_empty
cargo test -p pixivbot --no-default-features extract_eh_galleries_finds_multiple_links
```

Expected: FAIL because help still says `<url|gid>`, `is_empty()` ignores `telegraph`, and multi-extraction helper does not exist.

- [ ] **Step 3: Implement bot/config behavior**

Thread Telegraph availability into bot wiring:

```rust
// src/bot/mod.rs
#[allow(clippy::too_many_arguments)]
pub async fn run(
    bot: ThrottledBot,
    config: TelegramConfig,
    repo: Arc<Repo>,
    pixiv_client: Arc<tokio::sync::RwLock<PixivClient>>,
    notifier: notifier::Notifier,
    sensitive_tags: Vec<String>,
    image_size: pixiv_client::ImageSize,
    download_original_threshold: u8,
    cache_dir: String,
    log_dir: String,
    booru_registry: Arc<BooruSiteRegistry>,
    eh_client: Option<Arc<eh_client::EhClient>>,
    has_telegraph: bool,
) -> Result<()> {
    let handler = BotHandler::new(
        repo.clone(),
        pixiv_client.clone(),
        notifier.clone(),
        sensitive_tags,
        config.owner_id,
        is_public_mode,
        image_size,
        download_original_threshold,
        config.require_mention_in_group,
        cache_dir,
        log_dir,
        booru_registry,
        eh_client,
        has_telegraph,
    );
}

// src/bot/handler.rs
pub struct BotHandler {
    eh_client: Option<Arc<eh_client::EhClient>>,
    has_telegraph: bool,
}

#[allow(clippy::too_many_arguments)]
pub fn new(
    repo: Arc<Repo>,
    pixiv_client: Arc<tokio::sync::RwLock<PixivClient>>,
    notifier: Notifier,
    default_sensitive_tags: Vec<String>,
    owner_id: Option<i64>,
    is_public_mode: bool,
    image_size: pixiv_client::ImageSize,
    download_original_threshold: u8,
    require_mention_in_group: bool,
    cache_dir: String,
    log_dir: String,
    booru_registry: Arc<BooruSiteRegistry>,
    eh_client: Option<Arc<eh_client::EhClient>>,
    has_telegraph: bool,
) -> Self {
    Self { repo, pixiv_client, notifier, default_sensitive_tags, owner_id, is_public_mode, image_size, download_original_threshold, require_mention_in_group, cache_dir, log_dir, booru_registry, eh_client, has_telegraph }
}
```

Pass `telegraph_client.is_some()` from `src/main.rs` to `bot::run`. In `src/main.rs`, if `config.ehentai.upload_telegraph && config.ehentai.telegraph_access_token.is_none()`, log a warning and treat global Telegraph upload as disabled for scheduler enqueue decisions.

Update EH handlers so signatures include `has_telegraph: bool`. Reject Telegraph requests with friendly messages:

```rust
if telegraph_on && !has_telegraph {
    bot.send_message(msg.chat.id, "Telegraph 未配置，无法启用 telegraph=on。请配置 ehentai.telegraph_access_token 后重试。")
        .await?;
    return Ok(());
}
```

Apply this to the `/esub` branch after `telegraph_on` is parsed, to the `/edl` branch after its options are parsed, and at the start of `handle_telegraph()` before this Telegraph enqueue shape:

```rust
self.repo
    .enqueue_eh_download(chat_id.0, gid as i64, &token, &title, true, SOURCE_DIRECT)
    .await
    .context("加入 Telegraph 下载队列失败")?;
```

Update `EhFilter::is_empty()`:

```rust
pub fn is_empty(&self) -> bool {
    self.min_rating.is_none()
        && self.min_pages.is_none()
        && self.max_pages.is_none()
        && !self.telegraph
}
```

Fix MarkdownV2 labels and list escaping:

```rust
let site_label = markdown::escape("E-Hentai");
let task_text = markdown::escape(&task.value);
```

Update `/download` EH behavior:

```rust
fn extract_eh_galleries_from_text(text: &str) -> Vec<(i64, String)> {
    let re = Regex::new(r"https?://(?:e-|ex)hentai\.org/g/(\d+)/([A-Za-z0-9_-]+)/?")
        .expect("valid EH gallery regex");
    re.captures_iter(text)
        .filter_map(|cap| {
            let gid = cap.get(1)?.as_str().parse::<i64>().ok()?;
            let token = cap.get(2)?.as_str().to_string();
            Some((gid, token))
        })
        .collect()
}

let eh_galleries = extract_eh_galleries(&msg, &args);
if eh_galleries.len() > 1 {
    bot.send_message(msg.chat.id, "一次只能处理一个 E-Hentai 链接，请使用 /edl <url>。").await?;
    return Ok(());
}
if eh_galleries.len() == 1 && !targets.is_empty() {
    bot.send_message(msg.chat.id, "请不要把 E-Hentai 链接和 Pixiv/Booru 链接混在同一次 /download 中；E-Hentai 请使用 /edl <url>。").await?;
    return Ok(());
}
```

When enqueueing EH from `/download`, propagate errors:

```rust
self.repo
    .enqueue_eh_download(chat_id.0, *gid as i64, token, &title, false, SOURCE_DIRECT)
    .await
    .context("加入 E-Hentai 下载队列失败")?;
```

Update `Command::EDl` help text from `<url|gid>` to `<url>`.

- [ ] **Step 4: Run bot tests**

Run:

```powershell
cargo test -p pixivbot --no-default-features commands
cargo test -p pixivbot --no-default-features ehentai
cargo test -p pixivbot --no-default-features download
cargo test -p pixivbot --no-default-features eh_filter
```

Expected: PASS.

- [ ] **Step 5: Commit Task 6**

Run:

```powershell
git add src/bot/mod.rs src/bot/handler.rs src/bot/commands.rs src/bot/handlers/subscription/ehentai.rs src/bot/handlers/subscription/helpers.rs src/bot/handlers/subscription/list.rs src/bot/handlers/download.rs src/db/types/eh_filter.rs src/main.rs; git commit -m "fix: gate EH Telegraph commands" -m "Reject unconfigured Telegraph requests, preserve telegraph-only subscription filters, escape EH MarkdownV2 output, and make /download EH handling explicit."
```

Expected: commit succeeds.

---

### Task 7: Focused Regression Run, Formatting, Full CI, and Final Review

**Files:**
- No planned source edits. Formatting or verification may touch files already changed in Tasks 1-6; if that happens, commit only those verification-induced diffs.

- [ ] **Step 1: Run formatting**

Run:

```powershell
make fmt
```

Expected: exit code 0.

- [ ] **Step 2: Run focused EH tests**

Run:

```powershell
cargo test -p pixivbot --no-default-features eh_
cargo test -p eh_client
```

Expected: both commands PASS.

- [ ] **Step 3: Run LSP diagnostics on changed Rust files**

Run LSP diagnostics for changed Rust files, at minimum:

```text
src/db/types/state.rs
src/config.rs
src/db/entities/eh_download_queue.rs
src/db/repo/eh_download_queue.rs
src/scheduler/eh_engine.rs
eh_client/src/client.rs
eh_client/src/telegraph.rs
src/bot/handler.rs
src/bot/mod.rs
src/bot/handlers/subscription/ehentai.rs
src/bot/handlers/download.rs
```

Expected: no new errors.

- [ ] **Step 4: Run full repository CI**

Run:

```powershell
make ci
```

Expected: exit code 0.

- [ ] **Step 5: Commit verification fixes if any**

If formatting or CI changed files, run:

```powershell
git add .; git commit -m "chore: verify EH review fixes" -m "Apply formatting or test-driven follow-up fixes from the EH review-fix verification run."
```

Expected: commit succeeds only if there were actual changes.

- [ ] **Step 6: Request final code review**

Use the requesting-code-review skill with:

Run this first to get the actual HEAD SHA:

```powershell
git rev-parse HEAD
```

```text
DESCRIPTION: Implemented EH review fixes: pending collect backlog, publish sent markers, queue merge semantics, missing ZIP retry, Telegraph gating/fallback, Markdown command fixes, and direct image all-or-error.
PLAN_OR_REQUIREMENTS: docs/superpowers/specs/2026-06-29-eh-review-fixes-design.md and this implementation plan.
BASE_SHA: 9124b49
HEAD_SHA: paste the exact SHA printed by `git rev-parse HEAD`
```

Expected: reviewer returns no Critical or Important issues. Fix any Critical/Important feedback using receiving-code-review before reporting completion.

---

## Plan Self-Review

- Spec coverage: every accepted spec requirement maps to at least one task: config/schema/state in Task 1; queue merge/quota/markers/defer in Task 2; collect backlog in Task 3; worker idempotency/missing ZIP/fallback/defer in Task 4; client all-or-error and Telegraph split in Task 5; bot Markdown/commands/token gating/filter persistence in Task 6; verification and review in Task 7.
- Placeholder scan: this plan contains no deferred requirements or unspecified task slots.
- Type consistency: new names are consistent across tasks: `EhPendingGallery`, `pending_galleries`, `pending_high_water_ts`, `archive_sent_at`, `telegraph_sent_at`, `mark_eh_archive_sent`, `mark_eh_telegraph_sent`, and `defer_eh_download`.
