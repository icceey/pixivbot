# PR #112 GP attempt ledger design

## Context

PR #112 now serializes process-local GP budget checks until a successful archive download records its cost. Review comment `3610561804` identifies the remaining failure window: E-Hentai may charge GP when the archive POST is accepted, but redirect parsing, archive transfer, ZIP validation, rename, or queue persistence can fail afterward. The current queue row records `gp_cost` only after the entire download succeeds, so a retry can repeat a paid POST while the rolling budget omits the earlier attempt.

One queue row can have multiple paid attempts. Its single `gp_cost` and `completed_at` values cannot represent those attempts without either overwriting spend or assigning several charges one timestamp. The process mutex prevents concurrent stale reads but does not persist an external side effect after failure.

## Goals

- Persist every positive-GP archive attempt before its POST can be sent.
- Count failed, uncertain, and successful paid attempts in the rolling GP window.
- Preserve atomic process-local budget check-and-reserve behavior across main and background workers.
- Keep free and unlocked archive downloads outside the GP ledger.
- Preserve existing queue lifecycle, retry counts, and successful-download metadata.
- Preserve already-recorded GP spend across upgrades.

## Non-goals

- Do not reconcile local reservations with an authoritative E-Hentai billing API.
- Do not refund reservations when a request appears not to reach E-Hentai.
- Do not add cross-process locking or database-level distributed reservations.
- Do not prevent all retries of a paid gallery; retries remain subject to the rolling budget.
- Do not persist redirect URLs or make archive POSTs remotely idempotent.
- Do not add a reservation state machine that does not affect budget decisions.

## Approaches considered

### Reuse the queue row

Pre-writing or incrementing `eh_download_queue.gp_cost` has the smallest file footprint but cannot model repeated attempts correctly. Updating one timestamp extends old spend into later windows; retaining the old timestamp omits new spend; a later successful mark can overwrite the accumulated value. This approach is rejected.

### Record after a successful POST response

Splitting the client at the POST boundary and recording after a successful response covers redirect and download failures, but it still omits requests accepted by the server when the client loses the response. It also leaves a crash window between the remote side effect and local persistence. This approach is rejected as the accounting boundary.

### Append-only pre-POST attempt ledger

Create one durable row before every positive-GP archive POST. The row represents the maximum local GP exposure of that attempt, not a confirmed server invoice. It remains counted whether the later request succeeds, fails, or has an uncertain result. This is the selected approach because it fails closed across every locally observable failure boundary.

## Data model

Add an `eh_gp_spend_attempts` table:

- `id`: integer primary key.
- `queue_id`: nullable reference to `eh_download_queue.id`, using `ON DELETE SET NULL` so deleting queue work cannot delete spend history.
- `gid`: gallery ID retained after a queue row is deleted.
- `gp_cost`: positive GP amount reserved for this attempt.
- `created_at`: attempt reservation time, defaulting to the current timestamp.

Create an index on `created_at` for rolling-window queries. Multiple rows may reference the same queue entry. Rows are append-only; no result state is required because every result must remain in the budget sum and state transitions would add no correctness value.

The migration backfills each existing queue row with `gp_cost > 0` and non-null `completed_at` into one ledger attempt. The ledger row uses the queue row's original `completed_at`, not migration time. The rolling total therefore remains stable across the upgrade. Previously unrecorded failed charges cannot be reconstructed and are not guessed.

## Repository boundaries

Add a focused SeaORM entity and repository module for GP attempts. The repository exposes:

- an append operation accepting `queue_id`, `gid`, and a positive `gp_cost`;
- `get_eh_gp_cost_in_window(hours)`, summing ledger rows whose `created_at` is within the cutoff.

The existing queue `gp_cost` column remains a compatibility/display field for the most recent successful archive download. Successful main and background marks continue to write it. It is no longer a source for rolling-budget calculations, preventing double counting between queue rows and the ledger.

## Check-and-reserve flow

Replace the current long-lived permit flow with a shared check-and-reserve operation used by both workers:

1. Apply the existing byte-window and cost-classification checks without sending a POST.
2. For a positive `DownloadCost::Gp`, acquire the process-wide GP mutex when the rolling budget is enabled.
3. Read the ledger total and defer if the new attempt would exceed the configured window budget.
4. Apply the existing per-archive GP threshold.
5. Append the attempt row while still holding the mutex. A failed insert returns an error and prohibits the POST.
6. Release the mutex after the reservation commits.
7. Send the archive POST and run redirect parsing, transfer, ZIP validation, and rename.

When `gp_rate_limit == 0`, positive-GP attempts still append a ledger row, but no mutex or window read is needed. Free and unlocked downloads append no row. Unknown, unavailable, insufficient, or rejected costs defer before reservation.

Because the committed reservation is visible to the next worker, the mutex no longer needs to span network transfer or queue success marking. The private permit type and its `hold_until` helper are removed. Main and background workers still derive the successful queue display cost from the prepared request.

## Error handling

- Reservation insert failure aborts the operation before POST and follows the existing worker retry path.
- Once reservation succeeds, every subsequent POST, HTTP response, redirect, transfer, ZIP, rename, or queue-mark failure leaves the attempt in the ledger.
- A crash after reservation but before POST leaves a conservative reservation in the window.
- Queue retry and permanent-failure transitions do not alter ledger rows.
- Queue deletion sets `queue_id` to null but retains `gid`, amount, and timestamp.
- No remote error permits deleting a reservation because the client cannot prove that E-Hentai did not charge the account.

## Testing

- Repository tests verify multiple attempts for one queue row are summed independently, old attempts leave the window, and all queue statuses are irrelevant.
- A migration test verifies historical positive queue cost is backfilled with its original completion time and the post-migration rolling total matches the old total.
- A main-worker HTTP regression returns a successful archiver POST with malformed redirect HTML, then verifies the queue retries while one attempt and its GP amount remain recorded.
- Background-worker coverage verifies the same shared reservation boundary is used.
- A database failure injected before reservation insert proves no archive POST is sent.
- Free and unlocked downloads create no attempt.
- Existing background/background and main/background concurrency tests verify a budget that fits one request creates one attempt and sends one POST.
- A repeated-attempt test with the rolling limit disabled verifies two attempts for the same queue row create two rows and sum both costs.
- Existing archive form/cost correspondence tests remain unchanged.

## Operational trade-off

Pre-POST reservation can overcount when the process crashes before sending or when a connection attempt never reaches E-Hentai. Without an authoritative billing query or idempotent remote operation, exact classification is impossible. Conservatively consuming local budget for the configured window prevents the review's unsafe failure mode: unrecorded potentially charged requests and repeated paid retries.

## Self-review

- Placeholder scan: no placeholders or deferred requirements.
- Internal consistency: the ledger is the sole rolling-GP source, while queue cost remains success metadata.
- Scope: migration, entity, repository, two worker call sites, and direct tests only.
- Ambiguity: reservation timing, backfill, deletion, disabled-budget behavior, and uncertainty policy are explicit.
