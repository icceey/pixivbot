# EH Result Reuse and Push Ledger Implementation Plan

> **For agentic workers:** Use the subagent-driven-development skill to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Persist reusable IPFS3 upload results keyed by variant + source fingerprint, and add a chat-level per-surface push ledger that dedups subscription deliveries while preserving direct-request force semantics.

**Architecture:** Two new tables (`eh_gallery_results`, `eh_gallery_push_ledger`) plus a nullable `eh_gallery_jobs.source_fingerprint` column. Result records are written atomically at upload success and applied at new-generation job creation / late telegraph demand. Ledger rows are upserted at successful surface-send markers and consulted only by subscription-source enqueues. One eh_client change exposes the IPFS3 CID on `TelegraphImageUrlPair`.

**Tech Stack:** Rust 1.94, SeaORM 1.1.20 (sqlx-sqlite), existing migration crate patterns, colocated tokio unit tests.

**Spec:** `docs/superpowers/specs/2026-08-26-eh-result-reuse-and-push-ledger-design.md`

**Global Constraints:**
- Rust 1.94; no new dependencies; no new config knobs.
- No changes to `EhGalleryVariant` identity semantics.
- Existing shared-job invariants (CAS generations, chat locks, fail-closed cleanup, exact-once notifications, marker preservation) remain authoritative.
- Migration registered in `migration/src/lib.rs`; test-schema parity in `setup_test_db`.
- No secrets in any persisted record.
- Subagents perform no Git writes.

---

### Task 1: Migration + entities + fingerprint plumbing

**Files:**
- Create: `migration/src/m20260826_000000_eh_result_reuse_and_push_ledger.rs`
- Modify: `migration/src/lib.rs`
- Create: `src/db/entities/eh_gallery_results.rs`, `src/db/entities/eh_gallery_push_ledger.rs`
- Modify: `src/db/entities/mod.rs`, `src/db/entities/eh_gallery_jobs.rs`
- Modify: `src/db/repo.rs` (`setup_test_db` schema parity)
- Modify: `eh_client/src/models.rs` (`EhGallery::source_fingerprint()`), `src/db/types/state.rs` (`EhPendingGallery.fingerprint`), `src/db/repo/eh_gallery_jobs.rs` (job creation writes fingerprint; `EhEnqueueRequest` gains `fingerprint: Option<String>`)

**Interfaces:**
- Produces: `EhGallery::source_fingerprint() -> String`; `EhPendingGallery { gid, token, title, posted, fingerprint: Option<String> }`; `EhEnqueueRequest.fingerprint`; private `EhGalleryJobResolution { job: eh_gallery_jobs::Model, started_new_generation: bool }` from `get_or_create_eh_gallery_job_in_txn`; entities `eh_gallery_results::Model`, `eh_gallery_push_ledger::Model` with all columns; migration creating both tables + `source_fingerprint` column + unique indexes.

- [ ] **Step 1: Write the failing migration test**

In `src/db/repo/eh_gp_spend_attempts.rs` (established migration-test home), add `migration_reuse_tables_and_fingerprint_column`. The test must run the NEW m20260826 migration too: add a `migrate_reuse_ledger_up(&db)` helper following the adjacent `shared_jobs_target_migration` pattern (eh_gp_spend_attempts.rs:133-147). It must explicitly run the existing shared-jobs target migration sequence and then invoke `m20260826_000000_eh_result_reuse_and_push_ledger::Migration.up(...)`; do **not** call `Migrator::up(db, None)` against the manually pre-created legacy schema. Use the helper in the new test (the existing `migrate_shared_jobs_up` stops before m20260826):

```rust
#[tokio::test]
async fn migration_reuse_tables_and_fingerprint_column() -> Result<()> {
    let db = new_db().await?;
    create_legacy_shared_jobs_tables(&db).await?;
    db.execute_unprepared(
        "INSERT INTO eh_download_queue (id, chat_id, gid, token, title, telegraph, source, status, created_at) VALUES \
         (1, 10, 501, 'tok-501', 'G501', 1, 'subscription', 'pending', '2026-08-20 00:00:00')"
    ).await?;
    migrate_reuse_ledger_up(&db).await?;
    // reuse table exists with the unique variant key
    db.execute_unprepared(
        "INSERT INTO eh_gallery_results (gid, token, download_mode, resolution, source_fingerprint, telegraph_url, created_at, updated_at) VALUES \
         (501, 'tok-501', 'archive', '780', '1|2|3|0', 'https://telegra.ph/x', '2026-08-20 01:00:00', '2026-08-20 01:00:00')"
    ).await?;
    let dup = db.execute_unprepared(
        "INSERT INTO eh_gallery_results (gid, token, download_mode, resolution, source_fingerprint, telegraph_url, created_at, updated_at) VALUES \
         (501, 'tok-501', 'archive', '780', '9|9|9|0', 'https://telegra.ph/y', '2026-08-20 02:00:00', '2026-08-20 02:00:00')"
    ).await;
    assert!(dup.is_err(), "duplicate variant must violate the unique index");
    // ledger unique key
    db.execute_unprepared(
        "INSERT INTO eh_gallery_push_ledger (chat_id, gid, archive_sent_at, telegraph_sent_at, updated_at) VALUES \
         (10, 501, '2026-08-20 03:00:00', NULL, '2026-08-20 03:00:00')"
    ).await?;
    let dup_ledger = db.execute_unprepared(
        "INSERT INTO eh_gallery_push_ledger (chat_id, gid, updated_at) VALUES (10, 501, '2026-08-20 04:00:00')"
    ).await;
    assert!(dup_ledger.is_err(), "duplicate (chat_id, gid) must violate the unique index");
    // jobs gained the nullable fingerprint column
    let job = db.query_one(Statement::from_string(DbBackend::Sqlite,
        "SELECT source_fingerprint FROM eh_gallery_jobs WHERE gid = 501".to_owned())).await?
        .expect("job for 501 exists");
    assert_eq!(job.try_get::<Option<String>>("", "source_fingerprint")?, None);
    Ok(())
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p pixivbot --bin pixivbot migration_reuse_tables -- --nocapture`
Expected: FAIL - `no such table: eh_gallery_results` / unknown column.

- [ ] **Step 3: Implement migration + entities + plumbing**

Follow `m20260824_000000_eh_shared_gallery_jobs.rs` patterns exactly (SQLite transaction branch, plain SQL DDL/DML, matching `DeriveIden` style, symmetric `down()` dropping tables/index/column). Table DDL per spec (including `UNIQUE (gid, token, download_mode, resolution)` and `UNIQUE (chat_id, gid)` inline constraints; timestamps NOT NULL with defaults where the existing migration style uses them). Add indexes only where the existing migration defines analogous ones (none required beyond the unique constraints; `created_at` index on results is unnecessary - queries are by unique key).

Entities: mirror existing entity files (`#[derive(Clone, Debug, PartialEq, DeriveModel, ...)]`, `Relation` impls not needed unless pattern requires; export in `mod.rs`). `eh_gallery_jobs` entity gains `pub source_fingerprint: Option<String>`.

`setup_test_db`: append both CREATE TABLEs and (if the helper adds columns separately) the fingerprint column, matching production DDL exactly.

`EhGallery::source_fingerprint()`:

```rust
pub fn source_fingerprint(&self) -> String {
    format!("{}|{}|{}|{}", self.posted, self.filecount, self.filesize, self.expunged)
}
```

`EhPendingGallery`: add `#[serde(default)] pub fingerprint: Option<String>`; existing constructors/tests updated mechanically.

`EhEnqueueRequest` gains `fingerprint: Option<String>`; `get_or_create_eh_gallery_job_in_txn` persists it into the new column on insert and returns `EhGalleryJobResolution { job, started_new_generation: true }`. On reset-for-new-generation (`reset_eh_gallery_job_generation_in_txn`) update it to the request's current value and also return `started_new_generation: true`; an unchanged existing generation returns `false`. The column records the current generation fingerprint and is read for cache-write/apply gating. `enqueue_eh_download` / `enqueue_eh_subscription_download` signatures gain `fingerprint: Option<&str>` (callers: `src/bot/handlers/download.rs`, `src/bot/handlers/subscription/ehentai.rs` both direct paths, and the collect loop at `eh_engine.rs:1225` pass `Some(gallery.fingerprint...)`; where `EhGallery` metadata is in scope use `Some(metadata.source_fingerprint())`, otherwise pass the stored backlog fingerprint or `None` for legacy callers). Collect loop stores `Some(fingerprint)` into new `EhPendingGallery` entries (compute from the batch metadata lookup that already produced `g.posted` - the same `EhGallery` objects are available where `eligible` is built).

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p pixivbot --bin pixivbot migration_reuse_tables -- --nocapture`
Expected: PASS. Then `cargo test -p pixivbot --bin pixivbot migration_ -- --nocapture` (all 12 migration tests).

- [ ] **Step 5: Full workspace check**

Run: `cargo check --workspace --all-targets` then `cargo test --workspace --all-targets`
Expected: compile-clean, all existing tests pass (mechanical signature updates compile).

---

### Task 2: Result-cache repo layer (write + apply helpers)

**Files:**
- Create: `src/db/repo/eh_gallery_results.rs`
- Modify: `src/db/repo.rs` (export), `src/db/repo/eh_gallery_jobs.rs`

**Interfaces:**
- Consumes: Task 1 entities.
- Produces: `pub async fn upsert_eh_gallery_result_in_txn(txn: &DatabaseTransaction, gid: i64, token: &str, variant: &EhGalleryVariant, fingerprint: &str, telegraph_url: &str, rewrite_data: Option<&str>, media_cids: Option<&str>) -> Result<()>`; `pub async fn find_eh_gallery_result_in_txn(txn, gid: i64, token: &str, variant: &EhGalleryVariant) -> Result<Option<eh_gallery_results::Model>>`; `pub(crate) async fn try_apply_cached_eh_result_in_txn(txn, job_id: i32, send_archive: bool) -> Result<bool>` (in `eh_gallery_jobs.rs`, returns whether a matching result was applied).

- [ ] **Step 1: Write failing repo tests**

In `src/db/repo/eh_gallery_results.rs` colocated tests:
- `upsert_replaces_prior_generation`: insert result for variant, upsert with new URL/fingerprint/media_cids -> single row, new values, `updated_at` advanced.
- `apply_cached_result_makes_telegraph_only_job_zipless_ready`: create an unclaimed pending job (fingerprint set, telegraph demand), matching result record exists; call apply helper inside a transaction -> job `status='downloaded'`, `zip_path` NULL, `telegraph_status='ready'`, URL set, rewrite payload set with `telegraph_rewritten_at` NULL and all rewrite scheduling/claim fields NULL; returns true.
- `apply_with_archive_demand_keeps_pending`: same but active delivery also wants archive -> job stays `pending`, telegraph ready; returns true.
- `apply_rejects_fingerprint_mismatch_or_missing_record_or_null_job_fingerprint`: three sub-cases -> returns false, job untouched.
- `apply_variant_isolation`: record for resolution "780" never applies to job of resolution "direct".

RED: helpers don't exist (compile failure is acceptable RED evidence for new modules).

- [ ] **Step 2: Implement helpers**

`upsert`: SQLite `INSERT ... ON CONFLICT(gid, token, download_mode, resolution) DO UPDATE SET ...` via raw statement or SeaORM `insert ... on_conflict` - match existing repo raw-SQL style for upserts (`eh_gp_spend_attempts` patterns). NOTE: `EhGalleryVariant` (eh_gallery_jobs.rs:54-58) carries only `download_mode`/`resolution` - gid/token must be threaded as separate parameters (e.g. `upsert_eh_gallery_result_in_txn(txn, gid, token, variant: &EhGalleryVariant, ...)` and the same for `find_...`), or the struct extended; the plan fixes the signature with separate `gid: i64, token: &str` parameters and no struct change. The apply helper derives all four key values from the job row directly.

`try_apply_cached_eh_result_in_txn`:
1. Load job; require `job.source_fingerprint` Some.
2. Load result by variant (derive `EhGalleryVariant` from job's gid/token/mode/resolution columns - reuse the existing variant-from-job mapping used elsewhere, e.g. how `archive_artifacts_for_job` or job selectors reconstruct it; if no such helper exists, construct directly).
3. Compare fingerprint strings; mismatch -> false.
4. UPDATE job: `telegraph_status='ready'`, `telegraph_url`, rewrite payload (set `telegraph_rewrite_data` from record; `telegraph_rewrite_status=NULL`, `telegraph_rewrite_after=NULL`, `telegraph_rewrite_started_at=NULL`, `telegraph_rewrite_next_retry_at=NULL`, `telegraph_rewrite_retry_count=0`, `telegraph_rewrite_error=NULL`, `telegraph_rewritten_at=NULL`), CAS-guarded by `job.id` + current `telegraph_status`/`status`/cleanup/upload-claim fields read in this transaction. This deliberately leaves the rewrite unscheduled until `mark_eh_telegraph_delivery_sent` observes the first successful reused-link send.
5. Determine archive demand: EXISTS delivery with `job_id=job.id`, active status, `archive_sent_at IS NULL` AND global `send_archive=true`. If there is no archive demand and the job is **unclaimed pending** (`status='pending'`, `background_download_status IS NULL`, cleanup none, Telegraph not uploading), set `status='downloaded'`, `zip_path=NULL`, `file_size=0`, `gp_cost=0`, `completed_at=now`. If a normal download (`status='downloading'`) or background download (`background_download_status='running'`) is in flight, preserve status, `started_at`, background status/lease, path and accounting fields while applying only Telegraph/rewrite fields. (`send_archive` is the third parameter of the helper signature above.)
6. Return true.

Wire into callers in later tasks. `mark_eh_job_telegraph_ready` unchanged in this task.

- [ ] **Step 3: GREEN + gates**

Run the new tests, then `cargo test -p pixivbot --bin pixivbot eh_gallery_results -- --nocapture`; fmt/clippy/check on workspace.

---

### Task 3: CID capture + ready-time result write

**Files:**
- Modify: `eh_client/src/telegraph.rs`, `src/scheduler/eh_engine.rs`, `src/db/repo/eh_gallery_jobs.rs`

**Interfaces:**
- Consumes: Task 1 fingerprint column, Task 2 upsert helper.
- Produces: `TelegraphImageUrlPair { preview_url, public_url, cid: Option<String> }`; `mark_eh_job_telegraph_ready(..., media_cids: Option<&str>)` writing the result record in the same transaction.

- [ ] **Step 1: Failing tests**

- `eh_client`: extend `ipfs3_uploader_returns_preview_and_public_url_pairs` (and the ZIP-extract pair test) to assert `pair.cid == Some(expected_cid)`; add a small assertion on a non-IPFS3 constructor (pixi/catbox/s3 pair builders) that `cid` is None.
- `pixivbot` upload worker: new test `successful_upload_persists_result_record_for_ipfs3` - uploader returns pairs with CIDs, job has fingerprint -> after upload tick: `eh_gallery_results` row exists with URL/rewrite payload/media_cids JSON `{"name":"001.jpg","cid":"bafk..."}`; job ready as before. Second test `non_ipfs3_upload_writes_no_result_record` - pairs with `cid: None` -> no row.
- RED: field/method don't exist.

- [ ] **Step 2: Implement**

- `TelegraphImageUrlPair` gains field; `url_pair_for_cid` sets it; every other pair construction site adds `cid: None` (grep all struct literals).
- Upload worker (`EhUploadWorker::process` + `create_telegraph_page_for_job`): build `Vec<(name, cid)>` alongside `all_url_pairs` (ZIP-first path already has `entry_names` in order; per-image path zips the collected pairs). Serialize `Some(json)` iff every pair has a CID. Pass through to `mark_eh_job_telegraph_ready(..., media_cids)`; inside its transaction, when `job.source_fingerprint.is_some() && media_cids.is_some()`, call `upsert_eh_gallery_result_in_txn` with the job's variant/fingerprint, page URL, serialized rewrite payload, media CIDs. Failure propagates (fails the ready transaction per spec).
- Rewrite payload capture: `mark_eh_job_telegraph_ready` already receives `rewrite_data_json`; pass the same string to the upsert.

- [ ] **Step 3: GREEN + gates**

New tests + existing `upload_worker_` suite + eh_client telegraph tests; fmt/clippy/check.

---

### Task 4: Reuse at new-generation creation + late telegraph demand

**Files:**
- Modify: `src/db/repo/eh_gallery_jobs.rs`

**Interfaces:**
- Consumes: `try_apply_cached_eh_result_in_txn` (Task 2).
- Produces: cache application at the two enqueue/demand sites.

- [ ] **Step 1: Failing tests** (in `eh_gallery_jobs.rs` colocated tests)

- `new_wave_telegraph_only_enqueue_reuses_cached_result`: seed result record; enqueue telegraph-only delivery for the variant with matching fingerprint -> returned job has `status='downloaded'`, telegraph ready, `zip_path` NULL; `get_next_eh_job_for_download_with_policy` / background selector return None; publish selector claims the delivery; no upload claim.
- `new_wave_with_archive_demand_reuses_telegraph_only`: job stays pending; telegraph ready; download selector claims; completion keeps ready state; upload selector never claims.
- `fingerprint_mismatch_falls_back_to_full_flow`: pending job + upload wave as today; on success the result record is replaced (new fingerprint).
- `late_telegraph_demand_applies_cached_result`: downloaded archive-only job, then telegraph demand arrives (recompute path) with matching record -> ready without upload claim.
- `late_telegraph_demand_during_archive_download_preserves_claim`: normal download claim is active, then matching Telegraph demand arrives -> ready fields apply while status/generation remain downloading; matching completion succeeds, preserves ready state, and no upload claim appears. Repeat the state assertion for a background-running claim or cover it in the Task 7 matrix.
- `cached_result_unsent_before_retirement_still_rewrites_after_reuse`: seed a cached result whose rewrite payload was persisted but no original delivery marker ever scheduled it; reuse it in a later wave, assert `telegraph_rewritten_at` remains NULL before send, then call the production Telegraph marker and assert one pending rewrite is scheduled.
- `archive_only_new_wave_does_not_consult_cache`: no result read when no telegraph demand at creation (assert via: record exists but job remains pending downloader-claimable; no observable change).
- RED: apply helper not yet wired.

- [ ] **Step 2: Implement**

- In `enqueue_eh_download_in_txn`, destructure the Task 1 `EhGalleryJobResolution`; AFTER `upsert_eh_delivery_in_txn` (so the archive-demand EXISTS check sees the new wave's deliveries) and BEFORE the telegraph recompute, call `try_apply_cached_eh_result_in_txn(txn, job.id, send_archive)` iff `req.telegraph && started_new_generation`. This explicit flag is the only generation-boundary test; do not infer it from `pending`, missing deliveries, timestamps, or claim state. The signature needs `send_archive`, which `enqueue_eh_download_request` must thread (add a `send_archive: bool` field to `EhEnqueueRequest`, set by both public enqueue APIs; thread as new parameter `send_archive: bool` on both public methods; callers: bot handlers use `self.eh_config.send_archive`, engine uses `self.config.send_archive`). The apply helper's own job UPDATE uses the current in-transaction job state (status/telegraph_status read inside the txn), so ordering after the delivery upsert is safe; the recompute that follows sees the applied ready state and skips demanding an upload.
- In `recompute_eh_job_telegraph_requirement_in_txn`: when demand flips to required (false->true) and job `telegraph_status` is `not_required` (the only eligible pre-upload state; `pending`/`uploading` waves and terminal `failed` keep their existing transitions), call the apply helper.
- Ensure apply respects existing guards: never when `cleanup_status != 'none'` and never during a Telegraph upload claim. Creation-time application targets only the explicit fresh-generation boundary. Late-demand application may run while a normal or background **download** claim exists; its CAS must preserve job `status`, `started_at`, background status/lease, artifact ownership and accounting fields. Zipless conversion is restricted to an unclaimed pending job with no archive demand.

- [ ] **Step 3: GREEN + gates**

New tests + adjacent enqueue/late-demand tests; full workspace test run.

---

### Task 5: Push ledger repo layer + marker integration

**Files:**
- Create: `src/db/repo/eh_gallery_push_ledger.rs`
- Modify: `src/db/repo.rs`, `src/db/repo/eh_download_queue.rs`, `src/db/repo/eh_gallery_jobs.rs`

**Interfaces:**
- Produces: `pub async fn record_eh_push_in_txn(txn, chat_id, gid, surface: EhPushSurface, sent_at: DateTime) -> Result<()>` with `pub enum EhPushSurface { Archive, Telegraph }`; integration into `mark_eh_archive_delivery_sent` (made transactional, `eh_download_queue.rs:2601`) and `mark_eh_telegraph_delivery_sent` (`eh_gallery_jobs.rs:1775`).

- [ ] **Step 1: Failing tests**

In `eh_gallery_push_ledger.rs`:
- `marker_writes_ledger_in_same_transaction`: seed active delivery/job; call production `mark_eh_archive_delivery_sent` -> ledger row has `archive_sent_at`, NULL telegraph; call production `mark_eh_telegraph_delivery_sent` -> same row gains `telegraph_sent_at`.
- `ledger_write_failure_fails_marker_transaction`: use the established SQLite trigger fault-injection pattern - trigger that raises on ledger insert -> `mark_eh_archive_delivery_sent` returns Err and the delivery marker stays NULL (full rollback).
- `ledger_upsert_is_idempotent`: two archive sends for same (chat,gid) -> single row, latest timestamp.
- RED: helper/table integration missing.

- [ ] **Step 2: Implement**

`record_eh_push_in_txn`: SQLite UPSERT `ON CONFLICT(chat_id, gid) DO UPDATE SET archive_sent_at=COALESCE(excluded.archive_sent_at, archive_sent_at), telegraph_sent_at=COALESCE(excluded.telegraph_sent_at, telegraph_sent_at), updated_at=excluded.updated_at` (insert the sent column, keep NULL for the other).

**Production integration points (critical):** the archive marker's production writer is `mark_eh_archive_delivery_sent` (`eh_download_queue.rs:2601`, called from the publish worker at `eh_engine.rs:2391`) - it currently has NO transaction. Wrap it in one: begin, read the delivery row (chat_id, gid, status, marker null-ness), CAS the marker via conditional UPDATE, upsert the ledger row with `EhPushSurface::Archive`, commit; rollback on any error so the ledger write fails together with the marker. The `#[cfg(test)]`-only `mark_eh_archive_sent` (`eh_download_queue.rs:2774`) is NOT the integration point - leave it test-only. `mark_eh_telegraph_delivery_sent` (`eh_gallery_jobs.rs:1775`, already transactional) adds the ledger upsert with `EhPushSurface::Telegraph` after its marker CAS inside the same transaction. The terminal-upload-failure archive fallback reaches the ledger through the standard publish-worker send path (the fallback downgrades the row to archive-only; the worker then sends the ZIP via `mark_eh_archive_delivery_sent`), so no separate fallback wiring is needed - but verify with a test. Task 5 Step 1 tests must exercise the production methods (`mark_eh_archive_delivery_sent`, `mark_eh_telegraph_delivery_sent`), not the cfg(test) helper.

- [ ] **Step 3: GREEN + gates**

New tests + all marker/publish/fallback suites (`test_publish_*`, `terminal_upload_*`, `upload_terminal_failure_*`).

---

### Task 6: Subscription enqueue dedup via ledger

**Files:**
- Modify: `src/db/repo/eh_gallery_jobs.rs`, `src/scheduler/eh_engine.rs`, `src/bot/handlers/download.rs`, `src/bot/handlers/subscription/ehentai.rs`

**Interfaces:**
- Consumes: ledger reads.
- Produces: `enqueue_eh_subscription_download` / `enqueue_eh_download` returning `Result<Option<eh_download_queue::Model>>`; skip + pre-mark logic for subscription source.

- [ ] **Step 1: Failing tests**

- `subscription_enqueue_skips_fully_delivered_gallery`: ledger row with both surfaces -> `enqueue_eh_subscription_download` returns `Ok(None)`; no delivery row/job change; collect-loop helper still advances `pushed_gids` (assert via the returned None being treated as success in the engine-level test below).
- `subscription_enqueue_pre_marks_archive_only_surface`: ledger has archive only; request wants both -> wave created with `archive_sent_at` set, `telegraph_sent_at` NULL; publish sends only link; delivery reaches done; ledger gains telegraph timestamp.
- `subscription_enqueue_pre_marks_telegraph_only_surface`: ledger has telegraph only -> pre-marked `telegraph_sent_at`; publish sends archive only; `telegraph_required` aggregation does not demand upload (job telegraph_status stays not_required).
- `direct_enqueue_bypasses_ledger_dedup`: ledger fully satisfied -> `/edl`-style direct enqueue still returns `Some`, fresh wave, both markers cleared on the new-wave delivery.
- `active_wave_merges_without_premark`: existing active delivery for (chat,gid) -> owner merge only; markers untouched.
- Engine-level: extend a collect test so that a fully-ledgered gallery returns None and `pushed_gids` still records the GID (collect loop: `if enqueue None { add_pushed_gid; continue; }` - treat as success).
- RED: signature returns Model, no dedup.

- [ ] **Step 2: Implement**

In `enqueue_eh_download_request` (single transaction, chat lock): for `source=subscription` only, read ledger row for (chat_id, gid). Wanted = archive iff `req.send_archive`, telegraph iff `req.telegraph`. All wanted satisfied -> commit empty transaction, return `Ok(None)`. Otherwise proceed; when creating/resetting the delivery wave (the terminal-reset branch of `upsert_eh_delivery_in_txn`), pre-set satisfied marker columns from ledger values before the delivery insert/update; ensure `recompute_eh_job_telegraph_requirement_in_txn` runs after (existing call) so pre-marked telegraph yields no upload demand.

Public API returns `Option<...>`; update all callers (collect loop treats None as pushed; direct handler paths unwrap-or-respond as today - they always get Some).

- [ ] **Step 3: GREEN + gates**

New tests + `test_collect_*` suite + bot handler compile.

---

### Task 7: End-to-end regressions + full gates

**Files:**
- Modify: `src/scheduler/eh_engine.rs` (tests), `src/db/repo/eh_gallery_results.rs` (tests)

**Interfaces:** none (verification only).

- [ ] **Step 1: Write flagship e2e tests**

- `chat_b_reuses_retired_gallery_result_without_source_work` (spec test 14): chat A full delivery (archive+telegraph via IPFS3 mock), job retires and ZIP family cleaned; later chat B subscription enqueue for same variant, fingerprint match; assert zero EH archive POSTs (WireMock server request count), zero provider upload calls (mock uploader counter), chat B receives cached URL, ledger row for chat B written, job retires again.
- `fingerprint_change_forces_full_refetch` (spec test 15): second wave with different filecount fingerprint -> one archive POST, one upload, result record replaced.
- `terminal_upload_failure_fallback_then_later_telegraph_subscription` (spec test 13): mixed delivery terminal upload failure with archive fallback; later telegraph-only subscription -> telegraph-only wave.
- `overlapping_subscriptions_deliver_gallery_once` (spec test 8, engine level): two subscriptions in one chat matching same GID; first wave completes both surfaces; second subscription's collect finds the GID -> `Ok(None)` skip; only one Telegram send sequence ever occurs.
- Include `late_telegraph_demand_during_background_download_preserves_claim` if Task 4 covered only the normal lane, and verify the reused-link marker schedules a cached rewrite payload exactly once.

- [ ] **Step 2: Run all gates**

`cargo fmt --all -- --check`; `$env:RUSTFLAGS="-Dwarnings"; cargo clippy --workspace --all-targets -- -D warnings`; same env `cargo check --workspace --all-targets`; `cargo test --workspace --all-targets`; `cargo build --release --workspace`; LSP diagnostics on all changed files; `git diff --check`.

- [ ] **Step 3: Self-review the whole diff against the spec**

Walk spec testing matrix items 1-15 and mark each as covered by a named test; fix gaps immediately.

---

## Execution order

`1 -> 2 -> 3 -> 4 -> 5 -> 6 -> 7` (strictly sequential; each task's helpers feed the next).

## Final acceptance

After Task 7: dispatch identity-bound final review (Oracle + Reviewer lanes per requesting-code-review) on the working tree; fix findings; then present the commit plan to the user (subagents never commit).
