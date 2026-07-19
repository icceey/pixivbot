# PR #112 Review Fixes Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix the archive form/cost mismatch, serialize concurrent paid GP-budget checks until spend is persisted, and count persisted GP independently of queue status.

**Architecture:** `eh_client` will use one private resolution-to-`dltype` mapping for both form and cost selection. The scheduler will return a private RAII permit from the GP guard and retain it through the main/background persistence call, while the repository will define the spend window solely by `completed_at`.

**Tech Stack:** Rust 1.94, Tokio `Mutex`/`OwnedMutexGuard`, SeaORM, Wiremock, Cargo, GNU Make

---

## Constraints and file map

- Approved spec: `docs/superpowers/specs/2026-07-19-pr-112-review-fixes-design.md`.
- Modify `eh_client/src/parser.rs`: shared resolution mapping, matched form parser, parser tests.
- Modify `eh_client/src/client.rs`: use matched form, client request regression test.
- Modify `src/db/repo/eh_download_queue.rs`: status-independent GP query.
- Modify `src/scheduler/eh_engine.rs`: GP permit, worker lifetime wiring, repository and concurrency tests.
- Do not add dependencies, migrations, cross-process locking, or a GP ledger.
- Do not read `/config.toml`.
- Do not execute Git writes. Commit steps below only report suggested boundaries.

## Task 1: Match the POST form to the parsed resolution cost

**Files:**
- Modify: `eh_client/src/parser.rs:134-166,276-330,489+`
- Modify: `eh_client/src/client.rs:394-436,1154+`
- Regression: `eh_client/tests/integration.rs::test_prepare_and_download_archive_form_flow`

- [ ] **Step 1: Add failing two-form parser tests**

In `eh_client/src/parser.rs`, replace the single-form parser test with a fixture whose forms have observable sentinels:

```rust
const ARCHIVER_TWO_DISTINCT_FORMS: &str = r#"
<form method="post" action="/archiver.php?selected=org">
  <input type="hidden" name="dltype" value="org" />
  <input type="hidden" name="sentinel" value="original-form" />
  <input type="submit" name="dlcheck" value="Download Original Archive" />
</form>
<form method="post" action="/archiver.php?selected=res">
  <input type="hidden" name="dltype" value="res" />
  <input type="hidden" name="sentinel" value="resample-form" />
  <input type="submit" name="dlcheck" value="Download Resample Archive" />
</form>
"#;

#[test]
fn test_parse_archiver_form_selects_requested_dltype() {
    let original = parse_archiver_form(ARCHIVER_TWO_DISTINCT_FORMS, "original").unwrap();
    assert_eq!(original.action, "/archiver.php?selected=org");
    assert!(original.fields.contains(&("sentinel".into(), "original-form".into())));

    let empty = parse_archiver_form(ARCHIVER_TWO_DISTINCT_FORMS, "").unwrap();
    assert_eq!(empty.action, "/archiver.php?selected=org");

    let resample = parse_archiver_form(ARCHIVER_TWO_DISTINCT_FORMS, "1280x").unwrap();
    assert_eq!(resample.action, "/archiver.php?selected=res");
    assert!(resample.fields.contains(&("dltype".into(), "res".into())));
    assert!(resample.fields.contains(&("sentinel".into(), "resample-form".into())));
}

#[test]
fn test_parse_archiver_form_rejects_missing_requested_dltype() {
    let only_original = r#"
    <form action="/archiver.php" method="post">
      <input type="hidden" name="dltype" value="org" />
      <input type="submit" name="dlcheck" value="Download Original Archive" />
    </form>"#;
    assert!(parse_archiver_form(only_original, "1280x").is_none());
}
```

- [ ] **Step 2: Run parser tests and capture RED**

```powershell
cargo test -p eh_client test_parse_archiver_form -- --nocapture
```

Expected: compilation fails because `parse_archiver_form` currently accepts only `html`.

- [ ] **Step 3: Add a failing client request consistency test**

In the private `tests` module in `eh_client/src/client.rs`, import Wiremock and add `test_prepare_archive_download_1280x_uses_resample_form_and_cost`. Mock:

- `GET /g/4034806/abc123def0/` returning an archiver link;
- `GET /archiver.php?gid=4034806&token=abc123def0` returning original `8,800 GP` and resample `218 GP` forms with the same sentinels as above.

Build with `EhClientBuilder::new().base_url(&server.uri()).build()`, call:

```rust
let request = client
    .prepare_archive_download(4034806, "abc123def0", "1280x")
    .await
    .unwrap();

assert_eq!(request.cost, parser::DownloadCost::Gp(218));
assert_eq!(request.action_url, format!("{}/archiver.php?selected=res", server.uri()));
assert!(request.form_data.contains(&("dltype".into(), "res".into())));
assert!(request.form_data.contains(&("sentinel".into(), "resample-form".into())));
assert!(request.form_data.contains(&("hathdl_xres".into(), "1280".into())));
assert!(!request.form_data.contains(&("sentinel".into(), "original-form".into())));
```

The fixture must not contain an `{integer}--{hex}` archiver key.

- [ ] **Step 4: Run the client test and capture behavioral RED**

```powershell
cargo test -p eh_client test_prepare_archive_download_1280x_uses_resample_form_and_cost -- --nocapture
```

Expected: cost is `Gp(218)` but action/form fields come from the original form.

- [ ] **Step 5: Implement one resolution mapping and matched form selection**

Add before `parse_archiver_form`:

```rust
fn resolution_dltype(resolution: &str) -> &'static str {
    if resolution.is_empty() || resolution == "original" {
        "org"
    } else {
        "res"
    }
}
```

Change the parser signature and selection condition:

```rust
pub fn parse_archiver_form(html: &str, resolution: &str) -> Option<ArchiverForm> {
    let target_dltype = resolution_dltype(resolution);
    for cap in archiver_form_re().captures_iter(html) {
        // Keep existing action and field parsing.
        // Return only a form containing the exact target dltype.
        if fields
            .iter()
            .any(|(name, value)| name == "dltype" && value == target_dltype)
        {
            return Some(ArchiverForm { action, fields });
        }
    }
    None
}
```

In `parse_archive_download_cost`, use the same helper:

```rust
let target_dltype = resolution_dltype(resolution);
if target_dltype == "res" && unlocked_resample_re().is_match(html) {
    return DownloadCost::Unlocked;
}
```

Retain the one-cost simplified-fixture fallback, and replace the existing local target calculation with `target_dltype`.

- [ ] **Step 6: Use the matched parser from the client**

```rust
let form = parser::parse_archiver_form(&archiver_html, resolution).ok_or_else(|| {
    Error::Parse("archiver download form not found in archiver.php response".into())
})?;
```

Do not make `apply_resolution_to_form_data` convert `org` to `res`; the selected form must already be correct.

- [ ] **Step 7: Run Task 1 GREEN tests**

```powershell
cargo test -p eh_client test_parse_archiver_form -- --nocapture
cargo test -p eh_client test_parse_archive_download_cost -- --nocapture
cargo test -p eh_client test_prepare_archive_download_1280x_uses_resample_form_and_cost -- --nocapture
cargo test -p eh_client test_prepare_and_download_archive_form_flow -- --nocapture
```

Expected: all pass; original/empty select `org`, `1280x` selects `res`, and missing `res` fails closed.

- [ ] **Step 8: Report suggested boundary; do not commit**

Suggested files: `eh_client/src/parser.rs`, `eh_client/src/client.rs`. Suggested message: `fix: align EH archive form with requested resolution`.

## Task 2: Count persisted GP spend regardless of workflow status

**Files:**
- Modify: `src/db/repo/eh_download_queue.rs:686-707`
- Test: `src/scheduler/eh_engine.rs:4757-4821`

- [ ] **Step 1: Add explicit status/time aggregation tests**

Factor the existing ActiveModel setup into a test helper:

```rust
async fn insert_gp_spend(
    repo: &Repo,
    gid: i64,
    status: &str,
    gp_cost: i64,
    completed_at: Option<chrono::NaiveDateTime>,
) {
    let now = Local::now().naive_local();
    eh_download_queue::ActiveModel {
        chat_id: Set(-100),
        gid: Set(gid),
        token: Set(format!("gp-{gid}")),
        title: Set("GP spend".into()),
        telegraph: Set(false),
        source: Set(SOURCE_DIRECT.into()),
        status: Set(status.into()),
        file_size: Set(1000),
        gp_cost: Set(gp_cost),
        error: Set(None),
        retry_count: Set(0),
        created_at: Set(now),
        started_at: Set(completed_at),
        completed_at: Set(completed_at),
        zip_path: Set(None),
        telegraph_url: Set(None),
        next_retry_at: Set(None),
        ..Default::default()
    }
    .insert(repo.db()).await.unwrap();
}
```

Add three tests:

```rust
#[tokio::test]
async fn test_get_eh_gp_cost_in_window_counts_pending_completed_spend() {
    let repo = tests_helpers::setup_test_db().await.unwrap();
    let now = Local::now().naive_local();
    insert_gp_spend(&repo, 1, STATUS_PENDING, 100, Some(now)).await;
    insert_gp_spend(&repo, 2, STATUS_DONE, 250, Some(now)).await;
    assert_eq!(repo.get_eh_gp_cost_in_window(24).await.unwrap(), 350);
}

#[tokio::test]
async fn test_get_eh_gp_cost_in_window_excludes_null_completed_at() {
    let repo = tests_helpers::setup_test_db().await.unwrap();
    insert_gp_spend(&repo, 1, STATUS_PENDING, 500, None).await;
    assert_eq!(repo.get_eh_gp_cost_in_window(24).await.unwrap(), 0);
}
```

Keep the existing 48-hour-old record test, rewritten through `insert_gp_spend`.

- [ ] **Step 2: Run aggregation tests and capture RED**

```powershell
cargo test -p pixivbot test_get_eh_gp_cost_in_window -- --nocapture
```

Expected: pending + done returns 250 instead of 350; NULL and old records remain excluded.

- [ ] **Step 3: Remove only the status predicate**

Implement:

```rust
let result = eh_download_queue::Entity::find()
    .filter(eh_download_queue::Column::CompletedAt.gte(cutoff))
    .all(&self.db)
    .await
    .context("Failed to fetch eh gp cost in window")?;
```

Keep summing `gp_cost`; do not change byte accounting or add a migration.

- [ ] **Step 4: Run Task 2 GREEN tests**

```powershell
cargo test -p pixivbot test_get_eh_gp_cost_in_window -- --nocapture
```

Expected: pending completed spend is counted; NULL and old timestamps are excluded.

- [ ] **Step 5: Report suggested boundary; do not commit**

Suggested files: `src/db/repo/eh_download_queue.rs` and its test hunks in `src/scheduler/eh_engine.rs`. Suggested message: `fix: count EH GP spend independently of queue status`.

## Task 3: Serialize paid budget checks through GP persistence

**Files:**
- Modify: `src/scheduler/eh_engine.rs:1-172,244-365,1040-1157,4495+`

- [ ] **Step 1: Add a deterministic failing permit test**

Add `test_check_archive_cost_serializes_paid_budget_until_spend_is_recorded`:

```rust
let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
let mut config = make_config();
config.max_archive_gp_cost = 300;
config.gp_rate_limit = 300;
let config = Arc::new(config);

let first = check_archive_cost_or_defer(
    repo.as_ref(), config.as_ref(), &DownloadCost::Gp(200)
).await.unwrap();
assert!(matches!(first, ArchiveCostCheck::Proceed { .. }));

let barrier = Arc::new(tokio::sync::Barrier::new(2));
let task_barrier = Arc::clone(&barrier);
let task_repo = Arc::clone(&repo);
let task_config = Arc::clone(&config);
let second = tokio::spawn(async move {
    task_barrier.wait().await;
    check_archive_cost_or_defer(
        task_repo.as_ref(), task_config.as_ref(), &DownloadCost::Gp(200)
    ).await.unwrap()
});
barrier.wait().await;
for _ in 0..16 { tokio::task::yield_now().await; }
assert!(!second.is_finished(), "second paid check must wait for the first permit");

insert_gp_spend(
    repo.as_ref(), 7001, STATUS_PENDING, 200, Some(Local::now().naive_local())
).await;
drop(first);
match second.await.unwrap() {
    ArchiveCostCheck::Defer { reason, .. } => assert!(reason.contains("200 + 200 > 300")),
    ArchiveCostCheck::Proceed { .. } => panic!("second request exceeded the GP window"),
}
```

Also add direct assertions that `Proceed` contains no guard for `DownloadCost::Free` and for `DownloadCost::Gp(200)` when `gp_rate_limit=0`; do not use timing assertions for these bypass cases.

- [ ] **Step 2: Run the permit test and capture RED**

```powershell
cargo test -p pixivbot test_check_archive_cost_serializes_paid_budget_until_spend_is_recorded -- --nocapture
```

Expected: compilation fails until `Proceed` carries a permit, or the second check finishes before the first result is dropped.

- [ ] **Step 3: Define the process-wide lock and private permit**

Update imports and define:

```rust
use std::sync::{Arc, LazyLock};
use tokio::sync::{mpsc, Mutex, OwnedMutexGuard};

static EH_GP_BUDGET_LOCK: LazyLock<Arc<Mutex<()>>> =
    LazyLock::new(|| Arc::new(Mutex::new(())));

#[derive(Default)]
struct ArchiveGpPermit {
    _guard: Option<OwnedMutexGuard<()>>,
}

enum ArchiveCostCheck {
    Proceed { permit: ArchiveGpPermit },
    Defer { delay_secs: i64, reason: String },
}
```

The underscore-prefixed field intentionally owns the RAII guard without being read in production, avoiding a `dead_code` warning under `-Dwarnings`. Tests may inspect `permit._guard.is_none()` because they are in the child module; production callers must only retain/drop the permit.

Remove `pub` from both `ArchiveCostCheck` and `check_archive_cost_or_defer`. They are used only in this module and its child test module. Keeping either interface public while it exposes the private permit would trigger `private_interfaces`, which fails the repository's `-Dwarnings` CI gate.

- [ ] **Step 4: Acquire before the rolling-total read and return the permit**

After the byte limit check and before `get_eh_gp_cost_in_window`:

```rust
let permit = if cost.gp_amount().is_some() && config.gp_rate_limit > 0 {
    ArchiveGpPermit {
        _guard: Some(Arc::clone(&*EH_GP_BUDGET_LOCK).lock_owned().await),
    }
} else {
    ArchiveGpPermit::default()
};
```

Keep existing GP-window and per-archive decisions. Return `ArchiveCostCheck::Proceed { permit }`. Every `Defer` or `?` after acquisition drops the local permit automatically.

- [ ] **Step 5: Carry the permit through the main worker mark**

Change the result tuple to `(file_size, gp_cost, permit)`. Capture:

```rust
let permit = match check_archive_cost_or_defer(...).await? {
    ArchiveCostCheck::Proceed { permit } => permit,
    ArchiveCostCheck::Defer { delay_secs, reason } => {
        self.repo.defer_eh_download(entry.id, STATUS_PENDING, delay_secs).await?;
        return Ok(());
    }
};
```

The unauthenticated branch returns `ArchiveGpPermit::default()`. After the existing mark call:

```rust
self.repo
    .mark_eh_download_downloaded(entry.id, file_size as i64, &zip_path_str, gp_cost)
    .await?;
drop(permit);
```

- [ ] **Step 6: Carry the permit through the background worker mark**

Extend `BackgroundDownloadOutcome::Completed` with `permit: ArchiveGpPermit`. Make `download_claimed` return it alongside `file_size` and `gp_cost`. In `process_claimed`, destructure and release only after:

```rust
self.repo
    .mark_eh_background_download_downloaded(
        entry.id,
        file_size as i64,
        &zip_path.to_string_lossy(),
        gp_cost,
    )
    .await?;
drop(permit);
```

- [ ] **Step 7: Add a background concurrency HTTP regression**

Make `mock_eh_archiver_page_with_cost` match `gid` and `token` query parameters so two concurrent galleries cannot consume each other's fixture. Add `test_background_gp_rate_limit_allows_only_one_post` with:

- two distinct rows scheduled by `schedule_eh_background_download_from`;
- `background_download_concurrency=2`, `max_archive_size_mb=0`;
- both resample costs `218 GP`;
- `max_archive_gp_cost=218`, `gp_rate_limit=218`;
- valid mocked ZIP response;
- a `POST /archiver.php` mock with `.expect(1)`.

After one `tick()`, assert exactly one row has `STATUS_DOWNLOADED`, the other has `BACKGROUND_STATUS_PENDING`, `get_eh_gp_cost_in_window(24) == 218`, and recorded HTTP requests contain exactly one archive POST.

- [ ] **Step 8: Add a main/background cross-worker HTTP regression**

Add `test_main_and_background_gp_rate_limit_allows_only_one_post`. Use two distinct paid rows and query-specific gallery/archiver mocks. Leave one row for `EhDownloadWorker::tick()` and schedule the other with `schedule_eh_background_download_from` for `EhBackgroundDownloadWorker::tick()`. Construct the worker types independently with the same `Arc<Repo>`, client, config, and cache directory; do not inject a worker-local lock.

Use `max_archive_size_mb=0`, `max_archive_gp_cost=218`, `gp_rate_limit=218`, and a valid shared ZIP response. Mount the archive POST with `.expect(1)`, then run:

```rust
let (main_result, background_result) =
    tokio::join!(main_worker.tick(), background_worker.tick());
main_result.unwrap();
background_result.unwrap();
```

Assert exactly one row reaches `STATUS_DOWNLOADED`, the other remains pending/deferred for a later attempt, the rolling GP total is `218`, and recorded requests contain one `POST /archiver.php`. This test is required in addition to the background/background test: it verifies the process-wide permit is shared by the independently constructed worker types and that each path retains it through its own database mark before the competing path can re-read spend.

- [ ] **Step 9: Run Task 3 focused tests**

```powershell
cargo test -p pixivbot test_check_archive_cost -- --nocapture
cargo test -p pixivbot test_background_gp_rate_limit_allows_only_one_post -- --nocapture
cargo test -p pixivbot test_main_and_background_gp_rate_limit_allows_only_one_post -- --nocapture
cargo test -p pixivbot test_download_worker_gp -- --nocapture
cargo test -p pixivbot test_background_worker_gp -- --nocapture
```

Expected: all pass, with no sleeps and exactly one spending POST in the concurrent test.

- [ ] **Step 10: Report suggested boundary; do not commit**

Suggested file: permit/worker/test hunks in `src/scheduler/eh_engine.rs`. Suggested message: `fix: serialize EH paid archive budget checks`.

## Task 4: Final verification and acceptance

**Files:** all files listed above plus this plan and its spec.

- [ ] **Step 1: Format and run focused suites**

```powershell
cargo fmt --all
cargo test -p eh_client -- --nocapture
cargo test -p pixivbot test_get_eh_gp_cost_in_window -- --nocapture
cargo test -p pixivbot test_check_archive_cost -- --nocapture
cargo test -p pixivbot test_background_gp_rate_limit_allows_only_one_post -- --nocapture
```

Expected: all commands exit 0.

- [ ] **Step 2: Run the repository completion gate**

```powershell
make ci
```

Expected: formatting, clippy with warnings denied, check, tests, and release build all pass. If native FFmpeg requirements block the command, report the exact environment error and do not install software.

- [ ] **Step 3: Check diagnostics and patch integrity**

Run `lsp_diagnostics` with severity `all` on each modified Rust file, then:

```powershell
git diff --check
git status --short
```

Expected: no new diagnostics, no whitespace errors, and only the intended source/tests/spec/plan are modified.

- [ ] **Step 4: Final review**

Use `requesting-code-review`. Because this is concurrency-sensitive and cross-module, dispatch both Oracle and reviewer against the working-tree diff, fix verified blockers, rerun affected tests, and require unconditional approval.

- [ ] **Step 5: Report suggested commit sets; do not execute**

Report the three functional boundaries from Tasks 1–3 and one optional docs boundary (`docs: document PR 112 review fixes`). Do not stage, commit, push, tag, or reply to GitHub review threads without explicit user permission.

## Self-review

- Spec coverage: every review item maps to a separate RED/GREEN task and focused regression.
- Placeholder scan: no TBD/TODO/fill-in-later steps.
- Type consistency: `ArchiveGpPermit`, `ArchiveCostCheck::Proceed { permit }`, and `BackgroundDownloadOutcome::Completed { permit, .. }` are used consistently.
- Scope: no migration, dependency, config, or unrelated refactor is included.
