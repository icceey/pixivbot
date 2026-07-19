# PR #112 review fixes design

## Context

PR #112 adds an E-Hentai archive GP guard. Review found three correctness gaps:

1. `prepare_archive_download()` parses the cost for the configured resolution but, on pages without an `archiver_key`, `parse_archiver_form()` returns the first archive form. A resample request can therefore be approved using the resample cost and POST the original form.
2. Concurrent paid downloads independently read the rolling GP total. Because `gp_cost` is recorded only after a download completes, multiple workers can pass the same remaining-budget check and collectively exceed `gp_rate_limit`.
3. `get_eh_gp_cost_in_window()` filters by queue status. A row that already spent GP but is returned to `pending` for a missing-ZIP redownload is omitted from the rolling total.

The application runs one main EH download worker and an optional concurrent background worker in one process. They share a repository and EH account. The current queue schema records GP cost only after a successful archive download; introducing a crash-safe cross-process GP ledger is outside this correction.

## Goals

- Ensure the archive form POSTed by `prepare_archive_download()` has the same original/resample class used to parse and approve its cost.
- Prevent concurrent paid archive downloads in this process from checking the same stale GP total.
- Keep already-recorded GP spend in the rolling total regardless of the queue row's current workflow status.
- Preserve concurrency for free/unlocked downloads and when the rolling GP budget is disabled.

## Non-goals

- Do not add a new GP reservation or spend-ledger table.
- Do not provide cross-process synchronization for multiple bot instances sharing one EH account.
- Do not change archive retry policy, byte-rate limiting, or per-archive GP thresholds.
- Do not attempt to infer whether EH charged GP when a POST succeeds but redirect, download, ZIP validation, or database marking later fails.

## Design

### Resolution-matched archive forms

Add a parser entry point that selects an archive form by the target `dltype`. The target is derived from the same rule used by cost parsing:

- `resolution == "original"` or an empty resolution selects `dltype=org`.
- Every configured resample resolution selects `dltype=res`.

The parser scans archive forms, parses each form's inputs, and returns only the form whose `dltype` equals the target. It must not fall back to the first form when the target is absent. `prepare_archive_download()` uses this selected form, so the form fields and parsed cost describe the same operation. A shared private resolution-to-`dltype` helper keeps form selection and cost selection from diverging.

`apply_resolution_to_form_data()` continues to set the requested H@H resolution and submit label, but it is not responsible for converting an original form into a resample form.

### Serialized rolling-budget checks

Add one process-wide asynchronous mutex for GP-budgeted archive spending. `check_archive_cost_or_defer()` returns a private permit with `Proceed`:

- For `DownloadCost::Gp(_)` when `gp_rate_limit > 0`, acquire the mutex before reading `get_eh_gp_cost_in_window()`.
- For free/unlocked downloads, non-GP outcomes, or `gp_rate_limit == 0`, return an empty permit without locking.
- If a guard rejects the request, release any acquired lock when returning `Defer`.
- If the request proceeds, retain the permit across the archive POST, redirect/download/ZIP validation, and the database method that writes `gp_cost` and `completed_at`.

The main download worker carries the permit in its download result tuple and explicitly drops it after `mark_eh_download_downloaded()` succeeds. `BackgroundDownloadOutcome::Completed` carries the same permit across `download_claimed()` into `process_claimed()`, where it is dropped after `mark_eh_background_download_downloaded()` succeeds. Errors release the permit through normal drop semantics.

This serializes only paid downloads subject to the rolling budget. It covers independently constructed main and background workers because the mutex is process-wide.

### Status-independent GP aggregation

Change `Repo::get_eh_gp_cost_in_window(hours)` to sum `gp_cost` for every row whose `completed_at` is within the cutoff, without filtering by queue status. Rows with `completed_at = NULL` remain excluded by the cutoff predicate, and zero-cost rows do not affect the sum.

`completed_at` remains the spend-window timestamp already written at successful download completion. Returning a row to `pending` must not clear its existing `gp_cost` or `completed_at` when the row represents a previously completed paid download.

## Data flow

1. The worker fetches the archiver page and determines the target original/resample class.
2. The client parses both the cost and the POST form for that same class.
3. For a paid request with a configured rolling budget, the guard acquires the GP mutex and then reads status-independent spend in the configured window.
4. An over-budget request is deferred without POSTing; an allowed request retains the permit and POSTs the selected form.
5. After the ZIP is downloaded and validated, the worker records `gp_cost` and `completed_at` while still holding the permit.
6. The permit is released, allowing the next paid request to observe the newly recorded spend.

## Error handling

- Missing target forms produce a parse error rather than POSTing a mismatched form.
- Budget or per-archive rejections keep existing non-error defer behavior and do not consume retry attempts.
- Download and database errors retain existing retry behavior; the GP permit is always released when the operation returns.
- A POST that spends GP followed by a later failure can still be absent from the rolling total. Solving this requires a durable spend ledger with separate attempted/confirmed semantics and is not part of these three review fixes.

## Testing

- Parser tests use a two-form page with distinct sentinel fields and verify original/empty resolutions select `org`, resample resolutions select `res`, and a missing target form returns `None`.
- Client coverage verifies a `1280x` request returns the resample cost and builds the resample POST body, including `dltype=res` and the requested H@H resolution.
- A deterministic concurrency test obtains the first paid permit, starts a second budget check, proves it cannot complete while the first permit is held, records the first spend, releases the permit, and verifies the second check defers.
- Worker tests verify the permit remains held through the corresponding main/background database mark and concurrent background paid work cannot issue more POSTs than the rolling budget allows.
- Repository tests verify a positive-GP row remains counted after its status returns to `pending`; records outside the window and records without `completed_at` remain excluded.
- Run focused `eh_client` parser/client tests and `pixivbot` scheduler/repository tests, then `make ci` as required for Rust changes.

## Self-review

- Placeholder scan: no placeholders or incomplete requirements.
- Internal consistency: the same original/resample selection drives cost and form parsing; the permit spans budget read through spend persistence.
- Scope: changes are limited to the three review findings and direct regression coverage.
- Ambiguity: process-local serialization and the post-success/pre-persistence residual risk are explicitly bounded.
