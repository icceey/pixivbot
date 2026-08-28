# EH Result Reuse and Chat-Level Push Ledger — Design

Date: 2026-08-26
Status: Approved (user delegation + self-review)
PR context: follows PR #125 shared-gallery-jobs work on `feat/eh-shared-gallery-jobs`.

## Goal

Two related capabilities on top of the shared EH Job/Delivery architecture:

1. **Cross-generation reuse of completed IPFS3 upload results.** When a shared job has already uploaded a gallery's images to IPFS3 and built a Telegraph page, a *later* wave for the same variant must reuse that page (skip source download and provider upload) as long as the EH source content is unchanged.
2. **Chat-level subscription dedup.** A gallery that has already been successfully delivered to a chat must not be re-delivered by another (overlapping or newer) subscription in the same chat. Only the surfaces (archive / Telegraph link) that were not yet successfully sent are re-sent. Direct `/edl` / `/telegraph` requests always bypass the dedup and force a fresh delivery.

## User decisions (binding)

- **Provider scope:** only IPFS3/CID results are reusable. S3/Pixi/Catbox URLs are not persisted as reusable results.
- **Freshness:** every new demand re-checks the source. The cache is validated against a metadata fingerprint taken at enqueue time; mismatch → full re-download/upload.
- **Dedup granularity:** per-surface. A chat that already received the archive but not the Telegraph link gets only the link on later subscription matches, and vice versa.

## Part A — EH gallery result cache

### Table `eh_gallery_results`

```sql
CREATE TABLE eh_gallery_results (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    gid BIGINT NOT NULL,
    token TEXT NOT NULL,
    download_mode TEXT NOT NULL,
    resolution TEXT NOT NULL,
    source_fingerprint TEXT NOT NULL,
    telegraph_url TEXT NOT NULL,
    telegraph_rewrite_data TEXT,
    media_cids TEXT,                -- JSON: [{"name":"001.jpg","cid":"bafk..."}, ...] in uploadable order
    created_at TIMESTAMP NOT NULL,
    updated_at TIMESTAMP NOT NULL,
    UNIQUE (gid, token, download_mode, resolution)
)
```

- Results remain keyed only by `EhGalleryVariant` (`gid, token, download_mode, resolution`). Jobs are more specific: each Job is one variant plus an immutable source-fingerprint generation (with one NULL/unknown bucket), so concurrent generations never mutate each other. The result table deliberately retains only the latest row per variant.
- One record per variant: each successful upload **upserts** (replaces) the previous record. No history of superseded results.
- `telegraph_rewrite_data` stores the full page set (paths + content + gateways) exactly as the job's rewrite payload did, captured at upload success while it still exists.
- `media_cids` stores the ordered `(entry_name, cid)` list; `cid` comes from the new `TelegraphImageUrlPair.cid` field (Part A.2). Empty/absent when the uploader is not IPFS3.
- No backfill from existing data: retired jobs have no CIDs and historical pages cannot be attributed reliably. The cache starts empty and fills on each successful upload.

### Source fingerprint

- Definition: `format!("{posted}|{filecount}|{filesize}|{expunged}")` of `EhGallery` (eh_client `models.rs` helper `source_fingerprint()`).
- **Rating and title are excluded** — they change without content changes and would permanently invalidate the cache. `posted` is stable; `filecount`/`filesize` change whenever pages are added or replaced; `expunged` toggles are treated as source changes (conservative).
- New column `eh_gallery_jobs.source_fingerprint TEXT NULL`:
  - Set at job creation from the enqueue request (both direct and subscription paths provide `EhGallery` metadata today).
  - It is immutable Job identity rather than mutable freshness state: a different known fingerprint creates a distinct concurrent Job, while repeated NULL requests share the unknown bucket.
  - `NULL` means "fingerprint unknown" (legacy rows, tests): the job never consults the cache and its upload success does not write a result record.
  - `EhPendingGallery` (overflow backlog in `EhTagState`) gains `fingerprint: Option<String>` with `#[serde(default)]` so old serialized states keep deserializing; backlog re-drain reuses the stored value.

### Capture of CIDs (eh_client)

- `TelegraphImageUrlPair` gains `pub cid: Option<String>`.
- `IpfS3Uploader::url_pair_for_cid` populates it; all other providers construct pairs with `cid: None`.
- The upload worker (ZIP-first and per-image paths) collects the ordered `(entry_name, cid)` list whenever **all** pairs carry a CID (IPFS3); otherwise the media list is empty.

### Write path

On upload success, inside the same transaction that marks the job Telegraph-ready (`mark_eh_job_telegraph_ready` — extended or wrapped):

- If `job.source_fingerprint` is non-NULL **and** the media list is non-empty (IPFS3), upsert `eh_gallery_results` for the variant with: fingerprint, `telegraph_url`, serialized rewrite payload (if any), media CIDs, timestamps.
- Non-IPFS3 providers or missing fingerprint → no record write; job-ready semantics unchanged.
- Failure to write the record fails the ready transaction (single atomic state: job ready ⇔ record persisted when applicable).

### Read path (reuse)

A single repo helper `try_apply_cached_eh_result_in_txn(txn, job_id, send_archive) -> bool` used at exactly two sites:

1. **Job creation with telegraph demand** (inside `enqueue_eh_download_in_txn`, after the new delivery is upserted and an explicit generation-boundary result from `get_or_create_eh_gallery_job_in_txn` confirms that the job was inserted or reset): look up the variant's result record; on `source_fingerprint` match:
   - Set `telegraph_status='ready'`, `telegraph_url`, and the cached rewrite payload with **unscheduled** rewrite state (`telegraph_rewritten_at = NULL`, rewrite status/deadlines/claim fields NULL). A prior result may have retired before its first link was sent, so the cache must not assert that `editPage` already ran. After the reused link is first sent, the existing marker transaction schedules the rewrite exactly once; replaying an already completed rewrite is idempotent.
   - If **every** active delivery of the new wave is Telegraph-only (no unsent archive surface): set `status='downloaded'`, `zip_path=NULL`, `completed_at=now` — the proven zipless-ready configuration (`ready_telegraph_delivery_during_cleanup_reuses_page_without_redownload` already exercises publish on it). No download claim occurs; publish sends the cached URL directly.
   - If any active delivery still needs the archive: keep `status='pending'` (the ZIP must be fetched for the archive surface) while Telegraph is already ready — upload claim is skipped (claim requires `pending`), download completion must preserve ready state (covered by late-demand semantics).
   - No active telegraph demand at creation (archive-only wave): do not consult the cache; a later telegraph demand goes through (2).
2. **Late telegraph demand** (`recompute_eh_job_telegraph_requirement_in_txn` flipping demand to required while the job is not already ready/failed-terminal): same helper; on hit the job becomes ready without an upload wave. A normal or background archive download claim may remain in flight: cache application changes only Telegraph/rewrite fields and preserves the download status, generation, background lease, and artifact ownership. Zipless `status='downloaded'` conversion is allowed only for an unclaimed pending job with no archive demand.

Fingerprint mismatch or absent record → normal flow (pending download and/or upload). The stale record is *not* deleted at mismatch; it is replaced on the next successful upload.

### Invalidation

- Explicit: none in v1 (no TTL, no manual purge).
- Implicit: fingerprint mismatch on next demand; superseded by upsert on next successful upload.
- The cache never gates correctness: a miss degrades to today's behavior.

### Scope note (explicit non-goal)

Archive delivery still requires the actual ZIP bytes; a cache hit with archive demand re-downloads the archive (paid POST) but skips the IPFS upload. Fetching the ZIP back from IPFS via an archive CID is future work.

## Part B — Chat-level push ledger

### Table `eh_gallery_push_ledger`

```sql
CREATE TABLE eh_gallery_push_ledger (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    chat_id BIGINT NOT NULL,
    gid BIGINT NOT NULL,
    archive_sent_at TIMESTAMP,
    telegraph_sent_at TIMESTAMP,
    updated_at TIMESTAMP NOT NULL,
    UNIQUE (chat_id, gid)
)
```

- Records **successful surface sends** per `(chat_id, gid)` regardless of source (direct or subscription): if the chat already received the content, a later subscription must not duplicate it.
- No retention/pruning in v1 (rows are tiny; note for future work).

### Write path

- Production methods `mark_eh_archive_delivery_sent(delivery_id)` and `mark_eh_telegraph_delivery_sent(...)`: after the existing marker CAS succeeds, upsert the ledger row (set the respective `*_sent_at`, leave the other column untouched) **in the same transaction**. `mark_eh_archive_delivery_sent` must be wrapped in a transaction; the `#[cfg(test)]`-only `mark_eh_archive_sent` is not an integration point. These methods run under the chat lock inside the publish worker.
- Terminal upload-failure archive fallback writes the archive marker through the same method, so it lands in the ledger too.

### Read path (subscription enqueue dedup)

Inside `enqueue_eh_download_request` (chat lock + single transaction already guaranteed), **only for `source = subscription`**:

1. Read the ledger row for `(chat_id, gid)`.
2. Wanted surfaces for this request: archive (iff `send_archive`) and telegraph (iff request `telegraph`).
3. If every wanted surface already has a ledger timestamp → **skip enqueue entirely**: no delivery row, no job creation/rebind, no wave. The enqueue API returns `Ok(None)`; the caller (collect loop) still records the GID in `pushed_gids` and advances normally.
4. Otherwise create the new wave as today, but pre-mark satisfied surfaces on the fresh delivery row:
   - `archive_sent_at = ledger.archive_sent_at` when archive is wanted and already sent;
   - `telegraph_sent_at = ledger.telegraph_sent_at` when telegraph is wanted and already sent.
   Pre-marked surfaces are skipped by the publish worker (existing marker-skip behavior) and excluded from `telegraph_required` aggregation (existing unsent-demand rule), so no upload is demanded for an already-delivered link.
5. Pre-marking applies only to **new-wave creation** (terminal reset path). An existing active delivery keeps its markers untouched (owner merge only).
6. Direct requests (`source = direct`) bypass steps 1–4 completely; their successful sends still write the ledger.

### API change

`enqueue_eh_subscription_download` / `enqueue_eh_download` return `Result<Option<eh_download_queue::Model>>` (`None` = deduped skip). Collect-loop callers treat `None` as success. Direct callers always receive `Some`.

### Backfill

None. The ledger starts empty; per-subscription `pushed_gids` semantics are unchanged and continue to provide same-subscription dedup. Cross-subscription duplicates may occur once for galleries delivered before this feature; after that the ledger prevents them.

## Concurrency and invariants

- Cache reads occur inside the enqueue transaction under `EH_CHAT_LOCKS`; cache writes occur inside the job-generation-guarded ready transaction; ledger reads and marker writes occur inside the existing enqueue/marker transactions under `EH_CHAT_LOCKS`. No new lock ordering is introduced.
- Reuse configuration happens only at new-generation job creation or late-demand recompute — both are transactional CAS points today; a concurrent writer cannot observe a half-reused job.
- Cache application rejects cleanup or Telegraph-upload claims, but may coexist with a normal/background download claim because it preserves every download/background claim field; download completion preserves the already-ready Telegraph state.
- Ledger upserts are idempotent (UPSERT by `(chat_id, gid)`); replay after crash is harmless.
- A reused (zipless-ready) job follows existing liveness: consumerless → retire, no cleanup (no owned ZIP); later archive demand reactivates via the existing `recover_eh_job_for_missing_archive_in_txn` path.

## Testing matrix

Result cache:
1. Upload success with fingerprint + IPFS3 CIDs writes/replaces the result record atomically with job-ready.
2. Non-IPFS3 provider (cid None) or NULL fingerprint → no record.
3. Later Telegraph-only enqueue, fingerprint match → no source claim by normal/background selectors, no upload claim, publish sends cached URL, first send schedules any cached rewrite payload, job retires zipless.
4. Fingerprint mismatch → pending job, normal download/upload, record replaced on new success.
5. Cache hit with archive demand → job pending + telegraph ready; download completes; ready state preserved; publish sends ZIP + cached link; no upload provider call.
6. Late telegraph demand on downloaded or currently downloading job with matching record → ready without upload wave; an in-flight download keeps its generation and completion preserves ready state.
7. Variant isolation: different resolution/mode never hits another variant's record.

Push ledger:
8. Overlapping subscription B after A delivered both surfaces → enqueue skipped (`None`), no new wave, `pushed_gids` still advances.
9. Partial: archive sent only → later subscription wanting both enqueues a wave with pre-marked `archive_sent_at`; publish sends only the link; done.
10. Partial: telegraph sent only → pre-marked `telegraph_sent_at`; publish sends only the archive; no upload demanded.
11. Direct `/edl` after full ledger satisfaction → fresh wave, both surfaces sent (explicit force), ledger refreshed.
12. Crash window: enqueue committed, delivery not yet sent → re-enqueue merges into the active wave (no duplicate), exactly one send later.
13. Terminal upload failure fallback → archive marker lands in ledger; later telegraph-wanting subscription enqueues a telegraph-only wave.

End-to-end:
14. Flagship: chat A consumed the gallery long ago (job retired, ZIP deleted); chat B subscribes to the same gallery later; fingerprint match → chat B receives the cached Telegraph link with zero EH download and zero provider upload.
15. Fingerprint change between waves (filecount differs) → full re-download and re-upload for the new wave.

## Constraints

- Rust 1.94; no new dependencies; no new config knobs (features are always-on with safe degradation).
- No changes to `EhGalleryVariant` identity semantics.
- No provider/network behavior changes beyond exposing the CID in `TelegraphImageUrlPair`.
- Existing shared-job invariants (CAS generations, chat locks, fail-closed cleanup, exact-once notifications, marker preservation) remain authoritative.
- Migration registered in `migration/src/lib.rs`; test-schema parity in `setup_test_db`.
- No secrets in any persisted record.
