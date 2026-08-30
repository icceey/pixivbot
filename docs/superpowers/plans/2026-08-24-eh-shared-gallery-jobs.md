# EH Shared Gallery Jobs Implementation Plan

> **For agentic workers:** Use the subagent-driven-development skill to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Normalize EH gallery work into one durable job per `(gid, token, download_mode, resolution)` while retaining independent, bounded-concurrent Telegram delivery and cancellation for every chat.

**Architecture:** Add `eh_gallery_jobs` as the sole owner of download, background-download, IPFS/Telegraph, rewrite, retry, and artifact state; keep `eh_download_queue` as a per-chat delivery table joined through nullable `job_id`, and preserve generation history in append-only GP/download-completion ledgers. Enqueue creates/reuses the canonical job and merges/rebinds the delivery in one SQLite transaction, shared workers claim jobs, and the publish worker claims deliveries with bounded concurrency under keyed chat locks. Cleanup is a persisted, generation-guarded maintenance lifecycle that blocks dirty job reuse and never deletes multipart state until provider abort succeeds.

**Tech Stack:** Rust 1.94, Tokio, SeaORM/SeaQuery 1.1.20, SQLite, teloxide 0.17 `Throttle<Bot>`, reqwest/wiremock 0.6, `eh_client::ArchiveArtifacts`, cargo, rustfmt, Clippy, PowerShell.

**Spec:** `docs/superpowers/specs/2026-08-24-eh-shared-gallery-jobs-design.md`

**Global Constraints:**
- Rust remains pinned to 1.94 by `rust-toolchain.toml`; do not raise the MSRV.
- Add no dependency unless an existing crate truly cannot implement the behavior; the design below uses only current dependencies.
- Exactly the same `(gid, token, download_mode, resolution)` shares one active job; a different mode or resolution is a different job and artifact family.
- Canonical variants are `archive:<download_resolution>` for logged-in direct requests, `archive:<subscription_resolution>` for logged-in subscription requests, and `images` with an empty resolution when not logged in.
- Migrated active work uses only `legacy:direct` or `legacy:subscription`; new enqueue paths never create a legacy variant.
- Archive download, image/IPFS upload, Telegraph creation, and delayed rewrite run once per shared job generation.
- Subscription ownership, cancellation, disabled-chat deferral, Telegram retry, and archive/Telegraph sent markers remain independent per delivery/chat.
- `ehentai.publish_concurrency` defaults to `2` and is clamped to `1..=10`; this does not add concurrency to downloads or uploads of unrelated galleries.
- Telegram calls continue through `Notifier` and its existing `ThrottledBot = Throttle<Bot>`; add no manual Telegram rate-limit sleeps.
- Multipart cleanup is fail-closed: do not remove local upload manifests, parts, or ZIPs until provider abort succeeds or the upload is known complete.
- Log internal failures with `{:#}`, but user-facing failure text must remain short and must not contain raw `anyhow`, database, EH, Telegram, IPFS/S3, or Telegraph error chains.
- Positive `eh_gp_spend_attempts` created by new archive POST attempts reference `job_id`, not a delivery; existing `queue_id` remains nullable historical provenance.
- Every successful `mark_eh_job_downloaded` generation appends one `eh_download_completions` row in the same transaction; rolling bytes sum this append-only ledger and never infer history from a reusable `eh_gallery_jobs` row.
- Rolling GP and byte windows aggregate actual shared attempts/completions exactly once, never one copy per delivery; complete→retire→reactivate never erases an earlier generation.
- A retired job with cleanup `pending`, `running`, or `failed` may accept a new delivery binding but cannot reset or be downloaded until fail-closed maintenance completes; cleanup failure retains every ZIP, part, and manifest.
- Pending/rewriting Telegraph work prevents final job retirement and payload clearing; success or terminal rewrite failure reevaluates liveness.
- Terminal download failure remains log plus `/estatus` only. Terminal Telegraph upload failure sends exactly one fixed friendly notification attempt to each newly failed Telegraph delivery, never to archive-only deliveries and never with the internal error chain.
- Every repository test helper that manually creates SQLite tables must include `eh_gallery_jobs`, `eh_download_completions`, both queue/GP `job_id` columns, indexes, foreign keys, and current defaults.
- Keep `src/scheduler/eh_engine.rs` in its current structure and change only the EH worker sections required by this design; do not perform an unrelated split or refactor.
- Preserve direct-request priority, direct-over-subscription merge behavior, subscription progress, background downloads, Telegraph rewrite timing, queue status copy, and retry markers unless the spec explicitly changes state ownership.
- Do not implement cross-gallery download/upload concurrency, a permanent ZIP cache, multi-process send-versus-unsubscribe ordering, or highest-resolution sharing.
- Do not read ignored `config.toml`, `/data`, `/logs`, or secrets; use `config.toml.example` as the public configuration surface.
- Do not add commit steps or execute Git write commands; task boundaries are test/review boundaries only.
- Finish with focused tests, `make quick`, and the repository-required `make ci`; every modified Rust file must have zero new language-server diagnostics.

---

## Authoritative inputs, state ownership, and execution waves

- The approved spec above is the source of truth. Current implementation facts are in `src/db/repo/eh_download_queue.rs`, `src/scheduler/eh_engine.rs`, and migrations through `m20260719_000000_eh_gp_spend_attempts`.
- Delivery statuses after migration are exactly `waiting`, `publishing`, `done`, `failed`, and `canceled`.
- Job download statuses are exactly `pending`, `downloading`, `downloaded`, `failed`, and `retired`.
- Job Telegraph statuses are exactly `not_required`, `pending`, `uploading`, `ready`, and `failed`.
- Job cleanup statuses are exactly `none`, `pending`, `running`, and `failed`.
- Shared mutable columns already on `eh_download_queue` remain compatibility columns with their current nullability/defaults, but code handling a non-null `job_id` must not read or mirror them as authoritative state.
- **Wave 0:** Task 1 only. It establishes schema, entities, relations, and real-SQLite helpers.
- **Wave 1:** Task 2 only. It establishes canonical variants and the transactional enqueue/rebind contract used by every worker test.
- **Wave 2:** Tasks 3 and 7 are logically parallel after Task 2: Task 3 owns shared job download/GP methods and the download section of `eh_engine.rs`; Task 7 owns delivery cancellation/liveness and keyed locks. If workers share one checkout, serialize their edits to repository imports/tests.
- **Wave 3:** Tasks 4 and 5 are logically parallel after Task 3: Task 4 owns background job methods and the background worker section; Task 5 owns Telegraph upload job methods and the upload worker section. They touch disjoint worker sections but both modify `src/db/repo/eh_gallery_jobs.rs`, so serialize them in a shared checkout rather than creating Git branches/worktrees.
- **Wave 4:** Task 6 follows Tasks 5 and 7 because rewrite completion must invoke liveness.
- **Wave 5:** Task 8 follows Tasks 5–7 because delivery markers call Task 6's rewrite scheduler and publish uses Task 7's keyed locks.
- **Wave 6:** Task 9 follows Tasks 3–8. Task 10 follows Task 9 and is the final user-visible/regression slice.
- A worker implementing in a single checkout should execute Task 1 → 2 → 3 → 7 → 4 → 5 → 6 → 8 → 9 → 10 to avoid same-file write conflicts.
- At every task boundary, run the focused command and inspect `git diff --check` read-only; there are no commit checkpoints.

## File map

| File | Responsibility in this change |
|---|---|
| `migration/src/m20260824_000000_eh_shared_gallery_jobs.rs` | Create/backfill shared jobs and download-completion history, bind active legacy deliveries, add GP job provenance, indexes, rollback behavior, and SQLite-transaction safety. |
| `migration/src/lib.rs` | Register the new migration last. |
| `src/db/entities/eh_gallery_jobs.rs` | SeaORM model for all shared lifecycle, artifact, background, and rewrite state. |
| `src/db/entities/eh_download_completions.rs` | Append-only model for one successful shared download generation, including nullable retained-job provenance. |
| `src/db/entities/eh_download_queue.rs` | Add nullable delivery `job_id`, relations, and delivery-owned state documentation. |
| `src/db/entities/eh_gp_spend_attempts.rs` | Add nullable `job_id` relation while retaining historical `queue_id`. |
| `src/db/entities/mod.rs` | Export `eh_gallery_jobs` and `eh_download_completions`. |
| `src/db/repo.rs` | Register the focused job/completion repository modules and make the shared SQLite test schema match production. |
| `src/db/repo/eh_gallery_jobs.rs` | New focused repository module for canonical variants, job claims/transitions, shared retries, background work, Telegraph/rewrite state, liveness, and cleanup decisions. |
| `src/db/repo/eh_download_queue.rs` | Transactional job+delivery enqueue, delivery claims/markers/retries, keyed chat locks, cancellation ownership, joined status snapshots, and compatibility migrations of existing tests. |
| `src/db/repo/eh_gp_spend_attempts.rs` | Record new GP attempts against jobs and keep rolling ledger behavior/migration coverage. |
| `src/db/repo/eh_download_completions.rs` | Append completion rows only inside the successful download transaction and sum rolling byte windows. |
| `src/db/repo/eh_integration_tests.rs` | Migration, shared lifecycle, status-join, and adjacent repository scenarios on real SQLite. |
| `src/scheduler/eh_engine.rs` | Minimally retarget existing workers: shared job download/background/upload/rewrite and bounded per-delivery publish. |
| `src/config.rs` | Add/clamp `publish_concurrency`, preserve archive/background concurrency semantics, and test defaults/bounds. |
| `config.toml.example` | Document delivery-only concurrency and its `1..=10` clamp. |
| `src/bot/handler.rs` | Hold the EH config needed to compute canonical direct-request variants. |
| `src/bot/mod.rs` | Thread `Arc<EhentaiConfig>` into `BotHandler` without changing `ThrottledBot`. |
| `src/bot/handlers/subscription/ehentai.rs` | Pass canonical direct variants at `/edl` and `/telegraph`; derive `/estatus` labels from joined delivery/job state without leaking internals. |
| `src/main.rs` | Thread EH config, invoke new startup recovery/cleanup APIs, and construct workers with unchanged throttled notifier wiring. |

## Planned interfaces

Define these names once and use them consistently in all tasks:

```rust
// src/db/repo/eh_gallery_jobs.rs
pub const DOWNLOAD_MODE_ARCHIVE: &str = "archive";
pub const DOWNLOAD_MODE_IMAGES: &str = "images";
pub const DOWNLOAD_MODE_LEGACY: &str = "legacy";
pub const JOB_STATUS_PENDING: &str = "pending";
pub const JOB_STATUS_DOWNLOADING: &str = "downloading";
pub const JOB_STATUS_DOWNLOADED: &str = "downloaded";
pub const JOB_STATUS_FAILED: &str = "failed";
pub const JOB_STATUS_RETIRED: &str = "retired";
pub const TELEGRAPH_STATUS_NOT_REQUIRED: &str = "not_required";
pub const TELEGRAPH_STATUS_PENDING: &str = "pending";
pub const TELEGRAPH_STATUS_UPLOADING: &str = "uploading";
pub const TELEGRAPH_STATUS_READY: &str = "ready";
pub const TELEGRAPH_STATUS_FAILED: &str = "failed";
pub const BACKGROUND_STATUS_PENDING: &str = "pending";
pub const BACKGROUND_STATUS_RUNNING: &str = "running";
pub const TELEGRAPH_REWRITE_STATUS_PENDING: &str = "pending";
pub const TELEGRAPH_REWRITE_STATUS_REWRITING: &str = "rewriting";
pub const TELEGRAPH_REWRITE_STATUS_FAILED: &str = "failed";
pub const CLEANUP_STATUS_NONE: &str = "none";
pub const CLEANUP_STATUS_PENDING: &str = "pending";
pub const CLEANUP_STATUS_RUNNING: &str = "running";
pub const CLEANUP_STATUS_FAILED: &str = "failed";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EhGalleryVariant {
    pub download_mode: String,
    pub resolution: String,
}

impl EhGalleryVariant {
    pub fn for_request(is_logged_in: bool, source: &str, config: &EhentaiConfig) -> Self;
    pub fn archive(resolution: impl Into<String>) -> Self;
    pub fn images() -> Self;
}

#[derive(Clone, Debug)]
pub struct EhDeliveryClaim {
    pub delivery: eh_download_queue::Model,
    pub job: eh_gallery_jobs::Model,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EhJobCleanupDecision {
    pub job_id: i32,
    pub zip_path: Option<String>,
    pub retire: bool,
    pub remove_archive_family: bool,
    pub preserve_rewrite_payload: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EhFailedTelegraphDelivery {
    pub delivery_id: i32,
    pub chat_id: i64,
    pub title: String,
}

#[derive(Clone, Debug)]
pub enum EhJobUploadFailureOutcome {
    RetryScheduled(eh_gallery_jobs::Model),
    Terminal {
        job: eh_gallery_jobs::Model,
        deliveries: Vec<EhFailedTelegraphDelivery>,
    },
    Stale,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EhCleanupFinalizeOutcome {
    ReactivatedPending,
    CleanRetired,
    RetainedForRewrite,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EhStaleResetCounts {
    pub downloads: u64,
    pub uploads: u64,
    pub background_downloads: u64,
    pub rewrites: u64,
    pub cleanups: u64,
    pub deliveries: u64,
}

// src/db/repo/eh_download_queue.rs
pub const DELIVERY_STATUS_WAITING: &str = "waiting";
pub const DELIVERY_STATUS_PUBLISHING: &str = "publishing";
pub const DELIVERY_STATUS_DONE: &str = "done";
pub const DELIVERY_STATUS_FAILED: &str = "failed";
pub const DELIVERY_STATUS_CANCELED: &str = "canceled";

pub struct EhChatLockRegistry { /* Mutex<HashMap<i64, Weak<Mutex<()>>>> */ }

impl EhChatLockRegistry {
    pub async fn lock_chat(&self, chat_id: i64) -> tokio::sync::OwnedMutexGuard<()>;
    pub async fn lock_chats(&self, chat_ids: &[i64]) -> Vec<tokio::sync::OwnedMutexGuard<()>>;
}

pub static EH_CHAT_LOCKS: LazyLock<EhChatLockRegistry>;
```

The registry sorts and deduplicates multi-chat input, upgrades/removes `Weak` entries while holding only the registry mutex, releases that mutex before awaiting a per-chat lock, and acquires owned guards in ascending chat-ID order.

### Task 1: Migrate active legacy work into durable shared jobs

**Files:**
- Create: `migration/src/m20260824_000000_eh_shared_gallery_jobs.rs`
- Create: `src/db/entities/eh_gallery_jobs.rs`
- Create: `src/db/entities/eh_download_completions.rs`
- Modify: `migration/src/lib.rs:3-38`
- Modify: `src/db/entities/eh_download_queue.rs:9-94`
- Modify: `src/db/entities/eh_gp_spend_attempts.rs:4-33`
- Modify: `src/db/entities/mod.rs:1-8`
- Modify: `src/db/repo.rs:4-11,33-194`
- Test: `src/db/repo/eh_integration_tests.rs`

**Interfaces:**
- Consumes: current `eh_download_queue` schema and unique `(chat_id, gid)` index; current `eh_gp_spend_attempts.queue_id`; historical queue `file_size/completed_at`; `Migrator::migrations()`; `Repo::tests_helpers::setup_test_db()`.
- Produces: registered migration `m20260824_000000_eh_shared_gallery_jobs`; entities `eh_gallery_jobs::Model`, `eh_download_completions::Model`, `eh_download_queue::Model::job_id`, `eh_gp_spend_attempts::Model::job_id`; matching real-SQLite test schema.

- [ ] **Step 1: Write failing migration and schema-parity tests**

Add a migration harness beside the existing EH integration tests. Seed two active rows sharing `gid/token/source=subscription`, one active direct row for the same gallery, one active row for another gallery, and terminal `done`/`failed`/`canceled` rows. Give the active rows non-null obsolete claim fields and distinct sent markers. Give one bound active row and two terminal rows positive `file_size` plus distinct non-null `completed_at` values; leave another positive-size row without a completion timestamp to prove it is not backfilled.

```rust
#[tokio::test]
async fn migration_groups_active_legacy_variants_and_leaves_terminal_history_unbound() {
    let db = new_eh_legacy_migration_db().await;
    seed_legacy_shared_job_rows(&db).await;
    run_migration(&db, "m20260824_000000_eh_shared_gallery_jobs").await;

    let jobs = query_rows(&db,
        "SELECT gid, token, download_mode, resolution, status, telegraph_required \
         FROM eh_gallery_jobs ORDER BY gid, resolution").await;
    assert_eq!(jobs.len(), 3);
    assert_row(&jobs[0], (100_i64, "tok", "legacy", "direct", "pending"));
    assert_row(&jobs[1], (100_i64, "tok", "legacy", "subscription", "pending"));
    assert_row(&jobs[2], (200_i64, "tok2", "legacy", "subscription", "pending"));

    let active = query_rows(&db,
        "SELECT status, job_id, archive_sent_at, telegraph_sent_at, started_at, next_retry_at \
         FROM eh_download_queue WHERE gid IN (100, 200) ORDER BY id").await;
    assert!(active.iter().all(|row| row.string("status") == "waiting"));
    assert!(active.iter().all(|row| row.optional_i32("job_id").is_some()));
    assert!(active.iter().all(|row| row.optional_string("started_at").is_none()));
    assert!(active.iter().all(|row| row.optional_string("next_retry_at").is_none()));
    assert!(active.iter().any(|row| row.optional_string("archive_sent_at").is_some()));
    assert!(active.iter().any(|row| row.optional_string("telegraph_sent_at").is_some()));

    let terminal_job_ids = query_optional_i32s(&db,
        "SELECT job_id FROM eh_download_queue WHERE status IN ('done','failed','canceled')").await;
    assert_eq!(terminal_job_ids, vec![None, None, None]);
}

#[tokio::test]
async fn migration_backfills_append_only_download_completions_before_clearing_compatibility() {
    let db = new_eh_legacy_migration_db().await;
    seed_legacy_shared_job_rows(&db).await;
    run_migration(&db, "m20260824_000000_eh_shared_gallery_jobs").await;

    let completions = query_rows(&db,
        "SELECT job_id, gid, file_size, created_at FROM eh_download_completions \
         ORDER BY created_at, id").await;
    assert_eq!(completions.len(), 3);
    assert_eq!(completions.iter().map(|r| r.i64("file_size")).collect::<Vec<_>>(),
        vec![101, 202, 303]);
    assert_eq!(completions.iter().filter(|r| r.optional_i32("job_id").is_some()).count(), 1);
    assert_eq!(completions.iter().filter(|r| r.optional_i32("job_id").is_none()).count(), 2,
        "terminal historical completions remain valid without a manufactured job");
    assert!(completions.iter().all(|r| r.optional_string("created_at").is_some()));
}

#[tokio::test]
async fn shared_job_test_schema_enforces_variant_and_foreign_keys() {
    let repo = tests_helpers::setup_test_db().await.unwrap();
    repo.db().execute_unprepared("PRAGMA foreign_keys = ON").await.unwrap();
    let job_id = insert_schema_contract_job(repo.db(), 42, "tok", "archive", "original")
        .await.unwrap();
    let job = load_job(&repo, job_id).await;
    assert_eq!(job.cleanup_status, "none");
    assert!(job.cleanup_started_at.is_none());
    assert!(job.cleanup_error.is_none());
    assert!(job.cleanup_next_retry_at.is_none());
    assert!(insert_schema_contract_job(repo.db(), 42, "tok", "archive", "original")
        .await.is_err(), "variant unique constraint must reject the duplicate");

    let delivery_id = insert_delivery_bound_to_job(repo.db(), job_id).await.unwrap();
    let gp_id = insert_gp_attempt_bound_to_job(repo.db(), job_id, 42, 10).await.unwrap();
    let completion_id = insert_download_completion_bound_to_job(repo.db(), job_id, 42, 100)
        .await.unwrap();
    delete_job(repo.db(), job_id).await.unwrap();
    assert_eq!(delivery_job_id(repo.db(), delivery_id).await, None);
    assert_eq!(gp_attempt_job_id(repo.db(), gp_id).await, None);
    assert_eq!(completion_job_id(repo.db(), completion_id).await, None);
    assert_eq!(count_download_completions(repo.db()).await, 1,
        "job deletion must preserve append-only completion history");
}

#[tokio::test]
async fn migration_rolls_back_ddl_when_legacy_backfill_fails() {
    let db = new_eh_legacy_migration_db_without_source_column().await;
    let error = run_migration_result(
        &db, "m20260824_000000_eh_shared_gallery_jobs").await.unwrap_err();
    assert!(error.to_string().contains("source"));
    assert!(!sqlite_table_exists(&db, "eh_gallery_jobs").await);
    assert!(!sqlite_table_exists(&db, "eh_download_completions").await);
    assert!(!sqlite_column_exists(&db, "eh_download_queue", "job_id").await);
    assert!(!sqlite_column_exists(&db, "eh_gp_spend_attempts", "job_id").await);
}
```

- [ ] **Step 2: Run the migration tests and confirm RED**

Run:

```powershell
cargo test -p pixivbot --bin pixivbot migration_groups_active_legacy_variants_and_leaves_terminal_history_unbound -- --nocapture
cargo test -p pixivbot --bin pixivbot migration_backfills_append_only_download_completions_before_clearing_compatibility -- --nocapture
```

Expected: compilation or lookup FAILS because the migration is not registered and `eh_gallery_jobs`/`eh_download_completions`/`job_id` do not exist.

- [ ] **Step 3: Implement the SQLite-transactional migration and exact schema**

Follow the transaction pattern in `m20260719_000000_eh_gp_spend_attempts.rs`. The production table must contain this exact ownership set (all timestamps use `.timestamp()` and all retry counters default to zero):

```sql
CREATE TABLE eh_gallery_jobs (
  id INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL,
  gid INTEGER NOT NULL,
  token TEXT NOT NULL,
  download_mode TEXT NOT NULL,
  resolution TEXT NOT NULL DEFAULT '',
  title TEXT NOT NULL,
  status TEXT NOT NULL DEFAULT 'pending',
  telegraph_status TEXT NOT NULL DEFAULT 'not_required',
  telegraph_required BOOLEAN NOT NULL DEFAULT FALSE,
  file_size INTEGER NOT NULL DEFAULT 0,
  gp_cost INTEGER NOT NULL DEFAULT 0,
  zip_path TEXT,
  telegraph_url TEXT,
  error TEXT,
  retry_count INTEGER NOT NULL DEFAULT 0,
  next_retry_at TIMESTAMP,
  cleanup_status TEXT NOT NULL DEFAULT 'none',
  cleanup_started_at TIMESTAMP,
  cleanup_error TEXT,
  cleanup_next_retry_at TIMESTAMP,
  created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  started_at TIMESTAMP,
  completed_at TIMESTAMP,
  background_download_status TEXT,
  background_download_started_at TIMESTAMP,
  background_download_next_retry_at TIMESTAMP,
  background_download_attempt_count INTEGER NOT NULL DEFAULT 0,
  background_download_error TEXT,
  telegraph_rewrite_data TEXT,
  telegraph_rewrite_status TEXT,
  telegraph_rewrite_after TIMESTAMP,
  telegraph_rewrite_started_at TIMESTAMP,
  telegraph_rewrite_next_retry_at TIMESTAMP,
  telegraph_rewrite_retry_count INTEGER NOT NULL DEFAULT 0,
  telegraph_rewrite_error TEXT,
  telegraph_rewritten_at TIMESTAMP,
  UNIQUE(gid, token, download_mode, resolution)
);

CREATE TABLE eh_download_completions (
  id INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL,
  job_id INTEGER,
  gid INTEGER NOT NULL,
  file_size INTEGER NOT NULL,
  created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  FOREIGN KEY(job_id) REFERENCES eh_gallery_jobs(id) ON DELETE SET NULL
);
```

Create these named indexes: `idx_eh_gallery_jobs_status_retry(status, next_retry_at)`, `idx_eh_gallery_jobs_telegraph_retry(telegraph_status, next_retry_at)`, `idx_eh_gallery_jobs_cleanup_retry(cleanup_status, cleanup_next_retry_at)`, `idx_eh_gallery_jobs_background_status(background_download_status)`, `idx_eh_gallery_jobs_rewrite_status(telegraph_rewrite_status)`, `idx_eh_gallery_jobs_completed_at(completed_at)`, `idx_eh_download_completions_created_at(created_at)`, and `idx_eh_download_completions_job_id(job_id)`. Add nullable `eh_download_queue.job_id REFERENCES eh_gallery_jobs(id) ON DELETE SET NULL` with `idx_eh_download_queue_job_id`, and nullable `eh_gp_spend_attempts.job_id REFERENCES eh_gallery_jobs(id) ON DELETE SET NULL` with `idx_eh_gp_spend_attempts_job_id`. Existing `queue_id` and its foreign key remain unchanged. Do not add update/delete repository APIs for completion rows; job deletion uses `ON DELETE SET NULL` and preserves ledger history.

Backfill in this order inside the same transaction:

1. Insert one job per active group using current active statuses `pending/downloading/downloaded/uploading/uploaded/publishing`, `download_mode='legacy'`, and `resolution = CASE WHEN source='direct' THEN 'direct' ELSE 'subscription' END`.
2. Set `telegraph_required` only when at least one grouped delivery has `telegraph=TRUE AND telegraph_sent_at IS NULL`; initialize its `telegraph_status` to `pending`, otherwise `not_required`.
3. Pick `title` from the lowest delivery ID in each group and `created_at=MIN(created_at)` so migration output is deterministic; do not copy a delivery's shared path/result/error into the job.
4. Bind every active delivery through a correlated match on `gid`, `token`, and source variant.
5. Before clearing any compatibility value, append exactly one completion for every historical queue row matching `file_size > 0 AND completed_at IS NOT NULL` using `INSERT INTO eh_download_completions(job_id, gid, file_size, created_at) SELECT job_id, gid, file_size, completed_at ...`. Active rows use the job bound in step 4; unbound terminal rows intentionally produce `job_id=NULL`.
6. Set active deliveries to `waiting`; preserve `subscription_ids`, `telegraph_subscription_ids`, `archive_sent_at`, and `telegraph_sent_at`; clear delivery `started_at`, `completed_at`, `error`, `retry_count`, `next_retry_at`, background claim fields, and rewrite claim fields. Set compatibility `file_size/gp_cost` to zero and compatibility `zip_path/telegraph_url` to null because legacy partial/shared ownership is not trusted.
7. Do not bind or mutate terminal history beyond the completion-ledger read in step 5.

The down migration drops completion-ledger indexes/table and other new indexes first, drops nullable columns using the repository's existing SQLite `drop_column` convention when supported, and then drops `eh_gallery_jobs`; it does not reconstruct job progress or accounting history in delivery rows.

- [ ] **Step 4: Add entities, relations, and the exact test-helper schema**

Create `eh_gallery_jobs::Model` and `eh_download_completions::Model` with fields matching the SQL above. Add optional belongs-to relations from queue and both ledgers, and has-many relations from jobs:

```rust
impl Related<super::eh_gallery_jobs::Entity> for eh_download_queue::Entity { /* Job */ }
impl Related<super::eh_gallery_jobs::Entity> for eh_gp_spend_attempts::Entity { /* Job */ }
impl Related<super::eh_gallery_jobs::Entity> for eh_download_completions::Entity { /* Job */ }
impl Related<super::eh_download_queue::Entity> for eh_gallery_jobs::Entity { /* Deliveries */ }
impl Related<super::eh_gp_spend_attempts::Entity> for eh_gallery_jobs::Entity { /* GpAttempts */ }
impl Related<super::eh_download_completions::Entity> for eh_gallery_jobs::Entity { /* DownloadCompletions */ }
```

Update `setup_test_db()` so `eh_gallery_jobs` exists before queue/ledger inserts, `eh_download_completions` matches production exactly, queue and GP ledger each expose nullable `job_id`, and all indexes/foreign keys/defaults match the migration. Keep the compatibility columns on `eh_download_queue` because existing tests and terminal history still deserialize them.

- [ ] **Step 5: Run migration/entity tests and confirm GREEN**

Run:

```powershell
cargo test -p pixivbot --bin pixivbot migration_groups_active_legacy_variants_and_leaves_terminal_history_unbound -- --nocapture
cargo test -p pixivbot --bin pixivbot migration_backfills_append_only_download_completions_before_clearing_compatibility -- --nocapture
cargo test -p pixivbot --bin pixivbot shared_job_test_schema_enforces_variant_and_foreign_keys -- --nocapture
cargo test -p pixivbot --bin pixivbot migration_rolls_back_ddl_when_legacy_backfill_fails -- --nocapture
cargo test -p pixivbot --bin pixivbot eh_gp_spend_attempts -- --nocapture
```

Expected: all commands PASS; migration grouping is one job per legacy variant, terminal rows remain unbound, three historical completion generations survive with correct nullable provenance, and existing GP migration tests still pass.

### Task 2: Canonical transactional enqueue, deduplication, and variant rebinding

**Files:**
- Create: `src/db/repo/eh_gallery_jobs.rs`
- Modify: `src/db/repo.rs:4-11`
- Modify: `src/db/repo/eh_download_queue.rs:18-616` and colocated tests
- Modify: `src/config.rs:328-637,700-819`
- Modify: `config.toml.example:175-205`
- Modify: `src/bot/handler.rs:20-80`
- Modify: `src/bot/mod.rs:9-51,74-89`
- Modify: `src/bot/handlers/subscription/ehentai.rs:409-420,537-548`
- Modify: `src/scheduler/eh_engine.rs:600-623,830-1004`
- Modify: `src/main.rs:480-502`

**Interfaces:**
- Consumes: `EhGalleryVariant`, job cleanup columns, and job entity from Task 1; current direct-wins/source-owner merge semantics; `EhClient::is_logged_in()`; `EhentaiConfig::{download_resolution,subscription_resolution}`.
- Produces: `EhGalleryVariant::{for_request,archive,images}`; transactional `Repo::enqueue_eh_download(..., variant: &EhGalleryVariant)` and `Repo::enqueue_eh_subscription_download(..., variant: &EhGalleryVariant)`; internal `retire_consumerless_eh_job_in_txn`; dirty-retired binding without reactivation; `EhentaiConfig::publish_concurrency_clamped() -> usize`.

- [ ] **Step 1: Add failing concurrent sharing, isolation, and rebind tests**

```rust
#[tokio::test]
async fn shared_enqueue_is_atomic_across_job_and_delivery_unique_constraints() {
    let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
    let variant = EhGalleryVariant::archive("1280x");
    let (left, right) = tokio::join!(
        repo.enqueue_eh_download(-100, 42, "tok", "A", true, SOURCE_DIRECT, &variant),
        repo.enqueue_eh_download(-200, 42, "tok", "A", true, SOURCE_DIRECT, &variant),
    );
    let left = left.unwrap();
    let right = right.unwrap();
    assert_ne!(left.id, right.id);
    assert_eq!(left.job_id, right.job_id);
    assert_eq!(count_jobs(&repo, 42).await, 1);
    assert_eq!(count_deliveries(&repo, 42).await, 2);

    let (same_chat_left, same_chat_right) = tokio::join!(
        repo.enqueue_eh_download(-300, 42, "tok", "A", false, SOURCE_DIRECT, &variant),
        repo.enqueue_eh_download(-300, 42, "tok", "A", true, SOURCE_DIRECT, &variant),
    );
    let same_chat_left = same_chat_left.unwrap();
    let same_chat_right = same_chat_right.unwrap();
    assert_eq!(same_chat_left.id, same_chat_right.id);
    assert_eq!(count_deliveries_for_chat_gid(&repo, -300, 42).await, 1);
    assert!(load_delivery(&repo, same_chat_left.id).await.telegraph,
        "delivery-unique conflict recovery must reselect and merge intent");
}

#[tokio::test]
async fn enqueue_isolates_variants_and_rebinds_direct_upgrade_before_markers() {
    let repo = tests_helpers::setup_test_db().await.unwrap();
    let subscription = EhGalleryVariant::archive("980x");
    let direct = EhGalleryVariant::archive("original");
    let delivery = repo.enqueue_eh_subscription_download(
        -100, 7, 42, "tok", "A", false, &subscription).await.unwrap();
    let old_job = delivery.job_id.unwrap();

    let upgraded = repo.enqueue_eh_download(
        -100, 42, "tok", "A2", true, SOURCE_DIRECT, &direct).await.unwrap();
    assert_ne!(upgraded.job_id, Some(old_job));
    assert_eq!(upgraded.source, SOURCE_DIRECT);
    assert_eq!(job_variant(&repo, upgraded.job_id.unwrap()).await, ("archive", "original"));
    assert_eq!(job_status(&repo, old_job).await, JOB_STATUS_RETIRED);

    set_archive_marker(&repo, upgraded.id).await;
    let marker_safe = repo.enqueue_eh_download(
        -100, 42, "tok", "A3", true, SOURCE_DIRECT,
        &EhGalleryVariant::archive("780x")).await.unwrap();
    assert_eq!(marker_safe.job_id, upgraded.job_id,
        "a committed send marker prevents mid-wave variant rebinding");
    assert_eq!(count_active_jobs_for_variant(&repo, 42, "archive", "780x").await, 0,
        "a marker-blocked requested variant must not leave an active orphan job");
}

#[tokio::test]
async fn enqueue_binds_dirty_retired_job_without_reactivating_or_clearing_artifacts() {
    let repo = tests_helpers::setup_test_db().await.unwrap();
    let variant = EhGalleryVariant::archive("original");
    let dirty = seed_retired_job(&repo, 88, "tok", &variant,
        CLEANUP_STATUS_FAILED, Some("C:/cache/88.zip"), Some("abort failed")).await;

    let delivery = repo.enqueue_eh_download(
        -100, 88, "tok", "A", true, SOURCE_DIRECT, &variant).await.unwrap();
    assert_eq!(delivery.job_id, Some(dirty.id));
    let rebound = load_job(&repo, dirty.id).await;
    assert_eq!(rebound.status, JOB_STATUS_RETIRED);
    assert_eq!(rebound.cleanup_status, CLEANUP_STATUS_FAILED);
    assert_eq!(rebound.zip_path.as_deref(), Some("C:/cache/88.zip"));
    assert_eq!(rebound.cleanup_error.as_deref(), Some("abort failed"));
}

#[test]
fn publish_concurrency_defaults_to_two_and_clamps_to_supported_range() {
    assert_eq!(EhentaiConfig::default().publish_concurrency_clamped(), 2);
    assert_eq!(EhentaiConfig { publish_concurrency: 0, ..Default::default() }
        .publish_concurrency_clamped(), 1);
    assert_eq!(EhentaiConfig { publish_concurrency: 99, ..Default::default() }
        .publish_concurrency_clamped(), 10);
}
```

- [ ] **Step 2: Run the tests and confirm RED**

Run:

```powershell
cargo test -p pixivbot --bin pixivbot shared_enqueue_is_atomic_across_job_and_delivery_unique_constraints -- --nocapture
cargo test -p pixivbot --bin pixivbot enqueue_binds_dirty_retired_job_without_reactivating_or_clearing_artifacts -- --nocapture
cargo test -p pixivbot --bin pixivbot publish_concurrency_defaults_to_two_and_clamps_to_supported_range -- --nocapture
```

Expected: compilation FAILS because `EhGalleryVariant`, `job_id` enqueue binding, dirty-cleanup gating, and `publish_concurrency` APIs do not exist.

- [ ] **Step 3: Implement canonical variants and the public configuration contract**

Use these exact rules:

```rust
pub fn for_request(is_logged_in: bool, source: &str, config: &EhentaiConfig) -> Self {
    if !is_logged_in {
        return Self::images();
    }
    let resolution = if source == SOURCE_DIRECT {
        config.download_resolution.clone()
    } else {
        config.subscription_resolution.clone()
    };
    Self::archive(resolution)
}

pub fn publish_concurrency_clamped(&self) -> usize {
    self.publish_concurrency.clamp(1, 10)
}
```

Add `#[serde(default = "default_eh_publish_concurrency")] pub publish_concurrency: usize`, with a raw default of `2`; accept configured zero and clamp it to one rather than failing deserialization. Document that this controls only Telegram delivery futures and that `Throttle<Bot>` remains authoritative.

- [ ] **Step 4: Replace two-step enqueue with one retryable database transaction**

Extend `EhEnqueueRequest` with `variant: &'a EhGalleryVariant`. Use `DatabaseTransaction` for every select/insert/update and a maximum of three whole-transaction attempts. One attempt must:

1. Select the unique job by all four variant columns.
2. Insert it as `pending/not_required` when absent. On a job unique conflict, roll back, reselect on the next attempt, and continue.
3. Reactivate a clean `retired` or consumerless retryable `failed` job only when `cleanup_status='none'`, clearing the previous generation's shared transient state and setting `pending`; never reset a job that still has an active delivery. If cleanup is `pending`, `running`, or `failed`, bind the new delivery but preserve status, paths, manifests, cleanup error/generation, and retry time exactly—the job remains non-claimable until Task 9 maintenance finalizes cleanup.
4. Select or insert the unique `(chat_id, gid)` delivery. On a delivery unique conflict, roll back and reselect on the next attempt.
5. Apply existing direct-wins and subscription-owner CAS merge rules using delivery statuses `waiting/publishing/done/failed/canceled`.
6. Rebind to the requested job only when both sent markers are null. Preserve a marker-bearing delivery's current job until that wave terminates.
7. Recompute `telegraph_required` from active deliveries (`waiting` or `publishing`) whose `telegraph` is true and `telegraph_sent_at` is null. If a downloaded job gains its first Telegraph consumer, set `telegraph_status='pending'`; if the aggregate becomes false before upload claim, set `not_required`.
8. Evaluate the old job after a successful rebind; mark it `retired` only if it has no active delivery and no pending/rewriting Telegraph rewrite. Set cleanup `pending` when it owns an artifact family; an empty newly-created job may become clean `retired/none` without maintenance.
9. If a committed send marker prevents rebinding, retire a just-created requested job when it has no other active consumer, so enqueue cannot leave an unowned `pending` job; this empty job uses cleanup `none`.
10. Update the job's shared title to the latest non-empty metadata title while keeping each delivery's own title; an empty incoming title never erases a non-empty job title.
11. Commit and re-read the delivery. Any unique conflict or SQLite busy/serialization conflict retries the complete transaction; other errors retain their context and return.

Expose the exact signatures:

```rust
pub async fn enqueue_eh_download(
    &self, chat_id: i64, gid: i64, token: &str, title: &str,
    telegraph: bool, source: &str, variant: &EhGalleryVariant,
) -> Result<eh_download_queue::Model>;

pub async fn enqueue_eh_subscription_download(
    &self, chat_id: i64, subscription_id: i32, gid: i64,
    token: &str, title: &str, telegraph: bool,
    variant: &EhGalleryVariant,
) -> Result<eh_download_queue::Model>;

pub(crate) async fn retire_consumerless_eh_job_in_txn(
    txn: &DatabaseTransaction, job_id: i32,
) -> Result<EhJobCleanupDecision>;
```

`retire_consumerless_eh_job_in_txn` is the transaction-local form used only after rebind/marker-blocked orphan handling. It rechecks active deliveries, respects the rewrite interlock, and persists cleanup `pending` when the old job owns an artifact; Task 7's public liveness API generalizes the same state rules for cancellation/completion with archive policy.

- [ ] **Step 5: Thread canonical variants through every enqueue caller**

Store `Arc<EhentaiConfig>` in `BotHandler`; thread it through `bot::run` from `main.rs`. `/edl` and `/telegraph` calculate the direct variant with the existing `EhClient::is_logged_in()`. `EhEngine` calculates the subscription variant once per enqueue from its existing client/config. Do not change command text or bot throttling.

- [ ] **Step 6: Run enqueue/config/caller tests and confirm GREEN**

Run:

```powershell
cargo test -p pixivbot --bin pixivbot shared_enqueue_is_atomic_across_job_and_delivery_unique_constraints -- --nocapture
cargo test -p pixivbot --bin pixivbot enqueue_isolates_variants_and_rebinds_direct_upgrade_before_markers -- --nocapture
cargo test -p pixivbot --bin pixivbot enqueue_binds_dirty_retired_job_without_reactivating_or_clearing_artifacts -- --nocapture
cargo test -p pixivbot --bin pixivbot publish_concurrency_defaults_to_two_and_clamps_to_supported_range -- --nocapture
cargo test -p pixivbot --bin pixivbot collect_overflow_pending_enqueued_on_next_tick -- --nocapture
```

Expected: all commands PASS; concurrent same-variant requests produce one job/two deliveries, variant isolation is exact, dirty jobs retain their old generation while accepting a consumer, direct upgrade remains marker-safe, and subscription backlog behavior remains green.

### Task 3: Download one shared artifact and charge GP once per job attempt

**Files:**
- Modify: `src/db/repo/eh_gallery_jobs.rs`
- Modify: `src/db/repo/eh_gp_spend_attempts.rs:7-56,58-468`
- Create: `src/db/repo/eh_download_completions.rs`
- Modify: `src/db/repo.rs:4-11`
- Modify: `src/scheduler/eh_engine.rs:1-345,1113-1408` and download/GP tests
- Modify: `src/db/repo/eh_integration_tests.rs`

**Interfaces:**
- Consumes: canonical jobs/deliveries from Task 2; `eh_download_completions::Model` from Task 1; `ArchiveArtifacts`; current `check_and_reserve_archive_cost`; `EhClient::{prepare_archive_download,download_archive_with_request_and_options,download_gallery_images}`.
- Produces: job-scoped download claim/transition/retry APIs, job-scoped artifact path, `Repo::append_eh_job_gp_spend_attempt(job_id, gid, gp_cost)`, atomic `append_eh_download_completion_in_txn`, and `Repo::get_eh_downloaded_bytes_in_window` over the completion ledger.

- [ ] **Step 1: Add a failing shared download/GP/artifact isolation test**

```rust
#[tokio::test]
async fn two_chats_share_one_archive_post_one_gp_attempt_and_one_artifact() {
    let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
    setup_chat(&repo, -100, true).await;
    setup_chat(&repo, -200, true).await;
    let variant = EhGalleryVariant::archive("1280x");
    let first = repo.enqueue_eh_download(-100, 2284788, "7841d194d4", "G", false,
        SOURCE_DIRECT, &variant).await.unwrap();
    let second = repo.enqueue_eh_download(-200, 2284788, "7841d194d4", "G", false,
        SOURCE_DIRECT, &variant).await.unwrap();
    assert_eq!(first.job_id, second.job_id);

    mount_paid_archive_flow(&eh_server, 2284788, "7841d194d4", 218, 1).await;
    let worker = make_download_worker(repo.clone(), &eh_server, temp.path(), 500);
    worker.tick().await.unwrap();

    assert_eq!(archiver_post_count(&eh_server).await, 1);
    let job = load_job(&repo, first.job_id.unwrap()).await;
    assert_eq!(job.status, JOB_STATUS_DOWNLOADED);
    assert_eq!(job.gp_cost, 218);
    assert!(std::path::Path::new(job.zip_path.as_ref().unwrap()).exists());
    assert_eq!(active_delivery_statuses(&repo, job.id).await,
        vec![DELIVERY_STATUS_WAITING, DELIVERY_STATUS_WAITING]);
    let attempts = load_gp_attempts(&repo).await;
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].job_id, Some(job.id));
    assert_eq!(attempts[0].queue_id, None);
    let completions = load_download_completions(&repo).await;
    assert_eq!(completions.len(), 1);
    assert_eq!(completions[0].job_id, Some(job.id));
    assert_eq!(completions[0].file_size, job.file_size);
    assert_eq!(repo.get_eh_downloaded_bytes_in_window(24).await.unwrap(), job.file_size);
}

#[tokio::test]
async fn completion_ledger_counts_both_generations_after_clean_retired_job_reactivation() {
    let repo = tests_helpers::setup_test_db().await.unwrap();
    let variant = EhGalleryVariant::archive("original");
    let delivery = repo.enqueue_eh_download(
        -100, 42, "tok", "first", false, SOURCE_DIRECT, &variant).await.unwrap();
    let job_id = delivery.job_id.unwrap();

    let first_claim = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
    repo.mark_eh_job_downloaded(job_id, first_claim.started_at.unwrap(),
        100, "C:/cache/first.zip", 0).await.unwrap();
    // This test-only fixture establishes Task 2's clean-retired precondition directly;
    // Task 9 separately proves the real cleanup transition that creates it.
    seed_delivery_done_and_job_clean_retired(&repo, delivery.id, job_id).await;

    let rebound = repo.enqueue_eh_download(
        -100, 42, "tok", "second", false, SOURCE_DIRECT, &variant).await.unwrap();
    assert_eq!(rebound.job_id, Some(job_id));
    let second_claim = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
    repo.mark_eh_job_downloaded(job_id, second_claim.started_at.unwrap(),
        250, "C:/cache/second.zip", 0).await.unwrap();

    let rows = load_download_completions(&repo).await;
    assert_eq!(rows.iter().map(|row| row.file_size).collect::<Vec<_>>(), vec![100, 250]);
    assert_eq!(rows.iter().map(|row| row.job_id).collect::<Vec<_>>(),
        vec![Some(job_id), Some(job_id)]);
    assert_eq!(repo.get_eh_downloaded_bytes_in_window(24).await.unwrap(), 350);
}

#[test]
fn artifact_identity_contains_job_and_sanitized_variant() {
    let archive = sample_job(11, 42, "tok", "archive", "1280x");
    let images = sample_job(12, 42, "tok", "images", "");
    assert_eq!(artifact_filename(&archive), "42_tok_j11_archive_1280x.zip");
    assert_eq!(artifact_filename(&images), "42_tok_j12_images_none.zip");
}
```

- [ ] **Step 2: Run the tests and confirm RED**

Run:

```powershell
cargo test -p pixivbot --bin pixivbot two_chats_share_one_archive_post_one_gp_attempt_and_one_artifact -- --nocapture
cargo test -p pixivbot --bin pixivbot completion_ledger_counts_both_generations_after_clean_retired_job_reactivation -- --nocapture
```

Expected: compilation FAILS because download claims/GP attempts still use queue rows/IDs and no append-only completion ledger repository exists.

- [ ] **Step 3: Implement job download claims and generation-safe transitions**

Add these repository methods with the current monotonic whole-second claim generation and conditional-update/readback pattern:

```rust
pub async fn get_next_eh_job_for_download(&self) -> Result<Option<eh_gallery_jobs::Model>>;
pub async fn eh_job_has_active_deliveries(&self, job_id: i32) -> Result<bool>;
pub async fn defer_eh_job_download(&self, job_id: i32, delay_secs: i64) -> Result<()>;
pub async fn schedule_eh_job_download_retry(
    &self, job_id: i32, expected_started_at: DateTime,
    error: &str, max_retry_count: u8,
) -> Result<(eh_gallery_jobs::Model, bool)>;
pub async fn fail_eh_job_for_archive_policy(
    &self, job: &eh_gallery_jobs::Model, error: &str,
) -> Result<eh_gallery_jobs::Model>;
pub async fn mark_eh_job_downloaded(
    &self, job_id: i32, expected_started_at: DateTime,
    file_size: i64, zip_path: &str, gp_cost: i64,
) -> Result<eh_gallery_jobs::Model>;

// src/db/repo/eh_download_completions.rs; callable only by a surrounding
// job-transition transaction, never as a standalone post-commit append.
pub(crate) async fn append_eh_download_completion_in_txn(
    txn: &DatabaseTransaction, job_id: i32, gid: i64,
    file_size: i64, created_at: DateTime,
) -> Result<eh_download_completions::Model>;
pub async fn get_eh_downloaded_bytes_in_window(&self, hours: i64) -> Result<i64>;
```

The claim is `pending -> downloading`, requires `cleanup_status='none'`, excludes background-owned jobs, honors `next_retry_at`, and retains current recent-FIFO/old-LIFO ordering. Before network work, the worker checks that at least one active delivery exists; if none, it retires the job without a source request. Disabled chats may defer the job only when every active destination is currently non-notifiable; one enabled destination allows shared work to proceed.

`mark_eh_job_downloaded` starts one `DatabaseTransaction`, CAS-updates exactly the claimed `downloading` generation, appends exactly one completion row with the job's `gid`, successful `file_size`, and the same completion timestamp, then commits. A stale CAS rolls back without a completion row. Task 4's background completion must use the same private append helper in its own CAS transaction so every successful generation, regardless of worker lane, has one ledger row.

On terminal download/policy failure, store the internal error only on the job, set the job `failed` once, and transition every active delivery to `failed` without copying the chain or creating a retry per chat. Do not add a new notification or change existing Telegram copy in this stage; log the internal chain and expose only the existing friendly `/estatus` failure label.

- [ ] **Step 4: Make variant choice and artifact ownership job-scoped**

Replace `archive_artifacts_for_entry` with:

```rust
fn archive_artifacts_for_job(
    cache_dir: &std::path::Path,
    job: &eh_gallery_jobs::Model,
) -> ArchiveArtifacts;

fn artifact_filename(job: &eh_gallery_jobs::Model) -> String;
fn sanitize_artifact_component(value: &str) -> String;
```

The filename is `{gid}_{token}_j{id}_{mode}_{resolution-or-none}.zip`; sanitize every interpolated component to ASCII alphanumeric, `-`, `_`, or `.`, replacing all other characters with `_`.

Canonical `archive` jobs use their persisted resolution. Canonical `images` jobs call `download_gallery_images`. A legacy job resolves `direct`/`subscription` through current login/config only for this drain, without changing its persisted unique identity.

- [ ] **Step 5: Move GP and byte accounting to jobs**

Change the ledger API to:

```rust
pub async fn append_eh_job_gp_spend_attempt(
    &self, job_id: i32, gid: i64, gp_cost: i64,
) -> Result<eh_gp_spend_attempts::Model>;
```

New GP rows set `job_id=Some(job_id)` and `queue_id=None`. Change `check_and_reserve_archive_cost` to accept `job_id`; keep `EH_GP_BUDGET_LOCK`, positive-only validation, reserve-before-POST ordering, and conservative defer behavior. Implement `get_eh_downloaded_bytes_in_window` as `SUM(file_size)` over `eh_download_completions.created_at` in the clamped rolling interval—never over mutable job rows or delivery compatibility fields. Keep historical migration assertions for `queue_id`; deleting a retained job sets both ledger `job_id` values to null while preserving their rows and sums.

- [ ] **Step 6: Run shared download, GP, and regression tests and confirm GREEN**

Run:

```powershell
cargo test -p pixivbot --bin pixivbot two_chats_share_one_archive_post_one_gp_attempt_and_one_artifact -- --nocapture
cargo test -p pixivbot --bin pixivbot completion_ledger_counts_both_generations_after_clean_retired_job_reactivation -- --nocapture
cargo test -p pixivbot --bin pixivbot artifact_identity_contains_job_and_sanitized_variant -- --nocapture
cargo test -p pixivbot --bin pixivbot check_and_reserve_archive_cost -- --nocapture
cargo test -p pixivbot --bin pixivbot download_worker_ -- --nocapture
```

Expected: all commands PASS; one shared job causes one POST/GP row/completion row/artifact, two generations on the same reused job contribute both completion rows and bytes, policy/rate limits remain before POST, and terminal download failure sends no Telegram message.

### Task 4: Share background download ownership and recover job claims

**Files:**
- Modify: `src/db/repo/eh_gallery_jobs.rs`
- Modify: `src/scheduler/eh_engine.rs:347-595` and background tests
- Modify: `src/main.rs:279-306,383-399`

**Interfaces:**
- Consumes: job claim generation and job-scoped GP/artifacts from Task 3; current `EhBackgroundDownloadWorker` concurrency and backoff settings.
- Produces: job-scoped background handoff/claim/defer/retry/complete/release/reset methods; unchanged worker constructor.

- [ ] **Step 1: Add failing normal/background exclusion, shared GP, and stale-reset tests**

```rust
#[tokio::test]
async fn normal_and_background_claims_cannot_own_the_same_job_generation() {
    let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
    let delivery = enqueue_shared_archive(&repo, -100, 77).await;
    let job_id = delivery.job_id.unwrap();
    let claimed = repo.get_next_eh_job_for_download().await.unwrap().unwrap();
    assert_eq!(claimed.id, job_id);
    repo.schedule_eh_job_background_download(job_id, JOB_STATUS_DOWNLOADING, "slow")
        .await.unwrap();

    let (main, background) = tokio::join!(
        repo.get_next_eh_job_for_download(),
        repo.get_next_eh_job_for_background_download(),
    );
    assert!(main.unwrap().is_none());
    assert_eq!(background.unwrap().unwrap().id, job_id);

    age_background_claim(&repo, job_id).await;
    assert_eq!(repo.reset_stale_eh_job_background_downloads(1).await.unwrap(), 1);
    assert_eq!(repo.get_next_eh_job_for_background_download().await.unwrap().unwrap().id, job_id);
}
```

Extend the existing paid background wiremock scenario to enqueue two chats into the same job and assert one archive POST, one GP ledger `job_id`, one completion row with that same `job_id`, and one contribution to `get_eh_downloaded_bytes_in_window`.

- [ ] **Step 2: Run the background tests and confirm RED**

Run:

```powershell
cargo test -p pixivbot --bin pixivbot normal_and_background_claims_cannot_own_the_same_job_generation -- --nocapture
```

Expected: compilation FAILS because background state and methods still belong to deliveries.

- [ ] **Step 3: Implement job-scoped background transitions**

Provide these exact APIs and port the current CAS predicates from queue columns to job columns:

```rust
pub async fn schedule_eh_job_background_download(
    &self, job_id: i32, expected_status: &str, error: &str,
) -> Result<eh_gallery_jobs::Model>;
pub async fn get_next_eh_job_for_background_download(
    &self,
) -> Result<Option<eh_gallery_jobs::Model>>;
pub async fn defer_eh_job_background_download(
    &self, job_id: i32, delay_secs: i64, reason: &str,
) -> Result<eh_gallery_jobs::Model>;
pub async fn mark_eh_job_background_downloaded(
    &self, job_id: i32, expected_started_at: DateTime,
    file_size: i64, zip_path: &str, gp_cost: i64,
) -> Result<eh_gallery_jobs::Model>;
pub async fn schedule_eh_job_background_retry(
    &self, job_id: i32, expected_started_at: DateTime,
    error: &str, max_attempts: u8,
) -> Result<(eh_gallery_jobs::Model, bool)>;
pub async fn reset_stale_eh_job_background_downloads(&self, stale_sec: u64) -> Result<u64>;
pub async fn release_eh_job_background_downloads_to_main_queue(&self) -> Result<u64>;
```

`mark_eh_job_background_downloaded` performs its generation CAS and calls Task 3's `append_eh_download_completion_in_txn` inside one `DatabaseTransaction`; a stale/canceled generation rolls back both changes and appends no row. Keep `background_download_concurrency` and `drain_background_download_tasks` unchanged in meaning. Do not introduce normal download concurrency. A canceled final consumer makes completion CAS fail safely and leaves cleanup to Task 9; it must not resurrect a delivery.

- [ ] **Step 4: Update startup calls and run background tests GREEN**

Run:

```powershell
cargo test -p pixivbot --bin pixivbot normal_and_background_claims_cannot_own_the_same_job_generation -- --nocapture
cargo test -p pixivbot --bin pixivbot background_worker_ -- --nocapture
cargo test -p pixivbot --bin pixivbot main_and_background_gp_rate_limit_allows_only_one_post -- --nocapture
```

Expected: all commands PASS; normal/background claims are mutually exclusive per job generation, stale claims resume once, and GP remains one ledger row per actual POST.

### Task 5: Upload to IPFS/Telegraph once and isolate Telegraph-only failure

**Files:**
- Modify: `src/db/repo/eh_gallery_jobs.rs`
- Modify: `src/db/repo/eh_download_queue.rs`
- Modify: `src/scheduler/eh_engine.rs:1411-1841` and upload tests

**Interfaces:**
- Consumes: downloaded shared job/artifact from Task 3; `telegraph_required` aggregate from Task 2; existing ZIP-first/per-image upload and `UploadResumeContext` behavior.
- Produces: job-scoped upload claim/complete/retry/failure APIs; one shared `telegraph_url` and rewrite payload; `EhJobUploadFailureOutcome` carrying exact newly-failed delivery IDs/chats/titles; fixed per-Telegraph-delivery notification attempts.

- [ ] **Step 1: Add failing shared upload, late-requirement, and failure-isolation tests**

```rust
#[tokio::test]
async fn two_telegraph_deliveries_upload_zip_and_create_page_once() {
    let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
    let job = seed_downloaded_job_with_deliveries(&repo, &[(-100, true), (-200, true)]).await;
    let uploader = Arc::new(ZipFirstMockUploader::default());
    mount_telegraph_create_page_once(&tg_server).await;
    make_upload_worker(repo.clone(), uploader.clone(), &tg_server).tick().await.unwrap();

    assert_eq!(uploader.zip_calls.load(Ordering::SeqCst), 1);
    assert_eq!(telegraph_create_page_count(&tg_server).await, 1);
    let ready = load_job(&repo, job.id).await;
    assert_eq!(ready.telegraph_status, TELEGRAPH_STATUS_READY);
    assert!(ready.telegraph_url.is_some());
    assert_eq!(active_delivery_statuses(&repo, job.id).await,
        vec![DELIVERY_STATUS_WAITING, DELIVERY_STATUS_WAITING]);
}

#[tokio::test]
async fn late_telegraph_consumer_reuses_download_and_terminal_upload_failure_is_scoped() {
    let repo = tests_helpers::setup_test_db().await.unwrap();
    let archive_only = seed_downloaded_job_with_deliveries(&repo, &[(-100, false)]).await;
    let late = repo.enqueue_eh_download(-200, archive_only.gid, &archive_only.token,
        &archive_only.title, true, SOURCE_DIRECT,
        &job_variant_value(&archive_only)).await.unwrap();
    let updated = load_job(&repo, archive_only.id).await;
    assert_eq!(updated.status, JOB_STATUS_DOWNLOADED);
    assert_eq!(updated.telegraph_status, TELEGRAPH_STATUS_PENDING);

    let claimed = repo.get_next_eh_job_for_upload().await.unwrap().unwrap();
    let outcome = repo.record_eh_job_upload_failure(
        claimed.id, claimed.started_at.unwrap(), "provider secret", 0).await.unwrap();
    let EhJobUploadFailureOutcome::Terminal { deliveries, .. } = outcome else {
        panic!("expected terminal upload failure");
    };
    assert_eq!(deliveries, vec![EhFailedTelegraphDelivery {
        delivery_id: late.id,
        chat_id: -200,
        title: archive_only.title.clone(),
    }]);
    assert_eq!(delivery_status(&repo, late.id).await, DELIVERY_STATUS_FAILED);
    assert_eq!(delivery_status_for_chat(&repo, updated.id, -100).await,
        DELIVERY_STATUS_WAITING);
    assert_eq!(load_job(&repo, updated.id).await.telegraph_status,
        TELEGRAPH_STATUS_FAILED);
}

#[tokio::test]
async fn terminal_upload_notifies_each_telegraph_chat_once_and_never_archive_only() {
    let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
    let job = seed_downloaded_job_with_deliveries(
        &repo, &[(-100, true, "T1"), (-200, true, "T2"), (-300, false, "Archive")]).await;
    let uploader = Arc::new(AlwaysFailUploader::new(
        "sqlite secret; /private/path; multipart upload id=abc"));
    let worker = make_upload_worker_with_telegram(
        repo.clone(), uploader, &telegraph_server, &telegram_server, 0);
    worker.tick().await.unwrap();

    let messages = recorded_telegram_text_messages(&telegram_server).await;
    assert_eq!(messages.iter().map(|m| m.chat_id).collect::<Vec<_>>(), vec![-100, -200]);
    assert_eq!(messages.iter().map(|m| m.text.as_str()).collect::<Vec<_>>(), vec![
        "⚠️ Telegraph 上传失败，请稍后重试\n\n📦 T1",
        "⚠️ Telegraph 上传失败，请稍后重试\n\n📦 T2",
    ]);
    assert_eq!(delivery_status_for_chat(&repo, job.id, -300).await,
        DELIVERY_STATUS_WAITING);
    assert!(messages.iter().all(|m| !m.text.contains("sqlite secret")));
    assert!(messages.iter().all(|m| !m.text.contains("/private/path")));
    assert!(messages.iter().all(|m| !m.text.contains("upload id")));
}
```

Retain and adapt the existing abort-failure tests so an abort error leaves `uploads_dir`, parts, and ZIP present and returns the job to a retryable shared state.

- [ ] **Step 2: Run the upload tests and confirm RED**

Run:

```powershell
cargo test -p pixivbot --bin pixivbot two_telegraph_deliveries_upload_zip_and_create_page_once -- --nocapture
cargo test -p pixivbot --bin pixivbot terminal_upload_notifies_each_telegraph_chat_once_and_never_archive_only -- --nocapture
```

Expected: compilation FAILS because `EhUploadWorker` and upload transitions still claim a delivery row.

- [ ] **Step 3: Implement job-scoped Telegraph claims and transitions**

Add:

```rust
pub async fn get_next_eh_job_for_upload(&self) -> Result<Option<eh_gallery_jobs::Model>>;
pub async fn mark_eh_job_telegraph_ready(
    &self, job_id: i32, expected_started_at: DateTime,
    telegraph_url: &str, rewrite_data_json: Option<&str>,
) -> Result<eh_gallery_jobs::Model>;
pub async fn record_eh_job_upload_failure(
    &self, job_id: i32, expected_started_at: DateTime,
    error: &str, max_retry_count: u8,
) -> Result<EhJobUploadFailureOutcome>;
```

The claim predicate is `status='downloaded' AND telegraph_required=TRUE AND telegraph_status='pending'` with due retry and a generation CAS. `EhUploadWorker` no longer checks one chat or acquires a publish/cancel lock; the aggregate is the eligibility check. If cancellation removes the final Telegraph consumer before claim, no upload starts. Once status is `uploading`, cancellation does not interrupt it or delete manifests; success/abort completes normally.

Port ZIP-first upload, per-image fallback, Telegraph creation, and rewrite serialization unchanged except that they read job fields and persist once on the job. `record_eh_job_upload_failure` runs in one transaction: a retryable generation returns `RetryScheduled(job)`; a stale generation returns `Stale`; terminal exhaustion stores the internal chain only in the job error, sets `telegraph_status='failed'`, updates only active deliveries with `telegraph=TRUE AND telegraph_sent_at IS NULL` to `failed` with no raw provider error, and returns those newly transitioned rows as `Terminal { job, deliveries }` ordered by delivery ID. Archive-only deliveries remain waiting/publishable. A repeated/stale terminal call returns no affected deliveries, preventing duplicate fan-out.

On `Terminal`, `EhUploadWorker` calls `Notifier::send_text(ChatId(delivery.chat_id), &message, false)` once for each returned delivery and logs send failures without changing another delivery. The exact message is `⚠️ Telegraph 上传失败，请稍后重试\n\n📦 {escaped_title}`; only `title` is interpolated after `teloxide::utils::markdown::escape`. Never format the shared `error`, `e.to_string()`, a path, or provider identifiers into Telegram text. Download terminal failure remains untouched and sends no message.

- [ ] **Step 4: Preserve multipart abort fail-closed behavior at job scope**

Rename entry helpers to job helpers without weakening their gates:

```rust
async fn ensure_job_upload_state_aborted(
    job: &eh_gallery_jobs::Model,
    abort_uploader: Option<&dyn ImageUploader>,
) -> Result<UploadStateAbortPermit>;
async fn remove_job_upload_state(job: &eh_gallery_jobs::Model, permit: UploadStateAbortPermit);
async fn remove_job_archive_family(job: &eh_gallery_jobs::Model, permit: UploadStateAbortPermit);
```

No-abort-uploader and provider-abort-failed remain typed internal errors; they do not include manifest contents or remote upload IDs.

- [ ] **Step 5: Run shared upload and multipart tests GREEN**

Run:

```powershell
cargo test -p pixivbot --bin pixivbot two_telegraph_deliveries_upload_zip_and_create_page_once -- --nocapture
cargo test -p pixivbot --bin pixivbot late_telegraph_consumer_reuses_download_and_terminal_upload_failure_is_scoped -- --nocapture
cargo test -p pixivbot --bin pixivbot terminal_upload_notifies_each_telegraph_chat_once_and_never_archive_only -- --nocapture
cargo test -p pixivbot --bin pixivbot upload_worker_ -- --nocapture
cargo test -p pixivbot --bin pixivbot abort_fails -- --nocapture
```

Expected: all commands PASS; one job performs one upload/page creation, late demand does not download again, affected Telegraph chats receive exactly one fixed non-secret message each, archive-only chats receive none, and abort failures preserve every local artifact.

### Task 6: Schedule and execute Telegraph gateway rewrite once per job

**Files:**
- Modify: `src/db/repo/eh_gallery_jobs.rs`
- Modify: `src/db/repo/eh_download_queue.rs`
- Modify: `src/scheduler/eh_engine.rs:2178-2267` and rewrite tests
- Modify: `src/main.rs:284-293,445-464`

**Interfaces:**
- Consumes: job rewrite payload/URL from Task 5; `Repo::evaluate_eh_job_liveness` from Task 7; delivery `telegraph_sent_at`; existing `TelegraphRewriteData` and `rewrite_ipfs_gateway_nodes`.
- Produces: one job-level post-first-send rewrite schedule/claim/retry/completion lifecycle plus the rewrite/retirement interlock and terminal liveness reevaluation.

- [ ] **Step 1: Add a failing two-delivery one-rewrite test**

```rust
#[tokio::test]
async fn first_telegraph_delivery_schedules_one_job_rewrite() {
    let repo = tests_helpers::setup_test_db().await.unwrap();
    let (job, first, second) = seed_ready_telegraph_job_with_two_deliveries(&repo).await;

    repo.mark_eh_telegraph_delivery_sent(first.id, job.id, Some(60)).await.unwrap();
    let after_first = load_job(&repo, job.id).await;
    let scheduled_after = after_first.telegraph_rewrite_after.unwrap();
    assert_eq!(after_first.telegraph_rewrite_status.as_deref(),
        Some(TELEGRAPH_REWRITE_STATUS_PENDING));

    repo.mark_eh_telegraph_delivery_sent(second.id, job.id, Some(60)).await.unwrap();
    let after_second = load_job(&repo, job.id).await;
    assert_eq!(after_second.telegraph_rewrite_after, Some(scheduled_after));

    run_rewrite_worker_once(&repo, &telegraph_server).await;
    assert_eq!(telegraph_edit_page_count(&telegraph_server).await, 1);
    assert!(load_job(&repo, job.id).await.telegraph_rewritten_at.is_some());
    assert!(repo.get_next_eh_job_for_telegraph_rewrite().await.unwrap().is_none());
}

#[tokio::test]
async fn final_delivery_with_delayed_rewrite_keeps_payload_until_rewrite_is_terminal() {
    let repo = tests_helpers::setup_test_db().await.unwrap();
    let (job, delivery, subscription_id) =
        seed_ready_telegraph_job_with_one_subscription_delivery(&repo).await;
    repo.mark_eh_telegraph_delivery_sent(delivery.id, job.id, Some(60)).await.unwrap();
    repo.cancel_eh_subscription_queue_entries(subscription_id, true).await.unwrap();

    let interleaved = load_job(&repo, job.id).await;
    assert_ne!(interleaved.status, JOB_STATUS_RETIRED);
    assert_eq!(interleaved.telegraph_rewrite_status.as_deref(),
        Some(TELEGRAPH_REWRITE_STATUS_PENDING));
    assert!(interleaved.telegraph_rewrite_data.is_some());

    make_job_rewrite_due(&repo, job.id).await;
    mount_telegraph_edit_failure(&telegraph_server, "private provider detail").await;
    run_rewrite_worker_once_with_max_retry(&repo, &telegraph_server, 0).await;
    let terminal = load_job(&repo, job.id).await;
    assert_eq!(terminal.telegraph_rewrite_status.as_deref(),
        Some(TELEGRAPH_REWRITE_STATUS_FAILED));
    assert_eq!(terminal.status, JOB_STATUS_RETIRED,
        "rewrite terminal transition must immediately reevaluate liveness");
    assert!(terminal.telegraph_rewrite_data.is_none());
    assert_eq!(terminal.cleanup_status, CLEANUP_STATUS_PENDING);
}
```

- [ ] **Step 2: Run the test and confirm RED**

Run:

```powershell
cargo test -p pixivbot --bin pixivbot first_telegraph_delivery_schedules_one_job_rewrite -- --nocapture
cargo test -p pixivbot --bin pixivbot final_delivery_with_delayed_rewrite_keeps_payload_until_rewrite_is_terminal -- --nocapture
```

Expected: compilation FAILS because rewrite payload and lifecycle still belong to each queue row.

- [ ] **Step 3: Move rewrite scheduling and worker claims to jobs**

Implement:

```rust
pub async fn mark_eh_telegraph_delivery_sent(
    &self, delivery_id: i32, job_id: i32, rewrite_delay_secs: Option<i64>,
) -> Result<()>;
pub async fn get_next_eh_job_for_telegraph_rewrite(
    &self,
) -> Result<Option<eh_gallery_jobs::Model>>;
pub async fn mark_eh_job_telegraph_rewritten(
    &self, job_id: i32, expected_started_at: DateTime,
) -> Result<bool>;
pub async fn schedule_eh_job_telegraph_rewrite_retry(
    &self, job_id: i32, expected_started_at: DateTime,
    error: &str, max_retry_count: u8,
) -> Result<bool>;
pub async fn reset_stale_eh_job_telegraph_rewrites(&self, stale_sec: i64) -> Result<u64>;
```

The marker and first schedule update occur in one transaction: always set the delivery marker, but set job rewrite fields only when payload exists and the job is unscheduled/unrewritten. The second delivery cannot replace the first `rewrite_after`. Pending or `rewriting` status makes `evaluate_eh_job_liveness` return `retire=false` and `preserve_rewrite_payload=true`; archive cleanup may still be scheduled when no delivery/upload needs the ZIP, but no path may clear the rewrite payload/page state.

Port existing stale-reset, backoff, terminal-failure, and page edit semantics to jobs. `mark_eh_job_telegraph_rewritten` returns `true` only for the matching claim generation. `schedule_eh_job_telegraph_rewrite_retry` returns `true` only when that matching generation reaches terminal failure. After either success or terminal failure, `EhTelegraphRewriteWorker` immediately calls `evaluate_eh_job_liveness(job_id, send_archive)`; only that terminal transition may clear `telegraph_rewrite_data` and retire a consumerless job. Retry scheduling and stale CAS results preserve payload unchanged.

- [ ] **Step 4: Run rewrite tests and confirm GREEN**

Run:

```powershell
cargo test -p pixivbot --bin pixivbot first_telegraph_delivery_schedules_one_job_rewrite -- --nocapture
cargo test -p pixivbot --bin pixivbot final_delivery_with_delayed_rewrite_keeps_payload_until_rewrite_is_terminal -- --nocapture
cargo test -p pixivbot --bin pixivbot telegraph_rewrite -- --nocapture
```

Expected: all commands PASS; two deliveries produce one due rewrite and one set of Telegraph edits, and the final delivery cannot retire/erase the job payload until rewrite success or terminal failure reevaluates liveness.

### Task 7: Preserve cancellation ordering with keyed chat locks and job liveness

**Files:**
- Modify: `src/db/repo/eh_download_queue.rs:18-25,933-1421` and cancellation tests
- Modify: `src/db/repo/eh_gallery_jobs.rs`
- Modify: `src/bot/handlers/subscription/helpers.rs:99-136` only if a renamed repository call requires it

**Interfaces:**
- Consumes: job/delivery ownership and Telegraph aggregate from Task 2; cleanup/rewrite state columns from Task 1; existing subscription CSV merge/CAS rules.
- Produces: `EH_CHAT_LOCKS`; keyed single/multi-chat guards; `Repo::cancel_eh_subscription_queue_entries(subscription_id, send_archive)` and `Repo::delete_eh_subscription_and_cancel_queue(subscription_id, send_archive)`; cancellation that recomputes Telegraph aggregate and durably schedules liveness cleanup without affecting sibling chats.

- [ ] **Step 1: Add failing lock ordering and cancellation-isolation tests**

```rust
#[tokio::test]
async fn chat_locks_serialize_same_chat_but_not_different_chats() {
    let first = EH_CHAT_LOCKS.lock_chat(-100).await;
    let blocked_same = tokio::spawn(async { EH_CHAT_LOCKS.lock_chat(-100).await });
    let free_other = tokio::spawn(async { EH_CHAT_LOCKS.lock_chat(-200).await });
    assert!(tokio::time::timeout(Duration::from_millis(50), free_other).await.is_ok());
    assert!(tokio::time::timeout(Duration::from_millis(50), blocked_same).await.is_err());
    drop(first);

    let guards = EH_CHAT_LOCKS.lock_chats(&[-1, -3, -2, -1]).await;
    assert_eq!(guards.len(), 3);
}

#[tokio::test]
async fn cancel_one_shared_delivery_keeps_sibling_job_and_artifact_live() {
    let repo = tests_helpers::setup_test_db().await.unwrap();
    let (job, canceled, sibling) = seed_shared_subscription_job(&repo).await;
    repo.cancel_eh_subscription_queue_entries(canceled.subscription_id, true)
        .await.unwrap();

    assert_eq!(delivery_status(&repo, canceled.delivery_id).await, DELIVERY_STATUS_CANCELED);
    assert_eq!(delivery_status(&repo, sibling.delivery_id).await, DELIVERY_STATUS_WAITING);
    assert_ne!(load_job(&repo, job.id).await.status, JOB_STATUS_RETIRED);
    assert_eq!(load_job(&repo, job.id).await.telegraph_required, sibling.telegraph);
    assert!(std::path::Path::new(job.zip_path.as_ref().unwrap()).exists());
}

#[tokio::test]
async fn cancellation_before_and_after_upload_claim_obeys_aggregate_boundary() {
    let repo = tests_helpers::setup_test_db().await.unwrap();
    let before = seed_pending_telegraph_job(&repo, -100).await;
    cancel_only_owner(&repo, &before).await;
    assert_eq!(load_job(&repo, before.id).await.telegraph_status,
        TELEGRAPH_STATUS_NOT_REQUIRED);

    let after = seed_uploading_telegraph_job(&repo, -200).await;
    cancel_only_owner(&repo, &after).await;
    assert_eq!(load_job(&repo, after.id).await.telegraph_status,
        TELEGRAPH_STATUS_UPLOADING);
}
```

- [ ] **Step 2: Run the tests and confirm RED**

Run:

```powershell
cargo test -p pixivbot --bin pixivbot chat_locks_serialize_same_chat_but_not_different_chats -- --nocapture
cargo test -p pixivbot --bin pixivbot cancel_one_shared_delivery_keeps_sibling_job_and_artifact_live -- --nocapture
```

Expected: compilation FAILS because only the process-wide `EH_PUBLISH_CANCEL_LOCK` exists and cancellation still owns shared state on a queue row.

- [ ] **Step 3: Implement the keyed registry and remove the global lock**

Use the planned `EhChatLockRegistry`/`EH_CHAT_LOCKS` interfaces. The map value is `Weak<Mutex<()>>`, so finished chats do not create an unbounded permanent lock map. `lock_chats` sorts/deduplicates IDs and acquires in ascending order, preventing cancellation deadlock.

Remove `EH_PUBLISH_CANCEL_LOCK`. Download and upload workers acquire no chat lock. Task 8 will acquire one target-chat lock only around publish checks, sends, and markers.

- [ ] **Step 4: Port cancellation to delivery state and recompute the job aggregate**

`delete_eh_subscription_and_cancel_queue` first loads the subscription chat ID, acquires that chat's lock, then deletes the subscription and updates its deliveries. The standalone `cancel_eh_subscription_queue_entries` queries affected chat IDs, acquires sorted guards, re-queries under the guards, and applies CAS owner removal.

Use these signatures so every call site supplies the already-configured archive policy rather than introducing a global config lookup:

```rust
pub async fn cancel_eh_subscription_queue_entries(
    &self, subscription_id: i32, send_archive: bool,
) -> Result<u64>;
pub async fn delete_eh_subscription_and_cancel_queue(
    &self, subscription_id: i32, send_archive: bool,
) -> Result<()>;
```

`EhEngine` and its post-enqueue race repair pass `self.config.send_archive`; `BotHandler::delete_subscription` passes `self.eh_config.send_archive`.

For each changed delivery:

- remove the owner from `subscription_ids` and `telegraph_subscription_ids`;
- keep `waiting/publishing` when another owner remains;
- set `canceled` only when no owner remains;
- never clear sibling delivery markers;
- recompute the bound job's Telegraph aggregate;
- set `pending -> not_required` only before an upload claim;
- keep `uploading` intact after claim;
- invoke `Repo::evaluate_eh_job_liveness(job_id, send_archive)` after cancellation/rebind. It marks cleanup `pending` when an artifact family is no longer needed, but marks the job `retired` only when no active delivery remains **and** rewrite is neither `pending` nor `rewriting`.

Expose:

```rust
pub async fn evaluate_eh_job_liveness(
    &self, job_id: i32, send_archive: bool,
) -> Result<EhJobCleanupDecision>;
```

This method is database-only and generation-safe: it records cleanup `pending` without deleting files, returns `preserve_rewrite_payload=true` while rewrite is pending/running, and never clears a path, manifest, URL, or rewrite payload. With active consumers it retains the job; without consumers it leaves rewrite-active jobs non-retired, otherwise retires them. Task 9 claims and executes persisted cleanup safely.

- [ ] **Step 5: Run cancellation and lock tests GREEN**

Run:

```powershell
cargo test -p pixivbot --bin pixivbot chat_locks_serialize_same_chat_but_not_different_chats -- --nocapture
cargo test -p pixivbot --bin pixivbot cancel_one_shared_delivery_keeps_sibling_job_and_artifact_live -- --nocapture
cargo test -p pixivbot --bin pixivbot cancellation_before_and_after_upload_claim_obeys_aggregate_boundary -- --nocapture
cargo test -p pixivbot --bin pixivbot cancel_subscription_queue_entries -- --nocapture
```

Expected: all commands PASS; same-chat ordering is serialized, different chats remain independent, and one cancellation never retires or deletes a sibling's shared job.

### Task 8: Deliver to chats with bounded concurrency, independent retry, and shared missing-ZIP reset

**Files:**
- Modify: `src/db/repo/eh_download_queue.rs:2163-2406,2637-2947` and delivery tests
- Modify: `src/db/repo/eh_gallery_jobs.rs`
- Modify: `src/scheduler/eh_engine.rs:1844-2175` and publish tests
- Modify: `src/main.rs:424-443`

**Interfaces:**
- Consumes: ready job state from Tasks 3/5, job rewrite scheduling from Task 6, keyed locks from Task 7, `EhentaiConfig::publish_concurrency_clamped`, existing `Notifier`/`Throttle<Bot>`.
- Produces: joined `EhDeliveryClaim`, atomic `waiting -> publishing` claims, bounded task drain/refill, delivery-only retries, and one CAS reset of a missing shared ZIP generation.

- [ ] **Step 1: Add failing bounded-concurrency and retry-isolation tests**

```rust
#[tokio::test]
async fn publish_worker_claims_at_most_two_deliveries_and_refills() {
    let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
    seed_downloaded_job_with_deliveries(&repo,
        &[(-100, false), (-200, false), (-300, false)]).await;
    mount_delayed_send_document(&tg_server, Duration::from_millis(200)).await;
    let mut config = make_config();
    config.publish_concurrency = 2;
    let worker = make_publish_worker(repo.clone(), &tg_server, config);

    let running = tokio::spawn(async move { worker.tick().await });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(count_deliveries_in_status(&repo, DELIVERY_STATUS_PUBLISHING).await, 2);
    assert_eq!(count_deliveries_in_status(&repo, DELIVERY_STATUS_WAITING).await, 1);
    running.await.unwrap().unwrap();
    assert_eq!(count_deliveries_in_status(&repo, DELIVERY_STATUS_DONE).await, 3);
}

#[tokio::test]
async fn telegram_failure_retries_only_one_delivery_without_repeating_shared_work() {
    let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
    let job = seed_downloaded_job_with_deliveries(&repo,
        &[(-100, false), (-200, false)]).await;
    mount_send_document_failure_for_chat(&tg_server, -100).await;
    mount_send_document_success_for_chat(&tg_server, -200).await;
    make_publish_worker(repo.clone(), &tg_server, make_config()).tick().await.unwrap();

    assert_eq!(delivery_status_for_chat(&repo, job.id, -100).await, DELIVERY_STATUS_WAITING);
    assert_eq!(delivery_retry_count_for_chat(&repo, job.id, -100).await, 1);
    assert_eq!(delivery_status_for_chat(&repo, job.id, -200).await, DELIVERY_STATUS_DONE);
    assert_eq!(load_job(&repo, job.id).await.status, JOB_STATUS_DOWNLOADED);
    assert_eq!(source_download_count(&eh_server).await, 0);
}

#[tokio::test]
async fn archive_only_delivery_bypasses_upload_wait_and_disabled_chat_does_not_block_sibling() {
    let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
    setup_chat(&repo, -100, false).await;
    setup_chat(&repo, -200, true).await;
    let job = seed_uploading_job_with_archive_only_deliveries(&repo, &[-100, -200]).await;
    make_publish_worker(repo.clone(), &tg_server, make_config()).tick().await.unwrap();
    assert_eq!(delivery_status_for_chat(&repo, job.id, -100).await,
        DELIVERY_STATUS_WAITING);
    assert!(delivery_next_retry_for_chat(&repo, job.id, -100).await.is_some());
    assert_eq!(delivery_status_for_chat(&repo, job.id, -200).await,
        DELIVERY_STATUS_DONE);
    assert_eq!(load_job(&repo, job.id).await.telegraph_status,
        TELEGRAPH_STATUS_UPLOADING);
}
```

- [ ] **Step 2: Add a failing missing-ZIP shared-generation test**

```rust
#[tokio::test]
async fn missing_ready_zip_resets_one_job_generation_for_all_archive_consumers() {
    let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
    let job = seed_downloaded_job_with_missing_zip_and_two_deliveries(&repo).await;
    let first = repo.get_next_eh_delivery_for_publish(true).await.unwrap().unwrap();
    handle_missing_eh_job_zip(&repo, &first).await.unwrap();
    let second = repo.get_next_eh_delivery_for_publish(true).await.unwrap();
    assert!(second.is_none(), "job reset must make sibling delivery temporarily ineligible");

    let reset = load_job(&repo, job.id).await;
    assert_eq!(reset.status, JOB_STATUS_PENDING);
    assert!(reset.zip_path.is_none());
    assert_eq!(count_deliveries_in_status_for_job(&repo, job.id, DELIVERY_STATUS_WAITING).await, 2);
    assert_eq!(reset.retry_count, 1);
}
```

- [ ] **Step 3: Run delivery tests and confirm RED**

Run:

```powershell
cargo test -p pixivbot --bin pixivbot publish_worker_claims_at_most_two_deliveries_and_refills -- --nocapture
cargo test -p pixivbot --bin pixivbot missing_ready_zip_resets_one_job_generation_for_all_archive_consumers -- --nocapture
```

Expected: compilation FAILS because publish claims are not joined/bounded and missing ZIP still resets one queue row.

- [ ] **Step 4: Implement joined delivery eligibility and atomic claims**

Add:

```rust
pub async fn get_next_eh_delivery_for_publish(
    &self, send_archive: bool,
) -> Result<Option<EhDeliveryClaim>>;
pub async fn eh_delivery_is_active(
    &self, delivery_id: i32, expected_status: &str,
) -> Result<bool>;
pub async fn defer_eh_delivery_publish(
    &self, delivery_id: i32, delay_secs: i64,
) -> Result<()>;
pub async fn schedule_eh_delivery_retry(
    &self, delivery_id: i32, error: &str, max_retry_count: u8,
) -> Result<(eh_download_queue::Model, bool)>;
pub async fn mark_eh_archive_delivery_sent(&self, delivery_id: i32) -> Result<()>;
pub async fn mark_eh_delivery_done(&self, delivery_id: i32) -> Result<eh_download_queue::Model>;
pub async fn reset_eh_job_for_missing_zip(
    &self, job_id: i32, expected_zip_path: &str,
) -> Result<bool>;
```

Claim only `waiting` deliveries with due retry. A delivery is ready when:

- archive is enabled, its archive marker is null, the job is `downloaded`, and `zip_path` is non-null; or
- Telegraph is requested, its marker is null, the job Telegraph status is `ready`, and URL is non-null; or
- all requested/enabled surfaces already have markers, allowing marker-safe completion.

For a Telegraph delivery, do not claim before Telegraph is ready; an archive-only delivery may claim while job upload is `pending`, `uploading`, or `failed`. Historical terminal rows with `job_id=NULL` are never claimed.

- [ ] **Step 5: Implement bounded publish refill and keyed critical sections**

`EhPublishWorker::tick` creates a `JoinSet`, fills up to `publish_concurrency_clamped()`, waits for one completion, records/logs that delivery's result, and refills until no claim remains and the set drains. One task failure never cancels siblings.

Each claimed future:

1. acquires `EH_CHAT_LOCKS.lock_chat(delivery.chat_id)`;
2. re-reads chat eligibility and delivery/job state under the guard;
3. defers disabled chats without retry increment;
4. sends archive and/or Telegraph through the existing cloned `Notifier`;
5. persists each delivery marker immediately after its successful send;
6. schedules job rewrite through Task 6 after the first Telegraph marker;
7. marks only this delivery done;
8. calls liveness evaluation; Task 9 performs any permitted cleanup.

No lock is held for download/upload. No raw send error is included in the final user-facing failure notification.

- [ ] **Step 6: Make a missing ZIP reset the job once**

When a claimed archive surface finds the persisted ZIP absent, first return the delivery to `waiting`, then CAS the job from the expected downloaded generation/path to `pending`, increment shared retry once, clear `zip_path/file_size/gp_cost/completed_at`, and leave already-ready Telegraph URL/rewrite state intact. Reset all other `publishing` archive consumers for that job to `waiting`; sent markers remain untouched. A concurrent second reset returns `false` and does not increment again.

- [ ] **Step 7: Run publish/concurrency/retry tests and confirm GREEN**

Run:

```powershell
cargo test -p pixivbot --bin pixivbot publish_worker_claims_at_most_two_deliveries_and_refills -- --nocapture
cargo test -p pixivbot --bin pixivbot telegram_failure_retries_only_one_delivery_without_repeating_shared_work -- --nocapture
cargo test -p pixivbot --bin pixivbot archive_only_delivery_bypasses_upload_wait_and_disabled_chat_does_not_block_sibling -- --nocapture
cargo test -p pixivbot --bin pixivbot missing_ready_zip_resets_one_job_generation_for_all_archive_consumers -- --nocapture
cargo test -p pixivbot --bin pixivbot publish_ -- --nocapture
```

Expected: all commands PASS; the in-flight DB count never exceeds two, different chats complete independently, markers prevent resend, and missing ZIP causes one shared redownload generation.

### Task 9: Retire and clean shared jobs safely, then recover every stale claim

**Files:**
- Modify: `src/db/repo/eh_gallery_jobs.rs`
- Modify: `src/db/repo/eh_download_queue.rs:3410-3496` and cleanup/recovery tests
- Modify: `src/scheduler/eh_engine.rs:41-150,1141-1150,1496-1501,1928-1961,2129-2138`
- Modify: `src/main.rs:279-350`

**Interfaces:**
- Consumes: persisted cleanup decisions from Task 7; rewrite terminal/liveness callbacks from Task 6; job artifact/abort helpers from Tasks 3/5; all job/delivery claim states.
- Produces: generation-guarded cleanup claim/failure/finalization APIs, dirty-job reactivation only after successful cleanup, dirty-path orphan retention, and stale resets for download/upload/publish/background/rewrite/cleanup.

- [ ] **Step 1: Add failing retention/final-consumer/fail-closed cleanup tests**

```rust
#[tokio::test]
async fn shared_zip_survives_first_delivery_and_is_removed_after_final_consumer() {
    let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
    let (job, first, second, artifacts) = seed_downloaded_job_and_artifacts(&repo).await;
    complete_delivery(&repo, first.id).await;
    let keep = repo.evaluate_eh_job_liveness(job.id, true).await.unwrap();
    assert!(!keep.retire);
    assert!(!keep.remove_archive_family);
    assert!(artifacts.final_zip().exists());

    cancel_delivery(&repo, second.id).await;
    let retire = repo.evaluate_eh_job_liveness(job.id, true).await.unwrap();
    assert!(retire.retire);
    let scheduled = load_job(&repo, job.id).await;
    assert_eq!(scheduled.cleanup_status, CLEANUP_STATUS_PENDING);
    assert!(artifacts.final_zip().exists(), "liveness only schedules durable cleanup");
    let outcome = run_eh_job_cleanup_maintenance_once(
        &repo, Some(&abort_uploader), 60).await.unwrap().unwrap();
    assert_eq!(outcome, EhCleanupFinalizeOutcome::CleanRetired);
    assert!(!artifacts.final_zip().exists());
    let retired = load_job(&repo, job.id).await;
    assert_eq!(retired.status, JOB_STATUS_RETIRED);
    assert_eq!(retired.cleanup_status, CLEANUP_STATUS_NONE);
    assert!(retired.zip_path.is_none());
}

#[tokio::test]
async fn abort_failure_then_enqueue_blocks_download_until_cleanup_succeeds() {
    let repo = tests_helpers::setup_test_db().await.unwrap();
    let (job, artifacts, variant) =
        seed_retired_job_with_multipart_state_and_one_completion(&repo).await;
    let failing = RecordingAbortUploader::failing();
    assert!(run_eh_job_cleanup_maintenance_once(&repo, Some(&failing), 60).await.is_err());
    assert!(artifacts.final_zip().exists());
    assert!(artifacts.uploads_dir().exists());
    let failed = load_job(&repo, job.id).await;
    assert_eq!(failed.cleanup_status, CLEANUP_STATUS_FAILED);
    assert!(failed.cleanup_error.is_some());
    assert!(failed.cleanup_next_retry_at.is_some());
    assert!(failed.zip_path.is_some());

    let rebound = repo.enqueue_eh_download(
        -100, job.gid, &job.token, "new wave", false, SOURCE_DIRECT, &variant).await.unwrap();
    assert_eq!(rebound.job_id, Some(job.id));
    let still_dirty = load_job(&repo, job.id).await;
    assert_eq!(still_dirty.status, JOB_STATUS_RETIRED);
    assert_eq!(still_dirty.cleanup_status, CLEANUP_STATUS_FAILED);
    assert!(repo.get_next_eh_job_for_download().await.unwrap().is_none(),
        "dirty retired artifact family must not be overwritten");

    make_job_cleanup_due(&repo, job.id).await;
    let succeeding = RecordingAbortUploader::succeeding();
    let outcome = run_eh_job_cleanup_maintenance_once(
        &repo, Some(&succeeding), 60).await.unwrap().unwrap();
    assert_eq!(outcome, EhCleanupFinalizeOutcome::ReactivatedPending);
    assert!(!artifacts.uploads_dir().exists());
    let clean = load_job(&repo, job.id).await;
    assert_eq!(clean.status, JOB_STATUS_PENDING);
    assert_eq!(clean.cleanup_status, CLEANUP_STATUS_NONE);
    assert!(clean.zip_path.is_none());

    mount_archive_success_once(&eh_server, job.gid, &job.token).await;
    make_download_worker(repo.clone(), &eh_server, cache_dir(), 500).tick().await.unwrap();
    assert_eq!(archiver_post_count(&eh_server).await, 1);
    assert_eq!(load_download_completions_for_job(&repo, job.id).await.len(), 2,
        "the seeded old generation and post-cleanup generation both remain accounted");
}
```

- [ ] **Step 2: Add failing crash-recovery and orphan keep-set tests**

Seed one job in each transient state (`downloading`, Telegraph `uploading`, background `running`, rewrite `rewriting`, cleanup `running`) and one delivery in `publishing`, with send markers. Assert one recovery call yields job `pending`, Telegraph `pending`, background `pending`, rewrite `pending`, cleanup `pending`, delivery `waiting`, and unchanged markers/paths. Seed two active variant artifacts and one dirty retired artifact for the same `gid/token`; assert startup orphan cleanup keeps all three referenced paths, aborts only an unreferenced multipart family before deletion, and preserves that family if abort fails.

```rust
#[tokio::test]
async fn shared_job_crash_recovery_resets_each_claim_once() {
    let repo = tests_helpers::setup_test_db().await.unwrap();
    let seeded = seed_every_shared_claim_and_one_publishing_delivery(&repo).await;
    let counts = repo.reset_stale_eh_shared_work(1, 1).await.unwrap();
    assert_eq!(counts.downloads, 1);
    assert_eq!(counts.uploads, 1);
    assert_eq!(counts.background_downloads, 1);
    assert_eq!(counts.rewrites, 1);
    assert_eq!(counts.cleanups, 1);
    assert_eq!(counts.deliveries, 1);
    assert_recovered_states_and_preserved_markers(&repo, &seeded).await;
    assert_eq!(repo.reset_stale_eh_shared_work(1, 1).await.unwrap(),
        EhStaleResetCounts::default());
}

#[tokio::test]
async fn orphan_cleanup_uses_active_job_paths_and_never_crosses_variants() {
    let repo = tests_helpers::setup_test_db().await.unwrap();
    let (active_980, active_original, dirty_retired, orphan) =
        seed_two_active_variants_dirty_retired_and_one_orphan(&repo).await;
    let failing_abort = RecordingAbortUploader::failing();
    repo.cleanup_eh_cache_orphans(cache_dir(), Some(&failing_abort)).await.unwrap();
    assert!(active_980.final_zip().exists());
    assert!(active_original.final_zip().exists());
    assert!(dirty_retired.uploads_dir().exists(),
        "persisted failed cleanup owns its family until maintenance claims it");
    assert!(orphan.uploads_dir().exists(), "abort failure preserves orphan family");

    let succeeding_abort = RecordingAbortUploader::succeeding();
    repo.cleanup_eh_cache_orphans(cache_dir(), Some(&succeeding_abort)).await.unwrap();
    assert!(active_980.final_zip().exists());
    assert!(active_original.final_zip().exists());
    assert!(!orphan.final_zip().exists());
}
```

- [ ] **Step 3: Run cleanup/recovery tests and confirm RED**

Run:

```powershell
cargo test -p pixivbot --bin pixivbot shared_zip_survives_first_delivery_and_is_removed_after_final_consumer -- --nocapture
cargo test -p pixivbot --bin pixivbot abort_failure_then_enqueue_blocks_download_until_cleanup_succeeds -- --nocapture
cargo test -p pixivbot --bin pixivbot shared_job_crash_recovery_resets_each_claim_once -- --nocapture
```

Expected: compilation FAILS because cleanup and stale recovery still derive ownership from queue rows.

- [ ] **Step 4: Implement durable cleanup execution and finalization**

Add:

```rust
pub async fn get_next_eh_job_for_cleanup(&self) -> Result<Option<eh_gallery_jobs::Model>>;
pub async fn record_eh_job_cleanup_failure(
    &self, job_id: i32, expected_cleanup_started_at: DateTime,
    error: &str, retry_delay_secs: i64,
) -> Result<bool>;
pub async fn finalize_eh_job_cleanup(
    &self, job_id: i32, expected_cleanup_started_at: DateTime,
) -> Result<Option<EhCleanupFinalizeOutcome>>;
pub async fn cleanup_eh_cache_orphans(
    &self, cache_dir: &Path, abort_uploader: Option<&dyn ImageUploader>,
) -> Result<()>;
async fn execute_eh_job_cleanup(
    repo: &Repo, claimed_job: &eh_gallery_jobs::Model,
    abort_uploader: Option<&dyn ImageUploader>, retry_delay_secs: i64,
) -> Result<EhCleanupFinalizeOutcome>;
async fn run_eh_job_cleanup_maintenance_once(
    repo: &Repo, abort_uploader: Option<&dyn ImageUploader>,
    retry_delay_secs: i64,
) -> Result<Option<EhCleanupFinalizeOutcome>>;
```

Liveness rules are exact:

- any active delivery keeps the job row, but does not make a dirty retired generation claimable;
- any active delivery with archive enabled and `archive_sent_at=NULL` keeps the ZIP;
- Telegraph `pending/uploading` keeps the ZIP even when all archive markers are complete;
- no active delivery retires the job only when rewrite is already terminal; rewrite `pending/rewriting` keeps it non-retired and keeps payload/page state;
- archive removal requires no remaining archive need and no upload stage that needs the ZIP;
- liveness only sets `cleanup_status='pending'`; maintenance atomically claims due `pending/failed -> running`, writes a fresh `cleanup_started_at`, and reads back the winning generation;
- abort/no-abort checks occur before any local removal. Failure CASes that same generation to `failed`, stores the internal error/retry time, and preserves ZIP, manifests, paths, and rewrite payload;
- only after successful abort/local removal does `finalize_eh_job_cleanup` CAS the same `running` generation. With an active consumer it clears the old shared generation and returns `ReactivatedPending`; with no consumer and terminal rewrite it returns `CleanRetired`; archive-only cleanup while rewrite is pending returns `RetainedForRewrite`, clears only archive fields, and preserves rewrite payload/page state;
- successful active reactivation sets job `pending`, cleanup `none`, clears prior download/Telegraph result, retry/background/rewrite fields, and lets the existing `telegraph_required` aggregate drive Telegraph `pending` after the new download. Successful clean retirement leaves status `retired`, cleanup `none`, and no transient paths.

`get_next_eh_job_for_cleanup` selects `pending` immediately and `failed` only when `cleanup_next_retry_at IS NULL OR <= now`, orders by due time then job ID, CASes one snapshot to `running`, and returns only the read-back matching its new `cleanup_started_at`. `record_eh_job_cleanup_failure` and `finalize_eh_job_cleanup` return `false`/`None` on stale generations so a superseded maintenance attempt cannot clear persisted ownership fields or activate a waiting consumer.

Cancellation/completion/rebind only record durable liveness. Run one cleanup maintenance claim before each normal download-worker source claim and drain due cleanup at startup with the configured abort uploader. Therefore a runtime enqueue behind failed cleanup cannot start a download until a later maintenance success, without adding a new worker or unrelated-gallery concurrency.

- [ ] **Step 5: Port stale recovery and startup orphan cleanup to jobs**

Expose one startup entry point:

```rust
pub async fn reset_stale_eh_shared_work(
    &self, background_stale_sec: u64, rewrite_stale_sec: i64,
) -> Result<EhStaleResetCounts>;
pub async fn disable_eh_telegraph_for_unuploaded_jobs(&self) -> Result<u64>;
```

It performs generation-safe resets: job `downloading -> pending`, Telegraph `uploading -> pending`, background `running -> pending`, rewrite `rewriting -> pending`, cleanup `running -> pending`, and delivery `publishing -> waiting`; markers, cleanup paths, and retry payloads survive. `disable_eh_telegraph_for_unuploaded_jobs` changes pending/unclaimed Telegraph work to `not_required`, recomputes delivery intent, and never erases a ready URL or nonterminal rewrite. `cleanup_eh_cache_orphans` builds its keep-set from every persisted job path that is active **or** has cleanup `pending/running/failed`, plus deterministic paths for pending/downloading jobs, never from delivery compatibility columns. Only disk families unowned by any job use abort-first orphan cleanup.

Update `main.rs` startup in this order: reset stale shared claims; run orphan cleanup using the persisted keep-set; drain currently due cleanup claims with the existing S3/ipfS3 abort uploader until `get_next_eh_job_for_cleanup` returns `None`; release background jobs when disabled; disable only pending Telegraph requirements when no client exists; then spawn workers. A missing/failing abort uploader leaves the job `failed` with a retry instead of deleting local state. Remove the old legacy-owner cancellation call; the migration now preserves/binds active ownership safely.

- [ ] **Step 6: Run cleanup/recovery tests and confirm GREEN**

Run:

```powershell
cargo test -p pixivbot --bin pixivbot shared_zip_survives_first_delivery_and_is_removed_after_final_consumer -- --nocapture
cargo test -p pixivbot --bin pixivbot abort_failure_then_enqueue_blocks_download_until_cleanup_succeeds -- --nocapture
cargo test -p pixivbot --bin pixivbot shared_job_crash_recovery_resets_each_claim_once -- --nocapture
cargo test -p pixivbot --bin pixivbot cleanup_eh_cache_orphans -- --nocapture
```

Expected: all commands PASS; first consumer cannot delete shared state, abort failure plus immediate enqueue cannot trigger download, successful maintenance reactivates exactly one pending generation/download, stale cleanup resumes once, and variant artifacts never cross-delete.

### Task 10: Derive `/estatus` from joined state and lock adjacent regressions

**Files:**
- Modify: `src/db/repo/eh_download_queue.rs:90-113,884-921` and status tests
- Modify: `src/db/repo/eh_integration_tests.rs`
- Modify: `src/bot/handlers/subscription/ehentai.rs:283-308,747-997`
- Modify: `src/main.rs` only for final constructor/startup signature alignment
- Test: `src/scheduler/eh_engine.rs` shared end-to-end scenarios

**Interfaces:**
- Consumes: joined delivery/job states from all prior tasks; existing `/estatus` MarkdownV2 formatting and command text.
- Produces: safe derived `EhQueueStatusItem`; historical terminal compatibility; full happy-path, variant, cancellation, retry, GP, background, rewrite, cleanup, and direct-upgrade regression proof.

- [ ] **Step 1: Add a failing joined status and secrecy test**

Change status items to carry only the already-public derived fields:

```rust
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EhQueueStatusItem {
    pub gid: i64,
    pub title: String,
    pub status: String, // derived friendly-stage key, not raw shared internals
    pub background_download_status: Option<String>,
}
```

```rust
#[tokio::test]
async fn estatus_joins_active_job_state_and_preserves_unbound_terminal_history() {
    let repo = tests_helpers::setup_test_db().await.unwrap();
    seed_status_delivery(&repo, -100, DELIVERY_STATUS_WAITING,
        JOB_STATUS_DOWNLOADING, TELEGRAPH_STATUS_NOT_REQUIRED, None).await;
    seed_status_delivery(&repo, -100, DELIVERY_STATUS_WAITING,
        JOB_STATUS_DOWNLOADED, TELEGRAPH_STATUS_UPLOADING, Some(BACKGROUND_STATUS_RUNNING)).await;
    seed_status_delivery(&repo, -100, DELIVERY_STATUS_PUBLISHING,
        JOB_STATUS_DOWNLOADED, TELEGRAPH_STATUS_READY, None).await;
    seed_dirty_cleanup_status_delivery(&repo, -100, JOB_STATUS_RETIRED,
        CLEANUP_STATUS_FAILED, "provider abort id=secret").await;
    seed_unbound_terminal_delivery(&repo, -100, DELIVERY_STATUS_FAILED,
        "db password and /secret/path").await;

    let snapshot = repo.get_eh_queue_snapshot(-100).await.unwrap();
    assert_eq!(snapshot.active.iter().map(|x| x.status.as_str()).collect::<Vec<_>>(),
        vec!["downloading", "uploading", "publishing", "pending"]);
    assert_eq!(snapshot.recent_terminal.unwrap().status, "failed");
    assert!(!format!("{snapshot:?}").contains("password"));
    assert!(!format!("{snapshot:?}").contains("/secret/path"));
    assert!(!format!("{snapshot:?}").contains("provider abort"));
}
```

- [ ] **Step 2: Run the status test and confirm RED**

Run:

```powershell
cargo test -p pixivbot --bin pixivbot estatus_joins_active_job_state_and_preserves_unbound_terminal_history -- --nocapture
```

Expected: assertion FAILS because active stage data is still read from queue compatibility columns.

- [ ] **Step 3: Implement stage derivation without changing user copy**

`get_eh_queue_snapshot(chat_id)` left-joins jobs. Derive precedence as follows:

1. delivery `publishing` → `publishing`;
2. waiting + cleanup `pending/running/failed` on a bound retired job → existing friendly `pending` label, never the cleanup error;
3. waiting + background `running/pending` → current background labels;
4. waiting + job `pending/downloading` → `pending/downloading`;
5. waiting + Telegraph requested and job Telegraph `pending/uploading` → `downloaded/uploading` using existing friendly labels;
6. waiting + required surfaces ready → `uploaded` when Telegraph is ready, otherwise `downloaded`;
7. delivery terminal → its own terminal status, including `job_id=NULL` history.

Keep `eh_queue_stage`, exact Chinese text, MarkdownV2 escaping, 20-item truncation, and UTF-16 message limit unchanged. Never place job IDs, paths, errors, other chat IDs, or raw shared statuses into the returned item.

- [ ] **Step 4: Add full cross-surface regression scenarios**

Add focused tests rather than one opaque mega-test:

- `shared_happy_path_downloads_uploads_and_creates_page_once_for_two_chats`: two concurrent canonical enqueues, one archive POST, one uploader call, one Telegraph create, two chat deliveries.
- `same_gid_different_resolution_never_shares_artifact_or_cleanup`: two jobs/paths; retiring one leaves the other path and job unchanged.
- `direct_upgrade_over_subscription_rebinds_before_send_and_preserves_subscription_progress`: direct wins, old consumerless job retires, pushed GID state remains committed.
- `eunsub_cancels_one_delivery_while_shared_sibling_completes`: same-chat lock ordering and sibling completion.
- `delivery_retry_does_not_repeat_download_upload_or_page_creation`: one Telegram failure, one sibling success, shared request counts stay one.
- `gp_budget_counts_shared_job_once_with_two_deliveries`: one positive GP ledger row, one completion row, and one rolling contribution per actual shared attempt/generation.
- `background_shared_job_and_rewrite_run_once_after_recovery`: stale background/rewrite claims reset and each side effect resumes once.
- `download_terminal_failure_is_status_only_and_sends_no_telegram_message`: terminal source error fails active deliveries, appears through joined status, and records zero Telegram requests.
- `terminal_upload_notification_targets_only_telegraph_deliveries_with_fixed_copy`: inject database/provider text, require two Telegraph chats plus one archive-only chat, and assert exact nonempty recipients/body/count.
- Re-run `completion_ledger_counts_both_generations_after_clean_retired_job_reactivation`, `abort_failure_then_enqueue_blocks_download_until_cleanup_succeeds`, and `final_delivery_with_delayed_rewrite_keeps_payload_until_rewrite_is_terminal` as cross-task regression gates.

Each test uses existing wiremock request counts, `ArchiveArtifacts` temporary directories, and the real SQLite helper; no ignored local files.

Use these concrete assertion skeletons so each scenario proves side-effect counts rather than only final statuses:

```rust
struct SharedEhFixture {
    repo: Arc<Repo>,
    eh_server: MockServer,
    telegraph_server: MockServer,
    telegram_server: MockServer,
    uploader: Arc<ZipFirstMockUploader>,
    temp_dir: tempfile::TempDir,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RecordedTelegramMessage {
    chat_id: i64,
    text: String,
}

trait SharedEhFixtureApi {
    async fn two_telegraph_chats_same_variant() -> Self;
    async fn two_variants(first: &str, second: &str) -> Self;
    async fn one_send_failure_one_success() -> Self;
    async fn with_terminal_download_error(message: &str) -> Self;
    async fn with_terminal_upload_error_and_archive_sibling(message: &str) -> Self;
    async fn run_download_upload_publish(&self);
    async fn run_downloads(&self);
    async fn run_first_attempt(&self);
    async fn run_due_delivery_retry(&self);
    async fn run_to_terminal_download_failure(&self);
    async fn run_to_terminal_upload_failure(&self);
    async fn retire_and_cleanup(&self, job_id: i32);
    async fn jobs(&self) -> (eh_gallery_jobs::Model, eh_gallery_jobs::Model);
    async fn delivery_statuses(&self) -> Vec<String>;
    async fn telegram_messages(&self) -> Vec<RecordedTelegramMessage>;
    async fn archive_posts(&self) -> usize;
    async fn telegraph_create_calls(&self) -> usize;
    fn zip_upload_calls(&self) -> usize;
}

// Implement SharedEhFixtureApi for SharedEhFixture with the existing wiremock,
// temporary ArchiveArtifacts, real-SQLite, and ZipFirstMockUploader helpers.

#[tokio::test]
async fn shared_happy_path_downloads_uploads_and_creates_page_once_for_two_chats() {
    let fixture = SharedEhFixture::two_telegraph_chats_same_variant().await;
    fixture.run_download_upload_publish().await;
    assert_eq!(fixture.archive_posts().await, 1);
    assert_eq!(fixture.zip_upload_calls(), 1);
    assert_eq!(fixture.telegraph_create_calls().await, 1);
    assert_eq!(fixture.delivery_statuses().await,
        vec![DELIVERY_STATUS_DONE, DELIVERY_STATUS_DONE]);
}

#[tokio::test]
async fn same_gid_different_resolution_never_shares_artifact_or_cleanup() {
    let fixture = SharedEhFixture::two_variants("980x", "original").await;
    fixture.run_downloads().await;
    let (first, second) = fixture.jobs().await;
    assert_ne!(first.id, second.id);
    assert_ne!(first.zip_path, second.zip_path);
    fixture.retire_and_cleanup(first.id).await;
    assert!(!path_exists(first.zip_path.as_deref().unwrap()));
    assert!(path_exists(second.zip_path.as_deref().unwrap()));
}

#[tokio::test]
async fn delivery_retry_does_not_repeat_download_upload_or_page_creation() {
    let fixture = SharedEhFixture::one_send_failure_one_success().await;
    fixture.run_first_attempt().await;
    assert_eq!(fixture.archive_posts().await, 1);
    assert_eq!(fixture.zip_upload_calls(), 1);
    assert_eq!(fixture.telegraph_create_calls().await, 1);
    fixture.run_due_delivery_retry().await;
    assert_eq!(fixture.archive_posts().await, 1);
    assert_eq!(fixture.zip_upload_calls(), 1);
    assert_eq!(fixture.telegraph_create_calls().await, 1);
}

#[tokio::test]
async fn download_terminal_failure_is_status_only_and_sends_no_telegram_message() {
    let fixture = SharedEhFixture::with_terminal_download_error(
        "sqlite secret; /private/path").await;
    fixture.run_to_terminal_download_failure().await;
    assert_eq!(fixture.delivery_statuses().await,
        vec![DELIVERY_STATUS_FAILED, DELIVERY_STATUS_FAILED]);
    assert!(fixture.telegram_messages().await.is_empty(),
        "download terminal behavior remains log plus /estatus only");
}

#[tokio::test]
async fn terminal_upload_notification_targets_only_telegraph_deliveries_with_fixed_copy() {
    let fixture = SharedEhFixture::with_terminal_upload_error_and_archive_sibling(
        "sqlite secret; /private/path; multipart upload id=abc").await;
    fixture.run_to_terminal_upload_failure().await;
    let messages = fixture.telegram_messages().await;
    assert_eq!(messages.len(), 2, "assert nonempty before secrecy checks");
    assert_eq!(messages.iter().map(|m| m.chat_id).collect::<Vec<_>>(), vec![-100, -200]);
    assert_eq!(messages.iter().map(|m| m.text.as_str()).collect::<Vec<_>>(), vec![
        "⚠️ Telegraph 上传失败，请稍后重试\n\n📦 T1",
        "⚠️ Telegraph 上传失败，请稍后重试\n\n📦 T2",
    ]);
    assert_eq!(fixture.delivery_statuses().await, vec![
        DELIVERY_STATUS_FAILED,
        DELIVERY_STATUS_FAILED,
        DELIVERY_STATUS_WAITING,
    ]);
    assert!(messages.iter().all(|m| !m.text.contains("sqlite secret")));
    assert!(messages.iter().all(|m| !m.text.contains("/private/path")));
    assert!(messages.iter().all(|m| !m.text.contains("upload id")));
}
```

`RecordedTelegramMessage` contains only parsed request `chat_id` and `text`; the fixture sorts messages by chat ID before returning them. The direct-upgrade, `/eunsub`, GP, and background/rewrite tests use the same fixture style and must assert respectively: old/new job IDs and pushed GIDs; canceled/sibling delivery statuses plus one shared side-effect count; exactly one GP row and one completion row with `job_id`; and exactly one resumed source/rewrite request after stale reset.

- [ ] **Step 5: Run status and adjacent regression tests GREEN**

Run:

```powershell
cargo test -p pixivbot --bin pixivbot estatus_joins_active_job_state_and_preserves_unbound_terminal_history -- --nocapture
cargo test -p pixivbot --bin pixivbot shared_happy_path_downloads_uploads_and_creates_page_once_for_two_chats -- --nocapture
cargo test -p pixivbot --bin pixivbot same_gid_different_resolution_never_shares_artifact_or_cleanup -- --nocapture
cargo test -p pixivbot --bin pixivbot eunsub_cancels_one_delivery_while_shared_sibling_completes -- --nocapture
cargo test -p pixivbot --bin pixivbot delivery_retry_does_not_repeat_download_upload_or_page_creation -- --nocapture
cargo test -p pixivbot --bin pixivbot gp_budget_counts_shared_job_once_with_two_deliveries -- --nocapture
cargo test -p pixivbot --bin pixivbot background_shared_job_and_rewrite_run_once_after_recovery -- --nocapture
cargo test -p pixivbot --bin pixivbot download_terminal_failure_is_status_only_and_sends_no_telegram_message -- --nocapture
cargo test -p pixivbot --bin pixivbot terminal_upload_notification_targets_only_telegraph_deliveries_with_fixed_copy -- --nocapture
cargo test -p pixivbot --bin pixivbot completion_ledger_counts_both_generations_after_clean_retired_job_reactivation -- --nocapture
cargo test -p pixivbot --bin pixivbot abort_failure_then_enqueue_blocks_download_until_cleanup_succeeds -- --nocapture
cargo test -p pixivbot --bin pixivbot final_delivery_with_delayed_rewrite_keeps_payload_until_rewrite_is_terminal -- --nocapture
cargo test -p pixivbot --bin pixivbot eh_queue_status -- --nocapture
```

Expected: every command PASS; all ten spec verification scenarios and adjacent direct-upgrade, `/eunsub`, `/estatus`, GP, background, startup cleanup, terminal-failure, dirty-reuse, and rewrite interlock regressions are independently observable.

## Final verification and real-surface QA

- [ ] Run formatter and language-server checks before broad compilation:

```powershell
cargo fmt --all -- --check
```

Expected: exit code 0 and no formatting diff. Open each modified Rust file with `lsp_diagnostics`; expected: zero `error` and zero new `warning` diagnostics.

- [ ] Run focused repository and worker suites:

```powershell
cargo test -p pixivbot --bin pixivbot db::repo::eh_download_queue -- --nocapture
cargo test -p pixivbot --bin pixivbot db::repo::eh_gallery_jobs -- --nocapture
cargo test -p pixivbot --bin pixivbot db::repo::eh_gp_spend_attempts -- --nocapture
cargo test -p pixivbot --bin pixivbot db::repo::eh_download_completions -- --nocapture
cargo test -p pixivbot --bin pixivbot db::repo::eh_integration_tests -- --nocapture
cargo test -p pixivbot --bin pixivbot scheduler::eh_engine -- --nocapture
```

Expected: all focused tests PASS; no test reads `config.toml` or depends on persistent `data/test_cache` artifacts.

- [ ] Run the repository fast gate:

```powershell
make quick
```

Expected: format/check/focused default test/build stages in the Makefile complete successfully with no warnings promoted to errors.

- [ ] Exercise the real SQLite migration path in an isolated temporary database through the migration tests, then inspect read-only diff hygiene:

```powershell
cargo test -p pixivbot --bin pixivbot migration_groups_active_legacy_variants_and_leaves_terminal_history_unbound -- --nocapture
cargo test -p pixivbot --bin pixivbot migration_backfills_append_only_download_completions_before_clearing_compatibility -- --nocapture
git diff --check
```

Expected: both migration tests PASS and `git diff --check` prints no whitespace errors. Do not run the bot with a real token or inspect ignored configuration/data.

- [ ] Run the mandatory full gate:

```powershell
make ci
```

Expected: `fmt-check -> clippy` with `RUSTFLAGS=-Dwarnings` `-> check -> test -> release build` all PASS. Do not enable `ffmpeg-codec` unless the local FFmpeg H.264 encoder is already known to work.

## Self-review coverage matrix

| Requirement | Plan coverage |
|---|---|
| Migration, active legacy grouping, and historical completion backfill | Task 1 |
| Job/completion entities, relations, and SQLite helper schemas | Task 1 |
| Atomic enqueue, both unique-conflict recovery paths, and dirty-retired binding | Task 2 |
| Canonical same-variant sharing and variant isolation | Tasks 2–3 |
| Shared normal download, job-scoped GP attempts, and transactional append-only completion accounting | Task 3 |
| Same job complete→clean-retire→reactivate→complete counts both byte generations | Tasks 3 and 10 |
| Shared background download and stale claim recovery | Task 4 |
| Shared IPFS upload and Telegraph page creation | Task 5 |
| Late Telegraph demand and Telegraph-only terminal failure with affected delivery IDs/chats/titles | Task 5 |
| Exact fixed upload-failure fan-out; archive-only exclusion; download failure remains status-only | Tasks 3, 5, and 10 |
| One delayed gateway rewrite and rewrite/retirement payload interlock | Task 6 |
| Keyed chat cancellation ordering and sibling isolation | Task 7 |
| Bounded delivery concurrency default 2 and independent retry | Task 8 |
| Missing ZIP resets one shared generation | Task 8 |
| Persisted cleanup claim/retry/finalization, dirty reuse gate, fail-closed abort, orphan cleanup, and startup recovery | Task 9 |
| `/estatus` joined stages and historical unbound rows | Task 10 |
| Direct upgrade, `/eunsub`, GP, background, cleanup, rewrite regressions | Task 10 |
| `Throttle<Bot>`, safe user errors, no extra scope/dependencies, final `make ci` | Global Constraints and Final verification |

No implementation task contains a Git write or commit step. Logical wave dependencies are explicit, every behavior-changing task starts with a named RED test and ends with exact GREEN commands, and every spec verification scenario maps to a task.
