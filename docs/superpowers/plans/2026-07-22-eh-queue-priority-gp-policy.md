# EH Queue Priority and GP Policy Implementation Plan

> **For agentic workers:** Use the subagent-driven-development skill to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the main EH queue favor recent FIFO work before old LIFO backlog, permanently fail archives whose numeric GP cost exceeds policy without spending GP, and reject unsupported direct archive resolutions before network activity.

**Architecture:** Keep queue classification dynamic inside the existing single-candidate SeaORM claim query, preserving its conditional-update claim. Extend the shared archive gate to `Proceed`/`Defer`/`Reject`, route `Reject` through two claim-state-specific CAS failure transitions, and enforce one resolution allowlist both at serde configuration input and at every public `EhClient` request boundary that accepts a direct archive resolution. No schema, status, migration, H@H workflow, or background-order change is introduced.

**Tech Stack:** Rust 1.94, Tokio, SeaORM/SeaQuery 1.1.20 with SQLite repository tests, serde/serde_json, reqwest, wiremock 0.6, cargo, rustfmt, Clippy, PowerShell.

**Global Constraints:**
- Only the main download claim in `Repo::get_next_for_download()` changes; its existing pending/background/retry eligibility filters and conditional-update claim remain unchanged.
- The cutoff is fixed at two hours; `created_at > cutoff` is recent and `created_at <= cutoff` is old.
- Recent entries sort by `created_at ASC, id ASC`; old entries sort after all recent entries by `created_at DESC, id DESC`.
- Queue age classification is derived at claim time and is not persisted or exposed as a new status.
- Background download ordering remains `created_at ASC` and must not be modified.
- Only `DownloadCost::Gp(gp)` with `gp > max_archive_gp_cost` is permanent `Reject`; therefore every positive GP cost rejects when the maximum is zero, while `Gp(0)` proceeds.
- Byte-rate exhaustion, rolling GP-budget exhaustion, `Insufficient`, `Unavailable`, and `Unknown` remain `Defer`; `Free` and `Unlocked` remain `Proceed`.
- Main and background policy failures use CAS guards, set the whole queue row to `failed`, store a short policy reason and completion time, clear applicable retry/background claim state, and do not increment `retry_count` or `background_download_attempt_count`.
- A policy rejection must not append to `eh_gp_spend_attempts` and must not issue the archive POST.
- Supported direct archive resolutions are exactly `780x`, `980x`, `1280x`, and `original`.
- Both `EhentaiConfig` resolution fields reject donor and unknown values during deserialization with an error that lists all supported values.
- `EhClient::prepare_archive_download()` rejects unsupported resolutions before its first GET; the direct `download_archive*` boundary must likewise reject before its POST so the public API matches its documentation.
- Do not implement H@H donor downloads, redirect-parser expansion, a migration, a persisted stale/priority flag, a new queue status, background reprioritization, or unrelated refactoring.
- Do not read ignored `config.toml`; use `config.toml.example` as the public configuration surface.
- All commands in this plan are PowerShell-compatible. Do not install `make`, FFmpeg, `pkg-config`, or any other software.
- The user explicitly authorized autonomous execution and pushing the completed, reviewed change to a new remote branch. Version-control writes remain orchestrator-only and occur only after final verification and Oracle approval.

---

## Authoritative inputs and execution order

- Authoritative design: `docs/superpowers/specs/2026-07-22-eh-queue-priority-gp-policy-design.md`.
- Baseline: `master` at `df2b5d7`; the design spec is currently untracked and must not be altered by this implementation.
- Execute Tasks 1, 2, and 3 sequentially. Tasks 1 and 2 both edit `src/db/repo/eh_download_queue.rs`; Tasks 2 and 3 both edit `src/config.rs` and `config.toml.example`, so parallel workers would create avoidable merge conflicts.
- At each task boundary, run the listed focused tests and read-only diff checks. Do not create commit checkpoints.

## File map

| File | Responsibility in this change |
|---|---|
| `src/db/repo/eh_download_queue.rs` | Build the dynamic two-hour main-claim ordering; add main/background CAS transitions for archive-policy failure; hold repository ordering/concurrency tests. |
| `src/scheduler/eh_engine.rs` | Define and consume `Proceed`/`Defer`/`Reject`; keep temporary gates deferred; ensure both workers fail rejected rows without ledger writes, POSTs, or retry consumption; hold worker/gate tests. |
| `src/config.rs` | Correct `Gp(0)` threshold semantics; deserialize both resolution fields through the client allowlist; update field documentation; hold serde tests. |
| `eh_client/src/client.rs` | Own the canonical direct-archive resolution allowlist and validator; invoke it before network requests; update public API docs and unit tests. |
| `eh_client/src/parser.rs` | Keep generic original/resample parsing behavior unchanged while narrowing API comments and test inputs to the four supported request-boundary values. |
| `eh_client/tests/integration.rs` | Replace donor-resolution success coverage with wiremock-backed pre-network rejection coverage. |
| `config.toml.example` | Document only supported resolutions and distinguish permanent per-archive GP rejection from temporary defers. |
| `docs/eh-archiver-page-reference.md` | Correct the public direct-archive mapping: generic `org`/`res` forms only, unsupported donor values rejected, H@H workflow explicitly out of scope. |

No file is created other than this plan. Do not modify `eh_client/src/error.rs`, `eh_client/src/lib.rs`, entities, migrations, `Cargo.toml`, or `Cargo.lock`; `Error::Other` and the public `eh_client::client` module are sufficient.

### Task 1: Dynamic recent-first main queue claim

**Files:**
- Modify: `src/db/repo/eh_download_queue.rs:38-52,1831-1886,3163-5326`
- Test: `src/db/repo/eh_download_queue.rs` colocated `#[cfg(test)]` module

**Interfaces:**
- Consumes: `Repo::get_next_for_download(&self) -> anyhow::Result<Option<eh_download_queue::Model>>`, `eh_download_queue::{Column::CreatedAt, Column::Id}`, `DateTime`, and the existing pending/background/retry eligibility filters.
- Produces: the unchanged public `Repo::get_next_for_download` signature with recent `ASC/ASC` then old `DESC/DESC` selection; private deterministic seam `get_next_for_download_at(&self, now: DateTime) -> Result<Option<Model>>`; private constant `MAIN_DOWNLOAD_RECENT_WINDOW_HOURS: i64 = 2`; no persisted field or migration.

- [ ] **Step 1: Add a failing real-SQLite ordering, boundary, exclusion, and background-regression test**

Add this helper and test to `src/db/repo/eh_download_queue.rs` inside the existing `tests` module. It deliberately uses duplicate timestamps to prove both id tie-break directions, puts rows exactly on the cutoff, and leaves the future-retry and background-owned rows pending so exclusion is observable.

```rust
    async fn set_download_claim_fields(
        repo: &Repo,
        id: i32,
        created_at: DateTime,
        next_retry_at: Option<DateTime>,
        background_download_status: Option<&str>,
    ) {
        Entity::update_many()
            .col_expr(Column::CreatedAt, Expr::value(created_at))
            .col_expr(Column::NextRetryAt, Expr::value(next_retry_at))
            .col_expr(
                Column::BackgroundDownloadStatus,
                Expr::value(background_download_status.map(str::to_owned)),
            )
            .col_expr(
                Column::BackgroundDownloadNextRetryAt,
                Expr::value(None::<DateTime>),
            )
            .filter(Column::Id.eq(id))
            .exec(&repo.db)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_main_download_claim_prioritizes_recent_fifo_then_old_lifo() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let anchor = Local::now().naive_local();
        let cutoff = anchor - chrono::Duration::hours(2);

        let recent_tie_low = repo
            .enqueue_eh_download(-100, 101, "a", "recent tie low", false, SOURCE_DIRECT)
            .await
            .unwrap();
        let recent_tie_high = repo
            .enqueue_eh_download(-100, 102, "b", "recent tie high", false, SOURCE_DIRECT)
            .await
            .unwrap();
        let recent_newest = repo
            .enqueue_eh_download(-100, 103, "c", "recent newest", false, SOURCE_DIRECT)
            .await
            .unwrap();
        let old_boundary_low = repo
            .enqueue_eh_download(-100, 104, "d", "old boundary low", false, SOURCE_DIRECT)
            .await
            .unwrap();
        let old_boundary_high = repo
            .enqueue_eh_download(-100, 105, "e", "old boundary high", false, SOURCE_DIRECT)
            .await
            .unwrap();
        let old_oldest = repo
            .enqueue_eh_download(-100, 106, "f", "old oldest", false, SOURCE_DIRECT)
            .await
            .unwrap();
        let future_retry = repo
            .enqueue_eh_download(-100, 107, "g", "future retry", false, SOURCE_DIRECT)
            .await
            .unwrap();
        let background_old = repo
            .enqueue_eh_download(-100, 108, "h", "background old", false, SOURCE_DIRECT)
            .await
            .unwrap();
        let background_new = repo
            .enqueue_eh_download(-100, 109, "i", "background new", false, SOURCE_DIRECT)
            .await
            .unwrap();

        let recent_tie_time = anchor - chrono::Duration::minutes(90);
        set_download_claim_fields(&repo, recent_tie_low.id, recent_tie_time, None, None).await;
        set_download_claim_fields(&repo, recent_tie_high.id, recent_tie_time, None, None).await;
        set_download_claim_fields(
            &repo,
            recent_newest.id,
            anchor - chrono::Duration::minutes(30),
            None,
            None,
        )
        .await;
        set_download_claim_fields(&repo, old_boundary_low.id, cutoff, None, None).await;
        set_download_claim_fields(&repo, old_boundary_high.id, cutoff, None, None).await;
        set_download_claim_fields(
            &repo,
            old_oldest.id,
            anchor - chrono::Duration::hours(4),
            None,
            None,
        )
        .await;
        set_download_claim_fields(
            &repo,
            future_retry.id,
            anchor - chrono::Duration::minutes(100),
            Some(anchor + chrono::Duration::hours(1)),
            None,
        )
        .await;
        set_download_claim_fields(
            &repo,
            background_old.id,
            anchor - chrono::Duration::minutes(110),
            None,
            Some(BACKGROUND_STATUS_PENDING),
        )
        .await;
        set_download_claim_fields(
            &repo,
            background_new.id,
            anchor - chrono::Duration::minutes(10),
            None,
            Some(BACKGROUND_STATUS_PENDING),
        )
        .await;

        assert!(recent_tie_low.id < recent_tie_high.id);
        assert!(old_boundary_low.id < old_boundary_high.id);

        let expected_main_order = [
            recent_tie_low.id,
            recent_tie_high.id,
            recent_newest.id,
            old_boundary_high.id,
            old_boundary_low.id,
            old_oldest.id,
        ];
        let mut actual_main_order = Vec::new();
        for _ in expected_main_order {
            actual_main_order.push(
                repo.get_next_for_download_at(anchor)
                    .await
                    .unwrap()
                    .unwrap()
                    .id,
            );
        }

        assert_eq!(actual_main_order, expected_main_order);
        assert!(repo
            .get_next_for_download_at(anchor)
            .await
            .unwrap()
            .is_none());

        let future = Entity::find_by_id(future_retry.id)
            .one(&repo.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(future.status, STATUS_PENDING);
        assert!(future.next_retry_at.is_some());

        let first_background = repo
            .get_next_for_background_download()
            .await
            .unwrap()
            .unwrap();
        let second_background = repo
            .get_next_for_background_download()
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first_background.id, background_old.id);
        assert_eq!(second_background.id, background_new.id);
    }
```

- [ ] **Step 2: Run the new test and confirm RED**

Run:

```powershell
cargo test -p pixivbot --bin pixivbot main_download_claim_prioritizes_recent_fifo_then_old_lifo -- --nocapture
```

Expected: compilation FAILS because the deterministic `get_next_for_download_at` seam does not exist. This is the RED signal; do not add the seam until Step 3. Once it exists, the test uses the in-memory SQLite database to prove the exact cutoff boundary and full order.

- [ ] **Step 3: Replace only candidate ordering in `get_next_for_download`**

Add the constant next to the queue status constants:

```rust
const MAIN_DOWNLOAD_RECENT_WINDOW_HOURS: i64 = 2;
```

Make the public method obtain the real clock once and delegate to a private deterministic seam. Move the existing claim body into that seam, keeping all three candidate filters, the entire conditional update, `rows_affected` handling, and re-fetch unchanged. This lets the test prove the exact equality boundary rather than relying on wall-clock timing:

```rust
    pub async fn get_next_for_download(&self) -> Result<Option<eh_download_queue::Model>> {
        self.get_next_for_download_at(Local::now().naive_local())
            .await
    }

    async fn get_next_for_download_at(
        &self,
        now: DateTime,
    ) -> Result<Option<eh_download_queue::Model>> {
```

Inside the seam, replace the single `.order_by(CreatedAt, Asc)` with this one-query CASE ordering:

```rust
        let cutoff = now - chrono::Duration::hours(MAIN_DOWNLOAD_RECENT_WINDOW_HOURS);
        let is_recent = || eh_download_queue::Column::CreatedAt.gt(cutoff);
        let entry = eh_download_queue::Entity::find()
            .filter(eh_download_queue::Column::Status.eq(STATUS_PENDING))
            .filter(eh_download_queue::Column::BackgroundDownloadStatus.is_null())
            .filter(
                eh_download_queue::Column::NextRetryAt
                    .is_null()
                    .or(eh_download_queue::Column::NextRetryAt.lte(now)),
            )
            .order_by(
                Expr::case(is_recent(), Expr::value(0_i32)).finally(Expr::value(1_i32)),
                Order::Asc,
            )
            .order_by(
                Expr::case(
                    is_recent(),
                    Expr::col(eh_download_queue::Column::CreatedAt),
                )
                .finally(Expr::value(None::<DateTime>)),
                Order::Asc,
            )
            .order_by(
                Expr::case(is_recent(), Expr::col(eh_download_queue::Column::Id))
                    .finally(Expr::value(None::<i32>)),
                Order::Asc,
            )
            .order_by(
                Expr::case(is_recent(), Expr::value(None::<DateTime>))
                    .finally(Expr::col(eh_download_queue::Column::CreatedAt)),
                Order::Desc,
            )
            .order_by(
                Expr::case(is_recent(), Expr::value(None::<i32>))
                    .finally(Expr::col(eh_download_queue::Column::Id)),
                Order::Desc,
            )
            .one(&self.db)
            .await
            .context("Failed to fetch next for download")?;
```

After this candidate query, retain the current conditional update and re-fetch through the seam's closing brace. This yields `ORDER BY recent_group ASC`, then recent timestamp/id ascending, then old timestamp/id descending. Because `is_recent` is strictly `created_at > cutoff`, the CASE `else` path includes equality and the fixed `anchor` test proves a row exactly at `anchor - 2h` is old. Do not add raw SQL. If the locked SeaORM compiler rejects an expression conversion, inspect the SeaORM 1.1.20 diagnostic/source and adjust only the `IntoSimpleExpr` conversion; retain this five-key CASE structure and backend-neutral query.

- [ ] **Step 4: Compile the CASE expression before running behavior tests**

Run:

```powershell
cargo check -p pixivbot --bin pixivbot
```

Expected: PASS. No backend-specific SQL string or migration should be needed.

- [ ] **Step 5: Run focused GREEN tests, including unchanged claim guards**

Run each command:

```powershell
cargo test -p pixivbot --bin pixivbot main_download_claim_prioritizes_recent_fifo_then_old_lifo -- --nocapture
cargo test -p pixivbot --bin pixivbot deferred_item_not_claimable_before_delay_expires -- --nocapture
cargo test -p pixivbot --bin pixivbot background_owned_item_is_excluded_from_main_download_queue -- --nocapture
cargo test -p pixivbot --bin pixivbot reenqueue_during_downloading_blocks_stale_download_completion -- --nocapture
```

Expected: every command PASS. The first test proves the complete sequence `recent-oldest -> recent-newest -> old-newest -> old-oldest`, both id tie-breakers, the `<= cutoff` boundary, future-retry exclusion, and unchanged background FIFO order. The remaining tests prove that candidate reprioritization did not weaken retry eligibility or conditional-update stale-worker protection.

- [ ] **Step 6: Run diagnostics and the Task 1 read-only review boundary**

Tool call:

```text
lsp_diagnostics(filePath="C:\Users\hugefiver\source\pixivbot\src\db\repo\eh_download_queue.rs", severity="all")
```

Expected: no error or warning diagnostics introduced by Task 1.

Run:

```powershell
git diff --check -- "src/db/repo/eh_download_queue.rs"
git diff -- "src/db/repo/eh_download_queue.rs"
```

Expected: `git diff --check` prints nothing; the diff changes only the main candidate ordering/constant and colocated tests. In particular, `get_next_for_background_download` still contains `.order_by(eh_download_queue::Column::CreatedAt, Order::Asc)` and its claim update is unchanged.

### Task 2: Permanent GP-policy rejection with CAS-safe worker transitions

**Files:**
- Modify: `src/db/repo/eh_download_queue.rs:38-52,2410-3095,3163-5326`
- Modify: `src/scheduler/eh_engine.rs:108-210,282-413,1004-1214,4972-6221`
- Modify: `src/config.rs:380-391,495-518,702-757`
- Modify: `config.toml.example:194-199`
- Test: colocated repository and scheduler tests in the two Rust files above

**Interfaces:**
- Consumes: Task 1's unchanged `Repo::get_next_for_download` claim contract; `DownloadCost`; `Repo::append_eh_gp_spend_attempt`; existing defer/retry methods; main claim state `status=downloading`; background claim state `status=pending AND background_download_status=running`.
- Produces: `ArchiveCostCheck::{Proceed, Defer { delay_secs, reason }, Reject { reason }}`; `Repo::fail_eh_download_for_archive_policy(id: i32, error: &str) -> Result<Model>`; `Repo::fail_eh_background_download_for_archive_policy(id: i32, error: &str) -> Result<Model>`; `BackgroundDownloadOutcome::Rejected { reason }`.

- [ ] **Step 1: Add RED repository tests for stale main/background policy failures**

Add these tests to `src/db/repo/eh_download_queue.rs`. Together they prove that a re-enqueue and a cancellation occurring after claim cannot be overwritten by a stale policy decision.

```rust
    #[tokio::test]
    async fn test_main_archive_policy_failure_does_not_overwrite_reenqueued_row() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let model = repo
            .enqueue_eh_subscription_download(-100, 501, 7001, "tok", "Title", false)
            .await
            .unwrap();
        let claimed = repo.get_next_for_download().await.unwrap().unwrap();
        assert_eq!(claimed.id, model.id);
        assert_eq!(claimed.status, STATUS_DOWNLOADING);

        let reenqueued = repo
            .enqueue_eh_download(-100, 7001, "new", "New", false, SOURCE_DIRECT)
            .await
            .unwrap();
        assert_eq!(reenqueued.status, STATUS_PENDING);

        let error = repo
            .fail_eh_download_for_archive_policy(
                claimed.id,
                "EH archive GP cost 1 exceeds configured max_archive_gp_cost=0",
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("expected status 'downloading'"));

        let row = Entity::find_by_id(model.id)
            .one(&repo.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.status, STATUS_PENDING);
        assert_eq!(row.retry_count, 0);
        assert!(row.completed_at.is_none());
        assert!(row.error.is_none());
    }

    #[tokio::test]
    async fn test_background_archive_policy_failure_does_not_overwrite_canceled_row() {
        let repo = tests_helpers::setup_test_db().await.unwrap();
        let model = repo
            .enqueue_eh_subscription_download(-100, 502, 7002, "tok", "Title", false)
            .await
            .unwrap();
        let main_claim = repo.get_next_for_download().await.unwrap().unwrap();
        repo.schedule_eh_background_download_from(
            main_claim.id,
            STATUS_DOWNLOADING,
            "test handoff",
        )
        .await
        .unwrap();
        let background_claim = repo
            .get_next_for_background_download()
            .await
            .unwrap()
            .unwrap();
        assert_eq!(background_claim.id, model.id);
        assert_eq!(
            background_claim.background_download_status.as_deref(),
            Some(BACKGROUND_STATUS_RUNNING)
        );

        assert_eq!(
            repo.cancel_eh_subscription_queue_entries(502)
                .await
                .unwrap(),
            1
        );
        let error = repo
            .fail_eh_background_download_for_archive_policy(
                background_claim.id,
                "EH archive GP cost 1 exceeds configured max_archive_gp_cost=0",
            )
            .await
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("expected a running background claim"));

        let row = Entity::find_by_id(model.id)
            .one(&repo.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.status, STATUS_CANCELED);
        assert!(row.background_download_status.is_none());
        assert!(row.completed_at.is_some());
    }
```

- [ ] **Step 2: Run the repository tests and confirm RED**

Run:

```powershell
cargo test -p pixivbot --bin pixivbot archive_policy_failure_does_not_overwrite -- --nocapture
```

Expected: compilation FAILS with `no method named fail_eh_download_for_archive_policy` and/or `no method named fail_eh_background_download_for_archive_policy`.

- [ ] **Step 3: Implement the two guarded policy-failure repository interfaces**

Add this private claim selector near the queue constants:

```rust
#[derive(Clone, Copy)]
enum ArchivePolicyClaim {
    Main,
    Background,
}
```

Add these methods near the existing defer/retry transitions. The shared implementation clears in-flight timestamps plus all background claim/retry fields, but intentionally leaves `retry_count` untouched. Resetting `background_download_attempt_count` to zero is cleanup, not consumption of another attempt.

```rust
    pub async fn fail_eh_download_for_archive_policy(
        &self,
        id: i32,
        error: &str,
    ) -> Result<eh_download_queue::Model> {
        self.fail_eh_download_for_archive_policy_claim(id, error, ArchivePolicyClaim::Main)
            .await
    }

    pub async fn fail_eh_background_download_for_archive_policy(
        &self,
        id: i32,
        error: &str,
    ) -> Result<eh_download_queue::Model> {
        self.fail_eh_download_for_archive_policy_claim(id, error, ArchivePolicyClaim::Background)
            .await
    }

    async fn fail_eh_download_for_archive_policy_claim(
        &self,
        id: i32,
        error: &str,
        claim: ArchivePolicyClaim,
    ) -> Result<eh_download_queue::Model> {
        let now = Local::now().naive_local();
        let update = eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::Status,
                Expr::value(STATUS_FAILED),
            )
            .col_expr(
                eh_download_queue::Column::Error,
                Expr::value(Some(error.to_string())),
            )
            .col_expr(
                eh_download_queue::Column::CompletedAt,
                Expr::value(Some(now)),
            )
            .col_expr(
                eh_download_queue::Column::StartedAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::NextRetryAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::BackgroundDownloadStatus,
                Expr::value(None::<String>),
            )
            .col_expr(
                eh_download_queue::Column::BackgroundDownloadStartedAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::BackgroundDownloadNextRetryAt,
                Expr::value(None::<DateTime>),
            )
            .col_expr(
                eh_download_queue::Column::BackgroundDownloadAttemptCount,
                Expr::value(0),
            )
            .col_expr(
                eh_download_queue::Column::BackgroundDownloadError,
                Expr::value(None::<String>),
            )
            .filter(eh_download_queue::Column::Id.eq(id));

        let update = match claim {
            ArchivePolicyClaim::Main => {
                update.filter(eh_download_queue::Column::Status.eq(STATUS_DOWNLOADING))
            }
            ArchivePolicyClaim::Background => update
                .filter(eh_download_queue::Column::Status.eq(STATUS_PENDING))
                .filter(
                    eh_download_queue::Column::BackgroundDownloadStatus
                        .eq(BACKGROUND_STATUS_RUNNING),
                ),
        };
        let result = update
            .exec(&self.db)
            .await
            .context("Failed to mark EH download failed by archive policy")?;

        if result.rows_affected != 1 {
            match claim {
                ArchivePolicyClaim::Main => anyhow::bail!(
                    "Cannot fail EH download {} by archive policy: expected status '{}'",
                    id,
                    STATUS_DOWNLOADING
                ),
                ArchivePolicyClaim::Background => anyhow::bail!(
                    "Cannot fail EH download {} by archive policy: expected a running background claim",
                    id
                ),
            }
        }

        eh_download_queue::Entity::find_by_id(id)
            .one(&self.db)
            .await?
            .context("Entry disappeared after archive-policy failure")
    }
```

Do not reuse the old unguarded `mark_eh_download_failed`: it increments `retry_count` and performs an unguarded active-model update.

- [ ] **Step 4: Run the repository policy-CAS tests GREEN**

Run:

```powershell
cargo test -p pixivbot --bin pixivbot archive_policy_failure_does_not_overwrite -- --nocapture
```

Expected: 2 tests PASS; final rows remain `pending` after re-enqueue and `canceled` after cancellation.

- [ ] **Step 5: Rewrite the main/background worker tests to expect permanent rejection**

Replace `test_download_worker_gp_cost_exceeds_limit_defers_without_post` with the following. This is the required positive-GP/max-zero case and asserts the complete no-spend/no-retry state.

```rust
    #[tokio::test]
    async fn test_download_worker_gp_cost_exceeds_policy_fails_without_spend_or_retry() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let eh_server = MockServer::start().await;
        let temp = tempfile::tempdir().unwrap();

        setup_chat(&repo, -100, true).await;
        let entry = insert_queue_entry(
            &repo,
            -100,
            2284788,
            "7841d194d4",
            "GP Required Gallery",
            false,
            STATUS_PENDING,
            None,
            None,
        )
        .await;
        mock_eh_archiver_page_with_cost(
            &eh_server,
            2284788,
            "7841d194d4",
            "8,800 GP",
            "218 GP",
        )
        .await;
        Mock::given(method("POST"))
            .and(path("/archiver.php"))
            .respond_with(ResponseTemplate::new(200).set_body_string("must not be called"))
            .expect(0)
            .mount(&eh_server)
            .await;

        let mut config = make_config();
        config.background_download_enabled = false;
        config.max_archive_gp_cost = 0;
        let worker = EhDownloadWorker::new(
            Arc::clone(&repo),
            make_eh_client(&eh_server),
            Arc::new(config),
            temp.path().to_path_buf(),
        );
        worker.tick().await.unwrap();

        let updated = eh_download_queue::Entity::find_by_id(entry.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(updated.status, STATUS_FAILED);
        assert_eq!(
            updated.error.as_deref(),
            Some("EH archive GP cost 218 exceeds configured max_archive_gp_cost=0")
        );
        assert!(updated.completed_at.is_some());
        assert!(updated.started_at.is_none());
        assert!(updated.next_retry_at.is_none());
        assert_eq!(updated.retry_count, 0);
        assert!(updated.background_download_status.is_none());
        assert!(gp_attempts(repo.as_ref()).await.is_empty());
    }
```

Replace `test_background_worker_gp_cost_exceeds_defers_without_retry_increment` with the following:

```rust
    #[tokio::test]
    async fn test_background_worker_gp_cost_exceeds_policy_fails_without_spend_or_attempt() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let eh_server = MockServer::start().await;
        let temp = tempfile::tempdir().unwrap();

        let entry = insert_queue_entry(
            &repo,
            -100,
            2284788,
            "7841d194d4",
            "BG GP Gallery",
            false,
            STATUS_PENDING,
            None,
            None,
        )
        .await;
        repo.schedule_eh_background_download_from(entry.id, STATUS_PENDING, "test setup")
            .await
            .unwrap();
        mock_eh_archiver_page_with_cost(
            &eh_server,
            2284788,
            "7841d194d4",
            "8,800 GP",
            "218 GP",
        )
        .await;
        Mock::given(method("POST"))
            .and(path("/archiver.php"))
            .respond_with(ResponseTemplate::new(200).set_body_string("must not be called"))
            .expect(0)
            .mount(&eh_server)
            .await;

        let mut config = make_config();
        config.background_download_enabled = true;
        config.background_download_concurrency = 1;
        config.max_archive_gp_cost = 0;
        let worker = EhBackgroundDownloadWorker::new(
            Arc::clone(&repo),
            make_eh_client(&eh_server),
            Arc::new(config),
            temp.path().to_path_buf(),
        );
        worker.tick().await.unwrap();

        let updated = eh_download_queue::Entity::find_by_id(entry.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(updated.status, STATUS_FAILED);
        assert_eq!(
            updated.error.as_deref(),
            Some("EH archive GP cost 218 exceeds configured max_archive_gp_cost=0")
        );
        assert!(updated.completed_at.is_some());
        assert_eq!(updated.retry_count, 0);
        assert!(updated.next_retry_at.is_none());
        assert!(updated.background_download_status.is_none());
        assert!(updated.background_download_started_at.is_none());
        assert!(updated.background_download_next_retry_at.is_none());
        assert!(updated.background_download_error.is_none());
        assert_eq!(updated.background_download_attempt_count, 0);
        assert!(gp_attempts(repo.as_ref()).await.is_empty());
    }
```

Replace `test_download_worker_original_resolution_gp_cost_defers` with
`test_download_worker_original_resolution_gp_cost_rejects`. Keep its existing
original-resolution Archiver fixture and zero-POST mock, then replace the final
policy assertions with the complete permanent-failure state:

```rust
        let updated = eh_download_queue::Entity::find_by_id(entry.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(updated.status, STATUS_FAILED);
        assert_eq!(
            updated.error.as_deref(),
            Some("EH archive GP cost 8800 exceeds configured max_archive_gp_cost=0")
        );
        assert!(updated.completed_at.is_some());
        assert!(updated.started_at.is_none());
        assert!(updated.next_retry_at.is_none());
        assert_eq!(updated.retry_count, 0);
        assert!(updated.background_download_status.is_none());
        assert!(gp_attempts(repo.as_ref()).await.is_empty());
```

Update the test's rustdoc and inline comment from “must defer” to “must reject”.
This locks the same numeric threshold policy for the original form's distinct
8,800 GP cost and prevents the old pending-state assertion from surviving the
policy change.

Replace the existing mixed “unallowed costs” gate test with this classification test. It explicitly covers threshold rejection, max-zero rejection, all nonnumeric defers, byte defer, and no ledger writes:

```rust
    #[tokio::test]
    async fn test_archive_cost_policy_rejects_only_gp_above_max_and_defers_temporary_states() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        let entry = insert_queue_entry(
            &repo,
            -100,
            1001,
            "a1b2c3d4",
            "Policy gallery",
            false,
            STATUS_PENDING,
            None,
            None,
        )
        .await;
        let mut config = make_config();
        config.max_archive_gp_cost = 218;
        config.gp_rate_limit = 218;

        let over_max = check_and_reserve_archive_cost(
            repo.as_ref(),
            &config,
            entry.id,
            entry.gid,
            &DownloadCost::Gp(219),
        )
        .await
        .unwrap();
        assert!(matches!(over_max, ArchiveCostCheck::Reject { .. }));

        config.max_archive_gp_cost = 0;
        let positive_with_zero_max = check_and_reserve_archive_cost(
            repo.as_ref(),
            &config,
            entry.id,
            entry.gid,
            &DownloadCost::Gp(1),
        )
        .await
        .unwrap();
        assert!(matches!(
            positive_with_zero_max,
            ArchiveCostCheck::Reject { .. }
        ));

        for cost in [
            DownloadCost::Unknown,
            DownloadCost::Unavailable,
            DownloadCost::Insufficient,
        ] {
            let outcome = check_and_reserve_archive_cost(
                repo.as_ref(),
                &config,
                entry.id,
                entry.gid,
                &cost,
            )
            .await
            .unwrap();
            assert!(matches!(outcome, ArchiveCostCheck::Defer { .. }));
        }

        config.download_rate_limit_gb = 0;
        let byte_limited = check_and_reserve_archive_cost(
            repo.as_ref(),
            &config,
            entry.id,
            entry.gid,
            &DownloadCost::Free,
        )
        .await
        .unwrap();
        match byte_limited {
            ArchiveCostCheck::Defer { reason, .. } => {
                assert!(reason.contains("byte rate limit"));
            }
            ArchiveCostCheck::Proceed => panic!("byte exhaustion must defer"),
            ArchiveCostCheck::Reject { reason } => {
                panic!("byte exhaustion must not reject: {reason}")
            }
        }
        assert!(gp_attempts(repo.as_ref()).await.is_empty());
    }
```

- [ ] **Step 6: Run the scheduler policy tests and confirm RED**

Run:

```powershell
cargo test -p pixivbot --bin pixivbot archive_cost_policy -- --nocapture
cargo test -p pixivbot --bin pixivbot gp_cost_exceeds_policy -- --nocapture
```

Expected: compilation FAILS because `ArchiveCostCheck::Reject` and `check_and_reserve_archive_cost` do not exist yet. If the test names compile after a partial edit, worker assertions FAIL because current behavior leaves rows pending.

- [ ] **Step 7: Implement the three-way gate and correct `Gp(0)` policy semantics**

Rename `check_and_reserve_archive_cost_or_defer` to `check_and_reserve_archive_cost` everywhere. Replace its enum and function with this code:

```rust
/// Outcome of the shared archive gate after the safe Archiver-page GET and
/// before any ledger reservation or archive POST.
enum ArchiveCostCheck {
    Proceed,
    Defer { delay_secs: i64, reason: String },
    Reject { reason: String },
}

async fn check_and_reserve_archive_cost(
    repo: &Repo,
    config: &EhentaiConfig,
    queue_id: i32,
    gid: i64,
    cost: &DownloadCost,
) -> Result<ArchiveCostCheck> {
    let defer_delay_secs = config.download_poll_interval_sec.max(60) as i64;

    match cost {
        DownloadCost::Gp(gp) if *gp > config.max_archive_gp_cost => {
            return Ok(ArchiveCostCheck::Reject {
                reason: format!(
                    "EH archive GP cost {} exceeds configured max_archive_gp_cost={}",
                    gp, config.max_archive_gp_cost
                ),
            });
        }
        DownloadCost::Insufficient
        | DownloadCost::Unavailable
        | DownloadCost::Unknown => {
            return Ok(ArchiveCostCheck::Defer {
                delay_secs: defer_delay_secs,
                reason: format!("EH archive cost {cost:?} is not currently usable"),
            });
        }
        DownloadCost::Free | DownloadCost::Unlocked | DownloadCost::Gp(_) => {}
    }

    let downloaded_bytes = repo
        .get_eh_downloaded_bytes_in_window(config.download_rate_window_hours)
        .await?;
    if downloaded_bytes >= config.download_rate_limit_bytes() as i64 {
        return Ok(ArchiveCostCheck::Defer {
            delay_secs: defer_delay_secs,
            reason: format!(
                "EH byte rate limit reached ({} bytes in last {}h)",
                downloaded_bytes, config.download_rate_window_hours
            ),
        });
    }

    let DownloadCost::Gp(gp) = cost else {
        return Ok(ArchiveCostCheck::Proceed);
    };
    if *gp == 0 {
        return Ok(ArchiveCostCheck::Proceed);
    }
    let gp_cost = i64::try_from(*gp).context("EH archive GP cost exceeds supported range")?;

    if config.gp_rate_limit > 0 {
        let _budget_lock = EH_GP_BUDGET_LOCK.lock().await;
        let window_hours = config.gp_rate_window_hours_clamped();
        let spent = repo.get_eh_gp_cost_in_window(window_hours).await?;
        if i128::from(spent) + i128::from(gp_cost) > i128::from(config.gp_rate_limit) {
            return Ok(ArchiveCostCheck::Defer {
                delay_secs: gp_rate_defer_delay_secs(window_hours),
                reason: format!(
                    "EH GP rate limit would be exceeded ({} + {} > {} in last {}h)",
                    spent, gp_cost, config.gp_rate_limit, window_hours
                ),
            });
        }
        repo.append_eh_gp_spend_attempt(queue_id, gid, gp_cost)
            .await?;
    } else {
        repo.append_eh_gp_spend_attempt(queue_id, gid, gp_cost)
            .await?;
    }

    Ok(ArchiveCostCheck::Proceed)
}
```

The static permanent policy is evaluated before temporary byte/rolling quotas so an archive that can never pass the configured maximum is not repeatedly deferred. Nonnumeric states still return `Defer` and never reach a ledger write.

Update `EhentaiConfig::allows_archive_gp_cost` so `Gp(0)` is allowed when the maximum is zero:

```rust
    /// Returns whether the parsed cost is statically allowed by the per-archive
    /// threshold. Nonnumeric states return false here; the scheduler maps those
    /// states to a temporary defer rather than a permanent policy rejection.
    pub fn allows_archive_gp_cost(&self, cost: &eh_client::parser::DownloadCost) -> bool {
        use eh_client::parser::DownloadCost;
        match cost {
            DownloadCost::Free | DownloadCost::Unlocked => true,
            DownloadCost::Gp(gp) => *gp <= self.max_archive_gp_cost,
            DownloadCost::Insufficient | DownloadCost::Unavailable | DownloadCost::Unknown => false,
        }
    }
```

Update `test_config_allows_archive_gp_cost` so its default-max assertions include:

```rust
        assert!(cfg.allows_archive_gp_cost(&DownloadCost::Free));
        assert!(cfg.allows_archive_gp_cost(&DownloadCost::Unlocked));
        assert!(cfg.allows_archive_gp_cost(&DownloadCost::Gp(0)));
        assert!(!cfg.allows_archive_gp_cost(&DownloadCost::Gp(1)));
        assert!(!cfg.allows_archive_gp_cost(&DownloadCost::Insufficient));
        assert!(!cfg.allows_archive_gp_cost(&DownloadCost::Unavailable));
        assert!(!cfg.allows_archive_gp_cost(&DownloadCost::Unknown));
```

- [ ] **Step 8: Route `Reject` through main and background CAS transitions**

Extend `BackgroundDownloadOutcome`:

```rust
enum BackgroundDownloadOutcome {
    Completed {
        file_size: u64,
        zip_path: std::path::PathBuf,
        gp_cost: i64,
    },
    Deferred {
        reason: String,
    },
    Rejected {
        reason: String,
    },
}
```

Add this arm to `EhBackgroundDownloadWorker::process_claimed` between `Deferred` and `Err` so a CAS error propagates directly instead of being misclassified as a network retry:

```rust
            Ok(BackgroundDownloadOutcome::Rejected { reason }) => {
                self.repo
                    .fail_eh_background_download_for_archive_policy(entry.id, &reason)
                    .await?;
                warn!(
                    "Rejected EH background download gid={} by archive policy: {}",
                    entry.gid, reason
                );
            }
```

In the background gate match, retain the existing `Proceed` and `Defer` arms and add:

```rust
                ArchiveCostCheck::Reject { reason } => {
                    return Ok(BackgroundDownloadOutcome::Rejected { reason });
                }
```

In the main worker gate match, retain the existing `Proceed` and `Defer` arms and add:

```rust
                ArchiveCostCheck::Reject { reason } => {
                    self.repo
                        .fail_eh_download_for_archive_policy(entry.id, &reason)
                        .await?;
                    warn!(
                        "Rejected EH download gid={} by archive policy: {}",
                        entry.gid, reason
                    );
                    return Ok(());
                }
```

Returning `Ok`/`Rejected` after the guarded state update is essential: neither worker may fall through to `schedule_eh_retry_from` or `schedule_eh_background_download_retry` for this policy outcome.

Update every test-only exhaustive match over `ArchiveCostCheck` to include a `Reject` arm. Within-limit concurrency/rolling-budget tests should treat rejection as a test failure, for example:

```rust
                ArchiveCostCheck::Proceed => proceeds += 1,
                ArchiveCostCheck::Defer { .. } => defers += 1,
                ArchiveCostCheck::Reject { reason } => {
                    panic!("within-limit archive must not reject: {reason}")
                }
```

- [ ] **Step 9: Update GP-policy documentation without changing configuration shape**

In the `EhentaiConfig::max_archive_gp_cost` field comment and `config.toml.example`, state the exact outcomes. Use this example text:

```rust
    /// Maximum GP cost allowed for one archive. A numeric cost above this value
    /// permanently fails the queue row without a ledger reservation or archive
    /// POST. `0` permits only `Gp(0)`, Free, or Unlocked archives. Nonnumeric
    /// costs and rolling/byte budget exhaustion remain temporary defers.
    #[serde(default = "default_eh_max_archive_gp_cost")]
    pub max_archive_gp_cost: u64,
```

Use the corresponding public example text:

```toml
# # Maximum GP cost allowed for a single archive download (default: 0).
# # A numeric GP cost above this value permanently fails the queue entry without
# # a ledger reservation or archive POST; 0 therefore permits only zero-GP,
# # Free, or Unlocked archives.
# # Insufficient Funds / N/A / Unknown and rolling/byte budget exhaustion defer.
# max_archive_gp_cost = 0
```

Do not add a config field or post-deserialization validation pass.

- [ ] **Step 10: Run focused GREEN policy and regression tests**

Run:

```powershell
cargo test -p pixivbot --bin pixivbot archive_policy_failure -- --nocapture
cargo test -p pixivbot --bin pixivbot archive_cost_policy -- --nocapture
cargo test -p pixivbot --bin pixivbot download_worker_gp_cost_exceeds_policy -- --nocapture
cargo test -p pixivbot --bin pixivbot background_worker_gp_cost_exceeds_policy -- --nocapture
cargo test -p pixivbot --bin pixivbot original_resolution_gp_cost_rejects -- --nocapture
cargo test -p pixivbot --bin pixivbot non_positive_or_free_costs_skip_ledger -- --nocapture
cargo test -p pixivbot --bin pixivbot gp_rate_limit -- --nocapture
cargo test -p pixivbot --bin pixivbot check_and_reserve_archive_cost_allows_one_concurrent_gp_attempt -- --nocapture
```

Expected: every command PASS. Specifically, both worker tests observe `failed`, a completion timestamp and exact policy reason, no archive POST, empty GP ledger, and zero consumed retry/attempt count; the temporary-state and rolling/byte-budget tests remain `Defer`; free/unlocked/zero-GP paths remain `Proceed`.

- [ ] **Step 11: Verify all gate matches, run diagnostics, and review the Task 2 diff**

Run:

```powershell
rg -n "check_and_reserve_archive_cost_or_defer|ArchiveCostCheck::" "src/scheduler/eh_engine.rs"
```

Expected: no old function-name match; every match site includes the `Reject` outcome or uses `matches!` for one explicitly intended variant.

Tool calls:

```text
lsp_diagnostics(filePath="C:\Users\hugefiver\source\pixivbot\src\db\repo\eh_download_queue.rs", severity="all")
lsp_diagnostics(filePath="C:\Users\hugefiver\source\pixivbot\src\scheduler\eh_engine.rs", severity="all")
lsp_diagnostics(filePath="C:\Users\hugefiver\source\pixivbot\src\config.rs", severity="all")
```

Expected: no new error or warning diagnostics.

Run:

```powershell
git diff --check -- "src/db/repo/eh_download_queue.rs" "src/scheduler/eh_engine.rs" "src/config.rs" "config.toml.example"
git diff -- "src/db/repo/eh_download_queue.rs" "src/scheduler/eh_engine.rs" "src/config.rs" "config.toml.example"
```

Expected: no whitespace errors; no retry increment in either policy-failure method; no ledger append before a `Reject`; no archive POST before the gate; no background-order edit.

### Task 3: Resolution allowlist at config and client network boundaries

**Files:**
- Modify: `eh_client/src/client.rs:10-13,86-130,319-432,961-1020`
- Modify: `eh_client/src/parser.rs:11-17,387-418,610-703,1007-1025,1112-1139`
- Modify: `eh_client/tests/integration.rs:724-994`
- Modify: `src/config.rs:328-355,535-607,702-757`
- Modify: `config.toml.example:155-160`
- Modify: `docs/eh-archiver-page-reference.md:110-178`
- Test: `eh_client/src/client.rs` unit tests, `eh_client/tests/integration.rs`, and `src/config.rs` unit tests

**Interfaces:**
- Consumes: `eh_client::Error::Other(String)`, public module path `eh_client::client`, serde field deserializers, and existing `EhClient::{prepare_archive_download, download_archive_with_options}` signatures.
- Produces: `eh_client::client::SUPPORTED_ARCHIVE_RESOLUTIONS: [&str; 4]`; `eh_client::client::validate_archive_resolution(resolution: &str) -> eh_client::Result<()>`; both `EhentaiConfig` resolution fields remain `String` but deserialize through `deserialize_eh_archive_resolution`.

- [ ] **Step 1: Add the client validator unit test and confirm RED**

Add this test to `eh_client/src/client.rs` inside its existing `tests` module:

```rust
    #[test]
    fn test_archive_resolution_validation_accepts_only_supported_values() {
        for resolution in SUPPORTED_ARCHIVE_RESOLUTIONS {
            validate_archive_resolution(resolution).unwrap();
        }

        for resolution in ["1600x", "2400x", "bogus", ""] {
            let error = validate_archive_resolution(resolution).unwrap_err();
            let message = error.to_string();
            assert!(message.contains("unsupported EH archive resolution"));
            assert!(message.contains("780x, 980x, 1280x, original"));
        }
    }
```

Run:

```powershell
cargo test -p eh_client --lib archive_resolution_validation_accepts_only_supported_values -- --nocapture
```

Expected: compilation FAILS because the constant and validator are not defined.

- [ ] **Step 2: Add the canonical allowlist and validator**

Add this next to the archive timeout constants in `eh_client/src/client.rs`:

```rust
pub const SUPPORTED_ARCHIVE_RESOLUTIONS: [&str; 4] = ["780x", "980x", "1280x", "original"];

pub fn validate_archive_resolution(resolution: &str) -> Result<()> {
    if SUPPORTED_ARCHIVE_RESOLUTIONS.contains(&resolution) {
        return Ok(());
    }

    Err(Error::Other(format!(
        "unsupported EH archive resolution '{resolution}'; supported values: 780x, 980x, 1280x, original"
    )))
}
```

Run the unit command from Step 1 again.

Expected: PASS for all four supported values and all four unsupported examples.

- [ ] **Step 3: Replace donor-success integration coverage with pre-network rejection coverage**

Delete `test_prepare_archive_download_1600x_uses_direct_resample_form` and replace it with:

```rust
#[tokio::test]
async fn test_prepare_archive_download_rejects_unsupported_resolution_before_network() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(500).set_body_string("network must not be reached"))
        .expect(0)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500).set_body_string("network must not be reached"))
        .expect(0)
        .mount(&server)
        .await;
    let client = client_at(&server);

    for resolution in ["1600x", "2400x", "bogus", ""] {
        let error = client
            .prepare_archive_download(4034806, "e13b7d119b", resolution)
            .await
            .unwrap_err();
        let message = error.to_string();
        assert!(message.contains("unsupported EH archive resolution"));
        assert!(message.contains("780x, 980x, 1280x, original"));
    }

    assert!(server.received_requests().await.unwrap().is_empty());
}
```

Keep the existing `1280x` and `original` end-to-end form tests; they prove valid resample/original traffic still reaches the intended direct archive forms.

Add a second integration test for the archiver-key public methods, proving both
`download_archive` and `download_archive_with_options` reject before their POST:

```rust
#[tokio::test]
async fn test_archive_key_downloads_reject_unsupported_resolution_before_network() {
    let server = MockServer::start().await;
    let client = client_at(&server);
    let temp = tempfile::tempdir().unwrap();

    for resolution in ["1600x", "2400x", "bogus", ""] {
        let simple_dest = temp.path().join(format!("simple-{resolution}.zip"));
        let simple_error = client
            .download_archive(
                4034806,
                "e13b7d119b",
                "123456--abc123def456",
                resolution,
                &simple_dest,
            )
            .await
            .unwrap_err();
        let simple_message = simple_error.to_string();
        assert!(simple_message.contains("unsupported EH archive resolution"));
        assert!(simple_message.contains("780x, 980x, 1280x, original"));

        let options_dest = temp.path().join(format!("options-{resolution}.zip"));
        let options_error = client
            .download_archive_with_options(
                4034806,
                "e13b7d119b",
                "123456--abc123def456",
                resolution,
                &options_dest,
                ArchiveDownloadOptions::default(),
            )
            .await
            .unwrap_err();
        let options_message = options_error.to_string();
        assert!(options_message.contains("unsupported EH archive resolution"));
        assert!(options_message.contains("780x, 980x, 1280x, original"));
    }

    assert!(server.received_requests().await.unwrap().is_empty());
}
```

`ArchiveDownloadOptions` is already imported at the top of the integration test.

- [ ] **Step 4: Add config serde acceptance/rejection tests**

Add these tests to `src/config.rs`:

```rust
    #[test]
    fn test_eh_archive_resolutions_accept_all_supported_values() {
        for resolution in ["780x", "980x", "1280x", "original"] {
            let json = format!(
                r#"{{"subscription_resolution":"{resolution}","download_resolution":"{resolution}"}}"#
            );
            let config: EhentaiConfig = serde_json::from_str(&json).unwrap();
            assert_eq!(config.subscription_resolution, resolution);
            assert_eq!(config.download_resolution, resolution);
        }
    }

    #[test]
    fn test_eh_archive_resolution_fields_reject_donor_and_unknown_values() {
        for field in ["subscription_resolution", "download_resolution"] {
            for resolution in ["1600x", "2400x", "bogus", ""] {
                let json = format!(r#"{{"{field}":"{resolution}"}}"#);
                let error = serde_json::from_str::<EhentaiConfig>(&json).unwrap_err();
                let message = error.to_string();
                assert!(message.contains("unsupported EH archive resolution"));
                assert!(message.contains("780x, 980x, 1280x, original"));
            }
        }
    }
```

- [ ] **Step 5: Run the public-boundary tests and confirm RED**

Run:

```powershell
cargo test -p eh_client --test integration prepare_archive_download_rejects_unsupported_resolution_before_network -- --nocapture
cargo test -p eh_client --test integration archive_key_downloads_reject_unsupported_resolution_before_network -- --nocapture
cargo test -p pixivbot --bin pixivbot eh_archive_resolution -- --nocapture
```

Expected: both integration tests FAIL because `prepare_archive_download` currently performs a GET and the archiver-key methods currently perform a POST before validating; the config test FAILS because ordinary `String` deserialization currently accepts donor/unknown values.

- [ ] **Step 6: Enforce validation before all public client network paths**

Make validation the first executable line in `prepare_archive_download`, before `fetch_archiver_page`:

```rust
    pub async fn prepare_archive_download(
        &self,
        gid: u64,
        token: &str,
        resolution: &str,
    ) -> Result<ArchiveDownloadRequest> {
        validate_archive_resolution(resolution)?;
        let (archiver_gid, archiver_token, archiver_html) =
            self.fetch_archiver_page(gid, token).await?;
```

Make validation the first executable line in `download_archive_with_options`, before constructing a request that can be POSTed:

```rust
    pub async fn download_archive_with_options(
        &self,
        gid: u64,
        token: &str,
        archiver_key: &str,
        resolution: &str,
        dest: &Path,
        options: ArchiveDownloadOptions,
    ) -> Result<u64> {
        validate_archive_resolution(resolution)?;
        let request = ArchiveDownloadRequest::from_archiver_key(
            &self.base_url,
            gid,
            token,
            archiver_key,
            resolution,
        );
        self.download_archive_with_request_and_options(&request, dest, options)
            .await
    }
```

Do not change `ArchiveDownloadRequest`, add an error enum variant, or teach the parser/redirect code about H@H.

- [ ] **Step 7: Deserialize both config fields through the canonical validator**

Update both attributes:

```rust
    #[serde(
        default = "default_eh_subscription_resolution",
        deserialize_with = "deserialize_eh_archive_resolution"
    )]
    pub subscription_resolution: String,
    #[serde(
        default = "default_eh_download_resolution",
        deserialize_with = "deserialize_eh_archive_resolution"
    )]
    pub download_resolution: String,
```

Add this deserializer next to `deserialize_nonzero_usize`:

```rust
fn deserialize_eh_archive_resolution<'de, D>(
    deserializer: D,
) -> std::result::Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let resolution = String::deserialize(deserializer)?;
    eh_client::client::validate_archive_resolution(&resolution)
        .map_err(serde::de::Error::custom)?;
    Ok(resolution)
}
```

The default functions remain unchanged at `1280x`; no `Config::load` post-validation and no new enum are needed.

- [ ] **Step 8: Narrow client/parser API docs and parser tests without changing parser behavior**

Update `prepare_archive_download`, `download_archive`, and `download_archive_with_options` rustdoc to list exactly `780x`, `980x`, `1280x`, and `original`, and state that unsupported values return `Error::Other` before HTTP.

Replace the parser API wording with:

```rust
/// `resolution` is expected to have passed the client request-boundary
/// validator: `"original"` selects `dltype=org`, while supported resamples
/// (`"780x"`, `"980x"`, and `"1280x"`) select the direct `dltype=res` form.
/// The separate H@H Downloader form and table are intentionally ignored.
```

Make these exact test-input updates in `eh_client/src/parser.rs` while leaving `resolution_dltype`, form parsing, cost parsing, estimated-size parsing, and redirect parsing unchanged:

```rust
        for resolution in ["780x", "980x", "1280x"] {
            let form = parse_archiver_form(html, resolution).expect("should parse resample form");
            assert_eq!(
                form.action,
                "https://exhentai.org/archiver.php?gid=4034806&token=res123def0"
            );
            assert!(form
                .fields
                .contains(&("res_sentinel".to_string(), "resample-only".to_string())));
            assert!(form
                .fields
                .contains(&("dltype".to_string(), "res".to_string())));
            assert!(!form.fields.iter().any(|(name, _)| name == "hathdl_xres"));
        }
```

Rename `test_parse_archiver_form_uses_resample_for_all_non_original_resolutions` to `test_parse_archiver_form_uses_resample_for_supported_resolutions` and replace its donor assertions with:

```rust
        for resolution in ["780x", "980x", "1280x"] {
            let form = parse_archiver_form(html, resolution)
                .expect("supported resolution should use generic resample form");
            assert!(form
                .fields
                .contains(&("res_sentinel".to_string(), "resample-only".to_string())));
            assert_eq!(
                parse_archive_download_cost(html, resolution),
                DownloadCost::Gp(218)
            );
        }
```

Also narrow the H@H-ignored cost loop to `["780x", "980x", "1280x"]`, replace the `1600x` assertion in `test_hathdl_forms_do_not_affect_direct_resamples` with `980x`, and replace the `1600x` size assertion in `test_parse_archive_download_estimated_size_ignores_hathdl_table` with `1280x`.

- [ ] **Step 9: Correct public configuration and archiver reference documentation**

Replace the stale resolution comments above the two fields in `src/config.rs` with:

```rust
    /// Direct archive resolution for subscription downloads. Supported values:
    /// `"780x"`, `"980x"`, `"1280x"`, and `"original"`. Default: `"1280x"`.
    /// Donor resolutions require the separate H@H Downloader workflow and are rejected.
    #[serde(
        default = "default_eh_subscription_resolution",
        deserialize_with = "deserialize_eh_archive_resolution"
    )]
    pub subscription_resolution: String,
    /// Direct archive resolution for `/edl`; uses the same supported values.
    #[serde(
        default = "default_eh_download_resolution",
        deserialize_with = "deserialize_eh_archive_resolution"
    )]
    pub download_resolution: String,
```

Use this resolution text in `config.toml.example`:

```toml
# # Direct archive resolution for subscription downloads. Supported values only:
# #   "780x", "980x", "1280x" (resamples), or "original".
# # Donor resolutions require a separate H@H Downloader workflow and are rejected.
# subscription_resolution = "1280x"
# # Direct archive resolution for /edl downloads; same supported values.
# download_resolution = "1280x"
```

Replace the stale mapping/parser-strategy section of `docs/eh-archiver-page-reference.md` with this accurate boundary contract:

```markdown
## Supported Direct Archive Resolution Mapping

The direct archive workflow posts only the two generic forms. The separate
`form#hathdl_form` and its per-resolution table belong to the H@H Downloader
workflow and are not posted or used for direct archive cost/size selection.

| Config `resolution` | Direct form POSTed | Cost and estimate source |
|---|---|---|
| `original` | left form (`dltype=org`) | Original form's cost and nearby estimate |
| `780x` | right form (`dltype=res`) | Generic resample form's cost and nearby estimate |
| `980x` | right form (`dltype=res`) | Generic resample form's cost and nearby estimate |
| `1280x` | right form (`dltype=res`) | Generic resample form's cost and nearby estimate |

`1600x`, `2400x`, an empty string, and every unknown value are unsupported.
Configuration deserialization rejects them, and `EhClient` validates them before
any GET or POST. Supporting donor resolutions would require implementing the
separate H@H Downloader workflow and is outside the direct archive API.

## Parser Strategy

`parse_archive_download_cost(html, resolution) -> DownloadCost`:

1. Select `dltype=org` for `original`; select `dltype=res` for each supported
   resample resolution.
2. If the selected resample has the unlocked marker, return
   `DownloadCost::Unlocked`.
3. Parse the selected generic form's cost as `Free`, numeric GP,
   `Insufficient`, `Unavailable`, or `Unknown`.
4. Ignore H@H forms/tables for direct archive selection.

`parse_archive_download_estimated_size(html, resolution) -> Option<u64>` uses
the same generic-form selection and rounds displayed decimal MiB upward. A
missing, malformed, or zero estimate does not block the existing size gate.
```

Update the “When POST Charges GP” section so it says valid `prepare_archive_download` calls perform safe GETs, unsupported values fail before those GETs, and only `download_archive_with_request` performs the GP-spending POST.

- [ ] **Step 10: Run focused GREEN client, parser, and config tests**

Run:

```powershell
cargo test -p eh_client --lib archive_resolution -- --nocapture
cargo test -p eh_client --lib parser::tests:: -- --nocapture
cargo test -p eh_client --test integration prepare_archive_download -- --nocapture
cargo test -p pixivbot --bin pixivbot eh_archive_resolution -- --nocapture
cargo test -p pixivbot --bin pixivbot eh_archive_download_concurrency -- --nocapture
```

Expected: every command PASS. The validator/config tests accept all four supported values; both config fields reject donor/unknown/empty values with the supported-values message; the integration test records zero HTTP requests for invalid inputs; existing `1280x` and `original` flows still pass.

- [ ] **Step 11: Run diagnostics, documentation scans, and the Task 3 read-only review boundary**

Tool calls:

```text
lsp_diagnostics(filePath="C:\Users\hugefiver\source\pixivbot\eh_client\src\client.rs", severity="all")
lsp_diagnostics(filePath="C:\Users\hugefiver\source\pixivbot\eh_client\src\parser.rs", severity="all")
lsp_diagnostics(filePath="C:\Users\hugefiver\source\pixivbot\src\config.rs", severity="all")
```

Expected: no new error or warning diagnostics.

Run:

```powershell
rg -n '1600x|2400x|donor|hathdl_xres' "src/config.rs" "config.toml.example" "eh_client/src/client.rs" "eh_client/src/parser.rs" "eh_client/tests/integration.rs" "docs/eh-archiver-page-reference.md"
git diff --check -- "eh_client/src/client.rs" "eh_client/src/parser.rs" "eh_client/tests/integration.rs" "src/config.rs" "config.toml.example" "docs/eh-archiver-page-reference.md"
git diff -- "eh_client/src/client.rs" "eh_client/src/parser.rs" "eh_client/tests/integration.rs" "src/config.rs" "config.toml.example" "docs/eh-archiver-page-reference.md"
```

Expected: remaining donor/H@H mentions either document observed page content that the direct parser ignores or describe explicit rejection/non-support; no supported-values list contains `1600x` or `2400x`; no whitespace errors; no H@H POST path, redirect parsing, dependency, or error-enum expansion appears.

## Final integration and verification

- [ ] **Run formatting, then re-run focused tests affected by formatting**

Run:

```powershell
cargo fmt --all
cargo test -p pixivbot --bin pixivbot main_download_claim_prioritizes_recent_fifo_then_old_lifo -- --nocapture
cargo test -p pixivbot --bin pixivbot archive_cost_policy -- --nocapture
cargo test -p pixivbot --bin pixivbot gp_cost_exceeds_policy -- --nocapture
cargo test -p eh_client --test integration prepare_archive_download -- --nocapture
cargo test -p pixivbot --bin pixivbot eh_archive_resolution -- --nocapture
```

Expected: formatting completes and every focused command PASS.

- [ ] **Run LSP diagnostics over every changed Rust file**

Tool calls:

```text
lsp_diagnostics(filePath="C:\Users\hugefiver\source\pixivbot\src\db\repo\eh_download_queue.rs", severity="all")
lsp_diagnostics(filePath="C:\Users\hugefiver\source\pixivbot\src\scheduler\eh_engine.rs", severity="all")
lsp_diagnostics(filePath="C:\Users\hugefiver\source\pixivbot\src\config.rs", severity="all")
lsp_diagnostics(filePath="C:\Users\hugefiver\source\pixivbot\eh_client\src\client.rs", severity="all")
lsp_diagnostics(filePath="C:\Users\hugefiver\source\pixivbot\eh_client\src\parser.rs", severity="all")
```

Expected: zero error diagnostics and no new warning diagnostics.

- [ ] **Run the repository CI target when `make` exists, otherwise run its exact PowerShell-equivalent commands**

Run this PowerShell block:

```powershell
$makeCommand = Get-Command make -ErrorAction SilentlyContinue
if ($null -ne $makeCommand) {
    make ci
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
} else {
    cargo fmt --all -- --check
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

    $previousRustFlags = $env:RUSTFLAGS
    $env:RUSTFLAGS = "-Dwarnings"
    cargo clippy --workspace --all-targets -- -D warnings
    $clippyExit = $LASTEXITCODE
    if ($null -eq $previousRustFlags) {
        Remove-Item Env:RUSTFLAGS -ErrorAction SilentlyContinue
    } else {
        $env:RUSTFLAGS = $previousRustFlags
    }
    if ($clippyExit -ne 0) { exit $clippyExit }

    cargo check --workspace --all-targets
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

    cargo test --workspace --all-targets
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

    cargo build --release --workspace --features ffmpeg-codec
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
}
```

Expected: rustfmt clean; Clippy clean with warnings denied; workspace check and all-target tests PASS; release workspace build with `ffmpeg-codec` PASS when FFmpeg development libraries and `pkg-config` are already present. If the final build alone fails because those native prerequisites are absent, record the exact environment blocker, do not install anything, and do not claim the release-build gate passed; all preceding Rust checks must still be green.

- [ ] **Perform real-surface QA without a live EH account or GP-spending endpoint**

Use the existing real boundaries rather than invoking the bot with secrets:

1. The Task 1 repository test must execute against `tests_helpers::setup_test_db()` and prove actual generated SQLite ordering/CAS behavior.
2. The two worker tests must inspect wiremock request counts and the real SQLite ledger table, proving a safe Archiver GET may occur but no archive POST or ledger row occurs on `Reject`.
3. The invalid-resolution integration test must assert `MockServer::received_requests()` is empty, proving failure before network.
4. The serde tests must deserialize both actual `EhentaiConfig` fields and assert the supported-values message.
5. Do not run against a live E-Hentai/ExHentai endpoint and do not read `config.toml`.

- [ ] **Run final read-only scope and whitespace checks**

Run:

```powershell
git diff --check
git status --short
git diff --stat
git diff -- "src/db/repo/eh_download_queue.rs" "src/scheduler/eh_engine.rs" "src/config.rs" "eh_client/src/client.rs" "eh_client/src/parser.rs" "eh_client/tests/integration.rs" "config.toml.example" "docs/eh-archiver-page-reference.md"
```

Expected: only the eight mapped implementation files plus the already-untracked design spec and this plan are present. There is no migration, entity/schema, dependency/lockfile, H@H workflow, background-order, or unrelated refactor diff.

- [ ] **Trigger the final Oracle review required by the design**

The orchestrator must request one read-only final Oracle review after all available checks are green. Provide the authoritative spec, this plan path, the complete current diff, focused-test evidence, CI-equivalent evidence, and any native FFmpeg build blocker. Require the review to check these concrete risks: CASE ordering and cutoff equality, preservation of both claim CAS filters, stale-worker policy-failure guards, no retry/attempt consumption, no ledger/POST on `Reject`, continued `Defer` behavior, and pre-network resolution rejection. Resolve every correctness, data-loss, concurrency, or GP-spend blocker before execution handoff; a timeout or partial review is not a pass.

## Handoff and version-control boundary

- Implementation order is Task 1 -> Task 2 -> Task 3 -> final verification -> final Oracle review.
- No implementation subagent performs git writes. After final Oracle approval, the orchestrator creates a new branch, commits the verified scope, and pushes it to `origin` under the user's explicit authorization.
- Suggested implementation commit title: `fix: enforce EH queue and archive policies`.
