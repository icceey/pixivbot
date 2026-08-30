# EH Shared Gallery Jobs Design

## Context

`eh_download_queue` currently combines two responsibilities:

1. gallery work that is identical for every consumer (archive download, image upload,
   Telegraph page creation, and delayed gateway rewrite); and
2. delivery state that is specific to one Telegram chat (subscription ownership,
   cancellation, archive/link send markers, and Telegram retry).

The table is unique on `(chat_id, gid)`, so two chats requesting the same gallery create two
rows and execute the shared stages twice. The normal download worker claims one row per tick,
and `EH_PUBLISH_CANCEL_LOCK` is held across download, upload, and publish network operations,
so those duplicate rows are also processed serially.

At the same time, archive artifacts are named only `{gid}_{token}.zip`. Different queue rows can
therefore write and delete the same final ZIP, partial files, multipart parts, and upload
manifests without sharing ownership. A completed delivery can delete an artifact still needed by
a sibling, after which the sibling returns to `pending` and downloads the gallery again.

## Goals

- Execute archive download exactly once for simultaneous consumers of the same gallery variant.
- Execute image/IPFS upload and Telegraph page creation exactly once for that shared variant.
- Preserve independent Telegram delivery, cancellation, retry, and subscription progress per chat.
- Share only identical variants; different download modes or resolutions remain isolated.
- Allow bounded concurrent delivery to different chats while preserving cancellation ordering and
  the existing throttled Telegram client.
- Make artifact ownership, crash recovery, GP accounting, and cleanup durable in SQLite rather
  than dependent on an in-memory single-flight map.
- Preserve existing direct-request priority, subscription ownership, queue-status visibility,
  background download, multipart abort, Telegraph rewrite, and retry behavior.

## Non-goals

- Do not make unrelated galleries download or upload concurrently; this change only adds bounded
  concurrency to the per-chat delivery stage.
- Do not retain ZIPs as a permanent historical cache. Sharing applies to one active consumer wave;
  a later request after retirement may download again.
- Do not merge different resolutions by downloading the highest quality.
- Do not change Telegram message text, archive captions, command behavior, EH search scheduling,
  public config defaults other than the new delivery concurrency setting, or image providers.
- Do not support multiple bot processes as a new deployment mode. Database claims remain safe,
  but send-versus-unsubscribe ordering retains the existing single-process assumption.

## Approaches Considered

1. **Normalized shared job plus per-chat delivery (selected).** Persist common work once and keep
   chat-specific state in `eh_download_queue`. This gives each lifecycle a single owner and makes
   retry and cleanup rules explicit.
2. **Owner row plus sibling references in the existing table.** This appears smaller, but owner
   cancellation, failure, re-enqueue, and cleanup require ownership transfer and duplicate shared
   state across rows. It recreates a job table implicitly and is rejected.
3. **In-memory single-flight plus file existence checks.** This does not survive restart, does not
   coordinate background workers or another process, cannot safely share Telegraph results, and
   does not solve premature cleanup. It is rejected.

## Data Model

### `eh_gallery_jobs`

Add a table whose row owns one active gallery-variant **source-fingerprint generation** and every
shared side effect. `EhGalleryVariant` remains only `(gid, token, download_mode, resolution)`;
the Job identity adds the immutable fingerprint bucket.

| Column | Meaning |
| --- | --- |
| `id` | Integer primary key. |
| `gid`, `token` | EH gallery identity and access token. |
| `download_mode` | `archive` for logged-in archive downloads, `images` for direct image ZIP creation, or transitional `legacy` for migrated active work. |
| `resolution` | Canonical archive resolution; empty string for `images`. |
| `source_fingerprint` | Immutable source-generation identity. Non-NULL values distinguish concurrent known generations; NULL is one shared unknown-fingerprint bucket for the variant. |
| `title` | Shared gallery title used for filenames and the Telegraph page. |
| `status` | Download lifecycle: `pending`, `downloading`, `downloaded`, `failed`, `retired`. |
| `telegraph_status` | Shared publication lifecycle: `not_required`, `pending`, `uploading`, `ready`, `failed`. |
| `telegraph_required` | Cached aggregate indicating whether any active delivery requests Telegraph. |
| `file_size`, `gp_cost`, `zip_path` | Shared archive result. |
| `telegraph_url` | Shared Telegraph page URL. |
| `error`, `retry_count`, `next_retry_at` | Shared-stage retry state. |
| `cleanup_status`, `cleanup_started_at`, `cleanup_error`, `cleanup_next_retry_at` | Fail-closed artifact cleanup lifecycle. |
| `created_at`, `started_at`, `completed_at` | Claim generation and lifecycle timestamps. |
| background download fields | Existing background status, retry, and progress fields moved to job scope. |
| Telegraph rewrite fields | Existing rewrite payload, status, retry, and completion fields moved to job scope. |

SQLite uses two partial unique indexes for Job identity: `(gid, token, download_mode, resolution,
source_fingerprint) WHERE source_fingerprint IS NOT NULL` and `(gid, token, download_mode,
resolution) WHERE source_fingerprint IS NULL`. A variant helper computes:

```text
logged in + direct request       => archive:<download_resolution>
logged in + subscription request => archive:<subscription_resolution>
not logged in                    => images
```

Migrated active rows use `download_mode = legacy` and `resolution = direct|subscription`. The
worker resolves that transitional variant using current authentication and the corresponding
configured resolution. New enqueue operations never create legacy variants.

The artifact family includes the job identity and sanitized variant, rather than only
`gid_token`, so incompatible variants cannot collide.

### `eh_download_queue` as delivery

Keep the existing table as the durable per-chat delivery table and add nullable `job_id` with a
foreign key to `eh_gallery_jobs`. New active rows always have a job. Historical terminal rows may
keep `job_id = NULL` so migration does not manufacture shared state that will never run.

The delivery row continues to own:

- `chat_id`, `gid`, title, `source`, `subscription_ids`, and
  `telegraph_subscription_ids`;
- whether that chat requests Telegraph;
- `archive_sent_at` and `telegraph_sent_at`;
- delivery status, Telegram retry/error, and delivery timestamps.

Delivery status is exactly `waiting`, `publishing`, `done`, `failed`, or `canceled`. User-facing
download/upload stages are derived by joining a waiting delivery to its job.

Shared columns currently present on `eh_download_queue` remain nullable for schema compatibility
during this migration but are no longer authoritative for rows with `job_id`. Repository APIs must
read shared state through the job relation and must not mirror mutable job state back into every
delivery.

The existing unique `(chat_id, gid)` constraint remains. A direct request can therefore upgrade an
existing subscription delivery without sending the same gallery twice to one chat. If that upgrade
changes the variant, the delivery is rebound transactionally to the new job before any send marker
has been committed.

### Accounting ledgers

Add nullable `job_id` to `eh_gp_spend_attempts`; new reservations reference the shared job and are
written once per actual archive POST. Existing `queue_id` values remain as historical provenance.

Add append-only `eh_download_completions(id, job_id, gid, file_size, created_at)`. Every successful
shared download generation appends one row in the same transaction that marks the job downloaded.
The migration backfills one row for each historical queue row with positive `file_size` and a
completion timestamp, preserving downloads that occurred before sharing. Rolling byte windows sum
this ledger; rolling GP windows sum spend attempts. Reusing a retired job row therefore cannot
erase an older generation from either window, and adding more chat deliveries does not multiply new
accounting.

## Enqueue and Deduplication

Enqueue runs in one database transaction:

1. Compute the complete variant from authentication mode, request source, and configured
   resolution.
2. Upsert the exact fingerprint-generation job. A clean `retired` or retryable `failed` job is reset to `pending`
   only when a new active consumer wave requires work. A retired job with pending/failed cleanup is
   bound to the new delivery but remains non-claimable until maintenance completes cleanup.
3. Select or insert the `(chat_id, gid)` delivery with existing CAS merge rules.
4. Bind the delivery to the job, merge subscription owners, recompute the job's active
   `telegraph_required` aggregate, and set `telegraph_status = pending` when a downloaded job gains
   its first Telegraph consumer.
5. If a source upgrade changes variants, release the old job only after rebinding succeeds. The
   old job is retired or canceled only when it has no active deliveries.

Concurrent inserts recover from both unique constraints by re-selecting and retrying the
transaction. At commit there is exactly one Job per `(variant, source-fingerprint bucket)` and at most one delivery per
`(chat_id, gid)`.

## Shared Job Pipeline

### Download

`EhDownloadWorker` claims `eh_gallery_jobs.status = pending`, not delivery rows. Before work it
loads active deliveries and proceeds when at least one destination remains eligible. The selected
job determines download mode and resolution; no chat-specific row owns the ZIP.

On success the worker records one ZIP, file size, GP cost, and `downloaded` state. If Telegraph is
currently required, it also changes `telegraph_status` from `not_required` to `pending`. All waiting
deliveries observe that result through `job_id`. On failure it updates job retry state once; a
terminal download failure transitions active deliveries to failed state without issuing one source
download retry per chat. This preserves the current download-worker behavior: it logs the internal
chain and exposes failure through queue status, but does not add a new Telegram failure message.

Background download claims and progress also use job IDs. Normal and background claims remain
CAS-guarded so only one writer owns a variant generation.

### Image upload and Telegraph

`EhUploadWorker` claims a downloaded job only when `telegraph_status = pending`. It extracts and
uploads the ZIP once, creates one Telegraph page, persists the URL and rewrite payload on the job,
then marks `telegraph_status = ready`.

An archive-only delivery can publish once the job is `downloaded`, including while a Telegraph
upload is running or has failed for other consumers. A Telegraph delivery waits for
`telegraph_status = ready` and a non-null URL. If a Telegraph requirement is added after download,
the aggregate update makes the existing job eligible for upload without downloading again. A
terminal upload failure fails only Telegraph-dependent deliveries; archive-only deliveries remain
eligible. The transition returns affected delivery IDs, chat IDs, and titles so the existing upload
worker can send one fixed friendly failure message to each affected chat. It never formats the
shared internal error into Telegram text.

Delayed preview-to-public gateway rewrite is job-scoped and runs once. It is scheduled after the
first successful Telegraph-link delivery, matching the current post-send delay while avoiding
duplicate page edits.

## Delivery and Concurrency

Add `ehentai.publish_concurrency`, default `2`, clamped to `1..=10`. The publish worker maintains at
most that many claimed delivery futures. Every claim remains an atomic
`waiting -> publishing` transition, and one delivery failure affects only that row.

Replace the global publish/cancel mutex with keyed chat locks:

- download and upload never acquire a publish/cancel lock;
- publish acquires only the target chat lock around final active checks, Telegram calls, and sent
  markers;
- subscription cancellation acquires affected chat locks in sorted order before mutating their
  deliveries;
- different chats may publish concurrently, while unsubscribe and send operations for the same
  chat retain one observable order.

The existing `Throttle<Bot>` remains the Telegram rate-limit authority. No manual Telegram sleeps
are added.

Delivery eligibility is derived from both states:

- archive requested: job download `status = downloaded` and ZIP exists;
- Telegraph requested: job `telegraph_status = ready` and `telegraph_url` exists;
- disabled chat: delivery is deferred without blocking the job or other chats;
- both sent markers complete: delivery becomes `done` without repeating shared work.

## Cancellation and Cleanup

Subscription ownership remains delivery-scoped. Removing one subscription may downgrade that
delivery's Telegraph requirement or cancel it when no owner remains. It never directly cancels a
job used by another delivery.

After enqueue, cancellation, or rebind, `telegraph_required` is recomputed from active deliveries.
If it becomes false before an upload claim starts, `telegraph_status` returns to `not_required`.
Once upload is claimed, cancellation does not interrupt or erase its multipart state; normal
completion/abort rules finish the already-started shared side effect.

After every delivery cancellation, completion, or rebind, repository code evaluates job liveness:

- active consumers retain the job; its ZIP remains only while an unsent archive surface or an
  upload stage still needs it;
- with no active consumers during a cancellable shared stage, retire the job and mark cleanup
  pending; abort persisted multipart state through the configured uploader before local deletion;
- delete the ZIP only after no active delivery still needs archive sending and no upload stage
  still needs the ZIP;
- retain the job row as `retired`; clear transient paths, retry state, and upload manifests only
  after cleanup succeeds.

`cleanup_status` is `none`, `pending`, `running`, or `failed`. Cleanup is a generation-guarded
maintenance claim. Abort/removal failure records `failed` plus retry time and leaves every path and
manifest intact. A new consumer may bind to that job but cannot reactivate or write its stable
artifact family. After cleanup succeeds, finalization either resets the job to `pending` when an
active consumer is waiting, or leaves a clean `retired` row; only then may the same job ID and path
be reused.

A pending or running Telegraph rewrite keeps the job non-retired after the final delivery. Archive
artifacts may be cleaned once no delivery/upload needs them, but rewrite payload and page state are
retained. The rewrite worker reevaluates liveness after success or terminal failure; only a terminal
rewrite allows final retirement and clearing its payload.

A delivery missing a supposedly ready ZIP does not independently reset itself to source download.
It atomically resets the shared job generation once and makes all still-active archive consumers
wait for the same replacement download.

## Failure and Crash Recovery

- Shared download and upload errors are stored on their respective job lifecycles. Download failure
  retains current log/status-only behavior. Existing upload/publish failure messages remain short,
  are fanned out per affected delivery where required, and never expose internal error chains.
- Telegram send errors remain on the delivery and retry independently.
- Stale `downloading` jobs and `telegraph_status = uploading` claims reset to their prior claimable
  shared state using the existing generation timestamp rules.
- Stale `publishing` deliveries reset to `waiting`; sent markers prevent repeating completed
  surfaces.
- Startup orphan cleanup derives the keep-set from active jobs, not queue rows.
- Multipart abort rules remain fail-closed: local upload state is not deleted until provider abort
  succeeds or the upload has completed.

## Migration

Create the job table, completion ledger, indexes, `eh_download_queue.job_id`, and
`eh_gp_spend_attempts.job_id` in one migration.

For existing rows:

1. Leave terminal (`done`, `failed`, `canceled`) rows unbound for historical status display.
2. Group active rows by `(gid, token, legacy source variant)`, where legacy variants keep direct
   and subscription work separate because the migration cannot read runtime resolution config.
3. Insert one `pending` job per active legacy group and bind every member delivery to it.
4. Reset active delivery status to `waiting`, preserving subscription ownership and already-sent
   markers, while clearing obsolete per-row claims. Existing partial files are not trusted as a
   completed shared result; safe orphan cleanup may remove them after startup.
5. New requests always use canonical mode/resolution variants. Legacy jobs drain without being
   merged into canonical jobs.
6. Backfill `eh_download_completions` from positive historical queue `file_size` values with their
   completion timestamps before active compatibility fields are cleared.

This migration may redownload one in-flight gallery after upgrade, but it never treats an absent or
partially owned legacy artifact as complete and does not duplicate already-recorded Telegram sends.

The down migration removes foreign-key indexes/columns and the job table only where supported by
the repository's existing SQLite migration conventions; it does not attempt to reconstruct shared
job progress into delivery rows.

## User-visible Queue Status

`/estatus` remains chat-scoped. For an active delivery it joins the job and derives the existing
friendly stage labels from job download/upload/background state, with delivery `publishing` taking
precedence. Historical terminal rows with no job continue to display normally. No shared errors,
paths, job IDs, or other chats are exposed.

## Verification Scenarios

1. **Shared happy path:** concurrently enqueue two chats for the same canonical variant. Exactly
   one job exists, source archive POST/download occurs once, image upload and Telegraph page
   creation occur once, and both chats receive the requested surfaces.
2. **Variant isolation:** enqueue the same `gid/token` at two resolutions. Two jobs and two distinct
   artifact families exist; neither result or cleanup crosses variants.
3. **Cancellation isolation:** cancel one subscription while a shared job is active. Its delivery
   stops, the other delivery completes, and the job/artifact is not canceled or deleted early.
4. **Delivery retry isolation:** one chat's Telegram send fails and retries while another succeeds.
   No source download, image upload, or page creation repeats.
5. **Crash recovery:** interrupt each shared claim and one delivery claim, run stale reset, and
   verify one resumed shared operation plus marker-safe delivery.
6. **Cleanup:** complete the first delivery while the second remains active, verify ZIP retention,
   then complete/cancel the final delivery and verify one multipart-safe cleanup.
7. **Migration:** migrate active duplicate legacy rows and terminal history in SQLite; verify one
   pending legacy job per group, preserved send markers, and unbound terminal rows.
8. **Adjacent regression:** direct-over-subscription upgrades, `/eunsub`, `/estatus`, GP rolling
   budget, background download, startup cleanup, and Telegraph rewrite retain current behavior.
9. **Generation accounting and dirty reuse:** complete and retire the same variant twice and verify
   both byte-ledger rows remain in the window; force multipart abort failure, enqueue a new consumer,
   and verify no download starts until cleanup succeeds.
10. **Rewrite/retirement interlock:** complete the final delivery with delayed rewrite pending and
    verify the payload survives until one rewrite attempt reaches a terminal state.

Run focused repository and worker tests first, then `make quick`, and finally the repository-required
`make ci`. Modified Rust files must have no new language-server diagnostics.

## Files Expected to Change

- `migration/src/m20260824_000000_eh_shared_gallery_jobs.rs`
- `migration/src/lib.rs`
- `src/db/entities/eh_gallery_jobs.rs`
- `src/db/entities/eh_download_completions.rs`
- `src/db/entities/eh_download_queue.rs`
- `src/db/entities/eh_gp_spend_attempts.rs`
- `src/db/entities/mod.rs`
- `src/db/repo/eh_download_queue.rs`
- `src/db/repo/eh_gp_spend_attempts.rs`
- `src/db/repo/eh_download_completions.rs`
- repository test schemas and EH integration tests
- `src/scheduler/eh_engine.rs`
- `src/config.rs` and `config.toml.example`
- `src/main.rs` only if worker construction needs the new concurrency value explicitly

## Self-review

- Placeholder scan: no TBD, TODO, or incomplete requirement remains.
- Internal consistency: shared stages belong to one variant job; chat-specific send, ownership,
  cancellation, and retry remain on deliveries.
- Scope: this is one implementation plan despite crossing schema, repository, workers, and config;
  unrelated gallery concurrency and permanent caching are explicitly excluded.
- Ambiguity: sharing identity, migration behavior, state ownership, cancellation, cleanup,
  concurrency limit, failure propagation, and verification outcomes are explicit.
